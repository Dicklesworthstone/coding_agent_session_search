//! Bound extra native work before falling back to a complete exact search.
//!
//! Each retry searches the entire retained cohort. Limit both the requested
//! message window and total extra graph calls; neither bound covers resident
//! graph memory or the duration of one backend call.

use super::AnnSearchStats;

const MAX_REFILL_MESSAGE_WINDOW: usize = 4_096;
const MAX_REFILL_PASSES: usize = 4;
const MAX_REFILL_GRAPH_CALLS: usize = 32;

pub(super) struct NativeRefillBudget {
    message_window: usize,
    shards: usize,
    passes_left: usize,
    graph_calls_left: usize,
}

impl NativeRefillBudget {
    pub(super) fn new(message_window: usize, shards: usize) -> Self {
        Self {
            message_window,
            shards,
            passes_left: MAX_REFILL_PASSES,
            graph_calls_left: MAX_REFILL_GRAPH_CALLS,
        }
    }

    pub(super) fn next_window(&mut self) -> Option<usize> {
        if self.message_window == 0
            || self.shards == 0
            || self.passes_left == 0
            || self.graph_calls_left < self.shards
            || self.message_window >= MAX_REFILL_MESSAGE_WINDOW
        {
            return None;
        }
        self.message_window = self
            .message_window
            .saturating_mul(4)
            .min(MAX_REFILL_MESSAGE_WINDOW);
        self.passes_left -= 1;
        self.graph_calls_left -= self.shards;
        Some(self.message_window)
    }
}

/// These are additional searches of the SAME cohort, not additional shards.
/// Keep its size once, but count every completed native request and its cost.
pub(super) fn accumulate_native_stats(total: &mut AnnSearchStats, next: &AnnSearchStats) {
    total.ef_search = total.ef_search.max(next.ef_search);
    total.k_requested = total.k_requested.saturating_add(next.k_requested);
    total.k_returned = total.k_returned.saturating_add(next.k_returned);
    total.search_time_us = total.search_time_us.saturating_add(next.search_time_us);
    total.estimated_recall = total.estimated_recall.min(next.estimated_recall);
    total.is_approximate |= next.is_approximate;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refill_windows_grow_without_an_archive_sized_allocation_request() {
        let mut budget = NativeRefillBudget::new(100, 1);
        assert_eq!(budget.next_window(), Some(400));
        assert_eq!(budget.next_window(), Some(1_600));
        assert_eq!(budget.next_window(), Some(4_096));
        assert_eq!(budget.next_window(), None);
        for initial in [0, 4_096, usize::MAX] {
            assert_eq!(NativeRefillBudget::new(initial, 1).next_window(), None);
        }
    }

    #[test]
    fn a_pass_must_fit_the_global_extra_graph_call_budget() {
        let mut budget = NativeRefillBudget::new(1, 12);
        assert_eq!(budget.next_window(), Some(4));
        assert_eq!(budget.next_window(), Some(16));
        assert_eq!(budget.next_window(), None);
        assert_eq!(NativeRefillBudget::new(1, 33).next_window(), None);
        assert_eq!(NativeRefillBudget::new(1, 0).next_window(), None);
        let mut single = NativeRefillBudget::new(1, 1);
        assert_eq!((0..4).filter_map(|_| single.next_window()).count(), 4);
        assert_eq!(single.next_window(), None);
    }

    #[test]
    fn a_failed_refill_preserves_prior_work_and_clears_the_recall_heuristic() {
        let mut total = AnnSearchStats {
            index_size: 82,
            dimension: 2,
            k_requested: 16,
            k_returned: 16,
            search_time_us: 10,
            estimated_recall: 0.95,
            is_approximate: true,
            ..Default::default()
        };
        let completed_prefix = AnnSearchStats {
            index_size: 41,
            dimension: 2,
            k_requested: 32,
            k_returned: 32,
            search_time_us: 20,
            estimated_recall: 0.0,
            ..Default::default()
        };
        accumulate_native_stats(&mut total, &completed_prefix);
        assert_eq!(total.index_size, 82);
        assert_eq!(total.k_requested, 48);
        assert_eq!(total.k_returned, 48);
        assert_eq!(total.search_time_us, 30);
        assert_eq!(total.estimated_recall, 0.0);
    }

    #[test]
    fn native_work_counters_saturate_instead_of_wrapping() {
        let mut total = AnnSearchStats {
            k_requested: usize::MAX,
            k_returned: usize::MAX,
            search_time_us: u64::MAX,
            ..Default::default()
        };
        let next = AnnSearchStats {
            k_requested: 1,
            k_returned: 1,
            search_time_us: 1,
            ..Default::default()
        };
        accumulate_native_stats(&mut total, &next);
        assert_eq!(total.k_requested, usize::MAX);
        assert_eq!(total.k_returned, usize::MAX);
        assert_eq!(total.search_time_us, u64::MAX);
    }

    #[test]
    fn repeated_searches_count_work_without_multiplying_the_corpus_size() {
        let mut total = AnnSearchStats {
            index_size: 500,
            dimension: 384,
            ef_search: 64,
            k_requested: 40,
            k_returned: 40,
            search_time_us: 10,
            estimated_recall: 0.9,
            is_approximate: true,
            ..Default::default()
        };
        let next = AnnSearchStats {
            index_size: 500,
            dimension: 384,
            ef_search: 160,
            k_requested: 160,
            k_returned: 150,
            search_time_us: 20,
            estimated_recall: 0.95,
            ..Default::default()
        };
        accumulate_native_stats(&mut total, &next);
        assert_eq!(total.index_size, 500);
        assert_eq!(total.dimension, 384);
        assert_eq!(total.ef_search, 160);
        assert_eq!(total.k_requested, 200);
        assert_eq!(total.k_returned, 190);
        assert_eq!(total.search_time_us, 30);
        assert_eq!(total.estimated_recall, 0.9);
        assert!(total.is_approximate);
        assert!(total.exact_fallback.is_none());
    }
}
