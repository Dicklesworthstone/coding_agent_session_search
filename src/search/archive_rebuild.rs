//! Explicit, canonical-only lexical recovery after a logical archive import.
//!
//! This is not ordinary indexing: it never discovers provider histories,
//! imports historical bundles, or opens an embedder. The existing index-run
//! lock, scratch builder, checkpoint validation and atomic publisher remain
//! the only implementation of lexical reconstruction.

use std::fs::{self, Metadata, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use same_file::Handle;
use serde::Serialize;

use crate::search::tantivy::expected_index_dir;
use crate::storage::sqlite::FrankenStorage;

const MAX_MAINTENANCE_DEPTH: usize = 64;

fn refuse_link(metadata: &Metadata) -> Result<()> {
    ensure!(
        !metadata.file_type().is_symlink(),
        "indexed restore refuses a symlink in its protected or maintenance paths"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "indexed restore refuses a reparse point in its protected or maintenance paths"
        );
    }
    Ok(())
}

/// Compare descriptor identities without following a final link or blocking on
/// a substituted FIFO. The handle stays open while its identity is authoritative.
fn regular_file_identity(path: &Path) -> Result<Handle> {
    let metadata = fs::symlink_metadata(path).context("inspect indexed restore file identity")?;
    refuse_link(&metadata)?;
    ensure!(
        metadata.is_file(),
        "indexed restore requires regular protected and maintenance files"
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path).context("open indexed restore file identity")?;
    let metadata = file.metadata().context("inspect opened indexed restore file identity")?;
    refuse_link(&metadata)?;
    ensure!(
        metadata.is_file(),
        "indexed restore requires regular protected and maintenance files"
    );
    Handle::from_file(file).context("prove indexed restore file identity")
}

/// An explicit recovery target, independent of the ambient CASS_DATA_DIR.
/// Fields are private so callers cannot change the admitted layout afterward.
#[derive(Debug)]
pub struct ArchiveIndexPlan {
    database: PathBuf,
    data_dir: PathBuf,
    input: PathBuf,
    input_identity: Handle,
}

/// A successful lexical publication, not semantic readiness or an archive lease.
#[derive(Debug, Serialize)]
pub struct ArchiveIndexReceipt {
    pub data_dir: PathBuf,
    pub index_path: PathBuf,
    pub indexed_documents: usize,
    pub source: &'static str,
    pub provider_scan_performed: bool,
    pub semantic_assets_built: bool,
}

impl ArchiveIndexPlan {
    /// Admit an indexed restore before the importer creates any destination.
    ///
    /// An indexed restore uses the normal `agent_search.db` profile layout so
    /// subsequent `cass search --data-dir ...` cannot select a different DB.
    /// Keep the interchange file outside that profile: maintenance owns index,
    /// lock and checkpoint names there and must never move/truncate its input.
    /// Existing destinations are still admitted only by the importer's policy.
    pub fn prepare(database: &Path, input: &Path) -> Result<Self> {
        ensure!(
            database
                .file_name()
                .is_some_and(|name| name == "agent_search.db"),
            "--rebuild-index requires --output <data-directory>/agent_search.db"
        );
        let parent = database
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let metadata = std::fs::symlink_metadata(parent)
            .context("indexed restore requires an existing data directory")?;
        refuse_link(&metadata)?;
        ensure!(
            metadata.is_dir(),
            "indexed restore data directory must be a real directory, not a symlink"
        );
        let data_dir = parent
            .canonicalize()
            .context("resolve indexed restore directory")?;
        ensure!(
            data_dir.to_str().is_some(),
            "indexed restore path must be UTF-8"
        );
        let input_identity = regular_file_identity(input)?;
        let input = input
            .canonicalize()
            .context("resolve logical archive input")?;
        ensure!(
            !input.starts_with(&data_dir),
            "keep the logical archive input outside the indexed restore data directory"
        );
        let plan = Self {
            database: data_dir.join("agent_search.db"),
            data_dir,
            input,
            input_identity,
        };
        plan.validate_maintenance_layout()?;
        Ok(plan)
    }

