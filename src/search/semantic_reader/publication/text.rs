//! Text inference through a manifest-selected generation, retaining publication
//! provenance through both synchronous and lazy progressive result batches.
//!
//! The caller still supplies its independently pinned canonical corpus identity
//! when opening the selected reader. This adapter never infers that identity
//! from a model name, vector file, or the current pointer being inspected.

use super::super::text::{TextQueryProducers, TextSemanticSearch};
use super::*;

impl SelectedSemanticGeneration {
    /// Activate text against this exact selected publication and explicit model
    /// owners. Every requested producer is checked before first inference, with
    /// no filename discovery, automatic model download or hash substitution.
    ///
    /// Producer identity inspection may initialize a supplied lazy model. The
    /// quality inference and vector scan remain deferred by progressive().
    pub fn activate_text<'reader, 'request>(
        &'reader self,
        text: &'request str,
        producers: TextQueryProducers<'request>,
    ) -> SemanticReaderResult<SelectedTextSemanticSearch<'reader, 'request>> {
        Ok(SelectedTextSemanticSearch {
            search: self.reader.activate_text(text, producers)?,
            identity: Arc::clone(&self.identity),
        })
    }
}

/// Checked model owners and the selected reader remain borrowed throughout a
/// request; the exact publication identity also travels with every owned batch.
/// Synchronous inference/scan calls require caller-owned deadline supervision.
pub struct SelectedTextSemanticSearch<'reader, 'request> {
    search: TextSemanticSearch<'reader, 'request>,
    identity: Arc<SemanticSelectionIdentity>,
}

impl std::fmt::Debug for SelectedTextSemanticSearch<'_, '_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Do not expose corpus labels or paths through the publication's Debug.
        formatter
            .debug_struct("SelectedTextSemanticSearch")
            .field("search", &self.search)
            .finish_non_exhaustive()
    }
}

impl SelectedTextSemanticSearch<'_, '_> {
    /// The exact publication selected during admission, not a reread of CURRENT.
    pub fn selection(&self) -> &SemanticSelectionIdentity {
        &self.identity
    }

    /// Independently infer/search every requested tier, retaining source evidence.
    pub fn search(
        &self,
        k: usize,
        filter: Option<&dyn SearchFilter>,
    ) -> SemanticReaderResult<SelectedSemanticBatch> {
        self.search
            .search(k, filter)
            .map(|batch| self.retain(batch))
    }

    /// Opt into only the graphs already admitted by the selected reader.
    pub fn search_with_ann(
        &self,
        k: usize,
        filter: Option<&dyn SearchFilter>,
        policy: AnnSearchPolicy,
    ) -> SemanticReaderResult<SelectedSemanticBatch> {
        self.search
            .search_with_ann(k, filter, policy)
            .map(|batch| self.retain(batch))
    }

    /// Fast inference/retrieval first, independent quality inference/retrieval
    /// second. Both batches retain the same selected pointer, manifest, corpus
    /// identity and vector owners, even if publication paths subsequently change.
    pub fn progressive<'call>(
        &'call self,
        k: usize,
        filter: Option<&'call dyn SearchFilter>,
    ) -> SemanticReaderResult<
        impl std::iter::FusedIterator<Item = SemanticReaderResult<SelectedSemanticBatch>> + 'call,
    > {
        Ok(self
            .search
            .progressive(k, filter)?
            .map(move |result| result.map(|batch| self.retain(batch))))
    }

    /// Preserve lazy inference and publication provenance with explicit ANN policy.
    pub fn progressive_with_ann<'call>(
        &'call self,
        k: usize,
        filter: Option<&'call dyn SearchFilter>,
        policy: AnnSearchPolicy,
    ) -> SemanticReaderResult<
        impl std::iter::FusedIterator<Item = SemanticReaderResult<SelectedSemanticBatch>> + 'call,
    > {
        Ok(self
            .search
            .progressive_with_ann(k, filter, policy)?
            .map(move |result| result.map(|batch| self.retain(batch))))
    }

    fn retain(&self, batch: SemanticSearchBatch) -> SelectedSemanticBatch {
        SelectedSemanticBatch {
            batch,
            identity: Arc::clone(&self.identity),
        }
    }
}
