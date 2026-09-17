//! Public reader/ANN integration using real immutable FSVI v2 owners and native
//! graphs. Hash-control axis vectors establish plumbing/ranking invariants,
//! not native model quality, certified ANN recall, or CLI publication coverage.
#![cfg(any(target_os = "linux", target_os = "android"))]

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use coding_agent_search::search::semantic_manifest::TierKind;
use coding_agent_search::search::semantic_reader::ann::{
    AnnFallbackReason, AnnSearchPolicy, SemanticAnnAdmission, SemanticAnnExpectation,
    SemanticShardEngine,
};
use coding_agent_search::search::semantic_reader::{
    SemanticGenerationReader, SemanticReaderError, SemanticResultPhase, SemanticScoreKind,
    SemanticSearchBatch, SemanticShardExpectation,
};
use coding_agent_search::search::vector_index::{ROLE_USER, SemanticDocId, parse_semantic_doc_id};
use frankensearch::core::filter::SearchFilter;
use frankensearch::core::generation::{ArtifactGenerationIdentityV1, QuantizationFormat};
use frankensearch::core::{BoundQueryEmbedding, RetrievalTopology, TieredQueryEmbeddings};
use frankensearch::index::native_hnsw::{
    HnswParams, ValidatedNativeHnsw, native_hnsw_generation_receipt_path,
};
use frankensearch::index::{FsviV2IdentityBinding, ValidatedFsviBytes, VectorIndex};
use frankensearch::{HashAlgorithm, HashEmbedder};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn binding(sequence: u64, quantization: QuantizationFormat) -> FsviV2IdentityBinding {
    let hash = HashEmbedder::new(4, HashAlgorithm::FnvModular);
    let mut identity = frankensearch::Embedder::identity(&hash).unwrap().clone();
    identity.storage.format = "fsvi-v2".into();
    identity.storage.quantization = quantization;
    identity.storage.endianness = "little-endian".into();
    FsviV2IdentityBinding::new(
        ArtifactGenerationIdentityV1::new(sequence, [0x63; 16]).unwrap(),
        identity.freeze().unwrap(),
    ).unwrap()
}

fn query(space: &FsviV2IdentityBinding) -> BoundQueryEmbedding {
    let mut identity = space.frozen_identity().identity.clone();
    identity.storage.format = "in-memory-f32".into();
    identity.storage.quantization = QuantizationFormat::F32;
    identity.storage.endianness = "native-f32-values".into();
    BoundQueryEmbedding::new(vec![1.0, 0.0, 0.0, 0.0], identity).unwrap()
}

fn document(id: u64) -> String {
    SemanticDocId {
        message_id: id, chunk_idx: 0, agent_id: 1, workspace_id: 2,
        source_id: 3, role: ROLE_USER, created_at_ms: 1_700_000_000_000,
        content_hash: Some([0x36; 32]),
    }.to_doc_id_string()
}

fn shard(root: &Path, name: &str, space: &FsviV2IdentityBinding, rows: &[(u64, f32)]) -> SemanticShardExpectation {
    let path = root.join(name);
    let mut writer = VectorIndex::create_v2(&path, space.clone()).unwrap();
    for (id, score) in rows {
        writer.write_record(&document(*id), &[*score, 0.25, 0.0, 0.0]).unwrap();
    }
    writer.finish().unwrap();
    let witness = ValidatedFsviBytes::open_published(&path, space).unwrap().witness().clone();
    SemanticShardExpectation { path, binding: space.clone(), witness }
}

fn graph(selected: &SemanticShardExpectation, path: &Path, seed: u64) -> SemanticAnnExpectation {
    let owner = Arc::new(ValidatedFsviBytes::reopen_exact(
        &selected.path, &selected.binding, &selected.witness,
    ).unwrap());
    let params = HnswParams { m: 16, m0: 32, ef_construction: 64, ef_search: 8 };
    let graph = ValidatedNativeHnsw::build(owner, params, seed).unwrap();
    let receipt = graph.save(path).unwrap();
    SemanticAnnExpectation { graph_path: path.to_owned(), receipt }
}

fn ids(batch: &SemanticSearchBatch) -> Vec<u64> {
    batch.hits().iter().map(|hit| hit.document.message_id).collect()
}

fn full_width() -> AnnSearchPolicy {
    AnnSearchPolicy { initial_candidates: 32, max_candidates: 32 }
}

