//! Connector for ZCode CLI and Desktop's shared SQLite session store.
//!
//! Parsing, discovery and token extraction live in `franken_agent_detection`.

pub use franken_agent_detection::ZCodeConnector;

use std::path::{Path, PathBuf};

/// Recognize the native store and explicit ZCode overrides without opening
/// any files. This boundary also covers non-transcript files in that store.
pub(crate) fn owns_path(path: &Path) -> bool {
    let native_store = |path: &Path| {
        path.components().any(|part| {
            part.as_os_str()
                .to_str()
                .is_some_and(|part| part.eq_ignore_ascii_case(".zcode"))
        })
    };
    if native_store(path)
        || dunce::canonicalize(path).is_ok_and(|canonical| native_store(&canonical))
    {
        return true;
    }
    if let Some(storage) = env_path("ZCODE_STORAGE_DIR")
        && path_within(path, &storage)
    {
        return true;
    }
    if let Some(database) =
        env_path("ZCODE_SESSION_DB_PATH").or_else(|| env_path("ZCODE_SESSION_DB"))
    {
        if path_within(path, &database) {
            return true;
        }
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut sidecar = database.as_os_str().to_os_string();
            sidecar.push(suffix);
            if path_within(path, Path::new(&sidecar)) {
                return true;
            }
        }
    }
    false
}

fn env_path(name: &str) -> Option<PathBuf> {
    let value = dotenvy::var(name).ok()?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let expanded = if value == "~" {
        dirs::home_dir()?
    } else if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        dirs::home_dir()?.join(rest)
    } else {
        PathBuf::from(value)
    };
    Some(std::path::absolute(&expanded).unwrap_or(expanded))
}

fn path_within(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
        || dunce::canonicalize(path)
            .ok()
            .is_some_and(|path| dunce::canonicalize(root).is_ok_and(|root| path.starts_with(root)))
}
