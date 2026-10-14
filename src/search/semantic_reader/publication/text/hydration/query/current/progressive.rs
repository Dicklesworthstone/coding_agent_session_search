//! Current-source progressive delivery over one retained generation and snapshot.
//!
//! Each phase completes bounded stale-hit refill before exposing text. Refinement
//! reuses fast ranking only while its exclusions are unchanged. New quality-side
//! exclusions force both tier rankings to be recomputed before refusion. Query inference is request-cached;
//! no model is run twice merely because retrieval needs another pass.

use std::iter::FusedIterator;

use super::*;
use crate::search::semantic_reader::text::refinement::FastTierCache;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Initial,
    Refined,
}

struct Continuation<'request> {
    request: CurrentTextQuery<'request>,
    cached: CachedProducers<'request>,
    work: Work,
    state: RefillState,
    phase: Phase,
    ranking: FastTierCache,
}

struct CurrentStream<'request, 'connection, F> {
    continuation: Option<Continuation<'request>>,
    transaction: &'request Transaction<'connection>,
    retrieve: F,
}

impl<'request, F, B> Iterator for CurrentStream<'request, '_, F>
where
    F: Fn(
        &CachedProducers<'request>,
        &CurrentTextQuery<'request>,
        Phase,
        &Excluding<'_>,
        &mut FastTierCache,
    ) -> SemanticReaderResult<B>,
    B: BatchSource,
{
    type Item = Result<CurrentSemanticBatch<B>, CurrentSearchError>;

    fn next(&mut self) -> Option<Self::Item> {
        // Take ownership BEFORE inference/SQL. Error or unwinding leaves no
        // continuation to restart, and drops cached vectors/exclusions too.
        let mut current = self.continuation.take()?;
        let phase = current.phase;
        let cached = &current.cached;
        let request = &current.request;
        let ranking = &mut current.ranking;
        let mut result = refill_accounted(
            request,
            self.transaction,
            &mut current.work,
            &mut current.state,
            |filter| (self.retrieve)(cached, request, phase, filter, ranking),
        );
        current.state.stats.fast_tier_reuses = current.ranking.reuses();
        if let Ok(batch) = &mut result {
            batch.stats.fast_tier_reuses = current.state.stats.fast_tier_reuses;
        }
        if result.is_ok() && current.phase == Phase::Initial {
            current.phase = Phase::Refined;
            self.continuation = Some(current);
        }
        // After Refined, embeddings, fast candidates and exclusions are released now.
        // A later poll cannot manufacture a third deadline error.
        Some(result)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = match self.continuation.as_ref().map(|current| current.phase) {
            Some(Phase::Initial) => 2,
            Some(Phase::Refined) => 1,
            None => 0,
        };
        (0, Some(remaining))
    }
}

impl<'request, F, B> FusedIterator for CurrentStream<'request, '_, F>
where
    F: Fn(
        &CachedProducers<'request>,
        &CurrentTextQuery<'request>,
        Phase,
        &Excluding<'_>,
        &mut FastTierCache,
    ) -> SemanticReaderResult<B>,
    B: BatchSource,
{
}

fn prepare<'request>(
    request: CurrentTextQuery<'request>,
    validate: impl FnOnce(
        &CurrentTextQuery<'request>,
        &CachedProducers<'request>,
    ) -> SemanticReaderResult<()>,
) -> Result<Continuation<'request>, CurrentSearchError> {
    if !matches!(request.producers, TextQueryProducers::Progressive { .. }) {
        return Err(SemanticReaderError::ProgressiveRequiresBothTiers.into());
    }
    // Admit both result pages and at least one retrieval pass per phase before
    // producer identity inspection can initialize a caller-supplied lazy model.
    let work = request.admit_phases(2)?;
    let cached = CachedProducers::new(request.text, request.producers);
    validate(&request, &cached)?;
    work.check()?;
    let state = RefillState::new(request.refill);
    Ok(Continuation {
        request,
        cached,
        work,
        state,
        phase: Phase::Initial,
        ranking: FastTierCache::default(),
    })
}

