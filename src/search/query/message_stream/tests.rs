use super::*;
use super::super::{SearchClient, SemanticCandidateContext, SemanticDocId,
    SemanticFilter, SemanticFilterMaps, SessionScopedSemanticFilter};
use frankensearch::index::Quantization;
use std::path::Path;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering as AtomicOrdering}};
use std::time::Instant;

fn doc(message_id: u64, chunk_idx: u8, source_id: u32) -> String {
    SemanticDocId { message_id, chunk_idx, agent_id: 1, workspace_id: 2,
        source_id, role: 1, created_at_ms: 100, content_hash: None }.to_doc_id_string()
}

fn vector(dimension: usize, score: f32) -> Vec<f32> {
    let mut value = vec![0.0; dimension];
    value[0] = score;
    value[1] = (1.0 - score * score).max(0.0).sqrt();
    value
}

fn write_artifact(path: &Path, dimension: usize, quantization: Quantization,
    rows: &[(String, Vec<f32>)]) -> Result<SemanticIndexArtifact> {
    let mut writer = FsVectorIndex::create_with_revision(path, "stream-test", "v1", dimension, quantization)?;
    for (id, vector) in rows { writer.write_record(id, vector)?; }
    writer.finish()?;
    Ok(SemanticIndexArtifact::open(path, None)?)
}

fn context(artifacts: Vec<SemanticIndexArtifact>) -> SemanticCandidateContext {
    SemanticCandidateContext { artifacts: Arc::new(artifacts),
        filter_maps: SemanticFilterMaps::for_tests(HashMap::new(), HashMap::new(),
            HashMap::new(), HashSet::new()), roles: None }
}

fn signature(hits: &[VectorSearchResult]) -> Vec<(u64, u8, u32)> {
    hits.iter().map(|hit| (hit.message_id, hit.chunk_idx, hit.score.to_bits())).collect()
}

// Independent oracle: the real backend exhausts each retained source, resolves
// WAL shadowing/duplicates, then a full sort groups messages. No bounded
// selection code, candidate window or streaming admission policy is reused.
fn oracle(artifacts: &[SemanticIndexArtifact], query: &[f32], limit: usize,
    filter: Option<&dyn FsSearchFilter>) -> Result<Vec<(u64, u8, u32)>> {
    let mut all = Vec::new();
    for (shard, artifact) in artifacts.iter().enumerate() {
        let index = artifact.index();
        for hit in index.search_top_k(query, index.record_count() + index.wal_record_count(), filter)? {
            let identity = parse_semantic_doc_id(&hit.doc_id).ok_or_else(|| anyhow!("invalid oracle identity"))?;
            all.push((identity.message_id, identity.chunk_idx, hit.score, shard, hit.index));
        }
    }
    all.sort_by(|left, right| right.2.total_cmp(&left.2)
        .then(left.0.cmp(&right.0)).then(left.3.cmp(&right.3)).then(left.4.cmp(&right.4)));
    let mut seen = HashSet::new();
    Ok(all.into_iter().filter(|hit| seen.insert(hit.0)).take(limit)
        .map(|hit| (hit.0, hit.1, hit.2.to_bits())).collect())
}

struct CountingFilter { calls: AtomicUsize }
impl CountingFilter { fn new() -> Self { Self { calls: AtomicUsize::new(0) } } }
impl FsSearchFilter for CountingFilter {
    fn matches(&self, _: &str, _: Option<&serde_json::Value>) -> bool {
        self.calls.fetch_add(1, AtomicOrdering::Relaxed);
        true
    }
    fn name(&self) -> &str { "count-real-row-visits" }
}

#[test]
fn streaming_selection_retains_reentering_messages_and_exact_ties() -> Result<()> {
    let mut selection = Selection::new(2);
    for (message_id, chunk_idx, score) in [(9,0,0.1), (8,0,0.2), (7,0,0.3),
        (9,1,0.9), (8,1,0.8), (9,2,0.9), (1,0,0.8)] {
        selection.consider(Winner { message_id, chunk_idx, score })?;
        assert!(selection.ordered.len() <= 2);
        assert_eq!(selection.ordered.len(), selection.by_message.len());
    }
    let hits = selection.finish();
    assert_eq!(hits.iter().map(|hit| (hit.message_id, hit.chunk_idx)).collect::<Vec<_>>(),
        vec![(9,1), (1,0)]);
    Ok(())
}

