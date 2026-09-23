use super::*;
use coding_agent_search::search::tantivy::TantivyIndex;
use frankensearch::quill::cass::CassDocument;
use std::io::Cursor;

fn request(value: Value) -> Request {
    serde_json::from_value(value).unwrap()
}

fn search(session: &mut Session, id: u64, query: &str) -> Value {
    let (reply, stopped) = session.handle(request(json!({"op":"search", "id":id, "query":query})));
    assert!(!stopped);
    assert!(reply.ok, "{reply:?}");
    reply.result.unwrap()
}

fn document(id: i64, text: &str, source: &str) -> CassDocument {
    CassDocument {
        agent: "codex".into(),
        workspace: Some("/workspace/δ".into()),
        workspace_original: None,
        source_path: "/history/same.jsonl".into(),
        msg_idx: 7,
        created_at: Some(1_700_000_000_000),
        title: Some("search fixture".into()),
        content: text.into(),
        source_id: source.into(),
        origin_kind: if source == "local" { "local" } else { "remote" }.into(),
        origin_host: None,
        conversation_id: Some(id),
    }
}

fn index(path: &Path, docs: &[CassDocument]) -> Result<TantivyIndex> {
    let mut writer = TantivyIndex::open_or_create(path)?;
    writer.add_prebuilt_documents_slice(docs)?;
    writer.commit()?;
    Ok(writer)
}

fn tree(path: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let mut snapshot = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            snapshot.extend(tree(&entry.path())?);
        } else {
            snapshot.push((entry.path(), std::fs::read(entry.path())?));
        }
    }
    snapshot.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(snapshot)
}

#[test]
fn invalid_requests_and_status_do_not_open_or_create_an_index() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("missing");
    let mut session = Session::new(path.clone());
    assert_eq!(session.status()["open_attempts"], 0);
    for value in [
        json!({"op":"search", "id":1, "query":"x", "limit":0}),
        json!({"op":"search", "id":2, "query":"x", "limit":101}),
        json!({"op":"search", "id":3, "query":" ", "limit":1}),
        json!({"op":"search", "id":4, "query":"x", "offset":usize::MAX}),
        json!({"op":"search", "id":5, "query":"x", "filters":{"created_from":2, "created_to":1}}),
        json!({"op":"search", "id":6, "query":"x", "filters":{"agents":[""]}}),
        json!({"op":"search", "id":7, "query":"x", "filters":{"source_id":" local "}}),
    ] {
        let (reply, stop) = session.handle(request(value));
        assert!(!reply.ok);
        assert!(!stop);
        assert_eq!(reply.error.unwrap().kind, "invalid_request");
    }
    assert_eq!(session.status()["open_attempts"], 0);
    assert!(!path.exists());
    assert!(tree(temp.path())?.is_empty());
    Ok(())
}

#[test]
fn protocol_rejects_unsupported_work_instead_of_silently_downgrading() {
    for value in [
        json!({"op":"search", "id":1, "query":"x", "mode":"semantic"}),
        json!({"op":"search", "id":1, "query":"x", "filters":{"session_paths":["/secret"]}}),
        json!({"op":"reload", "id":1, "index":"/another/archive"}),
        json!({"op":"status", "id":null}),
        json!({"op":"search", "id":1, "query":"x", "limit":-1}),
        json!({"op":"index", "id":1}),
    ] {
        assert!(
            serde_json::from_value::<Request>(value.clone()).is_err(),
            "{value}"
        );
    }
    assert!(
        serde_json::from_str::<Request>(
            r#"{"op":"search","id":1,"query":"x","limit":1,"limit":2}"#
        )
        .is_err()
    );
}