    /// The exact destination to pass to the logical importer.
    pub fn database(&self) -> &Path {
        &self.database
    }

    fn validate_index_layout(&self) -> Result<PathBuf> {
        let index = expected_index_dir(&self.data_dir);
        let relative = index
            .strip_prefix(&self.data_dir)
            .context("lexical index must be inside the recovered profile")?;
        ensure!(
            !relative.as_os_str().is_empty(),
            "lexical index cannot replace its data directory"
        );
        let root = self.data_dir.join(
            relative
                .components()
                .next()
                .context("lexical index has no profile-relative root")?
                .as_os_str(),
        );
        let mut path = self.data_dir.clone();
        for component in relative.components() {
            ensure!(
                matches!(component, std::path::Component::Normal(_)),
                "lexical index must stay inside the recovered profile"
            );
            path.push(component.as_os_str());
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    refuse_link(&metadata)?;
                    ensure!(
                        metadata.is_dir(),
                        "indexed restore refuses a non-directory in the lexical index path"
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("inspect recovered lexical index path"),
            }
        }
        Ok(root)
    }

    fn validate_maintenance_file(&self, path: &Path, database: Option<&Handle>) -> Result<()> {
        let identity = regular_file_identity(path)?;
        ensure!(
            identity != self.input_identity,
            "indexed restore maintenance file aliases the logical archive input; lexical writes were refused"
        );
        ensure!(
            database.is_none_or(|database| identity != *database),
            "indexed restore maintenance file aliases the canonical database; lexical writes were refused"
        );
        Ok(())
    }

    /// ReadDir retains one entry at a time. A bounded recursion stack avoids
    /// retaining a whole directory listing or an index-sized identity set.
    fn validate_maintenance_tree(
        &self,
        root: &Path,
        database: Option<&Handle>,
        depth: usize,
    ) -> Result<()> {
        ensure!(
            depth <= MAX_MAINTENANCE_DEPTH,
            "indexed restore maintenance directory exceeds the supported depth"
        );
        for entry in fs::read_dir(root).context("inspect indexed restore maintenance directory")? {
            let path = entry.context("inspect indexed restore maintenance entry")?.path();
            let metadata = fs::symlink_metadata(&path)
                .context("inspect indexed restore maintenance entry type")?;
            refuse_link(&metadata)?;
            if metadata.is_dir() {
                self.validate_maintenance_tree(&path, database, depth + 1)?;
            } else {
                self.validate_maintenance_file(&path, database)?;
            }
        }
        Ok(())
    }