#[test]
fn streaming_selection_matches_full_sort_for_adversarial_orders() -> Result<()> {
    for limit in [0, 1, 4, 31, 128] {
        for seed in 0..32_u64 {
            let mut state = seed + 1;
            let mut rows = Vec::new();
            let mut selection = Selection::new(limit);
            for ordinal in 0..4096 {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let message_id = state % 257;
                let score = ((state >> 32) % 17) as f32 / 8.0 - 1.0;
                let chunk_idx = (ordinal % 256) as u8;
                rows.push((Winner { message_id, chunk_idx, score }, ordinal));
                selection.consider(rows.last().unwrap().0)?;
                assert!(selection.by_message.len() <= limit);
                assert_eq!(selection.ordered.len(), selection.by_message.len());
            }
            rows.sort_by(|a, b| b.0.score.total_cmp(&a.0.score)
                .then(a.0.message_id.cmp(&b.0.message_id)).then(a.1.cmp(&b.1)));
            let mut seen = HashSet::new();
            let expected: Vec<_> = rows.into_iter().filter(|row| seen.insert(row.0.message_id))
                .take(limit).map(|row| (row.0.message_id, row.0.chunk_idx, row.0.score.to_bits())).collect();
            let actual: Vec<_> = selection.finish().into_iter()
                .map(|hit| (hit.message_id, hit.chunk_idx, hit.score.to_bits())).collect();
            assert_eq!(actual, expected, "seed {seed}, limit {limit}");
        }
    }
    Ok(())
}

#[test]
fn streaming_selection_rejects_nonfinite_and_orders_signed_zero() -> Result<()> {
    let mut selection = Selection::new(2);
    for score in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(selection.consider(Winner { message_id: 1, chunk_idx: 0, score }).is_err());
        assert!(selection.by_message.is_empty());
    }
    for (message_id, score) in [(u64::MAX, 0.0), (0, -0.0), (1, -1.0)] {
        selection.consider(Winner { message_id, chunk_idx: 0, score })?;
    }
    assert_eq!(selection.finish().iter().map(|hit| hit.message_id).collect::<Vec<_>>(), vec![u64::MAX, 0]);
    Ok(())
}

#[test]
fn streaming_parallel_budget_is_cohort_bounded_and_overflow_safe() {
    for rows in [0, 1, 65_536, usize::MAX] {
        for limit in [0, 1, 8192, 32768, 65536, usize::MAX] {
            for pool in [0, 1, 4, 128, usize::MAX] {
                let workers = worker_count(rows, limit, pool);
                assert!((1..=8).contains(&workers));
                if workers > 1 { assert!(workers * limit <= MAX_PARALLEL_SELECTION_KEYS); }
                let mut prior_end = 0;
                for worker in 0..workers {
                    let width = rows / workers;
                    let extra = rows % workers;
                    let start = worker * width + worker.min(extra);
                    let end = start + width + usize::from(worker < extra);
                    assert_eq!(start, prior_end);
                    assert!(end <= rows);
                    prior_end = end;
                }
                assert_eq!(prior_end, rows);
            }
        }
    }
}

