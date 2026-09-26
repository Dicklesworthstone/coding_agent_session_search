//! Real FSVI coverage for the exact-message query driver. No models, inference,
//! archive reconstruction, or fixture verdict overrides are involved.

use super::*;
use crate::search::vector_index::Quantization;

fn doc(id: u64, chunk: u8, source: u32) -> String {
    SemanticDocId {
        message_id: id,
        chunk_idx: chunk,
        agent_id: 1,
        workspace_id: 2,
        source_id: source,
        role: 1,
        created_at_ms: 100,
        content_hash: None,
    }
    .to_doc_id_string()
}

fn artifact(path: &Path, records: &[(String, [f32; 2])]) -> SemanticIndexArtifact {
    let mut writer = FsVectorIndex::create_with_revision(
        path,
        "fnv1a-2",
        "exact-message-regression",
        2,
        Quantization::F16,
    )
    .unwrap();
    for (id, vector) in records {
        writer.write_record(id, vector).unwrap();
    }
    writer.finish().unwrap();
    SemanticIndexArtifact::open(path, None).unwrap()
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

fn ranked(hits: &[VectorSearchResult]) -> Vec<(u64, u8, u32)> {
    hits.iter()
        .map(|hit| (hit.message_id, hit.chunk_idx, hit.score.to_bits()))
        .collect()
}

#[test]
fn exact_messages_recover_after_both_old_windows_are_exhausted_by_one_message() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("dominant.fsvi");
    let mut records = (0..=255)
        .map(|chunk| (doc(1, chunk, 3), [1.0, 0.0]))
        .collect::<Vec<_>>();
    records.extend([
        (doc(2, 0, 3), [0.8, 0.6]),
        (doc(3, 0, 3), [0.6, 0.8]),
        (doc(4, 0, 3), [0.0, 1.0]),
    ]);
    let ctx = context(vec![artifact(&path, &records)]);
    let bytes = std::fs::read(&path).unwrap();
    for old_limit in [3, 9] {
        let (old, state) = SearchClient::search_exact_semantic_indexes_initial_window(
            &ctx,
            &[1.0, 0.0],
            old_limit,
            None,
        )
        .unwrap();
        assert_eq!(
            old.len(),
            1,
            "both old windows contain only the dominant message"
        );
        assert!(state.exact_window_may_omit_competitor);
    }
    let (hits, state) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 3, None).unwrap();
    assert_eq!(
        hits.iter()
            .take(3)
            .map(|hit| hit.message_id)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(!state.exact_window_may_omit_competitor);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn exact_message_refills_preserve_original_metadata_and_session_membership() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("scoped.fsvi");
    let mut records = (0..=255)
        .map(|chunk| (doc(1, chunk, 3), [1.0, 0.0]))
        .collect::<Vec<_>>();
    records.extend([
        (doc(2, 0, 3), [0.8, 0.6]),
        (doc(3, 0, 4), [0.9, 0.4358899]),
        (doc(4, 0, 3), [0.6, 0.8]),
        (doc(5, 0, 3), [1.0, 0.0]),
    ]);
    let ctx = context(vec![artifact(&path, &records)]);
    let metadata = SemanticFilter {
        agents: Some(HashSet::from([1])),
        workspaces: Some(HashSet::from([2])),
        sources: Some(HashSet::from([3])),
        roles: Some(HashSet::from([1])),
        created_from: Some(100),
        created_to: Some(100),
    };
    // Message 3 is in the session but fails source scope; message 5 matches
    // metadata but is outside the selected session. Neither may reappear.
    let message_ids = HashSet::from([1, 2, 3, 4]);
    let filter = SessionScopedSemanticFilter {
        metadata: &metadata,
        message_ids: &message_ids,
    };
    let (hits, state) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 3, Some(&filter)).unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        vec![1, 2, 4]
    );
    assert!(!state.exact_window_may_omit_competitor);
}

