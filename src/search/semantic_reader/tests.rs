//! Real FSVI v2 owners with deterministic axis-vector ranking fixtures. The
//! explicit hash-control identity is obtained from the real hash embedder;
//! these are reader/identity tests, not native semantic-model qualification.
#![cfg(any(target_os = "linux", target_os = "android"))]

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use super::*;
use frankensearch::core::generation::{EmbeddingIdentityBundleV1, QuantizationFormat};
use frankensearch::index::{Quantization, VectorIndex, wal_path_for};
use frankensearch::{HashAlgorithm, HashEmbedder};

use crate::search::vector_index::SemanticFilter;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn binding(sequence: u64, nonce: u8, dimension: usize) -> FsviV2IdentityBinding {
    let hash = HashEmbedder::new(dimension, HashAlgorithm::FnvModular);
    let mut identity = frankensearch::Embedder::identity(&hash).unwrap().clone();
    identity.storage.format = "fsvi-v2".into();
    identity.storage.quantization = QuantizationFormat::F16;
    identity.storage.endianness = "little-endian".into();
    FsviV2IdentityBinding::new(
        ArtifactGenerationIdentityV1::new(sequence, [nonce; 16]).unwrap(),
        identity.freeze().unwrap(),
    )
    .unwrap()
}

fn query_identity(binding: &FsviV2IdentityBinding) -> EmbeddingIdentityBundleV1 {
    let mut identity = binding.frozen_identity().identity.clone();
    identity.storage.format = "in-memory-f32".into();
    identity.storage.quantization = QuantizationFormat::F32;
    identity.storage.endianness = "native-f32-values".into();
    identity
}

fn query(binding: &FsviV2IdentityBinding) -> BoundQueryEmbedding {
    let mut vector = vec![0.0; binding.dimension()];
    vector[0] = 1.0;
    BoundQueryEmbedding::new(vector, query_identity(binding)).unwrap()
}

fn document(message_id: u64) -> SemanticDocId {
    SemanticDocId {
        message_id,
        chunk_idx: 0,
        agent_id: 4,
        workspace_id: 5,
        source_id: 6,
        role: ROLE_USER,
        created_at_ms: 1_700_000_000_000,
        content_hash: Some([0x52; 32]),
    }
}

fn write_at(
    path: &Path,
    binding: &FsviV2IdentityBinding,
    records: &[(String, Vec<f32>)],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = VectorIndex::create_v2(path, binding.clone())?;
    for (id, vector) in records {
        writer.write_record(id, vector)?;
    }
    writer.finish()?;
    Ok(())
}

fn shard(
    directory: &Path,
    name: &str,
    binding: &FsviV2IdentityBinding,
    records: &[(u64, f32)],
) -> SemanticShardExpectation {
    let path = directory.join(name);
    let records: Vec<_> = records
        .iter()
        .map(|(id, score)| {
            let mut vector = vec![0.0; binding.dimension()];
            vector[0] = *score;
            // Avoid the intentional zero-signal case in ordinary rank fixtures.
            if *score == 0.0 {
                vector[1] = 1.0;
            }
            (document(*id).to_doc_id_string(), vector)
        })
        .collect();
    write_at(&path, binding, &records).unwrap();
    let witness = ValidatedFsviBytes::open_published(&path, binding)
        .unwrap()
        .witness()
        .clone();
    SemanticShardExpectation {
        path,
        binding: binding.clone(),
        witness,
    }
}

fn ids(batch: &SemanticSearchBatch) -> Vec<u64> {
    batch
        .hits()
        .iter()
        .map(|hit| hit.document.message_id)
        .collect()
}

#[test]
fn quality_only_reads_its_own_shards_without_any_fast_artifact() -> TestResult {
    let temp = tempfile::tempdir()?;
    let space = binding(7, 1, 4);
    let quality = [shard(
        temp.path(),
        "quality.fsvi",
        &space,
        &[(7, 1.0), (8, 0.0)],
    )];
    let reader = SemanticGenerationReader::open(None, Some(&quality))?;
    let queries = TieredQueryEmbeddings::quality_only(query(&space));
    let batch = reader.activate(&queries)?.search(1, None)?;
    assert_eq!(ids(&batch), [7]);
    assert_eq!(batch.score_kind(), SemanticScoreKind::Exact);
    assert_eq!(
        batch.coverage().requested_topology,
        RetrievalTopology::HashControl
    );
    assert!(batch.coverage().fast.is_none());
    let observed = batch.coverage().quality.as_ref().unwrap();
    assert_eq!(observed.witnessed_live_passages, 2);
    assert_eq!(observed.retrieved_candidates, 1);
    assert_eq!(observed.contributed_candidates, 1);
    assert!(batch.hits()[0].fast.is_none());
    assert_eq!(batch.hits()[0].quality.as_ref().unwrap().tier_rank, 1);
    let wrong = TieredQueryEmbeddings::fast_only(query(&space));
    assert!(matches!(
        reader.activate(&wrong),
        Err(SemanticReaderError::MissingTier(TierKind::Fast))
    ));
    Ok(())
}

