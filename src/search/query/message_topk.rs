//! Bounded, exact message selection over a chunk-ranked vector backend.
//!
//! A raw top-k window is not a top-k message window: one message can own every
//! returned chunk. Refill with retained messages excluded instead of inflating
//! a raw window until it contains the whole archive. The backend must apply the
//! supplied membership predicate BEFORE selecting its score-ordered window.

use super::VectorSearchResult;
use std::collections::{HashMap, HashSet};
use std::fmt;

/// Limits extra full-vector passes per candidate call, across shards. The caller
/// still owns its wall-clock cancellation. Exhaustion is an error, never an
/// apparently complete but incorrectly ranked exact result.
pub(super) const MAX_EXACT_MESSAGE_REFILLS: usize = 64;

/// Automatic growth stops here; an already larger caller window is not shrunk.
/// This bounds candidate storage, not vector ownership, RSS, or query duration.
const MAX_ADAPTIVE_EXACT_WINDOW: usize = 16_384;

pub(super) struct MessageTopK {
    pub(super) hits: Vec<VectorSearchResult>,
    pub(super) rounds: usize,
}

#[derive(Debug)]
pub(super) enum RefillError<E> {
    Backend(E),
    InvalidBatch(&'static str),
    BudgetExhausted,
}

impl<E: fmt::Display> fmt::Display for RefillError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(error) => error.fmt(formatter),
            Self::InvalidBatch(reason) => write!(formatter, "invalid exact semantic batch: {reason}"),
            Self::BudgetExhausted => formatter.write_str(
                "exact semantic message refinement exceeded its work budget; narrow the search or request a smaller page",
            ),
        }
    }
}

