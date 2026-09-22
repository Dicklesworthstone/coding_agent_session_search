use super::super::{Session, protocol::Request};
use super::*;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::storage::sqlite::FrankenStorage;
use std::path::PathBuf;

struct Fixture {
    root: tempfile::TempDir,
    db: PathBuf,
    conversation: i64,
}

fn archive_image(db: &Path) -> Result<Vec<(PathBuf, Option<Vec<u8>>)>> {
    let mut image = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let mut name = db.as_os_str().to_os_string();
        name.push(suffix);
        let path = PathBuf::from(name);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        image.push((path, bytes));
    }
    Ok(image)
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let db = root.path().join("archive.db");
        let storage = FrankenStorage::open(&db)?;
        let agent = storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let outcome = storage.insert_conversation_tree(
            agent,
            None,
            &Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: None,
                external_id: Some("canonical-service".into()),
                title: Some("Canonical service".into()),
                source_path: PathBuf::from("/absent/shared.sqlite"),
                started_at: None,
                ended_at: None,
                approx_tokens: None,
                metadata_json: json!({}),
                source_id: "local".into(),
                origin_host: None,
                messages: [0, 7, 12, 99, 1000]
                    .into_iter()
                    .map(|idx| Message {
                        id: None,
                        idx,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: None,
                        content: format!("canonical content {idx}"),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    })
                    .collect(),
            },
        )?;
        drop(storage);
        Ok(Self {
            root,
            db,
            conversation: outcome.conversation_id,
        })
    }

    fn view(&self, context: usize) -> View<'_> {
        View {
            source_path: "/absent/shared.sqlite",
            source_id: "local",
            conversation_id: self.conversation,
            message_index: 13,
            context,
        }
    }

    fn request(&self, id: u64, context: usize) -> Request {
        Request::View {
            id,
            source_path: "/absent/shared.sqlite".into(),
            source_id: "local".into(),
            conversation_id: self.conversation,
            message_index: 13,
            context,
        }
    }

    fn content(&self, idx: i64, content: &str) -> Result<()> {
        let storage = FrankenStorage::open(&self.db)?;
        storage.raw().execute_compat(
            "UPDATE messages SET content = ?1 WHERE conversation_id = ?2 AND idx = ?3",
            params![content, self.conversation, idx],
        )?;
        Ok(())
    }
}

#[test]
fn canonical_service_requires_opt_in_and_refuses_invalid_work_before_access() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut session = Session::new(fixture.root.path().join("absent-index"));
    let (reply, stop) = session.handle(fixture.request(7, 1));
    assert!(!stop);
    assert_eq!(reply.error.unwrap().kind, "canonical_access_disabled");
    session.archive = Some(fixture.db.clone());
    for (index, context) in [(0, 0), (u64::MAX, 0), (13, MAX_CONTEXT + 1)] {
        let mut value = json!({"op":"view", "id":9, "source_path":"/absent/shared.sqlite",
            "source_id":"local", "conversation_id":fixture.conversation,
            "message_index":index, "context":context});
        let (reply, _) = session.handle(serde_json::from_value(value.clone())?);
        assert_eq!(reply.error.unwrap().kind, "invalid_request");
        value["db"] = json!(fixture.db);
        assert!(
            serde_json::from_value::<Request>(value).is_err(),
            "per-request DB authority is forbidden"
        );
    }
    assert_eq!(session.status()["canonical_read_attempts"], 0);
    assert_eq!(session.open_attempts, 0);
    assert_eq!(session.status()["canonical_database_accessed"], false);
    Ok(())
}

#[test]
fn sparse_context_is_real_messages_with_exact_identity_and_no_raw_access() -> Result<()> {
    let fixture = Fixture::new()?;
    let before = archive_image(&fixture.db)?;
    let payload = read(&fixture.db, &fixture.view(1))?;
    let messages = payload["messages"].as_array().unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|m| m["message_index"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [8, 13, 100]
    );
    assert_eq!(messages[1]["content"], "canonical content 12");
    assert_eq!(
        messages.iter().filter(|m| m["is_target"] == true).count(),
        1
    );
    assert_eq!(payload["source_path"], "/absent/shared.sqlite");
    assert_eq!(payload["source_id"], "local");
    assert_eq!(payload["conversation_id"], fixture.conversation);
    assert_eq!(payload["more_before"], true);
    assert_eq!(payload["more_after"], true);
    assert!(payload["matches_lexical_snapshot"].is_null());
    assert_eq!(archive_image(&fixture.db)?, before);
    Ok(())
}

