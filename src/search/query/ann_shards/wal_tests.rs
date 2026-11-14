//! Durable FSVI WAL regressions through the production ANN execution boundary.
//! No model, mocked graph, or synthetic successful receipt is used.

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

fn shard<const N: usize>(
    directory: &Path,
    name: &str,
    base: &[(String, [f32; N])],
    updates: &[(String, [f32; N])],
) -> SemanticIndexArtifact {
    let path = directory.join(format!("{name}.fsvi"));
    let ann = directory.join(format!("{name}.chsw"));
    let mut writer = FsVectorIndex::create_with_revision(
        &path,
        &format!("fnv1a-{N}"),
        "wal-overlay-regression",
        N,
        Quantization::F32,
    )
    .unwrap();
    for (id, vector) in base {
        writer.write_record(id, vector).unwrap();
    }
    writer.finish().unwrap();
    let original_rows = {
        let source = FsVectorIndex::open_read_only(&path).unwrap();
        FsHnswIndex::build_from_vector_index(
            &source,
            HnswConfig {
                m: 64,
                ..Default::default()
            },
        )
        .unwrap()
        .save(&ann)
        .unwrap();
        (0..source.record_count())
            .map(|row| {
                (
                    source.doc_id_at(row).unwrap().to_owned(),
                    source.vector_at_f32(row).unwrap(),
                )
            })
            .collect::<Vec<_>>()
    };
    let original_main = std::fs::read(&path).unwrap();
    let original_graph = std::fs::read(&ann).unwrap();
    if !updates.is_empty() {
        let mut source = FsVectorIndex::open_writer(&path).unwrap();
        for (id, vector) in updates {
            source
                .append_batch(&[(id.clone(), vector.to_vec())])
                .unwrap();
        }
        assert!(source.wal_record_count() > 0);
    }
    // Replacements durably tombstone old main rows; appending a new ID does
    // not. Neither operation rebuilds the graph or rewrites main vectors.
    if !updates
        .iter()
        .any(|(id, _)| base.iter().any(|(old, _)| old == id))
    {
        assert_eq!(std::fs::read(&path).unwrap(), original_main);
    }
    assert_eq!(std::fs::read(&ann).unwrap(), original_graph);
    let artifact = SemanticIndexArtifact::open(path, Some(ann)).unwrap();
    assert_eq!(artifact.index().record_count(), base.len());
    for (row, (id, vector)) in original_rows.iter().enumerate() {
        assert_eq!(artifact.index().doc_id_at(row).unwrap(), id);
        assert_eq!(artifact.index().vector_at_f32(row).unwrap(), *vector);
        assert_eq!(
            artifact.index().is_deleted(row),
            updates.iter().any(|(updated, _)| updated == id)
        );
    }
    if !updates.is_empty() {
        assert!(
            artifact.index().wal_record_count() > 0,
            "reopen must replay durable updates"
        );
    }
    artifact
}

fn context(artifacts: Vec<SemanticIndexArtifact>) -> SemanticCandidateContext {
    SemanticCandidateContext {
        artifacts: Arc::new(artifacts),
        filter_maps: SemanticFilterMaps::for_tests(
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashSet::new(),
        ),
        roles: None,
    }
}

fn signature(hits: &[VectorSearchResult]) -> Vec<(u64, u8, u32)> {
    hits.iter()
        .map(|hit| (hit.message_id, hit.chunk_idx, hit.score.to_bits()))
        .collect()
}

fn files(directory: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut result = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.extend(files(&path));
        } else {
            result.push((path.clone(), std::fs::read(path).unwrap()));
        }
    }
    result.sort_by(|left, right| left.0.cmp(&right.0));
    result
}