struct OnlyMessage(u64);
impl SearchFilter for OnlyMessage {
    fn matches(&self, id: &str, _: Option<&serde_json::Value>) -> bool {
        parse_semantic_doc_id(id).is_some_and(|id| id.message_id == self.0)
    }
    fn name(&self) -> &str { "only_fixture_message" }
}

#[test]
fn mixed_ann_and_exact_shards_merge_without_losing_a_global_winner() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let space = binding(100, QuantizationFormat::F16);
    let selected = [
        shard(&root, "a.fsvi", &space, &[(1, 0.75), (2, 0.25)]),
        shard(&root, "b.fsvi", &space, &[(3, 1.0), (4, 0.5)]),
        shard(&root, "c.fsvi", &space, &[(5, 0.875)]),
    ];
    let a = graph(&selected[0], &root.join("a.fshnsw"), 1);
    let c = graph(&selected[2], &root.join("c.fshnsw"), 2);
    let reader = SemanticGenerationReader::open(Some(&selected), None)?
        .with_ann(Some(&[Some(a), None, Some(c)]), None)?;
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    let active = reader.activate(&queries)?;
    let exact = active.search(3, None)?;
    let ann = active.search_with_ann(3, None, full_width())?;
    assert_eq!(ids(&ann), [3, 5, 1]);
    assert_eq!(ann.hits(), exact.hits());
    assert_eq!(ann.score_kind(), SemanticScoreKind::AnnRescored);
    assert_eq!(ann.coverage().requested_topology, RetrievalTopology::HashControl);
    let reports = ann.execution();
    assert_eq!(reports.len(), 3);
    assert_eq!(reports[0].engine, SemanticShardEngine::NativeAnn);
    assert_eq!(reports[1].engine, SemanticShardEngine::ExactFallback);
    assert_eq!(reports[1].fallback_reason, Some(AnnFallbackReason::NotSelected));
    assert_eq!(reports[2].engine, SemanticShardEngine::NativeAnn);
    assert!(reports[1].graph_sha256.is_none());
    assert_eq!(ann.hits()[0].fast.as_ref().unwrap().shard, 1);
    Ok(())
}

#[test]
fn attaching_graphs_does_not_change_the_default_exact_route() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let space = binding(101, QuantizationFormat::F16);
    let selected = [shard(&root, "source.fsvi", &space, &[(1, 0.5), (2, 1.0)])];
    let sidecar = graph(&selected[0], &root.join("source.fshnsw"), 3);
    let reader = SemanticGenerationReader::open(Some(&selected), None)?;
    let original = reader.clone();
    let reader = reader.with_ann(Some(&[Some(sidecar)]), None)?;
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    assert!(matches!(reader.ann_admission(TierKind::Fast, 0), Some(SemanticAnnAdmission::Admitted { .. })));
    assert!(matches!(original.ann_admission(TierKind::Fast, 0), Some(SemanticAnnAdmission::Unavailable { reason: AnnFallbackReason::NotSelected })));
    let expected = original.activate(&queries)?.search(2, None)?;
    let actual = reader.activate(&queries)?.search(2, None)?;
    assert_eq!(actual.hits(), expected.hits());
    assert_eq!(actual.score_kind(), SemanticScoreKind::Exact);
    assert_eq!(actual.execution()[0].engine, SemanticShardEngine::Exact);
    assert_eq!(actual.execution()[0].ann_windows, 0);
    assert!(actual.execution()[0].graph_sha256.is_none());
    Ok(())
}

#[test]
fn a_valid_same_owner_graph_cannot_replace_the_selected_receipt() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let space = binding(102, QuantizationFormat::F16);
    let selected = [shard(&root, "source.fsvi", &space, &[(1, 1.0), (2, 0.5)])];
    let path = root.join("source.fshnsw");
    let expected = graph(&selected[0], &path, 3);
    let replacement = graph(&selected[0], &path, 4);
    assert_ne!(expected.receipt, replacement.receipt);
    let reader = SemanticGenerationReader::open(Some(&selected), None)?
        .with_ann(Some(&[Some(expected)]), None)?;
    assert_eq!(reader.ann_admission(TierKind::Fast, 0), Some(SemanticAnnAdmission::Unavailable {
        reason: AnnFallbackReason::ReceiptMismatch,
    }));
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    let batch = reader.activate(&queries)?.search_with_ann(1, None, full_width())?;
    assert_eq!(ids(&batch), [1]);
    assert_eq!(batch.execution()[0].engine, SemanticShardEngine::ExactFallback);
    assert_eq!(batch.execution()[0].ann_windows, 0);
    Ok(())
}

