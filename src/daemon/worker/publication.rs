//! Complete daemon passes must publish the generation ordinary search selects.
//!
//! Job progress writes change the archive's physical witnesses. Capture the
//! publication snapshot only after those writes, then independently replay the
//! canonical document identities before sealing the completed vector artifact.
//! A retry may reuse every vector while still repairing a failed ledger save.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, ensure};

use super::WorkerEmbedderKind;
use crate::indexer::semantic::{
    SemanticBuildSnapshot, expected_vector_space_revision,
    invalidate_identity_stale_semantic_shards, semantic_doc_id_for_input,
    semantic_index_document_digest_for_canonical_ids, visit_packet_embedding_inputs_from_storage,
};
use crate::search::model_download::ModelManifest;
use crate::search::policy::{CHUNKING_STRATEGY_VERSION, SEMANTIC_SCHEMA_VERSION};
use crate::search::semantic_manifest::{
    ArtifactRecord, SemanticCurrentPointerV1, SemanticManifest, TierKind,
};
use crate::search::vector_index::{VectorIndex, vector_index_path};
use crate::storage::sqlite::{FrankenStorage, SemanticArchiveState, SemanticIdentityTier};

pub(super) struct PublicationAdmission {
    expected: Option<SemanticManifest>,
    tier: TierKind,
    embedder_id: String,
    dimension: usize,
    model_revision: String,
    started_at_ms: i64,
}

fn require_legacy_selection(data_dir: &Path) -> Result<()> {
    match fs::symlink_metadata(SemanticCurrentPointerV1::path(data_dir)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("inspect daemon semantic generation selection"),
        Ok(_) => anyhow::bail!(
            "daemon semantic publication requires the legacy serving ledger; an immutable current generation requires its owner-backed writer"
        ),
    }
}

fn check_cancelled(cancelled: &dyn Fn() -> bool) -> Result<()> {
    ensure!(!cancelled(), "daemon semantic publication cancelled");
    Ok(())
}

impl PublicationAdmission {
    /// Freeze the full ledger before vector work. A late publisher may not
    /// overwrite a newer tier, checkpoint, coverage proof, or ANN declaration.
    pub(super) fn capture(
        data_dir: &Path,
        kind: &WorkerEmbedderKind,
        tier: TierKind,
    ) -> Result<Self> {
        require_legacy_selection(data_dir)?;
        let (embedder_id, dimension, model_revision) = match kind {
            WorkerEmbedderKind::Hash => (
                "fnv1a-384".to_owned(),
                crate::search::hash_embedder::DEFAULT_DIMENSION,
                "hash".to_owned(),
            ),
            WorkerEmbedderKind::FastEmbed {
                model_name,
                embedder_id,
                dimension,
            } => (
                embedder_id.clone(),
                *dimension,
                ModelManifest::for_embedder(model_name)
                    .context("daemon producer has no pinned model revision")?
                    .revision,
            ),
        };
        let expected =
            SemanticManifest::load(data_dir).context("read daemon semantic publication ledger")?;
        if let Some(manifest) = &expected {
            let sibling = match tier {
                TierKind::Fast => manifest.quality_tier.as_ref(),
                TierKind::Quality => manifest.fast_tier.as_ref(),
            };
            // Coverage is keyed by producer, so replacing its certificate
            // cannot silently relabel a different tier's retained artifact.
            ensure!(
                sibling.is_none_or(|artifact| artifact.embedder_id != embedder_id),
                "daemon producer already belongs to the other published tier"
            );
        }
        Ok(Self {
            expected,
            tier,
            embedder_id,
            dimension,
            model_revision,
            started_at_ms: chrono::Utc::now().timestamp_millis(),
        })
    }