#[test]
fn global_top_k_keeps_shard_and_physical_provenance() -> TestResult {
    let temp = tempfile::tempdir()?;
    let space = binding(8, 2, 4);
    let selected = [
        shard(temp.path(), "a.fsvi", &space, &[(1, 0.25), (2, -0.5)]),
        shard(temp.path(), "b.fsvi", &space, &[(3, 1.0), (4, 0.5)]),
        shard(temp.path(), "c.fsvi", &space, &[(5, 0.75)]),
    ];
    let reader = SemanticGenerationReader::open(Some(&selected), None)?;
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    let result = reader.activate(&queries)?.search(3, None)?;
    assert_eq!(ids(&result), [3, 5, 4]);
    assert_eq!(
        result
            .coverage()
            .fast
            .as_ref()
            .unwrap()
            .witnessed_live_passages,
        5
    );
    for hit in result.hits() {
        let source = hit.fast.as_ref().unwrap();
        let owner = &reader.fast.as_ref().unwrap().shards[source.shard];
        assert_eq!(owner.doc_id_at(source.physical_index)?, hit.doc_id);
        assert_eq!(
            result.witness(TierKind::Fast, source.shard),
            Some(owner.witness())
        );
    }
    Ok(())
}

#[test]
fn ties_use_selection_order_not_an_invented_global_row_number() -> TestResult {
    let temp = tempfile::tempdir()?;
    let space = binding(9, 3, 4);
    let a = shard(temp.path(), "a.fsvi", &space, &[(99, 1.0)]);
    let b = shard(temp.path(), "b.fsvi", &space, &[(1, 1.0)]);
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    let reader = SemanticGenerationReader::open(Some(&[a.clone(), b.clone()]), None)?;
    for _ in 0..3 {
        assert_eq!(ids(&reader.activate(&queries)?.search(1, None)?), [99]);
    }
    let reader = SemanticGenerationReader::open(Some(&[b, a]), None)?;
    assert_eq!(ids(&reader.activate(&queries)?.search(1, None)?), [1]);
    Ok(())
}

#[test]
fn canonical_filters_apply_before_each_shards_top_k() -> TestResult {
    let temp = tempfile::tempdir()?;
    let space = binding(10, 4, 4);
    let selected = [shard(
        temp.path(),
        "index.fsvi",
        &space,
        &[(1, 1.0), (2, 0.75)],
    )];
    let mut second = document(2);
    second.agent_id = 9;
    second.role = ROLE_TOOL;
    write_at(
        &selected[0].path,
        &space,
        &[
            (document(1).to_doc_id_string(), vec![1.0, 0.0, 0.0, 0.0]),
            (second.to_doc_id_string(), vec![0.75, 0.0, 0.0, 0.0]),
        ],
    )?;
    let mut selected = selected;
    selected[0].witness = ValidatedFsviBytes::open_published(&selected[0].path, &space)?
        .witness()
        .clone();
    let reader = SemanticGenerationReader::open(Some(&selected), None)?;
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    let filter = SemanticFilter {
        agents: Some(HashSet::from([9])),
        roles: Some(HashSet::from([ROLE_TOOL])),
        created_from: Some(second.created_at_ms),
        created_to: Some(second.created_at_ms),
        ..Default::default()
    };
    let result = reader.activate(&queries)?.search(1, Some(&filter))?;
    assert_eq!(ids(&result), [2]);
    let no_match = SemanticFilter {
        agents: Some(HashSet::new()),
        ..Default::default()
    };
    let result = reader.activate(&queries)?.search(10, Some(&no_match))?;
    assert!(result.hits().is_empty());
    let observed = result.coverage().fast.as_ref().unwrap();
    assert_eq!(observed.witnessed_live_passages, 2);
    assert_eq!(observed.retrieved_candidates, 0);
    assert_eq!(observed.contributed_candidates, 0);
    Ok(())
}