#[test]
fn streaming_fsvi_scores_and_chunks_match_backend_for_standard_widths() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    for dimension in [32, 64, 128, 384] {
        for quantization in [Quantization::F16, Quantization::F32] {
            let rows: Vec<_> = (0..512_u64).map(|n| {
                let coordinates = (0..dimension).map(|i| {
                    (((n as usize * 17 + i * 13) % 101) as f32 - 50.0) / 64.0
                }).collect();
                (doc(n % 43, (n % 256) as u8, 3), coordinates)
            }).collect();
            let path = tmp.path().join(format!("d{dimension}-{quantization:?}.fsvi"));
            let artifacts = vec![write_artifact(&path, dimension, quantization, &rows)?];
            let before = std::fs::read(&path)?;
            for q in 0..4 {
                let query: Vec<_> = (0..dimension).map(|i| (((i * 7 + q * 3) % 31) as f32 - 15.0) / 32.0).collect();
                for limit in [1, 7, 43, 1000] {
                    let hits = try_collect_exact_messages(&artifacts, &query, limit, None)?.unwrap();
                    assert_eq!(signature(&hits), oracle(&artifacts, &query, limit, None)?);
                }
            }
            assert_eq!(std::fs::read(path)?, before);
        }
    }
    Ok(())
}

#[test]
fn streaming_fsvi_applies_all_scope_before_ranking_and_keeps_shared_messages() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let query = vector(32, 1.0);
    let mut artifacts = Vec::new();
    for shard in 0..3_u64 {
        let mut rows: Vec<_> = (0..=255_u8).map(|chunk| (doc(1, chunk, 3), vector(32, 1.0))).collect();
        rows.extend([(doc(2, 0, 3), vector(32, 0.5 + shard as f32 / 8.0)),
            (doc(3, 0, 4), vector(32, 0.99)), (doc(4, 0, 3), vector(32, 0.8)),
            (doc(10 + shard, 0, 3), vector(32, 0.6))]);
        artifacts.push(write_artifact(&tmp.path().join(format!("s{shard}.fsvi")), 32, Quantization::F16, &rows)?);
    }
    let metadata = SemanticFilter { agents: Some(HashSet::from([1])), workspaces: Some(HashSet::from([2])),
        sources: Some(HashSet::from([3])), roles: Some(HashSet::from([1])), created_from: Some(100), created_to: Some(100) };
    let message_ids = HashSet::from([1,2,3,10,11,12]);
    let filter = SessionScopedSemanticFilter { metadata: &metadata, message_ids: &message_ids };
    let hits = try_collect_exact_messages(&artifacts, &query, 5, Some(&filter))?.unwrap();
    assert_eq!(signature(&hits), oracle(&artifacts, &query, 5, Some(&filter))?);
    assert_eq!(hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(), vec![1,2,10,11,12]);
    Ok(())
}

#[test]
fn streaming_fsvi_shadowing_deletion_and_duplicate_ids_match_live_wal_view() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("wal.fsvi");
    let rows = vec![(doc(1,0,3), vector(32,1.0)), (doc(1,0,3), vector(32,0.9)),
        (doc(1,1,3), vector(32,0.2)), (doc(2,0,3), vector(32,0.6)), (doc(3,0,3), vector(32,0.8))];
    drop(write_artifact(&path, 32, Quantization::F16, &rows)?);
    let mut writer = FsVectorIndex::open_writer(&path)?;
    writer.soft_delete(&doc(3,0,3))?;
    writer.append_batch(&[(doc(1,0,3), vector(32,-1.0)), (doc(4,0,3), vector(32,0.7))])?;
    drop(writer);
    let artifacts = vec![SemanticIndexArtifact::open(&path, None)?];
    let wal_path = frankensearch::index::wal_path_for(&path);
    let before = (std::fs::read(&path)?, std::fs::read(&wal_path)?);
    let query = vector(32,1.0);
    let hits = try_collect_exact_messages(&artifacts, &query, 9, None)?.unwrap();
    assert_eq!(signature(&hits), oracle(&artifacts, &query, 9, None)?);
    assert_eq!(hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(), vec![4,2,1]);
    assert_eq!(hits[2].chunk_idx, 1, "obsolete duplicate main score must stay shadowed");
    assert_eq!((std::fs::read(path)?, std::fs::read(wal_path)?), before);
    Ok(())
}