#[test]
fn missing_and_corrupt_sidecars_fall_back_without_rewriting_artifacts() -> TestResult {
    for missing_receipt in [false, true] {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let space = binding(103, QuantizationFormat::F16);
        let selected = [shard(&root, "source.fsvi", &space, &[(1, 1.0), (2, 0.5)])];
        let path = root.join("source.fshnsw");
        let expected = graph(&selected[0], &path, 5);
        let receipt_path = native_hnsw_generation_receipt_path(&path)?;
        if missing_receipt {
            fs::rename(&receipt_path, root.join("retained.receipt"))?;
        } else {
            fs::write(&path, b"corrupt graph sentinel")?;
        }
        let graph_before = fs::read(&path)?;
        let modified_before = fs::metadata(&path)?.modified()?;
        let mut inventory_before = fs::read_dir(&root)?.map(|entry| entry.map(|e| e.file_name()))
            .collect::<Result<Vec<_>, _>>()?;
        inventory_before.sort();
        let reader = SemanticGenerationReader::open(Some(&selected), None)?
            .with_ann(Some(&[Some(expected)]), None)?;
        let queries = TieredQueryEmbeddings::fast_only(query(&space));
        let batch = reader.activate(&queries)?.search_with_ann(1, None, full_width())?;
        assert_eq!(ids(&batch), [1]);
        assert_eq!(batch.score_kind(), SemanticScoreKind::Exact);
        assert_eq!(batch.execution()[0].fallback_reason, Some(AnnFallbackReason::SidecarUnavailable));
        assert_eq!(fs::read(&path)?, graph_before);
        assert_eq!(fs::metadata(&path)?.modified()?, modified_before);
        let mut inventory_after = fs::read_dir(&root)?.map(|entry| entry.map(|e| e.file_name()))
            .collect::<Result<Vec<_>, _>>()?;
        inventory_after.sort();
        assert_eq!(inventory_after, inventory_before);
    }
    Ok(())
}

#[test]
fn foreign_generation_receipts_are_rejected_without_losing_exact_search() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let space = binding(104, QuantizationFormat::F16);
    let other_space = binding(105, QuantizationFormat::F16);
    let selected = [shard(&root, "current.fsvi", &space, &[(1, 1.0)])];
    let other = shard(&root, "other.fsvi", &other_space, &[(1, 1.0)]);
    let expected = graph(&other, &root.join("other.fshnsw"), 6);
    let reader = SemanticGenerationReader::open(Some(&selected), None)?
        .with_ann(Some(&[Some(expected)]), None)?;
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    let batch = reader.activate(&queries)?.search_with_ann(1, None, full_width())?;
    assert_eq!(ids(&batch), [1]);
    assert_eq!(batch.execution()[0].fallback_reason, Some(AnnFallbackReason::InvalidExpectation));
    assert_eq!(batch.execution()[0].ann_windows, 0);
    Ok(())
}

#[test]
fn restrictive_filters_widen_then_use_exact_at_the_candidate_cap() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let space = binding(106, QuantizationFormat::F16);
    let rows: Vec<_> = (1..=8).map(|id| (id, (9 - id) as f32 / 8.0)).collect();
    let selected = [shard(&root, "source.fsvi", &space, &rows)];
    let expected = graph(&selected[0], &root.join("source.fshnsw"), 7);
    let reader = SemanticGenerationReader::open(Some(&selected), None)?
        .with_ann(Some(&[Some(expected)]), None)?;
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    let filter = OnlyMessage(8);
    let batch = reader.activate(&queries)?.search_with_ann(1, Some(&filter),
        AnnSearchPolicy { initial_candidates: 1, max_candidates: 2 })?;
    assert_eq!(ids(&batch), [8]);
    let report = &batch.execution()[0];
    assert_eq!(report.engine, SemanticShardEngine::ExactFallback);
    assert_eq!(report.fallback_reason, Some(AnnFallbackReason::FilterUnderfill));
    assert_eq!(report.ann_windows, 2);
    assert_eq!(report.candidate_rows, 3);
    assert_eq!(report.final_candidate_limit, 2);
    assert!(report.graph_sha256.is_none());
    Ok(())
}

