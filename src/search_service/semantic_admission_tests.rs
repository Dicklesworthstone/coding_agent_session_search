//! Exercise semantic invalidation against real canonical/vector readers and
//! the actual one-slot OS-lock pool. No model download or HTTP server is needed.

use super::super::admission::{Pool, Refusal as PoolRefusal};
use super::*;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::tantivy::TantivyIndex;
use coding_agent_search::search::vector_index::{SemanticDocId, VectorIndex};
use coding_agent_search::storage::sqlite::FrankenStorage;
use frankensearch::quill::cass::CassDocument;

fn fixture() -> Result<(tempfile::TempDir, Session, Pool)> {
    let root = tempfile::tempdir()?;
    let database = root.path().join("agent_search.db");
    let storage = FrankenStorage::open(&database)?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let source_path = root.path().join("never-opened-provider.jsonl");
    let content = "capacityneedle retained semantic and lexical readers";
    let outcome = storage.insert_conversation_tree(
        agent,
        None,
        &Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some("semantic-admission".into()),
            title: Some("Admission fixture".into()),
            source_path: source_path.clone(),
            started_at: None,
            ended_at: None,
            approx_tokens: None,
            metadata_json: json!({}),
            source_id: "local".into(),
            origin_host: None,
            messages: vec![Message {
                id: None,
                idx: 7,
                role: MessageRole::Agent,
                author: None,
                created_at: None,
                content: content.into(),
                extra_json: json!({}),
                snippets: Vec::new(),
            }],
        },
    )?;
    let messages = storage.fetch_messages(outcome.conversation_id)?;
    let embedder = HashEmbedder::default();
    let path = vector_index_path(root.path(), embedder.id());
    std::fs::create_dir_all(path.parent().context("vector parent missing")?)?;
    let mut vectors = VectorIndex::create_with_revision(
        &path,
        embedder.id(),
        coding_agent_search::indexer::semantic::HASH_VECTOR_SPACE_REVISION,
        embedder.dimension(),
        frankensearch::index::Quantization::F16,
    )?;
    for message in messages {
        let id = SemanticDocId {
            message_id: u64::try_from(message.id.context("fixture message ID missing")?)?,
            chunk_idx: 0,
            agent_id: u32::try_from(agent)?,
            workspace_id: 0,
            source_id: crc32fast::hash(b"local"),
            role: 1,
            created_at_ms: 0,
            content_hash: None,
        }
        .to_doc_id_string();
        vectors.write_record(&id, &embedder.embed_sync(&message.content)?)?;
    }
    vectors.finish()?;
    drop(storage);

    let index = root.path().join("lexical-index");
    let mut writer = TantivyIndex::open_or_create(&index)?;
    writer.add_prebuilt_documents_slice(&[CassDocument {
        agent: "codex".into(),
        workspace: None,
        workspace_original: None,
        source_path: source_path.to_string_lossy().into_owned(),
        msg_idx: 7,
        created_at: None,
        title: Some("Admission fixture".into()),
        content: content.into(),
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: Some(outcome.conversation_id),
    }])?;
    writer.commit()?;
    drop(writer);

    let pool = Pool::new(root.path().join("reader-pool"), 1)?;
    let mut session = Session::new(index);
    session.archive = Some(database);
    session.semantic = Semantic::new(root.path().to_path_buf(), EmbedderChoice::Hash);
    session.admission_pool = Some(pool.clone());
    Ok((root, session, pool))
}

fn semantic_query(session: &mut Session, mode: &str) -> protocol::Reply {
    let request = serde_json::from_value(json!({
        "op": "semantic_search", "id": 1, "query": "capacityneedle",
        "mode": mode, "limit": 1,
    }))
    .unwrap();
    let (reply, stopped) = session.handle(request);
    assert!(!stopped);
    reply
}

fn lexical_query(session: &mut Session) -> protocol::Reply {
    let request = serde_json::from_value(json!({
        "op": "search", "id": 2, "query": "capacityneedle", "limit": 1,
    }))
    .unwrap();
    let (reply, stopped) = session.handle(request);
    assert!(!stopped);
    reply
}

fn invalidate(root: &Path) -> Result<()> {
    // Appearance of a previously absent guarded publication forces reload.
    // The real lexical index is untouched, so a peer can still search it.
    let path = SemanticManifest::path(root);
    std::fs::create_dir_all(path.parent().context("manifest parent missing")?)?;
    std::fs::write(path, b"publication changed after reader admission")?;
    Ok(())
}