#[test]
fn native_chunk_ties_follow_the_retained_exact_row_order() {
    #[derive(Debug, Clone, Copy)]
    enum Placement {
        Wal,
        MainWal,
        Shards,
        NativeThenExact,
        ExactThenNative,
    }

    let temp = tempfile::Builder::new()
        .disable_cleanup(true)
        .tempdir()
        .unwrap();
    let identity = |chunk_idx| SemanticDocId {
        message_id: 7,
        chunk_idx,
        agent_id: 1,
        workspace_id: 2,
        source_id: 3,
        role: 1,
        created_at_ms: 100,
        content_hash: None,
    };
    for (case, vectors, expected_chunk) in [
        ("equal", [[1.0, 0.0], [1.0, 0.0]], 0),
        ("lower_chunk_better", [[1.0, 0.0], [0.0, 1.0]], 0),
        ("higher_chunk_better", [[0.0, 1.0], [1.0, 0.0]], 1),
    ] {
        for reverse in [true, false] {
            for placement in [
                Placement::Wal,
                Placement::MainWal,
                Placement::Shards,
                Placement::NativeThenExact,
                Placement::ExactThenNative,
            ] {
                let directory = temp.path().join(format!("{case}-{reverse}-{placement:?}"));
                std::fs::create_dir_all(&directory).unwrap();
                let mut chunks = [
                    (identity(0).to_doc_id_string(), vectors[0]),
                    (identity(1).to_doc_id_string(), vectors[1]),
                ];
                if reverse {
                    chunks.swap(0, 1);
                }
                let filler = (doc(99, 3), [-1.0, 0.0]);
                let artifacts = match placement {
                    Placement::Wal => vec![shard(&directory, "a", &[filler], &chunks)],
                    Placement::MainWal => vec![shard(
                        &directory,
                        "a",
                        &[chunks[0].clone(), filler],
                        &chunks[1..],
                    )],
                    Placement::Shards | Placement::NativeThenExact | Placement::ExactThenNative => {
                        let first = shard(&directory, "a", &[chunks[0].clone(), filler], &[]);
                        let second = shard(&directory, "b", &chunks[1..], &[]);
                        match placement {
                            Placement::NativeThenExact => vec![
                                first,
                                SemanticIndexArtifact::open(second.fsvi_path(), None).unwrap(),
                            ],
                            Placement::ExactThenNative => vec![
                                SemanticIndexArtifact::open(first.fsvi_path(), None).unwrap(),
                                second,
                            ],
                            _ => vec![first, second],
                        }
                    }
                };
                let ctx = context(artifacts);
                let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
                let before = files(&directory);
                // Independent fixture oracle: exhaust the real backend, then
                // order by score, cohort ordinal and physical row. This keeps
                // the incumbent exact rule, not a new lowest-chunk policy.
                let mut exhaustive = Vec::new();
                for (ordinal, artifact) in ctx.artifacts.iter().enumerate() {
                    let index = artifact.index();
                    for hit in index
                        .search_top_k(
                            &[1.0, 0.0],
                            index.record_count() + index.wal_record_count(),
                            None,
                        )
                        .unwrap()
                    {
                        exhaustive.push((ordinal, hit));
                    }
                }
                exhaustive.sort_by(|(left_shard, left), (right_shard, right)| {
                    right
                        .score
                        .total_cmp(&left.score)
                        .then_with(|| left_shard.cmp(right_shard))
                        .then_with(|| left.index.cmp(&right.index))
                });
                let winner = parse_semantic_doc_id(&exhaustive[0].1.doc_id).unwrap();
                assert_eq!(winner.message_id, 7);
                if case != "equal" {
                    assert_eq!(winner.chunk_idx, expected_chunk, "score beats chunk order");
                }
                let expected = vec![(7, winner.chunk_idx, 1.0_f32.to_bits())];
                let (exact, _) =
                    SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 1, None)
                        .unwrap();
                assert!(!exact.is_empty(), "the exact fixture has a winner");
                assert_eq!(
                    signature(&exact[..1]),
                    expected,
                    "{case}, reverse={reverse}, {placement:?}: exact"
                );
                assert_eq!(exact[0].identity, Some(winner));
                let (actual, _, stats) = set
                    .search_with_exact_fallback(&ctx, &[1.0, 0.0], 1, None)
                    .unwrap();
                assert!(!actual.is_empty(), "the native fixture has a winner");
                assert_eq!(
                    signature(&actual[..1]),
                    expected,
                    "{case}, reverse={reverse}, {placement:?}: native"
                );
                assert_eq!(actual[0].identity, Some(winner));
                let stats = stats.unwrap();
                assert!(stats.k_requested > 0, "a real native graph must execute");
                let mixed = matches!(
                    placement,
                    Placement::NativeThenExact | Placement::ExactThenNative
                );
                if case == "equal" && mixed {
                    assert_eq!(signature(&actual), signature(&exact));
                    let fallback = stats.exact_fallback.as_ref().unwrap();
                    assert_eq!(
                        serde_json::to_value(fallback).unwrap()["reason"],
                        "chunk_tie_requires_exact"
                    );
                    assert_eq!(fallback.shard_count, ctx.artifacts.len());
                    assert_eq!(fallback.returned_messages, exact.len());
                    assert!(!stats.is_approximate);
                } else {
                    assert_eq!(actual.len(), 1, "the native page keeps its limit");
                    assert!(stats.exact_fallback.is_none(), "unambiguous native page");
                }
                assert_eq!(
                    files(&directory),
                    before,
                    "query must not rewrite artifacts"
                );
            }
        }
    }
}

