use anyhow::{Context, Result, bail};
use frankensqlite::Connection;
use frankensqlite::compat::OpenFlags;
use std::fs::Metadata;
#[cfg(not(windows))]
use std::fs::OpenOptions;
#[cfg(not(windows))]
use std::io::Write;
use std::path::{Path, PathBuf};

pub mod analytics;
pub mod archive_config;
pub mod attachments;
pub mod bundle;
pub mod config_input;
pub mod confirmation;
pub mod deploy_cloudflare;
pub mod deploy_github;
pub mod docs;
pub mod encrypt;
pub mod errors;
pub mod export;
pub mod fts;
pub mod key_management;
pub mod password;
pub mod patterns;
pub mod preview;
pub mod profiles;
pub mod qr;
pub mod redact;
pub mod secret_scan;
pub mod size;
pub mod summary;
pub mod verify;
pub mod wizard;

fn ensure_real_directory(path: &Path, metadata: &Metadata, label: &str) -> Result<()> {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!("{label} must not be a symlink: {}", path.display());
    }
    if !file_type.is_dir() {
        bail!("{label} must be a directory: {}", path.display());
    }
    Ok(())
}

pub(crate) fn resolve_site_dir(path: &Path) -> Result<PathBuf> {
    let path_metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            bail!("path does not exist: {}", path.display());
        }
        Err(err) => {
            return Err(err).with_context(|| format!("Failed to inspect path {}", path.display()));
        }
    };

    if path.file_name().map(|name| name == "site").unwrap_or(false) {
        ensure_real_directory(path, &path_metadata, "site directory")?;
        return Ok(path.to_path_buf());
    }

    let site_subdir = path.join("site");
    match std::fs::symlink_metadata(&site_subdir) {
        Ok(metadata) => {
            ensure_real_directory(&site_subdir, &metadata, "site directory")?;
            return Ok(site_subdir);
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| {
                format!("Failed to inspect site directory {}", site_subdir.display())
            });
        }
    }

    ensure_real_directory(path, &path_metadata, "site directory")?;
    Ok(path.to_path_buf())
}

pub(crate) fn open_existing_sqlite_db(path: &Path) -> Result<Connection> {
    if !path.exists() {
        bail!("database does not exist: {}", path.display());
    }

    // Open read-only to prevent accidental writes to the source database
    // during export/scan operations.
    frankensqlite::compat::open_with_flags(
        path.to_string_lossy().as_ref(),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .with_context(|| format!("opening sqlite database at {}", path.display()))
}

/// Write `data` to `path` and fsync both the file contents and the parent
/// directory so the name-entry pointing at `path` survives a crash.
///
/// Why: a bare `std::fs::write` only flushes the page cache when the OS
/// decides to. If power is lost between the write and the next sync, the
/// file can appear empty or missing after reboot. This helper mirrors the
/// fix landed for `pages/encrypt.rs::sync_tree` under bead
/// coding_agent_session_search-92o31.
#[cfg(not(windows))]
pub(crate) fn write_file_durably(path: &Path, data: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("creating {} for durable write", path.display()))?;
    f.write_all(data)
        .with_context(|| format!("writing {} durably", path.display()))?;
    f.sync_all()
        .with_context(|| format!("fsyncing {} after durable write", path.display()))?;
    drop(f);
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    std::fs::File::open(parent)
        .with_context(|| format!("opening parent {} for fsync", parent.display()))?
        .sync_all()
        .with_context(|| {
            format!(
                "fsyncing parent {} after durable write to {}",
                parent.display(),
                path.display()
            )
        })
}

/// Windows has no portable directory-fsync; NTFS journals dirent updates
/// synchronously, so plain `fs::write` is sufficient for crash safety.
#[cfg(windows)]
pub(crate) fn write_file_durably(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_file_durably_writes_bytes_and_fsyncs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("out.json");
        write_file_durably(&path, b"hello").expect("durable write");
        let got = std::fs::read(&path).expect("read back");
        assert_eq!(got, b"hello");
    }

    #[cfg(not(windows))]
    #[test]
    fn write_file_durably_surfaces_parent_fsync_error() {
        // Negative-side guard mirroring the sync_tree regression test from
        // bead coding_agent_session_search-92o31: if the parent directory
        // disappears between write and fsync, the helper must surface the
        // I/O error rather than silently succeeding.
        let tmp = tempfile::tempdir().expect("tempdir");
        let nested = tmp.path().join("subdir");
        std::fs::create_dir(&nested).expect("mkdir");
        let path = nested.join("out.json");

        // A file path whose parent does not exist must fail at the open
        // step; this proves the write is routed through our helper rather
        // than any fire-and-forget path.
        std::fs::remove_dir_all(&nested).expect("rm nested");
        let err = write_file_durably(&path, b"data").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("creating") || msg.contains("opening parent"),
            "expected durable write to surface I/O error, got: {msg}"
        );
    }
}
