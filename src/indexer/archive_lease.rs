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

impl Drop for ArchiveWriteLease {
    fn drop(&mut self) {
        // A cloned or fork-inherited descriptor shares the Unix flock's open
        // file description. Closing this descriptor alone can leave the next
        // indexer blocked after its logical owner has finished. End ownership
        // explicitly, without truncating, unlinking or replacing the sidecar.
        if let Err(error) = self._file.unlock() {
            tracing::warn!(
                error = %error,
                database_path = %self.database_path.display(),
                "failed to explicitly release canonical archive writer lease"
            );
        }
    }
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

    /// Filesystem and unsupported-lock errors stay errors. Dropping the lease
    /// explicitly unlocks before closing its handle, including on unwind.
    /// Parent creation matches first-index storage preparation; the database
    /// itself is never created here.
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn acquire(db_path: &Path) -> io::Result<ArchiveWriteLease> {
        match ArchiveWriteLease::try_acquire(db_path)? {
            ArchiveLeaseAttempt::Acquired(lease) => Ok(lease),
            ArchiveLeaseAttempt::Busy(path) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("archive writer lease unexpectedly busy: {}", path.display()),
            )),
        }
    }

    #[test]
    fn archive_lease_drop_releases_lock_while_cloned_descriptor_survives() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("archive.db");
        let lease = acquire(&db_path)?;
        let duplicate = lease._file.try_clone()?;
        assert!(matches!(
            ArchiveWriteLease::try_acquire(&db_path)?,
            ArchiveLeaseAttempt::Busy(_)
        ));
        drop(lease);
        let successor = acquire(&db_path)?;
        assert!(matches!(
            ArchiveWriteLease::try_acquire(&db_path)?,
            ArchiveLeaseAttempt::Busy(_)
        ));
        drop(duplicate);
        assert!(matches!(
            ArchiveWriteLease::try_acquire(&db_path)?,
            ArchiveLeaseAttempt::Busy(_)
        ));
        drop(successor);
        drop(acquire(&db_path)?);
        assert!(
            !db_path.exists(),
            "lease admission must not create the database"
        );
        Ok(())
    }

    #[test]
    fn archive_lease_unwind_releases_lock_while_cloned_descriptor_survives() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("archive.db");
        let lease = acquire(&db_path)?;
        let duplicate = lease._file.try_clone()?;
        let interrupted = std::panic::catch_unwind(move || {
            let _lease = lease;
            panic!("intentional archive owner interruption");
        });
        assert!(interrupted.is_err());
        let successor = acquire(&db_path)?;
        drop(duplicate);
        assert!(matches!(
            ArchiveWriteLease::try_acquire(&db_path)?,
            ArchiveLeaseAttempt::Busy(_)
        ));
        drop(successor);
        drop(acquire(&db_path)?);
        Ok(())
    }

    #[test]
    fn archive_lease_contenders_do_not_release_the_active_owner() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("archive.db");
        let lease = acquire(&db_path)?;
        let expected = ArchiveWriteLease::path(&db_path)?;
        for _ in 0..32 {
            match ArchiveWriteLease::try_acquire(&db_path)? {
                ArchiveLeaseAttempt::Busy(path) => assert_eq!(path, expected),
                ArchiveLeaseAttempt::Acquired(_) => {
                    panic!("a failed contender released the active writer's lock")
                }
            }
        }
        drop(lease);
        drop(acquire(&db_path)?);
        Ok(())
    }

    #[test]
    fn archive_lease_releases_without_replacing_sidecar_or_changing_archive() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("archive.db");
        fs::write(&db_path, b"canonical archive bytes remain untouched")?;
        let lock_path = ArchiveWriteLease::path(&db_path)?;
        fs::write(&lock_path, b"stable sidecar evidence")?;
        let before = fs::metadata(&lock_path)?;
        for _ in 0..8 {
            let lease = acquire(&db_path)?;
            let duplicate = lease._file.try_clone()?;
            drop(lease);
            let successor = acquire(&db_path)?;
            let after = fs::metadata(&lock_path)?;
            assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
            assert_eq!(fs::read(&lock_path)?, b"stable sidecar evidence");
            assert_eq!(
                fs::read(&db_path)?,
                b"canonical archive bytes remain untouched"
            );
            drop(duplicate);
            drop(successor);
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    const CHILD: &str = "indexer::archive_lease::process_tests::archive_lease_child";
    const ROOT_ENV: &str = "CASS_TEST_ARCHIVE_LEASE_ROOT";
    const MODE_ENV: &str = "CASS_TEST_ARCHIVE_LEASE_MODE";

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn command(root: &Path, mode: &str) -> io::Result<Command> {
        let mut command = Command::new(std::env::current_exe()?);
        command.args(["--ignored", "--exact", CHILD, "--nocapture"]);
        command.env(ROOT_ENV, root).env(MODE_ENV, mode);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        Ok(command)
    }

    fn finish(child: &mut ChildGuard) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.0.try_wait()? {
                assert!(status.success(), "archive lease child failed: {status}");
                return Ok(());
            }
            assert!(Instant::now() < deadline, "archive lease child timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn probe(root: &Path, mode: &str) -> io::Result<()> {
        finish(&mut ChildGuard(command(root, mode)?.spawn()?))
    }

    #[test]
    #[ignore = "subprocess fixture invoked explicitly by the lease handoff test"]
    fn archive_lease_child() -> io::Result<()> {
        let root = PathBuf::from(std::env::var_os(ROOT_ENV).expect("child fixture root"));
        let mode = std::env::var(MODE_ENV).expect("child fixture mode");
        let db_path = root.join("archive.db");
        if mode == "hold" {
            // stdin intentionally holds the parent's duplicated lease handle.
            // Exec's normal close-on-exec behavior must not discard this probe.
            fs::write(root.join("ready"), b"inherited descriptor is live")?;
            let deadline = Instant::now() + Duration::from_secs(10);
            while !root.join("release").try_exists()? {
                assert!(Instant::now() < deadline, "parent did not release child");
                std::thread::sleep(Duration::from_millis(5));
            }
        } else {
            let attempt = ArchiveWriteLease::try_acquire(&db_path)?;
            match (mode.as_str(), attempt) {
                ("busy", ArchiveLeaseAttempt::Busy(_)) => {}
                ("acquire", ArchiveLeaseAttempt::Acquired(_lease)) => {}
                _ => panic!("unexpected cross-process archive admission for {mode}"),
            }
        }
        Ok(())
    }

    #[test]
    fn archive_lease_handoff_with_live_inherited_descriptor_excludes_competing_processes()
    -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let db_path = root.join("archive.db");
        let ArchiveLeaseAttempt::Acquired(lease) = ArchiveWriteLease::try_acquire(&db_path)? else {
            panic!("initial archive owner must acquire");
        };
        let mut holder_command = command(root, "hold")?;
        holder_command.stdin(Stdio::from(lease._file.try_clone()?));
        let mut holder = ChildGuard(holder_command.spawn()?);
        drop(holder_command);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !root.join("ready").try_exists()? {
            assert!(holder.0.try_wait()?.is_none(), "holder exited before ready");
            assert!(Instant::now() < deadline, "holder did not become ready");
            std::thread::sleep(Duration::from_millis(5));
        }
        probe(root, "busy")?;
        drop(lease);
        assert!(
            holder.0.try_wait()?.is_none(),
            "inherited descriptor must survive owner drop"
        );
        probe(root, "acquire")?;
        let ArchiveLeaseAttempt::Acquired(successor) = ArchiveWriteLease::try_acquire(&db_path)?
        else {
            panic!("successor archive owner must acquire");
        };
        probe(root, "busy")?;
        fs::write(root.join("release"), b"release old inherited handle")?;
        finish(&mut holder)?;
        probe(root, "busy")?;
        drop(successor);
        probe(root, "acquire")?;
        assert!(
            !db_path.exists(),
            "probes must never create a canonical database"
        );
        Ok(())
    }
}
