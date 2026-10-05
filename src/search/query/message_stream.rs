//! Single-pass, bounded message selection for unresolved exact chunk windows.
//!
//! Retain the best chunk per message DURING the scan, not after raw top-k.
//! The cutoff only improves: an evicted message can re-enter only with a better
//! chunk, so no archive-sized seen-message set or repeated exclusion scan is
//! needed. The ordinary first-window fast path remains in the caller.

use super::{FsSearchFilter, FsVectorIndex, SemanticIndexArtifact, VectorSearchResult,
    parse_semantic_doc_id};
use anyhow::{Result, anyhow, ensure};
use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};

#[cfg(test)]
mod tests;

// Borrowed WAL keys are additional per-request state. Larger retained deltas
// use the existing exact driver, never a partial main-only result.
const MAX_WAL_SHADOW_KEYS: usize = 4_096;
const MAX_SCAN_WORKERS: usize = 8;
const MIN_ROWS_PER_WORKER: usize = 32_768;
// Bound aggregate partial selections, not just each worker's local state.
const MAX_PARALLEL_SELECTION_KEYS: usize = 65_536;

#[derive(Clone, Copy, Debug)]
struct Winner {
    message_id: u64,
    chunk_idx: u8,
    score: f32,
}

impl PartialEq for Winner {
    fn eq(&self, other: &Self) -> bool {
        self.message_id == other.message_id && self.score.to_bits() == other.score.to_bits()
    }
}
impl Eq for Winner {}
impl PartialOrd for Winner {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl Ord for Winner {
    fn cmp(&self, other: &Self) -> Ordering {
        // Ascending quality, so the first entry is the eviction candidate.
        self.score.total_cmp(&other.score)
            .then_with(|| other.message_id.cmp(&self.message_id))
    }
}

struct Selection {
    limit: usize,
    by_message: HashMap<u64, Winner>,
    ordered: BTreeSet<Winner>,
}

impl Selection {
    fn new(limit: usize) -> Self {
        Self { limit, by_message: HashMap::new(), ordered: BTreeSet::new() }
    }

    fn consider(&mut self, hit: Winner) -> Result<()> {
        ensure!(hit.score.is_finite(), "non-finite exact semantic chunk score");
        if self.limit == 0 { return Ok(()); }
        if let Some(prior) = self.by_message.get(&hit.message_id).copied() {
            // Equal-score chunks retain the first physical row, as in the
            // backend's score/row ordering. No stale heap nodes accumulate.
            if hit.score.total_cmp(&prior.score).is_le() { return Ok(()); }
            self.ordered.remove(&prior);
        } else if self.ordered.len() == self.limit {
            let worst = self.ordered.first().copied()
                .ok_or_else(|| anyhow!("exact message selection lost its cutoff"))?;
            if hit <= worst { return Ok(()); }
            self.ordered.pop_first();
            self.by_message.remove(&worst.message_id);
        }
        self.ordered.insert(hit);
        self.by_message.insert(hit.message_id, hit);
        Ok(())
    }

