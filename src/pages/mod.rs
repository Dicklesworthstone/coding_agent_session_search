use anyhow::{Context, Result, bail};
use frankensqlite::Connection;
use std::path::Path;

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

pub(crate) fn open_existing_sqlite_db(path: &Path) -> Result<Connection> {
    if !path.exists() {
        bail!("database does not exist: {}", path.display());
    }

    Connection::open(path.to_string_lossy().as_ref())
        .with_context(|| format!("opening sqlite database at {}", path.display()))
}
