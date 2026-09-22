// Included by mcp_tests.rs: exercise the real adapter, Quill reader and archive.
// Native inference is opt-in; the default admission test never needs a model.

struct RefinementEvidenceFixture {
    session: Session,
    conversation_id: i64,
    body: String,
    // Drop native owners and their lease before cleaning up the fixture tree.
    root: tempfile::TempDir,
}

fn refinement_evidence_fixture() -> anyhow::Result<RefinementEvidenceFixture> {
    use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use coding_agent_search::search::tantivy::TantivyIndex;
    use coding_agent_search::storage::sqlite::FrankenStorage;
    use frankensearch::quill::cass::CassDocument;

    let root = tempfile::tempdir()?;
    let source = root.path().join("absent-history.jsonl");
    let database = root.path().join("agent_search.db");
    let body = format!(
        "performanceneedle: reduce search latency by retaining one reader.\n{}\0CANONICAL-END: λ 🚀",
        "Complete canonical detail beyond the bounded preview. ".repeat(40)
    );
    let storage = FrankenStorage::open(&database)?;
    let agent_id = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let outcome = storage.insert_conversation_tree(
        agent_id,
        None,
        &Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some("refinement-evidence".into()),
            title: Some("Retained reader".into()),
            source_path: source.clone(),
            started_at: None,
            ended_at: None,
            approx_tokens: None,
            metadata_json: json!({}),
            messages: [
                (7, "Earlier context".to_string()),
                (12, body.clone()),
                (99, "Later context".to_string()),
            ]
            .into_iter()
            .map(|(idx, content)| Message {
                id: None,
                idx,
                role: MessageRole::Agent,
                author: None,
                created_at: None,
                content,
                extra_json: json!({}),
                snippets: Vec::new(),
            })
            .collect(),
            source_id: "local".into(),
            origin_host: None,
        },
    )?;
    drop(storage);
    let path = root.path().join("index");
    let mut writer = TantivyIndex::open_or_create(&path)?;
    writer.add_prebuilt_documents_slice(&[CassDocument {
        agent: "codex".into(),
        workspace: Some("/work".into()),
        workspace_original: None,
        source_path: source.to_string_lossy().into_owned(),
        msg_idx: 12,
        created_at: None,
        title: Some("Retained reader".into()),
        content: body.clone(),
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: Some(outcome.conversation_id),
    }])?;
    writer.commit()?;
    drop(writer);
    let mut session = Session::new(path);
    session.archive = Some(database);
    session.admission_pool = Some(super::super::admission::Pool::new(
        root.path().join("admission"),
        1,
    )?);
    Ok(RefinementEvidenceFixture {
        root,
        session,
        conversation_id: outcome.conversation_id,
        body,
    })
}

fn refinement_archive_image(db: &std::path::Path) -> anyhow::Result<Vec<Option<Vec<u8>>>> {
    ["", "-wal", "-shm"]
        .into_iter()
        .map(|suffix| {
            let mut path = db.as_os_str().to_os_string();
            path.push(suffix);
            match std::fs::read(PathBuf::from(path)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            }
        })
        .collect()
}

fn refinement_evidence_arguments() -> Value {
    json!({
        "lexical_query": "performanceneedle",
        "query": "Which change reduced search latency?",
        "candidate_limit": 10,
        "limit": 5,
        "filters": {"source_id": "local"}
    })
}

#[test]
fn mcp_refinement_respects_admission_and_unload_restores_capacity() -> anyhow::Result<()> {
    use super::super::admission::Refusal;

    let mut fixture = refinement_evidence_fixture()?;
    let session = &mut fixture.session;
    session.refiner =
        super::super::refinement::Refiner::new(Some(fixture.root.path().join("absent-model")));
    let database = session.archive.clone().unwrap();
    let archive_before = refinement_archive_image(&database)?;
    let pool = session.admission_pool.clone().unwrap();
    let mut peer = Session::new(session.index.clone());
    peer.admission_pool = Some(pool.clone());
    peer.ensure_loaded()?;
    let mut adapter = Adapter::default();
    initialize(&mut adapter, session, CURRENT_VERSION);
    let blocked = adapter
        .handle(
            session,
            call(
                "busy".into(),
                "cass_refine",
                refinement_evidence_arguments(),
            ),
        )
        .unwrap();
    assert_eq!(blocked["id"], "busy");
    assert_eq!(blocked["result"]["isError"], true, "{blocked}");
    assert_eq!(
        blocked["result"]["structuredContent"]["error"]["kind"],
        "admission_busy"
    );
    assert_eq!(session.open_attempts, 0);
    assert_eq!(session.refiner.status()["load_attempts"], 0);
    assert!(session.client.is_none());
    assert!(session.reader_lease.is_none());
    drop(peer);

    let unavailable = adapter
        .handle(
            session,
            call(
                "model-absent".into(),
                "cass_refine",
                refinement_evidence_arguments(),
            ),
        )
        .unwrap();
    assert_eq!(unavailable["result"]["isError"], true, "{unavailable}");
    assert_eq!(
        unavailable["result"]["structuredContent"]["error"]["kind"],
        "refinement_failed"
    );
    assert!(
        unavailable["result"]["structuredContent"]
            .get("hits")
            .is_none()
    );
    assert_eq!(session.successful_opens, 1);
    assert_eq!(session.refiner.status()["load_attempts"], 1);
    assert!(session.reader_lease.is_some());
    assert!(!session.refiner.loaded());
    assert_eq!(session.canonical_read_attempts, 0);
    assert!(matches!(pool.acquire(), Err(Refusal::Busy)));

    // Failed neural setup must not remove the useful retained lexical reader.
    let lexical = adapter
        .handle(
            session,
            call(
                "lexical".into(),
                "cass_search",
                json!({"query": "performanceneedle"}),
            ),
        )
        .unwrap();
    assert_eq!(lexical["result"]["isError"], false, "{lexical}");
    assert_eq!(lexical["result"]["structuredContent"]["count"], 1);
    assert_eq!(
        lexical["result"]["structuredContent"]["reader_reused"],
        true
    );
    assert_eq!(session.successful_opens, 1);
    assert_eq!(session.refiner.status()["load_attempts"], 1);

    let unloaded = adapter
        .handle(session, call("unload".into(), "cass_unload", json!({})))
        .unwrap();
    assert_eq!(unloaded["result"]["isError"], false, "{unloaded}");
    assert!(session.client.is_none());
    assert!(!session.refiner.loaded());
    assert!(session.refiner.enabled());
    assert!(session.reader_lease.is_none());
    let mut successor = Session::new(session.index.clone());
    successor.admission_pool = Some(pool);
    successor.ensure_loaded()?;
    assert_eq!(successor.successful_opens, 1);
    assert_eq!(refinement_archive_image(&database)?, archive_before);
    assert_eq!(session.canonical_read_attempts, 0);
    assert!(!fixture.root.path().join("absent-model").exists());
    assert!(!fixture.root.path().join("absent-history.jsonl").exists());
    Ok(())
}

