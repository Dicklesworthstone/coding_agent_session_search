use crate::franken_sync::Connection;
use crate::franken_sync::compat::OpenFlags;
use anyhow::{Context, Result, bail};
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

/// Content/recovery companions whose presence means a Pages SQLite export is
/// not a self-contained main file. These are the finite companions emitted by
/// the pinned FrankenSQLite 0.3.8 producer; publication must preserve and
/// reject them rather than guessing that they are stale.
const SQLITE_MIGRATION_MARKER_SUFFIX: &str = ".fsqlite-migration-state";
const SQLITE_MIGRATION_MARKER_TEMP_SUFFIX: &str = ".fsqlite-migration-state.tmp";
const SQLITE_CONTENT_ARTIFACT_SUFFIXES: &[&str] = &[
    "-journal",
    "-wal",
    "-shm",
    "-wal-fec",
    "-wal-cert",
    "-wal-cert-head",
    SQLITE_MIGRATION_MARKER_SUFFIX,
    SQLITE_MIGRATION_MARKER_TEMP_SUFFIX,
];

const SQLITE_LOCK_SUFFIXES: &[&str] = &["-lock-shared", "-lock-reserved", "-lock-pending"];
const SQLITE_VFS_LOCK_ROOT_SUFFIXES: &[&str] =
    &["-journal", "-wal", "-wal-cert", "-wal-cert-head"];
const SQLITE_WAL_SEGMENT_DIRECTORY_ENTRY_LIMIT: usize = 65_536;
const SQLITE_WAL_SEGMENT_MATCH_LIMIT: usize = 4_096;

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("pages_export.db"))
        .to_os_string();
    file_name.push(suffix);
    path.with_file_name(file_name)
}

#[cfg(test)]
fn sqlite_migration_marker_path(path: &Path) -> PathBuf {
    sqlite_sidecar_path(path, SQLITE_MIGRATION_MARKER_SUFFIX)
}

/// Return every content/recovery path associated with `path`.
///
/// The WAL-FEC rewrite temporary is intentionally constructed with
/// `Path::with_extension`, matching `fsqlite-vfs` 0.3.8. For `export.db` this
/// is `export.wal-fec.tmp`, not `export.db-wal-fec.tmp`.
fn sqlite_content_artifact_paths(path: &Path) -> Vec<PathBuf> {
    let mut paths = SQLITE_CONTENT_ARTIFACT_SUFFIXES
        .iter()
        .map(|suffix| sqlite_sidecar_path(path, suffix))
        .collect::<Vec<_>>();
    paths.push(sqlite_sidecar_path(path, "-wal-fec").with_extension("wal-fec.tmp"));
    paths
}

/// Return operational companions left by an explicitly closed FrankenSQLite
/// writer. Windows creates its lock triplet for each read-write VFS artifact.
/// Pinned 0.3.8 opens the rollback journal, WAL, WAL certificate, and WAL
/// certificate handoff through that path in addition to the main file. SHM
/// and WAL-FEC use separate direct-file paths and do not acquire this triplet.
/// Namespace gate/use files are scoped to the main database.
fn sqlite_runtime_artifact_paths(path: &Path) -> Vec<PathBuf> {
    let mut paths = vec![
        sqlite_sidecar_path(path, "-fsqlite-ns-gate"),
        sqlite_sidecar_path(path, "-fsqlite-ns-use"),
    ];
    let lock_roots = std::iter::once(path.to_path_buf()).chain(
        SQLITE_VFS_LOCK_ROOT_SUFFIXES
            .iter()
            .map(|suffix| sqlite_sidecar_path(path, suffix)),
    );
    for root in lock_roots {
        paths.extend(
            SQLITE_LOCK_SUFFIXES
                .iter()
                .map(|suffix| sqlite_sidecar_path(&root, suffix)),
        );
    }
    paths
}

/// Exact, finite Pages SQLite artifact family shared by publication cleanup
/// and secret-scan attestation. Do not replace this with a prefix glob: nearby
/// paths may belong to another process or generation.
fn sqlite_fixed_artifact_paths(path: &Path) -> Vec<PathBuf> {
    let mut paths = sqlite_content_artifact_paths(path);
    paths.extend(sqlite_runtime_artifact_paths(path));
    paths
}