#[test]
fn missing_coordinates_never_substitute_neighbours_or_other_sources() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut view = fixture.view(1);
    view.message_index = 12;
    assert_eq!(
        error_kind(&read(&fixture.db, &view).unwrap_err()),
        "canonical_not_found"
    );
    view.message_index = 13;
    view.source_id = "work-laptop";
    assert_eq!(
        error_kind(&read(&fixture.db, &view).unwrap_err()),
        "canonical_identity_mismatch"
    );
    view.source_id = "local";
    view.source_path = "/absent/SHARED.sqlite";
    assert_eq!(
        error_kind(&read(&fixture.db, &view).unwrap_err()),
        "canonical_identity_mismatch"
    );
    Ok(())
}

#[test]
fn excluded_malformed_payloads_are_not_decoded_but_requested_ones_fail() -> Result<()> {
    let fixture = Fixture::new()?;
    {
        let storage = FrankenStorage::open(&fixture.db)?;
        storage.raw().execute_compat(
            "UPDATE messages SET role = ?1 WHERE conversation_id = ?2 AND idx = 0",
            params![vec![255_u8], fixture.conversation],
        )?;
    }
    assert_eq!(
        read(&fixture.db, &fixture.view(1))?["messages"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let failure = read(&fixture.db, &fixture.view(2)).unwrap_err();
    assert!(format!("{failure:#}").contains("canonical role must be text"));
    // The rejected read's rollback must not leave a transaction or reader alive.
    assert!(read(&fixture.db, &fixture.view(0)).is_ok());
    Ok(())
}

#[test]
fn content_budget_counts_utf8_and_embedded_nuls_without_partial_success() -> Result<()> {
    let fixture = Fixture::new()?;
    let exact = "\0é".repeat(MAX_CONTENT_BYTES / 3) + "x";
    assert_eq!(exact.len(), MAX_CONTENT_BYTES);
    fixture.content(12, &exact)?;
    let payload = read(&fixture.db, &fixture.view(0))?;
    assert_eq!(payload["messages"][0]["content"], exact);
    assert_eq!(payload["content_bytes"], MAX_CONTENT_BYTES);
    assert!(
        payload["more_before"].is_null(),
        "context-zero does not probe neighbours"
    );
    assert_eq!(
        error_kind(&read(&fixture.db, &fixture.view(1)).unwrap_err()),
        "canonical_payload_too_large"
    );
    fixture.content(12, &"x".repeat(MAX_CONTENT_BYTES + 1))?;
    assert_eq!(
        error_kind(&read(&fixture.db, &fixture.view(0)).unwrap_err()),
        "canonical_payload_too_large"
    );
    fixture.content(12, "readable after refusal")?;
    assert!(read(&fixture.db, &fixture.view(0)).is_ok());
    Ok(())
}

#[test]
fn canonical_views_do_not_load_or_relabel_the_lexical_snapshot() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut session = Session::new(fixture.root.path().join("absent-index"));
    session.archive = Some(fixture.db.clone());
    let (first, stop) = session.handle(fixture.request(1, 0));
    assert!(first.ok && !stop, "{first:?}");
    fixture.content(12, "new canonical content after writer commit")?;
    let (second, _) = session.handle(fixture.request(2, 0));
    assert!(second.ok, "{second:?}");
    assert_eq!(
        second.result.unwrap()["messages"][0]["content"],
        "new canonical content after writer commit"
    );
    assert_eq!(session.open_attempts, 0);
    assert_eq!(session.status()["canonical_read_attempts"], 2);
    assert_eq!(session.status()["canonical_reads_completed"], 2);
    assert!(session.status()["reader_epoch"].is_null());
    assert_eq!(session.status()["freshness"], "not_checked");
    Ok(())
}

#[test]
fn corrupt_or_missing_archive_fails_without_creating_or_repairing_it() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("archive.db");
    let request = View {
        source_path: "/source",
        source_id: "local",
        conversation_id: 1,
        message_index: 1,
        context: 0,
    };
    assert!(read(&path, &request).is_err());
    assert!(!path.exists());
    std::fs::write(&path, b"not an archive")?;
    assert!(read(&path, &request).is_err());
    assert_eq!(std::fs::read(&path)?, b"not an archive");
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlink_archive_is_not_admitted() -> Result<()> {
    let fixture = Fixture::new()?;
    let link = fixture.root.path().join("alias.db");
    std::os::unix::fs::symlink(&fixture.db, &link)?;
    assert!(format!("{:#}", read(&link, &fixture.view(0)).unwrap_err()).contains("not a symlink"));
    Ok(())
}

#[test]
fn expired_budget_is_an_explicit_refusal_before_snapshot_queries() -> Result<()> {
    let fixture = Fixture::new()?;
    let connection = open_archive(&fixture.db)?;
    let error = read_snapshot(
        &connection,
        &fixture.view(0),
        Instant::now() - Duration::from_secs(4),
    )
    .unwrap_err();
    assert_eq!(error_kind(&error), "canonical_deadline");
    Ok(())
}

fn mcp_exchange(session: &mut Session, requests: &[Value]) -> Result<Vec<Value>> {
    let mut frames = vec![
        json!({"jsonrpc":"2.0", "id":"init", "method":"initialize", "params":{
            "protocolVersion":"2025-11-25", "capabilities":{},
            "clientInfo":{"name":"canonical-regression", "version":"1"}
        }}),
        json!({"jsonrpc":"2.0", "method":"notifications/initialized"}),
    ];
    frames.extend_from_slice(requests);
    let input = frames
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mut output = Vec::new();
    super::super::mcp::serve_io(session, &mut std::io::Cursor::new(input), &mut output)?;
    std::str::from_utf8(&output)?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

fn mcp_view(fixture: &Fixture, id: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "method":"tools/call", "params":{
        "name":"cass_view", "arguments":{
            "source_path":"/absent/shared.sqlite", "source_id":"local",
            "conversation_id":fixture.conversation, "message_index":13, "context":1
        }
    }})
}

