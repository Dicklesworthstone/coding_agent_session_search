//! Candidate-owned history, not a publication/readiness certificate (GH #523).
//!
//! Capture identity once, before finalization: a swap can leave the prior live
//! index at the staging path. Never reacquire that path as a new candidate on a
//! later phase. All writes are best effort under the caller's index-run lock;
//! parents remain operator-controlled, as in the quarantine retention code.

use super::{Finalization, LAST_FAILURE_FILE};
use anyhow::{Context, Result, ensure};
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_CAUSE_BYTES: usize = 16 * 1024;
const MAX_RECORD_BYTES: usize = 128 * 1024;

pub(super) struct Candidate {
    path: PathBuf,
    canonical: PathBuf,
    owner: same_file::Handle,
    parent_owner: same_file::Handle,
}

fn directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir(),
        "candidate evidence requires a non-symlink directory"
    );
    Ok(())
}

fn absolute(path: &Path) -> Result<PathBuf> {
    ensure!(
        !path
            .components()
            .any(|part| matches!(part, Component::ParentDir)),
        "candidate evidence refuses parent traversal"
    );
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    })
}

impl Candidate {
    pub(super) fn capture(live: &Path, candidate: &Path) -> Result<Option<Self>> {
        if live == candidate {
            return Ok(None); // In-place finalization has only the existing live report.
        }
        let path = absolute(candidate)?;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => directory(&path)?,
        }
        let parent = path.parent().context("candidate evidence has no parent")?;
        directory(parent)?;
        let captured = Self {
            canonical: fs::canonicalize(&path)?,
            owner: same_file::Handle::from_path(&path)?,
            parent_owner: same_file::Handle::from_path(parent)?,
            path,
        };
        captured.check(live)?;
        Ok(Some(captured))
    }

    fn check(&self, live: &Path) -> Result<()> {
        directory(&self.path)?;
        let parent = self
            .path
            .parent()
            .context("candidate evidence has no parent")?;
        directory(parent)?;
        ensure!(
            self.owner == same_file::Handle::from_path(&self.path)?
                && self.parent_owner == same_file::Handle::from_path(parent)?
                && self.canonical == fs::canonicalize(&self.path)?,
            "candidate or its parent changed identity; evidence not written"
        );
        // Missing live is valid on a first rebuild; resolve only its existing
        // parent. Never manufacture a live index merely to save a diagnostic.
        let live = absolute(live)?;
        let live_canonical = match fs::symlink_metadata(&live) {
            Ok(_) => {
                directory(&live)?;
                ensure!(
                    self.owner != same_file::Handle::from_path(&live)?,
                    "candidate is now the live index; evidence not written"
                );
                fs::canonicalize(&live)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let parent = live.parent().context("live index has no parent")?;
                directory(parent)?;
                fs::canonicalize(parent)?.join(live.file_name().context("live index has no name")?)
            }
            Err(error) => return Err(error.into()),
        };
        ensure!(
            !self.canonical.starts_with(&live_canonical)
                && !live_canonical.starts_with(&self.canonical),
            "candidate and live directories overlap; evidence not written"
        );
        Ok(())
    }

    fn persist(
        &self,
        finalization: &Finalization<'_>,
        phase: &str,
        cause: &str,
    ) -> Result<Option<PathBuf>> {
        self.check(finalization.live)?;
        let destination = self.path.join(LAST_FAILURE_FILE);
        match fs::symlink_metadata(&destination) {
            Ok(_) => return Ok(None), // Preserve even malformed or ambiguous existing evidence.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut end = cause.len().min(MAX_CAUSE_BYTES);
        while !cause.is_char_boundary(end) {
            end -= 1;
        }
        let (build_path, live_path) = finalization.diagnostic_paths()?;
        let report = serde_json::json!({
            "version": 1,
            "record_scope": "candidate_first_failure",
            "at_ms": u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?,
            "process_id": std::process::id(),
            "phase": phase,
            "build_path": build_path,
            "live_path": live_path,
            "expected_docs": finalization.indexed_docs,
            "processed_conversations": finalization.processed_conversations,
            "error_chain": &cause[..end],
            "error_chain_truncated": end < cause.len(),
            "error_chain_original_bytes": cause.len(),
        });
        let bytes = serde_json::to_vec_pretty(&report)?;
        ensure!(
            bytes.len() <= MAX_RECORD_BYTES,
            "candidate failure record exceeds its byte limit"
        );
        self.check(finalization.live)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".lexical-rebuild-failure-")
            .tempfile_in(&self.path)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        self.check(finalization.live)?;
        // No clobber, including a concurrent first report, symlink or directory.
        // On platforms using link/unlink, a crash can leave a second temporary
        // link. Retention already refuses hard-linked/ambiguous bundles.
        temporary
            .persist_noclobber(&destination)
            .map_err(|error| error.error)?;
        #[cfg(unix)]
        fs::File::open(&self.path)?.sync_all()?;
        Ok(Some(destination))
    }
}

pub(super) fn record(finalization: &Finalization<'_>, phase: &str, cause: &str) {
    let candidate = match &finalization.candidate_evidence {
        Ok(Some(candidate)) => candidate,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(phase, error = %format!("{error:#}"),
                "candidate failure evidence was not admitted; preserving original finalization error");
            return;
        }
    };
    match candidate.persist(finalization, phase, cause) {
        Ok(Some(path)) => tracing::info!(phase, path = %path.display(),
            "persisted candidate-owned first-failure evidence"),
        Ok(None) => tracing::info!(
            phase,
            "preserved existing candidate failure evidence without replacement"
        ),
        Err(error) => tracing::warn!(phase, error = %format!("{error:#}"),
            "could not persist candidate failure evidence; preserving original finalization error"),
    }
}

#[cfg(test)]
mod tests;