#[test]
fn semantic_service_is_explicit_and_lazy() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut session = Session::new(temp.path().join("missing-index"));
    let request_value = json!({"op":"semantic_search", "id":1, "query":"needle"});
    let (reply, _) = session.handle(request(request_value.clone()));
    assert_eq!(reply.error.unwrap().kind, "semantic_disabled");
    session.semantic =
        semantic::Semantic::new(temp.path().to_path_buf(), semantic::EmbedderChoice::Hash);
    session.archive = Some(temp.path().join("missing.db"));
    assert_eq!(session.status()["semantic"]["load_attempts"], 0);
    let (invalid, _) = session.handle(request(
        json!({"op":"semantic_search", "id":2, "query":"needle", "limit":0}),
    ));
    assert_eq!(invalid.error.unwrap().kind, "invalid_request");
    assert_eq!(session.status()["semantic"]["load_attempts"], 0);
    assert!(tree(temp.path())?.is_empty());
    for argv in [
        vec![
            "cass",
            "serve",
            "--stdio",
            "--index",
            "x",
            "--semantic-embedder",
            "hash",
        ],
        vec![
            "cass",
            "serve",
            "--stdio",
            "--data-dir",
            "x",
            "--semantic-embedder",
            "hash",
        ],
    ] {
        assert!(ServiceCli::try_parse_from(argv).is_err());
    }
    assert!(
        ServiceCli::try_parse_from([
            "cass",
            "serve",
            "--stdio",
            "--data-dir",
            "x",
            "--db",
            "x/archive.db",
            "--semantic-embedder",
            "hash"
        ])
        .is_ok()
    );
    assert!(
        serde_json::from_value::<Request>(
            json!({"op":"semantic_search", "id":1, "query":"x", "mode":"lexical"})
        )
        .is_err()
    );
    Ok(())
}

fn semantic_service_fixture() -> Result<(tempfile::TempDir, Session, i64)> {
    use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use coding_agent_search::search::embedder::Embedder;
    use coding_agent_search::search::hash_embedder::HashEmbedder;
    use coding_agent_search::search::vector_index::{
        SemanticDocId, VectorIndex, vector_index_path,
    };
    use coding_agent_search::storage::sqlite::FrankenStorage;

    let root = tempfile::tempdir()?;
    let database = root.path().join("agent_search.db");
    let storage = FrankenStorage::open(&database)?;
    let agent_id = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let workspace = PathBuf::from("/work/semantic-fixture");
    let workspace_id = storage.ensure_workspace(&workspace, None)?;
    let source_path = root.path().join("raw-history-never-created.jsonl");
    let outcome = storage.insert_conversation_tree(
        agent_id,
        Some(workspace_id),
        &Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(workspace),
            external_id: Some("semantic-service-fixture".into()),
            title: Some("Global retrieval".into()),
            source_path: source_path.clone(),
            started_at: None,
            ended_at: None,
            approx_tokens: None,
            metadata_json: json!({}),
            source_id: "local".into(),
            origin_host: None,
            messages: [
                (7, "semanticneedle persistent retained vector search"),
                (12, "unrelated banana orchard cultivation"),
            ]
            .into_iter()
            .map(|(idx, content)| Message {
                id: None,
                idx,
                role: MessageRole::Agent,
                author: None,
                created_at: None,
                content: content.into(),
                extra_json: json!({}),
                snippets: Vec::new(),
            })
            .collect(),
        },
    )?;
    let messages = storage.fetch_messages(outcome.conversation_id)?;
    let embedder = HashEmbedder::default();
    let vector_path = vector_index_path(root.path(), embedder.id());
    std::fs::create_dir_all(vector_path.parent().unwrap())?;
    let mut vectors = VectorIndex::create_with_revision(
        &vector_path,
        embedder.id(),
        coding_agent_search::indexer::semantic::HASH_VECTOR_SPACE_REVISION,
        embedder.dimension(),
        frankensearch::index::Quantization::F16,
    )?;
    for message in &messages {
        let doc_id = SemanticDocId {
            message_id: message.id.unwrap() as u64,
            chunk_idx: 0,
            agent_id: agent_id as u32,
            workspace_id: workspace_id as u32,
            source_id: crc32fast::hash(b"local"),
            role: 1,
            created_at_ms: 0,
            content_hash: None,
        }
        .to_doc_id_string();
        vectors.write_record(&doc_id, &embedder.embed_sync(&message.content)?)?;
    }
    vectors.finish()?;
    drop(storage);
    let lexical = root.path().join("lexical-index");
    let mut writer = TantivyIndex::open_or_create(&lexical)?;
    writer.add_prebuilt_documents_slice(&[CassDocument {
        source_path: source_path.to_string_lossy().into_owned(),
        msg_idx: 12,
        conversation_id: Some(outcome.conversation_id),
        content: messages[1].content.clone(),
        workspace: Some("/work/semantic-fixture".into()),
        ..document(outcome.conversation_id, "", "local")
    }])?;
    writer.commit()?;
    drop(writer);
    let mut session = Session::new(lexical);
    session.archive = Some(database);
    session.semantic =
        semantic::Semantic::new(root.path().to_path_buf(), semantic::EmbedderChoice::Hash);
    Ok((root, session, outcome.conversation_id))
}