#[test]
fn mcp_catalog_and_dispatch_require_explicit_canonical_permission() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut session = Session::new(fixture.root.path().join("absent-index"));
    let list = json!({"jsonrpc":"2.0", "id":1, "method":"tools/list"});
    let replies = mcp_exchange(&mut session, &[list.clone(), mcp_view(&fixture, json!(2))])?;
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 4);
    assert_eq!(replies[2]["error"]["code"], -32602);
    assert_eq!(session.canonical_read_attempts, 0);
    session.archive = Some(fixture.db.clone());
    let replies = mcp_exchange(&mut session, &[list])?;
    let tools = replies[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 5);
    let view = tools
        .iter()
        .find(|tool| tool["name"] == "cass_view")
        .unwrap();
    assert_eq!(view["annotations"]["readOnlyHint"], true);
    assert_eq!(view["inputSchema"]["additionalProperties"], false);
    assert_eq!(view["inputSchema"]["required"].as_array().unwrap().len(), 4);
    assert_eq!(
        view["inputSchema"]["properties"]["context"]["maximum"],
        MAX_CONTEXT
    );
    assert_eq!(
        session.canonical_read_attempts, 0,
        "discovery must not open the archive"
    );
    Ok(())
}

#[test]
fn mcp_view_round_trip_preserves_full_evidence_and_rpc_identity() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.content(12, "full evidence: quotes \" and NUL \0 and Unicode δ😀")?;
    let before = archive_image(&fixture.db)?;
    let mut session = Session::new(fixture.root.path().join("absent-index"));
    session.archive = Some(fixture.db.clone());
    let replies = mcp_exchange(&mut session, &[mcp_view(&fixture, json!("view-δ"))])?;
    let reply = &replies[1];
    assert_eq!(reply["id"], "view-δ");
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let data = &reply["result"]["structuredContent"];
    let text: Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap())?;
    assert_eq!(&text, data);
    assert_eq!(
        data["messages"][1]["content"],
        "full evidence: quotes \" and NUL \0 and Unicode δ😀"
    );
    assert!(data["matches_lexical_snapshot"].is_null());
    assert_eq!(session.open_attempts, 0);
    assert_eq!(session.canonical_reads_completed, 1);
    assert_eq!(archive_image(&fixture.db)?, before);
    Ok(())
}

