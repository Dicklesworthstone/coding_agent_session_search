//! Real FSVI and pointer publication, with test-only semantic identity
//! declarations and axis vectors. These test selection/ownership mechanics,
//! NOT a native model, golden-vector conformance, or retrieval relevance.
#![cfg(any(target_os = "linux", target_os = "android"))]

use super::*;
use std::collections::BTreeMap;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use frankensearch::core::{
    EMBEDDING_INPUT_CONTRACT_SCHEMA_V1, EMBEDDING_PRODUCER_ATTESTATION_SCHEMA_V1,
    EMBEDDING_SPACE_IDENTITY_SCHEMA_V1, VECTOR_STORAGE_IDENTITY_SCHEMA_V1,
    BoundQueryEmbedding, EmbeddingArtifactIdentityV1, EmbeddingIdentityBundleV1,
    EmbeddingInputContractV1, EmbeddingProducerAttestationV1, EmbeddingSpaceIdentityV1,
    EmbeddingSpaceKindV1, GoldenVectorCertificateV1, VectorStorageIdentityV1,
};
use frankensearch::core::generation::{ArtifactGenerationIdentityV1, QuantizationFormat};
use crate::search::semantic_manifest::{
    SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION, SemanticArtifactValidationEvidence,
    SemanticExpectedCurrentV1, SemanticGenerationStatus, SemanticGenerationTopology,
    SemanticProducerAttestation,
};
use crate::search::semantic_reader::{SemanticResultPhase, SemanticShardExpectation};
use crate::search::vector_index::{ROLE_USER, SemanticDocId};
use sha2::{Digest, Sha256};

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Row = (u64, [f32; 4], bool);
const A: &[Row] = &[(1, [1.0, 0.0, 0.0, 0.0], true), (2, [0.0, 1.0, 0.0, 0.0], true)];
const B: &[Row] = &[(1, [0.0, 1.0, 0.0, 0.0], true), (2, [1.0, 0.0, 0.0, 0.0], true)];

fn digest(bytes: &[u8]) -> String { hex::encode(Sha256::digest(bytes)) }

fn identity(role: SemanticArtifactRole) -> EmbeddingIdentityBundleV1 {
    let input = EmbeddingInputContractV1 {
        schema_version: EMBEDDING_INPUT_CONTRACT_SCHEMA_V1,
        canonicalization: "cass-test-canonical-v1".into(),
        content_selection: "cass-test-content-v1".into(), chunking: "cass-test-chunks-v1".into(),
        query_instruction: "query:".into(), document_instruction: "document:".into(),
        doc_id_semantics: "cass-test-passage-id-v1".into(),
    };
    let space = EmbeddingSpaceIdentityV1 {
        schema_version: EMBEDDING_SPACE_IDENTITY_SCHEMA_V1,
        logical_model_id: format!("publication-fixture-{}", role.as_str()),
        immutable_revision: "fixture-v1".into(), kind: EmbeddingSpaceKindV1::Semantic,
        artifact_manifest_fingerprint: digest(b"fixture-manifest"),
        artifacts: vec![
            EmbeddingArtifactIdentityV1 { role: "tokenizer".into(), sha256: digest(b"tokenizer"), size: 9 },
            EmbeddingArtifactIdentityV1 { role: "weights".into(), sha256: digest(b"weights"), size: 7 },
        ],
        tokenizer_fingerprint: digest(b"tokenizer-contract"), vocabulary_fingerprint: digest(b"vocabulary"),
        model_config_fingerprint: digest(b"config"), model_preprocessing: "unicode=nfc".into(),
        sequence_policy: "truncate=tail;max_tokens=512;padding=none".into(),
        query_instruction: "query:".into(), document_instruction: "document:".into(),
        pooling: "mean-mask-aware".into(), output_normalization: "l2-f32-v1".into(),
        dimension: 4, input_contract_fingerprint: input.fingerprint(), hash_control: None, projection: None,
    };
    let producer = EmbeddingProducerAttestationV1 {
        schema_version: EMBEDDING_PRODUCER_ATTESTATION_SCHEMA_V1,
        backend: "publication-test-fixture".into(), implementation_revision: "fixture-v1".into(),
        protocol_revision: "in-process-v1".into(), numeric_profile: "test-f32-v1".into(),
        provenance_manifest_fingerprint: digest(b"fixture-provenance"), space_fingerprint: space.fingerprint(),
        golden_vectors: GoldenVectorCertificateV1 {
            corpus_sha256: digest(b"fixture-corpus"), vectors_sha256: digest(b"fixture-vectors"),
            vector_count: 2, dimension: 4,
        },
    };
    EmbeddingIdentityBundleV1 {
        space, producer, input,
        storage: VectorStorageIdentityV1 {
            schema_version: VECTOR_STORAGE_IDENTITY_SCHEMA_V1, format: "fsvi-v2".into(),
            quantization: QuantizationFormat::F16, endianness: "little-endian".into(),
            vector_normalization: "l2-f32-v1".into(), dimension: 4,
        },
    }
}