#[test]
fn semantic_service_reuses_global_vectors_without_a_lexical_match_and_preserves_filters()
-> Result<()> {
    let (root, mut session, conversation) = semantic_service_fixture()?;
    let before = tree(root.path())?;
    for id in 1..=2 {
        let (reply, _) = session.handle(request(json!({"op":"semantic_search", "id":id,
            "query":"semanticneedle", "mode":"semantic", "limit":1, "approximate":true,
            "filters":{"agents":["codex"], "source_id":"local", "workspaces":["/work/semantic-fixture"]}})));
        assert!(reply.ok, "{reply:?}");
        let result = reply.result.unwrap();
        assert_eq!(result["count"], 1);
        assert_eq!(result["hits"][0]["conversation_id"], conversation);
        assert_eq!(result["hits"][0]["message_index"], 8);
        assert_eq!(result["hits"][0]["source_id"], "local");
        assert_eq!(result["semantic_reused"], id > 1);
        assert_eq!(result["snapshot"]["semantic"]["successful_loads"], 1);
        assert_eq!(result["snapshot"]["semantic"]["neural"], false);
        assert_eq!(
            result["snapshot"]["open_attempts"], 0,
            "semantic-only search must never load lexical"
        );
        assert_eq!(result["ann"]["requested"], true);
        assert_eq!(
            result["ann"]["used"], false,
            "persistent service must use reclaimable exact vector owners"
        );
        assert_eq!(result["ann"]["attempted"], false);
        assert_eq!(
            result["ann"]["unavailable_reason"],
            "persistent_native_ann_requires_reclaimable_owners"
        );
        assert!(result["hits"][0].get("content").is_none());
    }
    let lexical = search(&mut session, 3, "semanticneedle");
    assert_eq!(
        lexical["count"], 0,
        "positive semantic evidence is absent from the lexical candidate pool"
    );
    let (hybrid, _) = session.handle(request(
        json!({"op":"semantic_search", "id":4, "query":"semanticneedle", "limit":1}),
    ));
    assert!(hybrid.ok, "{hybrid:?}");
    assert_eq!(hybrid.result.as_ref().unwrap()["realized_mode"], "hybrid");
    assert_eq!(hybrid.result.unwrap()["hits"][0]["message_index"], 8);
    for filters in [
        json!({"agents":["absent"]}),
        json!({"source_id":"different-source"}),
        json!({"workspaces":["/another/workspace"]}),
        json!({"created_from":1000}),
    ] {
        let (filtered, _) = session.handle(request(json!({"op":"semantic_search", "id":5,
            "query":"semanticneedle", "mode":"semantic", "filters":filters})));
        assert!(filtered.ok, "{filtered:?}");
        assert_eq!(filtered.result.unwrap()["count"], 0);
    }
    session.unload();
    assert_eq!(session.status()["semantic"]["loaded"], false);
    assert_eq!(
        tree(root.path())?,
        before,
        "read-only service changed its assets"
    );
    Ok(())
}