#[test]
fn native_rescoring_preserves_exact_tail_scores_across_partitions() {
    let temp = tempfile::Builder::new()
        .disable_cleanup(true)
        .tempdir()
        .unwrap();
    // Normalized vectors whose separate-mul/add and fused byte-tail scores
    // differ. Identical stored vectors must still tie across both partitions.
    let vector = [0x3f4b_c89b, 0x3f18_d674, 0x3dcb_c89b].map(f32::from_bits);
    let query = [0x3eeb_43ec, 0xbf1d_b22a, 0x3f23_cce1].map(f32::from_bits);
    for exact_first in [true, false] {
        let directory = temp.path().join(format!("tail-{exact_first}"));
        std::fs::create_dir_all(&directory).unwrap();
        let native = shard(&directory, "native", &[(doc(1, 3), vector)], &[]);
        let exact = shard(&directory, "exact", &[(doc(2, 3), vector)], &[]);
        let exact = SemanticIndexArtifact::open(exact.fsvi_path(), None).unwrap();
        let ctx = context(if exact_first {
            vec![exact, native]
        } else {
            vec![native, exact]
        });
        let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
        let before = files(&directory);
        let (expected, _) =
            SearchClient::search_exact_semantic_indexes(&ctx, &query, 2, None).unwrap();
        assert_eq!(
            expected
                .iter()
                .map(|hit| hit.message_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(
            expected
                .iter()
                .all(|hit| hit.score.to_bits() == 0x3d7c_a32e)
        );
        let (actual, _, stats) = set
            .search_with_exact_fallback(&ctx, &query, 2, None)
            .unwrap();
        let stats = stats.unwrap();
        assert_eq!(stats.k_requested, 1, "the real native graph must execute");
        assert!(stats.is_approximate);
        assert!(stats.exact_fallback.is_none());
        assert_eq!(stats.candidate_rescore.unwrap().candidates, 1);
        assert_eq!(
            signature(&actual),
            signature(&expected),
            "exact_first={exact_first}: native and exact scores must agree"
        );
        assert_eq!(files(&directory), before);
    }
}

#[test]
fn native_rescoring_preserves_physical_chunk_ties() {
    let temp = tempfile::Builder::new()
        .disable_cleanup(true)
        .tempdir()
        .unwrap();
    let identity = |chunk_idx| SemanticDocId {
        message_id: 7,
        chunk_idx,
        agent_id: 1,
        workspace_id: 2,
        source_id: 3,
        role: 1,
        created_at_ms: 100,
        content_hash: None,
    };
    let mut saw_native_order_change = false;
    for wal in [true, false] {
        for reverse in [true, false] {
            let directory = temp.path().join(format!("row-tie-{wal}-{reverse}"));
            std::fs::create_dir_all(&directory).unwrap();
            // Quantized forms of two unit vectors: equal stored first
            // components, slightly different norms. Native cosine order can
            // differ from the exact dot kernel's first-physical-row tie rule.
            let mut vectors = [[0.70703125, 0.5, 0.5], [0.70703125, 0.70703125, 0.0]];
            if reverse {
                vectors.swap(0, 1);
            }
            let chunks = [
                (identity(0).to_doc_id_string(), vectors[0]),
                (identity(1).to_doc_id_string(), vectors[1]),
            ];
            let filler = (doc(99, 3), [-1.0, 0.0, 0.0]);
            let native = shard(
                &directory,
                "native",
                &chunks,
                if wal {
                    std::slice::from_ref(&filler)
                } else {
                    &[]
                },
            );
            let mut artifacts = vec![native];
            if !wal {
                let exact = shard(&directory, "exact", std::slice::from_ref(&filler), &[]);
                artifacts.push(SemanticIndexArtifact::open(exact.fsvi_path(), None).unwrap());
            }
            let ctx = context(artifacts);
            let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
            let before = files(&directory);
            let (raw, measured) = set.graphs[0]
                .knn_search_with_stats_against(ctx.artifacts[0].index(), &[1.0, 0.0, 0.0], 2, 100)
                .unwrap();
            assert_eq!(raw.len(), 2);
            assert!(
                measured.is_approximate,
                "the control must execute native HNSW"
            );
            let (expected, _) =
                SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0, 0.0], 1, None)
                    .unwrap();
            let winner = expected[0].identity.unwrap();
            assert_eq!(winner.message_id, 7);
            saw_native_order_change |= parse_semantic_doc_id(&raw[0].doc_id) != Some(winner);
            let (actual, _, stats) = set
                .search_with_exact_fallback(&ctx, &[1.0, 0.0, 0.0], 1, None)
                .unwrap();
            let stats = stats.unwrap();
            assert_eq!(stats.k_requested, 2);
            assert!(stats.is_approximate);
            assert!(stats.exact_fallback.is_none());
            assert_eq!(stats.candidate_rescore.unwrap().candidates, 2);
            assert_eq!(
                signature(&actual),
                signature(&expected[..1]),
                "wal={wal}, reverse={reverse}: retain the exact winning passage"
            );
            assert_eq!(actual[0].identity, Some(winner));
            assert_eq!(files(&directory), before);
        }
    }
    assert!(
        saw_native_order_change,
        "the fixture must reorder a real native window"
    );
}