#[test]
fn streaming_wal_only_and_filters_preserve_complete_cohort() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("wal-only.fsvi");
    drop(write_artifact(&path, 32, Quantization::F32, &[])?);
    let mut writer = FsVectorIndex::open_writer(&path)?;
    writer.append_batch(&[(doc(1,0,3), vector(32,1.0)), (doc(2,0,4), vector(32,0.9))])?;
    drop(writer);
    let artifacts = vec![SemanticIndexArtifact::open(&path,None)?];
    let metadata = SemanticFilter { agents: None, workspaces: None, sources: Some(HashSet::from([3])),
        roles: None, created_from: None, created_to: None };
    let query = vector(32,1.0);
    let hits = try_collect_exact_messages(&artifacts,&query,10,Some(&metadata))?.unwrap();
    assert_eq!(signature(&hits), oracle(&artifacts,&query,10,Some(&metadata))?);
    assert_eq!(hits.len(),1);
    assert_eq!(hits[0].message_id,1);
    Ok(())
}

#[test]
fn streaming_nonstandard_widths_and_large_wal_fall_back_before_scoring() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let count = CountingFilter::new();
    for dimension in [2,33] {
        let artifacts = vec![write_artifact(&tmp.path().join(format!("d{dimension}.fsvi")), dimension,
            Quantization::F32, &[(doc(1,0,3),vector(dimension,1.0))])?];
        assert!(try_collect_exact_messages(&artifacts,&vector(dimension,1.0),1,Some(&count))?.is_none());
    }
    let path = tmp.path().join("large-wal.fsvi");
    drop(write_artifact(&path,32,Quantization::F32,&[])?);
    let mut writer = FsVectorIndex::open_writer(&path)?;
    let updates: Vec<_> = (0..=MAX_WAL_SHADOW_KEYS).map(|n| (doc(n as u64,0,3),vector(32,1.0))).collect();
    writer.append_batch(&updates)?;
    drop(writer);
    let artifacts = vec![SemanticIndexArtifact::open(&path,None)?];
    assert!(try_collect_exact_messages(&artifacts,&vector(32,1.0),1,Some(&count))?.is_none());
    assert_eq!(count.calls.load(AtomicOrdering::Relaxed),0);
    Ok(())
}

#[test]
fn streaming_invalid_query_or_later_shard_never_returns_a_partial_page() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let valid = write_artifact(&tmp.path().join("good.fsvi"),32,Quantization::F16,
        &[(doc(1,0,3),vector(32,1.0))])?;
    let bad = write_artifact(&tmp.path().join("bad.fsvi"),32,Quantization::F16,
        &[("not-a-cass-message".to_owned(),vector(32,1.0))])?;
    assert!(try_collect_exact_messages(&[valid.clone()],&[1.0],1,None).is_err());
    assert!(try_collect_exact_messages(&[valid.clone()],&vec![f32::NAN;32],1,None).is_err());
    assert!(try_collect_exact_messages(&[valid,bad],&vector(32,1.0),2,None).is_err());
    assert!(try_collect_exact_messages(&[],&[],0,None)?.unwrap().is_empty());
    Ok(())
}

#[test]
fn streaming_parallel_scan_is_once_per_row_and_deterministic_across_pools() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let rows: Vec<_> = (0..70_000_u64).map(|n| (doc(n % 128,(n % 256) as u8,3),vector(32,0.5))).collect();
    let artifacts = vec![write_artifact(&tmp.path().join("parallel.fsvi"),32,Quantization::F16,&rows)?];
    let query = vector(32,1.0);
    let expected = oracle(&artifacts,&query,31,None)?;
    for threads in [1,2,4] {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build()?;
        let count = CountingFilter::new();
        let hits = pool.install(|| try_collect_exact_messages(&artifacts,&query,31,Some(&count)))?.unwrap();
        assert_eq!(signature(&hits),expected);
        assert_eq!(count.calls.load(AtomicOrdering::Relaxed),70_000);
    }
    Ok(())
}

