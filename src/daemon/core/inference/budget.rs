//! Request-owned limits for foreground daemon inference. These bounds cover
//! admitted inputs and individual model calls, not the native model's total RSS.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::daemon::protocol::{ErrorCode, ErrorResponse};

pub(super) const MAX_ITEMS: usize = 1024;
pub(super) const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
pub(super) const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_BATCH_ITEMS: usize = 16;
const MAX_PADDED_CHARS: usize = 16 * 1024;

pub(super) fn error(code: ErrorCode, message: &str, retryable: bool) -> ErrorResponse {
    ErrorResponse {
        code,
        message: message.to_owned(),
        retryable,
        retry_after_ms: retryable.then_some(1000),
    }
}

/// One foreground request owns tokenization/inference at a time. A busy model
/// refuses admission instead of accumulating threads waiting on its mutex.
/// The native backend remains free to parallelize within an admitted batch.
#[derive(Default)]
pub(in crate::daemon::core) struct InferenceGate(AtomicBool);

pub(in crate::daemon::core) struct Permit<'a>(&'a AtomicBool);

impl InferenceGate {
    pub(in crate::daemon::core) fn try_enter(&self) -> Result<Permit<'_>, ErrorResponse> {
        self.0
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map(|_| Permit(&self.0))
            .map_err(|_| {
                error(
                    ErrorCode::Overloaded,
                    "foreground inference is busy; retry later",
                    true,
                )
            })
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(super) struct Budget<'a> {
    start: Instant,
    timeout: Duration,
    shutdown: &'a AtomicBool,
}

impl<'a> Budget<'a> {
    pub(super) fn new(timeout: Duration, shutdown: &'a AtomicBool) -> Self {
        Self {
            start: Instant::now(),
            timeout,
            shutdown,
        }
    }

    pub(super) fn check(&self) -> Result<(), ErrorResponse> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(error(
                ErrorCode::Overloaded,
                "daemon is shutting down",
                true,
            ));
        }
        if self.start.elapsed() >= self.timeout {
            return Err(error(
                ErrorCode::Timeout,
                "daemon inference deadline exceeded",
                true,
            ));
        }
        Ok(())
    }

    pub(super) fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
}

