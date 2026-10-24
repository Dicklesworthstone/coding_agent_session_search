//! Metadata-only Continue discovery with explicit failure and mirror ownership.
//!
//! Match the pinned FAD root layout and source identities, but do not turn
//! read_dir/stat/canonicalization failures into an empty successful inventory.
//! Parsing stays in FAD, through one explicit file scope per admitted source.
//! This inventory is not an atomic directory snapshot: later source changes
//! must still pass admission and completion checks in the caller.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::connectors::{
    DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot, file_modified_since,
};

pub(super) struct Source {
    pub(super) root: ScanRoot,
    pub(super) file: DiscoveredSourceFile,
}

/// Continue treats spaces and '~' literally in this override; relative values
/// are resolved against CWD. Do not apply the trimming/tilde rules of Codex.
fn global_dir() -> Result<Option<PathBuf>> {
    let configured = match dotenvy::var("CONTINUE_GLOBAL_DIR") {
        Ok(value) if !value.is_empty() => Some(PathBuf::from(value)),
        Ok(_) | Err(dotenvy::Error::EnvVar(std::env::VarError::NotPresent)) => None,
        Err(error) => return Err(error).context("read CONTINUE_GLOBAL_DIR"),
    };
    match configured {
        Some(path) if path.is_relative() => Ok(Some(
            std::env::current_dir()
                .context("resolve relative CONTINUE_GLOBAL_DIR")?
                .join(path),
        )),
        Some(path) => Ok(Some(path)),
        None => Ok(dirs::home_dir().map(|path| path.join(".continue"))),
    }
}

fn default_root(ctx: &ScanContext) -> Result<Option<PathBuf>> {
    let global = global_dir()?;
    let data = &ctx.data_dir;
    let native_data = data.file_name().is_some_and(|name| name == ".continue")
        || (data.file_name().is_some_and(|name| name == "sessions")
            && data
                .parent()
                .is_some_and(|parent| parent.file_name().is_some_and(|name| name == ".continue")))
        || global
            .as_ref()
            .is_some_and(|root| data == root || *data == root.join("sessions"));
    Ok(if native_data {
        Some(data.clone())
    } else {
        global.map(|root| root.join("sessions"))
    })
}

fn optional_directory(path: &Path) -> Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error)
            .with_context(|| format!("inspect Continue session directory {}", path.display())),
    }
}

fn mirror_root(root: &ScanRoot) -> Result<Option<ScanRoot>> {
    let Some(source) = &root.mirrored_source_path else {
        return Ok(Some(root.clone()));
    };
    // Never guess ownership from the local hashed container name, a generic
    // sessions directory, or a Continue-looking JSON payload from another agent.
    let parts: Vec<_> = source
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect();
    if !parts.ends_with(&[".continue"]) && !parts.ends_with(&[".continue", "sessions"]) {
        return Ok(None);
    }
    ensure!(
        root.origin.is_remote(),
        "Continue mirror metadata requires a remote origin"
    );
    let path = root
        .mirrored_store(&[".continue", "sessions"])
        .with_context(|| {
            format!(
                "Continue mirror has no readable, non-symlink session store beneath {}",
                root.path.display()
            )
        })?;
    Ok(Some(root.with_path(path)))
}

fn roots(ctx: &ScanContext, excludes: &dyn Fn(&Path) -> bool) -> Result<Vec<ScanRoot>> {
    let defaults = ctx.use_default_detection();
    let bases = if defaults {
        default_root(ctx)?
            .map(ScanRoot::local)
            .into_iter()
            .collect()
    } else {
        ctx.scan_roots.clone()
    };
    let mut expanded = Vec::new();
    for base in bases {
        if excludes(&base.path) {
            continue;
        }
        let Some(base) = mirror_root(&base)? else {
            continue;
        };
        if excludes(&base.path) {
            continue;
        }
        let metadata = match fs::metadata(&base.path) {
            Ok(metadata) => metadata,
            // Not having Continue installed is normal in default discovery.
            // An explicitly selected missing root is not a complete inventory.
            Err(error) if defaults && error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect selected Continue root {}", base.path.display())
                });
            }
        };
        if metadata.is_file() {
            expanded.push(base);
            continue;
        }
        ensure!(
            metadata.is_dir(),
            "Continue root is not a regular file or directory: {}",
            base.path.display()
        );
        // Preserve FAD's one-level home/profile expansion, including both
        // stores when a broad explicit profile contains them. Never recurse
        // through unrelated subdirectories of a renamed sessions directory.
        let mut found = false;
        for path in [
            base.path.join(".continue/sessions"),
            base.path.join("sessions"),
        ] {
            if optional_directory(&path)? {
                // An existing excluded store still determines the layout.
                // Do not fall back to reading JSON files in its parent.
                found = true;
                if !excludes(&path) {
                    expanded.push(base.with_path(path));
                }
            }
        }
        if !found {
            expanded.push(base);
        }
    }
    expanded.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(expanded)
}

fn session_name(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "json")
        && path.file_name().is_some_and(|name| name != "sessions.json")
}

pub(super) fn discover(ctx: &ScanContext, excludes: &dyn Fn(&Path) -> bool) -> Result<Vec<Source>> {
    let mut sources = Vec::new();
    let mut seen = HashSet::new();
    for root in roots(ctx, excludes)? {
        if let Some(tick) = &ctx.progress_tick {
            tick();
        }
        let metadata = fs::metadata(&root.path)
            .with_context(|| format!("recheck Continue discovery root {}", root.path.display()))?;
        let mut files = if metadata.is_file() {
            vec![root.path.clone()]
        } else {
            let entries = fs::read_dir(&root.path)
                .with_context(|| format!("list Continue sessions {}", root.path.display()))?;
            let mut files = Vec::new();
            for entry in entries {
                files.push(
                    entry
                        .with_context(|| {
                            format!("enumerate Continue sessions {}", root.path.display())
                        })?
                        .path(),
                );
                if files.len() % 128 == 0
                    && let Some(tick) = &ctx.progress_tick
                {
                    tick();
                }
            }
            files
        };
        files.sort();
        for (ordinal, path) in files.into_iter().enumerate() {
            if ordinal % 128 == 0
                && let Some(tick) = &ctx.progress_tick
            {
                tick();
            }
            if !session_name(&path) || excludes(&path) {
                continue;
            }
            let metadata = fs::metadata(&path)
                .with_context(|| format!("inspect Continue snapshot {}", path.display()))?;
            if !metadata.is_file() {
                continue;
            }
            // Check metadata errors before calling the shared cutoff helper,
            // whose historical best-effort fallback is otherwise permissive.
            metadata
                .modified()
                .context("read Continue snapshot modification time")?;
            if !file_modified_since(&path, ctx.since_ts) {
                continue;
            }
            let path = fs::canonicalize(&path)
                .with_context(|| format!("resolve Continue snapshot {}", path.display()))?;
            if excludes(&path) || !seen.insert(path.clone()) {
                continue;
            }
            let file = DiscoveredSourceFile::new(
                "continue",
                &root,
                path,
                DiscoveredSourceRole::PrimarySessionLog,
                true,
            )
            .with_fs_metadata();
            ensure!(
                file.size_bytes.is_some() && file.modified_at_ms.is_some(),
                "Continue snapshot became unavailable during discovery: {}",
                file.source_path.display()
            );
            sources.push(Source {
                root: root.clone(),
                file,
            });
        }
    }
    Ok(sources)
}

#[cfg(test)]
mod tests;
