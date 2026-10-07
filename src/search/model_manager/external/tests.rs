use super::*;
use frankensearch::index::{Quantization, VectorIndex};
use std::net::TcpListener;

fn fixture_config(listener: &TcpListener) -> ExternalEmbeddingConfig {
    ExternalEmbeddingConfig::from_lookup(|key| match key {
        "CASS_EXTERNAL_EMBEDDINGS" => Some("1".into()),
        "CASS_EXTERNAL_EMBEDDING_URL" => Some(format!(
            "http://{}/v1/embeddings",
            listener.local_addr().unwrap(),
        )),
        "CASS_EXTERNAL_EMBEDDING_MODEL" => Some("fixture".into()),
        "CASS_EXTERNAL_EMBEDDING_DIMENSION" => Some("384".into()),
        "CASS_EXTERNAL_EMBEDDING_TIMEOUT_MS" => Some("200".into()),
        _ => None,
    })
    .unwrap()
    .unwrap()
}

fn assert_no_http(listener: &TcpListener) {
    assert!(
        matches!(listener.accept(), Err(error)
        if error.kind() == std::io::ErrorKind::WouldBlock),
        "query admission sent an HTTP request"
    );
}

fn publication(data_dir: &Path, config: &ExternalEmbeddingConfig) -> ArtifactRecord {
    let path = vector_index_path(data_dir, &config.identity());
    ArtifactRecord {
        tier: TierKind::Quality,
        embedder_id: config.identity(),
        model_revision: config.identity(),
        schema_version: SEMANTIC_SCHEMA_VERSION,
        chunking_version: CHUNKING_STRATEGY_VERSION,
        dimension: config.dimension(),
        doc_count: 1,
        conversation_count: 1,
        db_fingerprint: "fixture-not-a-real-archive".into(),
        index_path: path
            .strip_prefix(data_dir)
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        size_bytes: 0,
        started_at_ms: 1,
        completed_at_ms: 2,
        ready: true,
    }
}

#[test]
fn external_query_missing_publication_does_not_contact_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = fixture_config(&listener);
    let result = load_with_config(
        dir.path(),
        &dir.path().join("missing.db"),
        true,
        config,
        Arc::new(|| panic!("preflight must follow artifact admission")),
    );
    assert!(result.context.is_none());
    assert!(matches!(
        result.availability,
        SemanticAvailability::IndexStale { .. }
    ));
    assert_no_http(&listener);
}

#[test]
fn external_query_refuses_foreign_same_dimension_and_wrong_revision_headers() {
    for variant in ["local", "dimension", "revision"] {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let config = fixture_config(&listener);
        let id = config.identity();
        let header_id = if variant == "local" {
            "minilm-384"
        } else {
            &id
        };
        let dimension = if variant == "dimension" { 256 } else { 384 };
        let revision = if variant == "revision" {
            "unrecognized-revision"
        } else {
            crate::indexer::semantic::expected_vector_space_revision(header_id).unwrap()
        };
        let path = vector_index_path(dir.path(), &id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut writer = VectorIndex::create_with_revision(
            &path,
            header_id,
            revision,
            dimension,
            Quantization::F16,
        )
        .unwrap();
        let mut vector = vec![0.0; dimension];
        vector[0] = 1.0;
        let doc_id = crate::search::vector_index::SemanticDocId {
            message_id: 1,
            chunk_idx: 0,
            agent_id: 0,
            workspace_id: 0,
            source_id: 0,
            role: ROLE_USER,
            created_at_ms: 0,
            content_hash: None,
        }
        .to_doc_id_string();
        writer.write_record(&doc_id, &vector).unwrap();
        writer.finish().unwrap();
        let mut manifest = SemanticManifest {
            quality_tier: Some(publication(dir.path(), &config)),
            ..SemanticManifest::default()
        };
        manifest.save(dir.path()).unwrap();
        let result = load_with_config(
            dir.path(),
            &dir.path().join("missing.db"),
            true,
            config,
            Arc::new(|| panic!("foreign vectors must be refused before preflight")),
        );
        assert!(result.context.is_none(), "{variant}");
        match result.availability {
            SemanticAvailability::LoadFailed { context } => {
                assert!(
                    context.contains("incompatible vector index"),
                    "{variant}: {context}"
                );
            }
            other => panic!("wrong refusal for {variant}: {other:?}"),
        }
        assert_no_http(&listener);
    }
}

#[test]
fn external_query_pending_checkpoint_cannot_be_certified_by_ready_quality_record() {
    use crate::search::semantic_manifest::BuildCheckpoint;
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = fixture_config(&listener);
    let mut manifest = SemanticManifest {
        quality_tier: Some(publication(dir.path(), &config)),
        checkpoint: Some(BuildCheckpoint {
            tier: TierKind::Quality,
            embedder_id: config.identity(),
            last_offset: 1,
            docs_embedded: 1,
            conversations_processed: 1,
            total_conversations: 2,
            db_fingerprint: "fixture-not-a-real-archive".into(),
            schema_version: SEMANTIC_SCHEMA_VERSION,
            chunking_version: CHUNKING_STRATEGY_VERSION,
            saved_at_ms: 3,
            last_message_id: Some(1),
            cursor_exhausted: false,
        }),
        ..SemanticManifest::default()
    };
    manifest.save(dir.path()).unwrap();
    let result = load_with_config(
        dir.path(),
        &dir.path().join("missing.db"),
        true,
        config,
        Arc::new(|| panic!("unfinished publication must not reach preflight")),
    );
    assert!(matches!(
        result.availability,
        SemanticAvailability::IndexStale { .. }
    ));
    assert!(result.context.is_none());
    assert_no_http(&listener);
}

#[test]
fn external_query_immutable_generation_gate_still_precedes_endpoint_access() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = fixture_config(&listener);
    let pointer = SemanticCurrentPointerV1::path(dir.path());
    std::fs::create_dir_all(pointer.parent().unwrap()).unwrap();
    std::fs::write(pointer, b"not a serving-authority token").unwrap();
    let result = load_with_config(
        dir.path(),
        &dir.path().join("missing.db"),
        true,
        config,
        Arc::new(|| panic!("owner-backed reader requirement must not be bypassed")),
    );
    assert!(matches!(
        result.availability,
        SemanticAvailability::LoadFailed { .. }
    ));
    assert!(result.context.is_none());
    assert_no_http(&listener);
}

