//! Native ANN over every explicitly paired shard of one retained vector cohort.
//!
//! Admission is all-or-nothing: an unavailable sidecar never silently removes
//! its source shard. The caller falls back to the complete exact cohort. Native
//! loading uses the existing no-rebuild admission function; no query writes or
//! rediscovers an artifact. Graph admission can still read all source vectors.

use super::*;
use crate::search::ann_index::{AnnExactFallbackReason, AnnExactFallbackStats, AnnSearchStats};

mod execution;
mod refinement;
use refinement::{NativeRefillBudget, accumulate_native_stats};

pub(super) struct SemanticAnnShardSet {
    artifacts: Arc<Vec<SemanticIndexArtifact>>,
    graphs: Vec<FsHnswIndex>,
}

/// Only errors after query/owner validation may degrade to exact retrieval.
/// Keep completed native measurements, but never carry partial hits or raw
/// backend diagnostics across the recovery boundary.
#[derive(Debug)]
struct NativeAnnSearchFailure {
    shard: usize,
    completed_stats: AnnSearchStats,
}

impl NativeAnnSearchFailure {
    fn new(shard: usize, mut completed_stats: AnnSearchStats) -> Self {
        // An incomplete native cohort has no meaningful recall estimate.
        completed_stats.estimated_recall = 0.0;
        Self {
            shard,
            completed_stats,
        }
    }
}

impl std::fmt::Display for NativeAnnSearchFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "native ANN shard {} search failed", self.shard)
    }
}

impl std::error::Error for NativeAnnSearchFailure {}

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
}

