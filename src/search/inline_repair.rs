//! Admit small search-triggered rebuilds without opening the canonical database.
//!
//! Serving an already-readable generation is not maintenance and must not use
//! this check. Explicit index commands also remain outside this search policy.
//! Recent canonical history may live almost entirely in the WAL. Count it, and
//! fail closed on unknown sizes rather than treating an I/O error as zero bytes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArchiveSize {
    pub database_bytes: u64,
    pub wal_bytes: u64,
    pub total_bytes: u64,
}

impl ArchiveSize {
    fn measured(database_bytes: u64, wal_bytes: u64) -> Result<Self> {
        let total_bytes = database_bytes
            .checked_add(wal_bytes)
            .context("canonical database plus WAL size overflowed the repair budget counter")?;
        Ok(Self {
            database_bytes,
            wal_bytes,
            total_bytes,
        })
    }

    fn admit(self, maximum_bytes: u64) -> Result<Self> {
        ensure!(
            self.total_bytes <= maximum_bytes,
            "search-triggered lexical rebuild was not started: canonical archive exceeds the \
             automatic-repair budget (database_bytes={}, wal_bytes={}, archive_bytes={}, \
             maximum_bytes={maximum_bytes}); run `cass index --full --json` with the same \
             --db and --data-dir as this search, then retry the query",
            self.database_bytes,
            self.wal_bytes,
            self.total_bytes,
        );
        Ok(self)
    }
}

fn wal_path(database_path: &Path) -> PathBuf {
    let mut path = database_path.as_os_str().to_os_string();
    path.push("-wal");
    PathBuf::from(path)
}

fn regular_file_size(path: &Path, missing_allowed: bool) -> Result<u64> {
    let entry = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if missing_allowed && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(0);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "search-triggered lexical rebuild was not started: cannot measure {}",
                    path.display()
                )
            });
        }
    };
    // A dangling WAL symlink is an unknown size, not a missing sidecar. Normal
    // file aliases remain usable, but a broken link must not bypass admission.
    let metadata = if entry.file_type().is_symlink() {
        std::fs::metadata(path)
            .with_context(|| format!("cannot measure archive symlink {}", path.display()))?
    } else {
        entry
    };
    ensure!(
        metadata.is_file(),
        "search-triggered lexical rebuild was not started: {} is not a regular archive file",
        path.display()
    );
    Ok(metadata.len())
}