fn document(id: u64) -> String {
    SemanticDocId {
        message_id: id, chunk_idx: 0, agent_id: 1, workspace_id: 1, source_id: 0,
        role: ROLE_USER, created_at_ms: 100, content_hash: Some([id as u8; 32]),
    }.to_doc_id_string()
}

fn binding(role: SemanticArtifactRole, sequence: u64) -> FsviV2IdentityBinding {
    FsviV2IdentityBinding::new(
        ArtifactGenerationIdentityV1::new(sequence, [42; 16]).unwrap(),
        identity(role).freeze().unwrap(),
    ).unwrap()
}

fn write_vector(path: &Path, binding: &FsviV2IdentityBinding, rows: &[Row]) -> ValidatedFsviBytes {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut writer = VectorIndex::create_v2(path, binding.clone()).unwrap();
    for (id, vector, live) in rows {
        if *live { writer.write_record(&document(*id), vector).unwrap(); }
        else { writer.write_tombstone_record(&document(*id), vector).unwrap(); }
    }
    writer.finish().unwrap();
    ValidatedFsviBytes::open_published(path, binding).unwrap()
}

fn fixture(
    root: &Path, name: &str, sequence: u64, fast: Option<&[Row]>, quality: Option<&[Row]>,
) -> SemanticGenerationManifestV1 {
    let topology = match (fast, quality) {
        (Some(_), Some(_)) => SemanticGenerationTopology::FullProgressive,
        (Some(_), None) => SemanticGenerationTopology::FastOnly,
        (None, Some(_)) => SemanticGenerationTopology::QualityOnly,
        _ => panic!("fixture needs a tier"),
    };
    let directory = root.join("vector_index/generations").join(name);
    let mut artifacts = Vec::new();
    for (role, rows) in [(SemanticArtifactRole::FastVector, fast), (SemanticArtifactRole::QualityVector, quality)] {
        let Some(rows) = rows else { continue; };
        let relative_path = format!("{}/selected.fsvi", role.as_str());
        let binding = binding(role, sequence);
        let owner = write_vector(&directory.join(&relative_path), &binding, rows);
        let witness = owner.witness();
        let content = if witness.live_count == 2 { digest(b"full-source-content") }
            else { digest(b"partial-source-content") };
        let validation = SemanticArtifactValidationEvidence {
            validator_id: "fsvi-v2-owner-fixture".into(), validated_at_ms: 25,
            artifact_sha256: hex::encode(witness.whole_image_sha256), size_bytes: witness.byte_len,
            selected_document_count: 2, covered_document_count: witness.live_count,
            vector_slot_count: witness.record_count, live_vector_count: witness.live_count,
            tombstone_vector_count: witness.tombstone_count, wal_entry_count: 0,
            generation_corpus_sha256: digest(b"source-corpus"),
            covered_live_docset_sha256: hex::encode(witness.ordered_live_docset_digest),
            covered_content_sha256: content,
        };
        artifacts.push(SemanticGenerationArtifact {
            role, relative_path, artifact_sha256: validation.artifact_sha256.clone(),
            size_bytes: validation.size_bytes, artifact_format: "fsvi-v2".into(),
            artifact_parameters_sha256: digest(b"fsvi-v2-f16-fixture"),
            embedding_identity: binding.frozen_identity().clone(), ann_base: None,
            selected_document_count: 2, covered_document_count: validation.covered_document_count,
            vector_slot_count: validation.vector_slot_count, live_vector_count: validation.live_vector_count,
            tombstone_vector_count: validation.tombstone_vector_count, wal_entry_count: 0,
            generation_corpus_sha256: validation.generation_corpus_sha256.clone(),
            covered_live_docset_sha256: validation.covered_live_docset_sha256.clone(),
            covered_content_sha256: validation.covered_content_sha256.clone(), validation,
        });
    }
    let full = artifacts.iter().find(|artifact| artifact.covered_document_count == 2).unwrap();
    let corpus = SemanticCorpusSnapshotIdentity {
        database_uuid: "publication-test-db".into(), source_change_sequence: 7,
        schema_contract_sha256: digest(b"schema"), content_selection_sha256: digest(b"selection"),
        canonicalization_sha256: digest(b"canonicalization"), chunking_sha256: digest(b"chunking"),
        document_id_contract_sha256: digest(b"document-id"), config_sha256: digest(b"config"),
        corpus_sha256: digest(b"source-corpus"), ordered_live_docset_sha256: full.covered_live_docset_sha256.clone(),
        content_sha256: digest(b"full-source-content"), document_count: 2,
    };
    let manifest = SemanticGenerationManifestV1 {
        schema_version: SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION,
        generation_id: name.into(), build_id: format!("build-{name}"), request_id: None,
        previous_generation_id: None, created_at_ms: 10, completed_at_ms: 20, sealed_at_ms: 30,
        requested_topology: topology, realized_topology: topology, status: SemanticGenerationStatus::Sealed,
        corpus, producer: SemanticProducerAttestation {
            cass_git_commit: "a".repeat(40), frankensearch_git_commit: "b".repeat(40),
            cargo_lock_sha256: digest(b"lock"), rust_toolchain: "fixture-toolchain".into(),
            enabled_features: vec!["semantic".into()], build_profile: "test".into(), binary_elf_sha256: digest(b"binary"),
        },
        selected_document_count: 2,
        total_vector_slot_count_across_tiers: artifacts.iter().map(|a| a.vector_slot_count).sum(),
        total_live_vector_count_across_tiers: artifacts.iter().map(|a| a.live_vector_count).sum(),
        total_tombstone_vector_count_across_tiers: artifacts.iter().map(|a| a.tombstone_vector_count).sum(),
        total_wal_entry_count_across_tiers: 0, artifacts,
    };
    manifest.validate().unwrap();
    manifest
}