#[test]
fn a_full_native_page_cannot_hide_a_new_durable_wal_winner() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = context(vec![shard(
        temp.path(),
        "append",
        &[(doc(1, 3), [0.8, 0.6]), (doc(2, 3), [0.6, 0.8])],
        &[(doc(99, 3), [1.0, 0.0])],
    )]);
    let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
    let (main_only, _) = set.graphs[0]
        .knn_search_with_stats_against(ctx.artifacts[0].index(), &[1.0, 0.0], 2, 100)
        .unwrap();
    assert_eq!(main_only.len(), 2, "fixture must fill the old native page");
    assert!(
        main_only
            .iter()
            .all(|hit| parse_semantic_doc_id(&hit.doc_id).unwrap().message_id != 99)
    );
    let before = files(temp.path());
    let (expected, _) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 1, None).unwrap();
    let (actual, retry, stats) = set
        .search_with_exact_fallback(&ctx, &[1.0, 0.0], 1, None)
        .unwrap();
    assert_eq!(
        actual.len(),
        1,
        "native serving preserves the requested page"
    );
    assert_eq!(actual[0].message_id, 99);
    assert_eq!(signature(&actual), signature(&expected[..1]));
    assert!(retry.has_more_candidates);
    let stats = stats.unwrap();
    assert_eq!(stats.k_requested, 2, "the retained graph remains in use");
    assert!(
        stats.exact_fallback.is_none(),
        "a small WAL is not a full scan"
    );
    assert_eq!(files(temp.path()), before);
}

#[test]
fn latest_wal_replacement_supersedes_a_better_old_graph_vector() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = context(vec![shard(
        temp.path(),
        "replace",
        &[(doc(1, 3), [1.0, 0.0]), (doc(2, 3), [0.6, 0.8])],
        &[(doc(1, 3), [0.8, 0.6]), (doc(1, 3), [0.0, 1.0])],
    )]);
    assert_eq!(
        ctx.artifacts[0].index().wal_record_count(),
        1,
        "last durable write wins"
    );
    let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
    let before = files(temp.path());
    let (expected, _) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 2, None).unwrap();
    let (actual, _, stats) = set
        .search_with_exact_fallback(&ctx, &[1.0, 0.0], 2, None)
        .unwrap();
    assert_eq!(actual[0].message_id, 2);
    assert_eq!(signature(&actual), signature(&expected));
    assert_eq!(
        actual.iter().find(|hit| hit.message_id == 1).unwrap().score,
        0.0
    );
    assert!(stats.unwrap().exact_fallback.is_none());
    assert_eq!(files(temp.path()), before);
}

