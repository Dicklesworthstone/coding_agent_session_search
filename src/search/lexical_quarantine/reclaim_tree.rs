//! Linux removal of only the entries captured by the locked target recheck.
//!
//! Never re-enumerate and recursively delete a changed tree. Directory-relative
//! operations keep traversal bound to opened, identity-checked directories. New
//! children cause rmdir to fail rather than expanding the approved deletion.
//! The index-run lock excludes cooperating writers; lstat/unlink is not atomic
//! against a non-cooperating writer replacing an entry between those calls.

use super::{Artifact, Budget, mounts, observe_recording, real_directory};
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

mod sys;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    fn matches(self, metadata: &fs::Metadata) -> bool {
        self == Self::of(metadata)
    }
}

#[derive(Debug)]
struct Node {
    identity: Identity,
    directory: bool,
    // Directory times change as our own children are unlinked. File metadata
    // must still match the recheck exactly; payload bytes are never opened.
    file_version: Option<(u64, i64, i64, i64, i64)>,
}

impl Node {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            identity: Identity::of(metadata),
            directory: metadata.is_dir(),
            file_version: metadata.is_file().then(|| {
                (
                    metadata.len(),
                    metadata.mtime(),
                    metadata.mtime_nsec(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            }),
        }
    }

    fn matches(&self, metadata: &fs::Metadata) -> bool {
        if !self.identity.matches(metadata) {
            return false;
        }
        if self.directory {
            return metadata.is_dir();
        }
        metadata.is_file()
            && metadata.nlink() == 1
            && self.file_version
                == Some((
                    metadata.len(),
                    metadata.mtime(),
                    metadata.mtime_nsec(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                ))
    }
}

pub(super) struct Plan {
    nodes: BTreeMap<PathBuf, Node>,
}

impl Plan {
    /// Capture during the existing recheck, not through a second walk. Nothing
    /// captured here is a public cleanup capability or a persisted path list.
    pub(super) fn observe(
        artifact: &Artifact,
        now: SystemTime,
        budget: &mut Budget,
    ) -> io::Result<(Artifact, Self)> {
        let mut nodes = BTreeMap::new();
        let current = observe_recording(
            &artifact.path,
            artifact.interrupted_reclaim,
            now,
            budget,
            mounts::check,
            |relative, metadata| {
                if relative.components().any(|part| !matches!(part, Component::Normal(_))) {
                    return Err(io::Error::other("non-relative quarantine removal entry"));
                }
                if nodes.insert(relative.to_owned(), Node::of(metadata)).is_some() {
                    return Err(io::Error::other("duplicate quarantine removal entry"));
                }
                Ok(())
            },
        )?;
        Ok((current, Self { nodes }))
    }

    fn directory(&self, relative: &Path, directory: &File) -> io::Result<()> {
        let node = self.nodes.get(relative)
            .ok_or_else(|| io::Error::other("unobserved quarantine directory"))?;
        let metadata = directory.metadata()?;
        if !node.directory || !metadata.is_dir() || node.identity != Identity::of(&metadata) {
            return Err(io::Error::other("quarantine directory changed identity"));
        }
        Ok(())
    }

    /// Open one component at a time with O_NOFOLLOW. At most a few descriptors
    /// are live at once, independently of the number of files in the bundle.
    fn parent(&self, root: &File, relative: &Path, budget: &Budget) -> io::Result<File> {
        let mut directory = root.try_clone()?;
        let mut prefix = PathBuf::new();
        self.directory(&prefix, &directory)?;
        for component in relative.parent().unwrap_or_else(|| Path::new("")).components() {
            budget.check()?;
            let Component::Normal(name) = component else {
                return Err(io::Error::other("invalid quarantine removal parent"));
            };
            let child = sys::open_child(&directory, &sys::name(name)?)?;
            prefix.push(name);
            self.directory(&prefix, &child)?;
            directory = child;
        }
        Ok(directory)
    }

    pub(super) fn remove(
        &self,
        doomed: &Path,
        wrapped: bool,
        parent_owner: &same_file::Handle,
        budget: &mut Budget,
    ) -> io::Result<()> {
        budget.check()?;
        let parent_path = doomed.parent()
            .ok_or_else(|| io::Error::other("reclaim directory has no parent"))?;
        real_directory(parent_path)?;
        let parent = sys::open_directory(parent_path)?;
        if *parent_owner != same_file::Handle::from_file(parent.try_clone()?)? {
            return Err(io::Error::other("reclaim parent changed before removal"));
        }
        let outer_name = sys::name(doomed.file_name()
            .ok_or_else(|| io::Error::other("reclaim directory has no name"))?)?;
        let outer = sys::open_child(&parent, &outer_name)?;
        let outer_identity = Identity::of(&outer.metadata()?);
        let root = if wrapped {
            sys::open_child(&outer, c"quarantine")?
        } else {
            outer.try_clone()?
        };
        self.directory(Path::new(""), &root)?;
        let root_identity = Identity::of(&root.metadata()?);
        let check_location = || -> io::Result<()> {
            real_directory(parent_path)?;
            if *parent_owner != same_file::Handle::from_path(parent_path)? {
                return Err(io::Error::other("managed quarantine parent moved during removal"));
            }
            check_directory_entry(&parent, &outer_name, outer_identity)?;
            if wrapped {
                check_directory_entry(&outer, c"quarantine", root_identity)?;
            }
            Ok(())
        };

        // Paths order ancestors before their descendants, so reverse iteration
        // is a deterministic postorder without retaining another path vector.
        for (relative, node) in self.nodes.iter().rev() {
            if relative.as_os_str().is_empty() {
                continue;
            }
            budget.check()?;
            check_location()?;
            let directory = self.parent(&root, relative, budget)?;
            let name = sys::name(relative.file_name()
                .ok_or_else(|| io::Error::other("quarantine entry has no name"))?)?;
            if !node.matches(&sys::stat_child(&directory, &name)?) {
                return Err(io::Error::other(format!(
                    "quarantine entry changed before removal: {relative:?}"
                )));
            }
            remove_entry(&directory, &name, node.directory, budget)?;
        }
        check_location()?;
        if wrapped {
            remove_entry(&outer, c"quarantine", true, budget)?;
            // Extra reservation children are never enumerated or deleted.
            check_directory_entry(&parent, &outer_name, outer_identity)?;
        }
        remove_entry(&parent, &outer_name, true, budget)
    }
}

fn check_directory_entry(parent: &File, name: &CStr, identity: Identity) -> io::Result<()> {
    let stat = sys::stat_child(parent, name)?;
    if !stat.is_dir() || !identity.matches(&stat) {
        return Err(io::Error::other("reclaim directory was replaced or moved"));
    }
    Ok(())
}

fn remove_entry(parent: &File, name: &CStr, directory: bool, budget: &mut Budget) -> io::Result<()> {
    budget.check()?;
    if budget.removal_entries >= budget.limits.tree_entries {
        return Err(io::Error::other("quarantine removal-entry budget exhausted"));
    }
    // Charge attempts, including a failed unlink. This counter and the deadline
    // belong to the entire sweep, not a renewed allowance for each bundle.
    budget.removal_entries += 1;
    sys::unlink_child(parent, name, directory)
}

#[cfg(test)]
mod tests;