fn select(root: &Path, manifest: &SemanticGenerationManifestV1, prior: Option<&SemanticCurrentPointerV1>) -> SemanticCurrentPointerV1 {
    manifest.write_immutable(root).unwrap();
    let epoch = prior.map_or(1, |pointer| pointer.selection_epoch + 1);
    let pointer = SemanticCurrentPointerV1::for_manifest(manifest, 40 + epoch as i64, epoch).unwrap();
    let expected = prior.map_or(SemanticExpectedCurrentV1::Absent, SemanticExpectedCurrentV1::exact);
    pointer.publish_atomic(root, &expected).unwrap();
    pointer
}

fn query(role: SemanticArtifactRole) -> BoundQueryEmbedding {
    let mut identity = identity(role);
    identity.storage.format = "in-memory-f32".into();
    identity.storage.quantization = QuantizationFormat::F32;
    identity.storage.endianness = "native-f32-values".into();
    BoundQueryEmbedding::new(vec![1.0, 0.0, 0.0, 0.0], identity).unwrap()
}

fn queries() -> TieredQueryEmbeddings { TieredQueryEmbeddings::fast_only(query(SemanticArtifactRole::FastVector)) }
fn open(root: &Path, m: &SemanticGenerationManifestV1) -> SelectedSemanticGeneration {
    SelectedSemanticGeneration::open_current(root, &m.corpus, SemanticSelectionBudget::default()).unwrap()
}
fn first_id(batch: &SelectedSemanticBatch) -> u64 { batch.batch().hits()[0].document.message_id }

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() { pending.push(path); }
            else { files.insert(path.strip_prefix(root).unwrap().to_path_buf(), fs::read(path).unwrap()); }
        }
    }
    files
}

