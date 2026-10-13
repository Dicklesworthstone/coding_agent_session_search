//! Lifecycle tests exercise the actual continuation and its shared work account.
//! They do not stand in for the separate real FSVI/source regression fixtures.

use super::*;
use crate::search::hash_embedder::HashEmbedder;
use std::cell::Cell;
use std::time::Duration;

#[test]
fn phase_selection_is_lazy_and_requires_a_real_second_success() {
    let calls = Cell::new(0);
    let producer = || {
        calls.set(calls.get() + 1);
        Some(Ok::<_, SemanticReaderError>(calls.get()))
    };
    assert_eq!(phase_batch(std::iter::from_fn(producer), Phase::Initial).unwrap(), 1);
    assert_eq!(calls.get(), 1, "Initial must not request a quality result");
    calls.set(0);
    assert_eq!(phase_batch(std::iter::from_fn(producer), Phase::Refined).unwrap(), 2);
    assert_eq!(calls.get(), 2);
    assert!(matches!(
        phase_batch(std::iter::once(Ok(1)), Phase::Refined),
        Err(SemanticReaderError::InvalidCandidate)
    ));
}

#[test]
fn phase_selection_never_skips_an_initial_error_to_reach_quality() {
    let calls = Cell::new(0);
    let result = phase_batch(
        std::iter::from_fn(|| {
            calls.set(calls.get() + 1);
            Some(Err::<(), _>(SemanticReaderError::InvalidCandidate))
        }),
        Phase::Refined,
    );
    assert!(matches!(result, Err(SemanticReaderError::InvalidCandidate)));
    assert_eq!(calls.get(), 1);
}

fn continuation(model: &HashEmbedder) -> Continuation<'_> {
    let request = CurrentTextQuery::new(
        "authentication middleware",
        TextQueryProducers::Progressive { fast: model, quality: model },
        1,
        PassageProjection::RawPassagesV2,
    );
    let work = request.admit_phases(2).unwrap();
    let cached = CachedProducers::new(request.text, request.producers);
    let state = RefillState::new(request.refill);
    Continuation { request, cached, work, state, phase: Phase::Initial, ranking: FastTierCache::default() }
}

#[test]
fn expired_continuation_is_terminal_before_any_retrieval() {
    let conn = Connection::open(":memory:").unwrap();
    let mut transaction = conn.transaction().unwrap();
    let model = HashEmbedder::new(32);
    let calls = Cell::new(0);
    let mut current = continuation(&model);
    current.work.budget.timeout = Duration::ZERO;
    let mut stream = CurrentStream {
        continuation: Some(current),
        transaction: &transaction,
        retrieve: |_: &CachedProducers<'_>, _: &CurrentTextQuery<'_>, _: Phase, _: &Excluding<'_>, _: &mut FastTierCache|
            -> SemanticReaderResult<SemanticSearchBatch> {
            calls.set(calls.get() + 1);
            Err(SemanticReaderError::InvalidCandidate)
        },
    };
    assert!(matches!(
        stream.next(),
        Some(Err(CurrentSearchError::Hydration(HydrationError::DeadlineExceeded)))
    ));
    assert!(stream.next().is_none());
    assert_eq!(stream.size_hint(), (0, Some(0)));
    assert!(stream.continuation.is_none());
    assert_eq!(calls.get(), 0);
    drop(stream);
    transaction.rollback().unwrap();
}

#[test]
fn a_caught_retrieval_panic_cannot_resume_the_owned_continuation() {
    let conn = Connection::open(":memory:").unwrap();
    let mut transaction = conn.transaction().unwrap();
    let model = HashEmbedder::new(32);
    let calls = Cell::new(0);
    let mut stream = CurrentStream {
        continuation: Some(continuation(&model)),
        transaction: &transaction,
        retrieve: |_: &CachedProducers<'_>, _: &CurrentTextQuery<'_>, _: Phase, _: &Excluding<'_>, _: &mut FastTierCache|
            -> SemanticReaderResult<SemanticSearchBatch> {
            calls.set(calls.get() + 1);
            panic!("injected retrieval panic");
        },
    };
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| stream.next())).is_err());
    assert!(stream.continuation.is_none(), "cached work must be released on unwinding");
    assert!(stream.next().is_none());
    assert_eq!(stream.size_hint(), (0, Some(0)));
    assert_eq!(calls.get(), 1);
    drop(stream);
    transaction.rollback().unwrap();
}