#[test]
fn semantic_service_missing_assets_falls_back_only_for_hybrid() -> Result<()> {
    let (root, mut session, _) = semantic_service_fixture()?;
    session.semantic =
        semantic::Semantic::new(root.path().to_path_buf(), semantic::EmbedderChoice::Minilm);
    let before = tree(root.path())?;
    let (hybrid, _) = session.handle(request(
        json!({"op":"semantic_search", "id":1, "query":"banana"}),
    ));
    assert!(hybrid.ok, "{hybrid:?}");
    let hybrid = hybrid.result.unwrap();
    assert_eq!(hybrid["count"], 1);
    assert_eq!(hybrid["realized_mode"], "lexical");
    assert!(
        hybrid["semantic_fallback_reason"]
            .as_str()
            .unwrap()
            .contains("model")
    );
    let (semantic, _) = session.handle(request(
        json!({"op":"semantic_search", "id":2, "query":"banana", "mode":"semantic"}),
    ));
    assert!(!semantic.ok);
    assert_eq!(semantic.error.unwrap().kind, "semantic_unavailable");
    session.unload();
    assert_eq!(
        tree(root.path())?,
        before,
        "missing models must never initiate downloads or create assets"
    );
    Ok(())
}

#[test]
fn semantic_service_replacement_invalidates_retained_context_until_reload() -> Result<()> {
    use coding_agent_search::search::embedder::Embedder;
    use coding_agent_search::search::hash_embedder::HashEmbedder;
    use coding_agent_search::search::vector_index::vector_index_path;

    let (root, mut session, _) = semantic_service_fixture()?;
    let search_request = json!({"op":"semantic_search", "id":1, "query":"semanticneedle", "mode":"semantic", "limit":1});
    assert!(session.handle(request(search_request.clone())).0.ok);
    let path = vector_index_path(root.path(), HashEmbedder::default().id());
    let replacement = root.path().join("replacement.fsvi");
    std::fs::copy(&path, &replacement)?;
    std::fs::rename(&path, root.path().join("retained-old.fsvi"))?;
    std::fs::rename(&replacement, &path)?;
    let (changed, _) = session.handle(request(search_request.clone()));
    assert_eq!(changed.error.unwrap().kind, "semantic_reload_required");
    assert_eq!(session.status()["semantic"]["loaded"], false);
    assert_eq!(session.status()["semantic"]["requires_reload"], true);
    assert_eq!(
        session
            .handle(request(search_request.clone()))
            .0
            .error
            .unwrap()
            .kind,
        "semantic_reload_required"
    );
    assert!(session.handle(Request::Reload { id: 3 }).0.ok);
    let reloaded = session.handle(request(search_request)).0;
    assert!(reloaded.ok, "{reloaded:?}");
    assert_eq!(
        reloaded.result.unwrap()["snapshot"]["semantic"]["successful_loads"],
        2
    );
    session.unload();
    Ok(())
}

#[cfg(unix)]
#[test]
fn semantic_service_same_size_archive_edit_with_restored_mtime_is_refused() -> Result<()> {
    let (_root, mut session, _) = semantic_service_fixture()?;
    let value =
        json!({"op":"semantic_search", "id":1, "query":"semanticneedle", "mode":"semantic"});
    assert!(session.handle(request(value.clone())).0.ok);
    let path = session.archive.as_ref().unwrap();
    let original_time = std::fs::metadata(path)?.modified()?;
    let original = std::fs::read(path)?;
    // The bytes and count even remain identical: ctime, not size/count/mtime,
    // must detect an in-place rewrite of a previously admitted database.
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.write_all(&original)?;
    file.set_modified(original_time)?;
    drop(file);
    let (changed, _) = session.handle(request(value));
    assert_eq!(changed.error.unwrap().kind, "semantic_reload_required");
    session.unload();
    Ok(())
}

