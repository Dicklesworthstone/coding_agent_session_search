// Real persisted FSVI/HNSW coverage for mixed plans, using the parent fixtures.
#[test]
fn mixed_native_ordinals_survive_exact_first_and_alternating_shards() {
    let dir = tempfile::tempdir().unwrap();
    let artifacts = (0..4)
        .map(|ordinal| {
            let artifact = shard(
                dir.path(),
                &format!("ordinal-{ordinal}"),
                &[(doc(ordinal + 1, 3), [0.6 + ordinal as f32 * 0.1, 0.0])],
            );
            if ordinal % 2 == 0 {
                SemanticIndexArtifact::open(artifact.fsvi_path(), None).unwrap()
            } else {
                artifact
            }
        })
        .collect();
    let context = recovery_context(Arc::new(artifacts));
    let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
    assert_eq!(set.native_ordinals, vec![1, 3]);
    let before = snapshot(dir.path());
    for k in [1, 2, 4, 8] {
        let (expected, _) =
            SearchClient::search_exact_semantic_indexes(&context, &[1.0, 0.0], k, None).unwrap();
        let (hits, _, stats) = set
            .search_with_exact_fallback(&context, &[1.0, 0.0], k, None)
            .unwrap();
        assert_eq!(ids(&hits), ids(&expected[..expected.len().min(k)]));
        let stats = stats.unwrap();
        assert_eq!(stats.index_size, 2);
        let receipt = stats.partial_exact.as_ref().unwrap();
        assert_eq!(
            receipt
                .shards
                .iter()
                .map(|part| part.shard)
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert!(
            receipt
                .shards
                .iter()
                .all(|part| part.reason == SemanticAnnUnavailableReason::SidecarMissing)
        );
        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(
            json["partial_exact"]["shards"][0]["reason"],
            "sidecar_missing"
        );
        assert!(json.get("exact_fallback").is_none());
    }
    assert_eq!(snapshot(dir.path()), before);
}

#[test]
fn mixed_corrupt_accelerator_cannot_hide_its_exact_winner() {
    let dir = tempfile::tempdir().unwrap();
    let ready = shard(dir.path(), "healthy", &[(doc(1, 3), [0.6, 0.8])]);
    let broken = shard(dir.path(), "broken", &[(doc(2, 3), [1.0, 0.0])]);
    std::fs::write(broken.ann_path().unwrap(), b"not native metadata").unwrap();
    let context = recovery_context(Arc::new(vec![ready, broken]));
    let before = snapshot(dir.path());
    let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
    assert_eq!(set.native_ordinals, vec![0]);
    assert_eq!(set.exact_shards.len(), 1);
    let (hits, _, stats) = set
        .search_with_exact_fallback(&context, &[1.0, 0.0], 1, None)
        .unwrap();
    assert_eq!(ids(&hits), vec![2]);
    assert_eq!(hits[0].score.to_bits(), 1.0_f32.to_bits());
    let stats = stats.unwrap();
    assert_eq!(stats.index_size, 1);
    assert_eq!(stats.partial_exact.unwrap().shards[0].shard, 1);
    assert_eq!(snapshot(dir.path()), before);
}

#[test]
fn mixed_exact_partition_refines_chunk_dominated_message_windows() {
    let dir = tempfile::tempdir().unwrap();
    let mut rows = (0..80)
        .map(|chunk| {
            let mut identity = parse_semantic_doc_id(&doc(1, 3)).unwrap();
            identity.chunk_idx = chunk;
            (identity.to_doc_id_string(), [0.9, 0.0])
        })
        .collect::<Vec<_>>();
    rows.push((doc(2, 3), [0.8, 0.0]));
    let exact = shard(dir.path(), "chunks", &rows);
    let exact = SemanticIndexArtifact::open(exact.fsvi_path(), None).unwrap();
    let native = shard(dir.path(), "one-native", &[(doc(3, 3), [0.6, 0.0])]);
    let context = recovery_context(Arc::new(vec![exact, native]));
    let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
    let before = snapshot(dir.path());
    let (hits, _, stats) = set
        .search_with_exact_fallback(&context, &[1.0, 0.0], 3, None)
        .unwrap();
    assert_eq!(ids(&hits), vec![1, 2, 3]);
    let stats = stats.unwrap();
    assert_eq!(stats.index_size, 1);
    assert_eq!(stats.partial_exact.unwrap().returned_messages, 2);
    assert!(stats.exact_fallback.is_none());
    assert_eq!(snapshot(dir.path()), before);
}

#[test]
fn mixed_exact_page_does_not_bypass_native_refills_or_repeat_exact_scoring() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountExact {
        calls: AtomicUsize,
        base: SemanticFilter,
    }
    impl FsSearchFilter for CountExact {
        fn name(&self) -> &str {
            "count_exact_partition"
        }
        fn matches(&self, id: &str, metadata: Option<&serde_json::Value>) -> bool {
            if parse_semantic_doc_id(id).unwrap().message_id == 2 {
                self.calls.fetch_add(1, Ordering::Relaxed);
            }
            self.base.matches(id, metadata)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let native = selective_shard(dir.path(), "native-refill", 1, 0.9);
    let exact = shard(dir.path(), "exact-once", &[(doc(2, 3), [0.6, 0.8])]);
    let exact = SemanticIndexArtifact::open(exact.fsvi_path(), None).unwrap();
    let filter = CountExact {
        calls: AtomicUsize::new(0),
        base: selected_source(),
    };
    let control = recovery_context(Arc::new(vec![exact.clone()]));
    SearchClient::search_exact_semantic_indexes(&control, &[1.0, 0.0], 1, Some(&filter)).unwrap();
    let expected_calls = filter.calls.swap(0, Ordering::Relaxed);
    assert!(expected_calls > 0);
    let context = recovery_context(Arc::new(vec![native, exact]));
    let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
    let (hits, _, stats) = set
        .search_with_exact_fallback(&context, &[1.0, 0.0], 1, Some(&filter))
        .unwrap();
    assert_eq!(
        ids(&hits),
        vec![1],
        "exact's lower score cannot end native refinement early"
    );
    assert_eq!(
        filter.calls.load(Ordering::Relaxed),
        expected_calls,
        "score exact subset just once"
    );
    let stats = stats.unwrap();
    assert_eq!(stats.index_size, 41);
    assert!(
        stats.k_requested > 4,
        "the filtered native window must refill"
    );
    assert!(stats.exact_fallback.is_none());
    assert_eq!(stats.partial_exact.unwrap().returned_messages, 1);
}

#[test]
fn mixed_cohort_retains_both_owners_after_backing_paths_are_renamed() {
    let dir = tempfile::tempdir().unwrap();
    let native = shard(dir.path(), "retained-native", &[(doc(1, 3), [0.6, 0.8])]);
    let exact = shard(dir.path(), "retained-exact", &[(doc(2, 3), [1.0, 0.0])]);
    let exact = SemanticIndexArtifact::open(exact.fsvi_path(), None).unwrap();
    let context = recovery_context(Arc::new(vec![exact, native]));
    let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
    for artifact in context.artifacts.iter() {
        std::fs::rename(
            artifact.fsvi_path(),
            artifact.fsvi_path().with_extension("parked-fsvi"),
        )
        .unwrap();
        if let Some(path) = artifact.ann_path() {
            std::fs::rename(path, path.with_extension("parked-ann")).unwrap();
        }
    }
    let before = snapshot(dir.path());
    let (hits, _, stats) = set
        .search_with_exact_fallback(&context, &[1.0, 0.0], 2, None)
        .unwrap();
    assert_eq!(ids(&hits), vec![2, 1]);
    assert_eq!(stats.unwrap().partial_exact.unwrap().shards[0].shard, 0);
    assert_eq!(snapshot(dir.path()), before);
}

#[test]
fn mixed_zero_limit_and_wrong_context_never_visit_either_partition() {
    struct NoVisits;
    impl FsSearchFilter for NoVisits {
        fn name(&self) -> &str {
            "refuse_unadmitted_visits"
        }
        fn matches(&self, _: &str, _: Option<&serde_json::Value>) -> bool {
            panic!("no query admitted");
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let native = shard(dir.path(), "guard-native", &[(doc(1, 3), [0.6, 0.8])]);
    let exact = shard(dir.path(), "guard-exact", &[(doc(2, 3), [1.0, 0.0])]);
    let exact = SemanticIndexArtifact::open(exact.fsvi_path(), None).unwrap();
    let context = recovery_context(Arc::new(vec![native, exact]));
    let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
    let (hits, _, stats) = set
        .search_with_exact_fallback(&context, &[f32::NAN], 0, Some(&NoVisits))
        .unwrap();
    assert!(hits.is_empty() && stats.is_none());
    for vector in [vec![1.0], vec![f32::NAN, 0.0]] {
        assert!(
            set.search_with_exact_fallback(&context, &vector, 1, Some(&NoVisits))
                .is_err()
        );
    }
    let foreign = recovery_context(Arc::new(context.artifacts.as_ref().clone()));
    assert!(
        set.search_with_exact_fallback(&foreign, &[1.0, 0.0], 0, Some(&NoVisits))
            .is_err()
    );
}

#[test]
fn mixed_runtime_failure_discards_native_prefix_and_reports_both_exact_operations() {
    let dir = tempfile::tempdir().unwrap();
    let exact = shard(dir.path(), "fallback-exact", &[(doc(1, 3), [0.6, 0.8])]);
    let exact = SemanticIndexArtifact::open(exact.fsvi_path(), None).unwrap();
    let native = shard(dir.path(), "fallback-native", &[(doc(2, 3), [0.8, 0.6])]);
    let failed = shard(dir.path(), "fallback-failed", &[(doc(3, 3), [1.0, 0.0])]);
    let wrong = shard(
        dir.path(),
        "fallback-wrong",
        &[(doc(8, 3), [0.0, 1.0]), (doc(9, 3), [0.6, 0.8])],
    );
    let context = recovery_context(Arc::new(vec![exact, native, failed]));
    let mut set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
    // Fault injection uses a real, incompatible native graph. Production never admits it.
    set.graphs[1] = open_fs_semantic_ann_index(wrong.index(), wrong.ann_path().unwrap()).unwrap();
    let before = snapshot(dir.path());
    let (hits, _, stats) = set
        .search_with_exact_fallback(&context, &[1.0, 0.0], 3, None)
        .unwrap();
    assert_eq!(ids(&hits), vec![3, 2, 1]);
    let stats = stats.unwrap();
    assert!(!stats.is_approximate);
    assert_eq!(
        stats.index_size, 1,
        "only the first completed native call is counted"
    );
    assert_eq!(stats.partial_exact.unwrap().shards[0].shard, 0);
    let fallback = stats.exact_fallback.unwrap();
    assert_eq!(fallback.reason, AnnExactFallbackReason::NativeSearchFailed);
    assert_eq!(fallback.shard_count, 3);
    assert_eq!(snapshot(dir.path()), before);
}

#[test]
fn mixed_exact_wal_does_not_spend_the_native_overlay_budget() {
    let dir = tempfile::tempdir().unwrap();
    let exact = shard(dir.path(), "large-exact-wal", &[(doc(1, 3), [-1.0, 0.0])]);
    let path = exact.fsvi_path().to_owned();
    drop(exact);
    let mut writer = FsVectorIndex::open_writer(&path).unwrap();
    let updates = (0..4097)
        .map(|index| (doc(10_000 + index, 3), vec![1.0, 0.0]))
        .collect::<Vec<_>>();
    writer.append_batch(&updates).unwrap();
    drop(writer);
    let exact = SemanticIndexArtifact::open(path, None).unwrap();
    assert_eq!(exact.index().wal_record_count(), 4097);
    let native = shard(dir.path(), "small-native", &[(doc(2, 3), [0.6, 0.8])]);
    let context = recovery_context(Arc::new(vec![exact, native]));
    let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
    let before = snapshot(dir.path());
    let (expected, _) =
        SearchClient::search_exact_semantic_indexes(&context, &[1.0, 0.0], 1, None).unwrap();
    let (hits, _, stats) = set
        .search_with_exact_fallback(&context, &[1.0, 0.0], 1, None)
        .unwrap();
    assert_eq!(ids(&hits), ids(&expected[..1]));
    let stats = stats.unwrap();
    assert_eq!(stats.index_size, 1);
    assert!(stats.k_requested > 0);
    assert!(
        stats.exact_fallback.is_none(),
        "exact-only WAL is not native overlay work"
    );
    assert_eq!(stats.partial_exact.unwrap().shards[0].shard, 0);
    assert_eq!(snapshot(dir.path()), before);
}

#[test]
#[ignore = "paired synthetic search timing; execute explicitly"]
fn mixed_cohort_paired_search_measurement() {
    use std::time::Instant;
    let dir = tempfile::tempdir().unwrap();
    let mut artifacts = Vec::new();
    for ordinal in 0..8_u64 {
        let rows = (0..2048_u64)
            .map(|index| {
                let angle = 0.003 + (ordinal + index * 8) as f32 * 0.00015;
                (
                    doc(ordinal * 10_000 + index + 1, 3),
                    [angle.cos(), angle.sin()],
                )
            })
            .collect::<Vec<_>>();
        let artifact = shard_with_config(
            dir.path(),
            &format!("paired-{ordinal}"),
            &rows,
            HnswConfig {
                m: 32,
                ..Default::default()
            },
        );
        artifacts.push(if ordinal == 0 {
            SemanticIndexArtifact::open(artifact.fsvi_path(), None).unwrap()
        } else {
            artifact
        });
    }
    let context = recovery_context(Arc::new(artifacts));
    let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
    let signature = |hits: &[VectorSearchResult]| {
        hits.iter()
            .take(8)
            .map(|hit| (hit.message_id, hit.score.to_bits()))
            .collect::<Vec<_>>()
    };
    let (reference, _) =
        SearchClient::search_exact_semantic_indexes(&context, &[1.0, 0.0], 8, None).unwrap();
    let mut exact_us = Vec::new();
    let mut mixed_us = Vec::new();
    for round in 0..10 {
        for mixed in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let started = Instant::now();
            let hits = if mixed {
                let (hits, _, stats) = set
                    .search_with_exact_fallback(&context, &[1.0, 0.0], 8, None)
                    .unwrap();
                mixed_us.push(started.elapsed().as_micros());
                let stats = stats.unwrap();
                assert_eq!(stats.index_size, 7 * 2048);
                assert_eq!(stats.partial_exact.unwrap().shards.len(), 1);
                assert!(stats.exact_fallback.is_none());
                hits
            } else {
                let (hits, _) =
                    SearchClient::search_exact_semantic_indexes(&context, &[1.0, 0.0], 8, None)
                        .unwrap();
                exact_us.push(started.elapsed().as_micros());
                hits
            };
            assert_eq!(
                signature(&hits),
                signature(&reference),
                "paired ranking must agree"
            );
            std::hint::black_box(hits);
        }
    }
    eprintln!(
        "MIXED_ANN_PAIRED vectors=16384 native_shards=7 exact_shards=1 k=8 exact_us={exact_us:?} mixed_us={mixed_us:?}"
    );
}

#[test]
fn mixed_equal_vectors_have_equal_scores_and_stable_message_order() {
    let dir = tempfile::tempdir().unwrap();
    let native = shard(dir.path(), "tie-native", &[(doc(1, 3), [0.8, 0.6])]);
    let exact = shard(dir.path(), "tie-exact", &[(doc(2, 3), [0.8, 0.6])]);
    let exact = SemanticIndexArtifact::open(exact.fsvi_path(), None).unwrap();
    let context = recovery_context(Arc::new(vec![native, exact]));
    let set = SemanticAnnShardSet::open(Arc::clone(&context.artifacts)).unwrap();
    let (hits, _, stats) = set
        .search_with_exact_fallback(&context, &[1.0, 0.0], 2, None)
        .unwrap();
    assert_eq!(ids(&hits), vec![1, 2]);
    assert_eq!(hits[0].score.to_bits(), hits[1].score.to_bits());
    assert_eq!(hits[0].score.to_bits(), 0.8_f32.to_bits());
    assert_eq!(stats.unwrap().candidate_rescore.unwrap().candidates, 1);
}
