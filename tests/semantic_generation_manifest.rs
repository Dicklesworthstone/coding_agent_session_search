//! Fresh-process contract tests for immutable semantic generations.
//!
//! These tests intentionally exercise only the public manifest/pointer API.
//! They prove that a restarted consumer resolves the exact generation named by
//! `current.json`, verifies the manifest and artifact bytes, and never depends
//! on historical conventional vector filenames.

#[rustfmt::skip]
#[cfg(any())]
mod superseded_pre_review_fresh_process_contract {
use coding_agent_search::search::semantic_manifest::{
    SEMANTIC_CURRENT_POINTER_SCHEMA_VERSION, SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION,
    SemanticArtifactRole, SemanticArtifactValidationEvidence, SemanticCorpusSnapshotIdentity,
    SemanticCurrentPointerV1, SemanticEmbeddingSpaceIdentity, SemanticGenerationArtifact,
    SemanticGenerationManifestV1, SemanticGenerationStatus, SemanticGenerationTopology,
    SemanticProducerAttestation, load_current_semantic_generation,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

const CHILD_DATA_DIR_ENV: &str = "CASS_TEST_SEMANTIC_GENERATION_DIR";
const CHILD_GENERATION_ENV: &str = "CASS_TEST_SEMANTIC_GENERATION_ID";
const CHILD_CORPUS_ENV: &str = "CASS_TEST_SEMANTIC_CORPUS_SHA256";
const CHILD_MARKER: &str = "semantic_generation_fresh_process_verified";

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn corpus_identity() -> SemanticCorpusSnapshotIdentity {
    SemanticCorpusSnapshotIdentity {
        database_uuid: "cass-fresh-process-db".to_owned(),
        source_change_sequence: 73,
        schema_contract_sha256: sha256(b"schema"),
        content_selection_sha256: sha256(b"selection"),
        canonicalization_sha256: sha256(b"canonicalization"),
        chunking_sha256: sha256(b"chunking"),
        document_id_contract_sha256: sha256(b"document-id"),
        config_sha256: sha256(b"config"),
        corpus_sha256: sha256(b"corpus"),
        ordered_live_docset_sha256: sha256(b"ordered-docset"),
        content_sha256: sha256(b"content"),
        document_count: 4,
    }
}

fn artifact(corpus: &SemanticCorpusSnapshotIdentity, bytes: &[u8]) -> SemanticGenerationArtifact {
    let artifact_sha256 = sha256(bytes);
    let size_bytes = u64::try_from(bytes.len()).expect("fixture artifact length fits u64");
    let validation = SemanticArtifactValidationEvidence {
        validator_id: "fresh-process-validator-v1".to_owned(),
        validated_at_ms: 1_721_990_000_200,
        artifact_sha256: artifact_sha256.clone(),
        size_bytes,
        document_count: corpus.document_count,
        vector_count: corpus.document_count,
        live_vector_count: corpus.document_count,
        tombstone_count: 0,
        wal_entry_count: 0,
        generation_sha256: corpus.corpus_sha256.clone(),
        ordered_live_docset_sha256: corpus.ordered_live_docset_sha256.clone(),
        content_sha256: corpus.content_sha256.clone(),
    };
    SemanticGenerationArtifact {
        role: SemanticArtifactRole::FastVector,
        relative_path: "fast/primary.fsvi".to_owned(),
        artifact_sha256,
        size_bytes,
        storage_format: "fsvi".to_owned(),
        storage_format_version: 1,
        quantization: "f16".to_owned(),
        embedding: SemanticEmbeddingSpaceIdentity {
            embedder_id: "model2vec-256".to_owned(),
            embedder_revision: "model2vec-fixture-revision".to_owned(),
            dimension: 256,
            model_manifest_sha256: sha256(b"model-manifest"),
            model_files_sha256: sha256(b"model-files"),
            input_contract_sha256: sha256(b"embedding-input"),
        },
        document_count: corpus.document_count,
        vector_count: corpus.document_count,
        live_vector_count: corpus.document_count,
        tombstone_count: 0,
        wal_entry_count: 0,
        coverage_numerator: corpus.document_count,
        coverage_denominator: corpus.document_count,
        generation_sha256: corpus.corpus_sha256.clone(),
        ordered_live_docset_sha256: corpus.ordered_live_docset_sha256.clone(),
        content_sha256: corpus.content_sha256.clone(),
        validation,
    }
}

fn manifest(bytes: &[u8]) -> SemanticGenerationManifestV1 {
    let corpus = corpus_identity();
    let artifact = artifact(&corpus, bytes);
    SemanticGenerationManifestV1 {
        schema_version: SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION,
        generation_id: "gen-fresh-process-0001".to_owned(),
        build_id: "build-fresh-process-0001".to_owned(),
        request_id: Some("request-fresh-process-0001".to_owned()),
        previous_generation_id: None,
        created_at_ms: 1_721_990_000_000,
        completed_at_ms: 1_721_990_000_200,
        published_at_ms: 1_721_990_000_300,
        requested_topology: SemanticGenerationTopology::FastOnly,
        realized_topology: SemanticGenerationTopology::FastOnly,
        status: SemanticGenerationStatus::Complete,
        corpus,
        producer: SemanticProducerAttestation {
            cass_git_commit: "a".repeat(40),
            frankensearch_git_commit: "b".repeat(40),
            cargo_lock_sha256: sha256(b"cargo-lock"),
            rust_toolchain: "stable-1.90.0".to_owned(),
            enabled_features: vec!["semantic".to_owned()],
            build_profile: "test".to_owned(),
            binary_elf_sha256: sha256(b"test-binary"),
        },
        document_count: 4,
        vector_count: 4,
        live_vector_count: 4,
        tombstone_count: 0,
        wal_entry_count: 0,
        artifacts: vec![artifact],
    }
}

fn write_sealed_generation(
    data_dir: &Path,
    manifest: &SemanticGenerationManifestV1,
    artifact_bytes: &[u8],
) {
    let generation_dir = manifest
        .generation_dir(data_dir)
        .expect("fixture generation path");
    let artifact_path = generation_dir.join(&manifest.artifacts[0].relative_path);
    fs::create_dir_all(artifact_path.parent().expect("artifact parent"))
        .expect("create fixture generation directory");
    fs::write(&artifact_path, artifact_bytes).expect("write fixture artifact");
    manifest
        .write_immutable(data_dir)
        .expect("seal fixture manifest");
    let pointer = SemanticCurrentPointerV1::for_manifest(manifest).expect("construct pointer");
    assert_eq!(
        pointer.schema_version,
        SEMANTIC_CURRENT_POINTER_SCHEMA_VERSION
    );
    pointer.publish_atomic(data_dir).expect("publish pointer");
}

#[test]
fn fresh_process_restart_loads_exact_manifest_selected_generation() {
    let temp = tempfile::tempdir().expect("temporary semantic data directory");
    let artifact_bytes = b"fixture-fsvi-bytes-with-no-conventional-name";
    let manifest = manifest(artifact_bytes);
    write_sealed_generation(temp.path(), &manifest, artifact_bytes);

    assert!(
        !temp.path().join("vector_index/vector.fast.idx").exists(),
        "fixture must not accidentally satisfy historical filename discovery"
    );
    assert!(
        !temp
            .path()
            .join("vector_index/index-model2vec-256.fsvi")
            .exists(),
        "fixture must be resolvable only through current.json plus manifest.json"
    );

    let output = Command::new(std::env::current_exe().expect("current integration test binary"))
        .arg("--exact")
        .arg("fresh_process_generation_child")
        .arg("--nocapture")
        .env(CHILD_DATA_DIR_ENV, temp.path())
        .env(CHILD_GENERATION_ENV, &manifest.generation_id)
        .env(CHILD_CORPUS_ENV, &manifest.corpus.corpus_sha256)
        .output()
        .expect("spawn fresh integration-test process");

    assert!(
        output.status.success(),
        "fresh-process loader failed\nstatus={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.lines().count() <= 10_000,
        "fresh-process diagnostics exceeded the 10,000-line contract"
    );
    assert!(
        combined.len() <= 8 * 1024 * 1024,
        "fresh-process diagnostics exceeded the 8 MiB contract"
    );
    assert!(
        combined.contains(CHILD_MARKER),
        "fresh child must emit a bounded verification marker: {combined}"
    );
    assert!(
        !combined.contains("TRACE") && !combined.contains("tokenizers"),
        "fresh-process manifest validation must not emit dependency TRACE spam"
    );
    assert!(
        !combined.contains(&temp.path().display().to_string())
            && !combined.contains(&manifest.corpus.corpus_sha256)
            && !combined.contains(&manifest.artifacts[0].artifact_sha256),
        "diagnostics must not expose local paths or full provenance digests"
    );
}

#[test]
fn fresh_process_generation_child() {
    let Some(data_dir) = std::env::var_os(CHILD_DATA_DIR_ENV) else {
        return;
    };
    let expected_generation =
        std::env::var(CHILD_GENERATION_ENV).expect("expected child generation ID");
    let expected_corpus = std::env::var(CHILD_CORPUS_ENV).expect("expected child corpus digest");

    let loaded =
        load_current_semantic_generation(Path::new(&data_dir), None).expect("fresh-process load");
    assert_eq!(loaded.manifest.generation_id, expected_generation);
    assert_eq!(loaded.manifest.corpus.corpus_sha256, expected_corpus);
    assert_eq!(loaded.artifact_paths.len(), 1);
    assert_eq!(
        loaded.artifact_paths.keys().copied().collect::<Vec<_>>(),
        vec![SemanticArtifactRole::FastVector]
    );
    let resolved = fs::read(
        loaded
            .artifact_paths
            .get(&SemanticArtifactRole::FastVector)
            .expect("resolved fast artifact"),
    )
    .expect("read resolved artifact");
    assert_eq!(
        sha256(&resolved),
        loaded.manifest.artifacts[0].artifact_sha256
    );

    let details = BTreeMap::from([
        ("artifact_count", loaded.artifact_paths.len().to_string()),
        ("document_count", loaded.manifest.document_count.to_string()),
        ("generation", loaded.manifest.generation_id),
        (
            "manifest_schema",
            loaded.manifest.schema_version.to_string(),
        ),
        ("status", CHILD_MARKER.to_owned()),
    ]);
    eprintln!(
        "{}",
        serde_json::to_string(&details).expect("serialize bounded child diagnostics")
    );
}
}

use coding_agent_search::search::semantic_manifest::{
    SEMANTIC_CURRENT_POINTER_SCHEMA_VERSION, SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION,
    SemanticArtifactRole, SemanticArtifactValidationEvidence, SemanticCorpusSnapshotIdentity,
    SemanticCurrentPointerV1, SemanticExpectedCurrentV1, SemanticGenerationArtifact,
    SemanticGenerationDiskStateV1, SemanticGenerationManifestV1, SemanticGenerationRecoveryAction,
    SemanticGenerationStatus, SemanticGenerationTopology, SemanticProducerAttestation,
    load_current_semantic_generation, resolve_semantic_generation_disk_state,
};
use frankensearch::core::{
    EMBEDDING_INPUT_CONTRACT_SCHEMA_V1, EMBEDDING_PRODUCER_ATTESTATION_SCHEMA_V1,
    EMBEDDING_SPACE_IDENTITY_SCHEMA_V1, EmbeddingArtifactIdentityV1, EmbeddingIdentityBundleV1,
    EmbeddingInputContractV1, EmbeddingProducerAttestationV1, EmbeddingSpaceIdentityV1,
    EmbeddingSpaceKindV1, GoldenVectorCertificateV1, QuantizationFormat,
    VECTOR_STORAGE_IDENTITY_SCHEMA_V1, VectorStorageIdentityV1,
};
use frankensearch::{VectorIndex, index};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use wait_timeout::ChildExt;

const ACCEPTED_CHILD_DATA_DIR_ENV: &str = "CASS_TEST_ACCEPTED_SEMANTIC_GENERATION_DIR";
const ACCEPTED_CHILD_GENERATION_ENV: &str = "CASS_TEST_ACCEPTED_SEMANTIC_GENERATION_ID";
const ACCEPTED_CHILD_CORPUS_ENV: &str = "CASS_TEST_ACCEPTED_SEMANTIC_CORPUS_SHA256";
const ACCEPTED_CHILD_MARKER: &str = "accepted_semantic_generation_fresh_process_verified";
const POINTER_PUBLISH_CRASH_DATA_DIR_ENV: &str = "CASS_TEST_POINTER_PUBLISH_CRASH_DATA_DIR";
const POINTER_PUBLISH_CRASH_EXIT_CODE: i32 = 86;
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);
const CHILD_OUTPUT_CAP: usize = 8 * 1024 * 1024;
const CHILD_STREAM_CAP: usize = CHILD_OUTPUT_CAP / 2;
const CHILD_LINE_CAP: usize = 10_000;