// Incumbent phase driver retained only for its existing control tests.
// Runtime uses the native TierResult continuation rather than repeating Initial.
#[cfg(test)]
fn phase_batch<B>(
    mut phases: impl Iterator<Item = SemanticReaderResult<B>>,
    phase: Phase,
) -> SemanticReaderResult<B> {
    let initial = phases
        .next()
        .ok_or(SemanticReaderError::InvalidCandidate)??;
    match phase {
        Phase::Initial => Ok(initial),
        Phase::Refined => {
            drop(initial);
            phases.next().ok_or(SemanticReaderError::InvalidCandidate)?
        }
    }
}

impl SemanticGenerationReader {
    /// Deliver current, verified Initial passages before running quality
    /// inference, then independently retrieve and verify a Refined page.
    ///
    /// Missing/changed passages trigger the same bounded refill as synchronous
    /// current-source search. Both phases share exclusions, inference responses,
    /// one caller-owned snapshot, and every passage/byte/search/deadline limit.
    /// Stats are cumulative snapshots; they must not be summed across phases.
    ///
    /// Admission validates BOTH producers, which may initialize a lazy model;
    /// actual quality inference/search waits for the second iterator step.
    /// Dropping after Initial never starts quality. Any error/panic is terminal.
    /// Native calls remain cooperative, not forcibly preemptible.
    pub fn progressive_current_text_in_transaction<'call>(
        &'call self,
        request: CurrentTextQuery<'call>,
        transaction: &'call Transaction<'_>,
    ) -> Result<
        impl FusedIterator<Item = Result<CurrentSemanticBatch<SemanticSearchBatch>, CurrentSearchError>>
        + 'call,
        CurrentSearchError,
    > {
        let continuation = prepare(request, |request, cached| {
            let search = self.activate_text(request.text, cached.producers())?;
            if let Some(policy) = request.ann {
                let _ = search.progressive_with_ann(request.limit, request.filter, policy)?;
            } else {
                let _ = search.progressive(request.limit, request.filter)?;
            }
            Ok(())
        })?;
        Ok(CurrentStream {
            continuation: Some(continuation),
            transaction,
            retrieve: move |cached: &CachedProducers<'call>,
                            request: &CurrentTextQuery<'call>,
                            phase,
                            filter: &Excluding<'_>,
                            ranking: &mut FastTierCache| {
                let search = self.activate_text(request.text, cached.producers())?;
                search.current_phase(
                    ranking,
                    phase == Phase::Refined,
                    request.limit,
                    filter,
                    request.ann,
                    filter.rejected.len(),
                )
            },
        })
    }
}

impl SelectedSemanticGeneration {
    /// The same lazy current-source contract, retaining the exact selected
    /// publication on both phase batches. Changes to selection paths cannot
    /// replace any vector/graph owner between phases or refill passes.
    pub fn progressive_current_text_in_transaction<'call>(
        &'call self,
        request: CurrentTextQuery<'call>,
        transaction: &'call Transaction<'_>,
    ) -> Result<
        impl FusedIterator<
            Item = Result<CurrentSemanticBatch<SelectedSemanticBatch>, CurrentSearchError>,
        > + 'call,
        CurrentSearchError,
    > {
        let continuation = prepare(request, |request, cached| {
            let search = self.activate_text(request.text, cached.producers())?;
            if let Some(policy) = request.ann {
                let _ = search.progressive_with_ann(request.limit, request.filter, policy)?;
            } else {
                let _ = search.progressive(request.limit, request.filter)?;
            }
            Ok(())
        })?;
        Ok(CurrentStream {
            continuation: Some(continuation),
            transaction,
            retrieve: move |cached: &CachedProducers<'call>,
                            request: &CurrentTextQuery<'call>,
                            phase,
                            filter: &Excluding<'_>,
                            ranking: &mut FastTierCache| {
                let search = self.activate_text(request.text, cached.producers())?;
                let batch = search.search.current_phase(
                    ranking,
                    phase == Phase::Refined,
                    request.limit,
                    filter,
                    request.ann,
                    filter.rejected.len(),
                )?;
                Ok(search.retain(batch))
            },
        })
    }
}

#[cfg(test)]
mod tests;
