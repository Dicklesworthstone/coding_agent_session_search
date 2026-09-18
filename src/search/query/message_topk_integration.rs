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
