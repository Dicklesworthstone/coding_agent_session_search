//! Own the indexer's permanent data-directory lock, without rewriting heartbeat
//! metadata or acquiring/opening the canonical archive. Drop releases the lock.

use super::*;
use std::fs::{File, OpenOptions};

pub(super) struct Lease {
    _file: File,
    file_owner: same_file::Handle,
    directory_owner: same_file::Handle,
    directory: PathBuf,
    path: PathBuf,
}

fn regular_lock(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::other(
            "index-run lock is not a regular non-symlink file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(io::Error::other("index-run lock must not be hard-linked"));
        }
    }
    Ok(())
}

impl Lease {
    pub(super) fn acquire(directory: &Path) -> Result<Self, CleanupError> {
        real_directory(directory)?;
        let directory_owner = same_file::Handle::from_path(directory)?;
        let path = directory.join("index-run.lock");
        match regular_lock(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .mode(0o600);
        }
        let file = options.open(&path)?;
        if let Err(error) = fs2::FileExt::try_lock_exclusive(&file) {
            return Err(if error.kind() == io::ErrorKind::WouldBlock {
                CleanupError::Busy
            } else {
                error.into()
            });
        }
        let file_owner = same_file::Handle::from_file(file.try_clone()?)?;
        let lease = Self {
            _file: file,
            file_owner,
            directory_owner,
            directory: directory.to_owned(),
            path,
        };
        lease.check()?;
        Ok(lease)
    }

    pub(super) fn check(&self) -> io::Result<()> {
        real_directory(&self.directory)?;
        regular_lock(&self.path)?;
        if self.directory_owner != same_file::Handle::from_path(&self.directory)?
            || self.file_owner != same_file::Handle::from_path(&self.path)?
        {
            return Err(io::Error::other(
                "index-run lock or data directory changed identity",
            ));
        }
        Ok(())
    }
}