#[test]
fn exact_message_refills_merge_shards_and_collapse_shared_messages() {
    let temp = tempfile::tempdir().unwrap();
    let mut artifacts = Vec::new();
    let mut before = Vec::new();
    for (shard, id, vector) in [
        (0, 10, [0.6, 0.8]),
        (1, 20, [0.9, 0.4358899]),
        (2, 30, [0.8, 0.6]),
    ] {
        let path = temp.path().join(format!("shard-{shard}.fsvi"));
        let mut records = (0..=255)
            .map(|chunk| (doc(1, chunk, 3), [1.0, 0.0]))
            .collect::<Vec<_>>();
        records.push((doc(id, 0, 3), vector));
        artifacts.push(artifact(&path, &records));
        before.push((path.clone(), std::fs::read(path).unwrap()));
    }
    let ctx = context(artifacts);
    let (hits, state) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 3, None).unwrap();
    assert_eq!(
        hits.iter()
            .take(3)
            .map(|hit| hit.message_id)
            .collect::<Vec<_>>(),
        vec![1, 20, 30]
    );
    assert_eq!(hits.iter().filter(|hit| hit.message_id == 1).count(), 1);
    assert!(!state.exact_window_may_omit_competitor);
    for (path, bytes) in before {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn exact_messages_break_score_ties_by_message_id_not_fsvi_row_order() {
    let temp = tempfile::tempdir().unwrap();
    let records = (1..=70)
        .rev()
        .map(|id| (doc(id, 0, 3), [1.0, 0.0]))
        .collect::<Vec<_>>();
    let ctx = context(vec![artifact(&temp.path().join("ties.fsvi"), &records)]);
    let (hits, state) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 3, None).unwrap();
    assert_eq!(
        hits.iter()
            .take(3)
            .map(|hit| hit.message_id)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(!state.exact_window_may_omit_competitor);
}

#[test]
fn ordinary_proven_exact_window_retains_identical_results() {
    let temp = tempfile::tempdir().unwrap();
    let records = (1..=40)
        .map(|id| {
            let score = 1.0 - id as f32 / 50.0;
            (doc(id, 0, 3), [score, (1.0 - score * score).sqrt()])
        })
        .collect::<Vec<_>>();
    let ctx = context(vec![artifact(&temp.path().join("ordinary.fsvi"), &records)]);
    let (old, old_state) =
        SearchClient::search_exact_semantic_indexes_initial_window(&ctx, &[1.0, 0.0], 3, None)
            .unwrap();
    assert!(!old_state.exact_window_may_omit_competitor);
    let (hits, state) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 3, None).unwrap();
    assert_eq!(ranked(&hits), ranked(&old));
    assert_eq!(state.has_more_candidates, old_state.has_more_candidates);
    assert_eq!(
        state.exact_window_may_omit_competitor,
        old_state.exact_window_may_omit_competitor
    );
}

#[test]
fn exact_message_work_budget_is_shared_across_shards_and_fails_explicitly() {
    let temp = tempfile::tempdir().unwrap();
    let mut artifacts = Vec::new();
    for shard in 0..22_u64 {
        let records = (0..3_u64)
            .flat_map(|offset| {
                let score = 1.0 - offset as f32 / 4.0;
                (0..=255).map(move |chunk| {
                    (
                        doc(shard * 3 + offset + 1, chunk, 3),
                        [score, (1.0 - score * score).sqrt()],
                    )
                })
            })
            .collect::<Vec<_>>();
        artifacts.push(artifact(
            &temp.path().join(format!("budget-{shard}.fsvi")),
            &records,
        ));
    }
    let ctx = context(artifacts);
    let result = SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 1, None);
    match result {
        Err(error) => assert!(error.to_string().contains("work budget"), "{error:#}"),
        Ok(_) => panic!("per-shard budget resets must not turn exhausted refinement into success"),
    }
}

