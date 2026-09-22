//! Explicit, canonical-only lexical recovery after a logical archive import.
//!
//! This is not ordinary indexing: it never discovers provider histories,
//! imports historical bundles, or opens an embedder. The existing index-run
//! lock, scratch builder, checkpoint validation and atomic publisher remain
//! the only implementation of lexical reconstruction.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::search::tantivy::expected_index_dir;
use crate::storage::sqlite::FrankenStorage;

/// An explicit recovery target, independent of the ambient CASS_DATA_DIR.
/// Fields are private so callers cannot change the admitted layout afterward.
#[derive(Debug)]
pub struct ArchiveIndexPlan {
    database: PathBuf,
    data_dir: PathBuf,
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
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "indexed restore data directory must be a real directory, not a symlink"
        );
        let data_dir = parent
            .canonicalize()
            .context("resolve indexed restore directory")?;
        ensure!(
            data_dir.to_str().is_some(),
            "indexed restore path must be UTF-8"
        );
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
        };
        plan.validate_index_layout()?;
        Ok(plan)
    }

    /// The exact destination to pass to the logical importer.
    pub fn database(&self) -> &Path {
        &self.database
    }

    fn validate_index_layout(&self) -> Result<()> {
        let index = expected_index_dir(&self.data_dir);
        let relative = index
            .strip_prefix(&self.data_dir)
            .context("lexical index must be inside the recovered profile")?;
        ensure!(
            !relative.as_os_str().is_empty(),
            "lexical index cannot replace its data directory"
        );
        let mut path = self.data_dir.clone();
        for component in relative.components() {
            ensure!(
                matches!(component, std::path::Component::Normal(_)),
                "lexical index must stay inside the recovered profile"
            );
            path.push(component.as_os_str());
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) => ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "indexed restore refuses a non-directory or symlink in the lexical index path"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("inspect recovered lexical index path"),
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
        self.validate_index_layout()?;
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