fn accepted_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn accepted_corpus_identity() -> SemanticCorpusSnapshotIdentity {
    SemanticCorpusSnapshotIdentity {
        database_uuid: "cass-fresh-process-db".to_owned(),
        source_change_sequence: 73,
        schema_contract_sha256: accepted_sha256(b"schema"),
        content_selection_sha256: accepted_sha256(b"selection"),
        canonicalization_sha256: accepted_sha256(b"canonicalization"),
        chunking_sha256: accepted_sha256(b"chunking"),
        document_id_contract_sha256: accepted_sha256(b"document-id"),
        config_sha256: accepted_sha256(b"config"),
        corpus_sha256: accepted_sha256(b"corpus"),
        ordered_live_docset_sha256: accepted_sha256(b"ordered-docset"),
        content_sha256: accepted_sha256(b"content"),
        document_count: 4,
    }
}

fn accepted_embedding_identity() -> frankensearch::core::FrozenEmbeddingIdentityBundleV1 {
    let input = EmbeddingInputContractV1 {
        schema_version: EMBEDDING_INPUT_CONTRACT_SCHEMA_V1,
        canonicalization: "cass-canonicalization-v1".to_owned(),
        content_selection: "cass-selected-message-content-v1".to_owned(),
        chunking: "cass-chunking-v1".to_owned(),
        query_instruction: "query:".to_owned(),
        document_instruction: "document:".to_owned(),
        doc_id_semantics: "cass-stable-document-id-v1".to_owned(),
    };
    let space = EmbeddingSpaceIdentityV1 {
        schema_version: EMBEDDING_SPACE_IDENTITY_SCHEMA_V1,
        logical_model_id: "cass-fresh-process-semantic-fixture".to_owned(),
        immutable_revision: "fixture-v1".to_owned(),
        kind: EmbeddingSpaceKindV1::Semantic,
        artifact_manifest_fingerprint: accepted_sha256(b"model-manifest"),
        artifacts: vec![
            EmbeddingArtifactIdentityV1 {
                role: "tokenizer".to_owned(),
                sha256: accepted_sha256(b"tokenizer"),
                size: 101,
            },
            EmbeddingArtifactIdentityV1 {
                role: "weights".to_owned(),
                sha256: accepted_sha256(b"weights"),
                size: 10_001,
            },
        ],
        tokenizer_fingerprint: accepted_sha256(b"tokenizer-contract"),
        vocabulary_fingerprint: accepted_sha256(b"vocabulary"),
        model_config_fingerprint: accepted_sha256(b"model-config"),
        model_preprocessing: "unicode=nfc;lowercase=false".to_owned(),
        sequence_policy: "truncate=tail;max_tokens=512;padding=none".to_owned(),
        query_instruction: "query:".to_owned(),
        document_instruction: "document:".to_owned(),
        pooling: "mean-mask-aware".to_owned(),
        output_normalization: "l2-f32-v1".to_owned(),
        dimension: 256,
        input_contract_fingerprint: input.fingerprint(),
        hash_control: None,
        projection: None,
    };
    let producer = EmbeddingProducerAttestationV1 {
        schema_version: EMBEDDING_PRODUCER_ATTESTATION_SCHEMA_V1,
        backend: "frankensearch-native".to_owned(),
        implementation_revision: "fixture-v1".to_owned(),
        protocol_revision: "in-process-v1".to_owned(),
        numeric_profile: "deterministic-f32-v1".to_owned(),
        provenance_manifest_fingerprint: accepted_sha256(b"provenance-manifest"),
        space_fingerprint: space.fingerprint(),
        golden_vectors: GoldenVectorCertificateV1 {
            corpus_sha256: accepted_sha256(b"golden-corpus"),
            vectors_sha256: accepted_sha256(b"golden-vectors"),
            vector_count: 2,
            dimension: 256,
        },
    };
    EmbeddingIdentityBundleV1 {
        space,
        producer,
        input,
        storage: VectorStorageIdentityV1 {
            schema_version: VECTOR_STORAGE_IDENTITY_SCHEMA_V1,
            format: "fsvi-v1".to_owned(),
            quantization: QuantizationFormat::F16,
            // frankensearch validates the canonical spelling since 14d1480a.
            endianness: "little-endian".to_owned(),
            vector_normalization: "l2-f32-v1".to_owned(),
            dimension: 256,
        },
    }
    .freeze()
    .expect("accepted semantic fixture identity")
}