    /// Maintenance rewrites lock metadata, logs and checkpoint temporary files.
    /// A pathname outside this profile does not keep its bytes outside: a hard
    /// link or a maintenance symlink could still expose the input to those writes.
    /// Check the whole existing lexical root, including scratch/backup siblings,
    /// and direct profile files before import and again before lexical mutation.
    /// These checks are not a lease against concurrent uncooperative path changes.
    fn validate_maintenance_layout(&self) -> Result<()> {
        let metadata = fs::symlink_metadata(&self.data_dir)
            .context("inspect indexed restore data directory")?;
        refuse_link(&metadata)?;
        ensure!(metadata.is_dir(), "indexed restore data directory is not a directory");
        ensure!(
            regular_file_identity(&self.input)? == self.input_identity,
            "logical archive input changed after indexed restore admission"
        );
        let database = match fs::symlink_metadata(&self.database) {
            Ok(_) => Some(regular_file_identity(&self.database)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("inspect indexed restore canonical identity"),
        };
        ensure!(
            database.as_ref().is_none_or(|database| *database != self.input_identity),
            "indexed restore canonical database aliases the logical archive input"
        );
        let index_root = self.validate_index_layout()?;
        for entry in fs::read_dir(&self.data_dir).context("inspect indexed restore profile")? {
            let path = entry.context("inspect indexed restore profile entry")?.path();
            let metadata = fs::symlink_metadata(&path)
                .context("inspect indexed restore profile entry type")?;
            refuse_link(&metadata)?;
            if metadata.is_dir() {
                if path == index_root {
                    self.validate_maintenance_tree(&path, database.as_ref(), 1)?;
                }
            } else if path != self.database {
                self.validate_maintenance_file(&path, database.as_ref())?;
            }
        }
        Ok(())
    }

    /// Rebuild only after the importer has published/verified the canonical DB.
    ///
    /// No `run_index` call: even a force rebuild's general startup has more
    /// authority than a recovered archive needs. Strict admission cannot create
    /// or migrate a missing/old DB. The canonical-only repair holds index-run
    /// authority and publishes through the ordinary recoverable scratch path.
    /// On error the restored database is retained; callers must report failure,
    /// not undo canonical recovery or claim that search is ready.
    pub fn rebuild(&self) -> Result<ArchiveIndexReceipt> {
        self.validate_maintenance_layout()?;
        let metadata = std::fs::symlink_metadata(&self.database)
            .context("inspect restored canonical database before lexical rebuild")?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "restored canonical database must be a regular, non-symlink file"
        );
        let storage = FrankenStorage::open_strict_readonly(&self.database)
            .context("admit restored canonical database without migration or repair")?;
        storage
            .close_without_checkpoint()
            .context("close recovered archive admission without checkpointing")?;
        let result = crate::indexer::repair_lexical_index_from_canonical_db_for_search(
            &self.database,
            &self.data_dir,
            None,
        )
        .context("rebuild lexical search from the restored canonical archive")?;
        Ok(ArchiveIndexReceipt {
            data_dir: self.data_dir.clone(),
            index_path: expected_index_dir(&self.data_dir),
            indexed_documents: result.indexed_docs,
            source: "canonical_archive",
            provider_scan_performed: false,
            semantic_assets_built: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Result<(tempfile::TempDir, PathBuf, PathBuf)> {
        let root = tempfile::tempdir()?;
        let input = root.path().join("history.jsonl");
        std::fs::write(&input, b"interchange input")?;
        let data = root.path().join("recovered");
        std::fs::create_dir(&data)?;
        Ok((root, input, data))
    }

    fn maintenance_alias_paths() -> [PathBuf; 7] {
        let profile = Path::new("profile");
        let index = expected_index_dir(profile)
            .strip_prefix(profile)
            .expect("the lexical index is inside its profile")
            .to_path_buf();
        let root = index.parent().expect("the index has a staging parent");
        [
            PathBuf::from("index-run.lock"),
            PathBuf::from("index-run.lock.meta"),
            PathBuf::from(".maintenance-events.jsonl"),
            index.join(".lexical-rebuild-state.json"),
            index.join(".expected-lexical-docs.partial.json.123.tmp"),
            root.join("retained.rebuild-staging/.archive-fingerprint-cache.json.123.tmp"),
            root.join(".lexical-publish-backups/prior/checkpoint.json"),
        ]
    }

    #[test]
    fn indexed_restore_preflight_is_nonmutating_and_uses_the_output_profile() -> Result<()> {
        let (_root, input, data) = fixture()?;
        let plan = ArchiveIndexPlan::prepare(&data.join("agent_search.db"), &input)?;
        assert_eq!(
            plan.database(),
            data.canonicalize()?.join("agent_search.db")
        );
        assert_eq!(std::fs::read_dir(&data)?.count(), 0);
        assert_eq!(std::fs::read(&input)?, b"interchange input");
        Ok(())
    }

    #[test]
    fn indexed_restore_rejects_ambiguous_layouts_before_creating_anything() -> Result<()> {
        let (root, input, data) = fixture()?;
        assert!(ArchiveIndexPlan::prepare(&data.join("restored.db"), &input).is_err());
        let missing = root.path().join("missing");
        assert!(ArchiveIndexPlan::prepare(&missing.join("agent_search.db"), &input).is_err());
        assert!(!missing.exists());
        assert_eq!(std::fs::read_dir(&data)?.count(), 0);
        Ok(())
    }

    #[test]
    fn interchange_input_cannot_become_a_maintenance_owned_file() -> Result<()> {
        let (_root, _input, data) = fixture()?;
        let input = data.join("index-run.lock");
        std::fs::write(&input, b"never truncate this input")?;
        assert!(ArchiveIndexPlan::prepare(&data.join("agent_search.db"), &input).is_err());
        assert_eq!(std::fs::read(&input)?, b"never truncate this input");
        assert!(!data.join("agent_search.db").exists());
        Ok(())
    }

    #[test]
    fn absent_and_invalid_archives_do_not_create_indexes_or_locks() -> Result<()> {
        let (_root, input, data) = fixture()?;
        let plan = ArchiveIndexPlan::prepare(&data.join("agent_search.db"), &input)?;
        assert!(plan.rebuild().is_err());
        assert_eq!(std::fs::read_dir(&data)?.count(), 0);
        std::fs::write(plan.database(), b"not a canonical database")?;
        assert!(plan.rebuild().is_err());
        assert_eq!(std::fs::read(plan.database())?, b"not a canonical database");
        assert!(!expected_index_dir(&data).exists());
        assert!(!data.join("index-run.lock").exists());
        Ok(())
    }

    #[test]
    fn hard_linked_backup_aliases_are_refused_across_the_maintenance_surface() -> Result<()> {
        for relative in maintenance_alias_paths() {
            let (_root, input, data) = fixture()?;
            let alias = data.join(relative);
            fs::create_dir_all(alias.parent().expect("maintenance path has a parent"))?;
            fs::hard_link(&input, &alias)?;
            let error = ArchiveIndexPlan::prepare(&data.join("agent_search.db"), &input)
                .expect_err("maintenance must not gain write authority over a hard-linked input");
            assert!(error.to_string().contains("aliases the logical archive input"));
            assert_eq!(fs::read(&input)?, b"interchange input");
            assert_eq!(fs::read(&alias)?, b"interchange input");
            assert!(!data.join("agent_search.db").exists());
        }
        Ok(())
    }

    #[test]
    fn maintenance_aliases_to_an_existing_canonical_database_are_refused() -> Result<()> {
        for relative in maintenance_alias_paths() {
            let (_root, input, data) = fixture()?;
            let database = data.join("agent_search.db");
            fs::write(&database, b"existing canonical evidence")?;
            let alias = data.join(relative);
            fs::create_dir_all(alias.parent().expect("maintenance path has a parent"))?;
            fs::hard_link(&database, &alias)?;
            let error = ArchiveIndexPlan::prepare(&database, &input)
                .expect_err("maintenance cannot rewrite the canonical database through an alias");
            assert!(error.to_string().contains("aliases the canonical database"));
            assert_eq!(fs::read(&database)?, b"existing canonical evidence");
            assert_eq!(fs::read(&input)?, b"interchange input");
        }
        Ok(())
    }

    #[test]
    fn separate_maintenance_files_and_unrelated_hard_links_remain_admissible() -> Result<()> {
        let (root, input, data) = fixture()?;
        let database = data.join("agent_search.db");
        fs::write(&database, b"existing canonical evidence")?;
        let separate = root.path().join("separate-maintenance-file");
        fs::write(&separate, b"independent maintenance state")?;
        for relative in maintenance_alias_paths() {
            let path = data.join(relative);
            fs::create_dir_all(path.parent().expect("maintenance path has a parent"))?;
            fs::hard_link(&separate, &path)?;
        }
        let plan = ArchiveIndexPlan::prepare(&database, &input)?;
        plan.validate_maintenance_layout()?;
        assert_eq!(fs::read(&database)?, b"existing canonical evidence");
        assert_eq!(fs::read(&input)?, b"interchange input");
        assert_eq!(fs::read(&separate)?, b"independent maintenance state");
        Ok(())
    }

    #[test]
    fn aliases_added_after_preparation_are_refused_before_rebuild_writes() -> Result<()> {
        for alias_database in [false, true] {
            let (_root, input, data) = fixture()?;
            let plan = ArchiveIndexPlan::prepare(&data.join("agent_search.db"), &input)?;
            drop(FrankenStorage::open(plan.database())?);
            let before = fs::read(plan.database())?;
            let protected = if alias_database { plan.database() } else { &input };
            let alias = data.join("index-run.lock.meta");
            fs::hard_link(protected, &alias)?;
            let error = plan.rebuild().expect_err("rebuild must recheck protected file identities");
            assert!(error.to_string().contains(if alias_database {
                "aliases the canonical database"
            } else {
                "aliases the logical archive input"
            }));
            assert_eq!(fs::read(plan.database())?, before);
            assert_eq!(fs::read(&input)?, b"interchange input");
            assert!(!data.join("index-run.lock").exists());
            assert!(!expected_index_dir(&data).exists());
        }
        Ok(())
    }

    #[test]
    fn replacing_the_input_path_does_not_replace_its_admitted_identity() -> Result<()> {
        let (root, input, data) = fixture()?;
        let plan = ArchiveIndexPlan::prepare(&data.join("agent_search.db"), &input)?;
        let retained = root.path().join("retained-input.jsonl");
        fs::rename(&input, &retained)?;
        fs::write(&input, b"interchange input")?;
        let error = plan.rebuild().expect_err("equal bytes do not prove the admitted file identity");
        assert!(error.to_string().contains("input changed after indexed restore admission"));
        assert_eq!(fs::read(&input)?, fs::read(&retained)?);
        assert_eq!(fs::read_dir(&data)?.count(), 0);
        Ok(())
    }

    #[test]
    fn maintenance_walk_refuses_excessive_depth_without_retaining_a_tree() -> Result<()> {
        let (_root, input, data) = fixture()?;
        let index = expected_index_dir(&data);
        let mut path = index.parent().expect("the index has a staging parent").to_path_buf();
        fs::create_dir_all(&path)?;
        for _ in 0..MAX_MAINTENANCE_DEPTH {
            path.push("nested");
            fs::create_dir(&path)?;
        }
        let error = ArchiveIndexPlan::prepare(&data.join("agent_search.db"), &input)
            .expect_err("maintenance traversal must have a fixed depth ceiling");
        assert!(error.to_string().contains("exceeds the supported depth"));
        assert_eq!(fs::read(&input)?, b"interchange input");
        assert!(!data.join("agent_search.db").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_symlinks_are_refused_before_import_and_again_before_rebuild() -> Result<()> {
        for relative in maintenance_alias_paths() {
            let (_root, input, data) = fixture()?;
            let plan = ArchiveIndexPlan::prepare(&data.join("agent_search.db"), &input)?;
            let alias = data.join(relative);
            fs::create_dir_all(alias.parent().expect("maintenance path has a parent"))?;
            std::os::unix::fs::symlink(&input, &alias)?;
            assert!(ArchiveIndexPlan::prepare(plan.database(), &input).is_err());
            let error = plan.rebuild().expect_err("rebuild must not follow a newly added symlink");
            assert!(error.to_string().contains("symlink"));
            assert_eq!(fs::read(&input)?, b"interchange input");
            assert!(!plan.database().exists());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_special_files_are_refused_without_opening_them() -> Result<()> {
        let (_root, input, data) = fixture()?;
        let socket = data.join("index-run.lock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket)?;
        let error = ArchiveIndexPlan::prepare(&data.join("agent_search.db"), &input)
            .expect_err("a maintenance socket is not a regular file");
        assert!(error.to_string().contains("requires regular"));
        assert_eq!(fs::read(&input)?, b"interchange input");
        assert!(!data.join("agent_search.db").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn index_and_profile_aliases_are_refused_without_touching_their_targets() -> Result<()> {
        let (root, input, data) = fixture()?;
        let target = root.path().join("unrelated");
        std::fs::create_dir(&target)?;
        std::fs::write(target.join("keep"), b"unrelated evidence")?;
        let alias = root.path().join("profile-alias");
        std::os::unix::fs::symlink(&data, &alias)?;
        assert!(ArchiveIndexPlan::prepare(&alias.join("agent_search.db"), &input).is_err());
        std::os::unix::fs::symlink(&target, data.join("index"))?;
        assert!(ArchiveIndexPlan::prepare(&data.join("agent_search.db"), &input).is_err());
        assert_eq!(std::fs::read(target.join("keep"))?, b"unrelated evidence");
        assert_eq!(std::fs::read_dir(&target)?.count(), 1);
        Ok(())
    }
}
