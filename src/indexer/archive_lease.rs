//! Cross-data-directory writer admission for one canonical archive pathname.
//!
//! Resolve the original filesystem path before interpreting `..`, including
//! dangling final symlinks and missing ordinary parents. The sidecar is never
//! rewritten, renamed or removed: its open inode owns the OS lock and carries
//! no heartbeat or active-job metadata. Database replacement by a cooperating
//! owner keeps the same pathname lease throughout preparation and cleanup.
//!
//! This is pathname coordination, not inode-wide hard-link coordination.
//! Different hard-link names and external replacement of a symlink or the
//! sidecar itself are outside this lease's identity contract.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(super) struct ArchiveWriteLease {
    _file: File,
    database_path: PathBuf,
}

pub(super) enum ArchiveLeaseAttempt {
    Acquired(ArchiveWriteLease),
    Busy(PathBuf),
}

fn resolve_database_path(db_path: &Path, create_parents: bool) -> io::Result<PathBuf> {
    let mut unresolved = if db_path.is_absolute() {
        db_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(db_path)
    };
    // A dangling final symlink can lead to another dangling symlink. Bound
    // that traversal independently of the platform's canonicalize limit.
    for _ in 0..64 {
        let parent = unresolved.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "archive path has no parent")
        })?;
        let name = unresolved.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "archive path has no file name")
        })?;
        if create_parents {
            fs::create_dir_all(parent)?;
        }
        match fs::canonicalize(&unresolved) {
            Ok(resolved) => return Ok(resolved),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match fs::symlink_metadata(&unresolved) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = fs::read_link(&unresolved)?;
                unresolved = if target.is_absolute() {
                    target
                } else {
                    parent.join(target)
                };
            }
            Ok(_) => return Ok(fs::canonicalize(parent)?.join(name)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(fs::canonicalize(parent)?.join(name));
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "archive path exceeds the symbolic-link resolution limit",
    ))
}

/// Resolve identity without creating a database, directory or lease file.
pub(super) fn canonical_database_path(db_path: &Path) -> io::Result<PathBuf> {
    resolve_database_path(db_path, false)
}

fn lease_path(canonical_db_path: &Path) -> PathBuf {
    let mut path = canonical_db_path.as_os_str().to_os_string();
    path.push(".cass-index-run.lock");
    PathBuf::from(path)
}

impl ArchiveWriteLease {
    #[cfg(test)]
    pub(super) fn path(db_path: &Path) -> io::Result<PathBuf> {
        Ok(lease_path(&canonical_database_path(db_path)?))
    }

    pub(super) fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Filesystem and unsupported-lock errors stay errors. Dropping the only
    /// file handle releases the OS lock. Parent creation matches first-index
    /// storage preparation; the database itself is never created here.
    pub(super) fn try_acquire(db_path: &Path) -> io::Result<ArchiveLeaseAttempt> {
        let database_path = resolve_database_path(db_path, true)?;
        let path = lease_path(&database_path);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(ArchiveLeaseAttempt::Acquired(Self {
                _file: file,
                database_path,
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(ArchiveLeaseAttempt::Busy(path)),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }
}
