//! Durable admission for readers that can retain text across `cass forget`.
//!
//! The epoch is published before canonical deletion and stays pending until
//! every required derived surface has been rebuilt. It lives beside the
//! versioned lexical directory, so replacing that directory cannot remove the
//! fence. Readers only read this small file; they never open or repair SQLite.
//!
//! A result is admitted at its final `validate` call. An operation admitted
//! before a concurrent forget publishes its fence may finish delivering that
//! result; an operation that observes the fence must discard its entire result.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::semantic_manifest::{persist_named_temp, sync_parent_directory};

const STATE_FILE: &str = "forget-state.json";
const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 1024;

/// Privacy failures are never ordinary lexical/semantic fallback signals.
#[derive(Debug, thiserror::Error)]
pub enum PrivacyError {
    #[error(
        "forget-repair-pending: forgotten history is being removed from derived assets; retry the same forget command with --apply to finish repair before reading history; fence: {}",
        path.display()
    )]
    RepairPending { path: PathBuf },
    #[error(
        "privacy-snapshot-invalidated: forget invalidated this retained reader; reopen or reload it before reading {}",
        path.display()
    )]
    SnapshotInvalidated { path: PathBuf },
    #[error("privacy-state-unavailable: cannot verify the forget fence at {}: {detail}", path.display())]
    StateUnavailable { path: PathBuf, detail: String },
}

impl PrivacyError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RepairPending { .. } => "forget-repair-pending",
            Self::SnapshotInvalidated { .. } => "privacy-snapshot-invalidated",
            Self::StateUnavailable { .. } => "privacy-state-unavailable",
        }
    }
}

/// String-only UI events preserve the typed error's leading discriminator.
pub fn is_privacy_error_message(message: &str) -> bool {
    [
        "forget-repair-pending:",
        "privacy-snapshot-invalidated:",
        "privacy-state-unavailable:",
    ]
    .iter()
    .any(|prefix| message.starts_with(prefix))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    epoch: u64,
    pending: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            epoch: 0,
            pending: false,
        }
    }
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

pub fn state_path_for_database(database: &Path) -> PathBuf {
    parent_directory(database).join("index").join(STATE_FILE)
}

pub fn state_path_for_index(index: &Path) -> PathBuf {
    parent_directory(index).join(STATE_FILE)
}

fn unavailable(path: &Path, detail: impl ToString) -> anyhow::Error {
    PrivacyError::StateUnavailable {
        path: path.to_path_buf(),
        detail: detail.to_string(),
    }
    .into()
}

fn resolve_if_present(path: &Path, fence: &Path) -> Result<Option<PathBuf>> {
    match fs::canonicalize(path) {
        Ok(resolved) => Ok(Some(resolved)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::symlink_metadata(path) {
                // A dangling alias is not an ordinary absent optional asset.
                Ok(_) => Err(unavailable(fence, error)),
                Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(other) => Err(unavailable(fence, other)),
            }
        }
        Err(error) => Err(unavailable(fence, error)),
    }
}

fn read_state(path: &Path) -> Result<State> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Do not follow a substituted link, or block on a substituted FIFO.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open the reparse point itself, then reject it as a non-regular file.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(State::default());
        }
        Err(error) => return Err(unavailable(path, error)),
    };
    let metadata = file.metadata().map_err(|error| unavailable(path, error))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(unavailable(path, "the fence must be a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| unavailable(path, error))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(unavailable(path, "the fence exceeds 1024 bytes"));
    }
    let state: State = serde_json::from_slice(&bytes).map_err(|error| unavailable(path, error))?;
    if state.version != STATE_VERSION || state.epoch == 0 {
        return Err(unavailable(path, "unsupported fence version or zero epoch"));
    }
    Ok(state)
}