#[test]
fn current_pointer_selects_real_nonconventional_vectors_without_writes() -> TestResult {
    let root = tempfile::tempdir()?;
    let m = fixture(root.path(), "gen-first", 1, Some(A), None);
    let pointer = select(root.path(), &m, None);
    fs::write(root.path().join("vector_index/vector.fast.idx"), b"unselected decoy")?;
    let before = snapshot(root.path());
    let reader = open(root.path(), &m);
    let q = queries();
    let result = reader.activate(&q)?.search(1, None)?;
    assert_eq!(first_id(&result), 1);
    assert_eq!(result.selection().pointer(), &pointer);
    assert_eq!(result.selection().corpus(), &m.corpus);
    assert_eq!(snapshot(root.path()), before);
    Ok(())
}

#[test]
fn quality_only_does_not_require_a_fast_artifact() -> TestResult {
    let root = tempfile::tempdir()?;
    let m = fixture(root.path(), "gen-quality", 1, None, Some(B));
    select(root.path(), &m, None);
    let reader = open(root.path(), &m);
    assert!(reader.witness(TierKind::Fast).is_none());
    let q = TieredQueryEmbeddings::quality_only(query(SemanticArtifactRole::QualityVector));
    assert_eq!(first_id(&reader.activate(&q)?.search(1, None)?), 2);
    assert!(reader.activate(&queries()).is_err());
    Ok(())
}

#[test]
fn progressive_results_keep_selection_and_retrieve_outside_the_fast_pool() -> TestResult {
    struct Counter(AtomicUsize);
    impl SearchFilter for Counter {
        fn matches(&self, _: &str, _: Option<&serde_json::Value>) -> bool { self.0.fetch_add(1, Ordering::SeqCst); true }
        fn name(&self) -> &str { "publication-counter" }
    }
    let root = tempfile::tempdir()?;
    let m = fixture(root.path(), "gen-progressive", 1, Some(B), Some(A));
    select(root.path(), &m, None);
    let reader = open(root.path(), &m);
    let q = TieredQueryEmbeddings::progressive(query(SemanticArtifactRole::FastVector), query(SemanticArtifactRole::QualityVector));
    let active = reader.activate(&q)?;
    let counter = Counter(AtomicUsize::new(0));
    let mut phases = active.progressive(1, Some(&counter))?;
    let initial = phases.next().unwrap()?;
    assert_eq!(first_id(&initial), 2);
    assert_eq!(initial.batch().coverage().phase, SemanticResultPhase::Initial);
    let after_fast = counter.0.load(Ordering::SeqCst);
    assert!(after_fast > 0);
    let refined = phases.next().unwrap()?;
    assert_eq!(first_id(&refined), 1, "quality's winner was absent from the fast top-1");
    assert!(counter.0.load(Ordering::SeqCst) > after_fast);
    assert!(Arc::ptr_eq(&initial.identity, &refined.identity));
    assert!(phases.next().is_none());
    let mut early = active.progressive(1, Some(&counter))?;
    let _ = early.next().unwrap()?;
    let before_drop = counter.0.load(Ordering::SeqCst);
    drop(early);
    assert_eq!(counter.0.load(Ordering::SeqCst), before_drop);
    Ok(())
}