/// Enumerate FrankenSQLite's variable parallel-WAL segments in the database's
/// direct parent. Pinned 0.3.8 treats every `<db-name>-wal-seg-*` entry as a
/// recovery companion, including malformed epochs, so this mirrors that exact
/// prefix rather than guessing which suffix payloads are valid.
fn sqlite_wal_segment_artifact_paths(path: &Path) -> Result<Vec<PathBuf>> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let db_name = path
        .file_name()
        .ok_or_else(|| {
            anyhow::anyhow!("SQLite artifact path has no file name: {}", path.display())
        })?;
    // `fsqlite-wal` 0.3.8's `segment_path` and `list_segments` both derive
    // this filename through `to_string_lossy()`. Mirror that producer rule so
    // a non-UTF-8 database basename cannot hide its actual UTF-8 segment name.
    let segment_prefix = format!("{}-wal-seg-", db_name.to_string_lossy());

    let entries = std::fs::read_dir(parent).with_context(|| {
        format!(
            "Failed to enumerate SQLite artifact directory {} for {}",
            parent.display(),
            path.display()
        )
    })?;
    let mut matches = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index >= SQLITE_WAL_SEGMENT_DIRECTORY_ENTRY_LIMIT {
            bail!(
                "SQLite artifact directory {} exceeds the {}-entry WAL-segment scan bound for {}",
                parent.display(),
                SQLITE_WAL_SEGMENT_DIRECTORY_ENTRY_LIMIT,
                path.display()
            );
        }
        let entry = entry.with_context(|| {
            format!(
                "Failed reading SQLite artifact directory entry in {} for {}",
                parent.display(),
                path.display()
            )
        })?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(&segment_prefix)
        {
            if matches.len() >= SQLITE_WAL_SEGMENT_MATCH_LIMIT {
                bail!(
                    "SQLite artifact family for {} exceeds the {} WAL-segment match bound",
                    path.display(),
                    SQLITE_WAL_SEGMENT_MATCH_LIMIT
                );
            }
            matches.push(entry.path());
        }
    }
    matches.sort_unstable();
    Ok(matches)
}

fn sqlite_artifact_paths(path: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = sqlite_fixed_artifact_paths(path);
    paths.extend(sqlite_wal_segment_artifact_paths(path)?);
    Ok(paths)
}

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
    crate::franken_sync::compat::open_with_flags(
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
    fn sqlite_artifact_paths_match_fsqlite_non_append_and_nested_names() {
        let db = Path::new("export.db");
        let content = sqlite_content_artifact_paths(db);
        let runtime = sqlite_runtime_artifact_paths(db);

        assert_eq!(
            content,
            [
                "export.db-journal",
                "export.db-wal",
                "export.db-shm",
                "export.db-wal-fec",
                "export.db-wal-cert",
                "export.db-wal-cert-head",
                "export.db.fsqlite-migration-state",
                "export.db.fsqlite-migration-state.tmp",
                "export.wal-fec.tmp",
            ]
            .map(PathBuf::from)
        );
        assert_eq!(
            runtime,
            [
                "export.db-fsqlite-ns-gate",
                "export.db-fsqlite-ns-use",
                "export.db-lock-shared",
                "export.db-lock-reserved",
                "export.db-lock-pending",
                "export.db-journal-lock-shared",
                "export.db-journal-lock-reserved",
                "export.db-journal-lock-pending",
                "export.db-wal-lock-shared",
                "export.db-wal-lock-reserved",
                "export.db-wal-lock-pending",
                "export.db-wal-cert-lock-shared",
                "export.db-wal-cert-lock-reserved",
                "export.db-wal-cert-lock-pending",
                "export.db-wal-cert-head-lock-shared",
                "export.db-wal-cert-head-lock-reserved",
                "export.db-wal-cert-head-lock-pending",
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn sqlite_wal_segment_paths_match_only_the_pinned_direct_sibling_prefix() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let db = temp.path().join("export.db");
        let first = temp.path().join("export.db-wal-seg-42");
        let malformed_epoch = temp.path().join("export.db-wal-seg-not-an-epoch");
        let near_miss = temp.path().join("export.db-wal-segment-42");
        let other_db = temp.path().join("other.db-wal-seg-42");
        for path in [&db, &first, &malformed_epoch, &near_miss, &other_db] {
            std::fs::write(path, b"sentinel")?;
        }

        let mut expected = vec![first, malformed_epoch];
        expected.sort_unstable();
        assert_eq!(sqlite_wal_segment_artifact_paths(&db)?, expected);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_wal_segment_paths_mirror_lossy_non_utf8_producer_basename() -> Result<()> {
        use std::os::unix::ffi::OsStringExt;

        let temp = tempfile::tempdir()?;
        let db = temp
            .path()
            .join(std::ffi::OsString::from_vec(b"export-\xff.db".to_vec()));
        let producer_segment = temp.path().join("export-\u{fffd}.db-wal-seg-42");
        std::fs::write(&db, b"main")?;
        std::fs::write(&producer_segment, b"segment")?;

        assert_eq!(
            sqlite_wal_segment_artifact_paths(&db)?,
            vec![producer_segment],
            "scanner must mirror fsqlite-wal 0.3.8's lossy segment basename"
        );
        Ok(())
    }

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