    /// The caller holds archive mutation admission through job completion.
    /// No progress/database writes may interleave with the proof/seal phase.
    pub(super) fn publish(
        self,
        storage: &FrankenStorage,
        data_dir: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        check_cancelled(cancelled)?;
        require_legacy_selection(data_dir)?;
        let state = storage
            .semantic_archive_state()?
            .context("daemon publication requires canonical archive authority")?;
        let maximum = storage.max_message_id()?.unwrap_or(0);
        let mut snapshot = SemanticBuildSnapshot::capture(storage, &self.embedder_id)?;
        ensure!(
            !cfg!(unix) || snapshot.is_some(),
            "daemon publication could not capture canonical file witnesses"
        );
        let verify = |snapshot: Option<&SemanticBuildSnapshot>| -> Result<()> {
            check_cancelled(cancelled)?;
            if let Some(snapshot) = snapshot {
                snapshot.verify(storage)?;
            }
            ensure!(
                storage.semantic_archive_state()?.as_ref() == Some(&state)
                    && storage.max_message_id()?.unwrap_or(0) == maximum,
                "canonical source changed during daemon semantic publication"
            );
            check_cancelled(cancelled)
        };
        verify(snapshot.as_ref())?;

        let mut documents = HashSet::new();
        visit_packet_embedding_inputs_from_storage(storage, |input| {
            check_cancelled(cancelled)?;
            if let Some(id) = semantic_doc_id_for_input(&input) {
                ensure!(
                    documents.insert(id),
                    "daemon publication encountered duplicate canonical document identities"
                );
            }
            Ok(())
        })?;
        verify(snapshot.as_ref())?;

        let path = vector_index_path(data_dir, &self.embedder_id);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("inspect completed daemon vectors"),
        };
        let mut manifest = self.expected.clone().unwrap_or_default();
        if let Some(metadata) = metadata {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "completed daemon vectors must be a regular file"
            );
            let index = VectorIndex::open_read_only(&path)?;
            ensure!(
                index.embedder_id() == self.embedder_id
                    && index.dimension() == self.dimension
                    && Some(index.embedder_revision())
                        == expected_vector_space_revision(&self.embedder_id),
                "completed daemon vectors have a different producer contract"
            );
            let digest = semantic_index_document_digest_for_canonical_ids(&index, &documents)?;
            for row in 0..index.record_count() {
                check_cancelled(cancelled)?;
                ensure!(
                    index.is_vector_usable(row),
                    "completed daemon vectors contain an unusable canonical row"
                );
            }
            let count = u64::try_from(index.record_count())?;
            drop(index);
            drop(documents);
            verify(snapshot.as_ref())?;