#[test]
fn semantic_service_shares_one_admission_slot_with_hybrid_and_canonical_view() -> Result<()> {
    let (root, mut session, _) = semantic_service_fixture()?;
    let pool = admission::Pool::new(root.path().join("admission"), 1)?;
    session.admission_pool = Some(pool.clone());
    let (semantic, _) = session.handle(request(json!({"op":"semantic_search", "id":1,
        "query":"semanticneedle", "mode":"semantic", "limit":1})));
    assert!(semantic.ok, "{semantic:?}");
    assert!(
        pool.acquire().is_err(),
        "semantic context must retain the only owner slot"
    );
    assert_eq!(session.status()["canonical_database_accessed"], true);
    assert_eq!(session.status()["semantic"]["queries_attempted"], 1);
    let hit = &semantic.result.as_ref().unwrap()["hits"][0];
    let (view, _) = session.handle(request(json!({"op":"view", "id":2,
        "source_path":hit["source_path"], "source_id":hit["source_id"],
        "conversation_id":hit["conversation_id"], "message_index":hit["message_index"]})));
    assert!(
        view.ok,
        "canonical view must reuse the semantic owner slot: {view:?}"
    );
    let (hybrid, _) = session.handle(request(json!({"op":"semantic_search", "id":3,
        "query":"semanticneedle", "limit":1})));
    assert!(hybrid.ok, "{hybrid:?}");
    assert_eq!(
        hybrid.result.unwrap()["realized_mode"],
        "hybrid",
        "opening the lexical leg must not acquire a second slot"
    );
    assert!(pool.acquire().is_err());
    session.unload();
    assert!(
        pool.acquire().is_ok(),
        "unload must release every native owner before the slot"
    );
    Ok(())
}

#[test]
fn framed_exchange_is_lazy_recovers_after_bad_json_and_stops_on_shutdown() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut session = Session::new(temp.path().join("absent"));
    let mut input = Cursor::new(b"not json\n{\"op\":\"status\",\"id\":2}\r\n{\"op\":\"shutdown\",\"id\":3}\n{\"op\":\"search\",\"id\":4,\"query\":\"never\"}\n");
    let mut output = Vec::new();
    serve_io(&mut session, &mut input, &mut output)?;
    let replies: Vec<Value> = std::str::from_utf8(&output)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0]["ok"], false);
    assert!(replies[0]["id"].is_null());
    assert_eq!(replies[1]["result"]["successful_opens"], 0);
    assert_eq!(replies[2]["result"]["shutdown"], true);
    assert_eq!(session.open_attempts, 0);
    Ok(())
}

#[test]
fn frame_and_response_limits_never_publish_partial_successes() -> Result<()> {
    let mut exact = Cursor::new([vec![b' '; protocol::MAX_REQUEST_BYTES], b"\n".to_vec()].concat());
    assert!(
        matches!(protocol::read_frame(&mut exact)?, Frame::Line(line) if line.len() == protocol::MAX_REQUEST_BYTES)
    );
    let oversized = vec![b'x'; protocol::MAX_REQUEST_BYTES + 1];
    let mut session = Session::new(PathBuf::from("never-opened"));
    let mut output = Vec::new();
    serve_io(&mut session, &mut Cursor::new(oversized), &mut output)?;
    let reply: Value = serde_json::from_slice(&output)?;
    assert_eq!(reply["error"]["kind"], "request_too_large");
    assert_eq!(session.open_attempts, 0);

    output.clear();
    // Escaping also counts toward the wire limit, not only the string length.
    let huge = Reply::success(
        42,
        json!({"path":"\u{0001}".repeat(protocol::MAX_RESPONSE_BYTES / 4)}),
    );
    protocol::write_reply(&mut output, &huge)?;
    assert!(output.len() < 1024);
    let reply: Value = serde_json::from_slice(&output)?;
    assert_eq!(reply["id"], 42);
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["error"]["kind"], "response_too_large");
    assert!(reply.get("result").is_none());
    Ok(())
}

