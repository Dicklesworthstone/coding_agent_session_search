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

    /// The caller's existing maintenance lock must cover selection and this
    /// publication. This method does not grant permission to discard pending
    /// WAL writes: those require canonical reconciliation, not a full writer.
    fn publish_full_rebuild<I, F>(
        &self,
        embedded_messages: I,
        data_dir: &Path,
        mut on_progress: Option<F>,
    ) -> Result<VectorIndex>
    where
        I: IntoIterator<Item = EmbeddedMessage>,
        F: FnMut(usize),
    {
        let destination = vector_index_path(data_dir, self.embedder_id());
        refuse_publication_sidecars(&destination, false)?;
        let previous = RebuildDestination::capture(&destination)?;
        let previous_generation = previous.generation;
        let parent = destination.parent().context("semantic index has no parent")?;
        fs::create_dir_all(parent)?;
        let scratch = tempfile::Builder::new()
            .prefix(".semantic-full-rebuild-")
            .tempdir_in(parent)?;
        let candidate_path = vector_index_path(scratch.path(), self.embedder_id());

        if let Some(generation) = previous_generation {
            // A fresh writer's default generation is not a successor of the
            // existing destination. Stamp the successor at construction, not
            // by editing a finished header or deleting the destination's WAL.
            fs::create_dir_all(candidate_path.parent().context("candidate has no parent")?)?;
            let revision = expected_vector_space_revision(self.embedder_id())
                .context("full rebuild has no registered vector-space revision")?;
            let mut writer = VectorIndex::create_with_revision(
                &candidate_path,
                self.embedder_id(),
                revision,
                self.embedder_dimension(),
                Quantization::F16,
            )?
            .with_generation(next_generation(generation));
            let mut accepted = 0usize;
            for embedded in embedded_messages {
                ensure!(
                    embedded.embedding.len() == self.embedder_dimension(),
                    "embedding dimension mismatch: expected {}, got {}",
                    self.embedder_dimension(),
                    embedded.embedding.len()
                );
                ensure!(
                    embedded.embedding.iter().all(|value| value.is_finite()),
                    "embedding for message {} contains a non-finite value",
                    embedded.message_id
                );
                let doc_id = SemanticDocId {
                    message_id: embedded.message_id,
                    chunk_idx: embedded.chunk_idx,
                    agent_id: embedded.agent_id,
                    workspace_id: embedded.workspace_id,
                    source_id: embedded.source_id,
                    role: embedded.role,
                    created_at_ms: embedded.created_at_ms,
                    content_hash: Some(embedded.content_hash),
                }
                .to_doc_id_string();
                writer.write_record(&doc_id, &embedded.embedding).map_err(|error| {
                    let message = format!("write fsvi record failed: {error}");
                    anyhow::Error::new(error).context(message)
                })?;
                accepted = accepted.saturating_add(1);
                if let Some(progress) = on_progress.as_mut() {
                    progress(accepted);
                }
            }
            writer.finish().context("finish unpublished semantic replacement")?;
        } else {
            // Initial builds can use the original engine's writer unchanged,
            // but ONLY under fresh owned scratch, never the public path. Both
            // progress variants retain the engine's normal streaming behavior.
            let candidate = match on_progress {
                Some(progress) => self.inner.build_and_save_index_with_progress(
                    embedded_messages, scratch.path(), progress,
                )?,
                None => self.inner.build_and_save_index(embedded_messages, scratch.path())?,
            };
            drop(candidate);
        }

        // Validate the persisted representation too: a finite nonzero f32
        // vector may overflow or lose all signal when encoded to f16. Keep
        // only one decoded row at a time, never another corpus-sized slab.
        let candidate = VectorIndex::open_read_only(&candidate_path)?;
        ensure!(candidate.wal_record_count() == 0, "unpublished rebuild has a WAL");
        for row in 0..candidate.record_count() {
            ensure!(candidate.is_vector_usable(row), "unusable persisted semantic vector at row {row}");
        }
        drop(candidate);

        // Do not reclassify a missing/corrupt/replaced destination as permission
        // to overwrite it. The native installer also rechecks the generation.
        refuse_publication_sidecars(&destination, false)?;
        ensure!(
            RebuildDestination::capture(&destination)? == previous,
            "semantic destination changed during full rebuild; retry under the maintenance lock"
        );
        VectorIndex::install_replacement(&destination, &candidate_path)
            .context("install complete semantic rebuild")
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

    pub fn append_to_index(
        &self,
        embedded_messages: impl IntoIterator<Item = EmbeddedMessage>,
        data_dir: &Path,
    ) -> Result<usize> {
        self.inner.append_to_index(embedded_messages, data_dir)
    }

    pub fn reconcile_index_with_canonical_documents(
        &self,
        embedded_messages: Vec<EmbeddedMessage>,
        data_dir: &Path,
        tier: TierKind,
        db_fingerprint: &str,
        current_doc_ids: &HashSet<String>,
    ) -> Result<VectorIndex> {
        let destination = vector_index_path(data_dir, self.embedder_id());
        refuse_publication_sidecars(&destination, true)?;
        let wal_path = wal_path_for(&destination);
        let wal_before = ObservedSemanticFile::capture(&wal_path)?;
        // Keep the established no-WAL implementation. The generation-bound
        // merge below addresses retained deltas, including the full-replacement
        // case which cannot reset the main generation beside an old WAL.
        if wal_before.is_none() {
            return self.inner.reconcile_index_with_canonical_documents(
                embedded_messages, data_dir, tier, db_fingerprint, current_doc_ids,
            );
        }
        ensure!(!db_fingerprint.trim().is_empty(),
            "canonical semantic reconciliation requires a DB fingerprint");
        let revision = expected_vector_space_revision(self.embedder_id())
            .context("canonical reconciliation has no registered vector-space revision")?;
        let mut replacements = HashMap::new();
        replacements.try_reserve(embedded_messages.len())?;
        // Reject the whole input before reading vectors or preparing a candidate.
        for embedded in embedded_messages {
            ensure!(embedded.embedding.len() == self.embedder_dimension(),
                "canonical replacement has the wrong vector dimension");
            let norm = embedded.embedding.iter().fold(0.0f32, |sum, value| sum + value * value);
            ensure!(norm.is_finite() && norm > 0.0,
                "canonical replacement has a non-finite or zero-norm vector");
            let id = SemanticDocId {
                message_id: embedded.message_id,
                chunk_idx: embedded.chunk_idx,
                agent_id: embedded.agent_id,
                workspace_id: embedded.workspace_id,
                source_id: embedded.source_id,
                role: embedded.role,
                created_at_ms: embedded.created_at_ms,
                content_hash: Some(embedded.content_hash),
            }.to_doc_id_string();
            ensure!(current_doc_ids.contains(&id),
                "replacement document is not in the current canonical set");
            ensure!(replacements.insert(id, embedded).is_none(),
                "duplicate canonical replacement document");
        }

        // Unlike an unqualified full rebuild, this call has the exact canonical
        // set needed to retire deleted records and retain acknowledged WAL rows.
        refuse_publication_sidecars(&destination, true)?;
        let previous = RebuildDestination::capture(&destination)?;
        let generation = previous.generation
            .context("canonical reconciliation requires an existing vector artifact")?;
        // Retain the real source under its shared native lock. No source copy,
        // compaction, tombstone edit or writer-capable source map is necessary.
        let source = VectorIndex::open_read_only(&destination)?;
        ensure!(RebuildDestination::capture(&destination)? == previous
                && ObservedSemanticFile::capture(&wal_path)? == wal_before,
            "semantic source changed while opening canonical reconciliation");
        let complete_replacement = replacements.len() == current_doc_ids.len();
        ensure!(complete_replacement
                || (source.embedder_id() == self.embedder_id()
                    && source.embedder_revision() == revision
                    && source.dimension() == self.embedder_dimension()),
            "incompatible vector space requires a complete canonical replacement");

        let parent = destination.parent().context("semantic index has no parent")?;
        let scratch = tempfile::Builder::new()
            .prefix(".semantic-reconcile-")
            .tempdir_in(parent)?;
        let candidate_path = scratch.path().join("candidate.fsvi");
        let mut writer = VectorIndex::create_with_revision(
            &candidate_path, self.embedder_id(), revision,
            self.embedder_dimension(),
            if complete_replacement { Quantization::F16 } else { source.quantization() },
        )?.with_generation(next_generation(generation));
        let mut remaining = HashSet::new();
        remaining.try_reserve(current_doc_ids.len())?;
        remaining.extend(current_doc_ids.iter().map(String::as_str));
        for (id, embedded) in &replacements {
            writer.write_record(id, &embedded.embedding)?;
            remaining.remove(id.as_str());
        }

        if !complete_replacement {
            let mut wal_ids = HashSet::new();
            wal_ids.try_reserve(source.wal_record_count())?;
            // The native reader has already applied last-write-wins within the
            // retained log. Admit those rows BEFORE main rows so a pre-cleanup
            // main image cannot override an acknowledged replacement.
            for (id, vector) in source.wal_records() {
                ensure!(wal_ids.insert(id), "duplicate retained WAL identity");
                if remaining.remove(id) {
                    writer.write_record(id, vector)?;
                }
            }
            for row in 0..source.record_count() {
                if source.is_deleted(row) {
                    continue;
                }
                let id = source.doc_id_at(row)?;
                if !current_doc_ids.contains(id) || replacements.contains_key(id) || wal_ids.contains(id) {
                    continue;
                }
                ensure!(remaining.remove(id), "duplicate current document in the source index");
                writer.write_record(id, &source.vector_at_f32(row)?)?;
            }
        }
        ensure!(remaining.is_empty(),
            "canonical reconciliation lacks vectors for {} current documents", remaining.len());
        writer.finish().context("finish unpublished canonical generation")?;

        let candidate = VectorIndex::open_read_only(&candidate_path)?;
        ensure!(candidate.wal_record_count() == 0 && candidate.tombstone_count() == 0
                && candidate.record_count() == current_doc_ids.len(),
            "canonical replacement has incomplete or non-live physical coverage");
        remaining.extend(current_doc_ids.iter().map(String::as_str));
        for row in 0..candidate.record_count() {
            ensure!(remaining.remove(candidate.doc_id_at(row)?),
                "persisted replacement has an unexpected or duplicate identity");
            ensure!(candidate.is_vector_usable(row),
                "unusable persisted canonical vector at row {row}");
        }
        ensure!(remaining.is_empty(), "persisted replacement lacks canonical identities");
        drop(candidate);

        // Leave the acknowledged WAL with its original main until the native
        // generation-aware rename invalidates it atomically. Never park/delete
        // it first, and never infer identity from the wrapping generation alone.
        refuse_publication_sidecars(&destination, true)?;
        ensure!(RebuildDestination::capture(&destination)? == previous
                && ObservedSemanticFile::capture(&wal_path)? == wal_before,
            "semantic source changed during canonical reconciliation; retry under the maintenance lock");
        let published = VectorIndex::install_replacement(&destination, &candidate_path)
            .context("install complete canonical semantic generation")?;
        tracing::info!(tier = tier.as_str(), published_docs = published.record_count(),
            replaced_docs = replacements.len(), retained_wal_rows = source.wal_record_count(),
            "published canonical semantic reconciliation");
        Ok(published)
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
        (wal_path_for(destination), "wal_sidecar", "full rebuild refuses an existing WAL; reconcile acknowledged vectors before replacement"),
        (PathBuf::from(fec), "fec_sidecar", "full rebuild refuses an existing FEC sidecar; retire recovery protection through its owner before replacement"),
    ] {
        if canonical_reconciliation && field == "wal_sidecar" {
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(_) => return Err(frankensearch::SearchError::InvalidConfig {
                field: field.into(),
                value: path.display().to_string(),
                reason: reason.into(),
            }.into()),
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
            Some(VectorIndex::peek_compaction_gen(path)
                .context("inspect semantic destination generation without changing it")?)
        } else {
            None
        };
        ensure!(ObservedSemanticFile::capture(path)? == file,
            "semantic destination changed while inspecting its generation");
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
            Ok(metadata) => ensure!(metadata.file_type().is_file(),
                "semantic artifact must be a regular file, not a symlink or directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("inspect semantic artifact"),
        }
        let handle = same_file::Handle::from_path(path)?;
        let metadata = handle.as_file().metadata()?;
        ensure!(metadata.is_file(), "opened semantic artifact is not a regular file");
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
        assert!(indexer.build_and_save_index_with_progress(
            replacement, temp.path(), |accepted| progress.push(accepted),
        ).is_err());
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
            embedded(&indexer)?, temp.path(), |accepted| {
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