#[test]
fn partial_quality_coverage_is_preserved_not_promoted_to_full() -> TestResult {
    let root = tempfile::tempdir()?;
    let m = fixture(root.path(), "gen-partial", 1, Some(B), Some(&A[..1]));
    select(root.path(), &m, None);
    let reader = open(root.path(), &m);
    let q = TieredQueryEmbeddings::quality_only(query(SemanticArtifactRole::QualityVector));
    let result = reader.activate(&q)?.search(50, None)?;
    assert_eq!(result.batch().hits().len(), 1);
    let declared = result.selection().manifest().artifact(SemanticArtifactRole::QualityVector).unwrap();
    assert_eq!((declared.covered_document_count, declared.selected_document_count), (1, 2));
    assert_eq!(result.batch().witness(TierKind::Quality, 0).unwrap().live_count, 1);
    Ok(())
}

#[test]
fn source_identity_and_declared_budget_are_mandatory_admission_gates() -> TestResult {
    let root = tempfile::tempdir()?;
    let m = fixture(root.path(), "gen-budget", 1, Some(A), None);
    select(root.path(), &m, None);
    let mut wrong = m.corpus.clone(); wrong.source_change_sequence += 1;
    assert!(matches!(SelectedSemanticGeneration::open_current(root.path(), &wrong, SemanticSelectionBudget::default()),
        Err(SemanticSelectionError::Publication(SemanticGenerationError::StaleCorpus { .. }))));
    let bytes = m.artifacts[0].size_bytes;
    assert!(matches!(SelectedSemanticGeneration::open_current(root.path(), &m.corpus,
        SemanticSelectionBudget { max_declared_vector_bytes: bytes - 1 }), Err(SemanticSelectionError::BudgetExceeded)));
    SelectedSemanticGeneration::open_current(root.path(), &m.corpus,
        SemanticSelectionBudget { max_declared_vector_bytes: bytes })?;
    Ok(())
}

#[test]
fn manifest_consistent_but_false_vector_counts_are_refused() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut m = fixture(root.path(), "gen-false-count", 1, Some(A), None);
    let a = &mut m.artifacts[0];
    a.vector_slot_count += 1; a.tombstone_vector_count += 1;
    a.validation.vector_slot_count += 1; a.validation.tombstone_vector_count += 1;
    m.total_vector_slot_count_across_tiers += 1; m.total_tombstone_vector_count_across_tiers += 1;
    select(root.path(), &m, None);
    load_current_semantic_generation(root.path(), Some(&m.corpus))?;
    assert!(matches!(SelectedSemanticGeneration::open_current(root.path(), &m.corpus, SemanticSelectionBudget::default()),
        Err(SemanticSelectionError::ArtifactMismatch { field: "vector_slot_count", .. })));
    Ok(())
}

#[test]
fn manifest_consistent_but_false_docset_is_refused() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut m = fixture(root.path(), "gen-false-docset", 1, Some(A), None);
    let wrong = digest(b"not-the-actual-docset");
    m.corpus.ordered_live_docset_sha256 = wrong.clone();
    m.artifacts[0].covered_live_docset_sha256 = wrong.clone();
    m.artifacts[0].validation.covered_live_docset_sha256 = wrong;
    select(root.path(), &m, None);
    assert!(matches!(SelectedSemanticGeneration::open_current(root.path(), &m.corpus, SemanticSelectionBudget::default()),
        Err(SemanticSelectionError::ArtifactMismatch { field: "covered_live_docset_sha256", .. })));
    Ok(())
}

