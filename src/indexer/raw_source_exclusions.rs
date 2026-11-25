//! Raw copies of a shared container cannot omit one forgotten conversation.
//!
//! Load this snapshot from the actual canonical archive once per scan/batch,
//! before any raw capture. Canonical admission still uses exact session keys;
//! raw admission must suppress every copy of the enclosing physical source.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{SourceFileId, source_file_id};
use crate::storage::sqlite::FrankenStorage;

#[derive(Debug, Default)]
pub(super) struct RawSourceExclusions {
    paths: HashSet<PathBuf>,
    file_ids: HashSet<SourceFileId>,
}

impl RawSourceExclusions {
    pub(super) fn load(storage: &FrankenStorage) -> Result<Self> {
        if !crate::raw_mirror::capture_enabled() {
            return Ok(Self::default());
        }
        // Legacy single-file stamps deliberately do not participate: editing
        // those sources remains an explicit way to admit them again.
        let keys = storage
            .forgotten_conversation_keys()
            .context("loading permanent session exclusions before raw capture")?;
        // Logical source/provider identities cannot isolate bytes inside the
        // same physical container, so raw suppression is path based.
        Ok(Self::from_paths(
            keys.into_iter().map(|key| PathBuf::from(key.source_path)),
        ))
    }

    pub(super) fn from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut excluded = Self::default();
        for path in paths {
            if path.as_os_str().is_empty() {
                continue;
            }
            let path = std::path::absolute(&path).unwrap_or(path);
            excluded.insert_ancestors(&path);
            if let Some(canonical) = canonicalize_existing_prefix(&path) {
                excluded.insert_ancestors(&canonical);
            }
        }
        excluded
    }

    fn insert_ancestors(&mut self, path: &Path) {
        for candidate in path.ancestors() {
            if candidate.as_os_str().is_empty() || candidate.is_dir() {
                break;
            }
            if !self.paths.insert(candidate.to_path_buf()) {
                break;
            }
            // A virtual database/session path ends at the first real file.
            // Missing ancestors remain in the set, so a database returning
            // later in this watch batch is still excluded without a restat.
            if candidate.is_file() {
                if let Some(identity) = source_file_id(candidate) {
                    self.file_ids.insert(identity);
                }
                for suffix in ["-wal", "-shm", "-journal"] {
                    let mut sidecar = candidate.as_os_str().to_os_string();
                    sidecar.push(suffix);
                    if let Some(identity) = source_file_id(Path::new(&sidecar)) {
                        self.file_ids.insert(identity);
                    }
                }
                break;
            }
        }
    }

    pub(super) fn excludes(&self, path: &Path) -> bool {
        if self.paths.is_empty() {
            return false;
        }
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        self.matches_container(&path)
            || canonicalize_existing_prefix(&path)
                .is_some_and(|canonical| self.matches_container(&canonical))
    }

    fn matches_container(&self, path: &Path) -> bool {
        let matches_source = |source: &Path| {
            self.paths.contains(source)
                || (!self.file_ids.is_empty()
                    && source_file_id(source)
                        .is_some_and(|identity| self.file_ids.contains(&identity)))
        };
        path.ancestors().any(|candidate| {
            // A hard-link alias retains a different canonical pathname.
            // Recognize the protected container's identity beneath virtual
            // session paths and newly created SQLite sidecars as well.
            let matched = matches_source(candidate)
                || sqlite_sidecar_source(candidate)
                    .is_some_and(|source| matches_source(&source));
            // A missing virtual ancestor may later return as an ordinary
            // directory. Its other files are not part of a shared container.
            matched && !candidate.is_dir()
        })
    }
}

/// Canonicalize aliases even when the tail is a virtual session id or a source
/// which has temporarily disappeared. Canonicalizing that complete path alone
/// would fail with NotADirectory/NotFound and lose the physical spelling.
fn canonicalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    path.ancestors().find_map(|prefix| {
        let canonical = std::fs::canonicalize(prefix).ok()?;
        let tail = path.strip_prefix(prefix).ok()?;
        Some(if tail.as_os_str().is_empty() {
            canonical
        } else {
            canonical.join(tail)
        })
    })
}

