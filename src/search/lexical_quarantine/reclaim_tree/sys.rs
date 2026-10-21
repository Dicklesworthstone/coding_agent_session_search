//! Safe Linux descriptor-relative filesystem operations through proc fd links.
//! A live File owns each descriptor throughout the operation. The proc magic
//! link resolves that open directory, not its possibly renamed pathname. Only
//! one checked child component is appended; artifact symlinks are not followed.
//! Missing/unusable proc fd access fails closed, with no path-based fallback.
#![forbid(unsafe_code)]

use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

fn component(bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one path component",
        ));
    }
    Ok(())
}

pub(super) fn name(value: &OsStr) -> io::Result<CString> {
    let bytes = value.as_bytes();
    component(bytes)?;
    CString::new(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in filename"))
}

fn child_path(parent: &File, name: &CStr) -> io::Result<PathBuf> {
    component(name.to_bytes())?;
    let descriptor = PathBuf::from(format!("/proc/thread-self/fd/{}", parent.as_raw_fd()));
    // Check the descriptor bridge itself. The File borrow keeps this fd alive,
    // so safe Rust cannot recycle it while the returned path is being used by
    // the operation below. A privileged actor replacing proc remains outside
    // the same operator-controlled-namespace contract as the mountinfo guard.
    let original = parent.metadata()?;
    let resolved = fs::metadata(&descriptor)?;
    if !original.is_dir()
        || !resolved.is_dir()
        || original.dev() != resolved.dev()
        || original.ino() != resolved.ino()
    {
        return Err(io::Error::other(
            "proc fd link does not name the owned quarantine directory",
        ));
    }
    Ok(descriptor.join(OsStr::from_bytes(name.to_bytes())))
}

pub(super) fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

pub(super) fn open_child(parent: &File, name: &CStr) -> io::Result<File> {
    open_directory(&child_path(parent, name)?)
}

pub(super) fn stat_child(parent: &File, name: &CStr) -> io::Result<fs::Metadata> {
    fs::symlink_metadata(child_path(parent, name)?)
}

pub(super) fn unlink_child(parent: &File, name: &CStr, directory: bool) -> io::Result<()> {
    let path = child_path(parent, name)?;
    if directory {
        fs::remove_dir(path) // Empty only; never recursively enumerate children.
    } else {
        fs::remove_file(path) // Does not follow a final artifact symlink.
    }
}