#[test]
fn selected_archive_is_explicit_and_stdio_is_opt_in() {
    for argv in [
        vec!["cass", "serve", "--stdio"],
        vec!["cass", "serve", "--index", "x"],
        vec![
            "cass",
            "serve",
            "--stdio",
            "--index",
            "x",
            "--data-dir",
            "y",
        ],
    ] {
        assert!(ServiceCli::try_parse_from(argv).is_err());
    }
    assert!(ServiceCli::try_parse_from(["cass", "serve", "--stdio", "--index", "x"]).is_ok());
    assert!(ServiceCli::try_parse_from(["cass", "serve", "--stdio", "--data-dir", "y"]).is_ok());
}

#[test]
fn repeated_queries_reuse_one_real_reader_and_preserve_canonical_followup_identity() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("index");
    let writer = index(
        &path,
        &[
            document(41, "needle original", "local"),
            document(42, "needle remote", "work-laptop"),
        ],
    )?;
    drop(writer);
    // A corrupt adjacent database must be irrelevant to this index-only path.
    std::fs::write(temp.path().join("agent_search.db"), b"not a database")?;
    let before = tree(temp.path())?;
    let mut session = Session::new(path.clone());
    let first = search(&mut session, 1, "needle");
    let second = search(&mut session, 2, "needle");
    assert_eq!(first["reader_reused"], false);
    assert_eq!(second["reader_reused"], true);
    assert_eq!(second["snapshot"]["successful_opens"], 1);
    assert_eq!(second["snapshot"]["queries_completed"], 2);
    assert_eq!(first["hits"], second["hits"]);
    assert_eq!(second["count"], 2);
    assert!(second["hits"].as_array().unwrap().iter().all(|hit| {
        hit["message_index"] == 8
            && hit["conversation_id"].is_i64()
            && hit.get("content").is_none()
            && hit["source_path"] == "/history/same.jsonl"
    }));
    let (scoped, _) = session.handle(request(json!({
        "op":"search", "id":3, "query":"needle", "filters":{"source_id":"work-laptop"}
    })));
    assert!(scoped.ok, "{scoped:?}");
    let scoped = scoped.result.unwrap();
    assert_eq!(scoped["count"], 1);
    assert_eq!(scoped["hits"][0]["source_id"], "work-laptop");
    assert_eq!(scoped["hits"][0]["conversation_id"], 42);
    assert_eq!(tree(temp.path())?, before);
    Ok(())
}

#[test]
fn publication_is_invisible_until_reload_and_new_snapshot_replaces_old() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("index");
    let mut writer = index(&path, &[document(41, "needle original", "local")])?;
    let mut session = Session::new(path);
    assert_eq!(search(&mut session, 1, "needle")["count"], 1);
    writer.add_prebuilt_documents_slice(&[document(42, "needle appended", "local")])?;
    writer.commit()?;
    assert_eq!(search(&mut session, 2, "needle")["count"], 1);
    assert_eq!(session.successful_opens, 1);
    let (reload, stop) = session.handle(Request::Reload { id: 3 });
    assert!(reload.ok, "{reload:?}");
    assert!(!stop);
    assert_eq!(session.successful_opens, 2);
    let fresh = search(&mut session, 4, "needle");
    assert_eq!(fresh["count"], 2);
    assert_eq!(fresh["snapshot"]["reader_epoch"], 2);
    assert_eq!(fresh["snapshot"]["freshness"], "not_checked");
    Ok(())
}