fn write_real_fsvi(path: &Path) {
    fs::create_dir_all(path.parent().expect("FSVI parent")).expect("create FSVI parent");
    let mut writer = VectorIndex::create_with_revision(
        path,
        "cass-fresh-process-semantic-fixture",
        "fixture-v1",
        256,
        index::Quantization::F16,
    )
    .expect("create real FSVI writer");
    for document in 0..4 {
        let mut vector = vec![0.0_f32; 256];
        vector[document] = 1.0;
        writer
            .write_record(&format!("doc-{document}"), &vector)
            .expect("write real FSVI record");
    }
    writer.finish().expect("finish real FSVI");
}

fn accepted_manifest(data_dir: &Path) -> SemanticGenerationManifestV1 {
    let corpus = accepted_corpus_identity();
    let generation_id = "gen-fresh-process-accepted-0001";
    let relative_path = "fast/writer-produced.fsvi";
    let generation_dir = data_dir
        .join("vector_index")
        .join("generations")
        .join(generation_id);
    let artifact_path = generation_dir.join(relative_path);
    write_real_fsvi(&artifact_path);
    let artifact_bytes = fs::read(&artifact_path).expect("read writer-produced FSVI");
    let artifact_sha256 = accepted_sha256(&artifact_bytes);
    let size_bytes = u64::try_from(artifact_bytes.len()).expect("FSVI size fits u64");
    let validation = SemanticArtifactValidationEvidence {
        validator_id: "frankensearch-vector-index-open-v1".to_owned(),
        validated_at_ms: 1_721_990_000_250,
        artifact_sha256: artifact_sha256.clone(),
        size_bytes,
        selected_document_count: corpus.document_count,
        covered_document_count: corpus.document_count,
        vector_slot_count: corpus.document_count,
        live_vector_count: corpus.document_count,
        tombstone_vector_count: 0,
        wal_entry_count: 0,
        generation_corpus_sha256: corpus.corpus_sha256.clone(),
        covered_live_docset_sha256: corpus.ordered_live_docset_sha256.clone(),
        covered_content_sha256: corpus.content_sha256.clone(),
    };
    let artifact = SemanticGenerationArtifact {
        role: SemanticArtifactRole::FastVector,
        relative_path: relative_path.to_owned(),
        artifact_sha256,
        size_bytes,
        artifact_format: "fsvi-v1".to_owned(),
        artifact_parameters_sha256: accepted_sha256(b"fsvi-v1-f16-little-l2"),
        embedding_identity: accepted_embedding_identity(),
        ann_base: None,
        selected_document_count: corpus.document_count,
        covered_document_count: corpus.document_count,
        vector_slot_count: corpus.document_count,
        live_vector_count: corpus.document_count,
        tombstone_vector_count: 0,
        wal_entry_count: 0,
        generation_corpus_sha256: corpus.corpus_sha256.clone(),
        covered_live_docset_sha256: corpus.ordered_live_docset_sha256.clone(),
        covered_content_sha256: corpus.content_sha256.clone(),
        validation,
    };
    SemanticGenerationManifestV1 {
        schema_version: SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION,
        generation_id: generation_id.to_owned(),
        build_id: "build-fresh-process-accepted-0001".to_owned(),
        request_id: Some("request-fresh-process-accepted-0001".to_owned()),
        previous_generation_id: None,
        created_at_ms: 1_721_990_000_000,
        completed_at_ms: 1_721_990_000_200,
        sealed_at_ms: 1_721_990_000_300,
        requested_topology: SemanticGenerationTopology::FastOnly,
        realized_topology: SemanticGenerationTopology::FastOnly,
        status: SemanticGenerationStatus::Sealed,
        corpus,
        producer: SemanticProducerAttestation {
            cass_git_commit: "a".repeat(40),
            frankensearch_git_commit: "b".repeat(40),
            cargo_lock_sha256: accepted_sha256(b"cargo-lock"),
            rust_toolchain: "stable-1.90.0".to_owned(),
            enabled_features: vec!["semantic".to_owned()],
            build_profile: "test".to_owned(),
            binary_elf_sha256: accepted_sha256(b"test-binary"),
        },
        selected_document_count: 4,
        total_vector_slot_count_across_tiers: 4,
        total_live_vector_count_across_tiers: 4,
        total_tombstone_vector_count_across_tiers: 0,
        total_wal_entry_count_across_tiers: 0,
        artifacts: vec![artifact],
    }
}