#[test]
fn a_later_shards_wal_keeps_the_complete_source_scoped_cohort() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = context(vec![
        shard(temp.path(), "base-a", &[(doc(1, 3), [0.6, 0.8])], &[]),
        shard(
            temp.path(),
            "delta-b",
            &[(doc(2, 3), [0.8, 0.6])],
            &[(doc(3, 4), [1.0, 0.0]), (doc(4, 3), [0.9, 0.4358899])],
        ),
    ]);
    let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
    let before = files(temp.path());
    for sources in [HashSet::from([3]), HashSet::new()] {
        let expected = if sources.is_empty() {
            Vec::new()
        } else {
            vec![4, 2, 1]
        };
        let filter = SemanticFilter {
            agents: Some(HashSet::from([1])),
            workspaces: Some(HashSet::from([2])),
            sources: Some(sources),
            roles: Some(HashSet::from([1])),
            created_from: Some(100),
            created_to: Some(100),
        };
        let (actual, _, stats) = set
            .search_with_exact_fallback(&ctx, &[1.0, 0.0], 3, Some(&filter))
            .unwrap();
        assert_eq!(
            actual.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
            expected
        );
        assert!(stats.unwrap().exact_fallback.is_none());
    }
    assert_eq!(files(temp.path()), before);
}

#[test]
fn wal_recovery_never_reopens_renamed_sources_or_relaxes_request_validation() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = context(vec![shard(
        temp.path(),
        "retained",
        &[(doc(1, 3), [0.6, 0.8])],
        &[(doc(2, 3), [1.0, 0.0])],
    )]);
    let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
    let fsvi = ctx.artifacts[0].fsvi_path();
    let wal = frankensearch::index::wal_path_for(fsvi);
    for path in [fsvi, wal.as_path(), ctx.artifacts[0].ann_path().unwrap()] {
        let extension = path.extension().unwrap().to_string_lossy();
        std::fs::rename(path, path.with_extension(format!("{extension}-retained"))).unwrap();
    }
    let before = files(temp.path());
    let (actual, _, stats) = set
        .search_with_exact_fallback(&ctx, &[1.0, 0.0], 2, None)
        .unwrap();
    assert_eq!(
        actual.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert!(stats.unwrap().exact_fallback.is_none());
    assert!(
        set.search_with_exact_fallback(&ctx, &[f32::NAN, 0.0], 2, None)
            .is_err()
    );
    assert!(
        set.search_with_exact_fallback(&ctx, &[1.0], 2, None)
            .is_err()
    );
    let other = context(ctx.artifacts.as_ref().clone());
    assert!(
        set.search_with_exact_fallback(&other, &[1.0, 0.0], 2, None)
            .is_err()
    );
    let (empty, _, stats) = set.search_with_exact_fallback(&ctx, &[], 0, None).unwrap();
    assert!(empty.is_empty());
    assert!(stats.is_none());
    assert_eq!(files(temp.path()), before);
}

#[test]
fn wal_winners_fill_pages_larger_than_the_persisted_main_slab() {
    let temp = tempfile::tempdir().unwrap();
    let updates = (1..=64)
        .map(|id| (doc(id, 3), [1.0, 0.0]))
        .collect::<Vec<_>>();
    let ctx = context(vec![shard(
        temp.path(),
        "delta-heavy",
        &[(doc(1_000, 3), [-1.0, 0.0])],
        &updates,
    )]);
    let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
    let before = files(temp.path());
    let (hits, retry, stats) = set
        .search_with_exact_fallback(&ctx, &[1.0, 0.0], 32, None)
        .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        (1..=32).collect::<Vec<_>>()
    );
    assert!(retry.has_more_candidates);
    let stats = stats.unwrap();
    assert_eq!(stats.k_requested, 1, "no corpus-wide vector scan");
    assert!(stats.exact_fallback.is_none());
    assert_eq!(files(temp.path()), before);
}

#[test]
fn wal_shadowing_is_local_to_its_retained_source_shard() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = context(vec![
        shard(
            temp.path(),
            "shadow-a",
            &[(doc(1, 3), [1.0, 0.0])],
            &[(doc(1, 3), [0.0, 1.0])],
        ),
        shard(
            temp.path(),
            "shadow-b",
            &[(doc(1, 3), [0.8, 0.6]), (doc(2, 3), [0.6, 0.8])],
            &[],
        ),
    ]);
    let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
    let before = files(temp.path());
    let (hits, _, stats) = set
        .search_with_exact_fallback(&ctx, &[1.0, 0.0], 2, None)
        .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(
        hits[0].score.to_bits(),
        0.8_f32.to_bits(),
        "a shard-local replacement must not erase another shard's current score"
    );
    assert!(stats.unwrap().exact_fallback.is_none());
    assert_eq!(files(temp.path()), before);
}