#[cfg(unix)]
#[test]
fn failed_reload_releases_the_old_reader_and_recovery_never_serves_it_as_current() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("index");
    let writer = index(&path, &[document(41, "needle original", "local")])?;
    drop(writer);
    let mut session = Session::new(path.clone());
    search(&mut session, 1, "needle");
    let moved = temp.path().join("retained-index");
    std::fs::rename(&path, &moved)?;
    let (reply, _) = session.handle(Request::Reload { id: 2 });
    assert!(!reply.ok);
    assert_eq!(session.status()["loaded"], false);
    assert!(session.status()["reader_epoch"].is_null());
    assert!(
        !path.exists(),
        "read-only reload must not recreate an index"
    );
    std::fs::rename(moved, &path)?;
    let (reply, _) = session.handle(Request::Reload { id: 3 });
    assert!(reply.ok, "{reply:?}");
    assert_eq!(
        search(&mut session, 4, "needle")["snapshot"]["reader_epoch"],
        2
    );
    Ok(())
}

#[test]
fn pagination_never_claims_exhaustion_without_proof() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("index");
    let _writer = index(
        &path,
        &(1..=4)
            .map(|id| document(id, "needle searchable context", "local"))
            .collect::<Vec<_>>(),
    )?;
    let mut session = Session::new(path);
    let (reply, _) = session.handle(request(
        json!({"op":"search", "id":1, "query":"needle", "limit":2}),
    ));
    assert!(reply.ok, "{reply:?}");
    let page = reply.result.unwrap();
    assert_eq!(page["count"], 2);
    assert_eq!(page["has_more"], true);
    assert_eq!(page["next_offset"], 2);
    let absent = search(&mut session, 2, "xyznotpresent");
    assert_eq!(absent["count"], 0);
    assert!(absent["has_more"].is_null());
    assert!(absent["next_offset"].is_null());
    Ok(())
}

#[test]
fn previews_are_truncated_on_unicode_boundaries() {
    assert_eq!(prefix("αβ😀δ", 3), "αβ😀");
    assert_eq!(prefix("αβ😀δ", 0), "");
    assert_eq!(prefix("αβ😀δ", 99), "αβ😀δ");
}

#[test]
fn page_window_boundary_does_not_emit_an_unusable_continuation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("index");
    let docs = (1..=1030)
        .map(|id| document(id, "needle searchable context", "local"))
        .collect::<Vec<_>>();
    let writer = index(&path, &docs)?;
    drop(writer);
    let mut session = Session::new(path);
    let (reply, _) = session.handle(request(json!({
        "op": "search", "id": 1, "query": "needle", "limit": 10, "offset": 1010
    })));
    assert!(reply.ok, "{reply:?}");
    let page = reply.result.unwrap();
    assert_eq!(page["count"], 10);
    assert_eq!(page["has_more"], true);
    assert_eq!(page["page_window_exhausted"], true);
    assert!(page["next_offset"].is_null());
    Ok(())
}

#[test]
fn admission_is_lazy_for_status_invalid_queries_and_unload() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let pool = temp.path().join("pool");
    let mut session = Session::new(temp.path().join("absent"));
    session.admission_pool = Some(admission::Pool::new(pool.clone(), 1)?);
    assert!(session.handle(Request::Status { id: 1 }).0.ok);
    assert!(session.handle(Request::Unload { id: 2 }).0.ok);
    let (invalid, _) = session.handle(request(json!({"op":"search", "id":3, "query":" "})));
    assert_eq!(invalid.error.unwrap().kind, "invalid_request");
    assert_eq!(session.open_attempts, 0);
    assert!(!pool.exists());
    Ok(())
}