fn sqlite_sidecar_source(path: &Path) -> Option<PathBuf> {
    let file_name = path.file_name()?;
    let suffix = ["-wal", "-shm", "-journal"]
        .into_iter()
        .find(|suffix| {
            let bytes = file_name.as_encoded_bytes();
            let tail = &bytes[bytes.len().saturating_sub(suffix.len())..];
            #[cfg(windows)]
            {
                tail.eq_ignore_ascii_case(suffix.as_bytes())
            }
            #[cfg(not(windows))]
            {
                tail == suffix.as_bytes()
            }
        })
        .filter(|suffix| file_name.len() > suffix.len())?;
    // Sidecars can appear after the exclusion snapshot. Preserve the exact
    // native container name so those new files cannot escape a permanent
    // forget fence merely because a UTF-8 alias resolved to non-UTF8 bytes.
    #[cfg(unix)]
    let source = {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let bytes = file_name.as_bytes();
        std::ffi::OsString::from_vec(bytes[..bytes.len() - suffix.len()].to_vec())
    };
    #[cfg(windows)]
    let source = {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let mut units: Vec<u16> = file_name.encode_wide().collect();
        units.truncate(units.len() - suffix.len());
        std::ffi::OsString::from_wide(&units)
    };
    #[cfg(not(any(unix, windows)))]
    let source = {
        let file_name = file_name.to_str()?;
        std::ffi::OsString::from(&file_name[..file_name.len() - suffix.len()])
    };
    Some(path.with_file_name(source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_sources_block_the_container_and_sidecars_but_not_neighbours() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let database = temp.path().join("opencode.db");
        std::fs::write(&database, b"shared source")?;
        let excluded = RawSourceExclusions::from_paths([
            database.join("forgotten-session"),
            database.join("another-forgotten-session"),
        ]);
        assert!(excluded.excludes(&database));
        assert!(excluded.excludes(&database.join("retained-sibling")));
        for suffix in ["-wal", "-shm", "-journal"] {
            assert!(excluded.excludes(&temp.path().join(format!("opencode.db{suffix}"))));
        }
        assert!(!excluded.excludes(&temp.path().join("opencode.db.backup")));
        assert!(!excluded.excludes(&temp.path().join("other.db")));
        assert!(!excluded.excludes(temp.path()));
        Ok(())
    }

    #[test]
    fn missing_extensionless_container_stays_blocked_when_it_returns() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let database = temp.path().join("session-store");
        let excluded = RawSourceExclusions::from_paths([database.join("forgotten-session")]);
        assert!(excluded.excludes(&database));
        std::fs::write(&database, b"returned shared source")?;
        assert!(excluded.excludes(&database));
        assert!(excluded.excludes(&temp.path().join("session-store-wal")));

        let directory = temp.path().join("previously-missing-directory");
        let missing_file = RawSourceExclusions::from_paths([directory.join("forgotten.json")]);
        std::fs::create_dir(&directory)?;
        assert!(!missing_file.excludes(&directory.join("unrelated.json")));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn virtual_source_aliases_and_hard_links_resolve_to_the_same_container() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let physical = temp.path().join("physical");
        std::fs::create_dir(&physical)?;
        let database = physical.join("store");
        std::fs::write(&database, b"shared source")?;
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&physical, &alias)?;
        let hard_link = temp.path().join("hard-link");
        std::fs::hard_link(&database, &hard_link)?;
        let excluded = RawSourceExclusions::from_paths([alias.join("store/forgotten-session")]);
        assert!(excluded.excludes(&database));
        assert!(excluded.excludes(&hard_link));
        assert!(excluded.excludes(&physical.join("store-wal")));
        assert!(!excluded.excludes(&physical.join("other-store")));

        let missing = RawSourceExclusions::from_paths([alias.join("future/session")]);
        std::fs::write(physical.join("future"), b"newly available shared source")?;
        assert!(missing.excludes(&physical.join("future")));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn late_sidecars_of_hard_linked_containers_remain_excluded() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let database = temp.path().join("private.db");
        let public = temp.path().join("public.db");
        std::fs::write(&database, b"forgotten private session")?;
        std::fs::write(&public, b"retained public session")?;
        let excluded = RawSourceExclusions::from_paths([database.join("forgotten-session")]);

        // The alias and its sidecars appear after the exclusion snapshot.
        // The database inode is protected, but no sidecar inode or alias
        // pathname could have been recorded when the snapshot was taken.
        let alias = temp.path().join("new-hard-link.db");
        std::fs::hard_link(&database, &alias)?;
        assert!(excluded.excludes(&alias));
        assert!(excluded.excludes(&alias.join("retained-sibling")));
        assert!(!excluded.excludes(&public));
        assert!(!excluded.excludes(&public.join("retained-sibling")));
        for suffix in ["-wal", "-shm", "-journal"] {
            let private_sidecar = temp.path().join(format!("new-hard-link.db{suffix}"));
            let public_sidecar = temp.path().join(format!("public.db{suffix}"));
            std::fs::write(&private_sidecar, b"forgotten bytes in a late sidecar")?;
            std::fs::write(&public_sidecar, b"retained public sidecar")?;
            let sidecar_alias = temp.path().join(format!("sidecar-alias{suffix}"));
            std::os::unix::fs::symlink(&private_sidecar, &sidecar_alias)?;

            assert!(excluded.excludes(&private_sidecar), "{suffix}");
            assert!(excluded.excludes(&sidecar_alias), "{suffix} alias");
            assert!(!excluded.excludes(&public_sidecar), "{suffix} sibling");
        }
        assert_eq!(std::fs::read(&database)?, b"forgotten private session");
        assert_eq!(std::fs::read(&public)?, b"retained public session");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn gh566_late_sidecars_of_non_utf8_shared_stores_remain_excluded() -> Result<()> {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir()?;
        let database = temp
            .path()
            .join(OsString::from_vec(b"private-\xff.db".to_vec()));
        let public = temp
            .path()
            .join(OsString::from_vec(b"public-\xfe.db".to_vec()));
        std::fs::write(&database, b"forgotten private session")?;
        std::fs::write(&public, b"retained public session")?;
        let alias = temp.path().join("store-alias");
        symlink(&database, &alias)?;

        // Canonical conversation keys may use a UTF-8 alias whose target has
        // a native filename. No sidecar exists when the forget snapshot loads,
        // so later captures cannot rely on its saved sidecar inode identities.
        let excluded = RawSourceExclusions::from_paths([alias.join("forgotten-session")]);
        assert!(excluded.excludes(&database));
        assert!(!excluded.excludes(&public));
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut private_name = database.as_os_str().to_owned();
            private_name.push(suffix);
            let private_sidecar = PathBuf::from(private_name);
            let mut public_name = public.as_os_str().to_owned();
            public_name.push(suffix);
            let public_sidecar = PathBuf::from(public_name);
            std::fs::write(&private_sidecar, b"forgotten bytes in a late sidecar")?;
            std::fs::write(&public_sidecar, b"retained public sidecar")?;
            let sidecar_alias = temp.path().join(format!("sidecar-alias{suffix}"));
            symlink(&private_sidecar, &sidecar_alias)?;

            assert!(excluded.excludes(&private_sidecar), "{suffix}");
            assert!(excluded.excludes(&sidecar_alias), "{suffix} alias");
            assert!(!excluded.excludes(&public_sidecar), "{suffix} sibling");
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn gh566_late_sidecars_with_windows_case_aliases_remain_excluded() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let database = temp.path().join("private.db");
        let public = temp.path().join("public.db");
        std::fs::write(&database, b"forgotten private session")?;
        std::fs::write(&public, b"retained public session")?;
        let excluded = RawSourceExclusions::from_paths([database.join("forgotten-session")]);

        for suffix in ["-WAL", "-sHm", "-JoUrNaL"] {
            let private_sidecar = temp.path().join(format!("private.db{suffix}"));
            let public_sidecar = temp.path().join(format!("public.db{suffix}"));
            std::fs::write(&private_sidecar, b"forgotten bytes in a late sidecar")?;
            std::fs::write(&public_sidecar, b"retained public sidecar")?;
            assert!(excluded.excludes(&private_sidecar), "{suffix}");
            assert!(!excluded.excludes(&public_sidecar), "{suffix} sibling");
        }
        Ok(())
    }
}
