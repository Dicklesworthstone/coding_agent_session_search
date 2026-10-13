//! Text retrieval and source verification under one caller-owned read snapshot.
//!
//! The deadline starts at these entry points, before inference or retrieval.
//! Producer activation/admission happens earlier and is not included. Native
//! inference, vector scans and SQL remain cooperative boundaries, not preemptible
//! calls. No returned text bypasses the retained-batch fingerprint verifier.

pub mod current;
mod progressive;

use super::*;
use crate::search::semantic_reader::ann::AnnSearchPolicy;
use crate::search::semantic_reader::text::TextSemanticSearch;
use crate::search::semantic_reader::{SemanticReaderError, SemanticReaderResult};
use frankensearch::core::filter::SearchFilter;
use super::super::SelectedTextSemanticSearch;

/// A retrieval error remains distinct from a missing, changed or over-budget
/// source. Neither kind can turn a scored but unverified batch into text.
#[derive(Debug, thiserror::Error)]
pub enum HydratedSearchError {
    #[error(transparent)]
    Retrieval(#[from] SemanticReaderError),
    #[error(transparent)]
    Hydration(#[from] HydrationError),
}

pub type HydratedSearchResult<B> = Result<HydratedSemanticBatch<B>, HydratedSearchError>;

// Sealed in this module: only batch types retaining their serving owners can
// cross this composition boundary. No caller-supplied hit list is accepted.
trait BatchSource {
    fn batch(&self) -> &SemanticSearchBatch;
    fn batch_mut(&mut self) -> &mut SemanticSearchBatch;
}

// Current-source retrieval validates the complete union of the bounded tier
// windows before selecting its visible prefix. Keep coverage about returned
// contributors consistent with that prefix, without changing retrieved counts,
// scores, tier ranks, execution receipts, or the retained publication identity.
fn retain_verified_prefix<B: BatchSource>(batch: &mut B, k: usize) {
    let batch = batch.batch_mut();
    batch.hits.truncate(k);
    if let Some(coverage) = &mut batch.coverage.fast {
        coverage.contributed_candidates = batch.hits.iter().filter(|hit| hit.fast.is_some()).count();
    }
    if let Some(coverage) = &mut batch.coverage.quality {
        coverage.contributed_candidates = batch.hits.iter().filter(|hit| hit.quality.is_some()).count();
    }
}

impl BatchSource for SemanticSearchBatch {
    fn batch(&self) -> &SemanticSearchBatch { self }
    fn batch_mut(&mut self) -> &mut SemanticSearchBatch { self }
}

impl BatchSource for SelectedSemanticBatch {
    fn batch(&self) -> &SemanticSearchBatch { self.batch() }
    fn batch_mut(&mut self) -> &mut SemanticSearchBatch { &mut self.batch }
}

fn prepare(k: usize, phases: usize, budget: HydrationBudget) -> Result<Work, HydrationError> {
    let started = Instant::now();
    // Admit the requested worst-case output before a native search may allocate
    // from k. Progressive streams reserve both result pages, not just Initial.
    let requested = k.checked_mul(phases).ok_or(HydrationError::BudgetExceeded)?;
    budget.validate(requested)?;
    Work::new(budget, started)
}

fn verified<B: BatchSource>(
    batch: B,
    transaction: &Transaction<'_>,
    projection: PassageProjection,
    work: &mut Work,
) -> HydratedSearchResult<B> {
    work.check()?;
    let passages = hydrate_with_work(batch.batch(), transaction, projection, work)?;
    work.check()?;
    Ok(HydratedSemanticBatch { source: batch, passages, projection })
}

fn search<B: BatchSource>(
    transaction: &Transaction<'_>,
    projection: PassageProjection,
    mut work: Work,
    retrieve: impl FnOnce() -> SemanticReaderResult<B>,
) -> HydratedSearchResult<B> {
    work.check()?;
    let batch = retrieve()?;
    verified(batch, transaction, projection, &mut work)
}

impl TextSemanticSearch<'_, '_> {
    /// Infer, retrieve and verify canonical passages using one existing read
    /// snapshot and deadline. No transaction is begun, committed or rolled back.
    /// The caller must keep this snapshot and the filter stable during the call.
    pub fn search_hydrated_in_transaction(
        &self,
        k: usize,
        filter: Option<&dyn SearchFilter>,
        transaction: &Transaction<'_>,
        projection: PassageProjection,
        budget: HydrationBudget,
    ) -> HydratedSearchResult<SemanticSearchBatch> {
        let work = prepare(k, 1, budget)?;
        search(transaction, projection, work, || self.search(k, filter))
    }

    /// Preserve explicit native ANN policy, exact fallback receipts and source
    /// verification. The deadline is not renewed after native retrieval.
    pub fn search_hydrated_with_ann_in_transaction(
        &self,
        k: usize,
        filter: Option<&dyn SearchFilter>,
        policy: AnnSearchPolicy,
        transaction: &Transaction<'_>,
        projection: PassageProjection,
        budget: HydrationBudget,
    ) -> HydratedSearchResult<SemanticSearchBatch> {
        let work = prepare(k, 1, budget)?;
        search(transaction, projection, work, || self.search_with_ann(k, filter, policy))
    }
}

impl SelectedTextSemanticSearch<'_, '_> {
    /// Keep the selected pointer/manifest, serving owners and ranking attached
    /// to source-verified text. Admission and producer activation precede this
    /// deadline; all subsequent inference, retrieval and hydration share it.
    pub fn search_hydrated_in_transaction(
        &self,
        k: usize,
        filter: Option<&dyn SearchFilter>,
        transaction: &Transaction<'_>,
        projection: PassageProjection,
        budget: HydrationBudget,
    ) -> HydratedSearchResult<SelectedSemanticBatch> {
        let work = prepare(k, 1, budget)?;
        search(transaction, projection, work, || self.search(k, filter))
    }

    pub fn search_hydrated_with_ann_in_transaction(
        &self,
        k: usize,
        filter: Option<&dyn SearchFilter>,
        policy: AnnSearchPolicy,
        transaction: &Transaction<'_>,
        projection: PassageProjection,
        budget: HydrationBudget,
    ) -> HydratedSearchResult<SelectedSemanticBatch> {
        let work = prepare(k, 1, budget)?;
        search(transaction, projection, work, || self.search_with_ann(k, filter, policy))
    }
}

#[cfg(test)]
mod tests;
