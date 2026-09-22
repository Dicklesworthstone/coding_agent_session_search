//! Explicit forwarding keeps the existing UFCS API without exposing a Deref
//! escape hatch around the backfill artifact lease.

use super::*;
use std::collections::HashSet;
use std::path::PathBuf;

use frankensearch::index::VectorIndex;

use crate::search::semantic_manifest::TierKind;

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

    pub fn build_and_save_index<I>(
        &self,
        embedded_messages: I,
        data_dir: &Path,
    ) -> Result<VectorIndex>
    where
        I: IntoIterator<Item = EmbeddedMessage>,
    {
        self.inner.build_and_save_index(embedded_messages, data_dir)
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
        self.inner
            .build_and_save_index_with_progress(embedded_messages, data_dir, on_progress)
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
        self.inner.reconcile_index_with_canonical_documents(
            embedded_messages,
            data_dir,
            tier,
            db_fingerprint,
            current_doc_ids,
        )
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
