//! Fallible Codex metadata discovery with useful work retained after failures.
//!
//! Match the pinned FAD root, file-selection and cutoff rules. Its directory
//! walker discards traversal errors, so parsing remains in FAD while this
//! inventory owns enumeration. This is not an atomic directory snapshot;
//! discovered files still pass the existing admission and snapshot guards.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;
use franken_agent_detection::DiscoveredSourceRole;
use franken_agent_detection::connectors::codex::is_compressed_rollout;
use serde::Serialize;
use walkdir::WalkDir;

use super::path_policy::ScanExclusions;
use super::{DiscoveredSourceFile, ScanContext, ScanRoot};

const MAX_FAILURE_SAMPLES: usize = 32;

#[derive(Debug, Serialize)]
struct FailedPath {
    source_path: String,
    reason: &'static str,
}

#[derive(Debug, Serialize)]
struct FailureReport {
    reason: &'static str,
    // Count failed-path observations. The retained sample is deduplicated;
    // failures beyond its fixed bound may recur through overlapping roots.
    failed_path_count: usize,
    omitted_path_count: usize,
    failed_paths: Vec<FailedPath>,
}

impl Default for FailureReport {
    fn default() -> Self {
        Self {
            reason: "source_discovery_failures",
            failed_path_count: 0,
            omitted_path_count: 0,
            failed_paths: Vec::new(),
        }
    }
}

impl fmt::Display for FailureReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Codex source discovery incomplete: {}",
            serde_json::to_string(self).map_err(|_| fmt::Error)?
        )
    }
}

/// Known files and the failures that prevent certifying the whole selection.
#[derive(Default)]
pub(super) struct Inventory {
    sources: Vec<DiscoveredSourceFile>,
    source_keys: HashSet<PathBuf>,
    failed_paths: HashSet<PathBuf>,
    report: FailureReport,
    first_error: Option<anyhow::Error>,
}

impl Inventory {
    pub(super) fn record_failure(&mut self, path: &Path, error: anyhow::Error) {
        if self.failed_paths.contains(path) {
            return;
        }
        if self.failed_paths.len() < MAX_FAILURE_SAMPLES {
            self.failed_paths.insert(path.to_path_buf());
        }
        let reason = match error.downcast_ref::<io::Error>().map(io::Error::kind) {
            Some(io::ErrorKind::PermissionDenied) => "source_unreadable",
            Some(io::ErrorKind::NotFound) => "source_missing",
            _ => "source_discovery_failed",
        };
        self.report.failed_path_count = self.report.failed_path_count.saturating_add(1);
        if self.report.failed_paths.len() < MAX_FAILURE_SAMPLES {
            self.report.failed_paths.push(FailedPath {
                source_path: path.to_string_lossy().into_owned(),
                reason,
            });
        } else {
            self.report.omitted_path_count = self.report.omitted_path_count.saturating_add(1);
        }
        if self.first_error.is_none() {
            self.first_error = Some(error);
        }
    }

    pub(super) fn blocked(&self, path: &Path) -> bool {
        // Do not probe synthetic child roots after a retained ancestor failed.
        // Keep failure bookkeeping bounded as well as the displayed samples.
        path.ancestors()
            .any(|ancestor| self.failed_paths.contains(ancestor))
    }

    pub(super) fn should_skip(&mut self, path: &Path, exclusions: &ScanExclusions) -> bool {
        if self.blocked(path) {
            return true;
        }
        match exclusions.try_excludes_codex(path) {
            Ok(excluded) => excluded,
            Err(error) => {
                self.record_failure(path, error.into());
                true
            }
        }
    }

    pub(super) fn into_parts(self) -> (Vec<DiscoveredSourceFile>, Result<()>) {
        let result = self
            .first_error
            .map_or(Ok(()), |error| Err(error.context(self.report)));
        (self.sources, result)
    }

    pub(super) fn finish(self) -> Result<Vec<DiscoveredSourceFile>> {
        let (sources, result) = self.into_parts();
        result?;
        Ok(sources)
    }