#[test]
fn native_scores_use_the_exact_kernel_when_a_wal_delta_joins_the_merge() {
    // An append-only WAL tombstones nothing, so the graph stays native. Its
    // delta is scored with the exact kernel, so the native winners must be
    // too: HNSW restores 0.8 here one ULP low.
    let temp = tempfile::tempdir().unwrap();
    let ctx = context(vec![shard(
        temp.path(),
        "append-only",
        &[(doc(1, 3), [0.8, 0.6]), (doc(2, 3), [0.6, 0.8])],
        &[(doc(3, 3), [0.0, 1.0])],
    )]);
    let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
    let (hits, _, stats) = set
        .search_with_exact_fallback(&ctx, &[1.0, 0.0], 2, None)
        .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(hits[0].score.to_bits(), 0.8_f32.to_bits());
    assert_eq!(hits[1].score.to_bits(), 0.6_f32.to_bits());
    let stats = stats.unwrap();
    assert!(stats.exact_fallback.is_none());
    assert_eq!(stats.candidate_rescore.unwrap().candidates, 2);
}

#[test]
fn wal_chunk_replacement_keeps_unmodified_chunks_of_the_same_message() {
    let temp = tempfile::tempdir().unwrap();
    let other_chunk = SemanticDocId {
        message_id: 1,
        chunk_idx: 1,
        agent_id: 1,
        workspace_id: 2,
        source_id: 3,
        role: 1,
        created_at_ms: 100,
        content_hash: None,
    }
    .to_doc_id_string();
    let ctx = context(vec![shard(
        temp.path(),
        "chunk-replacement",
        &[
            (doc(1, 3), [1.0, 0.0]),
            (other_chunk, [0.8, 0.6]),
            (doc(2, 3), [0.6, 0.8]),
        ],
        &[(doc(1, 3), [0.0, 1.0])],
    )]);
    let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
    let before = files(temp.path());
    let (hits, _, stats) = set
        .search_with_exact_fallback(&ctx, &[1.0, 0.0], 2, None)
        .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(hits[0].chunk_idx, 1);
    assert_eq!(hits[0].score.to_bits(), 0.8_f32.to_bits());
    assert!(stats.unwrap().exact_fallback.is_none());
    assert_eq!(files(temp.path()), before);
}

#[test]
fn native_refills_reuse_the_prepared_wal_instead_of_filtering_it_again() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSourceFilter {
        wal_checks: AtomicUsize,
    }

    impl FsSearchFilter for CountingSourceFilter {
        fn matches(&self, doc_id: &str, _: Option<&serde_json::Value>) -> bool {
            let parsed = parse_semantic_doc_id(doc_id).unwrap();
            if parsed.message_id == 9_999 {
                self.wal_checks.fetch_add(1, Ordering::Relaxed);
            }
            parsed.source_id == 3
        }

        fn name(&self) -> &str {
            "counting-wal-source-filter"
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let mut base = (100..140)
        .map(|id| (doc(id, 4), [1.0, 0.0]))
        .collect::<Vec<_>>();
    base.extend([(doc(1, 3), [0.8, 0.6]), (doc(2, 3), [0.6, 0.8])]);
    let ctx = context(vec![shard(
        temp.path(),
        "refill-cache",
        &base,
        &[(doc(9_999, 4), [1.0, 0.0])],
    )]);
    let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
    let before = files(temp.path());
    let filter = CountingSourceFilter {
        wal_checks: AtomicUsize::new(0),
    };
    let (hits, _, stats) = set
        .search_with_exact_fallback(&ctx, &[1.0, 0.0], 2, Some(&filter))
        .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        vec![1, 2]
    );
    let stats = stats.unwrap();
    assert!(
        stats.k_requested > base.len(),
        "the fixture must execute multiple native candidate windows"
    );
    assert!(stats.exact_fallback.is_none());
    assert_eq!(
        filter.wal_checks.load(Ordering::Relaxed),
        1,
        "refill passes must reuse one prepared delta"
    );
    assert_eq!(files(temp.path()), before);
}