struct CountQualityScans(AtomicUsize);
impl SearchFilter for CountQualityScans {
    fn matches(&self, id: &str, _metadata: Option<&serde_json::Value>) -> bool {
        if parse_semantic_doc_id(id).unwrap().message_id >= 100 {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
        }
        true
    }
    fn name(&self) -> &str {
        "count-quality-scans"
    }
}

#[test]
fn progressive_retrieval_is_lazy_and_quality_contributes_outside_the_fast_pool() -> TestResult {
    let temp = tempfile::tempdir()?;
    let space = binding(11, 5, 4);
    let fast = [shard(
        temp.path(),
        "fast.fsvi",
        &space,
        &[(1, 1.0), (2, 0.25)],
    )];
    let quality = [shard(
        temp.path(),
        "quality.fsvi",
        &space,
        &[(100, 1.0), (101, 0.25)],
    )];
    let reader = SemanticGenerationReader::open(Some(&fast), Some(&quality))?;
    let queries = TieredQueryEmbeddings::progressive(query(&space), query(&space));
    let active = reader.activate(&queries)?;
    let filter = CountQualityScans(AtomicUsize::new(0));
    let mut phases = active.progressive(2, Some(&filter))?;
    assert_eq!(filter.0.load(AtomicOrdering::SeqCst), 0);
    let first = phases.next().unwrap()?;
    assert_eq!(first.coverage().phase, SemanticResultPhase::Initial);
    assert_eq!(ids(&first), [1, 2]);
    assert!(first.coverage().quality.is_none());
    assert_eq!(first.score_kind(), SemanticScoreKind::Exact);
    assert_eq!(filter.0.load(AtomicOrdering::SeqCst), 0);
    let refined = phases.next().unwrap()?;
    assert_eq!(refined.coverage().phase, SemanticResultPhase::Refined);
    assert_eq!(
        refined.score_kind(),
        SemanticScoreKind::ReciprocalRankFusion
    );
    assert!(ids(&refined).contains(&100));
    assert!(filter.0.load(AtomicOrdering::SeqCst) > 0);
    assert_eq!(
        refined
            .coverage()
            .quality
            .as_ref()
            .unwrap()
            .contributed_candidates,
        1
    );
    assert_eq!(
        ids(&first),
        [1, 2],
        "refinement must not mutate the initial result"
    );
    assert!(phases.next().is_none());
    assert!(phases.next().is_none());

    let filter = CountQualityScans(AtomicUsize::new(0));
    let mut phases = active.progressive(2, Some(&filter))?;
    assert!(phases.next().unwrap().is_ok());
    drop(phases);
    assert_eq!(
        filter.0.load(AtomicOrdering::SeqCst),
        0,
        "abandoning refinement must not scan quality"
    );
    Ok(())
}

#[test]
fn fusion_retains_both_contributors_and_agrees_with_two_phase_retrieval() -> TestResult {
    let temp = tempfile::tempdir()?;
    let space = binding(12, 6, 4);
    let fast = [shard(
        temp.path(),
        "fast.fsvi",
        &space,
        &[(1, 1.0), (2, 0.25)],
    )];
    let quality = [shard(
        temp.path(),
        "quality.fsvi",
        &space,
        &[(1, 0.25), (3, 1.0)],
    )];
    let reader = SemanticGenerationReader::open(Some(&fast), Some(&quality))?;
    let queries = TieredQueryEmbeddings::progressive(query(&space), query(&space));
    let active = reader.activate(&queries)?;
    let result = active.search(2, None)?;
    assert_eq!(ids(&result), [1, 3]);
    assert_eq!(result.hits()[0].fast.as_ref().unwrap().tier_rank, 1);
    assert_eq!(result.hits()[0].quality.as_ref().unwrap().tier_rank, 2);
    assert_eq!(result.hits()[0].ranking_score, 1.0 / 61.0 + 1.0 / 62.0);
    assert_eq!(
        result
            .coverage()
            .fast
            .as_ref()
            .unwrap()
            .contributed_candidates,
        1
    );
    assert_eq!(
        result
            .coverage()
            .quality
            .as_ref()
            .unwrap()
            .contributed_candidates,
        2
    );
    let mut phases = active.progressive(2, None)?;
    let _ = phases.next().unwrap()?;
    assert_eq!(phases.next().unwrap()?.hits(), result.hits());
    Ok(())
}

