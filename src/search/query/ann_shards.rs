//! Native ANN over every explicitly paired shard of one retained vector cohort.
//!
//! Admission is all-or-nothing: an unavailable sidecar never silently removes
//! its source shard. The caller falls back to the complete exact cohort. Native
//! loading uses the existing no-rebuild admission function; no query writes or
//! rediscovers an artifact. Graph admission can still read all source vectors.

use super::*;
use crate::search::ann_index::AnnSearchStats;

pub(super) struct SemanticAnnShardSet {
    artifacts: Arc<Vec<SemanticIndexArtifact>>,
    graphs: Vec<FsHnswIndex>,
}

impl SemanticAnnShardSet {
    /// Cheap retained metadata check, before allocating any native graph.
    pub(super) fn unavailable_reason(
        artifacts: &[SemanticIndexArtifact],
    ) -> Option<SemanticAnnUnavailableReason> {
        if artifacts.is_empty() {
            return Some(SemanticAnnUnavailableReason::SidecarMissing);
        }
        artifacts.iter().find_map(|artifact| {
            artifact.ann_unavailable_reason().or_else(|| {
                artifact
                    .ann_path()
                    .is_none()
                    .then_some(SemanticAnnUnavailableReason::SidecarMissing)
            })
        })
    }

    pub(super) fn open(
        artifacts: Arc<Vec<SemanticIndexArtifact>>,
    ) -> std::result::Result<Self, SemanticAnnOpenFailure> {
        if let Some(reason) = Self::unavailable_reason(&artifacts) {
            return Err(SemanticAnnOpenFailure {
                reason,
                diagnostic: "shard_pairing_unavailable",
            });
        }
        let first = artifacts[0].index();
        for artifact in artifacts.iter() {
            let index = artifact.index();
            // The enclosing context already validates embedder ID/dimension.
            // Retain that guard here for direct callers, and do not combine
            // explicitly different v2 spaces merely because their widths match.
            let same_space = match (
                first.metadata().identity_v2.as_ref(),
                index.metadata().identity_v2.as_ref(),
            ) {
                (None, None) => true, // preserve the existing legacy v1 contract
                (Some(left), Some(right)) => left.space_fingerprint == right.space_fingerprint,
                _ => false,
            };
            if first.dimension() != index.dimension()
                || first.embedder_id() != index.embedder_id()
                || !same_space
            {
                return Err(SemanticAnnOpenFailure {
                    reason: SemanticAnnUnavailableReason::SidecarOpenFailed,
                    diagnostic: "shard_embedding_space_mismatch",
                });
            }
        }
        let mut graphs = Vec::with_capacity(artifacts.len());
        for artifact in artifacts.iter() {
            let path = artifact.ann_path().ok_or(SemanticAnnOpenFailure {
                reason: SemanticAnnUnavailableReason::SidecarMissing,
                diagnostic: "shard_pairing_missing",
            })?;
            graphs.push(open_fs_semantic_ann_index(artifact.index(), path)?);
        }
        Ok(Self { artifacts, graphs })
    }

    /// Search each admitted graph, then merge by best chunk per message.
    ///
    /// Statistics describe actual native calls: sizes, requested/returned raw
    /// candidates, and native-search times are summed; ef is the maximum and
    /// estimated recall the minimum of the backend heuristics, NOT a measured
    /// recall guarantee for the fused message page. Candidate state is O(k),
    /// not O(k * shards); the retained graphs themselves are not memory-capped.
    pub(super) fn search(
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
                .map_err(|error| anyhow!("native ANN shard {ordinal} search failed: {error}"))?;
            if measured.dimension != dimension || hits.len() > candidate {
                bail!("native ANN shard returned inconsistent candidate metadata");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::vector_index::Quantization;
    use frankensearch::index::HnswConfig;

    fn doc(message: u64, source: u32) -> String {
        SemanticDocId {
            message_id: message,
            chunk_idx: 0,
            agent_id: 1,
            workspace_id: 2,
            source_id: source,
            role: 1,
            created_at_ms: 100,
            content_hash: None,
        }
        .to_doc_id_string()
    }

    fn shard(dir: &Path, name: &str, records: &[(String, [f32; 2])], config: HnswConfig) -> SemanticIndexArtifact {
        shard_with_config(dir, name, records, config)
    }
