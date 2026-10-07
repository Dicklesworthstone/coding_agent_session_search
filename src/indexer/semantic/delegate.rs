//! Preserve the UFCS API without a Deref escape around artifact ownership.
//! Full rebuilds own unpublished scratch until native generation installation;
//! a rejected input must never reach cleanup at the published destination.

use super::*;
use anyhow::{Context, ensure};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use frankensearch::index::{Quantization, VectorIndex, next_generation, wal_path_for};

use crate::search::semantic_manifest::TierKind;
use crate::search::vector_index::{SemanticDocId, vector_index_path};

mod append;
mod rebuild;
mod reconcile;

impl SemanticIndexer {
    pub fn batch_size(&self) -> usize {
        self.inner.batch_size()
    }

    pub fn embedder_id(&self) -> &str {
        self.inner.embedder_id()
    }

    pub fn embedder_dimension(&self) -> usize {
        self.inner.embedder_dimension()
    }

    pub fn embed_messages(&self, messages: &[EmbeddingInput]) -> Result<Vec<EmbeddedMessage>> {
        self.inner.embed_messages(messages)
    }

    pub fn embed_messages_with_sink(
        &self,
        messages: &[EmbeddingInput],
        sink: &SemanticProgressSink,
    ) -> Result<Vec<EmbeddedMessage>> {
        self.inner.embed_messages_with_sink(messages, sink)
    }

    pub(crate) fn embed_messages_with_progress<F>(
        &self,
        messages: &[EmbeddingInput],
        on_progress: F,
    ) -> Result<Vec<EmbeddedMessage>>
    where
        F: FnMut(usize, usize),
    {
        self.inner
            .embed_messages_with_progress(messages, on_progress)
    }

    /// Build in private scratch and publish only a complete, usable generation.
    /// Existing WAL entries require canonical reconciliation; an FEC sidecar
    /// requires owner-aware retirement. Unreadable destination headers are
    /// retained for explicit recovery, not overwritten.
    pub fn build_and_save_index<I>(
        &self,
        embedded_messages: I,
        data_dir: &Path,
    ) -> Result<VectorIndex>
    where
        I: IntoIterator<Item = EmbeddedMessage>,
    {
        self.publish_full_rebuild(embedded_messages, data_dir, None::<fn(usize)>)
    }

    pub(crate) fn build_and_save_index_with_progress<I, F>(
        &self,
        embedded_messages: I,
        data_dir: &Path,
        on_progress: F,
    ) -> Result<VectorIndex>
    where
        I: IntoIterator<Item = EmbeddedMessage>,
        F: FnMut(usize),
    {
        self.publish_full_rebuild(embedded_messages, data_dir, Some(on_progress))
    }

    /// The maintenance lock still spans selection through publication. Both
    /// first builds and replacements use cancellable, private preparation.
    fn publish_full_rebuild<I, F>(
        &self,
        embedded_messages: I,
        data_dir: &Path,
        on_progress: Option<F>,
    ) -> Result<VectorIndex>
    where
        I: IntoIterator<Item = EmbeddedMessage>,
        F: FnMut(usize),
    {
        rebuild::run(self, embedded_messages, data_dir, on_progress, || {
            self.inner.check_external_cancelled()
        })
    }

    pub fn build_and_save_index_shards<I>(
        &self,
        embedded_messages: I,
        data_dir: &Path,
        plan: SemanticShardBuildPlan,
    ) -> Result<SemanticShardBuildOutcome>
    where
        I: IntoIterator<Item = EmbeddedMessage>,
    {
        self.inner
            .build_and_save_index_shards(embedded_messages, data_dir, plan)
    }

    /// Prepare one bounded, validated, last-write-wins batch before taking the
    /// live writer lock. Oversized requests are refused, never partially split.
    /// CASS_SEMANTIC_APPEND_MAX_BYTES can lower the 64 MiB retained-payload cap.
    pub fn append_to_index(
        &self,
        embedded_messages: impl IntoIterator<Item = EmbeddedMessage>,
        data_dir: &Path,
    ) -> Result<usize> {
        append::run(self, embedded_messages, data_dir)
    }

    /// Reconcile against the caller's authoritative canonical identity set.
    /// An exactly unchanged, log-free generation is returned as a shared
    /// query owner without replacing its file or retiring recovery protection.
    /// Changed generations are assembled directly from a retained read-only
    /// source, without copying it into a second mutable snapshot. A first
    /// publication requires complete replacement coverage and no orphan WAL.
    pub fn reconcile_index_with_canonical_documents(
        &self,
        embedded_messages: Vec<EmbeddedMessage>,
        data_dir: &Path,
        tier: TierKind,
        db_fingerprint: &str,
        current_doc_ids: &HashSet<String>,
    ) -> Result<VectorIndex> {
        reconcile::run(
            self,
            embedded_messages,
            data_dir,
            tier,
            db_fingerprint,
            current_doc_ids,
            || self.inner.check_external_cancelled(),
        )
    }