#[cfg(test)]
mod wal_tests;

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

    fn shard(dir: &Path, name: &str, records: &[(String, [f32; 2])]) -> SemanticIndexArtifact {
        shard_with_config(dir, name, records, HnswConfig::default())
    }

    fn shard_with_config(
        dir: &Path,
        name: &str,
        records: &[(String, [f32; 2])],
        config: HnswConfig,
    ) -> SemanticIndexArtifact {
        let path = dir.join(format!("{name}.fsvi"));
        let ann = dir.join(format!("{name}.chsw"));
        let mut writer = FsVectorIndex::create_with_revision(
            &path,
            "fnv1a-2",
            "ann-shard-test",
            2,
            Quantization::F32,
        )
        .unwrap();
        for (id, vector) in records {
            writer.write_record(id, vector).unwrap();
        }
        writer.finish().unwrap();
        let index = FsVectorIndex::open_read_only(&path).unwrap();
        let graph = FsHnswIndex::build_from_vector_index(&index, config).unwrap();
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
            if path.is_dir() {
                files.extend(snapshot(&path));
            } else {
                files.push((path.clone(), std::fs::read(path).unwrap()));
            }
        }
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    }

    fn recovery_context(artifacts: Arc<Vec<SemanticIndexArtifact>>) -> SemanticCandidateContext {
        SemanticCandidateContext {
            artifacts,
            filter_maps: SemanticFilterMaps::for_tests(
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashSet::new(),
            ),
            roles: None,
        }
    }

    fn selective_shard(dir: &Path, name: &str, message: u64, score: f32) -> SemanticIndexArtifact {
        let mut rows = (0..40)
            .map(|n| (doc(message * 1000 + n, 4), [1.0, 0.0]))
            .collect::<Vec<_>>();
        rows.push((doc(message, 3), [score, (1.0 - score * score).sqrt()]));
        // Fully connect this small fixture so its native candidate window is
        // deterministic. This test measures filtering, not probabilistic recall.
        shard_with_config(
            dir,
            name,
            &rows,
            HnswConfig {
                m: 64,
                ..Default::default()
            },
        )
    }

    fn selected_source() -> SemanticFilter {
        SemanticFilter {
            agents: Some(HashSet::from([1])),
            workspaces: Some(HashSet::from([2])),
            sources: Some(HashSet::from([3])),
            roles: Some(HashSet::from([1])),
            created_from: Some(100),
            created_to: Some(100),
        }
    }

    #[test]
    fn filtered_native_underfill_refills_every_shard_before_exact_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let context = recovery_context(Arc::new(vec![
            selective_shard(dir.path(), "select-a", 1, 0.6),
            selective_shard(dir.path(), "select-b", 2, 0.8),
        ]));
        let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
        let before = snapshot(dir.path());
        let filter = selected_source();
        // Both incumbent windows lose every selected-source message.
        for limit in [2, 6] {
            let (native, retry, _) = set
                .search(&context.artifacts, &[1.0, 0.0], limit, Some(&filter))
                .unwrap();
            assert!(native.is_empty());
            assert!(retry.has_more_candidates);
        }
        let (hits, retry, stats) = set
            .search_with_exact_fallback(&context, &[1.0, 0.0], 2, Some(&filter))
            .unwrap();
        assert_eq!(ids(&hits), vec![2, 1]);
        assert!(!retry.exact_window_may_omit_competitor);
        let stats = stats.unwrap();
        assert_eq!(stats.index_size, 82, "count the retained cohort only once");
        // Per shard: 8 initial candidates, 32 on the first refill, and all 41
        // on the second. These are real backend requests, not result counts.
        assert_eq!(stats.k_requested, 2 * (8 + 32 + 41));
        assert!(stats.exact_fallback.is_none());
        assert!(
            serde_json::to_value(&stats)
                .unwrap()
                .get("exact_fallback")
                .is_none()
        );
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn native_chunk_dominance_recovers_distinct_messages_without_metadata_filters() {
        let dir = tempfile::tempdir().unwrap();
        let mut rows = (0..40)
            .map(|chunk| {
                let mut identity = parse_semantic_doc_id(&doc(1, 3)).unwrap();
                identity.chunk_idx = chunk;
                (identity.to_doc_id_string(), [1.0, 0.0])
            })
            .collect::<Vec<_>>();
        rows.extend([(doc(2, 3), [0.8, 0.6]), (doc(3, 3), [0.6, 0.8])]);
        let context = recovery_context(Arc::new(vec![shard_with_config(
            dir.path(),
            "chunks",
            &rows,
            HnswConfig {
                m: 64,
                ..Default::default()
            },
        )]));
        let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
        let (native, retry, _) = set
            .search(&context.artifacts, &[1.0, 0.0], 3, None)
            .unwrap();
        assert_eq!(ids(&native), vec![1]);
        assert!(retry.has_more_candidates);
        let (hits, _, stats) = set
            .search_with_exact_fallback(&context, &[1.0, 0.0], 3, None)
            .unwrap();
        assert_eq!(ids(&hits), vec![1, 2, 3]);
        let stats = stats.unwrap();
        assert!(stats.exact_fallback.is_none());
        assert_eq!(stats.index_size, 42);
        assert_eq!(stats.k_requested, 12 + 42);
    }

    #[test]
    fn filled_native_page_keeps_original_scores_and_avoids_exact_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let context = recovery_context(Arc::new(vec![selective_shard(
            dir.path(),
            "filled",
            1,
            0.8,
        )]));
        let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
        let (native, native_retry, native_stats) = set
            .search(&context.artifacts, &[1.0, 0.0], 2, None)
            .unwrap();
        let (actual, retry, stats) = set
            .search_with_exact_fallback(&context, &[1.0, 0.0], 2, None)
            .unwrap();
        let signature = |hits: &[VectorSearchResult]| {
            hits.iter()
                .map(|hit| (hit.message_id, hit.chunk_idx, hit.score.to_bits()))
                .collect::<Vec<_>>()
        };
        assert_eq!(signature(&actual), signature(&native));
        assert_eq!(retry.has_more_candidates, native_retry.has_more_candidates);
        let stats = stats.unwrap();
        assert_eq!(stats.k_requested, native_stats.unwrap().k_requested);
        assert!(stats.exact_fallback.is_none());
        assert!(
            serde_json::to_value(&stats)
                .unwrap()
                .get("exact_fallback")
                .is_none()
        );
        let (hits, _, stats) = set
            .search_with_exact_fallback(&context, &[], 0, None)
            .unwrap();
        assert!(hits.is_empty());
        assert!(
            stats.is_none(),
            "zero target performs no native or exact query"
        );
    }

    #[test]
    fn truly_exhausted_native_scope_does_not_trigger_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let context = recovery_context(Arc::new(vec![shard(
            dir.path(),
            "exhausted",
            &[(doc(1, 4), [1.0, 0.0])],
        )]));
        let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
        let filter = selected_source();
        let (hits, retry, stats) = set
            .search_with_exact_fallback(&context, &[1.0, 0.0], 2, Some(&filter))
            .unwrap();
        assert!(hits.is_empty());
        assert!(!retry.has_more_candidates);
        assert!(stats.unwrap().exact_fallback.is_none());
    }

    #[test]
    fn native_refinement_keeps_empty_filters_empty_and_rejects_wrong_owners() {
        let dir = tempfile::tempdir().unwrap();
        let context =
            recovery_context(Arc::new(vec![selective_shard(dir.path(), "empty", 1, 0.8)]));
        let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
        let filter = SemanticFilter {
            sources: Some(HashSet::new()),
            ..selected_source()
        };
        let (hits, _, stats) = set
            .search_with_exact_fallback(&context, &[1.0, 0.0], 2, Some(&filter))
            .unwrap();
        assert!(hits.is_empty());
        assert!(stats.unwrap().exact_fallback.is_none());
        let other = recovery_context(Arc::new(context.artifacts.as_ref().clone()));
        assert!(
            set.search_with_exact_fallback(&other, &[1.0, 0.0], 2, Some(&filter))
                .is_err()
        );
        assert!(
            set.search_with_exact_fallback(&context, &[f32::NAN, 0.0], 2, Some(&filter))
                .is_err()
        );
    }

    #[test]
    fn native_refinement_uses_retained_readers_after_paths_are_renamed() {
        let dir = tempfile::tempdir().unwrap();
        let context = recovery_context(Arc::new(vec![selective_shard(
            dir.path(),
            "retained",
            1,
            0.8,
        )]));
        let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
        for artifact in context.artifacts.iter() {
            for path in [artifact.fsvi_path(), artifact.ann_path().unwrap()] {
                std::fs::rename(
                    path,
                    path.with_extension(format!(
                        "{}-retained",
                        path.extension().unwrap().to_str().unwrap()
                    )),
                )
                .unwrap();
            }
        }
        let before = snapshot(dir.path());
        let filter = selected_source();
        let (hits, _, stats) = set
            .search_with_exact_fallback(&context, &[1.0, 0.0], 2, Some(&filter))
            .unwrap();
        assert_eq!(ids(&hits), vec![1]);
        assert!(stats.unwrap().exact_fallback.is_none());
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn refill_call_budget_is_shared_and_still_recovers_the_complete_exact_cohort() {
        let dir = tempfile::tempdir().unwrap();
        // Seventeen shards admit one extra full-cohort pass, not two: the
        // shared allowance is 32 graph calls. Neither 8 nor 32 candidates
        // reaches the selected source behind 40 unrelated messages.
        let artifacts = (1..=17)
            .map(|message| {
                selective_shard(dir.path(), &format!("budget-{message}"), message, 0.8)
            })
            .collect();
        let context = recovery_context(Arc::new(artifacts));
        let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
        let filter = selected_source();
        let before = snapshot(dir.path());
        let (expected, expected_retry) =
            SearchClient::search_exact_semantic_indexes(&context, &[1.0, 0.0], 2, Some(&filter))
                .unwrap();
        let (hits, retry, stats) = set
            .search_with_exact_fallback(&context, &[1.0, 0.0], 2, Some(&filter))
            .unwrap();
        assert_eq!(ids(&hits), ids(&expected));
        // Exact recovery retains the hydration overfetch window, unlike the
        // bounded native page. Preserve the entire exact candidate cohort.
        assert_eq!(hits.len(), 8);
        assert_eq!(retry.has_more_candidates, expected_retry.has_more_candidates);
        assert!(!retry.exact_window_may_omit_competitor);
        let stats = stats.unwrap();
        assert_eq!(stats.index_size, 17 * 41);
        assert_eq!(stats.k_requested, 17 * (8 + 32));
        assert!(!stats.is_approximate);
        let receipt = stats.exact_fallback.unwrap();
        assert_eq!(
            receipt.reason,
            AnnExactFallbackReason::FilteredCandidateUnderfill
        );
        assert_eq!(receipt.shard_count, 17);
        assert_eq!(receipt.returned_messages, hits.len());
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn refined_pages_do_not_erase_pagination_when_the_last_native_pass_is_exhaustive() {
        let dir = tempfile::tempdir().unwrap();
        let mut rows = (0..40)
            .map(|n| (doc(1000 + n, 4), [1.0, 0.0]))
            .collect::<Vec<_>>();
        rows.extend([
            (doc(1, 3), [0.8, 0.6]),
            (doc(2, 3), [0.6, 0.8]),
            (doc(3, 3), [0.0, 1.0]),
        ]);
        let context = recovery_context(Arc::new(vec![shard_with_config(
            dir.path(),
            "pagination",
            &rows,
            HnswConfig {
                m: 64,
                ..Default::default()
            },
        )]));
        let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
        let (hits, retry, stats) = set
            .search_with_exact_fallback(&context, &[1.0, 0.0], 2, Some(&selected_source()))
            .unwrap();
        assert_eq!(ids(&hits), vec![1, 2]);
        assert!(
            retry.has_more_candidates,
            "message 3 still exists beyond this page"
        );
        assert!(stats.unwrap().exact_fallback.is_none());
    }

    #[test]
    fn exhaustive_initial_native_windows_keep_omitted_message_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let rows = [
            (doc(1, 3), [1.0, 0.0]),
            (doc(2, 3), [0.8, 0.6]),
            (doc(3, 3), [0.6, 0.8]),
        ];
        for split in [false, true] {
            let artifacts = if split {
                vec![
                    shard(dir.path(), "page-a", &rows[..1]),
                    shard(dir.path(), "page-b", &rows[1..]),
                ]
            } else {
                vec![shard(dir.path(), "page-all", &rows)]
            };
            let context = recovery_context(Arc::new(artifacts));
            let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
            let (hits, retry, stats) = set
                .search_with_exact_fallback(&context, &[1.0, 0.0], 1, None)
                .unwrap();
            assert_eq!(ids(&hits), vec![1]);
            assert!(retry.has_more_candidates, "two eligible messages were omitted");
            let stats = stats.unwrap();
            assert_eq!(stats.k_requested, 3, "one exhaustive native pass is enough");
            assert!(stats.exact_fallback.is_none());
        }
    }

    #[test]
    fn native_ann_searches_all_shards_and_aggregates_actual_work() {
        let dir = tempfile::tempdir().unwrap();
        // This exact-winner assertion uses a fully connected small graph.
        // The default M=16 graph can miss the isolated positive point among
        // repeated negatives; the separate differential test below preserves
        // that original corpus without claiming ANN is an exhaustive oracle.
        let config = HnswConfig {
            m: 64,
            ..Default::default()
        };
        let artifacts = Arc::new(vec![
            shard_with_config(dir.path(), "a", &records(1, 0.6), config),
            shard_with_config(dir.path(), "b", &records(2, 0.9), config),
            shard_with_config(dir.path(), "c", &records(3, 0.8), config),
        ]);
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
    fn degenerate_default_graphs_preserve_per_shard_native_results_without_claiming_exact_recall() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = Arc::new(vec![
            shard(dir.path(), "a", &records(1, 0.6)),
            shard(dir.path(), "b", &records(2, 0.9)),
            shard(dir.path(), "c", &records(3, 0.8)),
        ]);
        let set = SemanticAnnShardSet::open(Arc::clone(&artifacts)).unwrap();
        let mut native_reference = HashMap::new();
        let mut exact_reference = HashMap::new();
        let mut actual_raw_count = 0;
        for (ordinal, graph) in set.graphs.iter().enumerate() {
            let (hits, _) = graph
                .knn_search_with_stats_against(artifacts[ordinal].index(), &[1.0, 0.0], 12, 100)
                .unwrap();
            actual_raw_count += hits.len();
            for hit in &hits {
                SearchClient::record_fs_semantic_hit(&mut native_reference, hit);
            }
            for hit in artifacts[ordinal]
                .index()
                .search_top_k(&[1.0, 0.0], 3, None)
                .unwrap()
            {
                SearchClient::record_fs_semantic_hit(&mut exact_reference, &hit);
            }
        }
        let expected = SearchClient::collapse_semantic_results(native_reference, 3);
        let exact = SearchClient::collapse_semantic_results(exact_reference, 3);
        assert_eq!(ids(&exact), vec![2, 3, 1]);
        let (hits, _, stats) = set.search(&artifacts, &[1.0, 0.0], 3, None).unwrap();
        let scored = |hits: &[VectorSearchResult]| {
            hits.iter()
                .map(|hit| (hit.message_id, hit.score.to_bits()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            scored(&hits),
            scored(&expected),
            "cohort merge must lose no per-graph winner"
        );
        assert_eq!(stats.unwrap().k_returned, actual_raw_count);
        let overlap = hits
            .iter()
            .filter(|hit| exact.iter().any(|other| hit.message_id == other.message_id))
            .count();
        println!(
            "DEGENERATE_NATIVE_RECALL top3_overlap={overlap}/3 native={:?} exact={:?}; heuristic is not measured recall",
            ids(&hits),
            ids(&exact)
        );
    }

    #[test]
    fn shard_merge_keeps_best_shared_message_and_metadata_scope() {
        let dir = tempfile::tempdir().unwrap();
        let a = [
            (doc(1, 3), [0.6, 0.8]),
            (doc(2, 4), [1.0, 0.0]),
            (doc(3, 3), [0.8, 0.6]),
        ];
        let b = [(doc(1, 3), [0.9, 0.4358899]), (doc(4, 3), [0.0, 1.0])];
        let artifacts = Arc::new(vec![shard(dir.path(), "a", &a), shard(dir.path(), "b", &b)]);
        let set = SemanticAnnShardSet::open(Arc::clone(&artifacts)).unwrap();
        let filter = SemanticFilter {
            agents: Some(HashSet::from([1])),
            workspaces: Some(HashSet::from([2])),
            sources: Some(HashSet::from([3])),
            roles: Some(HashSet::from([1])),
            created_from: Some(100),
            created_to: Some(100),
        };
        let (hits, _, stats) = set
            .search(&artifacts, &[1.0, 0.0], 4, Some(&filter))
            .unwrap();
        assert_eq!(ids(&hits), vec![1, 3, 4]);
        assert!(hits[0].score > 0.85);
        assert_eq!(stats.unwrap().index_size, 5);
        let reject = SemanticFilter {
            agents: Some(HashSet::new()),
            ..filter
        };
        let (hits, _, stats) = set
            .search(&artifacts, &[1.0, 0.0], 4, Some(&reject))
            .unwrap();
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
        assert!(matches!(
            SemanticAnnShardSet::unavailable_reason(&artifacts),
            Some(SemanticAnnUnavailableReason::SidecarMissing)
        ));
        let error = SemanticAnnShardSet::open(artifacts)
            .err()
            .expect("missing pair must fail");
        assert!(matches!(
            error.reason,
            SemanticAnnUnavailableReason::SidecarMissing
        ));
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn swapped_native_sidecar_is_not_accepted_as_a_shard() {
        let dir = tempfile::tempdir().unwrap();
        let first = shard(dir.path(), "a", &records(1, 0.6));
        let second = shard(dir.path(), "b", &records(2, 0.9));
        let wrong = SemanticIndexArtifact::open(
            second.fsvi_path(),
            first.ann_path().map(Path::to_path_buf),
        )
        .unwrap();
        let before = snapshot(dir.path());
        assert!(SemanticAnnShardSet::open(Arc::new(vec![first, wrong])).is_err());
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn identical_doc_ids_do_not_admit_changed_vector_contents() {
        let dir = tempfile::tempdir().unwrap();
        let old = shard(dir.path(), "old", &records(1, 0.6));
        let new = shard(dir.path(), "new", &records(1, 0.9));
        let wrong =
            SemanticIndexArtifact::open(new.fsvi_path(), old.ann_path().map(Path::to_path_buf))
                .unwrap();
        let before = snapshot(dir.path());
        assert!(SemanticAnnShardSet::open(Arc::new(vec![wrong])).is_err());
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn loaded_cohort_does_not_reopen_renamed_vector_or_ann_paths() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = Arc::new(vec![
            shard(dir.path(), "a", &records(1, 0.6)),
            shard(dir.path(), "b", &records(2, 0.9)),
        ]);
        let set = SemanticAnnShardSet::open(Arc::clone(&artifacts)).unwrap();
        let (before_hits, _, _) = set.search(&artifacts, &[1.0, 0.0], 2, None).unwrap();
        for artifact in artifacts.iter() {
            for path in [artifact.fsvi_path(), artifact.ann_path().unwrap()] {
                std::fs::rename(
                    path,
                    path.with_extension(format!(
                        "{}-retained",
                        path.extension().unwrap().to_str().unwrap()
                    )),
                )
                .unwrap();
            }
        }
        let before_files = snapshot(dir.path());
        let (after_hits, _, _) = set.search(&artifacts, &[1.0, 0.0], 2, None).unwrap();
        assert_eq!(ids(&after_hits), ids(&before_hits));
        assert_eq!(
            after_hits
                .iter()
                .map(|h| h.score.to_bits())
                .collect::<Vec<_>>(),
            before_hits
                .iter()
                .map(|h| h.score.to_bits())
                .collect::<Vec<_>>()
        );
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

    #[test]
    fn each_shard_bounds_its_requested_candidates_by_its_graph_size() {
        let dir = tempfile::tempdir().unwrap();
        let a = [(doc(1, 3), [1.0, 0.0])];
        let b = [
            (doc(2, 3), [0.8, 0.6]),
            (doc(3, 3), [0.6, 0.8]),
            (doc(4, 3), [0.0, 1.0]),
        ];
        let artifacts = Arc::new(vec![
            shard(dir.path(), "bounded-a", &a),
            shard(dir.path(), "bounded-b", &b),
        ]);
        let set = SemanticAnnShardSet::open(Arc::clone(&artifacts)).unwrap();
        let before = snapshot(dir.path());
        let (hits, retry, stats) = set.search(&artifacts, &[1.0, 0.0], 1_000, None).unwrap();
        assert_eq!(ids(&hits), vec![1, 2, 3, 4]);
        assert!(!retry.has_more_candidates);
        let stats = stats.unwrap();
        assert_eq!(stats.index_size, 4);
        assert_eq!(
            stats.k_requested, 4,
            "request each shard's actual extent, not 4,000 each"
        );
        assert_eq!(stats.k_returned, 4);
        assert_eq!(stats.ef_search, FS_HNSW_DEFAULT_EF_SEARCH);
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn a_later_graph_source_mismatch_returns_no_partial_cohort() {
        let dir = tempfile::tempdir().unwrap();
        let first = shard(dir.path(), "source-a", &[(doc(1, 3), [1.0, 0.0])]);
        let second = shard(dir.path(), "source-b", &[(doc(2, 3), [0.8, 0.6])]);
        let wrong = shard(
            dir.path(),
            "different-source",
            &[(doc(3, 3), [0.6, 0.8]), (doc(4, 3), [0.0, 1.0])],
        );
        let first_graph =
            open_fs_semantic_ann_index(first.index(), first.ann_path().unwrap()).unwrap();
        let wrong_graph =
            open_fs_semantic_ann_index(wrong.index(), wrong.ann_path().unwrap()).unwrap();
        let artifacts = Arc::new(vec![first, second]);
        // Deliberately inject an invalid private pairing to exercise the query
        // boundary. Normal all-or-nothing admission remains unchanged.
        let set = SemanticAnnShardSet {
            artifacts: Arc::clone(&artifacts),
            graphs: vec![first_graph, wrong_graph],
        };
        let before = snapshot(dir.path());
        let error = set.search(&artifacts, &[1.0, 0.0], 3, None).unwrap_err();
        assert!(
            error.to_string().contains("shard 1"),
            "the later graph must be checked: {error}"
        );
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn source_backed_shards_agree_with_independent_native_calls() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = Arc::new(vec![
            shard(dir.path(), "reference-a", &records(1, 0.6)),
            shard(dir.path(), "reference-b", &records(2, 0.9)),
        ]);
        let set = SemanticAnnShardSet::open(Arc::clone(&artifacts)).unwrap();
        let filter = SemanticFilter {
            agents: Some(HashSet::from([1])),
            workspaces: Some(HashSet::from([2])),
            sources: Some(HashSet::from([3])),
            roles: Some(HashSet::from([1])),
            created_from: Some(100),
            created_to: Some(100),
        };
        let mut reference = HashMap::new();
        let mut expected_requested = 0;
        let mut expected_returned = 0;
        let mut expected_approximate = false;
        for (graph, source) in set.graphs.iter().zip(artifacts.iter()) {
            let candidate = (3 * ANN_CANDIDATE_MULTIPLIER).min(graph.len());
            let (hits, measured) = graph
                .knn_search_with_stats_against(
                    source.index(),
                    &[1.0, 0.0],
                    candidate,
                    FS_HNSW_DEFAULT_EF_SEARCH.max(candidate),
                )
                .unwrap();
            expected_requested += measured.k_requested;
            expected_returned += measured.k_returned;
            expected_approximate |= measured.is_approximate;
            for hit in hits {
                if filter.matches(&hit.doc_id, None) {
                    SearchClient::record_fs_semantic_hit(&mut reference, &hit);
                }
            }
        }
        let reference = SearchClient::collapse_semantic_results(reference, 3);
        let (actual, _, stats) = set
            .search(&artifacts, &[1.0, 0.0], 3, Some(&filter))
            .unwrap();
        let signature = |hits: &[VectorSearchResult]| {
            hits.iter()
                .map(|hit| (hit.message_id, hit.chunk_idx, hit.score.to_bits()))
                .collect::<Vec<_>>()
        };
        assert_eq!(signature(&actual), signature(&reference));
        let stats = stats.unwrap();
        assert_eq!(stats.k_requested, expected_requested);
        assert_eq!(stats.k_returned, expected_returned);
        assert_eq!(stats.is_approximate, expected_approximate);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_ann_metadata_is_rejected_without_following_it() {
        let dir = tempfile::tempdir().unwrap();
        let source = shard(dir.path(), "a", &records(1, 0.6));
        let link = dir.path().join("alias.chsw");
        std::os::unix::fs::symlink(source.ann_path().unwrap(), &link).unwrap();
        let artifact = SemanticIndexArtifact::open(source.fsvi_path(), Some(link)).unwrap();
        let error = SemanticAnnShardSet::open(Arc::new(vec![artifact]))
            .err()
            .expect("symlink must fail");
        assert!(matches!(
            error.reason,
            SemanticAnnUnavailableReason::SidecarOpenFailed
        ));
    }

    fn runtime_failure_cohort(
        dir: &Path,
        failed_shard: usize,
    ) -> (SemanticCandidateContext, SemanticAnnShardSet) {
        let context = recovery_context(Arc::new(vec![
            selective_shard(dir, "runtime-a", 1, 0.6),
            selective_shard(dir, "runtime-b", 2, 0.8),
        ]));
        let mut set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
        let wrong = shard(dir, "runtime-wrong", &[(doc(999, 3), [1.0, 0.0])]);
        // Real HNSW source-extent validation fails at query time. No mocked
        // result, artifact mutation, or relaxed production admission is used.
        set.graphs[failed_shard] =
            open_fs_semantic_ann_index(wrong.index(), wrong.ann_path().unwrap()).unwrap();
        (context, set)
    }

    #[test]
    fn runtime_failure_recovers_every_shard_and_reports_only_completed_native_work() {
        for failed_shard in 0..2 {
            let dir = tempfile::tempdir().unwrap();
            let (context, set) = runtime_failure_cohort(dir.path(), failed_shard);
            let filter = selected_source();
            let before = snapshot(dir.path());
            assert!(
                set.search(&context.artifacts, &[1.0, 0.0], 2, Some(&filter))
                    .is_err(),
                "fixture must actually fail in the native backend"
            );
            let (expected, expected_retry) = SearchClient::search_exact_semantic_indexes(
                &context,
                &[1.0, 0.0],
                2,
                Some(&filter),
            )
            .unwrap();
            let (hits, retry, stats) = set
                .search_with_exact_fallback(&context, &[1.0, 0.0], 2, Some(&filter))
                .unwrap();
            let signature = |hits: &[VectorSearchResult]| {
                hits.iter()
                    .map(|hit| (hit.message_id, hit.chunk_idx, hit.score.to_bits()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(ids(&hits), vec![2, 1]);
            assert_eq!(signature(&hits), signature(&expected));
            assert_eq!(retry.has_more_candidates, expected_retry.has_more_candidates);
            assert_eq!(
                retry.exact_window_may_omit_competitor,
                expected_retry.exact_window_may_omit_competitor
            );
            let stats = stats.unwrap();
            assert!(!stats.is_approximate);
            assert_eq!(stats.estimated_recall, 0.0);
            assert_eq!(stats.index_size, failed_shard * 41);
            assert_eq!(stats.k_requested, failed_shard * 2 * ANN_CANDIDATE_MULTIPLIER);
            let receipt = stats.exact_fallback.as_ref().unwrap();
            assert_eq!(receipt.reason, AnnExactFallbackReason::NativeSearchFailed);
            assert_eq!(receipt.shard_count, 2);
            assert_eq!(receipt.returned_messages, 2);
            let json = serde_json::to_value(&stats).unwrap();
            assert_eq!(json["exact_fallback"]["reason"], "native_search_failed");
            assert_eq!(json["is_approximate"], false);
            assert!(!json.to_string().contains("runtime-wrong"));
            assert_eq!(snapshot(dir.path()), before);
        }
    }

    #[test]
    fn runtime_failure_never_widens_empty_scope_or_recovers_invalid_requests() {
        let dir = tempfile::tempdir().unwrap();
        let (context, set) = runtime_failure_cohort(dir.path(), 0);
        let filter = SemanticFilter {
            sources: Some(HashSet::new()),
            ..selected_source()
        };
        let before = snapshot(dir.path());
        let (hits, _, stats) = set
            .search_with_exact_fallback(&context, &[1.0, 0.0], 2, Some(&filter))
            .unwrap();
        assert!(hits.is_empty());
        assert_eq!(stats.unwrap().exact_fallback.unwrap().returned_messages, 0);
        for embedding in [
            &[1.0][..],
            &[f32::NAN, 0.0][..],
            &[f32::INFINITY, 0.0][..],
            &[][..],
        ] {
            let error = set
                .search_with_exact_fallback(&context, embedding, 2, Some(&filter))
                .unwrap_err();
            assert!(error.to_string().contains("admitted dimension"));
        }
        let other = recovery_context(Arc::new(context.artifacts.as_ref().clone()));
        for limit in [0, 2] {
            let error = set
                .search_with_exact_fallback(&other, &[1.0, 0.0], limit, None)
                .unwrap_err();
            assert!(error.to_string().contains("cohort does not match"));
        }
        let (hits, _, stats) = set
            .search_with_exact_fallback(&context, &[], 0, None)
            .unwrap();
        assert!(hits.is_empty());
        assert!(stats.is_none(), "zero target must not run either backend");
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn runtime_failure_recovery_uses_retained_readers_after_rename() {
        let dir = tempfile::tempdir().unwrap();
        let (context, set) = runtime_failure_cohort(dir.path(), 1);
        for artifact in context.artifacts.iter() {
            for path in [artifact.fsvi_path(), artifact.ann_path().unwrap()] {
                std::fs::rename(
                    path,
                    path.with_extension(format!(
                        "{}-retained",
                        path.extension().unwrap().to_str().unwrap()
                    )),
                )
                .unwrap();
            }
        }
        let before = snapshot(dir.path());
        let filter = selected_source();
        let (hits, _, stats) = set
            .search_with_exact_fallback(&context, &[1.0, 0.0], 2, Some(&filter))
            .unwrap();
        assert_eq!(ids(&hits), vec![2, 1]);
        assert_eq!(
            stats.unwrap().exact_fallback.unwrap().reason,
            AnnExactFallbackReason::NativeSearchFailed
        );
        assert_eq!(snapshot(dir.path()), before);
    }
}