#[test]
fn filter_widening_can_succeed_natively_before_exact_fallback() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let space = binding(107, QuantizationFormat::F16);
    let rows: Vec<_> = (1..=8).map(|id| (id, (9 - id) as f32 / 8.0)).collect();
    let selected = [shard(&root, "source.fsvi", &space, &rows)];
    let expected = graph(&selected[0], &root.join("source.fshnsw"), 8);
    let reader = SemanticGenerationReader::open(Some(&selected), None)?
        .with_ann(Some(&[Some(expected)]), None)?;
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    let filter = OnlyMessage(8);
    let batch = reader.activate(&queries)?.search_with_ann(1, Some(&filter),
        AnnSearchPolicy { initial_candidates: 1, max_candidates: 8 })?;
    assert_eq!(ids(&batch), [8]);
    let report = &batch.execution()[0];
    assert_eq!(report.engine, SemanticShardEngine::NativeAnn);
    assert_eq!(report.ann_windows, 4);
    assert_eq!(report.candidate_rows, 15);
    assert_eq!(report.final_candidate_limit, 8);
    assert!(report.fallback_reason.is_none());
    Ok(())
}

#[test]
fn exhausted_filters_return_truthful_short_or_empty_exact_results() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let space = binding(108, QuantizationFormat::F16);
    let selected = [shard(&root, "source.fsvi", &space, &[(1, 1.0), (2, 0.5)])];
    let expected = graph(&selected[0], &root.join("source.fshnsw"), 9);
    let reader = SemanticGenerationReader::open(Some(&selected), None)?
        .with_ann(Some(&[Some(expected)]), None)?;
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    for (id, expected) in [(1, vec![1]), (9, vec![])] {
        let filter = OnlyMessage(id);
        let batch = reader.activate(&queries)?.search_with_ann(2, Some(&filter), full_width())?;
        assert_eq!(ids(&batch), expected);
        assert_eq!(batch.execution()[0].engine, SemanticShardEngine::ExactFallback);
        assert_eq!(batch.execution()[0].returned_candidates, batch.hits().len());
    }
    Ok(())
}

#[test]
fn a_large_k_uses_exact_without_silently_applying_the_ann_cap() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let space = binding(109, QuantizationFormat::F16);
    let selected = [shard(&root, "source.fsvi", &space, &[(1, 1.0), (2, 0.5), (3, 0.25)])];
    let expected = graph(&selected[0], &root.join("source.fshnsw"), 10);
    let reader = SemanticGenerationReader::open(Some(&selected), None)?
        .with_ann(Some(&[Some(expected)]), None)?;
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    let batch = reader.activate(&queries)?.search_with_ann(usize::MAX, None,
        AnnSearchPolicy { initial_candidates: 1, max_candidates: 2 })?;
    assert_eq!(ids(&batch), [1, 2, 3]);
    assert_eq!(batch.execution()[0].fallback_reason, Some(AnnFallbackReason::CandidateLimit));
    assert_eq!(batch.execution()[0].ann_windows, 0);
    Ok(())
}

#[test]
fn quality_only_ann_search_requires_no_fast_index() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let space = binding(110, QuantizationFormat::F16);
    let selected = [
        shard(&root, "quality-a.fsvi", &space, &[(1, 0.5)]),
        shard(&root, "quality-b.fsvi", &space, &[(2, 1.0)]),
    ];
    let a = graph(&selected[0], &root.join("a.fshnsw"), 11);
    let b = graph(&selected[1], &root.join("b.fshnsw"), 12);
    let reader = SemanticGenerationReader::open(None, Some(&selected))?
        .with_ann(None, Some(&[Some(a), Some(b)]))?;
    let queries = TieredQueryEmbeddings::quality_only(query(&space));
    let batch = reader.activate(&queries)?.search_with_ann(1, None, full_width())?;
    assert_eq!(ids(&batch), [2]);
    assert!(batch.coverage().fast.is_none());
    assert!(batch.execution().iter().all(|r| r.tier == TierKind::Quality && r.engine == SemanticShardEngine::NativeAnn));
    assert!(reader.ann_admission(TierKind::Fast, 0).is_none());
    Ok(())
}

