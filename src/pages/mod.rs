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
