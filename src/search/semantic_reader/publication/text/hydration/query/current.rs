//! Current-source retrieval from retained generations, with bounded stale-hit refill.
//!
//! Only missing or changed canonical passages are exclusions. Invalid owners,
//! malformed source rows, database failures and exhausted budgets remain errors.
//! Every pass searches the original owners with the caller's original filter AND
//! accumulated passage exclusions. A failed pass never exposes partial text.

mod cache;
mod progressive;

use std::collections::BTreeSet;

use super::*;
use crate::search::semantic_reader::SemanticGenerationReader;
use crate::search::semantic_reader::publication::SelectedSemanticGeneration;
use crate::search::semantic_reader::text::TextQueryProducers;
use cache::CachedProducers;

/// Bounds additional work, not just the size of the final page. Hydration's
/// passage, byte and deadline accounts are also shared across all search passes.
#[derive(Debug, Clone, Copy)]
pub struct RefillBudget {
    pub max_searches: usize,
    pub max_rejected_passages: usize,
    pub max_exclusion_bytes: usize,
}

impl Default for RefillBudget {
    fn default() -> Self {
        Self {
            max_searches: 8,
            max_rejected_passages: 1_024,
            max_exclusion_bytes: 256 * 1_024,
        }
    }
}

impl RefillBudget {
    fn validate(self) -> Result<(), CurrentSearchError> {
        if !(1..=64).contains(&self.max_searches)
            || self.max_rejected_passages > 4_096
            || self.max_exclusion_bytes > 2 * 1_024 * 1_024
        {
            return Err(CurrentSearchError::InvalidRefillBudget);
        }
        Ok(())
    }
}

/// Explicit request policy. Projection must be the selected document contract,
/// not the newest writer's contract. Keep the transaction and filter stable.
/// No setting relaxes publication, producer, or source-fingerprint admission.
pub struct CurrentTextQuery<'request> {
    pub text: &'request str,
    pub producers: TextQueryProducers<'request>,
    pub limit: usize,
    pub filter: Option<&'request dyn SearchFilter>,
    pub projection: PassageProjection,
    pub hydration: HydrationBudget,
    pub refill: RefillBudget,
    pub ann: Option<AnnSearchPolicy>,
}

impl<'request> CurrentTextQuery<'request> {
    pub fn new(
        text: &'request str,
        producers: TextQueryProducers<'request>,
        limit: usize,
        projection: PassageProjection,
    ) -> Self {
        Self {
            text,
            producers,
            limit,
            filter: None,
            projection,
            hydration: HydrationBudget::default(),
            refill: RefillBudget::default(),
            ann: None,
        }
    }

    fn admit(&self) -> Result<Work, CurrentSearchError> {
        self.admit_phases(1)
    }

    fn admit_phases(&self, phases: usize) -> Result<Work, CurrentSearchError> {
        let started = Instant::now();
        self.refill.validate()?;
        if self.refill.max_searches < phases {
            return Err(CurrentSearchError::InvalidRefillBudget);
        }
        if let Some(policy) = self.ann {
            policy.validate()?;
        }
        let requested = self.limit.checked_mul(phases)
            .ok_or(HydrationError::BudgetExceeded)?;
        self.hydration.validate(requested)?;
        Ok(Work::new(self.hydration, started)?)
    }
}

impl std::fmt::Debug for CurrentTextQuery<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CurrentTextQuery")
            .field("limit", &self.limit)
            .field("projection", &self.projection)
            .field("refill", &self.refill)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CurrentSearchError {
    #[error("current semantic query has an invalid refill budget")]
    InvalidRefillBudget,
    #[error("current semantic query exhausted its refill budget; no partial page was returned")]
    RefillExhausted,
    #[error(transparent)]
    Retrieval(#[from] SemanticReaderError),
    #[error(transparent)]
    Hydration(#[from] HydrationError),
}

/// Counts describe this request, not whole-generation canonical coverage.
/// Source/output bytes include work discarded during unsuccessful refill passes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CurrentSearchStats {
    pub searches: usize,
    pub missing_passages: usize,
    pub stale_passages: usize,
    pub exclusion_bytes: usize,
    pub source_bytes_read: usize,
    pub output_bytes_checked: usize,
}

/// The final, fully verified filtered batch, including original serving owners
/// and selected publication identity when B is SelectedSemanticBatch.
#[derive(Clone)]
pub struct CurrentSemanticBatch<B> {
    verified: HydratedSemanticBatch<B>,
    stats: CurrentSearchStats,
}

impl<B> CurrentSemanticBatch<B> {
    pub fn verified(&self) -> &HydratedSemanticBatch<B> {
        &self.verified
    }

    pub fn stats(&self) -> &CurrentSearchStats {
        &self.stats
    }
}

impl<B> std::fmt::Debug for CurrentSemanticBatch<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CurrentSemanticBatch")
            .field("verified", &self.verified)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

struct Excluding<'filter> {
    original: Option<&'filter dyn SearchFilter>,
    rejected: &'filter BTreeSet<String>,
}