#[test]
fn foreign_quality_producer_is_rejected_before_any_fast_scan() -> TestResult {
    let temp = tempfile::tempdir()?;
    let space = binding(13, 7, 4);
    let fast = [shard(temp.path(), "fast.fsvi", &space, &[(100, 1.0)])];
    let quality = [shard(temp.path(), "quality.fsvi", &space, &[(101, 1.0)])];
    let reader = SemanticGenerationReader::open(Some(&fast), Some(&quality))?;
    let mut foreign = query_identity(&space);
    foreign.producer.backend.push_str("-foreign");
    let foreign = BoundQueryEmbedding::new(vec![1.0, 0.0, 0.0, 0.0], foreign)?;
    assert!(matches!(
        foreign.verify_producer_conformance(&space.frozen_identity().identity, "quality")?,
        SpaceIdentityAdmission::ConformanceCompatibleProducer { .. }
    ));
    let queries = TieredQueryEmbeddings::progressive(query(&space), foreign);
    let filter = CountQualityScans(AtomicUsize::new(0));
    for k in [0, 1] {
        let result = reader
            .activate(&queries)
            .and_then(|active| active.search(k, Some(&filter)));
        assert!(matches!(
            result,
            Err(SemanticReaderError::ForeignProducer(TierKind::Quality))
        ));
    }
    assert_eq!(filter.0.load(AtomicOrdering::SeqCst), 0);
    let valid = TieredQueryEmbeddings::fast_only(query(&space));
    reader.activate(&valid)?.search(1, Some(&filter))?;
    assert!(
        filter.0.load(AtomicOrdering::SeqCst) > 0,
        "the scan detector must observe real work"
    );
    Ok(())
}

#[test]
fn wrong_space_and_missing_requested_tier_are_errors_not_empty_hits() -> TestResult {
    let temp = tempfile::tempdir()?;
    let space = binding(14, 8, 4);
    let selected = [shard(temp.path(), "fast.fsvi", &space, &[])];
    let reader = SemanticGenerationReader::open(Some(&selected), None)?;
    let foreign_space = binding(14, 8, 8);
    let bad = TieredQueryEmbeddings::fast_only(query(&foreign_space));
    assert!(reader.activate(&bad).is_err());
    let missing = TieredQueryEmbeddings::quality_only(query(&space));
    assert!(matches!(
        reader.activate(&missing),
        Err(SemanticReaderError::MissingTier(TierKind::Quality))
    ));
    let valid = TieredQueryEmbeddings::fast_only(query(&space));
    let result = reader.activate(&valid)?.search(0, None)?;
    assert!(result.hits().is_empty());
    assert_eq!(
        result
            .coverage()
            .fast
            .as_ref()
            .unwrap()
            .witnessed_live_passages,
        0
    );
    assert!(matches!(
        reader.activate(&valid)?.progressive(1, None),
        Err(SemanticReaderError::ProgressiveRequiresBothTiers)
    ));
    Ok(())
}

#[test]
fn retained_reader_ignores_path_replacement_and_batches_pin_the_old_owner() -> TestResult {
    let temp = tempfile::tempdir()?;
    let space = binding(15, 9, 4);
    let selected = [shard(temp.path(), "index.fsvi", &space, &[(1, 1.0)])];
    let mut reader = SemanticGenerationReader::open(Some(&selected), None)?;
    let weak = Arc::downgrade(reader.fast.as_ref().unwrap());
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    let old_batch = reader.activate(&queries)?.search(1, None)?;
    fs::rename(
        &selected[0].path,
        temp.path().join("retained-original.fsvi"),
    )?;
    fs::write(&selected[0].path, b"not an index")?;
    assert_eq!(ids(&reader.activate(&queries)?.search(1, None)?), [1]);
    assert!(SemanticGenerationReader::open(Some(&selected), None).is_err());

    let successor_space = binding(16, 10, 4);
    let successor = [shard(
        temp.path(),
        "successor.fsvi",
        &successor_space,
        &[(2, 1.0)],
    )];
    reader.try_replace(Some(&successor), None)?;
    assert_eq!(reader.generation(), successor_space.generation());
    assert!(weak.upgrade().is_some());
    assert_eq!(
        old_batch.witness(TierKind::Fast, 0),
        Some(&selected[0].witness)
    );
    assert_eq!(ids(&old_batch), [1]);
    drop(old_batch);
    assert!(weak.upgrade().is_none());
    Ok(())
}

