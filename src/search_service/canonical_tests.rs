use super::*;
use super::super::{Session, protocol::Request};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use std::path::PathBuf;

struct Fixture {
    root: tempfile::TempDir,
    db: PathBuf,
    conversation: i64,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let db = root.path().join("archive.db");
        let storage = FrankenStorage::open(&db)?;
        let agent = storage.ensure_agent(&Agent {
            id: None, slug: "codex".into(), name: "Codex".into(),
            version: None, kind: AgentKind::Cli,
        })?;
        let outcome = storage.insert_conversation_tree(agent, None, &Conversation {
            id: None, agent_slug: "codex".into(), workspace: None,
            external_id: Some("canonical-service".into()), title: Some("Canonical service".into()),
            source_path: PathBuf::from("/absent/shared.sqlite"),
            started_at: None, ended_at: None, approx_tokens: None, metadata_json: json!({}),
            source_id: "local".into(), origin_host: None,
            messages: [0, 7, 12, 99, 1000].into_iter().map(|idx| Message {
                id: None, idx, role: MessageRole::Agent, author: None, created_at: None,
                content: format!("canonical content {idx}"), extra_json: json!({}), snippets: Vec::new(),
            }).collect(),
        })?;
        drop(storage);
        Ok(Self { root, db, conversation: outcome.conversation_id })
    }

    fn view(&self, context: usize) -> View<'_> {
        View { source_path: "/absent/shared.sqlite", source_id: "local",
            conversation_id: self.conversation, message_index: 13, context }
    }

    fn request(&self, id: u64, context: usize) -> Request {
        Request::View { id, source_path: "/absent/shared.sqlite".into(), source_id: "local".into(),
            conversation_id: self.conversation, message_index: 13, context }
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
        assert!(serde_json::from_value::<Request>(value).is_err(), "per-request DB authority is forbidden");
    }
    assert_eq!(session.status()["canonical_read_attempts"], 0);
    assert_eq!(session.open_attempts, 0);
    assert_eq!(session.status()["canonical_database_accessed"], false);
    Ok(())
}

#[test]
fn sparse_context_is_real_messages_with_exact_identity_and_no_raw_access() -> Result<()> {
    let fixture = Fixture::new()?;
    let before = std::fs::read(&fixture.db)?;
    let payload = read(&fixture.db, &fixture.view(1))?;
    let messages = payload["messages"].as_array().unwrap();
    assert_eq!(messages.iter().map(|m| m["message_index"].as_u64().unwrap()).collect::<Vec<_>>(), [8, 13, 100]);
    assert_eq!(messages[1]["content"], "canonical content 12");
    assert_eq!(messages.iter().filter(|m| m["is_target"] == true).count(), 1);
    assert_eq!(payload["source_path"], "/absent/shared.sqlite");
    assert_eq!(payload["source_id"], "local");
    assert_eq!(payload["conversation_id"], fixture.conversation);
    assert_eq!(payload["more_before"], true);
    assert_eq!(payload["more_after"], true);
    assert!(payload["matches_lexical_snapshot"].is_null());
    assert_eq!(std::fs::read(&fixture.db)?, before);
    Ok(())
}

#[test]
fn missing_coordinates_never_substitute_neighbours_or_other_sources() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut view = fixture.view(1);
    view.message_index = 12;
    assert_eq!(error_kind(&read(&fixture.db, &view).unwrap_err()), "canonical_not_found");
    view.message_index = 13;
    view.source_id = "work-laptop";
    assert_eq!(error_kind(&read(&fixture.db, &view).unwrap_err()), "canonical_identity_mismatch");
    view.source_id = "local";
    view.source_path = "/absent/SHARED.sqlite";
    assert_eq!(error_kind(&read(&fixture.db, &view).unwrap_err()), "canonical_identity_mismatch");
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
    assert_eq!(read(&fixture.db, &fixture.view(1))?["messages"].as_array().unwrap().len(), 3);
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
    assert!(payload["more_before"].is_null(), "context-zero does not probe neighbours");
    assert_eq!(error_kind(&read(&fixture.db, &fixture.view(1)).unwrap_err()), "canonical_payload_too_large");
    fixture.content(12, &"x".repeat(MAX_CONTENT_BYTES + 1))?;
    assert_eq!(error_kind(&read(&fixture.db, &fixture.view(0)).unwrap_err()), "canonical_payload_too_large");
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
    assert_eq!(second.result.unwrap()["messages"][0]["content"], "new canonical content after writer commit");
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
    let request = View { source_path: "/source", source_id: "local", conversation_id: 1, message_index: 1, context: 0 };
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
    let storage = FrankenStorage::open_strict_readonly(&fixture.db)?;
    let error = read_snapshot(&storage, &fixture.view(0), Instant::now() - Duration::from_secs(4)).unwrap_err();
    assert_eq!(error_kind(&error), "canonical_deadline");
    Ok(())
}
