//! Schema-v2 publication -> admission -> exact/progressive/ANN execution.
//! Real FSVI/HNSW files, fixture identity declarations and axis vectors only;
//! these are lifecycle/selection tests, not native model relevance evidence.
use super::*;
use crate::search::semantic_manifest::{
    SEMANTIC_SHARDED_GENERATION_MANIFEST_SCHEMA_VERSION, SemanticArtifactShardV2,
};

const FAST: &[Row] = &[
    (1, [0.0, 1.0, 0.0, 0.0], true),
    (2, [0.6, 0.8, 0.0, 0.0], true),
    (3, [1.0, 0.0, 0.0, 0.0], true),
];
const QUALITY: &[Row] = &[
    (1, [1.0, 0.0, 0.0, 0.0], true),
    (2, [0.6, 0.8, 0.0, 0.0], true),
    (3, [0.0, 1.0, 0.0, 0.0], true),
];

fn partitioned(
    root: &Path,
    name: &str,
    sequence: u64,
    quality: Option<&[Row]>,
) -> SemanticGenerationManifestV1 {
    let mut m = fixture(root, name, sequence, Some(A), quality.map(|_| A));
    let directory = m.generation_dir(root).unwrap();
    // Independent full-file FSVI witness supplies the expected corpus preimage.
    let full = write_vector(
        &directory.join("corpus-oracle.fsvi"),
        &binding(SemanticArtifactRole::FastVector, sequence),
        FAST,
    );
    m.schema_version = SEMANTIC_SHARDED_GENERATION_MANIFEST_SCHEMA_VERSION;
    m.corpus.document_count = FAST.len() as u64;
    m.selected_document_count = m.corpus.document_count;
    m.corpus.ordered_live_docset_sha256 = hex::encode(full.witness().ordered_live_docset_digest);
    let mut artifacts = Vec::new();
    for (role, rows) in [
        (SemanticArtifactRole::FastVector, Some(FAST)),
        (SemanticArtifactRole::QualityVector, quality),
    ] {
        let Some(rows) = rows else {
            continue;
        };
        let base = m.artifact(role).unwrap();
        for (ordinal, row) in rows.iter().enumerate() {
            let mut artifact = base.clone();
            artifact.relative_path = format!("{}/part-{ordinal}.fsvi", role.as_str());
            let owner = write_vector(
                &directory.join(&artifact.relative_path),
                &binding(role, sequence),
                std::slice::from_ref(row),
            );
            let witness = owner.witness();
            artifact.shard = Some(SemanticArtifactShardV2 {
                ordinal: ordinal as u32,
                shard_count: rows.len() as u32,
                first_document_id: document(row.0),
                last_document_id: document(row.0),
            });
            artifact.artifact_sha256 = hex::encode(witness.whole_image_sha256);
            artifact.size_bytes = witness.byte_len;
            artifact.selected_document_count = m.selected_document_count;
            artifact.covered_document_count = witness.live_count;
            artifact.vector_slot_count = witness.record_count;
            artifact.live_vector_count = witness.live_count;
            artifact.tombstone_vector_count = witness.tombstone_count;
            artifact.covered_live_docset_sha256 = hex::encode(witness.ordered_live_docset_digest);
            artifact.covered_content_sha256 =
                digest(format!("content-subset-{}", row.0).as_bytes());
            artifact.validation.artifact_sha256 = artifact.artifact_sha256.clone();
            artifact.validation.size_bytes = artifact.size_bytes;
            artifact.validation.selected_document_count = artifact.selected_document_count;
            artifact.validation.covered_document_count = artifact.covered_document_count;
            artifact.validation.vector_slot_count = artifact.vector_slot_count;
            artifact.validation.live_vector_count = artifact.live_vector_count;
            artifact.validation.tombstone_vector_count = artifact.tombstone_vector_count;
            artifact.validation.covered_live_docset_sha256 =
                artifact.covered_live_docset_sha256.clone();
            artifact.validation.covered_content_sha256 = artifact.covered_content_sha256.clone();
            artifacts.push(artifact);
        }
    }
    m.total_vector_slot_count_across_tiers = artifacts.iter().map(|a| a.vector_slot_count).sum();
    m.total_live_vector_count_across_tiers = artifacts.iter().map(|a| a.live_vector_count).sum();
    m.total_tombstone_vector_count_across_tiers = 0;
    m.artifacts = artifacts;
    m.validate().unwrap();
    m
}