#[test]
fn oversized_cohort_wal_recovers_exactly_before_any_native_graph_call() {
    let temp = tempfile::tempdir().unwrap();
    let mut artifacts = Vec::new();
    for (ordinal, count) in [(0_u64, 2_048_usize), (1_u64, 2_049_usize)] {
        let base = (0..4)
            .map(|offset| (doc(50_000 + ordinal * 10 + offset, 3), [-1.0, 0.0]))
            .collect::<Vec<_>>();
        let original = shard(temp.path(), &format!("budget-{ordinal}"), &base, &[]);
        let path = original.fsvi_path().to_path_buf();
        let ann = original.ann_path().unwrap().to_path_buf();
        drop(original);
        // One durable batch, not thousands of fsyncs. Each shard individually
        // fits the row limit, but their combined resident delta does not.
        let updates = (0..count)
            .map(|offset| {
                let score = if offset < 4 {
                    1.0 - offset as f32 / 8.0 - ordinal as f32 / 16.0
                } else {
                    0.0
                };
                (
                    doc(1 + ordinal * 10_000 + offset as u64, 3),
                    vec![score, (1.0 - score * score).sqrt()],
                )
            })
            .collect::<Vec<_>>();
        {
            let mut writer = FsVectorIndex::open_writer(&path).unwrap();
            writer.append_batch(&updates).unwrap();
        }
        artifacts.push(SemanticIndexArtifact::open(&path, Some(ann)).unwrap());
    }
    let ctx = context(artifacts);
    assert_eq!(
        ctx.artifacts
            .iter()
            .map(|artifact| artifact.index().wal_record_count())
            .sum::<usize>(),
        4_097
    );
    let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
    let before = files(temp.path());
    let (expected, expected_retry) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 1, None).unwrap();
    let (hits, retry, stats) = set
        .search_with_exact_fallback(&ctx, &[1.0, 0.0], 1, None)
        .unwrap();
    assert_eq!(signature(&hits), signature(&expected));
    assert_eq!(hits[0].message_id, 1);
    assert_eq!(
        retry.has_more_candidates,
        expected_retry.has_more_candidates
    );
    let stats = stats.unwrap();
    assert_eq!(stats.k_requested, 0);
    assert_eq!(stats.k_returned, 0);
    assert!(!stats.is_approximate);
    let receipt = stats.exact_fallback.unwrap();
    assert_eq!(
        receipt.reason,
        AnnExactFallbackReason::WalDeltaRequiresExact
    );
    assert_eq!(receipt.shard_count, 2);
    assert_eq!(receipt.returned_messages, hits.len());
    assert_eq!(files(temp.path()), before);
}

#[test]
fn later_native_failure_discards_earlier_graph_and_delta_winners() {
    let temp = tempfile::tempdir().unwrap();
    let first = shard(
        temp.path(),
        "failure-a",
        &[(doc(1, 3), [0.6, 0.8])],
        &[(doc(10, 3), [0.8, 0.6])],
    );
    let second = shard(
        temp.path(),
        "failure-b",
        &[(doc(2, 3), [0.9, 0.4358899])],
        &[],
    );
    let wrong = shard(
        temp.path(),
        "wrong-graph",
        &[(doc(30, 3), [1.0, 0.0]), (doc(31, 3), [0.0, 1.0])],
        &[],
    );
    let first_graph = open_fs_semantic_ann_index(first.index(), first.ann_path().unwrap()).unwrap();
    let wrong_graph = open_fs_semantic_ann_index(wrong.index(), wrong.ann_path().unwrap()).unwrap();
    let ctx = context(vec![first, second]);
    // Exercise a failed private pairing using a real incompatible graph.
    // Production admission remains all-or-nothing; no backend is mocked.
    let set = SemanticAnnShardSet {
        artifacts: Arc::clone(&ctx.artifacts),
        graphs: vec![first_graph, wrong_graph],
        native_ordinals: vec![0, 1],
        exact_shards: Vec::new(),
    };
    let before = files(temp.path());
    let (expected, _) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 1, None).unwrap();
    let (hits, _, stats) = set
        .search_with_exact_fallback(&ctx, &[1.0, 0.0], 1, None)
        .unwrap();
    assert_eq!(signature(&hits), signature(&expected));
    assert_eq!(
        hits[0].message_id, 2,
        "the failed later shard must survive recovery"
    );
    assert!(hits.iter().all(|hit| ![30, 31].contains(&hit.message_id)));
    let stats = stats.unwrap();
    assert_eq!(stats.k_requested, 1, "keep only completed native work");
    assert!(!stats.is_approximate);
    let receipt = stats.exact_fallback.unwrap();
    assert_eq!(receipt.reason, AnnExactFallbackReason::NativeSearchFailed);
    assert_eq!(receipt.shard_count, 2);
    assert_eq!(files(temp.path()), before);
}