struct CountQuality(AtomicUsize);
impl SearchFilter for CountQuality {
    fn matches(&self, id: &str, _: Option<&serde_json::Value>) -> bool {
        if parse_semantic_doc_id(id).is_some_and(|id| id.message_id >= 20) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        true
    }
    fn name(&self) -> &str { "count_quality_fixture_visits" }
}

#[test]
fn progressive_ann_queries_quality_independently_and_only_on_the_second_step() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let space = binding(111, QuantizationFormat::F16);
    let fast = [shard(&root, "fast.fsvi", &space, &[(10, 1.0), (11, 0.75)])];
    let quality = [shard(&root, "quality.fsvi", &space, &[(20, 1.0), (21, 0.75)])];
    let a = graph(&fast[0], &root.join("fast.fshnsw"), 13);
    let b = graph(&quality[0], &root.join("quality.fshnsw"), 14);
    let reader = SemanticGenerationReader::open(Some(&fast), Some(&quality))?
        .with_ann(Some(&[Some(a)]), Some(&[Some(b)]))?;
    let queries = TieredQueryEmbeddings::progressive(query(&space), query(&space));
    let active = reader.activate(&queries)?;
    let filter = CountQuality(AtomicUsize::new(0));
    let mut stream = active.progressive_with_ann(2, Some(&filter), full_width())?;
    let first = stream.next().unwrap()?;
    assert_eq!(ids(&first), [10, 11]);
    assert_eq!(filter.0.load(Ordering::Relaxed), 0);
    assert_eq!(first.coverage().phase, SemanticResultPhase::Initial);
    assert_eq!(first.execution().len(), 1);
    let refined = stream.next().unwrap()?;
    assert!(ids(&refined).contains(&20));
    assert!(ids(&refined).contains(&10));
    assert!(filter.0.load(Ordering::Relaxed) > 0);
    assert_eq!(refined.coverage().phase, SemanticResultPhase::Refined);
    assert_eq!(refined.score_kind(), SemanticScoreKind::ReciprocalRankFusion);
    assert_eq!(refined.execution().len(), 2);
    assert!(stream.next().is_none());
    assert!(stream.next().is_none());
    filter.0.store(0, Ordering::Relaxed);
    let mut abandoned = active.progressive_with_ann(1, Some(&filter), full_width())?;
    assert!(abandoned.next().unwrap().is_ok());
    drop(abandoned);
    assert_eq!(filter.0.load(Ordering::Relaxed), 0);
    Ok(())
}

#[test]
fn graph_attachment_and_search_keep_using_the_admitted_bytes_after_path_changes() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let space = binding(112, QuantizationFormat::F16);
    let selected = [shard(&root, "source.fsvi", &space, &[(1, 1.0), (2, 0.5)])];
    let path = root.join("source.fshnsw");
    let expected = graph(&selected[0], &path, 15);
    let reader = SemanticGenerationReader::open(Some(&selected), None)?;
    fs::rename(&selected[0].path, root.join("retained.fsvi"))?;
    fs::write(&selected[0].path, b"not the admitted FSVI")?;
    let reader = reader.with_ann(Some(&[Some(expected.clone())]), None)?;
    assert!(matches!(reader.ann_admission(TierKind::Fast, 0), Some(SemanticAnnAdmission::Admitted { .. })));
    fs::write(&path, b"not the admitted graph")?;
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    let batch = reader.activate(&queries)?.search_with_ann(1, None, full_width())?;
    assert_eq!(ids(&batch), [1]);
    assert_eq!(batch.execution()[0].engine, SemanticShardEngine::NativeAnn);
    let fresh = reader.clone().with_ann(Some(&[Some(expected)]), None)?;
    let fallback = fresh.activate(&queries)?.search_with_ann(1, None, full_width())?;
    assert_eq!(ids(&fallback), [1]);
    assert_eq!(fallback.execution()[0].engine, SemanticShardEngine::ExactFallback);
    drop(reader);
    drop(fresh);
    assert_eq!(batch.witness(TierKind::Fast, 0), Some(&selected[0].witness));
    assert_eq!(fs::read(&path)?, b"not the admitted graph");
    Ok(())
}