#[test]
fn failed_pair_replacement_preserves_the_last_good_reader() -> TestResult {
    let temp = tempfile::tempdir()?;
    let old_space = binding(17, 11, 4);
    let old = [shard(temp.path(), "old.fsvi", &old_space, &[(1, 1.0)])];
    let mut reader = SemanticGenerationReader::open(Some(&old), None)?;
    let newer = binding(18, 12, 4);
    let fast = [shard(temp.path(), "fast.fsvi", &newer, &[(2, 1.0)])];
    let mut bad_quality = fast[0].clone();
    bad_quality.path = temp.path().join("missing-quality.fsvi");
    let original = Arc::as_ptr(reader.fast.as_ref().unwrap());
    assert!(
        reader
            .try_replace(Some(&fast), Some(&[bad_quality]))
            .is_err()
    );
    assert_eq!(Arc::as_ptr(reader.fast.as_ref().unwrap()), original);
    assert_eq!(reader.generation(), old_space.generation());
    let queries = TieredQueryEmbeddings::fast_only(query(&old_space));
    assert_eq!(ids(&reader.activate(&queries)?.search(1, None)?), [1]);
    Ok(())
}

#[test]
fn same_sequence_different_nonce_and_mixed_tier_bundles_are_rejected() -> TestResult {
    let temp = tempfile::tempdir()?;
    let a_space = binding(19, 13, 4);
    let b_space = binding(19, 14, 4);
    let a = shard(temp.path(), "a.fsvi", &a_space, &[(1, 1.0)]);
    let b = shard(temp.path(), "b.fsvi", &b_space, &[(2, 1.0)]);
    assert!(matches!(
        SemanticGenerationReader::open(
            Some(std::slice::from_ref(&a)),
            Some(std::slice::from_ref(&b))
        ),
        Err(SemanticReaderError::MixedGeneration)
    ));
    let mut reader = SemanticGenerationReader::open(Some(std::slice::from_ref(&a)), None)?;
    assert!(matches!(
        reader.try_replace(Some(&[b]), None),
        Err(SemanticReaderError::StaleReplacement)
    ));
    reader.try_replace(Some(std::slice::from_ref(&a)), None)?;
    let different_width = binding(19, 13, 8);
    let c = shard(temp.path(), "c.fsvi", &different_width, &[(3, 1.0)]);
    assert!(matches!(
        SemanticGenerationReader::open(Some(&[a, c]), None),
        Err(SemanticReaderError::MixedTierIdentity)
    ));
    Ok(())
}

#[test]
fn exact_witness_rejects_changed_vectors_under_an_unchanged_binding() -> TestResult {
    let temp = tempfile::tempdir()?;
    let space = binding(20, 15, 4);
    let selected = [shard(temp.path(), "index.fsvi", &space, &[(1, 1.0)])];
    let expected = selected[0].witness.clone();
    write_at(
        &selected[0].path,
        &space,
        &[(document(1).to_doc_id_string(), vec![0.0, 1.0, 0.0, 0.0])],
    )?;
    let new = ValidatedFsviBytes::open_published(&selected[0].path, &space)?;
    assert_eq!(new.witness().generation, expected.generation);
    assert_eq!(
        new.witness().ordered_live_docset_digest,
        expected.ordered_live_docset_digest
    );
    assert_ne!(new.witness(), &expected);
    assert!(matches!(
        SemanticGenerationReader::open(Some(&selected), None),
        Err(SemanticReaderError::Admission { .. })
    ));
    Ok(())
}

#[test]
fn legacy_v1_and_adjacent_wal_never_become_admitted_generations() -> TestResult {
    let temp = tempfile::tempdir()?;
    let space = binding(21, 16, 4);
    let selected = [shard(temp.path(), "index.fsvi", &space, &[(1, 1.0)])];
    let wal = wal_path_for(&selected[0].path);
    fs::write(&wal, b"retained uncommitted WAL evidence")?;
    let bytes = fs::read(&selected[0].path)?;
    assert!(SemanticGenerationReader::open(Some(&selected), None).is_err());
    assert_eq!(fs::read(&wal)?, b"retained uncommitted WAL evidence");
    assert_eq!(fs::read(&selected[0].path)?, bytes);

    let v1 = temp.path().join("legacy.fsvi");
    let mut writer = VectorIndex::create_with_revision(&v1, "legacy", "1.0", 4, Quantization::F16)?;
    writer.write_record(&document(1).to_doc_id_string(), &[1.0, 0.0, 0.0, 0.0])?;
    writer.finish()?;
    let mut legacy = selected[0].clone();
    legacy.path = v1.clone();
    let before = fs::read(&v1)?;
    assert!(matches!(
        SemanticGenerationReader::open(Some(&[legacy]), None),
        Err(SemanticReaderError::Admission { .. })
    ));
    assert_eq!(fs::read(v1)?, before);
    Ok(())
}