#[test]
fn bounded_initial_shard_merge_matches_complete_candidate_reference() {
    let temp = tempfile::tempdir().unwrap();
    let mut artifacts = Vec::new();
    for shard in 0..12_u64 {
        let records = (0..40_u64)
            .map(|offset| {
                let id = (offset * 11 + shard * 7) % 97;
                let score = (offset + shard) as f32 / 100.0;
                (
                    doc(id, (shard % 256) as u8, 3),
                    [score, (1.0 - score * score).sqrt()],
                )
            })
            .collect::<Vec<_>>();
        artifacts.push(artifact(
            &temp.path().join(format!("merge-{shard}.fsvi")),
            &records,
        ));
    }
    let ctx = context(artifacts);
    let fetch_limit = 4;
    let mut reference = HashMap::new();
    let mut raw_count = 0;
    for artifact in ctx.artifacts.iter() {
        let limit = SearchClient::semantic_exact_candidate_limit(
            fetch_limit,
            artifact.index().record_count(),
        );
        let hits = artifact
            .index()
            .search_top_k(&[1.0, 0.0], limit, None)
            .unwrap();
        raw_count += hits.len();
        for hit in &hits {
            SearchClient::record_fs_semantic_hit(&mut reference, hit);
        }
    }
    let reference = SearchClient::collapse_semantic_results(
        reference,
        SearchClient::semantic_exact_candidate_limit(fetch_limit, raw_count),
    );
    let (actual, _) = SearchClient::search_exact_semantic_indexes_initial_window(
        &ctx,
        &[1.0, 0.0],
        fetch_limit,
        None,
    )
    .unwrap();
    assert_eq!(ranked(&actual), ranked(&reference));
}

#[test]
fn short_deduplicated_initial_window_does_not_hide_later_messages() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("duplicate-docs.fsvi");
    let mut records = vec![(doc(1, 0, 3), [1.0, 0.0]); 40];
    records.extend([(doc(2, 0, 3), [0.8, 0.6]), (doc(3, 0, 3), [0.6, 0.8])]);
    let ctx = context(vec![artifact(&path, &records)]);
    let before = std::fs::read(&path).unwrap();
    let (initial, state) =
        SearchClient::search_exact_semantic_indexes_initial_window(&ctx, &[1.0, 0.0], 2, None)
            .unwrap();
    assert_eq!(
        initial.len(),
        1,
        "FSVI deduplicates the raw candidate window"
    );
    assert!(
        state.has_more_candidates,
        "short deduplication is not exhaustion"
    );
    assert!(state.exact_window_may_omit_competitor);
    let (hits, state) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 2, None).unwrap();
    assert_eq!(
        hits.iter()
            .take(2)
            .map(|hit| hit.message_id)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(!state.exact_window_may_omit_competitor);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn short_deduplicated_shard_cannot_certify_an_incorrect_global_ranking() {
    let temp = tempfile::tempdir().unwrap();
    let mut first = vec![(doc(1, 0, 3), [1.0, 0.0]); 40];
    first.push((doc(2, 0, 3), [0.9, 0.4358899]));
    let second = [(doc(3, 0, 3), [0.6, 0.8]), (doc(4, 0, 3), [0.0, 1.0])];
    let ctx = context(vec![
        artifact(&temp.path().join("first.fsvi"), &first),
        artifact(&temp.path().join("second.fsvi"), &second),
    ]);
    let (initial, state) =
        SearchClient::search_exact_semantic_indexes_initial_window(&ctx, &[1.0, 0.0], 2, None)
            .unwrap();
    assert_eq!(
        initial
            .iter()
            .take(2)
            .map(|hit| hit.message_id)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert!(
        state.exact_window_may_omit_competitor,
        "a filled page is not a proof after raw deduplication"
    );
    let (hits, state) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 2, None).unwrap();
    assert_eq!(
        hits.iter()
            .take(2)
            .map(|hit| hit.message_id)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(!state.exact_window_may_omit_competitor);
}

#[test]
fn adaptive_exact_messages_retrieve_a_large_page_from_chunk_dominated_fsvi() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("adaptive-long-messages.fsvi");
    let records = (1..=240_u64)
        .flat_map(|id| {
            let score = 1.0 - id as f32 / 1024.0;
            (0..=255).map(move |chunk| (doc(id, chunk, 3), [score, (1.0 - score * score).sqrt()]))
        })
        .collect::<Vec<_>>();
    let ctx = context(vec![artifact(&path, &records)]);
    let before = std::fs::read(&path).unwrap();
    let (_, initial_state) =
        SearchClient::search_exact_semantic_indexes_initial_window(&ctx, &[1.0, 0.0], 50, None)
            .unwrap();
    assert!(initial_state.exact_window_may_omit_competitor);
    let (hits, state) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 50, None).unwrap();
    // The driver retains its 4x message allowance for downstream hydration.
    assert_eq!(
        hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        (1..=200).collect::<Vec<_>>()
    );
    assert!(!state.exact_window_may_omit_competitor);
    assert!(state.has_more_candidates);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn adaptive_exact_messages_preserve_session_and_metadata_scope_across_shards() {
    let temp = tempfile::tempdir().unwrap();
    let mut artifacts = Vec::new();
    let mut before = Vec::new();
    let mut membership = HashSet::new();
    for shard in 0..3_u64 {
        let path = temp.path().join(format!("adaptive-scope-{shard}.fsvi"));
        let mut records = Vec::new();
        for offset in 1..=32_u64 {
            let id = shard * 32 + offset;
            let score = 1.0 - id as f32 / 512.0;
            membership.insert(id);
            for chunk in 0..=255 {
                records.push((doc(id, chunk, 3), [score, (1.0 - score * score).sqrt()]));
            }
        }
        // Both decoys rank above every real message. One is in the selected
        // sessions but has the wrong source; the other is outside the sessions.
        let wrong_source = 1_000 + shard;
        membership.insert(wrong_source);
        records.push((doc(wrong_source, 0, 4), [1.0, 0.0]));
        records.push((doc(2_000 + shard, 0, 3), [1.0, 0.0]));
        artifacts.push(artifact(&path, &records));
        before.push((path.clone(), std::fs::read(&path).unwrap()));
    }
    let ctx = context(artifacts);
    let metadata = SemanticFilter {
        agents: Some(HashSet::from([1])),
        workspaces: Some(HashSet::from([2])),
        sources: Some(HashSet::from([3])),
        roles: Some(HashSet::from([1])),
        created_from: Some(100),
        created_to: Some(100),
    };
    let filter = SessionScopedSemanticFilter {
        metadata: &metadata,
        message_ids: &membership,
    };
    let (hits, state) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 20, Some(&filter)).unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        (1..=80).collect::<Vec<_>>()
    );
    assert!(!state.exact_window_may_omit_competitor);
    for (path, bytes) in before {
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn adaptive_exact_messages_resolve_large_tie_cohorts_without_source_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("adaptive-dense-ties.fsvi");
    let records = (1..=10_000_u64)
        .rev()
        .map(|id| (doc(id, 0, 3), [1.0, 0.0]))
        .collect::<Vec<_>>();
    let ctx = context(vec![artifact(&path, &records)]);
    let before = std::fs::read(&path).unwrap();
    let (hits, state) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 1, None).unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert!(!state.exact_window_may_omit_competitor);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