/// A metadata-only admission check. This never opens SQLite, creates an index,
/// truncates a sidecar or acquires a writer. Recheck immediately before retrying
/// after a concurrent indexer; the archive can grow during the wait.
pub(crate) fn admit(database_path: &Path, maximum_bytes: u64) -> Result<ArchiveSize> {
    let canonical_path = std::fs::canonicalize(database_path).with_context(|| {
        format!(
            "search-triggered lexical rebuild was not started: cannot resolve {}",
            database_path.display()
        )
    })?;
    let database_before = regular_file_size(&canonical_path, false)?;
    let wal_bytes = regular_file_size(&wal_path(&canonical_path), true)?;
    // A checkpoint can move pages from WAL to the database between stats. Do
    // not pair an old small database size with the newly truncated WAL. Taking
    // the larger database observation may overestimate during checkpointing,
    // which is preferable to admitting heavyweight work on a query's behalf.
    let database_after = regular_file_size(&canonical_path, false)?;
    ArchiveSize::measured(database_before.max(database_after), wal_bytes)?.admit(maximum_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gh494_inline_repair_budget_includes_the_actual_wal_without_mutating_it() {
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join("archive.with.dots.sqlite");
        let wal = wal_path(&database);
        std::fs::write(&database, b"main").unwrap();
        std::fs::write(&wal, b"canonical history in WAL").unwrap();
        let before_database = std::fs::read(&database).unwrap();
        let before_wal = std::fs::read(&wal).unwrap();
        let size = admit(&database, 1024).unwrap();
        assert_eq!(size.database_bytes, before_database.len() as u64);
        assert_eq!(size.wal_bytes, before_wal.len() as u64);
        assert_eq!(size.total_bytes, size.database_bytes + size.wal_bytes);
        let message = admit(&database, size.database_bytes)
            .unwrap_err()
            .to_string();
        assert!(message.contains("was not started"), "{message}");
        assert!(
            message.contains(&format!("wal_bytes={}", before_wal.len())),
            "{message}"
        );
        assert!(message.contains("cass index --full --json"), "{message}");
        assert_eq!(std::fs::read(&database).unwrap(), before_database);
        assert_eq!(std::fs::read(&wal).unwrap(), before_wal);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 2);
    }

    #[test]
    fn gh494_inline_repair_budget_allows_the_exact_boundary_and_a_missing_wal() {
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join("archive.sqlite");
        std::fs::write(&database, b"four").unwrap();
        let size = admit(&database, 4).unwrap();
        assert_eq!(size.wal_bytes, 0);
        assert_eq!(size.total_bytes, 4);
        assert!(admit(&database, 3).is_err());
        assert!(!wal_path(&database).exists());
    }

    #[test]
    fn gh494_zero_inline_repair_budget_does_not_admit_nonempty_archives() {
        assert!(ArchiveSize::measured(0, 0).unwrap().admit(0).is_ok());
        assert!(ArchiveSize::measured(1, 0).unwrap().admit(0).is_err());
        assert!(ArchiveSize::measured(0, 1).unwrap().admit(0).is_err());
    }

    #[test]
    fn gh494_inline_repair_budget_cannot_wrap_when_database_and_wal_are_large() {
        assert!(ArchiveSize::measured(u64::MAX, 1).is_err());
        assert_eq!(
            ArchiveSize::measured(u64::MAX - 1, 1)
                .unwrap()
                .admit(u64::MAX)
                .unwrap()
                .total_bytes,
            u64::MAX
        );
    }

    #[test]
    fn gh494_unknown_database_size_is_not_treated_as_an_empty_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing.sqlite");
        let error = admit(&missing, u64::MAX).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(!missing.exists());
        assert!(admit(tmp.path(), u64::MAX).is_err());
    }

    #[test]
    fn gh494_unknown_wal_size_is_not_treated_as_absent_history() {
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join("archive.sqlite");
        std::fs::write(&database, b"main").unwrap();
        let wal = wal_path(&database);
        std::fs::create_dir(&wal).unwrap();
        let evidence = wal.join("keep");
        std::fs::write(&evidence, b"evidence").unwrap();
        assert!(admit(&database, u64::MAX).is_err());
        assert_eq!(std::fs::read(&evidence).unwrap(), b"evidence");
    }

    #[cfg(unix)]
    #[test]
    fn gh494_inline_repair_wal_path_preserves_non_utf8_database_names() {
        use std::os::unix::ffi::OsStringExt;
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join(std::ffi::OsString::from_vec(
            b"archive-\xff.sqlite".to_vec(),
        ));
        std::fs::write(&database, b"main").unwrap();
        std::fs::write(wal_path(&database), b"wal").unwrap();
        assert_eq!(admit(&database, 7).unwrap().total_bytes, 7);
        assert!(admit(&database, 6).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn gh494_inline_repair_counts_the_canonical_database_wal_through_an_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join("archive.sqlite");
        let alias = tmp.path().join("alias.sqlite");
        std::fs::write(&database, b"main").unwrap();
        std::fs::write(wal_path(&database), b"wal").unwrap();
        std::os::unix::fs::symlink(&database, &alias).unwrap();
        assert!(!wal_path(&alias).exists());
        assert_eq!(admit(&alias, 7).unwrap().wal_bytes, 3);
        assert!(admit(&alias, 4).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn gh494_dangling_wal_symlink_is_not_a_zero_byte_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let database = tmp.path().join("archive.sqlite");
        std::fs::write(&database, b"main").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("missing-wal"), wal_path(&database)).unwrap();
        assert!(admit(&database, u64::MAX).is_err());
    }
}