/// Validate the complete request before any model call. Reranking accounts for
/// the query in every padded pair. Large individual texts are not silently
/// shortened: they either fit the request limit or are explicitly rejected.
pub(super) fn plan(
    texts: &[String],
    query: Option<&str>,
    budget: &Budget<'_>,
) -> Result<Vec<Range<usize>>, ErrorResponse> {
    budget.check()?;
    if texts.len() > MAX_ITEMS {
        return Err(error(
            ErrorCode::InvalidInput,
            "inference request exceeds 1024 items",
            false,
        ));
    }
    let mut total_bytes = 0usize;
    let mut query_chars = 0usize;
    if let Some(query) = query {
        validate_text(query)?;
        total_bytes = query.len();
        query_chars = query.chars().count();
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut longest = 0usize;
    for (index, text) in texts.iter().enumerate() {
        budget.check()?;
        validate_text(text)?;
        total_bytes = total_bytes.checked_add(text.len()).ok_or_else(|| {
            error(
                ErrorCode::InvalidInput,
                "inference input size overflow",
                false,
            )
        })?;
        if total_bytes > MAX_INPUT_BYTES {
            return Err(error(
                ErrorCode::InvalidInput,
                "inference request exceeds 4 MiB of input text",
                false,
            ));
        }
        let chars = text.chars().count().saturating_add(query_chars).max(1);
        let next_longest = longest.max(chars);
        let rows = index - start + 1;
        if index > start
            && (rows > MAX_BATCH_ITEMS || next_longest.saturating_mul(rows) > MAX_PADDED_CHARS)
        {
            ranges.push(start..index);
            start = index;
            longest = chars;
        } else {
            longest = next_longest;
        }
    }
    if start < texts.len() {
        ranges.push(start..texts.len());
    }
    budget.check()?;
    Ok(ranges)
}

fn validate_text(text: &str) -> Result<(), ErrorResponse> {
    if text.is_empty() {
        return Err(error(
            ErrorCode::InvalidInput,
            "inference input contains an empty text",
            false,
        ));
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(error(
            ErrorCode::InvalidInput,
            "inference text exceeds 64 KiB",
            false,
        ));
    }
    Ok(())
}

/// Keep the entire result private until every batch succeeds. Checks after the
/// last callback are essential: shutdown during the final native call must not
/// turn a cancelled request into successful output or a signed attestation.
pub(super) fn collect_batches<T>(
    texts: &[String],
    ranges: &[Range<usize>],
    budget: &Budget<'_>,
    mut infer: impl FnMut(&[String]) -> Result<Vec<T>, ErrorResponse>,
) -> Result<Vec<T>, ErrorResponse> {
    let mut result = Vec::with_capacity(texts.len());
    for range in ranges {
        budget.check()?;
        let batch = infer(&texts[range.clone()])?;
        budget.check()?;
        if batch.len() != range.len() {
            return Err(error(
                ErrorCode::Internal,
                "native inference returned an incomplete or oversized batch",
                false,
            ));
        }
        result.extend(batch);
    }
    budget.check()?;
    if result.len() != texts.len() {
        return Err(error(
            ErrorCode::Internal,
            "native inference returned an inconsistent result count",
            false,
        ));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_refuses_overlap_and_releases_on_return_and_unwind() {
        let gate = InferenceGate::default();
        let permit = gate.try_enter().unwrap();
        assert_eq!(gate.try_enter().err().unwrap().code, ErrorCode::Overloaded);
        drop(permit);
        let failure = std::panic::catch_unwind(|| {
            let _permit = gate.try_enter().unwrap();
            panic!("synthetic inference failure");
        });
        assert!(failure.is_err());
        assert!(gate.try_enter().is_ok());
    }

    #[test]
    fn concurrent_admission_has_exactly_one_owner() {
        let gate = InferenceGate::default();
        let barrier = std::sync::Barrier::new(9);
        let admitted = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    barrier.wait();
                    let permit = gate.try_enter();
                    if permit.is_ok() {
                        admitted.fetch_add(1, Ordering::SeqCst);
                    }
                    // Hold the winner until all contenders have attempted.
                    barrier.wait();
                    drop(permit);
                });
            }
            barrier.wait();
            barrier.wait();
        });
        assert_eq!(admitted.load(Ordering::SeqCst), 1);
        assert!(gate.try_enter().is_ok());
    }

    #[test]
    fn planner_bounds_rows_padded_text_and_rerank_query_pairs() {
        let shutdown = AtomicBool::new(false);
        let budget = Budget::new(Duration::from_secs(30), &shutdown);
        for query in [None, Some("a long query ")] {
            let texts = (0..101)
                .map(|index| "é".repeat(if index % 7 == 0 { 12000 } else { 600 }))
                .collect::<Vec<_>>();
            let ranges = plan(&texts, query, &budget).unwrap();
            assert_eq!(
                ranges
                    .iter()
                    .flat_map(|range| range.clone())
                    .collect::<Vec<_>>(),
                (0..101).collect::<Vec<_>>()
            );
            for range in ranges {
                let longest = texts[range.clone()]
                    .iter()
                    .map(|text| text.chars().count() + query.map_or(0, |q| q.chars().count()))
                    .max()
                    .unwrap();
                assert!(range.len() <= MAX_BATCH_ITEMS);
                assert!(range.len() == 1 || range.len() * longest <= MAX_PADDED_CHARS);
            }
        }
        let texts = vec!["x".into(); 33];
        assert_eq!(
            plan(&texts, None, &budget).unwrap(),
            [0..16, 16..32, 32..33]
        );
        let texts = vec!["x".into(); 3];
        assert_eq!(
            plan(&texts, Some(&"q".repeat(10000)), &budget).unwrap(),
            [0..1, 1..2, 2..3]
        );
    }

    #[test]
    fn invalid_final_input_is_rejected_before_any_inference() {
        let shutdown = AtomicBool::new(false);
        let budget = Budget::new(Duration::from_secs(30), &shutdown);
        for texts in [
            vec!["x".into(); MAX_ITEMS + 1],
            vec!["a".repeat(MAX_TEXT_BYTES + 1)],
            vec!["a".repeat(MAX_TEXT_BYTES); 65],
            vec!["valid".into(), String::new()],
        ] {
            assert_eq!(
                plan(&texts, None, &budget).unwrap_err().code,
                ErrorCode::InvalidInput
            );
        }
        assert!(plan(&vec!["a".repeat(MAX_TEXT_BYTES); 64], None, &budget).is_ok());
        assert!(plan(&[], None, &budget).unwrap().is_empty());
        assert_eq!(
            plan(&[], Some(""), &budget).unwrap_err().code,
            ErrorCode::InvalidInput
        );
    }

    #[test]
    fn batches_preserve_order_and_reject_partial_backend_success() {
        let shutdown = AtomicBool::new(false);
        let budget = Budget::new(Duration::from_secs(30), &shutdown);
        let texts = (0..37).map(|index| index.to_string()).collect::<Vec<_>>();
        let ranges = plan(&texts, None, &budget).unwrap();
        assert_eq!(
            collect_batches(&texts, &ranges, &budget, |batch| Ok(batch.to_vec())).unwrap(),
            texts
        );
        for delta in [-1isize, 1] {
            let mut calls = 0;
            let result = collect_batches(&texts, &ranges, &budget, |batch| {
                calls += 1;
                Ok(vec![0; (batch.len() as isize + delta) as usize])
            });
            assert_eq!(result.unwrap_err().code, ErrorCode::Internal);
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn failure_is_not_retried_or_returned_as_a_partial_vector_set() {
        let shutdown = AtomicBool::new(false);
        let budget = Budget::new(Duration::from_secs(30), &shutdown);
        let texts = vec!["x".into(); 48];
        let ranges = plan(&texts, None, &budget).unwrap();
        let mut calls = 0;
        let result = collect_batches(&texts, &ranges, &budget, |batch| {
            calls += 1;
            if calls == 2 {
                return Err(error(ErrorCode::Internal, "synthetic failure", false));
            }
            Ok(batch.to_vec())
        });
        assert_eq!(result.unwrap_err().message, "synthetic failure");
        assert_eq!(calls, 2);
    }

    #[test]
    fn shutdown_during_final_batch_and_expired_budget_discard_results() {
        let shutdown = AtomicBool::new(false);
        let budget = Budget::new(Duration::from_secs(30), &shutdown);
        let texts = vec!["last".into()];
        let ranges = plan(&texts, None, &budget).unwrap();
        let result = collect_batches(&texts, &ranges, &budget, |batch| {
            shutdown.store(true, Ordering::Release);
            Ok(batch.to_vec())
        });
        assert_eq!(result.unwrap_err().code, ErrorCode::Overloaded);
        shutdown.store(false, Ordering::Release);
        let expired = Budget::new(Duration::ZERO, &shutdown);
        assert_eq!(
            plan(&texts, None, &expired).unwrap_err().code,
            ErrorCode::Timeout
        );
        let result = collect_batches::<String>(&texts, &ranges, &expired, |_| {
            panic!("expired request reached backend")
        });
        assert_eq!(result.unwrap_err().code, ErrorCode::Timeout);
    }
}
