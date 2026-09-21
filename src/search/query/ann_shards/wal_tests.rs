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

fn shard(
    directory: &Path,
    name: &str,
    base: &[(String, [f32; 2])],
    updates: &[(String, [f32; 2])],
) -> SemanticIndexArtifact {
    let path = directory.join(format!("{name}.fsvi"));
    let ann = directory.join(format!("{name}.chsw"));
    let mut writer = FsVectorIndex::create_with_revision(
        &path,
        "fnv1a-2",
        "wal-overlay-regression",
        2,
        Quantization::F32,
    )
    .unwrap();
    for (id, vector) in base {
        writer.write_record(id, vector).unwrap();
    }
    writer.finish().unwrap();
    {
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
    }
    let original_main = std::fs::read(&path).unwrap();
    if !updates.is_empty() {
        let mut source = FsVectorIndex::open_writer(&path).unwrap();
        for (id, vector) in updates {
            source.append_batch(&[(id.clone(), vector.to_vec())]).unwrap();
        }
        assert!(source.wal_record_count() > 0);
    }
    assert_eq!(std::fs::read(&path).unwrap(), original_main);
    let artifact = SemanticIndexArtifact::open(path, Some(ann)).unwrap();
    assert_eq!(artifact.index().record_count(), base.len());
    if !updates.is_empty() {
        assert!(artifact.index().wal_record_count() > 0, "reopen must replay durable updates");
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
    hits.iter().map(|hit| (hit.message_id, hit.chunk_idx, hit.score.to_bits())).collect()
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
    assert!(main_only.iter().all(|hit| parse_semantic_doc_id(&hit.doc_id).unwrap().message_id != 99));
    let before = files(temp.path());
    let (expected, expected_retry) = SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 1, None).unwrap();
    let (actual, retry, stats) = set.search_with_exact_fallback(&ctx, &[1.0, 0.0], 1, None).unwrap();
    assert_eq!(actual[0].message_id, 99);
    assert_eq!(signature(&actual), signature(&expected));
    assert_eq!(retry.has_more_candidates, expected_retry.has_more_candidates);
    let stats = stats.unwrap();
    assert!(!stats.is_approximate);
    assert_eq!(stats.k_requested, 0, "the main-only graph must not execute");
    assert_eq!(stats.exact_fallback.unwrap().reason, AnnExactFallbackReason::WalDeltaRequiresExact);
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
    assert_eq!(ctx.artifacts[0].index().wal_record_count(), 1, "last durable write wins");
    let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
    let (expected, _) = SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 2, None).unwrap();
    let (actual, _, _) = set.search_with_exact_fallback(&ctx, &[1.0, 0.0], 2, None).unwrap();
    assert_eq!(actual[0].message_id, 2);
    assert_eq!(signature(&actual), signature(&expected));
    assert_eq!(actual.iter().find(|hit| hit.message_id == 1).unwrap().score, 0.0);
}

#[test]
fn a_later_shards_wal_keeps_the_complete_source_scoped_cohort() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = context(vec![
        shard(temp.path(), "base-a", &[(doc(1, 3), [0.6, 0.8])], &[]),
        shard(temp.path(), "delta-b", &[(doc(2, 3), [0.8, 0.6])], &[
            (doc(3, 4), [1.0, 0.0]),
            (doc(4, 3), [0.9, 0.4358899]),
        ]),
    ]);
    let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
    for sources in [HashSet::from([3]), HashSet::new()] {
        let filter = SemanticFilter {
            agents: Some(HashSet::from([1])),
            workspaces: Some(HashSet::from([2])),
            sources: Some(sources),
            roles: Some(HashSet::from([1])),
            created_from: Some(100),
            created_to: Some(100),
        };
        let (expected, _) = SearchClient::search_exact_semantic_indexes(&ctx, &[1.0, 0.0], 3, Some(&filter)).unwrap();
        let (actual, _, stats) = set.search_with_exact_fallback(&ctx, &[1.0, 0.0], 3, Some(&filter)).unwrap();
        assert_eq!(signature(&actual), signature(&expected));
        assert!(actual.iter().all(|hit| hit.message_id != 3));
        assert_eq!(stats.unwrap().exact_fallback.unwrap().shard_count, 2);
    }
}

#[test]
fn wal_recovery_never_reopens_renamed_sources_or_relaxes_request_validation() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = context(vec![shard(temp.path(), "retained", &[(doc(1, 3), [0.6, 0.8])], &[(doc(2, 3), [1.0, 0.0])])]);
    let set = SemanticAnnShardSet::open(Arc::clone(&ctx.artifacts)).unwrap();
    let fsvi = ctx.artifacts[0].fsvi_path();
    // Derive the actual WAL name from upstream rather than assuming its suffix.
    let wal = frankensearch::index::wal_path_for(fsvi);
    for path in [fsvi, wal.as_path(), ctx.artifacts[0].ann_path().unwrap()] {
        let extension = path.extension().unwrap().to_string_lossy();
        std::fs::rename(path, path.with_extension(format!("{extension}-retained"))).unwrap();
    }
    let before = files(temp.path());
    let (actual, _, _) = set.search_with_exact_fallback(&ctx, &[1.0, 0.0], 2, None).unwrap();
    assert_eq!(actual[0].message_id, 2);
    assert!(set.search_with_exact_fallback(&ctx, &[f32::NAN, 0.0], 2, None).is_err());
    assert!(set.search_with_exact_fallback(&ctx, &[1.0], 2, None).is_err());
    let other = context(ctx.artifacts.as_ref().clone());
    assert!(set.search_with_exact_fallback(&other, &[1.0, 0.0], 2, None).is_err());
    let (empty, _, stats) = set.search_with_exact_fallback(&ctx, &[], 0, None).unwrap();
    assert!(empty.is_empty());
    assert!(stats.is_none());
    assert_eq!(files(temp.path()), before);
}