#[test]
fn duplicate_passages_across_shards_and_noncanonical_ids_are_rejected() -> TestResult {
    let temp = tempfile::tempdir()?;
    let space = binding(22, 17, 4);
    let a = shard(temp.path(), "a.fsvi", &space, &[(1, 1.0)]);
    let b = shard(temp.path(), "b.fsvi", &space, &[(1, 0.5)]);
    assert!(matches!(
        SemanticGenerationReader::open(Some(&[a, b]), None),
        Err(SemanticReaderError::DuplicateDocument)
    ));
    for (name, id) in [
        ("foreign.fsvi", "not-a-cass-document".to_owned()),
        ("prefix.fsvi", "m|1|0|4|5|6|0|1700000000000".to_owned()),
        (
            "noncanonical.fsvi",
            document(1).to_doc_id_string().replacen("m|1|", "m|01|", 1),
        ),
    ] {
        let path = temp.path().join(name);
        write_at(&path, &space, &[(id, vec![1.0, 0.0, 0.0, 0.0])])?;
        let witness = ValidatedFsviBytes::open_published(&path, &space)?
            .witness()
            .clone();
        let selection = SemanticShardExpectation {
            path,
            binding: space.clone(),
            witness,
        };
        assert!(matches!(
            SemanticGenerationReader::open(Some(&[selection]), None),
            Err(SemanticReaderError::NonCanonicalDocuments)
        ));
    }
    Ok(())
}

#[test]
fn witness_round_trip_preserves_all_fields_not_just_content_count() -> TestResult {
    let temp = tempfile::tempdir()?;
    let space = binding(23, 18, 4);
    let selected = [shard(temp.path(), "index.fsvi", &space, &[(1, 1.0)])];
    let encoded = serde_json::to_vec(&selected[0].witness)?;
    let decoded: FsviV2Witness = serde_json::from_slice(&encoded)?;
    assert_eq!(decoded, selected[0].witness);
    let mut drifted = selected[0].clone();
    drifted.witness.whole_image_sha256[31] ^= 1;
    assert!(SemanticGenerationReader::open(Some(&[drifted]), None).is_err());
    let _ = SemanticGenerationReader::open(Some(&selected), None)?;
    Ok(())
}

#[test]
fn absent_and_empty_selections_are_not_successful_empty_archives() {
    assert!(matches!(
        SemanticGenerationReader::open(None, None),
        Err(SemanticReaderError::NoTiers)
    ));
    assert!(matches!(
        SemanticGenerationReader::open(Some(&[]), None),
        Err(SemanticReaderError::EmptyTier)
    ));
    assert!(matches!(
        SemanticGenerationReader::open(None, Some(&[])),
        Err(SemanticReaderError::EmptyTier)
    ));
}

#[test]
fn role_alias_cannot_masquerade_as_an_independent_quality_artifact() -> TestResult {
    let temp = tempfile::tempdir()?;
    let space = binding(24, 19, 4);
    let selected = [shard(temp.path(), "index.fsvi", &space, &[(1, 1.0)])];
    assert!(matches!(
        SemanticGenerationReader::open(Some(&selected), Some(&selected)),
        Err(SemanticReaderError::ArtifactRoleAlias)
    ));
    // A separate pathname containing the exact same image is not independent.
    let mut copied = selected[0].clone();
    copied.path = temp.path().join("copy.fsvi");
    fs::copy(&selected[0].path, &copied.path)?;
    assert!(matches!(
        SemanticGenerationReader::open(Some(&selected), Some(&[copied])),
        Err(SemanticReaderError::ArtifactRoleAlias)
    ));
    Ok(())
}

#[test]
fn limits_larger_than_the_corpus_return_only_real_passages() -> TestResult {
    let temp = tempfile::tempdir()?;
    let space = binding(25, 20, 4);
    let selected = [shard(
        temp.path(),
        "index.fsvi",
        &space,
        &[(1, 1.0), (2, 0.5)],
    )];
    let reader = SemanticGenerationReader::open(Some(&selected), None)?;
    let queries = TieredQueryEmbeddings::fast_only(query(&space));
    let result = reader.activate(&queries)?.search(usize::MAX, None)?;
    assert_eq!(ids(&result), [1, 2]);
    assert_eq!(
        result
            .coverage()
            .fast
            .as_ref()
            .unwrap()
            .retrieved_candidates,
        2
    );
    Ok(())
}
