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
                artifact.ann_path().is_none().then_some(SemanticAnnUnavailableReason::SidecarMissing)
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
    ) -> Result<(Vec<VectorSearchResult>, SemanticCandidateRetryState, Option<AnnSearchStats>)> {
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
        let candidate = fetch_limit.saturating_mul(ANN_CANDIDATE_MULTIPLIER).max(fetch_limit);
        let ef = FS_HNSW_DEFAULT_EF_SEARCH.max(candidate);
        let mut stats = AnnSearchStats { dimension, estimated_recall: 1.0, ..Default::default() };
        let mut best_by_message = HashMap::new();
        let mut has_more_candidates = false;
        for (ordinal, graph) in self.graphs.iter().enumerate() {
            let (hits, measured) = graph.knn_search_with_stats(embedding, candidate, ef)
                .map_err(|error| anyhow!("native ANN shard {ordinal} search failed: {error}"))?;
            // A failure above returns no partial success assembled from earlier
            // shards. No graph is skipped because it produced no filtered hits.
            has_more_candidates |= measured.index_size > hits.len();
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
                .into_iter().map(|hit| (hit.message_id, hit)).collect();
        }
        tracing::debug!(shards = self.graphs.len(), native_candidates = stats.k_returned,
            returned_messages = best_by_message.len(), "native ANN shard merge complete");
        Ok((SearchClient::collapse_semantic_results(best_by_message, fetch_limit),
            SemanticCandidateRetryState { has_more_candidates, exact_window_may_omit_competitor: false },
            Some(stats)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::vector_index::Quantization;
    use frankensearch::index::HnswConfig;

    fn doc(message: u64, source: u32) -> String {
        SemanticDocId { message_id: message, chunk_idx: 0, agent_id: 1,
            workspace_id: 2, source_id: source, role: 1, created_at_ms: 100,
            content_hash: None }.to_doc_id_string()
    }

    fn shard(dir: &Path, name: &str, records: &[(String, [f32; 2])]) -> SemanticIndexArtifact {
        let path = dir.join(format!("{name}.fsvi"));
        let ann = dir.join(format!("{name}.chsw"));
        let mut writer = FsVectorIndex::create_with_revision(&path, "fnv1a-2", "ann-shard-test", 2, Quantization::F32).unwrap();
        for (id, vector) in records { writer.write_record(id, vector).unwrap(); }
        writer.finish().unwrap();
        let index = FsVectorIndex::open_read_only(&path).unwrap();
        let graph = FsHnswIndex::build_from_vector_index(&index, HnswConfig::default()).unwrap();
        graph.save(&ann).unwrap();
        SemanticIndexArtifact::open(&path, Some(ann)).unwrap()
    }

    fn records(best: u64, score: f32) -> Vec<(String, [f32; 2])> {
        let mut rows = vec![(doc(best, 3), [score, (1.0 - score * score).sqrt()])];
        rows.extend((0..64).map(|n| (doc(best * 1000 + n, 4), [-1.0, 0.0])));
        rows
    }

    fn ids(hits: &[VectorSearchResult]) -> Vec<u64> {
        hits.iter().map(|hit| hit.message_id).collect()
    }

    fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() { files.extend(snapshot(&path)); }
            else { files.push((path.clone(), std::fs::read(path).unwrap())); }
        }
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    }

    #[test]
    fn native_ann_searches_all_shards_and_aggregates_actual_work() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = Arc::new(vec![shard(dir.path(), "a", &records(1, 0.6)),
            shard(dir.path(), "b", &records(2, 0.9)), shard(dir.path(), "c", &records(3, 0.8))]);
        let before = snapshot(dir.path());
        let set = SemanticAnnShardSet::open(Arc::clone(&artifacts)).unwrap();
        let (hits, retry, stats) = set.search(&artifacts, &[1.0, 0.0], 3, None).unwrap();
        assert_eq!(ids(&hits), vec![2, 3, 1]);
        assert!(retry.has_more_candidates);
        let stats = stats.unwrap();
        assert_eq!(stats.index_size, 195);
        assert_eq!(stats.k_requested, 36);
        assert_eq!(stats.k_returned, 36);
        assert_eq!(stats.dimension, 2);
        assert!(stats.is_approximate);
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn shard_merge_keeps_best_shared_message_and_metadata_scope() {
        let dir = tempfile::tempdir().unwrap();
        let a = [(doc(1, 3), [0.6, 0.8]), (doc(2, 4), [1.0, 0.0]), (doc(3, 3), [0.8, 0.6])];
        let b = [(doc(1, 3), [0.9, 0.4358899]), (doc(4, 3), [0.0, 1.0])];
        let artifacts = Arc::new(vec![shard(dir.path(), "a", &a), shard(dir.path(), "b", &b)]);
        let set = SemanticAnnShardSet::open(Arc::clone(&artifacts)).unwrap();
        let filter = SemanticFilter { agents: Some(HashSet::from([1])), workspaces: Some(HashSet::from([2])),
            sources: Some(HashSet::from([3])), roles: Some(HashSet::from([1])), created_from: Some(100), created_to: Some(100) };
        let (hits, _, stats) = set.search(&artifacts, &[1.0, 0.0], 4, Some(&filter)).unwrap();
        assert_eq!(ids(&hits), vec![1, 3, 4]);
        assert!(hits[0].score > 0.85);
        assert_eq!(stats.unwrap().index_size, 5);
        let reject = SemanticFilter { agents: Some(HashSet::new()), ..filter };
        let (hits, _, stats) = set.search(&artifacts, &[1.0, 0.0], 4, Some(&reject)).unwrap();
        assert!(hits.is_empty());
        assert_eq!(stats.unwrap().index_size, 5, "all graphs still execute");
    }

    #[test]
    fn a_missing_later_pair_rejects_the_whole_cohort() {
        let dir = tempfile::tempdir().unwrap();
        let first = shard(dir.path(), "a", &records(1, 0.6));
        let second = shard(dir.path(), "b", &records(2, 0.9));
        let second = SemanticIndexArtifact::open(second.fsvi_path(), None).unwrap();
        let artifacts = Arc::new(vec![first, second]);
        let before = snapshot(dir.path());
        assert!(matches!(SemanticAnnShardSet::unavailable_reason(&artifacts), Some(SemanticAnnUnavailableReason::SidecarMissing)));
        let error = SemanticAnnShardSet::open(artifacts).err().expect("missing pair must fail");
        assert!(matches!(error.reason, SemanticAnnUnavailableReason::SidecarMissing));
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn swapped_native_sidecar_is_not_accepted_as_a_shard() {
        let dir = tempfile::tempdir().unwrap();
        let first = shard(dir.path(), "a", &records(1, 0.6));
        let second = shard(dir.path(), "b", &records(2, 0.9));
        let wrong = SemanticIndexArtifact::open(second.fsvi_path(), first.ann_path().map(Path::to_path_buf)).unwrap();
        let before = snapshot(dir.path());
        assert!(SemanticAnnShardSet::open(Arc::new(vec![first, wrong])).is_err());
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn identical_doc_ids_do_not_admit_changed_vector_contents() {
        let dir = tempfile::tempdir().unwrap();
        let old = shard(dir.path(), "old", &records(1, 0.6));
        let new = shard(dir.path(), "new", &records(1, 0.9));
        let wrong = SemanticIndexArtifact::open(new.fsvi_path(), old.ann_path().map(Path::to_path_buf)).unwrap();
        let before = snapshot(dir.path());
        assert!(SemanticAnnShardSet::open(Arc::new(vec![wrong])).is_err());
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn loaded_cohort_does_not_reopen_renamed_vector_or_ann_paths() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = Arc::new(vec![shard(dir.path(), "a", &records(1, 0.6)), shard(dir.path(), "b", &records(2, 0.9))]);
        let set = SemanticAnnShardSet::open(Arc::clone(&artifacts)).unwrap();
        let (before_hits, _, _) = set.search(&artifacts, &[1.0, 0.0], 2, None).unwrap();
        for artifact in artifacts.iter() {
            for path in [artifact.fsvi_path(), artifact.ann_path().unwrap()] {
                std::fs::rename(path, path.with_extension(format!("{}-retained", path.extension().unwrap().to_str().unwrap()))).unwrap();
            }
        }
        let before_files = snapshot(dir.path());
        let (after_hits, _, _) = set.search(&artifacts, &[1.0, 0.0], 2, None).unwrap();
        assert_eq!(ids(&after_hits), ids(&before_hits));
        assert_eq!(after_hits.iter().map(|h| h.score.to_bits()).collect::<Vec<_>>(), before_hits.iter().map(|h| h.score.to_bits()).collect::<Vec<_>>());
        assert_eq!(snapshot(dir.path()), before_files);
    }

    #[test]
    fn stale_context_owner_cannot_use_a_cached_graph_cohort() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = Arc::new(vec![shard(dir.path(), "a", &records(1, 0.6))]);
        let set = SemanticAnnShardSet::open(Arc::clone(&artifacts)).unwrap();
        let other = Arc::new(artifacts.as_ref().clone());
        assert!(set.search(&other, &[1.0, 0.0], 2, None).is_err());
    }

    #[test]
    fn invalid_query_cannot_return_a_partial_shard_result() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = Arc::new(vec![shard(dir.path(), "a", &records(1, 0.6))]);
        let set = SemanticAnnShardSet::open(Arc::clone(&artifacts)).unwrap();
        assert!(set.search(&artifacts, &[f32::NAN, 0.0], 2, None).is_err());
        assert!(set.search(&artifacts, &[1.0], 2, None).is_err());
        let (hits, _, stats) = set.search(&artifacts, &[], 0, None).unwrap();
        assert!(hits.is_empty());
        assert!(stats.is_none(), "no ANN execution occurred");
    }

    #[test]
    fn bad_native_metadata_does_not_trigger_a_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let first = shard(dir.path(), "a", &records(1, 0.6));
        let second = shard(dir.path(), "b", &records(2, 0.9));
        let invalid = dir.path().join("invalid.chsw");
        std::fs::write(&invalid, b"not-json").unwrap();
        let second = SemanticIndexArtifact::open(second.fsvi_path(), Some(invalid)).unwrap();
        let before = snapshot(dir.path());
        assert!(SemanticAnnShardSet::open(Arc::new(vec![first, second])).is_err());
        assert_eq!(snapshot(dir.path()), before);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_ann_metadata_is_rejected_without_following_it() {
        let dir = tempfile::tempdir().unwrap();
        let source = shard(dir.path(), "a", &records(1, 0.6));
        let link = dir.path().join("alias.chsw");
        std::os::unix::fs::symlink(source.ann_path().unwrap(), &link).unwrap();
        let artifact = SemanticIndexArtifact::open(source.fsvi_path(), Some(link)).unwrap();
        let error = SemanticAnnShardSet::open(Arc::new(vec![artifact])).err().expect("symlink must fail");
        assert!(matches!(error.reason, SemanticAnnUnavailableReason::SidecarOpenFailed));
    }
}
