use super::*;
use super::super::super::{SearchClient, SemanticCandidateContext, SemanticDocId,
    SemanticFilter, SemanticFilterMaps, SessionScopedSemanticFilter};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

fn doc(message_id: u64, chunk_idx: u8, source_id: u32) -> String {
    SemanticDocId { message_id, chunk_idx, agent_id: 1, workspace_id: 2,
        source_id, role: 1, created_at_ms: 100, content_hash: None }.to_doc_id_string()
}

fn vector(dimension: usize, score: f32) -> Vec<f32> {
    let mut values = vec![0.0; dimension];
    values[0] = score;
    if dimension > 1 {
        values[1] = (1.0 - score * score).max(0.0).sqrt();
    }
    values
}

fn artifact(path: &Path, dimension: usize, quantization: Quantization,
    main: &[(String, Vec<f32>)], wal: &[(String, Vec<f32>)]) -> Result<SemanticIndexArtifact> {
    let mut writer = FsVectorIndex::create_with_revision(path, "retained-wal-test", "v1", dimension, quantization)?;
    for (id, value) in main { writer.write_record(id, value)?; }
    writer.finish()?;
    if !wal.is_empty() {
        let mut writer = FsVectorIndex::open_writer(path)?;
        writer.append_batch(wal)?;
    }
    Ok(SemanticIndexArtifact::open(path, None)?)
}

// Make a valid retained-log-before-main-cleanup image with the real writer.
// A completed append is not that image: it may clean up all superseded rows.
fn cut_artifact(path: &Path, dimension: usize, quantization: Quantization,
    main: &[(String, Vec<f32>)], wal: &[(String, Vec<f32>)]) -> Result<SemanticIndexArtifact> {
    drop(artifact(path, dimension, quantization, main, &[])?);
    let producer = path.with_extension("wal-producer.fsvi");
    std::fs::copy(path, &producer)?;
    let mut writer = FsVectorIndex::open_writer(&producer)?;
    writer.append_batch(wal)?;
    drop(writer);
    std::fs::copy(frankensearch::index::wal_path_for(&producer),
        frankensearch::index::wal_path_for(path))?;
    let image = SemanticIndexArtifact::open(path, None)?;
    ensure!(image.index().tombstone_count() == 0, "cut fixture must retain original live main rows");
    ensure!(image.index().wal_record_count() == wal.len(), "cut fixture must admit the genuine WAL");
    Ok(image)
}

fn context(artifacts: Vec<SemanticIndexArtifact>) -> SemanticCandidateContext {
    SemanticCandidateContext { artifacts: Arc::new(artifacts),
        filter_maps: SemanticFilterMaps::for_tests(HashMap::new(), HashMap::new(),
            HashMap::new(), HashSet::new()), roles: None }
}

fn signature(hits: &[VectorSearchResult]) -> Vec<(u64, u8, u32)> {
    hits.iter().map(|hit| (hit.message_id, hit.chunk_idx, hit.score.to_bits())).collect()
}

// Exhaust the real backend's retained physical view, then group by message.
// Neither its scorer, supersession resolver nor sorting uses the candidate's
// row codec, bounded Selection or borrowed shadow-reference implementation.
fn oracle(artifacts: &[SemanticIndexArtifact], query: &[f32], limit: usize,
    filter: Option<&dyn FsSearchFilter>) -> Result<Vec<(u64, u8, u32)>> {
    let mut rows = Vec::new();
    for (shard, artifact) in artifacts.iter().enumerate() {
        let index = artifact.index();
        for hit in index.search_top_k(query, index.record_count() + index.wal_record_count(), filter)? {
            let identity = parse_semantic_doc_id(&hit.doc_id).context("invalid oracle identity")?;
            rows.push((identity.message_id, identity.chunk_idx, hit.score, shard, hit.index));
        }
    }
    rows.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(&b.0))
        .then(a.3.cmp(&b.3)).then(a.4.cmp(&b.4)));
    let mut seen = HashSet::new();
    Ok(rows.into_iter().filter(|row| seen.insert(row.0)).take(limit)
        .map(|row| (row.0, row.1, row.2.to_bits())).collect())
}

fn files(root: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let mut result = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_file() { result.push((path.clone(), std::fs::read(path)?)); }
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}

#[test]
fn all_finite_half_patterns_survive_the_wire_roundtrip() {
    for bits in 0..=u16::MAX {
        let half = half::f16::from_bits(bits);
        if half.is_finite() {
            assert_eq!(half::f16::from_f32(half.to_f32()).to_bits(), bits);
        }
    }
}

