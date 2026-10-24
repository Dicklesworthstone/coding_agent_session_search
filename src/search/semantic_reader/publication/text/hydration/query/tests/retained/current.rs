//! Real canonical rows and real FSVI v2 files. Hash is an explicit control,
//! not native-model, ANN-acceleration, or archive-scale performance evidence.

use super::*;
use crate::search::semantic_reader::publication::text::hydration::query::current::{
    CurrentSearchError, CurrentTextQuery, RefillBudget,
};
use frankensearch::ModelCategory;
use frankensearch::core::{EmbeddingIdentityBundleV1, IdentityBoundEmbedding, SearchError};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const TEXTS: &[(u64, &str)] = &[
    (7, "authentication middleware"),
    (8, "authentication middleware routing"),
    (9, "middleware handles authentication credentials"),
    (10, "database transactions and durable storage"),
    (11, "rendering and terminal keyboard handling"),
];

struct BoundOnlyHash {
    inner: HashEmbedder,
    calls: AtomicUsize,
    foreign_response: AtomicBool,
    changed: EmbeddingIdentityBundleV1,
}

impl BoundOnlyHash {
    fn new() -> TestResult<Self> {
        let inner = HashEmbedder::new(32);
        let mut changed = inner.identity()?.clone();
        changed
            .producer
            .implementation_revision
            .push_str("-wrong-response");
        changed.validate()?;
        Ok(Self {
            inner,
            calls: AtomicUsize::new(0),
            foreign_response: AtomicBool::new(false),
            changed,
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Embedder for BoundOnlyHash {
    fn embed_sync(&self, _: &str) -> Result<Vec<f32>, SearchError> {
        panic!("the real identity-bearing override must be called, not raw vector stamping")
    }

    fn embed_bound_sync(&self, text: &str) -> Result<IdentityBoundEmbedding, SearchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut bound = self.inner.embed_bound_sync(text)?;
        if self.foreign_response.load(Ordering::SeqCst) {
            bound.identity = self.changed.clone();
        }
        Ok(bound)
    }

    fn identity(&self) -> Result<&EmbeddingIdentityBundleV1, SearchError> {
        self.inner.identity()
    }
    fn dimension(&self) -> usize {
        self.inner.dimension()
    }
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn is_semantic(&self) -> bool {
        false
    }
    fn category(&self) -> ModelCategory {
        ModelCategory::HashEmbedder
    }
}

fn request(model: &dyn Embedder, k: usize) -> CurrentTextQuery<'_> {
    CurrentTextQuery::new(
        "authentication middleware",
        TextQueryProducers::Fast(model),
        k,
        PassageProjection::RawPassagesV2,
    )
}

struct Allowed(BTreeSet<String>);
impl SearchFilter for Allowed {
    fn matches(&self, id: &str, _: Option<&serde_json::Value>) -> bool {
        self.0.contains(id)
    }
    fn name(&self) -> &str {
        "allowed"
    }
}

#[test]
fn missing_and_rewritten_winners_refill_exact_top_k_without_reembedding() -> TestResult {
    let (directory, conn, batch) = fixture(TEXTS)?;
    let before = fs::read(directory.path().join("passages.fsvi"))?;
    let removed = batch.hits()[0].document.message_id;
    let changed = batch.hits()[1].document.message_id;
    conn.execute_with_params(
        "DELETE FROM messages WHERE id = ?1",
        &[SqliteValue::Integer(removed as i64)],
    )?;
    conn.execute_with_params(
        "UPDATE messages SET content = 'changed private source' WHERE id = ?1",
        &[SqliteValue::Integer(changed as i64)],
    )?;
    let allowed = Allowed(
        batch.hits()[2..]
            .iter()
            .map(|hit| hit.doc_id.clone())
            .collect(),
    );
    let model = BoundOnlyHash::new()?;
    let expected = batch
        .reader
        .activate_text(
            "authentication middleware",
            TextQueryProducers::Fast(&model),
        )?
        .search(2, Some(&allowed))?;
    model.calls.store(0, Ordering::SeqCst);
    let mut transaction = conn.transaction()?;
    let answer = batch
        .reader
        .search_current_text_in_transaction(request(&model, 2), &transaction)?;
    assert_eq!(answer.verified().source().hits(), expected.hits());
    assert_eq!(answer.verified().passages().len(), 2);
    assert_eq!(answer.stats().searches, 2);
    assert_eq!(
        (
            answer.stats().missing_passages,
            answer.stats().stale_passages
        ),
        (1, 1)
    );
    assert_eq!(model.calls(), 1);
    assert!(
        answer
            .verified()
            .passages()
            .iter()
            .all(|passage| ![removed, changed].contains(&passage.hit().document.message_id))
    );
    transaction.query("SELECT 1")?;
    transaction.rollback()?;
    assert_eq!(before, fs::read(directory.path().join("passages.fsvi"))?);
    Ok(())
}

#[test]
fn refill_keeps_original_scope_and_never_substitutes_an_out_of_scope_hit() -> TestResult {
    let (_directory, conn, batch) = fixture(TEXTS)?;
    let scope = Allowed(
        batch.hits()[..3]
            .iter()
            .map(|hit| hit.doc_id.clone())
            .collect(),
    );
    let removed = batch.hits()[0].document.message_id;
    conn.execute_with_params(
        "DELETE FROM messages WHERE id = ?1",
        &[SqliteValue::Integer(removed as i64)],
    )?;
    let model = BoundOnlyHash::new()?;
    let mut query = request(&model, 2);
    query.filter = Some(&scope);
    let mut transaction = conn.transaction()?;
    let answer = batch
        .reader
        .search_current_text_in_transaction(query, &transaction)?;
    assert_eq!(
        answer
            .verified()
            .source()
            .hits()
            .iter()
            .map(|hit| (&hit.doc_id, hit.ranking_score.to_bits()))
            .collect::<Vec<_>>(),
        batch.hits()[1..3]
            .iter()
            .map(|hit| (&hit.doc_id, hit.ranking_score.to_bits()))
            .collect::<Vec<_>>()
    );
    assert_eq!(model.calls(), 1);
    assert_eq!(answer.stats().searches, 2);
    transaction.rollback()?;
    Ok(())
}

#[test]
fn all_missing_is_empty_only_after_an_actual_filtered_search() -> TestResult {
    let (_directory, conn, batch) = fixture(TEXTS)?;
    conn.execute("DELETE FROM messages")?;
    let model = BoundOnlyHash::new()?;
    let mut transaction = conn.transaction()?;
    let answer = batch
        .reader
        .search_current_text_in_transaction(request(&model, 2), &transaction)?;
    assert!(answer.verified().passages().is_empty());
    assert!(answer.verified().source().hits().is_empty());
    assert_eq!(answer.stats().searches, 4);
    assert_eq!(answer.stats().missing_passages, TEXTS.len());
    assert_eq!(model.calls(), 1);
    transaction.rollback()?;
    Ok(())
}

#[test]
fn search_exclusion_and_candidate_budgets_never_return_a_false_empty_page() -> TestResult {
    let (_directory, conn, batch) = fixture(TEXTS)?;
    conn.execute("DELETE FROM messages")?;
    let model = BoundOnlyHash::new()?;
    let mut transaction = conn.transaction()?;
    for refill in [
        RefillBudget {
            max_searches: 1,
            ..Default::default()
        },
        RefillBudget {
            max_rejected_passages: 0,
            ..Default::default()
        },
        RefillBudget {
            max_exclusion_bytes: 1,
            ..Default::default()
        },
    ] {
        let mut query = request(&model, 2);
        query.refill = refill;
        assert!(matches!(
            batch
                .reader
                .search_current_text_in_transaction(query, &transaction),
            Err(CurrentSearchError::RefillExhausted)
        ));
        transaction.query("SELECT 1")?;
    }
    let mut query = request(&model, 2);
    query.hydration.max_passages = 2;
    assert!(matches!(
        batch
            .reader
            .search_current_text_in_transaction(query, &transaction),
        Err(CurrentSearchError::Hydration(
            HydrationError::BudgetExceeded
        ))
    ));
    transaction.rollback()?;
    Ok(())
}

#[test]
fn invalid_limits_zero_deadlines_and_ann_policy_refuse_before_inference() -> TestResult {
    let (_directory, conn, batch) = fixture(TEXTS)?;
    let model = BoundOnlyHash::new()?;
    let mut transaction = conn.transaction()?;
    let mut invalid = request(&model, 2);
    invalid.refill.max_searches = 0;
    assert!(matches!(
        batch
            .reader
            .search_current_text_in_transaction(invalid, &transaction),
        Err(CurrentSearchError::InvalidRefillBudget)
    ));
    let mut expired = request(&model, 2);
    expired.hydration.timeout = Duration::ZERO;
    assert!(matches!(
        batch
            .reader
            .search_current_text_in_transaction(expired, &transaction),
        Err(CurrentSearchError::Hydration(
            HydrationError::DeadlineExceeded
        ))
    ));
    assert!(matches!(
        batch
            .reader
            .search_current_text_in_transaction(request(&model, usize::MAX), &transaction),
        Err(CurrentSearchError::Hydration(
            HydrationError::BudgetExceeded
        ))
    ));
    let mut invalid_ann = request(&model, 2);
    invalid_ann.ann = Some(AnnSearchPolicy {
        initial_candidates: 2,
        max_candidates: 1,
    });
    assert!(matches!(
        batch
            .reader
            .search_current_text_in_transaction(invalid_ann, &transaction),
        Err(CurrentSearchError::Retrieval(_))
    ));
    assert_eq!(model.calls(), 0);
    let answer = batch
        .reader
        .search_current_text_in_transaction(request(&model, 0), &transaction)?;
    assert!(answer.verified().passages().is_empty());
    assert_eq!(model.calls(), 0);
    transaction.rollback()?;
    Ok(())
}

#[test]
fn oversized_or_malformed_source_is_not_misclassified_as_a_stale_candidate() -> TestResult {
    let (_directory, conn, batch) = fixture(&TEXTS[..1])?;
    let model = BoundOnlyHash::new()?;
    let mut transaction = conn.transaction()?;
    let mut query = request(&model, 1);
    query.hydration.max_message_bytes = 1;
    assert!(matches!(
        batch
            .reader
            .search_current_text_in_transaction(query, &transaction),
        Err(CurrentSearchError::Hydration(
            HydrationError::BudgetExceeded
        ))
    ));
    transaction.execute("UPDATE messages SET content = NULL")?;
    assert!(matches!(
        batch
            .reader
            .search_current_text_in_transaction(request(&model, 1), &transaction),
        Err(CurrentSearchError::Hydration(HydrationError::InvalidSource))
    ));
    transaction.rollback()?;
    Ok(())
}

#[test]
fn changed_provenance_is_excluded_without_relabelling_old_scores() -> TestResult {
    let (_directory, conn, batch) = fixture(&TEXTS[..2])?;
    conn.execute("UPDATE conversations SET source_id = 'different-host', agent_id = 99")?;
    let model = BoundOnlyHash::new()?;
    let mut transaction = conn.transaction()?;
    let answer = batch
        .reader
        .search_current_text_in_transaction(request(&model, 2), &transaction)?;
    assert!(answer.verified().passages().is_empty());
    assert_eq!(answer.stats().stale_passages, 2);
    assert_eq!(answer.stats().searches, 2);
    transaction.rollback()?;
    Ok(())
}

#[test]
fn attested_response_override_is_preserved_and_foreign_response_is_rejected() -> TestResult {
    let (_directory, conn, batch) = fixture(&TEXTS[..1])?;
    let model = BoundOnlyHash::new()?;
    model.foreign_response.store(true, Ordering::SeqCst);
    let mut transaction = conn.transaction()?;
    assert!(matches!(
        batch
            .reader
            .search_current_text_in_transaction(request(&model, 1), &transaction),
        Err(CurrentSearchError::Retrieval(_))
    ));
    assert_eq!(model.calls(), 1);
    model.foreign_response.store(false, Ordering::SeqCst);
    let answer = batch
        .reader
        .search_current_text_in_transaction(request(&model, 1), &transaction)?;
    assert_eq!(answer.verified().passages().len(), 1);
    assert_eq!(
        model.calls(),
        2,
        "a later request must not reuse an earlier response"
    );
    transaction.rollback()?;
    Ok(())
}

#[test]
fn requested_ann_keeps_exact_fallback_receipts_after_refill() -> TestResult {
    use crate::search::semantic_reader::ann::SemanticShardEngine;
    let (_directory, conn, batch) = fixture(TEXTS)?;
    conn.execute_with_params(
        "DELETE FROM messages WHERE id = ?1",
        &[SqliteValue::Integer(
            batch.hits()[0].document.message_id as i64,
        )],
    )?;
    let model = BoundOnlyHash::new()?;
    let mut query = request(&model, 2);
    query.ann = Some(AnnSearchPolicy {
        initial_candidates: 2,
        max_candidates: 4,
    });
    let mut transaction = conn.transaction()?;
    let answer = batch
        .reader
        .search_current_text_in_transaction(query, &transaction)?;
    assert_eq!(answer.stats().searches, 2);
    assert_eq!(answer.verified().passages().len(), 2);
    assert!(
        answer
            .verified()
            .source()
            .execution()
            .iter()
            .all(|report| report.engine == SemanticShardEngine::ExactFallback
                && report.ann_windows == 0)
    );
    assert_eq!(model.calls(), 1);
    transaction.rollback()?;
    Ok(())
}

#[test]
fn source_byte_account_is_cumulative_across_failed_passes() -> TestResult {
    let (_directory, conn, batch) = fixture(&TEXTS[..2])?;
    let first = batch.hits()[0].document.message_id;
    conn.execute_with_params(
        "UPDATE messages SET content = 'changed' WHERE id = ?1",
        &[SqliteValue::Integer(first as i64)],
    )?;
    let model = BoundOnlyHash::new()?;
    let mut transaction = conn.transaction()?;
    // First stale read fits; the next source must not receive a new allowance.
    let mut query = request(&model, 1);
    query.hydration.max_total_source_bytes =
        "changedassistantworkstationprivate/session.jsonl".len();
    assert!(matches!(
        batch
            .reader
            .search_current_text_in_transaction(query, &transaction),
        Err(CurrentSearchError::Hydration(
            HydrationError::BudgetExceeded
        ))
    ));
    assert_eq!(model.calls(), 1);
    transaction.rollback()?;
    Ok(())
}