impl SearchFilter for Excluding<'_> {
    fn matches(&self, doc_id: &str, metadata: Option<&serde_json::Value>) -> bool {
        !self.rejected.contains(doc_id)
            && self.original.is_none_or(|filter| filter.matches(doc_id, metadata))
    }
}

struct RefillState {
    rejected: BTreeSet<String>,
    stats: CurrentSearchStats,
    budget: RefillBudget,
}

impl RefillState {
    fn new(budget: RefillBudget) -> Self {
        Self { rejected: BTreeSet::new(), stats: CurrentSearchStats::default(), budget }
    }

    fn reject(&mut self, doc_id: &str, missing: bool) -> Result<(), CurrentSearchError> {
        // No-progress is an engine contract failure, not another retry allowance.
        if self.rejected.contains(doc_id) {
            return Err(HydrationError::InvalidCandidate.into());
        }
        let bytes = self.stats.exclusion_bytes.checked_add(doc_id.len())
            .filter(|&bytes| bytes <= self.budget.max_exclusion_bytes)
            .ok_or(CurrentSearchError::RefillExhausted)?;
        if self.rejected.len() >= self.budget.max_rejected_passages {
            return Err(CurrentSearchError::RefillExhausted);
        }
        self.rejected.insert(doc_id.to_owned());
        self.stats.exclusion_bytes = bytes;
        if missing {
            self.stats.missing_passages += 1;
        } else {
            self.stats.stale_passages += 1;
        }
        Ok(())
    }
}