#[test]
fn zero_k_policy_errors_and_foreign_producers_do_not_execute_ann() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let space = binding(113, QuantizationFormat::F16);
    let selected = [shard(&root, "source.fsvi", &space, &[(1, 1.0)])];
    let expected = graph(&selected[0], &root.join("source.fshnsw"), 16);
    let reader = SemanticGenerationReader::open(Some(&selected), None)?
        .with_ann(Some(&[Some(expected)]), None)?;
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    let active = reader.activate(&queries)?;
    let batch = active.search_with_ann(0, None, full_width())?;
    assert!(batch.hits().is_empty());
    assert_eq!(batch.execution()[0].engine, SemanticShardEngine::Skipped);
    assert_eq!(batch.execution()[0].ann_windows, 0);
    for policy in [
        AnnSearchPolicy { initial_candidates: 0, max_candidates: 1 },
        AnnSearchPolicy { initial_candidates: 2, max_candidates: 1 },
        AnnSearchPolicy { initial_candidates: 1, max_candidates: usize::MAX },
    ] {
        assert!(matches!(active.search_with_ann(0, None, policy), Err(SemanticReaderError::InvalidAnnPolicy)));
    }
    let original = query(&space);
    let mut foreign = original.identity().clone();
    foreign.producer.backend = "different-test-producer".into();
    let foreign = TieredQueryEmbeddings::fast_only(BoundQueryEmbedding::new(original.vector().to_vec(), foreign)?);
    assert!(reader.activate(&foreign).is_err());
    Ok(())
}

#[test]
fn selection_lengths_and_missing_tiers_are_not_silently_ignored() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let space = binding(114, QuantizationFormat::F16);
    let selected = [shard(&root, "source.fsvi", &space, &[(1, 1.0)])];
    let reader = SemanticGenerationReader::open(Some(&selected), None)?;
    assert!(matches!(reader.clone().with_ann(Some(&[]), None), Err(SemanticReaderError::AnnSelectionMismatch(TierKind::Fast))));
    assert!(matches!(reader.clone().with_ann(None, Some(&[])), Err(SemanticReaderError::MissingTier(TierKind::Quality))));
    assert!(reader.ann_admission(TierKind::Fast, 1).is_none());
    assert!(reader.with_ann(Some(&[None]), None).is_ok());
    Ok(())
}

#[test]
fn ann_rescoring_preserves_small_f32_scores_lost_by_one_minus_distance() -> TestResult {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let space = binding(115, QuantizationFormat::F32);
    let selected = [shard(&root, "source.fsvi", &space, &[(1, 1.0e-8), (2, 2.0e-8)])];
    assert_eq!(1.0_f32 - 1.0e-8, 1.0_f32 - 2.0e-8);
    let expected = graph(&selected[0], &root.join("source.fshnsw"), 17);
    let reader = SemanticGenerationReader::open(Some(&selected), None)?
        .with_ann(Some(&[Some(expected)]), None)?;
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    let exact = reader.activate(&queries)?.search(2, None)?;
    let ann = reader.activate(&queries)?.search_with_ann(2, None, full_width())?;
    assert_eq!(ids(&ann), [2, 1]);
    assert_eq!(ann.hits(), exact.hits());
    assert_eq!(ann.hits()[0].fast.as_ref().unwrap().score.to_bits(), 2.0e-8_f32.to_bits());
    Ok(())
}

#[test]
fn graph_symlinks_and_hardlinks_are_rejected_without_touching_targets() -> TestResult {
    use std::os::unix::fs::symlink;
    for hardlink in [false, true] {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let space = binding(116, QuantizationFormat::F16);
        let selected = [shard(&root, "source.fsvi", &space, &[(1, 1.0)])];
        let source = root.join("source.fshnsw");
        let expected = graph(&selected[0], &source, 18);
        let before = fs::read(&source)?;
        let alias = root.join("alias.fshnsw");
        if hardlink {
            // Keep the original graph/receipt names and exact receipt valid.
            // Removing the nlink guard would admit this aliased graph.
            fs::hard_link(&source, &alias)?;
        } else {
            fs::rename(&source, &alias)?;
            symlink(&alias, &source)?;
        }
        let reader = SemanticGenerationReader::open(Some(&selected), None)?
            .with_ann(Some(&[Some(expected)]), None)?;
        let queries = TieredQueryEmbeddings::fast_only(query(&space));
        let batch = reader.activate(&queries)?.search_with_ann(1, None, full_width())?;
        assert_eq!(ids(&batch), [1]);
        assert_eq!(batch.execution()[0].engine, SemanticShardEngine::ExactFallback);
        assert_eq!(fs::read(&source)?, before);
    }
    Ok(())
}