#[test]
fn tail_width_scores_and_chunk_ties_match_the_exact_backend_bit_for_bit() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    for dimension in [1, 2, 3, 7, 15, 16, 17, 31, 33, 65] {
        for quantization in [Quantization::F16, Quantization::F32] {
            let path = tmp.path().join(format!("tail-{dimension}-{quantization:?}.fsvi"));
            let main: Vec<_> = (0..128_u64).map(|n| {
                let values = (0..dimension).map(|i| {
                    (((n as usize * 17 + i * 13) % 101) as f32 - 50.0) / 64.0 + 1.0 / 512.0
                }).collect();
                (doc(n % 19, (n % 256) as u8, 3), values)
            }).collect();
            let artifacts = vec![artifact(&path, dimension, quantization, &main,
                &[(doc(1000, 0, 3), vector(dimension, 0.5))])?];
            for q in 0..4 {
                let query: Vec<_> = (0..dimension).map(|i| (((i * 7 + q * 3) % 31) as f32 - 15.0) / 32.0).collect();
                for limit in [1, 5, 20, usize::MAX] {
                    assert_eq!(signature(&collect(&artifacts, &query, limit, None)?),
                        oracle(&artifacts, &query, limit, None)?,
                        "dimension {dimension}, quantization {quantization:?}, query {q}, limit {limit}");
                }
            }
        }
    }
    Ok(())
}

#[test]
fn legacy_width_live_caller_cannot_certify_an_erased_raw_window_as_empty() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    for dimension in [2, 33] {
        let path = tmp.path().join(format!("erased-{dimension}.fsvi"));
        let main = vec![(doc(1, 0, 3), vector(dimension, 1.0)); 12];
        let wal = vec![(doc(1, 0, 3), vector(dimension, -1.0)),
            (doc(2, 0, 3), vector(dimension, 0.8)), (doc(3, 0, 3), vector(dimension, 0.5))];
        let ctx = context(vec![cut_artifact(&path, dimension, Quantization::F16, &main, &wal)?]);
        let query = vector(dimension, 1.0);
        let before = files(tmp.path())?;
        assert!(ctx.artifacts[0].index().search_top_k(&query, 4, None)?.is_empty(),
            "reproducer must erase all raw winners, not merely return a short page");
        assert!(super::super::try_collect_exact_messages(&ctx.artifacts, &query, 4, None)?.is_none(),
            "this regression must exercise the previously unsupported width");
        let (hits, retry) = SearchClient::search_exact_semantic_indexes(&ctx, &query, 1, None)?;
        assert_eq!(signature(&hits), oracle(&ctx.artifacts, &query, 4, None)?);
        assert_eq!(hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(), vec![2, 3, 1]);
        assert!(!retry.exact_window_may_omit_competitor);
        assert_eq!(files(tmp.path())?, before);
    }
    Ok(())
}

#[test]
fn large_wal_live_caller_preserves_current_messages_and_pagination() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("large-wal.fsvi");
    let main = vec![(doc(1, 0, 3), vector(32, 1.0)); 12];
    let mut wal = vec![(doc(1, 0, 3), vector(32, -1.0))];
    wal.extend((2..=4100_u64).map(|id| (doc(id, 0, 3), vector(32, 0.5))));
    let ctx = context(vec![cut_artifact(&path, 32, Quantization::F32, &main, &wal)?]);
    let query = vector(32, 1.0);
    let before = files(tmp.path())?;
    assert!(ctx.artifacts[0].index().wal_record_count() > super::super::MAX_WAL_SHADOW_KEYS);
    assert!(ctx.artifacts[0].index().search_top_k(&query, 4, None)?.is_empty());
    assert!(super::super::try_collect_exact_messages(&ctx.artifacts, &query, 4, None)?.is_none());
    for requested in [1, 3, 100, 5000] {
        let (hits, retry) = SearchClient::search_exact_semantic_indexes(&ctx, &query, requested, None)?;
        assert_eq!(signature(&hits), oracle(&ctx.artifacts, &query, requested * 4, None)?);
        assert!(!retry.exact_window_may_omit_competitor);
        if requested == 1 { assert!(retry.has_more_candidates); }
        if requested == 5000 { assert!(!retry.has_more_candidates); }
    }
    assert_eq!(files(tmp.path())?, before);
    Ok(())
}

#[test]
fn filtered_multishard_wal_selection_preserves_scope_and_shared_best_chunks() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let mut artifacts = Vec::new();
    for shard in 0..3_u64 {
        artifacts.push(artifact(&tmp.path().join(format!("scope-{shard}.fsvi")), 33,
            Quantization::F16,
            &[(doc(1, 0, 3), vector(33, 1.0)), (doc(7, 0, 3), vector(33, 0.3))],
            &[(doc(1, 0, 3), vector(33, -1.0)),
                (doc(2, shard as u8, 3), vector(33, 0.5 + shard as f32 / 8.0)),
                (doc(3, 0, 4), vector(33, 0.99)), (doc(4, 0, 3), vector(33, 0.95)),
                (doc(10 + shard, 0, 3), vector(33, 0.6))])?);
    }
    let metadata = SemanticFilter { agents: Some(HashSet::from([1])), workspaces: Some(HashSet::from([2])),
        sources: Some(HashSet::from([3])), roles: Some(HashSet::from([1])), created_from: Some(100), created_to: Some(100) };
    let message_ids = HashSet::from([1, 2, 3, 7, 10, 11, 12]);
    let filter = SessionScopedSemanticFilter { metadata: &metadata, message_ids: &message_ids };
    let query = vector(33, 1.0);
    let before = files(tmp.path())?;
    let hits = collect(&artifacts, &query, 5, Some(&filter))?;
    assert_eq!(signature(&hits), oracle(&artifacts, &query, 5, Some(&filter))?);
    assert_eq!(hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(), vec![2, 10, 11, 12, 7]);
    assert_eq!(hits[0].chunk_idx, 2);
    assert_eq!(files(tmp.path())?, before);
    Ok(())
}

