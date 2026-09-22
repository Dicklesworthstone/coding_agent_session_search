//! Execution over a retained native cohort and its durable updates.
//!
//! Native HNSW covers only the persisted main slab. Small retained WAL deltas
//! are scored once per request and merged with that graph's current winners.
//! Oversized deltas retain complete-cohort exact recovery; no serving path
//! reopens, compacts, or publishes an artifact.

use super::*;

/// Shared across the cohort, not multiplied by shard count or refill passes.
/// These bound additional delta work/state, not graph ownership or total RSS.
const MAX_NATIVE_WAL_RECORDS: usize = 4_096;
const MAX_NATIVE_WAL_COMPONENTS: usize = 4_194_304;

struct WalShardOverlay<'a> {
    /// Include every updated document, even when its replacement is filtered
    /// out. An obsolete main-slab score must never resurrect through the graph.
    shadowed: HashSet<&'a str>,
    hits: Vec<VectorSearchResult>,
    has_more_candidates: bool,
}

type WalCohortOverlay<'a> = HashMap<usize, WalShardOverlay<'a>>;

fn wal_delta_fits_budget(shards: impl IntoIterator<Item = (usize, usize)>) -> bool {
    let mut records = 0usize;
    let mut components = 0usize;
    for (count, dimension) in shards {
        let Some(next_records) = records.checked_add(count) else {
            return false;
        };
        let Some(next_components) = count
            .checked_mul(dimension)
            .and_then(|count| components.checked_add(count))
        else {
            return false;
        };
        if next_records > MAX_NATIVE_WAL_RECORDS || next_components > MAX_NATIVE_WAL_COMPONENTS {
            return false;
        }
        records = next_records;
        components = next_components;
    }
    true
}

#[derive(Debug)]
struct PendingWalDelta {
    dimension: usize,
}

impl std::fmt::Display for PendingWalDelta {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("retained WAL updates exceed the native delta work budget")
    }
}

impl std::error::Error for PendingWalDelta {}