#[test]
fn streaming_live_driver_matches_incumbent_and_exhaustive_oracle() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let rows: Vec<_> = (1..=160_u64).flat_map(|id| (0..=255_u8).map(move |chunk|
        (doc(id,chunk,3),vector(32,1.0 - id as f32 / 256.0)))).collect();
    let ctx = context(vec![write_artifact(&tmp.path().join("chunk-heavy.fsvi"),32,Quantization::F16,&rows)?]);
    let query = vector(32,1.0);
    let (_,initial_retry) = SearchClient::search_exact_semantic_indexes_initial_window(&ctx,&query,32,None)?;
    assert!(initial_retry.exact_window_may_omit_competitor, "fixture must reach real refinement");
    let expected = oracle(&ctx.artifacts,&query,128,None)?;
    let count = CountingFilter::new();
    let (live, live_retry) = SearchClient::search_exact_semantic_indexes(&ctx,&query,32,Some(&count))?;
    assert_eq!(signature(&live), expected);
    assert!(!live_retry.exact_window_may_omit_competitor);
    assert_eq!(count.calls.load(AtomicOrdering::Relaxed), 2 * rows.len(),
        "the live caller must do one initial scan and one streaming refinement");
    // Same-binary incumbent/candidate calls with alternating order. Timings are
    // evidence, not a CI pass threshold or an owner-scale performance claim.
    for round in 0..4 {
        for enabled in if round % 2 == 0 { [false,true] } else { [true,false] } {
            let began = Instant::now();
            let (hits,retry) = SearchClient::search_exact_semantic_indexes_with_refinement(&ctx,&query,32,None,enabled)?;
            let elapsed_us = began.elapsed().as_micros();
            assert_eq!(signature(&hits),expected);
            assert!(!retry.exact_window_may_omit_competitor);
            eprintln!("{{\"scenario\":\"chunk_heavy_exact_refinement\",\"streaming\":{enabled},\"round\":{round},\"rows\":{},\"returned\":{},\"elapsed_us\":{elapsed_us}}}",rows.len(),hits.len());
        }
    }
    Ok(())
}

#[test]
fn streaming_live_wal_supersession_cannot_certify_a_false_empty_window() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("shadowed-window.fsvi");
    // These are valid v1 physical duplicate rows, not forged bytes. The
    // writer's best-effort tombstone can leave older duplicates live while
    // the durable WAL already supersedes their entire document identity.
    let mut rows = vec![(doc(1, 0, 3), vector(32, 1.0)); 32];
    rows.push((doc(2, 0, 3), vector(32, 0.5)));
    drop(write_artifact(&path, 32, Quantization::F16, &rows)?);
    let mut writer = FsVectorIndex::open_writer(&path)?;
    writer.append_batch(&[(doc(1, 0, 3), vector(32, -1.0))])?;
    drop(writer);
    let ctx = context(vec![SemanticIndexArtifact::open(&path, None)?]);
    let index = ctx.artifacts[0].index();
    assert_eq!(index.record_count(), 33);
    let query = vector(32, 1.0);
    assert!(index.search_top_k(&query, 4, None)?.is_empty(),
        "negative fixture must lose its raw top-k solely to retained WAL shadowing");
    let (incumbent, old_retry) = SearchClient::search_exact_semantic_indexes_with_refinement(
        &ctx, &query, 1, None, false)?;
    assert!(incumbent.is_empty());
    assert!(!old_retry.exact_window_may_omit_competitor);
    let before = (std::fs::read(&path)?,
        std::fs::read(frankensearch::index::wal_path_for(&path))?);
    let count = CountingFilter::new();
    let (hits, retry) = SearchClient::search_exact_semantic_indexes(&ctx, &query, 1, Some(&count))?;
    assert_eq!(signature(&hits), oracle(&ctx.artifacts, &query, 4, None)?);
    assert_eq!(hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(), vec![2, 1]);
    assert_eq!(count.calls.load(AtomicOrdering::Relaxed), 2,
        "only the surviving main document and current WAL replacement reach scoring scope");
    assert!(!retry.exact_window_may_omit_competitor);
    assert_eq!((std::fs::read(&path)?,
        std::fs::read(frankensearch::index::wal_path_for(&path))?), before);
    Ok(())
}