// These fixtures exercise the shared exact driver, independently of the native
// ANN delta lane. Reopen after durable append so the owner retains the WAL view.
/// The main file's records as (doc id, vector, tombstoned), read through the
/// index rather than as bytes.
fn main_records(path: &Path) -> Vec<(String, Vec<f32>, bool)> {
    let index = FsVectorIndex::open(path).unwrap();
    (0..index.record_count())
        .map(|record| {
            (
                index.doc_id_at(record).unwrap().to_string(),
                index.vector_at_f32(record).unwrap(),
                index.is_deleted(record),
            )
        })
        .collect()
}

fn retained_wal_artifact(
    path: &Path,
    main: &[(String, [f32; 2])],
    updates: &[(String, [f32; 2])],
) -> SemanticIndexArtifact {
    drop(artifact(path, main));
    let original_main = std::fs::read(path).unwrap();
    let before = main_records(path);
    if !updates.is_empty() {
        let mut writer = FsVectorIndex::open_writer(path).unwrap();
        let updates = updates
            .iter()
            .map(|(id, vector)| (id.clone(), vector.to_vec()))
            .collect::<Vec<_>>();
        writer.append_batch(&updates).unwrap();
        assert!(writer.wal_record_count() > 0);
    }
    // Appends go to the WAL and leave the main file byte-identical, except
    // that an update superseding a main record makes the pinned engine
    // tombstone that record in place (frankensearch-index append_batch).
    // Every main record then keeps its id and vector, and exactly the
    // superseded ones are tombstoned (nohx1).
    let superseded: HashSet<&str> = updates
        .iter()
        .map(|(id, _)| id.as_str())
        .filter(|id| main.iter().any(|(main_id, _)| main_id == id))
        .collect();
    if superseded.is_empty() {
        assert_eq!(std::fs::read(path).unwrap(), original_main);
    } else {
        let after = main_records(path);
        assert_eq!(after.len(), before.len());
        for ((id, vector, was_deleted), (id_after, vector_after, deleted)) in
            before.iter().zip(&after)
        {
            assert_eq!(id, id_after);
            assert_eq!(vector, vector_after);
            assert!(!was_deleted, "{id} was tombstoned before the append");
            assert_eq!(*deleted, superseded.contains(id.as_str()), "{id}");
        }
    }
    SemanticIndexArtifact::open(path, None).unwrap()
}