    pub(super) fn collect(&mut self, ctx: &ScanContext, exclusions: &ScanExclusions) {
        // FAD deduplicates before the cutoff within one discovery. Archive
        // collection is a separate discovery with no cutoff, so an old source
        // skipped in the active pass must remain eligible in that later pass.
        let mut seen_files = HashSet::new();
        for root in source_roots(ctx) {
            if self.should_skip(&root.path, exclusions) {
                continue;
            }
            let metadata = match optional_metadata(&root.path) {
                Ok(Some(metadata)) => metadata,
                Ok(None) => continue,
                Err(error) => {
                    self.record_failure(&root.path, error.into());
                    continue;
                }
            };
            let files = if metadata.is_file() {
                if !is_rollout_file(&root.path) {
                    continue;
                }
                // An explicit file remains selected even when its plain or
                // compressed sibling also exists, exactly as in FAD.
                vec![root.path.clone()]
            } else {
                let sessions = root.path.join("sessions");
                let selected = match optional_metadata(&sessions) {
                    Ok(Some(_)) => sessions,
                    Ok(None) => root.path.clone(),
                    Err(error) => {
                        self.record_failure(&sessions, error.into());
                        continue;
                    }
                };
                self.rollout_files(&selected, exclusions)
            };
            for file in files {
                if self.should_skip(&file, exclusions) || !seen_files.insert(leaf_key(&file)) {
                    continue;
                }
                let metadata = match fs::metadata(&file) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        self.record_failure(&file, error.into());
                        continue;
                    }
                };
                if !modified_since(&metadata, ctx.since_ts) {
                    continue;
                }
                // Archive discovery has always resolved physical aliases at
                // this boundary while retaining the first source's identity.
                let key = fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
                if self.source_keys.insert(key) {
                    self.sources.push(
                        DiscoveredSourceFile::new(
                            "codex",
                            &root,
                            file,
                            DiscoveredSourceRole::PrimarySessionLog,
                            true,
                        )
                        .with_fs_metadata(),
                    );
                }
            }
        }
    }

    fn rollout_files(&mut self, sessions: &Path, exclusions: &ScanExclusions) -> Vec<PathBuf> {
        if self.should_skip(sessions, exclusions) {
            return Vec::new();
        }
        let mut files = Vec::new();
        // An explicitly selected symlink root retains FAD's behavior. Nested
        // symlink files/directories are not followed or admitted by the walk.
        let mut entries = WalkDir::new(sessions).follow_links(false).into_iter();
        while let Some(entry) = entries.next() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    let path = error.path().unwrap_or(sessions).to_path_buf();
                    let kind = error
                        .io_error()
                        .map_or(io::ErrorKind::Other, io::Error::kind);
                    // The host diagnoses the exact path through WalkDir's
                    // error; callers also need the actionable I/O error kind.
                    let error = anyhow::Error::new(error)
                        .context(io::Error::new(kind, "enumerate Codex source directory"));
                    self.record_failure(&path, error);
                    continue;
                }
            };
            let file_type = entry.file_type();
            // Nested links are never followed, and other leaf names are not
            // rollout candidates. Their namespace cannot make this scan fail.
            if !file_type.is_dir() && (!file_type.is_file() || !is_rollout_file(entry.path())) {
                continue;
            }
            if self.should_skip(entry.path(), exclusions) {
                if file_type.is_dir() {
                    entries.skip_current_dir();
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            if is_compressed_rollout(entry.path()) {
                let plain = entry.path().with_extension("");
                match optional_metadata(&plain) {
                    Ok(Some(metadata)) if metadata.is_file() => continue,
                    Ok(_) => {}
                    Err(error) => {
                        self.record_failure(&plain, error.into());
                        continue;
                    }
                }
            }
            files.push(entry.into_path());
        }
        files.sort();
        files
    }
}

fn optional_metadata(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
    if ctx.use_default_detection() {
        return vec![ScanRoot::local(super::archives::default_home(ctx))];
    }
    let mut paths = Vec::new();
    for root in &ctx.scan_roots {
        paths.push(root.path.clone());
        if !root.path.is_file()
            && !root
                .path
                .ancestors()
                .any(|path| path.file_name().is_some_and(|name| name == ".codex"))
        {
            paths.push(root.path.join(".codex"));
        }
    }
    let mut roots: Vec<_> = paths
        .into_iter()
        .map(|path| {
            if let Some(root) = ctx
                .scan_roots
                .iter()
                .find(|root| path.starts_with(&root.path) || root.path.starts_with(&path))
            {
                root.with_path(path)
            } else {
                ScanRoot::local(path)
            }
        })
        .collect();
    roots.sort_by(|left, right| left.path.cmp(&right.path));
    roots.dedup_by(|left, right| left.path == right.path);
    roots
}

fn is_rollout_file(path: &Path) -> bool {
    is_compressed_rollout(path)
        || (path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-"))
            && path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("jsonl")
                        || extension.eq_ignore_ascii_case("json")
                }))
}

fn leaf_key(path: &Path) -> PathBuf {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        }
        _ => path.to_path_buf(),
    }
}

fn modified_since(metadata: &fs::Metadata, since_ts: Option<i64>) -> bool {
    since_ts.is_none_or(|timestamp| {
        let threshold = timestamp.saturating_sub(1_000);
        metadata.modified().map_or(true, |modified| {
            modified
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(true, |duration| {
                    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX) >= threshold
                })
        })
    })
}

#[cfg(test)]
mod tests;