fn write_state(path: &Path, state: State) -> Result<()> {
    let parent = parent_directory(path);
    fs::create_dir_all(parent).map_err(|error| unavailable(path, error))?;
    // Also persist a newly created index directory before admitting deletion.
    sync_parent_directory(parent).map_err(|error| unavailable(path, error))?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|error| unavailable(path, error))?;
    serde_json::to_writer(&mut temporary, &state).map_err(|error| unavailable(path, error))?;
    temporary
        .write_all(b"\n")
        .map_err(|error| unavailable(path, error))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| unavailable(path, error))?;
    let published: File =
        persist_named_temp(temporary, path, true).map_err(|error| unavailable(path, error))?;
    published
        .sync_all()
        .map_err(|error| unavailable(path, error))?;
    sync_parent_directory(path).map_err(|error| unavailable(path, error))
}

/// A reader's admitted privacy epoch. Capture it before opening retained
/// assets, validate after opening, and validate again before returning data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivacySnapshot {
    path: Option<PathBuf>,
    epoch: u64,
    /// An explicit database alias must observe both its configured archive
    /// fence and the target archive's fence. Writers may use either address.
    alias_fence: Option<(PathBuf, u64)>,
    /// Bind aliases before opening retained assets and detect a retargeted alias
    /// before releasing results from the retained reader.
    path_binding: Option<(PathBuf, PathBuf)>,
}