#[test]
fn invalidated_semantic_only_worker_hands_capacity_to_a_peer_without_unload() -> Result<()> {
    let (root, mut session, pool) = fixture()?;
    let first = semantic_query(&mut session, "semantic");
    assert!(first.ok, "{first:?}");
    assert_eq!(first.result.unwrap()["count"], 1);
    assert!(session.client.is_none(), "this worker owns only semantic assets");
    let mut peer = Session::new(session.index.clone());
    peer.admission_pool = Some(pool.clone());
    assert_eq!(lexical_query(&mut peer).error.unwrap().kind, "admission_busy");

    invalidate(root.path())?;
    let failed = semantic_query(&mut session, "semantic");
    assert!(!failed.ok);
    assert_eq!(failed.error.unwrap().kind, "semantic_reload_required");
    let status = session.status();
    assert_eq!(status["semantic"]["loaded"], false);
    assert_eq!(status["semantic"]["requires_reload"], true);
    assert_eq!(status["reader_admission"]["lease_held"], false);
    let recovered = lexical_query(&mut peer);
    assert!(recovered.ok, "{recovered:?}");
    assert_eq!(recovered.result.unwrap()["count"], 1);
    assert!(matches!(pool.acquire(), Err(PoolRefusal::Busy)));
    peer.unload();
    drop(pool.acquire()?);
    Ok(())
}

#[test]
fn failed_hybrid_fallback_releases_capacity_after_both_readers_are_gone() -> Result<()> {
    let (root, mut session, pool) = fixture()?;
    assert!(semantic_query(&mut session, "semantic").ok);
    session.index = root.path().join("unavailable-lexical-index");
    invalidate(root.path())?;
    assert!(!semantic_query(&mut session, "hybrid").ok);
    assert!(session.semantic.client.is_none());
    assert!(session.client.is_none());
    assert!(session.reader_lease.is_none());
    drop(pool.acquire()?);
    assert!(!session.index.exists(), "failure must not create a lexical index");
    Ok(())
}

#[test]
fn successful_hybrid_fallback_keeps_its_lexical_owner_admitted() -> Result<()> {
    let (root, mut session, pool) = fixture()?;
    assert!(semantic_query(&mut session, "semantic").ok);
    invalidate(root.path())?;
    let fallback = semantic_query(&mut session, "hybrid");
    assert!(fallback.ok, "{fallback:?}");
    let result = fallback.result.unwrap();
    assert_eq!(result["realized_mode"], "lexical");
    assert_eq!(result["count"], 1);
    assert!(result["semantic_fallback_reason"].is_string());
    assert!(session.semantic.client.is_none());
    assert!(session.client.is_some());
    assert_eq!(result["snapshot"]["reader_admission"]["lease_held"], true);
    assert!(matches!(pool.acquire(), Err(PoolRefusal::Busy)));
    session.unload();
    drop(pool.acquire()?);
    Ok(())
}

#[test]
fn semantic_refusal_does_not_release_a_preexisting_lexical_reader_slot() -> Result<()> {
    let (root, mut session, pool) = fixture()?;
    assert!(lexical_query(&mut session).ok);
    for _ in 0..2 {
        let result = semantic_query(&mut session, "semantic");
        assert!(result.ok, "{result:?}");
        assert!(matches!(pool.acquire(), Err(PoolRefusal::Busy)));
    }
    assert_eq!(session.semantic.successful_loads, 1);
    invalidate(root.path())?;
    assert!(!semantic_query(&mut session, "semantic").ok);
    assert!(session.client.is_some());
    assert!(session.reader_lease.is_some());
    assert!(matches!(pool.acquire(), Err(PoolRefusal::Busy)));
    assert!(lexical_query(&mut session).ok);
    session.unload();
    drop(pool.acquire()?);
    Ok(())
}

#[test]
fn cold_semantic_admission_failure_leaves_no_reader_slot_reserved() -> Result<()> {
    let root = tempfile::tempdir()?;
    let pool = Pool::new(root.path().join("reader-pool"), 1)?;
    let mut session = Session::new(root.path().join("missing-index"));
    session.archive = Some(root.path().join("missing.db"));
    session.semantic = Semantic::new(root.path().to_path_buf(), EmbedderChoice::Hash);
    session.admission_pool = Some(pool.clone());
    assert!(!semantic_query(&mut session, "semantic").ok);
    assert!(session.semantic.client.is_none());
    assert!(session.client.is_none());
    assert!(session.reader_lease.is_none());
    drop(pool.acquire()?);
    assert!(!session.archive.as_ref().unwrap().exists());
    Ok(())
}