    fn finish(self) -> Vec<Winner> { self.ordered.into_iter().rev().collect() }
}

fn worker_count(rows: usize, limit: usize, pool_workers: usize) -> usize {
    (rows / MIN_ROWS_PER_WORKER).max(1)
        .min(pool_workers.max(1))
        .min(MAX_SCAN_WORKERS)
        .min((MAX_PARALLEL_SELECTION_KEYS / limit.max(1)).max(1))
}

fn scan_range(
    index: &FsVectorIndex,
    embedding: &[f32],
    filter: Option<&dyn FsSearchFilter>,
    shadowed: &HashSet<&str>,
    range: std::ops::Range<usize>,
    limit: usize,
) -> Result<(Vec<Winner>, usize)> {
    let mut selection = Selection::new(limit);
    let mut scored = 0usize;
    for row in range {
        if index.is_deleted(row) { continue; }
        let doc_id = index.doc_id_at(row)?;
        if shadowed.contains(doc_id) || filter.is_some_and(|f| !f.matches(doc_id, None)) {
            continue;
        }
        let identity = parse_semantic_doc_id(doc_id)
            .ok_or_else(|| anyhow!("exact semantic scan found an invalid message identity"))?;
        // At these admitted widths this API uses the exact same byte-based
        // F16/F32 SIMD kernel as search_top_k, with no per-row vector Vec.
        let score = index.dot_query_at(row, embedding)?;
        selection.consider(Winner { message_id: identity.message_id,
            chunk_idx: identity.chunk_idx, score })?;
        scored += 1;
    }
    Ok((selection.finish(), scored))
}

/// None means use the incumbent exact refill driver, NOT an empty answer.
/// No file is reopened and no source, WAL or graph is written. All paths use
/// the already admitted cohort; a failed shard fails the entire operation.
///
/// Standard embedding widths share the backend's bit-exact SIMD reduction.
/// Non-multiples of 32 deliberately stay on the incumbent driver: upstream's
/// public row scorer uses a different tail reduction at those widths.
///
/// Selection state is O(k + min(8*k, 65536) + bounded WAL keys). This does not
/// bound the retained index, mapped pages, allocator RSS or query wall time.
pub(super) fn try_collect_exact_messages(
    artifacts: &[SemanticIndexArtifact],
    embedding: &[f32],
    limit: usize,
    filter: Option<&dyn FsSearchFilter>,
) -> Result<Option<Vec<VectorSearchResult>>> {
    if limit == 0 { return Ok(Some(Vec::new())); }
    let mut wal_keys = 0usize;
    for artifact in artifacts {
        let index = artifact.index();
        ensure!(index.dimension() == embedding.len(), "exact semantic query dimension mismatch");
        if !index.dimension().is_multiple_of(32) { return Ok(None); }
        let Some(total) = wal_keys.checked_add(index.wal_record_count()) else { return Ok(None); };
        if total > MAX_WAL_SHADOW_KEYS { return Ok(None); }
        wal_keys = total;
    }
    ensure!(embedding.iter().all(|value| value.is_finite()), "exact semantic query must be finite");
    let mut selection = Selection::new(limit);
    let mut main_scored = 0usize;
    let mut wal_scored = 0usize;
    let mut main_rows = 0usize;
    let mut max_workers = 1usize;
    for artifact in artifacts {
        let index = artifact.index();
        let shadowed: HashSet<&str> = index.wal_records().map(|(id, _)| id).collect();
        let rows = index.record_count();
        let workers = if rows < 2 * MIN_ROWS_PER_WORKER || limit > MAX_PARALLEL_SELECTION_KEYS / 2
            || !frankensearch::index::SearchParams::default().parallel_enabled {
            1
        } else {
            worker_count(rows, limit, rayon::current_num_threads())
        };
        max_workers = max_workers.max(workers);
        main_rows = main_rows.saturating_add(rows);
        // Indexed collection retains physical-range order even if workers
        // finish out of order. Merge in that order to preserve chunk ties.
        let partials: Vec<Result<(Vec<Winner>, usize)>> = if workers == 1 {
            vec![scan_range(index, embedding, filter, &shadowed, 0..rows, limit)]
        } else {
            (0..workers).into_par_iter().map(|worker| {
                // Quotient/remainder partition avoids rows * worker overflow.
                let width = rows / workers;
                let extra = rows % workers;
                let start = worker * width + worker.min(extra);
                let end = start + width + usize::from(worker < extra);
                scan_range(index, embedding, filter, &shadowed, start..end, limit)
            }).collect()
        };
        for partial in partials {
            let (hits, scored) = partial?;
            main_scored = main_scored.saturating_add(scored);
            for hit in hits { selection.consider(hit)?; }
        }
        // WAL shadowing happens BEFORE main selection, including replacements
        // rejected by the filter. An obsolete main score cannot resurface.
        for (doc_id, vector) in index.wal_records() {
            if filter.is_some_and(|f| !f.matches(doc_id, None)) { continue; }
            let identity = parse_semantic_doc_id(doc_id)
                .ok_or_else(|| anyhow!("exact semantic WAL has an invalid message identity"))?;
            let score = frankensearch::index::dot_product_f32_f32(vector, embedding)?;
            selection.consider(Winner { message_id: identity.message_id,
                chunk_idx: identity.chunk_idx, score })?;
            wal_scored = wal_scored.saturating_add(1);
        }
    }
    let hits: Vec<_> = selection.finish().into_iter().map(|hit| VectorSearchResult {
        message_id: hit.message_id, chunk_idx: hit.chunk_idx, score: hit.score,
    }).collect();
    tracing::debug!(shard_count = artifacts.len(), main_rows, main_scored, wal_scored,
        max_workers, selection_limit = limit, returned = hits.len(),
        "single-pass exact semantic message refinement complete");
    Ok(Some(hits))
}