fn attach_graphs(root: &Path, m: &mut SemanticGenerationManifestV1, sequence: u64) {
    let directory = m.generation_dir(root).unwrap();
    let bases = m.artifacts.clone();
    for base in bases {
        let ordinal = base.shard.as_ref().unwrap().ordinal;
        let owner = Arc::new(
            ValidatedFsviBytes::open_published(
                &directory.join(&base.relative_path),
                &binding(base.role, sequence),
            )
            .unwrap(),
        );
        let params = HnswParams {
            m: 4,
            m0: 8,
            ef_construction: 16,
            ef_search: 16,
        };
        let graph = ValidatedNativeHnsw::build(owner, params, 17).unwrap();
        let relative = format!("{}/graph-{ordinal}.fshnsw", base.role.as_str());
        let receipt = graph.save(&directory.join(&relative)).unwrap();
        m.artifacts
            .push(native_ann_artifact(&base, &receipt, &relative, 25).unwrap());
    }
    m.artifacts
        .sort_by_key(|a| (a.role, a.shard.as_ref().unwrap().ordinal));
    m.validate().unwrap();
}

#[test]
fn version_one_serialization_stays_unannotated_and_version_two_is_explicit() -> TestResult {
    let root = tempfile::tempdir()?;
    let old = fixture(root.path(), "legacy", 1, Some(A), None);
    assert!(!String::from_utf8(old.canonical_bytes()?)?.contains("\"shard\""));
    let mut annotated = old.clone();
    annotated.artifacts[0].shard = Some(SemanticArtifactShardV2 {
        ordinal: 0,
        shard_count: 1,
        first_document_id: document(1),
        last_document_id: document(2),
    });
    assert!(annotated.validate().is_err());
    annotated.schema_version = SEMANTIC_SHARDED_GENERATION_MANIFEST_SCHEMA_VERSION;
    annotated.validate()?;
    assert_ne!(old.sha256()?, annotated.sha256()?);
    let decoded: SemanticGenerationManifestV1 =
        serde_json::from_slice(&annotated.canonical_bytes()?)?;
    assert_eq!(decoded, annotated);
    Ok(())
}

#[test]
fn exact_selection_reads_later_shard_winners_and_keeps_every_owner() -> TestResult {
    let root = tempfile::tempdir()?;
    let m = partitioned(root.path(), "multi", 1, None);
    let pointer = select(root.path(), &m, None);
    let before = snapshot(root.path());
    let reader = open(root.path(), &m);
    assert_eq!(reader.shard_count(TierKind::Fast), 3);
    assert_eq!(reader.shard_count(TierKind::Quality), 0);
    assert!(m.artifact(SemanticArtifactRole::FastVector).is_none());
    assert!(
        load_current_semantic_generation(root.path(), Some(&m.corpus))?
            .artifact_paths
            .is_empty()
    );
    let result = reader.activate(&queries())?.search(3, None)?;
    assert_eq!(
        result
            .batch()
            .hits()
            .iter()
            .map(|h| h.document.message_id)
            .collect::<Vec<_>>(),
        vec![3, 2, 1]
    );
    assert_eq!(result.selection().pointer(), &pointer);
    for ordinal in 0..3 {
        assert!(reader.witness_at(TierKind::Fast, ordinal).is_some());
    }
    assert!(reader.witness_at(TierKind::Fast, 3).is_none());
    assert_eq!(before, snapshot(root.path()));
    Ok(())
}

#[test]
fn progressive_quality_searches_its_own_complete_partition() -> TestResult {
    let root = tempfile::tempdir()?;
    let m = partitioned(root.path(), "progressive-multi", 1, Some(QUALITY));
    select(root.path(), &m, None);
    let reader = open(root.path(), &m);
    let q = TieredQueryEmbeddings::progressive(
        query(SemanticArtifactRole::FastVector),
        query(SemanticArtifactRole::QualityVector),
    );
    let active = reader.activate(&q)?;
    let mut stream = active.progressive(1, None)?;
    let initial = stream.next().unwrap()?;
    assert_eq!(first_id(&initial), 3);
    let refined = stream.next().unwrap()?;
    assert_eq!(first_id(&refined), 1);
    assert!(Arc::ptr_eq(&initial.identity, &refined.identity));
    assert!(stream.next().is_none());
    Ok(())
}