fn verify_pass(
    batch: &SemanticSearchBatch,
    transaction: &Transaction<'_>,
    projection: PassageProjection,
    work: &mut Work,
    state: &mut RefillState,
) -> Result<Option<Vec<HydratedPassage>>, CurrentSearchError> {
    work.check()?;
    work.passages(batch.hits().len())?;
    let before = state.rejected.len();
    let mut by_message = BTreeMap::<u64, Vec<usize>>::new();
    for (rank, hit) in batch.hits().iter().enumerate() {
        work.check()?;
        validate_candidate(batch, hit)?;
        if state.rejected.contains(&hit.doc_id) {
            return Err(HydrationError::InvalidCandidate.into());
        }
        by_message.entry(hit.document.message_id).or_default().push(rank);
    }
    let mut verified = vec![None; batch.hits().len()];
    for (message_id, ranks) in by_message {
        work.check()?;
        let source = match read_message(transaction, message_id, work) {
            Ok(source) => source,
            Err(HydrationError::MissingSource) => {
                for rank in ranks {
                    state.reject(&batch.hits()[rank].doc_id, true)?;
                }
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let mut location = None;
        for rank in ranks {
            work.check()?;
            let hit = &batch.hits()[rank];
            let canonical_text = match verify_message(&hit.document, &source, projection) {
                Ok(text) => text,
                Err(HydrationError::StaleSource) => {
                    state.reject(&hit.doc_id, false)?;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            work.check()?;
            if location.is_none() {
                let value = source.location();
                work.output(value.source_id.len().saturating_add(value.source_path.len()))?;
                location = Some(Arc::new(value));
            }
            work.output(canonical_text.len())?;
            verified[rank] = Some(HydratedPassage {
                hit: hit.clone(),
                canonical_text,
                location: Arc::clone(location.as_ref().ok_or(HydrationError::InvalidSource)?),
            });
        }
    }
    work.check()?;
    if state.rejected.len() != before {
        return Ok(None);
    }
    Ok(Some(verified.into_iter()
        .map(|hit| hit.ok_or(HydrationError::InvalidCandidate))
        .collect::<Result<_, _>>()?))
}

fn refill<B: BatchSource>(
    request: &CurrentTextQuery<'_>,
    transaction: &Transaction<'_>,
    mut work: Work,
    retrieve: impl Fn(&dyn SearchFilter) -> SemanticReaderResult<B>,
) -> Result<CurrentSemanticBatch<B>, CurrentSearchError> {
    let mut state = RefillState::new(request.refill);
    refill_accounted(request, transaction, &mut work, &mut state, retrieve)
}

// Synchronous and progressive retrieval use the same candidate/source boundary.
// A progressive continuation carries this account AND its exclusions forward;
// neither a new phase nor successful source verification renews any allowance.
fn refill_accounted<B: BatchSource>(
    request: &CurrentTextQuery<'_>,
    transaction: &Transaction<'_>,
    work: &mut Work,
    state: &mut RefillState,
    retrieve: impl Fn(&dyn SearchFilter) -> SemanticReaderResult<B>,
) -> Result<CurrentSemanticBatch<B>, CurrentSearchError> {
    loop {
        work.check()?;
        // Admit each complete requested page before native work. Never shrink k
        // to remaining fuel and misrepresent the resulting short page as complete.
        work.budget.validate(request.limit)?;
        if state.stats.searches >= state.budget.max_searches {
            return Err(CurrentSearchError::RefillExhausted);
        }
        state.stats.searches += 1;
        let batch = retrieve(&Excluding {
            original: request.filter, rejected: &state.rejected,
        })?;
        if batch.batch().hits().len() > request.limit {
            return Err(HydrationError::InvalidCandidate.into());
        }
        if let Some(passages) = verify_pass(
            batch.batch(), transaction, request.projection, work, state,
        )? {
            state.stats.source_bytes_read = work.source_bytes;
            state.stats.output_bytes_checked = work.output_bytes;
            return Ok(CurrentSemanticBatch {
                verified: HydratedSemanticBatch {
                    source: batch, passages, projection: request.projection,
                },
                stats: state.stats,
            });
        }
        // Both provisional text and native candidates are dropped before refill.
        // The exclusion set is the only per-passage state carried to another pass.
    }
}

impl SemanticGenerationReader {
    /// Search retained owners against current source rows in one caller-owned
    /// snapshot. Missing/changed passages are filtered BEFORE the next top-k.
    /// Each selected producer performs at most one query inference per request.
    /// Budgets and the deadline span activation, inference and every refill pass.
    /// No transaction, index, current pointer or producer is replaced here.
    pub fn search_current_text_in_transaction(
        &self,
        request: CurrentTextQuery<'_>,
        transaction: &Transaction<'_>,
    ) -> Result<CurrentSemanticBatch<SemanticSearchBatch>, CurrentSearchError> {
        let work = request.admit()?;
        let cached = CachedProducers::new(request.text, request.producers);
        let search = self.activate_text(request.text, cached.producers())?;
        refill(&request, transaction, work, |filter| match request.ann {
            Some(policy) => search.search_with_ann(request.limit, Some(filter), policy),
            None => search.search(request.limit, Some(filter)),
        })
    }
}

impl SelectedSemanticGeneration {
    /// As above, retaining the exact selected publication on the verified page.
    /// Changes to current.json cannot select different owners between passes.
    pub fn search_current_text_in_transaction(
        &self,
        request: CurrentTextQuery<'_>,
        transaction: &Transaction<'_>,
    ) -> Result<CurrentSemanticBatch<SelectedSemanticBatch>, CurrentSearchError> {
        let work = request.admit()?;
        let cached = CachedProducers::new(request.text, request.producers);
        let search = self.activate_text(request.text, cached.producers())?;
        refill(&request, transaction, work, |filter| match request.ann {
            Some(policy) => search.search_with_ann(request.limit, Some(filter), policy),
            None => search.search(request.limit, Some(filter)),
        })
    }
}
