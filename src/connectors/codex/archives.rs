//! Include Codex's flat archived_sessions collection without widening narrow
//! session/file roots or changing the existing active-session cutoff.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;

use super::{Connector, DiscoveredSourceFile, Result, ScanContext, ScanRoot};

const ARCHIVE: &str = "archived_sessions";

fn default_home(ctx: &ScanContext) -> PathBuf {
    // Retain FAD's legacy data_dir-as-Codex-root override.
    let is_codex = ctx.data_dir.to_str().is_some_and(|path| {
        path.contains(".codex") || path.ends_with("/codex") || path.ends_with("\\codex")
    });
    if is_codex && ctx.data_dir.join("sessions").exists() {
        return ctx.data_dir.clone();
    }
    dotenvy::var("CODEX_HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".codex"))
}

fn archive_candidates(ctx: &ScanContext) -> Vec<ScanRoot> {
    let bases = if ctx.use_default_detection() {
        vec![ScanRoot::local(default_home(ctx))]
    } else {
        ctx.scan_roots.clone()
    };
    let mut roots = Vec::new();
    for base in bases {
        if base.path.is_file() {
            continue;
        }
        roots.push(base.with_path(base.path.join(ARCHIVE)));
        let under_codex = base
            .path
            .ancestors()
            .any(|path| path.file_name().is_some_and(|name| name == ".codex"));
        if !under_codex {
            roots.push(base.with_path(base.path.join(".codex").join(ARCHIVE)));
        }
    }
    roots
}

fn path_key(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(super) fn discover(
    inner: &dyn Connector,
    ctx: &ScanContext,
) -> Result<Vec<DiscoveredSourceFile>> {
    let mut sources = inner.discover_source_files(ctx)?;
    let mut seen = HashSet::new();
    sources.retain(|source| seen.insert(path_key(&source.source_path)));
    let mut seen_roots = HashSet::new();
    for root in archive_candidates(ctx) {
        if !seen_roots.insert(path_key(&root.path)) {
            continue;
        }
        match fs::metadata(&root.path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(error) => {
                return Err(error).context("inspect Codex archive collection");
            }
        }
        // Moving a rollout to the archive does not give its contents a new
        // mtime. A global cutoff cannot establish whether it was ever indexed.
        // Let the source-boundary ledger reuse unchanged archived files instead.
        let mut scope = ScanContext::with_roots(ctx.data_dir.clone(), vec![root], None);
        scope.progress_tick.clone_from(&ctx.progress_tick);
        for source in inner.discover_source_files(&scope)? {
            if seen.insert(path_key(&source.source_path)) {
                sources.push(source);
            }
        }
    }
    Ok(sources)
}

/// Preserve the full-home active-session identity when Codex archives a native
/// rollout. Explicit archive/file roots retain their existing scoped identities.
/// Non-native archive names use an archive namespace, not an invented date.
pub(super) fn session_id(ctx: &ScanContext, source: &DiscoveredSourceFile) -> Option<String> {
    if !source
        .source_path
        .ancestors()
        .any(|path| path.file_name().is_some_and(|name| name == ARCHIVE))
    {
        return None;
    }
    let root = archive_candidates(ctx).into_iter().find(|root| {
        root.origin == source.origin && source.source_path.starts_with(&root.path)
    })?;
    let relative = source.source_path.strip_prefix(&root.path).ok()?;
    if relative.components().count() == 1
        && let Some(id) = native_session_id(&source.source_path)
    {
        return Some(id);
    }
    Path::new(ARCHIVE)
        .join(relative.with_extension(""))
        .to_str()
        .map(str::to_owned)
}

fn native_session_id(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let suffix = stem.strip_prefix("rollout-")?;
    let timestamp = suffix.get(..19)?;
    chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H-%M-%S").ok()?;
    let uuid = suffix.get(19..)?.strip_prefix('-')?;
    if uuid.len() != 36
        || !uuid.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return None;
    }
    Path::new(timestamp.get(..4)?)
        .join(timestamp.get(5..7)?)
        .join(timestamp.get(8..10)?)
        .join(stem)
        .to_str()
        .map(str::to_owned)
}

#[cfg(test)]
mod tests;