#[test]
fn shadow_references_are_borrowed_and_budget_refusal_is_not_exhaustion() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let source = artifact(&tmp.path().join("shadow.fsvi"), 2, Quantization::F32, &[],
        &[(doc(2, 0, 3), vector(2, 0.8)), (doc(1, 0, 3), vector(2, 0.6))])?;
    let index = source.index();
    let needed = index.wal_record_count() * std::mem::size_of::<&str>();
    assert!(ShadowIds::with_budget(index, needed - 1).is_err());
    let shadowed = ShadowIds::with_budget(index, needed)?;
    for (id, _) in index.wal_records() {
        assert!(shadowed.contains(id));
        let slot = shadowed.ids.binary_search(&id).unwrap();
        assert_eq!(shadowed.ids[slot].as_ptr(), id.as_ptr(), "IDs must not be cloned");
    }
    assert!(!shadowed.contains(&doc(999, 0, 3)));
    Ok(())
}

#[test]
fn deleted_and_replaced_rows_never_resurface_after_a_cold_open() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("deleted.fsvi");
    drop(artifact(&path, 17, Quantization::F16,
        &[(doc(1, 0, 3), vector(17, 1.0)), (doc(1, 0, 3), vector(17, 0.9)),
            (doc(2, 0, 3), vector(17, 0.8)), (doc(3, 0, 3), vector(17, 0.6))], &[])?);
    let mut writer = FsVectorIndex::open_writer(&path)?;
    writer.soft_delete(&doc(2, 0, 3))?;
    writer.append_batch(&[(doc(1, 0, 3), vector(17, -1.0)), (doc(4, 0, 3), vector(17, 0.7))])?;
    drop(writer);
    let before = files(tmp.path())?;
    for _ in 0..2 {
        let sources = vec![SemanticIndexArtifact::open(&path, None)?];
        let query = vector(17, 1.0);
        let hits = collect(&sources, &query, usize::MAX, None)?;
        assert_eq!(signature(&hits), oracle(&sources, &query, usize::MAX, None)?);
        assert_eq!(hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(), vec![4, 3, 1]);
    }
    assert_eq!(files(tmp.path())?, before);
    Ok(())
}

struct CountReject(AtomicUsize);
impl FsSearchFilter for CountReject {
    fn matches(&self, _: &str, _: Option<&serde_json::Value>) -> bool {
        self.0.fetch_add(1, Ordering::Relaxed);
        false
    }
    fn name(&self) -> &str { "reject-all-retained-wal-test" }
}

#[test]
fn zero_limit_and_invalid_cohort_do_not_score_or_consult_scope() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let a = artifact(&tmp.path().join("a.fsvi"), 2, Quantization::F32, &[],
        &[(doc(1, 0, 3), vector(2, 1.0))])?;
    let b = artifact(&tmp.path().join("b.fsvi"), 3, Quantization::F32,
        &[(doc(2, 0, 3), vector(3, 1.0))], &[])?;
    let scope = CountReject(AtomicUsize::new(0));
    assert!(collect(&[a.clone()], &[f32::NAN, 0.0], 0, Some(&scope))?.is_empty());
    assert!(collect(&[a.clone()], &[f32::NAN, 0.0], 1, Some(&scope)).is_err());
    assert!(collect(&[a, b], &[1.0, 0.0], 1, Some(&scope)).is_err());
    assert_eq!(scope.0.load(Ordering::Relaxed), 0);
    Ok(())
}

#[test]
fn a_legitimate_filtered_empty_wal_is_still_an_empty_success() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let sources = vec![artifact(&tmp.path().join("none.fsvi"), 3, Quantization::F32,
        &[(doc(1, 0, 3), vector(3, 1.0))], &[(doc(2, 0, 3), vector(3, 0.8))])?];
    let scope = CountReject(AtomicUsize::new(0));
    assert!(collect(&sources, &vector(3, 1.0), 5, Some(&scope))?.is_empty());
    assert_eq!(scope.0.load(Ordering::Relaxed), 2);
    Ok(())
}

#[test]
fn no_wal_unsupported_width_keeps_the_existing_driver() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let sources = vec![artifact(&tmp.path().join("no-wal.fsvi"), 3, Quantization::F32,
        &[(doc(1, 0, 3), vector(3, 1.0))], &[])?];
    assert!(super::super::try_collect_retained_messages(&sources, &vector(3, 1.0), 1, None)?.is_none());
    Ok(())
}