#[test]
fn partial_quality_partition_does_not_claim_full_coverage() -> TestResult {
    let root = tempfile::tempdir()?;
    let m = partitioned(root.path(), "partial-multi", 1, Some(&QUALITY[..2]));
    select(root.path(), &m, None);
    let reader = open(root.path(), &m);
    assert_eq!(reader.shard_count(TierKind::Quality), 2);
    let q = TieredQueryEmbeddings::quality_only(query(SemanticArtifactRole::QualityVector));
    let batch = reader.activate(&q)?.search(20, None)?;
    assert_eq!(batch.batch().hits().len(), 2);
    assert_eq!(batch.selection().manifest().selected_document_count, 3);
    assert_eq!(
        batch
            .selection()
            .manifest()
            .artifacts_for(SemanticArtifactRole::QualityVector)
            .map(|a| a.covered_document_count)
            .sum::<u64>(),
        2
    );
    Ok(())
}

#[test]
fn malformed_partitions_fail_before_any_publication() -> TestResult {
    let root = tempfile::tempdir()?;
    let good = partitioned(root.path(), "structural", 1, None);
    let mutations: Vec<Box<dyn Fn(&mut SemanticGenerationManifestV1)>> = vec![
        Box::new(|m| m.artifacts[1].shard.as_mut().unwrap().ordinal = 2),
        Box::new(|m| m.artifacts[1].shard.as_mut().unwrap().shard_count = 4),
        Box::new(|m| {
            m.artifacts.remove(1);
        }),
        Box::new(|m| m.artifacts.swap(0, 1)),
        Box::new(|m| m.artifacts[1].shard = m.artifacts[0].shard.clone()),
        Box::new(|m| {
            let s = m.artifacts[1].shard.as_mut().unwrap();
            s.first_document_id = document(1);
            s.last_document_id = document(1);
        }),
        Box::new(|m| m.artifacts[0].shard = None),
        Box::new(|m| m.artifacts[0].shard.as_mut().unwrap().shard_count = 257),
        Box::new(|m| {
            m.artifacts[0]
                .shard
                .as_mut()
                .unwrap()
                .first_document_id
                .clear()
        }),
        Box::new(|m| {
            m.artifacts[0].embedding_identity = binding(SemanticArtifactRole::QualityVector, 1)
                .frozen_identity()
                .clone()
        }),
    ];
    for (index, mutate) in mutations.into_iter().enumerate() {
        let mut invalid = good.clone();
        mutate(&mut invalid);
        assert!(
            invalid.validate().is_err(),
            "accepted invalid partition {index}"
        );
        assert!(invalid.write_immutable(root.path()).is_err());
    }
    assert!(!SemanticCurrentPointerV1::path(root.path()).exists());
    good.validate()?;
    Ok(())
}

#[test]
fn partition_metadata_is_part_of_the_canonical_manifest_digest() -> TestResult {
    let root = tempfile::tempdir()?;
    let m = partitioned(root.path(), "digest", 1, None);
    let original = m.sha256()?;
    for field in 0..4 {
        let mut changed = m.clone();
        let s = changed.artifacts[0].shard.as_mut().unwrap();
        match field {
            0 => s.ordinal += 1,
            1 => s.shard_count += 1,
            2 => s.first_document_id.push('x'),
            _ => s.last_document_id.push('x'),
        }
        assert_ne!(changed.sha256()?, original);
    }
    Ok(())
}

#[test]
fn claimed_ranges_and_full_docset_are_checked_against_actual_rows() -> TestResult {
    for wrong_range in [false, true] {
        let root = tempfile::tempdir()?;
        let mut m = partitioned(root.path(), "actual-rows", 1, None);
        if wrong_range {
            let s = m.artifacts[2].shard.as_mut().unwrap();
            s.first_document_id = document(4);
            s.last_document_id = document(4);
        } else {
            m.corpus.ordered_live_docset_sha256 = digest(b"different-complete-docset");
        }
        m.validate()?;
        select(root.path(), &m, None);
        let before = snapshot(root.path());
        assert!(matches!(
            SelectedSemanticGeneration::open_current(
                root.path(),
                &m.corpus,
                SemanticSelectionBudget::default()
            ),
            Err(SemanticSelectionError::ArtifactMismatch { .. })
        ));
        assert_eq!(before, snapshot(root.path()));
    }
    Ok(())
}

#[test]
fn missing_later_shard_rejects_selection_but_retained_owners_still_search() -> TestResult {
    let root = tempfile::tempdir()?;
    let m = partitioned(root.path(), "missing-shard", 1, None);
    select(root.path(), &m, None);
    let reader = open(root.path(), &m);
    let path = m
        .generation_dir(root.path())?
        .join(&m.artifacts[2].relative_path);
    fs::rename(&path, path.with_extension("retained"))?;
    assert!(
        SelectedSemanticGeneration::open_current(
            root.path(),
            &m.corpus,
            SemanticSelectionBudget::default()
        )
        .is_err()
    );
    assert_eq!(first_id(&reader.activate(&queries())?.search(1, None)?), 3);
    Ok(())
}