    /// Return a shared, query-only owner when no vector or identity changes
    /// are necessary. Never infer this from the record count, fingerprint, or
    /// an empty embedding delta alone. Callers retain their maintenance lock
    /// and canonical-set authority; subsequent mutation needs a writer open.
    fn retain_unchanged_canonical_index(
        &self,
        destination: &Path,
        current_doc_ids: &HashSet<String>,
        check_cancelled: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<VectorIndex>> {
        check_cancelled()?;
        let before = RebuildDestination::capture(destination)?;
        if before.file.is_none() {
            return Ok(None);
        }
        let revision = expected_vector_space_revision(self.embedder_id())
            .context("canonical reconciliation has no registered vector-space revision")?;
        let source = VectorIndex::open_read_only(destination)?;
        check_cancelled()?;
        // Admission failures never authorize falling through to a mutable
        // snapshot of a source whose owner changed during the observation.
        ensure!(
            RebuildDestination::capture(destination)? == before
                && ObservedSemanticFile::capture(&wal_path_for(destination))?.is_none(),
            "semantic source changed while checking unchanged canonical coverage"
        );
        if source.embedder_id() != self.embedder_id()
            || source.embedder_revision() != revision
            || source.dimension() != self.embedder_dimension()
            || source.record_count() != current_doc_ids.len()
            || source.wal_record_count() != 0
        {
            return Ok(None);
        }

        // Borrow IDs from the caller instead of retaining a second string
        // inventory. Explicit removal also detects duplicate physical rows;
        // equal counts or adjacent-only duplicate checks are insufficient.
        let mut remaining = HashSet::new();
        remaining.try_reserve(current_doc_ids.len())?;
        for id in current_doc_ids {
            check_cancelled()?;
            remaining.insert(id.as_str());
        }
        for row in 0..source.record_count() {
            check_cancelled()?;
            if source.is_deleted(row) || !remaining.remove(source.doc_id_at(row)?) {
                return Ok(None);
            }
        }
        if !remaining.is_empty() {
            return Ok(None);
        }
        for row in 0..source.record_count() {
            check_cancelled()?;
            ensure!(
                source.is_vector_usable(row),
                "unchanged canonical coverage has an unusable stored vector at row {row}; rebuild from canonical text"
            );
        }
        ensure!(
            RebuildDestination::capture(destination)? == before
                && ObservedSemanticFile::capture(&wal_path_for(destination))?.is_none(),
            "semantic source changed during unchanged canonical validation"
        );
        check_cancelled()?;
        Ok(Some(source))
    }

    pub fn build_hnsw_index(
        &self,
        vector_index: &VectorIndex,
        data_dir: &Path,
        m: Option<usize>,
        ef_construction: Option<usize>,
    ) -> Result<PathBuf> {
        self.inner
            .build_hnsw_index(vector_index, data_dir, m, ef_construction)
    }

    pub(crate) fn completed_backfill_fingerprint(
        &self,
        storage: &FrankenStorage,
        data_dir: &Path,
        manifest: &SemanticManifest,
        tier: TierKind,
        model_revision: &str,
    ) -> Result<Option<String>> {
        self.inner
            .completed_backfill_fingerprint(storage, data_dir, manifest, tier, model_revision)
    }
}

fn refuse_publication_sidecars(destination: &Path, canonical_reconciliation: bool) -> Result<()> {
    let mut fec = destination.as_os_str().to_os_string();
    fec.push(".fec");
    // install_replacement handles WAL generation binding, but does not retire
    // an old FEC image. Never let successful replacement leave parity capable
    // of restoring the old generation. Refuse instead of deleting an entry
    // whose recovery ownership has not been admitted by this API.
    for (path, field, reason) in [
        (
            wal_path_for(destination),
            "wal_sidecar",
            "full rebuild refuses an existing WAL; reconcile acknowledged vectors before replacement",
        ),
        (
            PathBuf::from(fec),
            "fec_sidecar",
            "full rebuild refuses an existing FEC sidecar; retire recovery protection through its owner before replacement",
        ),
    ] {
        if canonical_reconciliation && field == "wal_sidecar" {
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(frankensearch::SearchError::InvalidConfig {
                    field: field.into(),
                    value: path.display().to_string(),
                    reason: reason.into(),
                }
                .into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect semantic destination sidecar"),
        }
    }
    Ok(())
}

/// Keep the inode alive while a replacement is prepared. The wrapping one-byte
/// generation alone cannot detect a different same-generation publication or
/// an in-place change. These observations supplement, not replace, the caller's
/// maintenance lock; they are not a filesystem compare-and-swap primitive.
#[derive(Debug, PartialEq, Eq)]
struct RebuildDestination {
    file: Option<ObservedSemanticFile>,
    generation: Option<u8>,
}

impl RebuildDestination {
    fn capture(path: &Path) -> Result<Self> {
        let file = ObservedSemanticFile::capture(path)?;
        let generation = if file.is_some() {
            Some(
                VectorIndex::peek_compaction_gen(path)
                    .context("inspect semantic destination generation without changing it")?,
            )
        } else {
            None
        };
        ensure!(
            ObservedSemanticFile::capture(path)? == file,
            "semantic destination changed while inspecting its generation"
        );
        Ok(Self { file, generation })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ObservedSemanticFile {
    handle: same_file::Handle,
    len: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl ObservedSemanticFile {
    fn capture(path: &Path) -> Result<Option<Self>> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => ensure!(
                metadata.file_type().is_file(),
                "semantic artifact must be a regular file, not a symlink or directory"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("inspect semantic artifact"),
        }
        let handle = same_file::Handle::from_path(path)?;
        let metadata = handle.as_file().metadata()?;
        ensure!(
            metadata.is_file(),
            "opened semantic artifact is not a regular file"
        );
        #[cfg(unix)]
        let changed = {
            use std::os::unix::fs::MetadataExt;
            (metadata.ctime(), metadata.ctime_nsec())
        };
        Ok(Some(Self {
            handle,
            len: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            changed,
        }))
    }
}

#[cfg(test)]
mod full_rebuild_tests {
    use super::*;

    mod canonical;
    mod cancellation;

    fn embedded(indexer: &SemanticIndexer) -> Result<Vec<EmbeddedMessage>> {
        indexer.embed_messages(&[
            EmbeddingInput::new(1, "first compiler record"),
            EmbeddingInput::new(2, "second network record"),
            EmbeddingInput::new(3, "third checkpoint record"),
        ])
    }

    #[test]
    fn full_rebuild_progress_reports_only_accepted_rows_before_rejection() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let indexer = SemanticIndexer::new("hash", None)?;
        drop(indexer.build_and_save_index(embedded(&indexer)?, temp.path())?);
        let path = vector_index_path(temp.path(), indexer.embedder_id());
        let before = fs::read(&path)?;
        let mut replacement = embedded(&indexer)?;
        replacement[1].embedding[0] = f32::NAN;
        let mut progress = Vec::new();
        assert!(
            indexer
                .build_and_save_index_with_progress(replacement, temp.path(), |accepted| progress
                    .push(accepted),)
                .is_err()
        );
        assert_eq!(progress, vec![1]);
        assert_eq!(fs::read(path)?, before);
        Ok(())
    }

    #[test]
    fn full_rebuild_progress_unwind_preserves_the_installed_generation() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let indexer = SemanticIndexer::new("hash", None)?;
        drop(indexer.build_and_save_index(embedded(&indexer)?, temp.path())?);
        let path = vector_index_path(temp.path(), indexer.embedder_id());
        let before = fs::read(&path)?;
        let replacement = embedded(&indexer)?;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = indexer.build_and_save_index_with_progress(replacement, temp.path(), |_| {
                panic!("intentional progress callback interruption");
            });
        }));
        assert!(outcome.is_err());
        assert_eq!(fs::read(&path)?, before);
        assert_eq!(VectorIndex::open_read_only(&path)?.record_count(), 3);
        Ok(())
    }

    #[test]
    fn full_rebuild_first_publication_is_invisible_until_all_callbacks_finish() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let indexer = SemanticIndexer::new("hash", None)?;
        let path = vector_index_path(temp.path(), indexer.embedder_id());
        let mut progress = Vec::new();
        let result = indexer.build_and_save_index_with_progress(
            embedded(&indexer)?,
            temp.path(),
            |accepted| {
                assert!(!path.exists(), "buffered prefix must not be published");
                progress.push(accepted);
            },
        )?;
        assert_eq!(progress, vec![1, 2, 3]);
        assert_eq!(result.path(), path);
        assert_eq!(result.record_count(), 3);
        Ok(())
    }
}