#[test]
#[ignore = "requires an explicitly supplied native CASS_TEST_RERANKER_MODEL; never downloads a model"]
fn mcp_native_refinement_preserves_evidence_and_releases_owners() -> anyhow::Result<()> {
    use super::super::admission::Refusal;

    let model = PathBuf::from(dotenvy::var("CASS_TEST_RERANKER_MODEL")?);
    let mut fixture = refinement_evidence_fixture()?;
    let session = &mut fixture.session;
    session.refiner = super::super::refinement::Refiner::new(Some(model));
    let database = session.archive.clone().unwrap();
    let archive_before = refinement_archive_image(&database)?;
    let pool = session.admission_pool.clone().unwrap();
    let mut adapter = Adapter::default();
    initialize(&mut adapter, session, CURRENT_VERSION);
    for (id, reused) in [("first", false), ("retained", true)] {
        let refined = adapter
            .handle(
                session,
                call(id.into(), "cass_refine", refinement_evidence_arguments()),
            )
            .unwrap();
        assert_eq!(refined["id"], id);
        assert_eq!(refined["result"]["isError"], false, "{refined}");
        let result = &refined["result"]["structuredContent"];
        assert_eq!(result["count"], 1);
        assert_eq!(result["refinement"]["status"], "applied");
        assert_eq!(result["refinement"]["model_reused"], reused);
        assert_eq!(result["reader_reused"], reused);
        assert_eq!(result["refinement"]["model"]["vector_assets_loaded"], false);
        let hit = &result["hits"][0];
        assert_eq!(hit["conversation_id"], fixture.conversation_id);
        assert_eq!(hit["message_index"], 13);
        assert_eq!(hit["source_id"], "local");
        assert!(hit["rerank_score"].as_f64().is_some_and(f64::is_finite));
        assert!(hit["snippet"].as_str().unwrap().len() < fixture.body.len());
        assert_eq!(session.successful_opens, 1);
        assert_eq!(session.refiner.status()["successful_loads"], 1);
        assert!(matches!(pool.acquire(), Err(Refusal::Busy)));

        // Follow the exact hit, not a fabricated coordinate or raw file path.
        let view_id = format!("view-{id}");
        let viewed = adapter
            .handle(
                session,
                call(
                    view_id.clone().into(),
                    "cass_view",
                    json!({
                        "source_path": hit["source_path"],
                        "source_id": hit["source_id"],
                        "conversation_id": hit["conversation_id"],
                        "message_index": hit["message_index"],
                        "context": 1
                    }),
                ),
            )
            .unwrap();
        assert_eq!(viewed["id"], view_id);
        assert_eq!(viewed["result"]["isError"], false, "{viewed}");
        let evidence = &viewed["result"]["structuredContent"];
        let messages = evidence["messages"].as_array().unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|message| message["message_index"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [8, 13, 100]
        );
        assert_eq!(messages[1]["content"], fixture.body);
        assert_eq!(messages[1]["is_target"], true);
        assert_eq!(messages[0]["is_target"], false);
        assert_eq!(messages[2]["is_target"], false);
        let text: Value =
            serde_json::from_str(viewed["result"]["content"][0]["text"].as_str().unwrap())?;
        assert_eq!(&text, evidence);
        assert!(matches!(pool.acquire(), Err(Refusal::Busy)));
    }
    assert_eq!(session.canonical_reads_completed, 2);
    assert_eq!(session.refiner.status()["calls_completed"], 2);
    assert!(!fixture.root.path().join("absent-history.jsonl").exists());
    assert_eq!(refinement_archive_image(&database)?, archive_before);
    let unloaded = adapter
        .handle(session, call("unload".into(), "cass_unload", json!({})))
        .unwrap();
    assert_eq!(unloaded["result"]["isError"], false, "{unloaded}");
    assert!(!session.refiner.loaded());
    assert!(session.client.is_none());
    assert!(session.reader_lease.is_none());
    let mut successor = Session::new(session.index.clone());
    successor.admission_pool = Some(pool);
    successor.ensure_loaded()?;
    assert_eq!(successor.successful_opens, 1);
    Ok(())
}