#[test]
fn vector_admission_budget_is_summed_across_all_tier_shards() -> TestResult {
    let root = tempfile::tempdir()?;
    let m = partitioned(root.path(), "all-shard-budget", 1, Some(QUALITY));
    select(root.path(), &m, None);
    let sum = m.artifacts.iter().map(|a| a.size_bytes).sum::<u64>();
    assert!(matches!(
        SelectedSemanticGeneration::open_current(
            root.path(),
            &m.corpus,
            SemanticSelectionBudget {
                max_declared_vector_bytes: sum - 1
            }
        ),
        Err(SemanticSelectionError::BudgetExceeded)
    ));
    SelectedSemanticGeneration::open_current(
        root.path(),
        &m.corpus,
        SemanticSelectionBudget {
            max_declared_vector_bytes: sum,
        },
    )?;
    Ok(())
}

#[test]
fn each_declared_graph_is_admitted_against_its_own_shard() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut m = partitioned(root.path(), "ann-all", 1, Some(QUALITY));
    attach_graphs(root.path(), &mut m, 1);
    assert_eq!(m.artifacts.len(), 12);
    select(root.path(), &m, None);
    let before = snapshot(root.path());
    let reader = open(root.path(), &m).with_ann(AnnAdmissionBudget::default())?;
    for tier in [TierKind::Fast, TierKind::Quality] {
        for ordinal in 0..3 {
            assert!(matches!(
                reader.ann_admission_at(tier, ordinal),
                Some(SemanticAnnAdmission::Admitted { .. })
            ));
        }
    }
    let batch = reader
        .activate(&queries())?
        .search_with_ann(3, None, native_policy())?;
    assert_eq!(first_id(&batch), 3);
    assert_eq!(batch.batch().execution().len(), 3);
    assert!(
        batch
            .batch()
            .execution()
            .iter()
            .all(|e| e.engine == SemanticShardEngine::NativeAnn)
    );
    assert_eq!(snapshot(root.path()), before);
    Ok(())
}

#[test]
fn missing_one_graph_receipt_falls_back_only_for_that_exact_shard() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut m = partitioned(root.path(), "ann-partial", 1, None);
    attach_graphs(root.path(), &mut m, 1);
    select(root.path(), &m, None);
    let path = m.generation_dir(root.path())?.join(
        &m.artifact_at(SemanticArtifactRole::FastAnn, 2)
            .unwrap()
            .relative_path,
    );
    let receipt = native_hnsw_generation_receipt_path(&path)?;
    fs::rename(&receipt, receipt.with_extension("unavailable"))?;
    let reader = open(root.path(), &m).with_ann(AnnAdmissionBudget::default())?;
    let batch = reader
        .activate(&queries())?
        .search_with_ann(3, None, native_policy())?;
    assert_eq!(first_id(&batch), 3);
    let execution = batch.batch().execution();
    assert_eq!(execution[0].engine, SemanticShardEngine::NativeAnn);
    assert_eq!(execution[1].engine, SemanticShardEngine::NativeAnn);
    assert_eq!(execution[2].engine, SemanticShardEngine::ExactFallback);
    Ok(())
}

#[test]
fn graph_ordinal_and_base_digest_cannot_be_substituted() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut m = partitioned(root.path(), "ann-binding", 1, None);
    attach_graphs(root.path(), &mut m, 1);
    let mut wrong = m.clone();
    let other = wrong.artifacts[1].artifact_sha256.clone();
    wrong.artifacts[3]
        .ann_base
        .as_mut()
        .unwrap()
        .base_artifact_sha256 = other;
    assert!(wrong.validate().is_err());
    let mut wrong = m.clone();
    wrong.artifacts[3].shard.as_mut().unwrap().ordinal = 2;
    assert!(wrong.validate().is_err());
    m.validate()?;
    Ok(())
}