#[test]
fn replaced_bytes_cannot_borrow_the_selected_manifests_authority() -> TestResult {
    let root = tempfile::tempdir()?;
    let m = fixture(root.path(), "gen-replaced", 1, Some(A), None);
    select(root.path(), &m, None);
    let selected = load_current_semantic_generation(root.path(), Some(&m.corpus))?;
    let path = &selected.artifact_paths[&SemanticArtifactRole::FastVector];
    let owner = write_vector(path, &binding(SemanticArtifactRole::FastVector, 1), B);
    assert_ne!(hex::encode(owner.witness().whole_image_sha256), m.artifacts[0].artifact_sha256);
    assert!(matches!(admit_tier(&selected, SemanticArtifactRole::FastVector),
        Err(SemanticSelectionError::ArtifactMismatch { field: "artifact_sha256", .. })));
    Ok(())
}

#[test]
fn newer_selection_epoch_can_explicitly_roll_back_to_an_older_vector_build() -> TestResult {
    let root = tempfile::tempdir()?;
    let a = fixture(root.path(), "gen-a", 10, Some(A), None);
    let b = fixture(root.path(), "gen-b", 20, Some(B), None);
    let first = select(root.path(), &a, None);
    let mut reader = open(root.path(), &a);
    let q = queries();
    let old_batch = reader.activate(&q)?.search(1, None)?;
    let second = select(root.path(), &b, Some(&first));
    assert!(reader.refresh_current(&b.corpus, SemanticSelectionBudget::default())?);
    assert_eq!(first_id(&reader.activate(&q)?.search(1, None)?), 2);
    let rollback = select(root.path(), &a, Some(&second));
    assert!(reader.refresh_current(&a.corpus, SemanticSelectionBudget::default())?);
    assert_eq!(reader.selection().pointer(), &rollback);
    assert_eq!(reader.witness(TierKind::Fast).unwrap().generation.sequence, 10);
    assert_eq!(first_id(&reader.activate(&q)?.search(1, None)?), 1);
    assert_eq!(old_batch.selection().pointer(), &first);
    assert_eq!(first_id(&old_batch), 1);
    assert!(!reader.refresh_current(&a.corpus, SemanticSelectionBudget::default())?);
    Ok(())
}

#[test]
fn failed_refresh_keeps_old_owners_and_does_not_repair_the_bad_pointer() -> TestResult {
    let root = tempfile::tempdir()?;
    let m = fixture(root.path(), "gen-old", 1, Some(A), None);
    select(root.path(), &m, None);
    let mut reader = open(root.path(), &m);
    let q = queries();
    let pointer = SemanticCurrentPointerV1::path(root.path());
    fs::write(&pointer, b"{malformed")?;
    let before = snapshot(root.path());
    assert!(reader.refresh_current(&m.corpus, SemanticSelectionBudget::default()).is_err());
    assert_eq!(first_id(&reader.activate(&q)?.search(1, None)?), 1);
    assert_eq!(snapshot(root.path()), before);
    Ok(())
}

#[test]
fn stale_or_same_epoch_reselection_cannot_replace_a_retained_publication() -> TestResult {
    let root = tempfile::tempdir()?;
    let a = fixture(root.path(), "gen-a", 1, Some(A), None);
    let b = fixture(root.path(), "gen-b", 2, Some(B), None);
    let first = select(root.path(), &a, None);
    let second = select(root.path(), &b, Some(&first));
    let mut reader = open(root.path(), &b);
    for forged in [first.clone(), SemanticCurrentPointerV1::for_manifest(&a, 99, second.selection_epoch)?] {
        fs::write(SemanticCurrentPointerV1::path(root.path()), forged.canonical_bytes()?)?;
        assert!(matches!(reader.refresh_current(&a.corpus, SemanticSelectionBudget::default()), Err(SemanticSelectionError::StaleSelection)));
        assert_eq!(reader.selection().pointer(), &second);
    }
    Ok(())
}