#[test]
fn external_publication_admits_the_cli_producer_identity_without_http() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = fixture_config(&listener);
    let record = publication(dir.path(), &config);
    let mut manifest = SemanticManifest {
        quality_tier: Some(record.clone()),
        ..SemanticManifest::default()
    };
    manifest.save(dir.path()).unwrap();
    let before = std::fs::read(SemanticManifest::path(dir.path())).unwrap();

    // Match actual CLI publication, not the generic header contract string.
    assert_eq!(record.model_revision, config.identity());
    assert_eq!(recorded_artifact(dir.path(), &config).unwrap(), record);
    let result = load_with_config(
        dir.path(),
        &dir.path().join("missing.db"),
        true,
        config,
        Arc::new(|| panic!("ledger admission alone must not authorize HTTP")),
    );
    assert!(result.context.is_none());
    assert!(matches!(
        result.availability,
        SemanticAvailability::IndexMissing { .. }
    ));
    assert_eq!(
        std::fs::read(SemanticManifest::path(dir.path())).unwrap(),
        before
    );
    assert_no_http(&listener);
}

#[test]
fn external_query_refuses_unbound_publication_revisions_before_http() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = fixture_config(&listener);
    let other_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    other_listener.set_nonblocking(true).unwrap();
    let other_identity = fixture_config(&other_listener).identity();
    assert_ne!(config.identity(), other_identity);

    for revision in [
        String::new(),
        "hash".into(),
        "local-model-revision".into(),
        crate::search::external_embedder::EXTERNAL_VECTOR_SPACE_REVISION.into(),
        other_identity,
    ] {
        let mut record = publication(dir.path(), &config);
        record.model_revision = revision.clone();
        let mut manifest = SemanticManifest {
            quality_tier: Some(record),
            ..SemanticManifest::default()
        };
        manifest.save(dir.path()).unwrap();
        let before = std::fs::read(SemanticManifest::path(dir.path())).unwrap();
        let result = load_with_config(
            dir.path(),
            &dir.path().join("missing.db"),
            true,
            config.clone(),
            Arc::new(|| panic!("a foreign producer revision must not reach preflight")),
        );
        assert!(result.context.is_none(), "{revision}");
        match result.availability {
            SemanticAvailability::IndexStale { reason, .. } => {
                assert!(reason.contains("artifact identity"), "{revision}: {reason}");
            }
            other => panic!("wrong ledger-revision refusal for {revision}: {other:?}"),
        }
        assert_eq!(
            std::fs::read(SemanticManifest::path(dir.path())).unwrap(),
            before,
            "admission must not rewrite the ledger for {revision}"
        );
        assert_no_http(&listener);
        assert_no_http(&other_listener);
    }
}
