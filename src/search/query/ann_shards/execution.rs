//! Execution over a retained native cohort and its durable updates.
//!
//! Native HNSW covers only the persisted main slab. A readable WAL does not
//! invalidate that graph's main-slab identity, but it adds or supersedes live
//! documents. Never report the main-only page as complete in that state.

use super::*;

#[derive(Debug)]
struct PendingWalDelta {
    dimension: usize,
}

impl std::fmt::Display for PendingWalDelta {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("retained WAL updates require delta-aware semantic retrieval")
    }
}

impl std::error::Error for PendingWalDelta {}

impl SemanticAnnShardSet {
    /// Refill underfilled native windows before paying for a complete exact scan.
    /// Extra native passes keep the same retained cohort, query and filter, and
    /// have a shared graph-call/window budget. A complete initial page performs
    /// no extra work. Native failure or unresolved underfill still recovers the
    /// entire exact cohort; no partial native/exact ranking is returned.
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
        let mut budget = NativeRefillBudget::new(fetch_limit, context.artifacts.len());
        let mut native_window = fetch_limit;
        let mut stats: Option<AnnSearchStats> = None;
        let mut best_by_message: HashMap<u64, VectorSearchResult> = HashMap::new();
        let (native_messages, reason) = loop {
            match self.search(&context.artifacts, embedding, native_window, filter) {
                Ok((hits, mut retry, measured)) => {
                    if let Some(measured) = measured {
                        match stats.as_mut() {
                            Some(total) => accumulate_native_stats(total, &measured),
                            None => stats = Some(measured),
                        }
                    }
                    // A larger approximate window need not be a strict superset
                    // of the previous one. Keep its earlier valid winners too;
                    // between passes, retain only the requested message page.
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
                    let hits =
                        SearchClient::collapse_semantic_results(best_by_message, fetch_limit);
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
                    best_by_message = hits.into_iter().map(|hit| (hit.message_id, hit)).collect();
                    native_window = next_window;
                }
                Err(error) if error.is::<PendingWalDelta>() => {
                    let pending = error
                        .downcast_ref::<PendingWalDelta>()
                        .expect("typed WAL error");
                    stats = Some(AnnSearchStats {
                        dimension: pending.dimension,
                        ..Default::default()
                    });
                    break (0, AnnExactFallbackReason::WalDeltaRequiresExact);
                }
                Err(error) => match error.downcast::<NativeAnnSearchFailure>() {
                    Ok(failure) => {
                        // Include completed calls from earlier refill passes,
                        // not just the prefix of the pass that failed. Discard
                        // ALL native hits before complete-cohort exact recovery.
                        match stats.as_mut() {
                            Some(total) => {
                                accumulate_native_stats(total, &failure.completed_stats);
                            }
                            None => stats = Some(failure.completed_stats),
                        }
                        break (0, AnnExactFallbackReason::NativeSearchFailed);
                    }
                    // Do not turn a caller/provenance error into an expensive
                    // exact scan, or mistake it for a failed derived graph.
                    Err(error) => return Err(error),
                },
            }
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

    /// Search each admitted graph, then merge by best chunk per message.
    ///
    /// Statistics describe actual native calls: sizes, requested/returned raw
    /// candidates, and native-search times are summed; ef is the maximum and
    /// estimated recall the minimum of the backend heuristics, NOT a measured
    /// recall guarantee for the fused message page. Candidate state is O(k),
    /// not O(k * shards); the retained graphs themselves are not memory-capped.
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
        if !Arc::ptr_eq(&self.artifacts, artifacts) {
            bail!("native ANN cohort does not match the admitted semantic context");
        }
        if fetch_limit == 0 {
            return Ok((Vec::new(), SemanticCandidateRetryState::default(), None));
        }
        let dimension = self.artifacts[0].index().dimension();
        if embedding.len() != dimension || embedding.iter().any(|value| !value.is_finite()) {
            bail!("native ANN query must have the admitted dimension and finite values");
        }
        if self
            .artifacts
            .iter()
            .any(|artifact| artifact.index().wal_record_count() > 0)
        {
            return Err(PendingWalDelta { dimension }.into());
        }
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
            // A large global page must not request a huge beam from a small
            // shard. The native graph count bounds its candidate request.
            let candidate = candidate_limit.min(graph.len());
            let ef = FS_HNSW_DEFAULT_EF_SEARCH.max(candidate);
            // Native underfill repair needs the exact source selected during
            // admission. Borrow its retained reader, never reopen its path.
            // The source-backed call also checks extent and returned row IDs.
            let source = self.artifacts[ordinal].index();
            let (hits, measured) = graph
                .knn_search_with_stats_against(source, embedding, candidate, ef)
                .map_err(|_| NativeAnnSearchFailure::new(ordinal, stats.clone()))?;
            if measured.dimension != dimension || hits.len() > candidate {
                return Err(NativeAnnSearchFailure::new(ordinal, stats).into());
            }
            // A failure above returns no partial success assembled from earlier
            // shards. No graph is skipped because it produced no filtered hits.
            // Post-top-k document deduplication can shorten a complete window.
            has_more_candidates |= candidate < measured.index_size;
            stats.index_size = stats.index_size.saturating_add(measured.index_size);
            stats.ef_search = stats.ef_search.max(measured.ef_search);
            stats.k_requested = stats.k_requested.saturating_add(measured.k_requested);
            stats.k_returned = stats.k_returned.saturating_add(measured.k_returned);
            stats.search_time_us = stats.search_time_us.saturating_add(measured.search_time_us);
            stats.estimated_recall = stats.estimated_recall.min(measured.estimated_recall as f32);
            stats.is_approximate |= measured.is_approximate;
            for hit in &hits {
                if filter.is_none_or(|filter| filter.matches(&hit.doc_id, None)) {
                    SearchClient::record_fs_semantic_hit(&mut best_by_message, hit);
                }
            }
            // Raw graph exhaustion is not message-page exhaustion: collapsing
            // an exhaustive native window can still discard eligible messages.
            has_more_candidates |= best_by_message.len() > fetch_limit;
            best_by_message = SearchClient::collapse_semantic_results(best_by_message, fetch_limit)
                .into_iter()
                .map(|hit| (hit.message_id, hit))
                .collect();
        }
        tracing::debug!(
            shards = self.graphs.len(),
            native_candidates = stats.k_returned,
            returned_messages = best_by_message.len(),
            "native ANN shard merge complete"
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