            let (conversations, fingerprint) = match snapshot.as_ref() {
                Some(snapshot) => (snapshot.conversation_count, snapshot.db_fingerprint.clone()),
                None => (
                    u64::try_from(storage.total_conversation_count()?)?,
                    crate::indexer::lexical_storage_fingerprint_for_storage(storage)?,
                ),
            };
            let mut artifact = ArtifactRecord {
                tier: self.tier,
                embedder_id: self.embedder_id.clone(),
                model_revision: self.model_revision.clone(),
                schema_version: SEMANTIC_SCHEMA_VERSION,
                chunking_version: CHUNKING_STRATEGY_VERSION,
                dimension: self.dimension,
                doc_count: count,
                conversation_count: conversations,
                db_fingerprint: fingerprint,
                index_path: path.strip_prefix(data_dir)?.to_string_lossy().into_owned(),
                size_bytes: metadata.len(),
                started_at_ms: self.started_at_ms,
                completed_at_ms: chrono::Utc::now().timestamp_millis(),
                ready: true,
            };
            if let Some(snapshot) = snapshot.as_mut() {
                check_cancelled(cancelled)?;
                snapshot.prepare_parents(storage, data_dir)?;
                check_cancelled(cancelled)?;
                let (sealed, certificate) = snapshot.seal(data_dir, &path, artifact, &digest)?;
                artifact = sealed;
                manifest.publish_covered_artifact(artifact.clone(), certificate)?;
            } else {
                // Match the existing non-Unix legacy publication contract;
                // never discard an authenticated owner without its file proof.
                let selected = match self.tier {
                    TierKind::Fast => manifest.fast_tier.as_ref(),
                    TierKind::Quality => manifest.quality_tier.as_ref(),
                };
                ensure!(
                    !manifest.coverage.contains_key(&self.embedder_id)
                        && selected.is_none_or(|prior| {
                            !manifest.coverage.contains_key(&prior.embedder_id)
                        }),
                    "daemon publication cannot replace certified coverage without file witnesses"
                );
                manifest.publish_artifact(artifact.clone());
            }
            manifest.refresh_backlog(artifact.conversation_count, &artifact.db_fingerprint);
        } else {
            ensure!(
                documents.is_empty(),
                "daemon completed without vectors for the canonical document set"
            );
            // Empty first jobs intentionally do not load a model or create a
            // vector file. Retire only a previous record for this producer;
            // absence must never leave its older certificate selected.
            let selected = match self.tier {
                TierKind::Fast => &mut manifest.fast_tier,
                TierKind::Quality => &mut manifest.quality_tier,
            };
            if selected
                .as_ref()
                .is_some_and(|record| record.embedder_id == self.embedder_id)
            {
                *selected = None;
                manifest.coverage.remove(&self.embedder_id);
                if let Some(ann) = manifest.hnsw.as_mut()
                    && ann.embedder_id == self.embedder_id
                {
                    ann.ready = false;
                }
            }
        }

        // publish_covered_artifact clears a same-tier checkpoint. Another
        // producer's or tier's unfinished work remains owned by that caller.
        if let Some(checkpoint) = self
            .expected
            .as_ref()
            .and_then(|prior| prior.checkpoint.as_ref())
            && (checkpoint.embedder_id != self.embedder_id || checkpoint.tier != self.tier)
        {
            manifest.checkpoint = Some(checkpoint.clone());
        }
        verify(snapshot.as_ref())?;
        require_legacy_selection(data_dir)?;
        check_cancelled(cancelled)?;
        // Even an empty first job performs the CAS. This refuses a concurrent
        // publication instead of acknowledging an unobserved serving ledger.
        manifest.save_if_unchanged(data_dir, self.expected.as_ref())?;

        self.acknowledge(storage, data_dir, &state, maximum, cancelled)
    }

    fn acknowledge(
        &self,
        storage: &FrankenStorage,
        data_dir: &Path,
        state: &SemanticArchiveState,
        maximum: i64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        // Serving selects by producer, independently of the requested daemon
        // pass order. Every native profile uses the canonical quality marker.
        let identity_tier = if self.embedder_id == "fnv1a-384" {
            SemanticIdentityTier::Fast
        } else {
            SemanticIdentityTier::Quality
        };
        check_cancelled(cancelled)?;
        // The daemon replaced the monolithic generation. Even a same-count
        // content edit can leave a formerly selected shard's coarse database
        // fingerprint unchanged; that shard must not win legacy admission.
        invalidate_identity_stale_semantic_shards(data_dir, &self.embedder_id)?;
        check_cancelled(cancelled)?;
        storage.set_last_embedded_message_id_if_unchanged(&self.embedder_id, maximum, state)?;
        check_cancelled(cancelled)?;
        storage.complete_semantic_identity_rebuild_if_current(identity_tier, state)?;
        // The guarded setters return false for both an already-current value
        // and a rejected source boundary. Inspect authority, not that boolean.
        // Our own writes have invalidated the earlier physical file stamps.
        ensure!(
            storage.semantic_archive_state()?.as_ref() == Some(state)
                && storage.get_last_embedded_message_id(&self.embedder_id)? == Some(maximum)
                && !storage.semantic_identity_rebuild_required(identity_tier)?,
            "canonical source changed before daemon publication acknowledgement"
        );
        check_cancelled(cancelled)
    }
}

#[cfg(all(test, unix))]
mod tests;