fn retained_query_files(directory: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

// Exhaust the retained view before grouping messages. Do not reproduce the
// driver's candidate-capacity policy as the oracle for that same policy.
fn exhaustive_retained_messages(
    ctx: &SemanticCandidateContext,
    embedding: &[f32],
    limit: usize,
    filter: Option<&dyn FsSearchFilter>,
) -> Vec<(u64, u32)> {
    let mut best = HashMap::<u64, f32>::new();
    for artifact in ctx.artifacts.iter() {
        let index = artifact.index();
        let count = index
            .record_count()
            .saturating_add(index.wal_record_count());
        for hit in index.search_top_k(embedding, count, filter).unwrap() {
            let identity = parse_semantic_doc_id(&hit.doc_id).unwrap();
            best.entry(identity.message_id)
                .and_modify(|score| {
                    if hit.score.total_cmp(score).is_gt() {
                        *score = hit.score;
                    }
                })
                .or_insert(hit.score);
        }
    }
    let mut hits = best.into_iter().collect::<Vec<_>>();
    hits.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    hits.truncate(limit.saturating_mul(4));
    hits.into_iter()
        .map(|(id, score)| (id, score.to_bits()))
        .collect()
}

fn retained_message_scores(hits: &[VectorSearchResult]) -> Vec<(u64, u32)> {
    hits.iter()
        .map(|hit| (hit.message_id, hit.score.to_bits()))
        .collect()
}

#[test]
fn wal_growth_beyond_main_extent_keeps_full_exact_page_and_pagination() {
    let temp = tempfile::tempdir().unwrap();
    let updates = (2..=65_u64)
        .map(|id| {
            let score = 1.0 - id as f32 / 128.0;
            (doc(id, 0, 3), [score, (1.0 - score * score).sqrt()])
        })
        .collect::<Vec<_>>();
    let ctx = context(vec![retained_wal_artifact(
        &temp.path().join("wal-growth.fsvi"),
        &[(doc(1, 0, 3), [-1.0, 0.0])],
        &updates,
    )]);
    assert_eq!(ctx.artifacts[0].index().record_count(), 1);
    assert_eq!(ctx.artifacts[0].index().wal_record_count(), 64);
    let before = retained_query_files(temp.path());
    let (hits, retry) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 2, None).unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        (2..=9).collect::<Vec<_>>()
    );
    assert_eq!(
        retained_message_scores(&hits),
        exhaustive_retained_messages(&ctx, &[1.0, 0.0], 2, None)
    );
    assert!(retry.has_more_candidates);
    assert!(!retry.exact_window_may_omit_competitor);
    assert_eq!(retained_query_files(temp.path()), before);
}

#[test]
fn wal_only_sources_participate_in_single_and_multi_shard_exact_search() {
    let temp = tempfile::tempdir().unwrap();
    let wal = retained_wal_artifact(
        &temp.path().join("wal-only.fsvi"),
        &[],
        &[(doc(7, 0, 3), [1.0, 0.0]), (doc(8, 0, 3), [0.8, 0.6])],
    );
    assert_eq!(wal.index().record_count(), 0);
    assert_eq!(wal.index().wal_record_count(), 2);
    let main = artifact(
        &temp.path().join("main.fsvi"),
        &[(doc(1, 0, 3), [0.6, 0.8])],
    );
    let before = retained_query_files(temp.path());
    for (ctx, expected) in [
        (context(vec![wal.clone()]), vec![7, 8]),
        (context(vec![main, wal]), vec![7, 8, 1]),
    ] {
        let (hits, retry) =
            SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 3, None).unwrap();
        assert_eq!(
            hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
            expected
        );
        assert!(!retry.has_more_candidates);
        assert!(!retry.exact_window_may_omit_competitor);
    }
    assert_eq!(retained_query_files(temp.path()), before);
}