/// Select `target` distinct messages (or all remaining messages), preserving the
/// best chunk score for each message. Scores descend; tied messages sort by ID.
///
/// `fetch` receives retained message IDs to exclude, an optional EXCLUSIVE
/// message-ID ceiling, and a bounded raw-chunk window size. The ID ceiling is
/// introduced only after a full message window and a score bound prove that any
/// omitted competitor can at most tie the current last result. Discarded tied
/// messages then lie outside the decreasing ceiling, so no archive-sized seen
/// set is necessary. Score ties within one message retain the first best chunk,
/// just like the incumbent collapse operation.
///
/// Start with `window` raw candidates. Only unresolved rounds grow the next
/// request, doubling up to the larger of the initial window and the automatic
/// growth cap. Candidate memory is O(target + max(window, growth cap)), not an
/// archive-sized seen set. A short post-dedup batch is not proof of exhaustion.
/// The round cap still bounds calls, not their duration or vector-owner RSS.
pub(super) fn collect_exact_messages<E>(
    target: usize,
    mut window: usize,
    max_rounds: usize,
    mut fetch: impl FnMut(&HashSet<u64>, Option<u64>, usize) -> Result<Vec<VectorSearchResult>, E>,
) -> Result<MessageTopK, RefillError<E>> {
    if target == 0 {
        return Ok(MessageTopK {
            hits: Vec::new(),
            rounds: 0,
        });
    }
    if window == 0 {
        return Err(RefillError::InvalidBatch("zero candidate window"));
    }
    let max_window = window.max(MAX_ADAPTIVE_EXACT_WINDOW);
    let mut retained: Vec<VectorSearchResult> = Vec::new();
    let mut ceiling = None;
    for round in 0..max_rounds {
        let excluded = retained.iter().map(|hit| hit.message_id).collect();
        let batch = fetch(&excluded, ceiling, window).map_err(RefillError::Backend)?;
        if batch.len() > window {
            return Err(RefillError::InvalidBatch(
                "backend exceeded candidate limit",
            ));
        }
        for (position, hit) in batch.iter().enumerate() {
            if !hit.score.is_finite() {
                return Err(RefillError::InvalidBatch("non-finite chunk score"));
            }
            if excluded.contains(&hit.message_id)
                || ceiling.is_some_and(|ceiling| hit.message_id >= ceiling)
            {
                return Err(RefillError::InvalidBatch(
                    "backend did not enforce refill scope",
                ));
            }
            if position > 0 && batch[position - 1].score.total_cmp(&hit.score).is_lt() {
                return Err(RefillError::InvalidBatch(
                    "backend scores are not descending",
                ));
            }
        }
        // FSVI deduplicates document IDs after selecting raw records. A
        // short nonempty window can still have unseen messages behind it.
        // Only an empty filtered result proves that this scope is exhausted.
        let exhausted = batch.is_empty();
        let tail_score = batch.last().map(|hit| hit.score);
        let mut best_by_message: HashMap<_, _> = retained
            .into_iter()
            .map(|hit| (hit.message_id, hit))
            .collect();
        for hit in batch {
            best_by_message
                .entry(hit.message_id)
                .and_modify(|best| {
                    if hit.score.total_cmp(&best.score).is_gt() {
                        best.score = hit.score;
                        best.chunk_idx = hit.chunk_idx;
                    }
                })
                .or_insert(hit);
        }
        retained = best_by_message.into_values().collect();
        retained.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.message_id.cmp(&right.message_id))
        });
        retained.truncate(target);
        if exhausted {
            return Ok(MessageTopK {
                hits: retained,
                rounds: round + 1,
            });
        }
        if retained.len() == target {
            // Every omitted chunk scores <= this round's tail. Strictly lower
            // scores cannot change the message window. Equality CAN change its
            // message-ID tiebreak, even when the window is already full.
            let cutoff = retained.last().expect("nonzero full message window");
            if tail_score.is_some_and(|tail| tail.total_cmp(&cutoff.score).is_lt()) {
                return Ok(MessageTopK {
                    hits: retained,
                    rounds: round + 1,
                });
            }
            ceiling = Some(cutoff.message_id);
        }
        // Amortize whole-vector scans for chunk-heavy messages and dense ties
        // without relaxing scope, the exact ranking proof, or the call budget.
        window = window.saturating_mul(2).min(max_window);
    }
    Err(RefillError::BudgetExhausted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(message_id: u64, chunk_idx: u8, score: f32) -> VectorSearchResult {
        VectorSearchResult {
            message_id,
            chunk_idx,
            score,
        }
    }

    // Model only the documented exact backend contract: filter before top-k,
    // descending scores, and arbitrary-but-stable chunk ordering within ties.
    fn backend(
        records: &[VectorSearchResult],
        excluded: &HashSet<u64>,
        ceiling: Option<u64>,
        window: usize,
    ) -> Vec<VectorSearchResult> {
        let mut rows = records
            .iter()
            .filter(|hit| {
                !excluded.contains(&hit.message_id)
                    && ceiling.is_none_or(|ceiling| hit.message_id < ceiling)
            })
            .cloned()
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| b.score.total_cmp(&a.score));
        rows.truncate(window);
        rows
    }

    fn select(records: &[VectorSearchResult], target: usize, window: usize) -> MessageTopK {
        collect_exact_messages(target, window, 4096, |excluded, ceiling, window| {
            assert!(
                excluded.len() <= target,
                "retained IDs must not grow with archive size"
            );
            Ok::<_, &'static str>(backend(records, excluded, ceiling, window))
        })
        .unwrap_or_else(|error| panic!("{error}"))
    }

    fn ids(result: &MessageTopK) -> Vec<u64> {
        result.hits.iter().map(|hit| hit.message_id).collect()
    }

    fn oracle(records: &[VectorSearchResult], limit: usize) -> Vec<(u64, u32)> {
        let mut best = HashMap::<u64, f32>::new();
        for hit in records {
            best.entry(hit.message_id)
                .and_modify(|score| {
                    if hit.score.total_cmp(score).is_gt() {
                        *score = hit.score;
                    }
                })
                .or_insert(hit.score);
        }
        let mut values = best.into_iter().collect::<Vec<_>>();
        values.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        values.truncate(limit);
        values
            .into_iter()
            .map(|(id, score)| (id, score.to_bits()))
            .collect()
    }

    #[test]
    fn dominant_message_cannot_hide_lower_ranked_messages() {
        let mut records = (0..=255)
            .map(|chunk| hit(1, chunk, 1.0))
            .collect::<Vec<_>>();
        records.extend([hit(2, 0, 0.8), hit(3, 0, 0.6), hit(4, 0, 0.4)]);
        // Even the old second window (three times a 4x overfetch) contains
        // only message 1. This fixture is deliberately beyond both windows.
        assert!(
            records
                .iter()
                .take(3 * 4 * 3)
                .all(|hit| hit.message_id == 1)
        );
        let result = select(&records, 3, 12);
        assert_eq!(ids(&result), vec![1, 2, 3]);
        assert_eq!(result.rounds, 2);
    }

    #[test]
    fn many_dominant_messages_keep_exclusion_storage_bounded() {
        let records = (1..=100_u64)
            .flat_map(|id| (0..=255).map(move |chunk| hit(id, chunk, 1.0 / id as f32)))
            .collect::<Vec<_>>();
        let result = select(&records, 7, 28);
        assert_eq!(ids(&result), (1..=7).collect::<Vec<_>>());
        assert!(result.rounds <= 8);
    }

    #[test]
    fn equal_score_ties_refine_to_lowest_message_ids() {
        let records = (1..=70)
            .rev()
            .flat_map(|id| [hit(id, 0, 0.5), hit(id, 1, 0.5)])
            .collect::<Vec<_>>();
        let result = select(&records, 5, 8);
        assert_eq!(ids(&result), vec![1, 2, 3, 4, 5]);
        assert!(result.rounds > 2);
    }

    #[test]
    fn score_order_precedes_id_ceiling_and_best_chunk_is_retained() {
        let records = vec![
            hit(100, 1, 0.8),
            hit(1, 0, 0.1),
            hit(100, 2, 0.9),
            hit(2, 7, 0.8),
        ];
        let result = select(&records, 2, 1);
        assert_eq!(ids(&result), vec![100, 2]);
        assert_eq!(result.hits[0].chunk_idx, 2);
    }

    #[test]
    fn zero_target_does_not_call_backend() {
        let result =
            collect_exact_messages::<&str>(0, 0, 0, |_, _, _| panic!("unexpected query")).unwrap();
        assert!(result.hits.is_empty());
        assert_eq!(result.rounds, 0);
    }

    #[test]
    fn empty_or_short_filtered_archive_is_complete() {
        assert!(select(&[], 4, 4).hits.is_empty());
        let records = vec![hit(3, 0, 0.5), hit(3, 1, 0.4), hit(7, 0, 0.3)];
        assert_eq!(ids(&select(&records, 10, 4)), vec![3, 7]);
    }

    #[test]
    fn negative_scores_and_extreme_ids_obey_total_order() {
        let records = vec![hit(u64::MAX, 0, -1.0), hit(0, 0, -1.0), hit(7, 0, -2.0)];
        assert_eq!(ids(&select(&records, 2, 1)), vec![0, u64::MAX]);
    }

    #[test]
    fn exhausted_refill_budget_never_returns_a_partial_success() {
        let records = vec![hit(9, 0, 0.5), hit(8, 0, 0.5), hit(1, 0, 0.5)];
        let result = collect_exact_messages(1, 1, 1, |excluded, ceiling, window| {
            Ok::<_, &str>(backend(&records, excluded, ceiling, window))
        });
        assert!(matches!(result, Err(RefillError::BudgetExhausted)));
        assert!(matches!(
            collect_exact_messages::<&str>(1, 1, 0, |_, _, _| unreachable!()),
            Err(RefillError::BudgetExhausted)
        ));
    }

    #[test]
    fn backend_failure_is_preserved_not_replaced_by_empty_results() {
        let result = collect_exact_messages(2, 2, 4, |_, _, _| {
            Err::<Vec<VectorSearchResult>, _>("vector read failed")
        });
        assert!(matches!(
            result,
            Err(RefillError::Backend("vector read failed"))
        ));
    }

    #[test]
    fn broken_backend_cannot_repeat_excluded_messages_forever() {
        let result = collect_exact_messages(2, 1, 4, |_, _, _| Ok::<_, &str>(vec![hit(1, 0, 1.0)]));
        assert!(matches!(
            result,
            Err(RefillError::InvalidBatch(
                "backend did not enforce refill scope"
            ))
        ));
    }

    #[test]
    fn invalid_backend_scores_and_windows_fail_explicitly() {
        for score in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let result =
                collect_exact_messages(1, 1, 1, |_, _, _| Ok::<_, &str>(vec![hit(1, 0, score)]));
            assert!(matches!(
                result,
                Err(RefillError::InvalidBatch("non-finite chunk score"))
            ));
        }
        let unsorted = collect_exact_messages(2, 2, 1, |_, _, _| {
            Ok::<_, &str>(vec![hit(1, 0, 0.1), hit(2, 0, 0.9)])
        });
        assert!(matches!(
            unsorted,
            Err(RefillError::InvalidBatch(
                "backend scores are not descending"
            ))
        ));
        let oversized = collect_exact_messages(1, 1, 1, |_, _, _| {
            Ok::<_, &str>(vec![hit(1, 0, 0.9), hit(2, 0, 0.8)])
        });
        assert!(matches!(
            oversized,
            Err(RefillError::InvalidBatch(
                "backend exceeded candidate limit"
            ))
        ));
        assert!(matches!(
            collect_exact_messages::<&str>(1, 0, 1, |_, _, _| unreachable!()),
            Err(RefillError::InvalidBatch("zero candidate window"))
        ));
    }

    #[test]
    fn short_post_dedup_windows_are_not_mistaken_for_exhaustion() {
        let mut records = vec![hit(1, 0, 1.0); 100];
        records.extend(vec![hit(2, 0, 0.8); 100]);
        records.push(hit(3, 0, 0.6));
        let result = collect_exact_messages(3, 12, 8, |excluded, ceiling, window| {
            let mut batch = backend(&records, excluded, ceiling, window);
            // Match the real FSVI resolver's post-top-k document-ID dedup.
            let mut seen = HashSet::new();
            batch.retain(|hit| seen.insert((hit.message_id, hit.chunk_idx)));
            assert!(batch.len() < window);
            Ok::<_, &str>(batch)
        })
        .unwrap();
        assert_eq!(ids(&result), vec![1, 2, 3]);
        assert_eq!(result.rounds, 4);
    }

    #[test]
    fn deterministic_generated_corpora_match_exhaustive_message_oracle() {
        let mut seed = 0x7ca5_5129_882d_91aa_u64;
        for case in 0..120_u64 {
            let mut records = Vec::new();
            for position in 0..400 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                // A small score alphabet exercises dense score ties. The
                // message population varies independently of chunk count.
                let message = seed % (3 + case % 71);
                let score = ((seed >> 24) % 13) as f32 / 8.0 - 1.0;
                records.push(hit(message, (position % 256) as u8, score));
            }
            for target in [1, 2, 7, 25] {
                for window in [1, 4, 17, 64] {
                    let result = select(&records, target, window);
                    let actual = result
                        .hits
                        .iter()
                        .map(|hit| (hit.message_id, hit.score.to_bits()))
                        .collect::<Vec<_>>();
                    assert_eq!(
                        actual,
                        oracle(&records, target),
                        "case={case} target={target} window={window}"
                    );
                }
            }
        }
    }

    #[test]
    fn adaptive_chunk_dominance_reduces_scans_without_losing_exact_messages() {
        let records = (1..=240_u64)
            .flat_map(|id| (0..=255).map(move |chunk| hit(id, chunk, 1.0 - id as f32 / 1024.0)))
            .collect::<Vec<_>>();
        let mut windows = Vec::new();
        let adaptive = collect_exact_messages(
            200,
            801,
            MAX_EXACT_MESSAGE_REFILLS + 1,
            |excluded, ceiling, window| {
                assert!(excluded.len() <= 200);
                windows.push(window);
                Ok::<_, &str>(backend(&records, excluded, ceiling, window))
            },
        )
        .unwrap();
        // A backend may return fewer records than requested. Capping it at
        // the original allowance models the former fixed-window scan cost.
        let fixed = collect_exact_messages(
            200,
            801,
            MAX_EXACT_MESSAGE_REFILLS + 1,
            |excluded, ceiling, _| Ok::<_, &str>(backend(&records, excluded, ceiling, 801)),
        )
        .unwrap();
        assert_eq!(ids(&adaptive), (1..=200).collect::<Vec<_>>());
        assert_eq!(ids(&adaptive), ids(&fixed));
        assert!(adaptive.rounds * 4 < fixed.rounds);
        assert_eq!(windows[0], 801);
        assert!(
            windows
                .iter()
                .all(|window| *window <= MAX_ADAPTIVE_EXACT_WINDOW)
        );
        assert!(windows.windows(2).all(|pair| {
            pair[1] == pair[0].saturating_mul(2).min(MAX_ADAPTIVE_EXACT_WINDOW)
        }));
        let actual = adaptive
            .hits
            .iter()
            .map(|hit| (hit.message_id, hit.score.to_bits()))
            .collect::<Vec<_>>();
        assert_eq!(actual, oracle(&records, 200));
    }

    #[test]
    fn adaptive_dense_ties_finish_within_the_unchanged_call_allowance() {
        let records = (1..=10_000)
            .rev()
            .map(|id| hit(id, 0, 0.5))
            .collect::<Vec<_>>();
        let fixed = collect_exact_messages(
            4,
            17,
            MAX_EXACT_MESSAGE_REFILLS + 1,
            |excluded, ceiling, _| Ok::<_, &str>(backend(&records, excluded, ceiling, 17)),
        );
        assert!(matches!(fixed, Err(RefillError::BudgetExhausted)));
        let adaptive = collect_exact_messages(
            4,
            17,
            MAX_EXACT_MESSAGE_REFILLS + 1,
            |excluded, ceiling, window| {
                Ok::<_, &str>(backend(&records, excluded, ceiling, window))
            },
        )
        .unwrap();
        assert_eq!(ids(&adaptive), vec![1, 2, 3, 4]);
        assert!(adaptive.rounds <= 12);
    }

    #[test]
    fn adaptive_growth_is_capped_without_restarting_the_round_budget() {
        let mut windows = Vec::new();
        let result = collect_exact_messages(1, 1_024, 6, |excluded, ceiling, window| {
            windows.push(window);
            let id = 100 - windows.len() as u64;
            assert!(!excluded.contains(&id));
            assert!(ceiling.is_none_or(|ceiling| id < ceiling));
            // A legal short post-dedup batch cannot prove scope exhaustion.
            Ok::<_, &str>(vec![hit(id, 0, 0.5)])
        });
        assert!(matches!(result, Err(RefillError::BudgetExhausted)));
        assert_eq!(windows, vec![1_024, 2_048, 4_096, 8_192, 16_384, 16_384]);
    }

    #[test]
    fn adaptive_large_caller_windows_never_shrink_or_wrap() {
        for initial in [
            MAX_ADAPTIVE_EXACT_WINDOW + 1,
            usize::MAX / 2 + 1,
            usize::MAX,
        ] {
            let mut windows = Vec::new();
            let result = collect_exact_messages(2, initial, 3, |_, _, window| {
                windows.push(window);
                Ok::<_, &str>(if windows.len() == 1 {
                    vec![hit(1, 0, 1.0)]
                } else {
                    Vec::new()
                })
            })
            .unwrap();
            assert_eq!(windows, vec![initial, initial]);
            assert_eq!(ids(&result), vec![1]);
        }
    }

    #[test]
    fn adaptive_proven_first_window_does_not_issue_a_larger_query() {
        let records = [hit(1, 7, 0.9), hit(2, 8, 0.6), hit(3, 9, 0.3)];
        let mut windows = Vec::new();
        let result = collect_exact_messages(2, 3, 65, |excluded, ceiling, window| {
            windows.push(window);
            Ok::<_, &str>(backend(&records, excluded, ceiling, window))
        })
        .unwrap();
        assert_eq!(windows, vec![3]);
        assert_eq!(result.rounds, 1);
        assert_eq!(ids(&result), vec![1, 2]);
        assert_eq!(result.hits[0].chunk_idx, 7);
    }

    #[test]
    fn adaptive_post_dedup_underfill_preserves_original_filters() {
        let records = (1..=32)
            .flat_map(|id| vec![hit(id, 0, 1.0 - id as f32 / 64.0); 100])
            .collect::<Vec<_>>();
        let selected = records
            .iter()
            .filter(|hit| hit.message_id % 3 == 0)
            .cloned()
            .collect::<Vec<_>>();
        let result = collect_exact_messages(8, 3, 65, |excluded, ceiling, window| {
            // Original membership applies BEFORE top-k on every refill.
            let mut batch = backend(&selected, excluded, ceiling, window);
            let mut seen = HashSet::new();
            batch.retain(|hit| seen.insert((hit.message_id, hit.chunk_idx)));
            Ok::<_, &str>(batch)
        })
        .unwrap();
        let actual = result
            .hits
            .iter()
            .map(|hit| (hit.message_id, hit.score.to_bits()))
            .collect::<Vec<_>>();
        assert_eq!(actual, oracle(&selected, 8));
        assert!(result.rounds > 1);
    }

    #[test]
    fn adaptive_backend_errors_are_not_partial_successes() {
        let mut calls = 0;
        let result = collect_exact_messages(2, 1, 5, |_, _, _| {
            calls += 1;
            if calls == 1 {
                Ok(vec![hit(1, 0, 1.0)])
            } else {
                Err("read failed")
            }
        });
        assert!(matches!(result, Err(RefillError::Backend("read failed"))));
        assert_eq!(calls, 2);
    }

    #[test]
    fn adaptive_cap_does_not_authorize_oversized_initial_batches() {
        let result = collect_exact_messages(2, 1, 5, |_, _, _| {
            Ok::<_, &str>(vec![hit(1, 0, 1.0), hit(2, 0, 0.5)])
        });
        assert!(matches!(
            result,
            Err(RefillError::InvalidBatch(
                "backend exceeded candidate limit"
            ))
        ));
    }

    #[test]
    fn adaptive_capped_windows_still_refine_dense_tie_ids() {
        let records = (1..=40_000)
            .rev()
            .map(|id| hit(id, 0, 0.5))
            .collect::<Vec<_>>();
        let mut calls = 0;
        let result = collect_exact_messages(
            4,
            MAX_ADAPTIVE_EXACT_WINDOW,
            MAX_EXACT_MESSAGE_REFILLS + 1,
            |excluded, ceiling, window| {
                calls += 1;
                assert_eq!(window, MAX_ADAPTIVE_EXACT_WINDOW);
                Ok::<_, &str>(backend(&records, excluded, ceiling, window))
            },
        )
        .unwrap();
        assert_eq!(ids(&result), vec![1, 2, 3, 4]);
        assert_eq!(result.rounds, calls);
        assert!(calls > 2);
    }
}