#[test]
fn multirow_shards_with_tombstones_match_an_independent_full_file_witness() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut manifest = partitioned(root.path(), "multirow-with-dead-rows", 1, None);
    let directory = manifest.generation_dir(root.path())?;
    let groups = [
        vec![FAST[0], FAST[1], (0, [1.0, 0.0, 0.0, 0.0], false)],
        vec![FAST[2], (4, [0.0, 1.0, 0.0, 0.0], false)],
    ];
    let mut partition = Vec::new();
    for (ordinal, rows) in groups.iter().enumerate() {
        let mut artifact = manifest.artifacts[ordinal].clone();
        artifact.relative_path = format!("multirow/part-{ordinal}.fsvi");
        let owner = write_vector(
            &directory.join(&artifact.relative_path),
            &binding(artifact.role, 1),
            rows,
        );
        let witness = owner.witness();
        let live_ids: Vec<_> = rows
            .iter()
            .filter(|row| row.2)
            .map(|row| document(row.0))
            .collect();
        artifact.shard = Some(SemanticArtifactShardV2 {
            ordinal: ordinal as u32,
            shard_count: 2,
            first_document_id: live_ids.iter().min().unwrap().clone(),
            last_document_id: live_ids.iter().max().unwrap().clone(),
        });
        artifact.artifact_sha256 = hex::encode(witness.whole_image_sha256);
        artifact.size_bytes = witness.byte_len;
        artifact.covered_document_count = witness.live_count;
        artifact.vector_slot_count = witness.record_count;
        artifact.live_vector_count = witness.live_count;
        artifact.tombstone_vector_count = witness.tombstone_count;
        artifact.covered_live_docset_sha256 = hex::encode(witness.ordered_live_docset_digest);
        artifact.covered_content_sha256 = digest(format!("multirow-subset-{ordinal}").as_bytes());
        artifact.validation.artifact_sha256 = artifact.artifact_sha256.clone();
        artifact.validation.size_bytes = artifact.size_bytes;
        artifact.validation.covered_document_count = artifact.covered_document_count;
        artifact.validation.vector_slot_count = artifact.vector_slot_count;
        artifact.validation.live_vector_count = artifact.live_vector_count;
        artifact.validation.tombstone_vector_count = artifact.tombstone_vector_count;
        artifact.validation.covered_live_docset_sha256 =
            artifact.covered_live_docset_sha256.clone();
        artifact.validation.covered_content_sha256 = artifact.covered_content_sha256.clone();
        partition.push(artifact);
    }
    manifest.artifacts = partition;
    manifest.total_vector_slot_count_across_tiers = 5;
    manifest.total_live_vector_count_across_tiers = 3;
    manifest.total_tombstone_vector_count_across_tiers = 2;
    manifest.validate()?;
    select(root.path(), &manifest, None);
    let before = snapshot(root.path());
    let reader = open(root.path(), &manifest);
    assert_eq!(reader.shard_count(TierKind::Fast), 2);
    assert_eq!(
        reader
            .witness_at(TierKind::Fast, 0)
            .unwrap()
            .tombstone_count,
        1
    );
    let result = reader.activate(&queries())?.search(10, None)?;
    assert_eq!(
        result
            .batch()
            .hits()
            .iter()
            .map(|hit| hit.document.message_id)
            .collect::<Vec<_>>(),
        vec![3, 2, 1]
    );
    assert_eq!(before, snapshot(root.path()));
    Ok(())
}

#[test]
fn merged_live_docset_matches_fsvi_order_and_rejects_duplicate_live_ids() -> TestResult {
    let root = tempfile::tempdir()?;
    let full = write_vector(
        &root.path().join("full.fsvi"),
        &binding(SemanticArtifactRole::FastVector, 1),
        FAST,
    );
    let shards: Vec<_> = FAST
        .iter()
        .enumerate()
        .map(|(index, row)| {
            Arc::new(write_vector(
                &root.path().join(format!("shard-{index}.fsvi")),
                &binding(SemanticArtifactRole::FastVector, 1),
                std::slice::from_ref(row),
            ))
        })
        .collect();
    let expected: Vec<_> = (0..full.record_count())
        .map(|index| full.doc_id_at(index).unwrap())
        .collect();
    let lexical = FAST.iter().map(|row| document(row.0)).collect::<Vec<_>>();
    assert_ne!(
        expected,
        lexical.iter().map(String::as_str).collect::<Vec<_>>(),
        "fixture must distinguish physical and lexical order"
    );
    let mut merged = super::super::live_docset::LiveDocuments::new(&shards)?;
    let mut actual = Vec::new();
    while let Some((_, id)) = merged.next()? {
        actual.push(id);
    }
    assert_eq!(actual, expected);
    assert_eq!(
        super::super::live_docset::digest(&shards, 3)?,
        full.witness().ordered_live_docset_digest
    );
    let duplicate = vec![Arc::clone(&shards[0]), Arc::clone(&shards[0])];
    assert!(super::super::live_docset::digest(&duplicate, 2).is_err());
    Ok(())
}