fn read_capped(mut reader: impl std::io::Read, cap: usize, overflow: Arc<AtomicBool>) -> Vec<u8> {
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).expect("read child diagnostics");
        if read == 0 {
            break;
        }
        let remaining = cap.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..read.min(remaining)]);
        if read > remaining {
            overflow.store(true, Ordering::Release);
        }
    }
    captured
}

fn sanitized_child_failure(combined: &str, sensitive_values: &[&str]) -> String {
    sensitive_values
        .iter()
        .fold(combined.to_owned(), |output, value| {
            if value.is_empty() {
                output
            } else {
                output.replace(value, "[redacted]")
            }
        })
}

#[test]
fn accepted_pointer_publication_crash_is_reported_as_retryable_interruption() {
    let temp = tempfile::tempdir().expect("temporary semantic data directory");
    let mut child = Command::new(std::env::current_exe().expect("integration test binary"))
        .arg("--exact")
        .arg("accepted_pointer_publication_crash_child")
        .arg("--nocapture")
        .env(POINTER_PUBLISH_CRASH_DATA_DIR_ENV, temp.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pointer-publication crash process");
    let status = match child
        .wait_timeout(CHILD_TIMEOUT)
        .expect("wait for pointer-publication crash process")
    {
        Some(status) => status,
        None => {
            child.kill().expect("kill timed-out crash process");
            let _ = child.wait();
            panic!("pointer-publication crash process exceeded {CHILD_TIMEOUT:?}");
        }
    };
    assert_eq!(
        status.code(),
        Some(POINTER_PUBLISH_CRASH_EXIT_CODE),
        "child must terminate at the injected pre-rename crash point"
    );

    let state = resolve_semantic_generation_disk_state(temp.path())
        .expect("resolve disk state after interrupted pointer publication");
    assert_eq!(
        state,
        SemanticGenerationDiskStateV1::InterruptedPublish {
            temporary_entry_count: 1,
            build_count: 0,
            unselected_generation_directory_count: 0,
            legacy_artifact_count: 0,
        }
    );
    assert_eq!(
        state.reason_code(),
        Some("semantic_pointer_publish_interrupted")
    );
    assert_eq!(
        state.recovery_action(),
        Some(SemanticGenerationRecoveryAction::RetryPublish)
    );
}

#[test]
fn accepted_pointer_publication_crash_child() {
    let Some(data_dir) = std::env::var_os(POINTER_PUBLISH_CRASH_DATA_DIR_ENV) else {
        return;
    };
    let vector_root = Path::new(&data_dir).join("vector_index");
    fs::create_dir_all(&vector_root).expect("create semantic vector root");
    let mut pointer_temp = tempfile::Builder::new()
        .prefix(".current.publish.")
        .tempfile_in(&vector_root)
        .expect("create same-directory pointer publication tempfile");
    pointer_temp
        .write_all(b"{\"injected\":\"pre-rename-crash\"}\n")
        .expect("write incomplete pointer publication");
    pointer_temp
        .as_file()
        .sync_all()
        .expect("flush incomplete pointer publication");

    // Deliberately bypass `NamedTempFile::drop` to reproduce a process crash
    // between durable tempfile creation and the atomic pointer rename.
    std::process::exit(POINTER_PUBLISH_CRASH_EXIT_CODE);
}

#[test]
fn accepted_fresh_process_restart_loads_writer_produced_fsvi_by_manifest_path() {
    let temp = tempfile::tempdir().expect("temporary semantic data directory");
    let manifest = accepted_manifest(temp.path());
    manifest
        .write_immutable(temp.path())
        .expect("seal manifest");
    let pointer = SemanticCurrentPointerV1::for_manifest(&manifest, manifest.sealed_at_ms + 1, 1)
        .expect("construct pointer");
    assert_eq!(
        pointer.schema_version,
        SEMANTIC_CURRENT_POINTER_SCHEMA_VERSION
    );
    pointer
        .publish_atomic(temp.path(), &SemanticExpectedCurrentV1::Absent)
        .expect("CAS publish pointer");

    for obsolete in [
        "vector_index/vector.fast.idx",
        "vector_index/index-model2vec-256.fsvi",
    ] {
        assert!(
            !temp.path().join(obsolete).exists(),
            "fixture must not satisfy historical filename discovery"
        );
    }

    let mut child = Command::new(std::env::current_exe().expect("integration test binary"))
        .arg("--exact")
        .arg("accepted_fresh_process_generation_child")
        .arg("--nocapture")
        .env(ACCEPTED_CHILD_DATA_DIR_ENV, temp.path())
        .env(ACCEPTED_CHILD_GENERATION_ENV, &manifest.generation_id)
        .env(ACCEPTED_CHILD_CORPUS_ENV, &manifest.corpus.corpus_sha256)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fresh integration-test process");
    let overflow = Arc::new(AtomicBool::new(false));
    let stdout = child.stdout.take().expect("child stdout");
    let stderr = child.stderr.take().expect("child stderr");
    let stdout_overflow = Arc::clone(&overflow);
    let stderr_overflow = Arc::clone(&overflow);
    let stdout_reader =
        std::thread::spawn(move || read_capped(stdout, CHILD_STREAM_CAP, stdout_overflow));
    let stderr_reader =
        std::thread::spawn(move || read_capped(stderr, CHILD_STREAM_CAP, stderr_overflow));
    let status = match child.wait_timeout(CHILD_TIMEOUT).expect("wait for child") {
        Some(status) => status,
        None => {
            child.kill().expect("kill timed-out child");
            let _ = child.wait();
            panic!("fresh-process loader exceeded {CHILD_TIMEOUT:?}");
        }
    };
    let stdout = stdout_reader.join().expect("join stdout reader");
    let stderr = stderr_reader.join().expect("join stderr reader");
    assert!(
        !overflow.load(Ordering::Acquire),
        "child output exceeded cap"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    let temp_path = temp.path().display().to_string();
    let sanitized_failure = sanitized_child_failure(
        &combined,
        &[
            &temp_path,
            &manifest.corpus.corpus_sha256,
            &manifest.artifacts[0].artifact_sha256,
            &manifest.producer.binary_elf_sha256,
        ],
    );
    assert!(
        status.success(),
        "fresh-process loader failed\nstatus={status:?}\n{sanitized_failure}"
    );
    assert!(combined.len() <= CHILD_OUTPUT_CAP);
    assert!(combined.lines().count() <= CHILD_LINE_CAP);
    assert!(combined.contains(ACCEPTED_CHILD_MARKER));
    assert!(combined.contains("semantic.manifest.load"));
    for field in [
        "build",
        "request",
        "requested_topology",
        "realized_topology",
        "manifest_sha256",
        "selected_document_count",
    ] {
        assert!(
            combined.contains(field),
            "missing first-party log field {field}"
        );
    }
    assert!(!combined.contains("TRACE") && !combined.contains("tokenizers"));
    assert!(!combined.contains(&temp_path));
    assert!(!combined.contains(&manifest.corpus.corpus_sha256));
    assert!(!combined.contains(&manifest.artifacts[0].artifact_sha256));
    assert!(!combined.contains(&manifest.producer.binary_elf_sha256));
}

#[test]
fn accepted_fresh_process_generation_child() {
    let Some(data_dir) = std::env::var_os(ACCEPTED_CHILD_DATA_DIR_ENV) else {
        return;
    };
    let expected_generation =
        std::env::var(ACCEPTED_CHILD_GENERATION_ENV).expect("expected generation");
    let expected_corpus = std::env::var(ACCEPTED_CHILD_CORPUS_ENV).expect("expected corpus digest");
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(std::io::stderr)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let loaded = load_current_semantic_generation(Path::new(&data_dir), None)
            .expect("fresh-process manifest load");
        assert_eq!(loaded.manifest.generation_id, expected_generation);
        assert_eq!(loaded.manifest.corpus.corpus_sha256, expected_corpus);
        assert_eq!(loaded.artifact_paths.len(), 1);
        let artifact_path = loaded
            .artifact_paths
            .get(&SemanticArtifactRole::FastVector)
            .expect("resolved fast artifact");
        // This contract intentionally stops at immutable selected bytes. The
        // accepted read-only FSVI semantic witness/search owner is the formal
        // blocker coding_agent_session_search-cass-fsvi-semantic-witness-xbq0b;
        // never substitute the mutating VectorIndex::open path here.
        let selected_bytes =
            fs::read(artifact_path).expect("read exact manifest-selected writer-produced FSVI");
        assert_eq!(
            accepted_sha256(&selected_bytes),
            loaded.manifest.artifacts[0].artifact_sha256
        );
        let details = BTreeMap::from([
            ("artifact_count", loaded.artifact_paths.len().to_string()),
            (
                "selected_document_count",
                loaded.manifest.selected_document_count.to_string(),
            ),
            ("generation", loaded.manifest.generation_id),
            (
                "manifest_schema",
                loaded.manifest.schema_version.to_string(),
            ),
            ("status", ACCEPTED_CHILD_MARKER.to_owned()),
        ]);
        eprintln!(
            "{}",
            serde_json::to_string(&details).expect("serialize bounded child marker")
        );
    });
}