#[test]
fn pointer_a_b_a_during_admission_is_not_mistaken_for_a_stable_selection() -> TestResult {
    let root = tempfile::tempdir()?;
    let a = fixture(root.path(), "gen-a", 1, Some(A), None);
    let b = fixture(root.path(), "gen-b", 2, Some(B), None);
    let first = select(root.path(), &a, None);
    let result = SelectedSemanticGeneration::open_current_with_checkpoint(
        root.path(), &a.corpus, SemanticSelectionBudget::default(), || {
            let second = select(root.path(), &b, Some(&first));
            select(root.path(), &a, Some(&second));
        },
    );
    assert!(matches!(result, Err(SemanticSelectionError::SelectionChanged)));
    Ok(())
}

#[test]
fn result_batches_retain_the_exact_owner_after_reader_and_source_replacement() -> TestResult {
    let root = tempfile::tempdir()?;
    let m = fixture(root.path(), "gen-owner", 1, Some(A), None);
    select(root.path(), &m, None);
    let reader = open(root.path(), &m);
    let weak = Arc::downgrade(&reader.reader.fast.as_ref().unwrap().shards[0]);
    let witness = reader.witness(TierKind::Fast).unwrap().clone();
    let q = queries();
    let result = reader.activate(&q)?.search(1, None)?;
    let path = m.generation_dir(root.path())?.join(&m.artifacts[0].relative_path);
    fs::rename(&path, path.with_extension("retained-original"))?;
    fs::write(&path, b"not an index")?;
    drop(reader);
    let retained = weak.upgrade().expect("the batch must still own its serving image");
    assert_eq!(retained.witness(), &witness);
    assert_eq!(retained.search_top_k(&[1.0, 0.0, 0.0, 0.0], 1, None)?[0].doc_id.as_str(), document(1));
    assert_eq!(first_id(&result), 1);
    drop(retained); drop(result);
    assert!(weak.upgrade().is_none());
    Ok(())
}

#[test]
fn tombstones_preserve_physical_identity_but_never_become_results() -> TestResult {
    let root = tempfile::tempdir()?;
    let rows = [A[0], A[1], (3, [1.0, 0.0, 0.0, 0.0], false)];
    let m = fixture(root.path(), "gen-tombstones", 1, Some(&rows), None);
    select(root.path(), &m, None);
    let reader = open(root.path(), &m);
    let witness = reader.witness(TierKind::Fast).unwrap();
    assert_eq!((witness.record_count, witness.live_count, witness.tombstone_count), (3, 2, 1));
    let q = queries();
    let batch = reader.activate(&q)?.search(usize::MAX, None)?;
    assert_eq!(batch.batch().hits().len(), 2);
    assert!(batch.batch().hits().iter().all(|hit| hit.document.message_id != 3));
    // The original expectation-based public reader must obey the same rule.
    let expected = SemanticShardExpectation {
        path: m.generation_dir(root.path())?.join(&m.artifacts[0].relative_path),
        binding: binding(SemanticArtifactRole::FastVector, 1), witness: witness.clone(),
    };
    let raw = SemanticGenerationReader::open(Some(&[expected]), None)?;
    assert_eq!(raw.activate(&q)?.search(10, None)?.hits().len(), 2);
    Ok(())
}

#[test]
fn missing_pointer_is_not_an_empty_searchable_generation() -> TestResult {
    let root = tempfile::tempdir()?;
    let m = fixture(root.path(), "gen-unselected", 1, Some(A), None);
    m.write_immutable(root.path())?;
    let before = snapshot(root.path());
    assert!(matches!(SelectedSemanticGeneration::open_current(root.path(), &m.corpus, SemanticSelectionBudget::default()),
        Err(SemanticSelectionError::Publication(SemanticGenerationError::MissingPointer))));
    assert_eq!(snapshot(root.path()), before);
    Ok(())
}
