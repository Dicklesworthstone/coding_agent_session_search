//! Current-source progressive delivery over one retained generation and snapshot.
//!
//! Each phase completes bounded stale-hit refill before exposing text. Refinement
//! recomputes both filtered tier rankings so new quality-side exclusions cannot
//! leave stale fast ranks in the fused result. Query inference is request-cached;
//! no model is run twice merely because retrieval needs another pass.

use std::iter::FusedIterator;

use super::*;

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
        &dyn SearchFilter,
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
        let result = refill_accounted(
            request,
            self.transaction,
            &mut current.work,
            &mut current.state,
            |filter| (self.retrieve)(cached, request, phase, filter),
        );
        if result.is_ok() && current.phase == Phase::Initial {
            current.phase = Phase::Refined;
            self.continuation = Some(current);
        }
        // After Refined, cached embeddings and exclusions are released now.
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
        &dyn SearchFilter,
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
    Ok(Continuation { request, cached, work, state, phase: Phase::Initial })
}

// Obtain genuine phase batches from the existing lazy engine. Never relabel a
// one-tier or Initial result as a successful refinement. For a refill of Refined,
// recompute fast under the new exclusions before independent quality retrieval.
fn phase_batch<B>(
    mut phases: impl Iterator<Item = SemanticReaderResult<B>>,
    phase: Phase,
) -> SemanticReaderResult<B> {
    let initial = phases.next().ok_or(SemanticReaderError::InvalidCandidate)??;
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
        impl FusedIterator<Item = Result<CurrentSemanticBatch<SemanticSearchBatch>, CurrentSearchError>> + 'call,
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
            retrieve: move |cached: &CachedProducers<'call>, request: &CurrentTextQuery<'call>, phase, filter: &dyn SearchFilter| {
                let search = self.activate_text(request.text, cached.producers())?;
                if let Some(policy) = request.ann {
                    phase_batch(search.progressive_with_ann(request.limit, Some(filter), policy)?, phase)
                } else {
                    phase_batch(search.progressive(request.limit, Some(filter))?, phase)
                }
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
        impl FusedIterator<Item = Result<CurrentSemanticBatch<SelectedSemanticBatch>, CurrentSearchError>> + 'call,
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
            retrieve: move |cached: &CachedProducers<'call>, request: &CurrentTextQuery<'call>, phase, filter: &dyn SearchFilter| {
                let search = self.activate_text(request.text, cached.producers())?;
                if let Some(policy) = request.ann {
                    phase_batch(search.progressive_with_ann(request.limit, Some(filter), policy)?, phase)
                } else {
                    phase_batch(search.progressive(request.limit, Some(filter))?, phase)
                }
            },
        })
    }
}

#[cfg(test)]
mod tests;