#[test]
fn one_pool_preserves_reader_reuse_and_allows_handoff_after_unload() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("index");
    drop(index(&path, &[document(42, "admissionneedle", "local")])?);
    let before = tree(&path)?;
    let pool = admission::Pool::new(temp.path().join("pool"), 1)?;
    let mut first = Session::new(path.clone());
    let mut second = Session::new(path.clone());
    first.admission_pool = Some(pool.clone());
    second.admission_pool = Some(pool.clone());
    assert_eq!(search(&mut first, 1, "admissionneedle")["count"], 1);
    assert_eq!(
        search(&mut first, 2, "admissionneedle")["reader_reused"],
        true
    );
    let (refused, _) = second.handle(request(
        json!({"op":"search", "id":3, "query":"admissionneedle"}),
    ));
    assert_eq!(refused.error.unwrap().kind, "admission_busy");
    assert_eq!(second.open_attempts, 0);
    let (unloaded, stop) = first.handle(Request::Unload { id: 4 });
    assert!(!stop);
    assert_eq!(
        unloaded.result.unwrap()["reader_admission"]["lease_held"],
        false
    );
    assert_eq!(search(&mut second, 5, "admissionneedle")["count"], 1);
    assert!(matches!(pool.acquire(), Err(admission::Refusal::Busy)));
    assert!(second.handle(Request::Shutdown { id: 6 }).1);
    let reopened = search(&mut first, 7, "admissionneedle");
    assert_eq!(reopened["snapshot"]["reader_epoch"], 2);
    assert_eq!(reopened["hits"][0]["conversation_id"], 42);
    first.unload();
    assert_eq!(tree(&path)?, before);
    Ok(())
}

#[test]
fn failed_index_admission_releases_the_reserved_slot() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let pool = admission::Pool::new(temp.path().join("pool"), 1)?;
    let mut session = Session::new(temp.path().join("absent"));
    session.admission_pool = Some(pool.clone());
    let (failed, _) = session.handle(Request::Reload { id: 1 });
    assert!(!failed.ok);
    assert_eq!(failed.error.unwrap().kind, "index_unavailable");
    assert_eq!(session.open_attempts, 1);
    assert!(
        !session.status()["reader_admission"]["lease_held"]
            .as_bool()
            .unwrap()
    );
    drop(pool.acquire()?);
    assert!(!session.index.exists());
    Ok(())
}

#[test]
fn semantic_service_invalid_optional_ann_does_not_block_exact_retrieval() -> Result<()> {
    use coding_agent_search::search::ann_index::hnsw_index_path;
    use coding_agent_search::search::embedder::Embedder;
    use coding_agent_search::search::hash_embedder::HashEmbedder;

    let (root, mut session, conversation) = semantic_service_fixture()?;
    // A directory at the expected sidecar path is unusable as ANN. The optional
    // accelerator must not become a prerequisite of the exact serving lane.
    let sidecar = hnsw_index_path(root.path(), HashEmbedder::default().id());
    std::fs::create_dir_all(&sidecar)?;
    let before = tree(root.path())?;
    for (id, approximate) in [(1, false), (2, true)] {
        let (reply, _) = session.handle(request(json!({"op":"semantic_search", "id":id,
            "query":"semanticneedle", "mode":"semantic", "limit":1, "approximate":approximate})));
        assert!(reply.ok, "{reply:?}");
        let result = reply.result.unwrap();
        assert_eq!(result["hits"][0]["conversation_id"], conversation);
        assert_eq!(result["hits"][0]["message_index"], 8);
        assert_eq!(result["realized_mode"], "semantic");
        assert_eq!(result["semantic_reused"], id > 1);
        assert_eq!(result["ann"]["requested"], approximate);
        assert_eq!(result["ann"]["used"], false);
        assert_eq!(result["ann"]["attempted"], false);
        assert!(result["ann"]["stats"].is_null());
        if approximate {
            assert_eq!(
                result["ann"]["unavailable_reason"],
                "persistent_native_ann_requires_reclaimable_owners"
            );
        } else {
            assert!(result["ann"]["unavailable_reason"].is_null());
        }
        assert_eq!(result["snapshot"]["semantic"]["ann_supported"], false);
    }
    session.unload();
    assert_eq!(tree(root.path())?, before);
    Ok(())
}