impl SemanticAnnShardSet {
    /// Refill underfilled native windows before paying for a complete exact scan.
    /// Every pass reuses one scored WAL overlay and the same retained cohort,
    /// query and filter. The graph-call/window budget is shared across shards.
    /// Native failure, oversized WAL deltas or unresolved underfill still recover
    /// the entire exact cohort; incomplete native results never escape recovery.
    /// Invalid queries and mismatched context owners remain errors, not fallbacks.
    pub(in super::super) fn search_with_exact_fallback(
        &self,
        context: &SemanticCandidateContext,
        embedding: &[f32],
        fetch_limit: usize,
        filter: Option<&dyn FsSearchFilter>,
    ) -> Result<(
        Vec<VectorSearchResult>,
        SemanticCandidateRetryState,
        Option<AnnSearchStats>,
    )> {
        if !self.validate_native_query(&context.artifacts, embedding, fetch_limit)? {
            return Ok((Vec::new(), SemanticCandidateRetryState::default(), None));
        }
        let mut budget = NativeRefillBudget::new(fetch_limit, context.artifacts.len());
        let mut native_window = fetch_limit;
        let mut stats: Option<AnnSearchStats> = None;
        let mut best_by_message: HashMap<u64, VectorSearchResult> = HashMap::new();
        let (native_messages, reason) =
            match self.prepare_wal_overlay(embedding, fetch_limit, filter) {
                Ok(overlay) => loop {
                    match self.search_prepared(embedding, native_window, filter, &overlay) {
                        Ok((hits, mut retry, measured)) => {
                            if let Some(measured) = measured {
                                match stats.as_mut() {
                                    Some(total) => accumulate_native_stats(total, &measured),
                                    None => stats = Some(measured),
                                }
                            }
                            // A wider approximate window need not contain all
                            // earlier winners. All passes apply the same WAL
                            // shadow set, so retaining these scores is safe.
                            for hit in hits {
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
                            let omitted_retained = best_by_message.len() > fetch_limit;
                            let hits = SearchClient::collapse_semantic_results(
                                best_by_message,
                                fetch_limit,
                            );
                            if hits.len() >= fetch_limit || !retry.has_more_candidates {
                                retry.has_more_candidates |= omitted_retained;
                                return Ok((hits, retry, stats));
                            }
                            let Some(next_window) = budget.next_window() else {
                                let reason = if filter.is_some() {
                                    AnnExactFallbackReason::FilteredCandidateUnderfill
                                } else {
                                    AnnExactFallbackReason::MessageCandidateUnderfill
                                };
                                break (hits.len(), reason);
                            };
                            best_by_message =
                                hits.into_iter().map(|hit| (hit.message_id, hit)).collect();
                            native_window = next_window;
                        }
                        Err(error) => {
                            let failure = error.downcast::<NativeAnnSearchFailure>()?;
                            // Keep completed work, but discard ALL native
                            // and delta hits before exact cohort recovery.
                            match stats.as_mut() {
                                Some(total) => {
                                    accumulate_native_stats(total, &failure.completed_stats);
                                }
                                None => stats = Some(failure.completed_stats),
                            }
                            break (0, AnnExactFallbackReason::NativeSearchFailed);
                        }
                    }
                },
                Err(error) if error.is::<PendingWalDelta>() => {
                    let pending = error
                        .downcast_ref::<PendingWalDelta>()
                        .expect("typed WAL error");
                    stats = Some(AnnSearchStats {
                        dimension: pending.dimension,
                        ..Default::default()
                    });
                    (0, AnnExactFallbackReason::WalDeltaRequiresExact)
                }
                Err(error) => return Err(error),
            };
        let started = std::time::Instant::now();
        let (exact, exact_retry) =
            SearchClient::search_exact_semantic_indexes(context, embedding, fetch_limit, filter)?;
        let receipt = AnnExactFallbackStats {
            reason,
            shard_count: context.artifacts.len(),
            search_time_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            returned_messages: exact.len(),
        };
        tracing::debug!(
            ?reason,
            native_messages,
            exact_messages = exact.len(),
            shards = context.artifacts.len(),
            "native retrieval recovered through the retained exact cohort"
        );
        if let Some(stats) = stats.as_mut() {
            stats.is_approximate = false;
            stats.exact_fallback = Some(receipt);
        }
        Ok((exact, exact_retry, stats))
    }

    /// Search the admitted main graphs plus their bounded, current WAL overlays.
    ///
    /// Statistics continue to describe native calls only. Their recall estimate
    /// is a backend heuristic, not a measured guarantee for the fused page.
    /// Delta work is separately traced; it never makes approximate main-slab
    /// retrieval exact. Additional state is O(k + bounded delta), not O(archive).
    #[cfg(test)]
    pub(in super::super) fn search(
        &self,
        artifacts: &Arc<Vec<SemanticIndexArtifact>>,
        embedding: &[f32],
        fetch_limit: usize,
        filter: Option<&dyn FsSearchFilter>,
    ) -> Result<(
        Vec<VectorSearchResult>,
        SemanticCandidateRetryState,
        Option<AnnSearchStats>,
    )> {
        if !self.validate_native_query(artifacts, embedding, fetch_limit)? {
            return Ok((Vec::new(), SemanticCandidateRetryState::default(), None));
        }
        let overlay = self.prepare_wal_overlay(embedding, fetch_limit, filter)?;
        self.search_prepared(embedding, fetch_limit, filter, &overlay)
    }

    /// Preserve owner validation even for zero-limit requests. As before, a
    /// zero-limit request does not validate/scour vectors or prepare any delta.
    fn validate_native_query(
        &self,
        artifacts: &Arc<Vec<SemanticIndexArtifact>>,
        embedding: &[f32],
        fetch_limit: usize,
    ) -> Result<bool> {
        if !Arc::ptr_eq(&self.artifacts, artifacts) {
            bail!("native ANN cohort does not match the admitted semantic context");
        }
        if fetch_limit == 0 {
            return Ok(false);
        }
        let dimension = self.artifacts[0].index().dimension();
        if embedding.len() != dimension || embedding.iter().any(|value| !value.is_finite()) {
            bail!("native ANN query must have the admitted dimension and finite values");
        }
        Ok(true)
    }

    /// All metadata admission precedes allocation/scoring and graph execution.
    /// Borrow the reader's already deduplicated last-write-wins rows, never the
    /// WAL pathname. Cache only per-shard top-k messages and document shadows.
    fn prepare_wal_overlay<'a>(
        &'a self,
        embedding: &[f32],
        fetch_limit: usize,
        filter: Option<&dyn FsSearchFilter>,
    ) -> Result<WalCohortOverlay<'a>> {
        if !wal_delta_fits_budget(self.artifacts.iter().map(|artifact| {
            let index = artifact.index();
            (index.wal_record_count(), index.dimension())
        })) {
            return Err(PendingWalDelta {
                dimension: embedding.len(),
            }
            .into());
        }
        let started = std::time::Instant::now();
        let mut overlay = HashMap::new();
        let mut scored = 0usize;
        let mut records = 0usize;
        for (ordinal, artifact) in self.artifacts.iter().enumerate() {
            let index = artifact.index();
            let count = index.wal_record_count();
            if count == 0 {
                continue;
            }
            let mut shadowed = HashSet::with_capacity(count);
            let mut best_by_message: HashMap<u64, VectorSearchResult> = HashMap::new();
            for (doc_id, vector) in index.wal_records() {
                shadowed.insert(doc_id);
                let parsed = parse_semantic_doc_id(doc_id).ok_or_else(|| {
                    anyhow!("retained WAL has an invalid semantic document identity")
                })?;
                if filter.is_some_and(|filter| !filter.matches(doc_id, None)) {
                    continue;
                }
                // Use the same f32 kernel as upstream exact WAL retrieval,
                // including the retained reader's F16 replay quantization.
                let score = frankensearch::index::dot_product_f32_f32(vector, embedding)
                    .map_err(|error| anyhow!("retained WAL scoring failed: {error}"))?;
                if !score.is_finite() {
                    bail!("retained WAL scoring produced a non-finite result");
                }
                scored += 1;
                best_by_message
                    .entry(parsed.message_id)
                    .and_modify(|best| {
                        if score.total_cmp(&best.score).is_gt() {
                            best.score = score;
                            best.chunk_idx = parsed.chunk_idx;
                        }
                    })
                    .or_insert(VectorSearchResult {
                        message_id: parsed.message_id,
                        chunk_idx: parsed.chunk_idx,
                        score,
                    });
            }
            records += count;
            let has_more_candidates = best_by_message.len() > fetch_limit;
            let hits = SearchClient::collapse_semantic_results(best_by_message, fetch_limit);
            overlay.insert(
                ordinal,
                WalShardOverlay {
                    shadowed,
                    hits,
                    has_more_candidates,
                },
            );
        }
        if !overlay.is_empty() {
            tracing::debug!(
                delta_shards = overlay.len(),
                delta_records = records,
                delta_records_scored = scored,
                delta_search_time_us =
                    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                "prepared retained WAL overlay once for native retrieval"
            );
        }
        Ok(overlay)
    }

    /// Called only after request validation and complete delta preparation.
    /// Each shadow set is shard-local: a replacement in one artifact cannot
    /// erase a distinct current contribution from another retained artifact.
    fn search_prepared(
        &self,
        embedding: &[f32],
        fetch_limit: usize,
        filter: Option<&dyn FsSearchFilter>,
        overlay: &WalCohortOverlay<'_>,
    ) -> Result<(
        Vec<VectorSearchResult>,
        SemanticCandidateRetryState,
        Option<AnnSearchStats>,
    )> {
        let dimension = embedding.len();
        let candidate_limit = fetch_limit
            .saturating_mul(ANN_CANDIDATE_MULTIPLIER)
            .max(fetch_limit);
        let mut stats = AnnSearchStats {
            dimension,
            estimated_recall: 1.0,
            ..Default::default()
        };
        let mut best_by_message = HashMap::new();
        let mut has_more_candidates = false;
        for (ordinal, graph) in self.graphs.iter().enumerate() {
            let candidate = candidate_limit.min(graph.len());
            let ef = FS_HNSW_DEFAULT_EF_SEARCH.max(candidate);
            // Borrow the exact source selected at admission, never its path.
            // The native call also checks source extent and returned row IDs.
            let source = self.artifacts[ordinal].index();
            let delta = overlay.get(&ordinal);
            let (hits, measured) = graph
                .knn_search_with_stats_against(source, embedding, candidate, ef)
                .map_err(|_| NativeAnnSearchFailure::new(ordinal, stats.clone()))?;
            if measured.dimension != dimension || hits.len() > candidate {
                return Err(NativeAnnSearchFailure::new(ordinal, stats).into());
            }
            // A failed later shard cannot return an earlier partial page.
            has_more_candidates |= candidate < measured.index_size;
            stats.index_size = stats.index_size.saturating_add(measured.index_size);
            stats.ef_search = stats.ef_search.max(measured.ef_search);
            stats.k_requested = stats.k_requested.saturating_add(measured.k_requested);
            stats.k_returned = stats.k_returned.saturating_add(measured.k_returned);
            stats.search_time_us = stats.search_time_us.saturating_add(measured.search_time_us);
            stats.estimated_recall = stats.estimated_recall.min(measured.estimated_recall as f32);
            stats.is_approximate |= measured.is_approximate;
            for hit in &hits {
                if delta.is_some_and(|delta| delta.shadowed.contains(hit.doc_id.as_str())) {
                    continue;
                }
                if filter.is_none_or(|filter| filter.matches(&hit.doc_id, None)) {
                    SearchClient::record_fs_semantic_hit(&mut best_by_message, hit);
                }
            }
            if let Some(delta) = delta {
                has_more_candidates |= delta.has_more_candidates;
                for hit in &delta.hits {
                    best_by_message
                        .entry(hit.message_id)
                        .and_modify(|best| {
                            if hit.score.total_cmp(&best.score).is_gt() {
                                best.score = hit.score;
                                best.chunk_idx = hit.chunk_idx;
                            }
                        })
                        .or_insert_with(|| hit.clone());
                }
            }
            // Exhausting the graph does not exhaust a larger WAL message page.
            has_more_candidates |= best_by_message.len() > fetch_limit;
            best_by_message = SearchClient::collapse_semantic_results(best_by_message, fetch_limit)
                .into_iter()
                .map(|hit| (hit.message_id, hit))
                .collect();
        }
        tracing::debug!(
            shards = self.graphs.len(),
            delta_shards = overlay.len(),
            native_candidates = stats.k_returned,
            returned_messages = best_by_message.len(),
            "native ANN and retained WAL shard merge complete"
        );
        Ok((
            SearchClient::collapse_semantic_results(best_by_message, fetch_limit),
            SemanticCandidateRetryState {
                has_more_candidates,
                exact_window_may_omit_competitor: false,
            },
            Some(stats),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_budget_is_shared_across_shards() {
        assert!(wal_delta_fits_budget([(2_048, 384), (2_048, 384)]));
        assert!(!wal_delta_fits_budget([(2_048, 384), (2_049, 384)]));
        assert!(!wal_delta_fits_budget(std::iter::repeat_n((1, 1), 4_097)));
    }

    #[test]
    fn wal_budget_bounds_scalar_work_and_handles_overflow() {
        assert!(wal_delta_fits_budget([(4_096, 1_024)]));
        assert!(!wal_delta_fits_budget([(4_096, 1_025)]));
        assert!(!wal_delta_fits_budget([(1, usize::MAX)]));
        assert!(!wal_delta_fits_budget([(2, usize::MAX)]));
        assert!(!wal_delta_fits_budget([(usize::MAX, 2)]));
        assert!(wal_delta_fits_budget([(0, usize::MAX)]));
        assert!(wal_delta_fits_budget([]));
    }
}