impl PrivacySnapshot {
    pub fn for_database(database: &Path) -> Result<Self> {
        if database == Path::new(":memory:") {
            return Ok(Self {
                path: None,
                epoch: 0,
                alias_fence: None,
                path_binding: None,
            });
        }
        let logical_fence = state_path_for_database(database);
        let mut snapshot = Self::at(logical_fence.clone())?;
        if let Some(resolved) = resolve_if_present(database, &logical_fence)?
            && resolved != database
        {
            let target_fence = state_path_for_database(&resolved);
            if target_fence != logical_fence {
                let target = Self::at(target_fence.clone())?;
                snapshot.alias_fence = Some((target_fence, target.epoch));
            }
            snapshot.path_binding = Some((database.to_path_buf(), resolved));
        }
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn for_index(index: &Path) -> Result<Self> {
        let fence = state_path_for_index(index);
        match resolve_if_present(index, &fence)? {
            Some(resolved) => {
                let mut snapshot = Self::at(state_path_for_index(&resolved))?;
                snapshot.path_binding = Some((index.to_path_buf(), resolved));
                Ok(snapshot)
            }
            None => {
                // A missing index remains the caller's normal missing-index
                // case. Its existing parent can still hold a pending fence.
                Self::at(fence)
            }
        }
    }

    fn at(path: PathBuf) -> Result<Self> {
        let state = read_state(&path)?;
        if state.pending {
            return Err(PrivacyError::RepairPending { path }.into());
        }
        Ok(Self {
            path: Some(path),
            epoch: state.epoch,
            alias_fence: None,
            path_binding: None,
        })
    }

    pub fn validate(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        Self::validate_fence(path, self.epoch)?;
        if let Some((alias, epoch)) = &self.alias_fence {
            Self::validate_fence(alias, *epoch)?;
        }
        if let Some((bound_path, admitted)) = &self.path_binding {
            match fs::canonicalize(bound_path) {
                Ok(current) if current == *admitted => {}
                Ok(_) => {
                    return Err(PrivacyError::SnapshotInvalidated { path: path.clone() }.into());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(PrivacyError::SnapshotInvalidated { path: path.clone() }.into());
                }
                Err(error) => return Err(unavailable(path, error)),
            }
        }
        Ok(())
    }

    fn validate_fence(path: &Path, epoch: u64) -> Result<()> {
        let current = read_state(path)?;
        if current.pending {
            return Err(PrivacyError::RepairPending { path: path.to_path_buf() }.into());
        }
        if current.epoch != epoch {
            return Err(PrivacyError::SnapshotInvalidated { path: path.to_path_buf() }.into());
        }
        Ok(())
    }
}

/// Publish before canonical deletion. Callers must hold the archive's
/// maintenance lease through deletion and the subsequent repair. A failed
/// deletion conservatively leaves repair pending; retry remains safe.
pub fn begin_forget(database: &Path) -> Result<()> {
    if database == Path::new(":memory:") {
        return Ok(());
    }
    let path = state_path_for_database(database);
    let current = read_state(&path)?;
    if current.pending {
        // A prior attempt may have failed its final directory fsync after
        // installing the pending file. Re-establish durability before another
        // caller is allowed to commit canonical deletion.
        return write_state(&path, current);
    }
    let epoch = current
        .epoch
        .checked_add(1)
        .ok_or_else(|| unavailable(&path, "the privacy epoch is exhausted"))?;
    write_state(
        &path,
        State {
            version: STATE_VERSION,
            epoch,
            pending: true,
        },
    )
}

pub fn repair_pending(database: &Path) -> Result<bool> {
    if database == Path::new(":memory:") {
        return Ok(false);
    }
    Ok(read_state(&state_path_for_database(database))?.pending)
}

/// Clear pending only after canonical-derived and lexical repairs complete.
/// Preserve the epoch permanently so an old pinned reader stays invalidated.
pub fn complete_forget(database: &Path) -> Result<()> {
    if database == Path::new(":memory:") {
        return Ok(());
    }
    let path = state_path_for_database(database);
    let mut current = read_state(&path)?;
    if !current.pending {
        return Ok(());
    }
    current.pending = false;
    write_state(&path, current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(error: &anyhow::Error) -> &'static str {
        error.downcast_ref::<PrivacyError>().unwrap().kind()
    }

    #[test]
    fn forget_admission_is_read_only_and_uses_the_same_database_free_fence() -> Result<()> {
        let root = tempfile::tempdir()?;
        let db = root.path().join("agent_search.db");
        let index = root.path().join("index/v9-quill");
        assert_eq!(
            PrivacySnapshot::for_database(&db)?,
            PrivacySnapshot::for_index(&index)?
        );
        assert!(!repair_pending(&db)?);
        complete_forget(&db)?;
        assert_eq!(fs::read_dir(root.path())?.count(), 0);
        Ok(())
    }

    #[test]
    fn forget_epoch_refuses_inflight_and_pinned_readers_through_repair() -> Result<()> {
        let root = tempfile::tempdir()?;
        let db = root.path().join("archive.db");
        let before = PrivacySnapshot::for_database(&db)?;
        before.validate()?;
        begin_forget(&db)?;
        assert!(repair_pending(&db)?);
        assert_eq!(
            kind(&before.validate().unwrap_err()),
            "forget-repair-pending"
        );
        assert_eq!(
            kind(&PrivacySnapshot::for_database(&db).unwrap_err()),
            "forget-repair-pending"
        );
        let pending_bytes = fs::read(state_path_for_database(&db))?;
        begin_forget(&db)?;
        assert_eq!(fs::read(state_path_for_database(&db))?, pending_bytes);
        complete_forget(&db)?;
        assert!(!repair_pending(&db)?);
        assert_eq!(
            kind(&before.validate().unwrap_err()),
            "privacy-snapshot-invalidated"
        );
        let repaired = PrivacySnapshot::for_database(&db)?;
        repaired.validate()?;
        assert_ne!(before, repaired);
        begin_forget(&db)?;
        complete_forget(&db)?;
        assert_eq!(
            kind(&repaired.validate().unwrap_err()),
            "privacy-snapshot-invalidated"
        );
        PrivacySnapshot::for_database(&db)?.validate()?;
        Ok(())
    }

    #[test]
    fn forget_fence_rejects_corrupt_unknown_and_oversized_state() -> Result<()> {
        let root = tempfile::tempdir()?;
        let db = root.path().join("archive.db");
        let old = PrivacySnapshot::for_database(&db)?;
        let path = state_path_for_database(&db);
        fs::create_dir_all(parent_directory(&path))?;
        for invalid in [
            Vec::new(),
            br#"{"version":1,"epoch":1"#.to_vec(),
            br#"{"version":2,"epoch":1,"pending":false}"#.to_vec(),
            br#"{"version":1,"epoch":0,"pending":false}"#.to_vec(),
            br#"{"version":1,"epoch":1,"pending":false,"extra":true}"#.to_vec(),
            vec![b' '; MAX_STATE_BYTES as usize + 1],
        ] {
            fs::write(&path, &invalid)?;
            for error in [
                old.validate().unwrap_err(),
                begin_forget(&db).unwrap_err(),
                complete_forget(&db).unwrap_err(),
            ] {
                assert_eq!(kind(&error), "privacy-state-unavailable");
                assert!(is_privacy_error_message(&error.to_string()));
            }
            assert_eq!(fs::read(&path)?, invalid);
        }
        Ok(())
    }

    #[test]
    fn forget_epoch_overflow_refuses_without_replacing_state() -> Result<()> {
        let root = tempfile::tempdir()?;
        let db = root.path().join("archive.db");
        let path = state_path_for_database(&db);
        write_state(
            &path,
            State {
                version: STATE_VERSION,
                epoch: u64::MAX,
                pending: false,
            },
        )?;
        let before = fs::read(&path)?;
        assert_eq!(
            kind(&begin_forget(&db).unwrap_err()),
            "privacy-state-unavailable"
        );
        assert_eq!(fs::read(&path)?, before);
        Ok(())
    }

    #[test]
    fn forget_memory_archive_never_uses_a_relative_disk_fence() -> Result<()> {
        let db = Path::new(":memory:");
        let snapshot = PrivacySnapshot::for_database(db)?;
        assert!(snapshot.path.is_none());
        begin_forget(db)?;
        snapshot.validate()?;
        assert!(!repair_pending(db)?);
        complete_forget(db)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn forget_fence_refuses_symlinks_without_following_or_overwriting_them() -> Result<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        let db = root.path().join("archive.db");
        let path = state_path_for_database(&db);
        fs::create_dir_all(parent_directory(&path))?;
        let target = root.path().join("other-file");
        let contents = br#"{"version":1,"epoch":1,"pending":false}"#;
        fs::write(&target, contents)?;
        symlink(&target, &path)?;
        assert_eq!(
            kind(&PrivacySnapshot::for_database(&db).unwrap_err()),
            "privacy-state-unavailable"
        );
        assert_eq!(
            kind(&begin_forget(&db).unwrap_err()),
            "privacy-state-unavailable"
        );
        assert_eq!(fs::read(&target)?, contents);
        assert!(fs::symlink_metadata(&path)?.file_type().is_symlink());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn forget_index_alias_observes_the_target_fence_and_cannot_be_retargeted() -> Result<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        let db = root.path().join("archive/agent_search.db");
        let index = root.path().join("archive/index/v9-quill");
        fs::create_dir_all(&index)?;
        let alias = root.path().join("index-alias");
        symlink(&index, &alias)?;
        let before = PrivacySnapshot::for_index(&alias)?;
        before.validate()?;
        begin_forget(&db)?;
        assert_eq!(
            kind(&before.validate().unwrap_err()),
            "forget-repair-pending"
        );
        assert_eq!(
            kind(&PrivacySnapshot::for_index(&alias).unwrap_err()),
            "forget-repair-pending"
        );
        complete_forget(&db)?;
        assert_eq!(
            kind(&before.validate().unwrap_err()),
            "privacy-snapshot-invalidated"
        );
        let fresh = PrivacySnapshot::for_index(&alias)?;
        fresh.validate()?;

        let other_index = root.path().join("another/index/v9-quill");
        fs::create_dir_all(&other_index)?;
        let other_alias = root.path().join("other-alias");
        symlink(&other_index, &other_alias)?;
        fs::rename(&other_alias, &alias)?;
        assert_eq!(
            kind(&fresh.validate().unwrap_err()),
            "privacy-snapshot-invalidated",
            "an alias change during reader admission must not bless another archive"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn forget_database_alias_observes_both_logical_and_target_fences() -> Result<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::Builder::new().disable_cleanup(true).tempdir()?;
        let db = root.path().join("archive/agent_search.db");
        let alias = root.path().join("configured/archive.db");
        fs::create_dir_all(parent_directory(&db))?;
        fs::create_dir_all(parent_directory(&alias))?;
        // Admission checks filesystem identity, never SQLite contents.
        let contents = b"identity-only archive fixture";
        fs::write(&db, contents)?;
        symlink(&db, &alias)?;
        let before = PrivacySnapshot::for_database(&alias)?;
        before.validate()?;
        assert!(!state_path_for_database(&alias).exists());
        assert!(!state_path_for_database(&db).exists());

        begin_forget(&db)?;
        assert_eq!(kind(&before.validate().unwrap_err()), "forget-repair-pending");
        assert_eq!(
            kind(&PrivacySnapshot::for_database(&alias).unwrap_err()),
            "forget-repair-pending"
        );
        complete_forget(&db)?;
        assert_eq!(kind(&before.validate().unwrap_err()), "privacy-snapshot-invalidated");
        let target_repaired = PrivacySnapshot::for_database(&alias)?;
        let target_bytes = fs::read(state_path_for_database(&db))?;
        target_repaired.validate()?;
        assert_eq!(fs::read(state_path_for_database(&db))?, target_bytes);

        // Resolving the alias must not discard the configured-path fence:
        // the existing writer protocol can publish through that address too.
        begin_forget(&alias)?;
        assert_eq!(kind(&target_repaired.validate().unwrap_err()), "forget-repair-pending");
        assert_eq!(
            kind(&PrivacySnapshot::for_database(&alias).unwrap_err()),
            "forget-repair-pending"
        );
        complete_forget(&alias)?;
        assert_eq!(kind(&target_repaired.validate().unwrap_err()), "privacy-snapshot-invalidated");
        let repaired = PrivacySnapshot::for_database(&alias)?;
        let logical_bytes = fs::read(state_path_for_database(&alias))?;
        repaired.validate()?;
        assert_eq!(fs::read(state_path_for_database(&alias))?, logical_bytes);
        assert_eq!(fs::read(state_path_for_database(&db))?, target_bytes);
        assert_eq!(fs::read(&db)?, contents);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn forget_database_alias_retargeting_invalidates_equal_epochs() -> Result<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::Builder::new().disable_cleanup(true).tempdir()?;
        let first = root.path().join("first/archive.db");
        let second = root.path().join("second/archive.db");
        let alias = root.path().join("links/archive.db");
        for path in [&first, &second, &alias] {
            fs::create_dir_all(parent_directory(path))?;
        }
        fs::write(&first, b"first identity")?;
        fs::write(&second, b"second identity")?;
        symlink("../first/archive.db", &alias)?;
        let before = PrivacySnapshot::for_database(&alias)?;
        before.validate()?;
        let replacement = root.path().join("links/replacement");
        symlink("../second/archive.db", &replacement)?;
        fs::rename(&replacement, &alias)?;
        assert_eq!(kind(&before.validate().unwrap_err()), "privacy-snapshot-invalidated");

        let fresh = PrivacySnapshot::for_database(&alias)?;
        fresh.validate()?;
        begin_forget(&first)?;
        fresh.validate()?;
        assert_eq!(fs::read(&alias)?, b"second identity");
        assert!(!state_path_for_database(&alias).exists());
        assert!(!state_path_for_database(&second).exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn forget_dangling_alias_is_unavailable_but_an_absent_asset_is_allowed() -> Result<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::Builder::new().disable_cleanup(true).tempdir()?;
        let absent = root.path().join("absent");
        let alias = root.path().join("alias");
        symlink(&absent, &alias)?;
        for capture in [PrivacySnapshot::for_database, PrivacySnapshot::for_index] {
            assert_eq!(kind(&capture(&alias).unwrap_err()), "privacy-state-unavailable");
            capture(&absent)?.validate()?;
        }
        assert_eq!(fs::read_link(&alias)?, absent);
        assert_eq!(fs::read_dir(root.path())?.count(), 1);
        Ok(())
    }
}