#[test]
fn wal_chunk_dominance_preserves_session_and_metadata_filters_during_exact_refills() {
    let temp = tempfile::tempdir().unwrap();
    let mut updates = (0..=255_u8)
        .map(|chunk| (doc(2, chunk, 3), [1.0, 0.0]))
        .collect::<Vec<_>>();
    updates.extend([
        (doc(3, 0, 3), [0.8, 0.6]),
        (doc(4, 0, 4), [0.9, 0.4358899]),
        (doc(5, 0, 3), [0.9, 0.4358899]),
        (doc(6, 0, 3), [0.7, 0.71414286]),
    ]);
    let ctx = context(vec![retained_wal_artifact(
        &temp.path().join("chunked-wal.fsvi"),
        &[(doc(1, 0, 3), [0.6, 0.8])],
        &updates,
    )]);
    let metadata = SemanticFilter {
        agents: Some(HashSet::from([1])),
        workspaces: Some(HashSet::from([2])),
        sources: Some(HashSet::from([3])),
        roles: Some(HashSet::from([1])),
        created_from: Some(100),
        created_to: Some(100),
    };
    let membership = HashSet::from([1, 2, 3, 4, 6]);
    let filter = SessionScopedSemanticFilter {
        metadata: &metadata,
        message_ids: &membership,
    };
    let before = retained_query_files(temp.path());
    let (initial, initial_retry) = SearchClient::search_exact_semantic_indexes_initial_window(
        &ctx,
        &[1.0, 0.0],
        3,
        Some(&filter),
    )
    .unwrap();
    assert_eq!(initial.len(), 1);
    assert!(initial_retry.exact_window_may_omit_competitor);
    let (hits, retry) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 3, Some(&filter)).unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        vec![2, 3, 6, 1]
    );
    assert_eq!(
        retained_message_scores(&hits),
        exhaustive_retained_messages(&ctx, &[1.0, 0.0], 3, Some(&filter))
    );
    assert!(!retry.exact_window_may_omit_competitor);
    assert_eq!(retained_query_files(temp.path()), before);
}

#[test]
fn exact_merge_reports_global_truncation_even_when_each_shard_is_exhausted() {
    let temp = tempfile::tempdir().unwrap();
    let artifacts = (1..=5_u64)
        .map(|id| {
            let score = 1.0 - id as f32 / 16.0;
            artifact(
                &temp.path().join(format!("exhausted-{id}.fsvi")),
                &[(doc(id, 0, 3), [score, (1.0 - score * score).sqrt()])],
            )
        })
        .collect();
    let ctx = context(artifacts);
    let before = retained_query_files(temp.path());
    let (initial, initial_retry) =
        SearchClient::search_exact_semantic_indexes_initial_window(&ctx, &[1.0, 0.0], 1, None)
            .unwrap();
    assert!(initial_retry.has_more_candidates);
    assert!(!initial_retry.exact_window_may_omit_competitor);
    let (hits, retry) =
        SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 1, None).unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_eq!(ranked(&hits), ranked(&initial));
    assert!(retry.has_more_candidates);
    assert_eq!(retained_query_files(temp.path()), before);
}

#[test]
fn wal_exact_large_requests_preserve_all_live_messages_not_raw_replacements() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = context(vec![retained_wal_artifact(
        &temp.path().join("complete-view.fsvi"),
        &[(doc(1, 0, 3), [1.0, 0.0])],
        &[
            (doc(1, 0, 3), [0.0, 1.0]),
            (doc(2, 0, 3), [0.9, 0.4358899]),
            (doc(3, 0, 3), [0.8, 0.6]),
            (doc(4, 0, 3), [0.6, 0.8]),
        ],
    )]);
    let before = retained_query_files(temp.path());
    for limit in [8, usize::MAX] {
        let (hits, retry) =
            SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], limit, None).unwrap();
        assert_eq!(
            hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
            vec![2, 3, 4, 1]
        );
        assert_eq!(hits.last().unwrap().score, 0.0);
        assert!(!retry.has_more_candidates);
        assert_eq!(
            retained_message_scores(&hits),
            exhaustive_retained_messages(&ctx, &[1.0, 0.0], limit, None)
        );
    }
    assert_eq!(retained_query_files(temp.path()), before);
}