#[test]
fn mcp_view_rejects_path_escalation_and_fire_and_forget_database_reads() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut session = Session::new(fixture.root.path().join("absent-index"));
    session.archive = Some(fixture.db.clone());
    let mut notification = mcp_view(&fixture, json!(1));
    notification.as_object_mut().unwrap().remove("id");
    let mut escalation = mcp_view(&fixture, json!(2));
    escalation["params"]["arguments"]["db"] = json!("/different/archive.db");
    let mut invalid = mcp_view(&fixture, json!(3));
    invalid["params"]["arguments"]["context"] = json!(MAX_CONTEXT + 1);
    let replies = mcp_exchange(&mut session, &[notification, escalation, invalid])?;
    assert_eq!(
        replies.len(),
        3,
        "tool notifications must receive no response"
    );
    assert_eq!(replies[1]["error"]["code"], -32602);
    assert_eq!(replies[2]["result"]["isError"], true);
    assert_eq!(
        replies[2]["result"]["structuredContent"]["error"]["kind"],
        "invalid_request"
    );
    assert_eq!(session.canonical_read_attempts, 0);
    Ok(())
}

#[test]
fn mcp_oversized_canonical_body_is_a_tool_error_not_partial_evidence() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.content(12, &"x".repeat(MAX_CONTENT_BYTES + 1))?;
    let mut session = Session::new(fixture.root.path().join("absent-index"));
    session.archive = Some(fixture.db.clone());
    let replies = mcp_exchange(&mut session, &[mcp_view(&fixture, json!(-17))])?;
    let reply = &replies[1];
    assert_eq!(reply["id"], -17);
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(
        reply["result"]["structuredContent"]["error"]["kind"],
        "canonical_payload_too_large"
    );
    assert!(
        reply["result"]["structuredContent"]
            .get("messages")
            .is_none()
    );
    assert_eq!(session.canonical_reads_completed, 0);
    Ok(())
}

#[test]
fn retained_read_transaction_cannot_mix_identity_with_a_newer_body() -> Result<()> {
    let fixture = Fixture::new()?;
    let connection = open_archive(&fixture.db)?;
    let snapshot = Snapshot::begin(&connection)?;
    let pinned = connection.query_map_collect(
        "SELECT id FROM conversations WHERE id = ?1",
        params![fixture.conversation],
        |row| row.get_typed::<i64>(0),
    )?;
    assert_eq!(pinned, vec![fixture.conversation]);
    fixture.content(12, "new body from concurrent writer")?;
    let before = archive_image(&fixture.db)?;
    let old = read_snapshot(&connection, &fixture.view(0), Instant::now())?;
    assert_eq!(old["messages"][0]["content"], "canonical content 12");
    snapshot.release()?;
    drop(connection);
    let new = read(&fixture.db, &fixture.view(0))?;
    assert_eq!(
        new["messages"][0]["content"],
        "new body from concurrent writer"
    );
    assert_eq!(archive_image(&fixture.db)?, before);
    Ok(())
}

#[test]
fn canonical_connection_enforces_engine_read_only_not_just_query_only() -> Result<()> {
    let fixture = Fixture::new()?;
    let before = archive_image(&fixture.db)?;
    let connection = open_archive(&fixture.db)?;
    assert_eq!(
        connection
            .query_row("PRAGMA query_only")?
            .get_typed::<i64>(0)?,
        1
    );
    let mutation = "CREATE TABLE forbidden_service_write (id INTEGER PRIMARY KEY)";
    assert!(connection.execute(mutation).is_err());
    // Even disabling the SQL policy cannot upgrade the read-only engine open.
    connection.execute("PRAGMA query_only = OFF")?;
    assert!(connection.execute(mutation).is_err());
    connection.close_without_checkpoint()?;
    assert_eq!(archive_image(&fixture.db)?, before);
    Ok(())
}

#[test]
fn canonical_reads_share_admission_and_release_temporary_leases() -> Result<()> {
    let fixture = Fixture::new()?;
    let pool = super::super::admission::Pool::new(fixture.root.path().join("pool"), 1)?;
    let mut session = Session::new(fixture.root.path().join("absent-index"));
    session.archive = Some(fixture.db.clone());
    session.admission_pool = Some(pool.clone());
    let before = archive_image(&fixture.db)?;
    let holder = pool.acquire()?;
    let (refused, _) = session.handle(fixture.request(1, 0));
    assert_eq!(refused.error.unwrap().kind, "admission_busy");
    assert_eq!(session.canonical_read_attempts, 0);
    drop(holder);
    for id in [2, 3] {
        let (reply, _) = session.handle(fixture.request(id, 0));
        assert!(reply.ok, "{reply:?}");
        assert_eq!(
            reply.result.unwrap()["messages"][0]["content"],
            "canonical content 12"
        );
        drop(pool.acquire()?);
    }
    assert_eq!(session.open_attempts, 0);
    assert_eq!(archive_image(&fixture.db)?, before);
    Ok(())
}
