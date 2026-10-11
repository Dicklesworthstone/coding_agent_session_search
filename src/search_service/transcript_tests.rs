use super::*;
use crate::search_service::{Session, admission, protocol};
use std::collections::BTreeMap;
use std::path::PathBuf;

const SOURCE: &str = "/never-opened/transcript-source.jsonl";

struct Fixture {
    root: tempfile::TempDir,
    db: PathBuf,
}

impl Fixture {
    fn new(rows: &[(i64, i64, &str)]) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let db = root.path().join("archive.db");
        let writer = Connection::open(db.to_str().context("fixture path")?)?;
        writer.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE conversations (id INTEGER PRIMARY KEY, source_path TEXT, source_id TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, conversation_id INTEGER, idx INTEGER, role TEXT, content TEXT);
             CREATE INDEX messages_by_conversation ON messages(conversation_id, idx);",
        )?;
        writer.execute_compat("INSERT INTO conversations VALUES (1, ?1, 'local')", params![SOURCE])?;
        for &(id, idx, content) in rows {
            writer.execute_compat(
                "INSERT INTO messages VALUES (?1, 1, ?2, 'agent', ?3)",
                params![id, idx, content],
            )?;
        }
        writer.close()?;
        Ok(Self { root, db })
    }

    fn writer(&self) -> Result<Connection> {
        Ok(Connection::open(self.db.to_str().context("fixture path")?)?)
    }

    fn image(&self) -> Result<Vec<Option<Vec<u8>>>> {
        ["", "-wal", "-shm", "-journal"].into_iter().map(|suffix| {
            let mut path = self.db.as_os_str().to_os_string();
            path.push(suffix);
            match std::fs::read(PathBuf::from(path)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            }
        }).collect()
    }
}

fn page(cursor: Option<&Cursor>, limit: usize) -> Page<'_> {
    Page { source_path: SOURCE, source_id: "local", conversation_id: 1, cursor, limit }
}

fn next(result: &Value) -> Result<Option<Cursor>> {
    Ok(serde_json::from_value(result["next_cursor"].clone())?)
}

fn wire(cursor: Option<Cursor>, limit: usize) -> Result<protocol::Request> {
    Ok(serde_json::from_value(json!({
        "op": "transcript", "id": 7, "source_path": SOURCE,
        "source_id": "local", "conversation_id": 1, "cursor": cursor, "limit": limit,
    }))?)
}

#[test]
fn transcript_pages_walk_sparse_indices_and_the_largest_canonical_index() -> Result<()> {
    let fixture = Fixture::new(&[(1, 0, "first"), (2, 7, "second"), (3, i64::MAX, "last")])?;
    let before = fixture.image()?;
    let first = read(&fixture.db, &page(None, 2))?;
    assert_eq!(first["count"], 2);
    assert_eq!(first["messages"][0]["message_index"], 1);
    assert_eq!(first["messages"][1]["message_index"], 8);
    assert_eq!(first["messages"][0]["role"], "assistant");
    assert_eq!(first["end_of_conversation"], false);
    let cursor = next(&first)?.context("next page")?;
    assert_eq!(cursor.message_index, (i64::MAX as u64) + 1);
    let last = read(&fixture.db, &page(Some(&cursor), 2))?;
    assert_eq!(last["count"], 1);
    assert_eq!(last["messages"][0]["content"], "last");
    assert_eq!(last["end_of_conversation"], true);
    assert!(next(&last)?.is_none());
    assert_eq!(fixture.image()?, before);
    Ok(())
}

#[test]
fn transcript_large_unicode_messages_reassemble_exactly_including_nuls() -> Result<()> {
    let text = format!("{}😀{}\0tail", "a".repeat(MAX_PAGE_BYTES - 1), "雪".repeat(MAX_PAGE_BYTES));
    let fixture = Fixture::new(&[(1, 0, ""), (2, 9, &text), (3, 12, "after\0message")])?;
    let before = fixture.image()?;
    let mut cursor = None;
    let mut bodies: BTreeMap<i64, String> = BTreeMap::new();
    let mut partial_seen = false;
    let mut finished = false;
    for _ in 0..10 {
        let result = read(&fixture.db, &page(cursor.as_ref(), 2))?;
        assert!(result["content_bytes"].as_u64().unwrap() <= MAX_PAGE_BYTES as u64);
        assert_eq!(result["cross_page_snapshot"], false);
        for message in result["messages"].as_array().context("messages")? {
            let id = message["message_id"].as_i64().context("message id")?;
            let body = bodies.entry(id).or_default();
            assert_eq!(message["byte_offset"].as_u64(), Some(body.len() as u64));
            let chunk = message["content"].as_str().context("chunk")?;
            body.push_str(chunk);
            assert_eq!(message["end_byte_offset"].as_u64(), Some(body.len() as u64));
            partial_seen |= message["partial_message"] == true;
        }
        cursor = next(&result)?;
        if cursor.is_none() {
            finished = true;
            break;
        }
    }
    assert!(finished && partial_seen);
    assert_eq!(bodies.get(&1).map(String::as_str), Some(""));
    assert_eq!(bodies.get(&2), Some(&text));
    assert_eq!(bodies.get(&3).map(String::as_str), Some("after\0message"));
    assert_eq!(fixture.image()?, before);
    Ok(())
}

#[test]
fn transcript_empty_conversations_succeed_but_unknown_identities_do_not() -> Result<()> {
    let fixture = Fixture::new(&[])?;
    let result = read(&fixture.db, &page(None, 1))?;
    assert_eq!(result["count"], 0);
    assert_eq!(result["end_of_conversation"], true);
    let error = read(&fixture.db, &Page { conversation_id: 2, ..page(None, 1) }).unwrap_err();
    assert_eq!(error_kind(&error), "canonical_not_found");
    for wrong in [
        Page { source_id: "other", ..page(None, 1) },
        Page { source_path: "/different/source", ..page(None, 1) },
    ] {
        let error = read(&fixture.db, &wrong).unwrap_err();
        assert_eq!(error_kind(&error), "canonical_identity_mismatch");
    }
    Ok(())
}

#[test]
fn transcript_cursor_refuses_deleted_replaced_or_resized_message() -> Result<()> {
    for action in ["deleted", "replaced", "resized"] {
        let fixture = Fixture::new(&[(1, 0, &"x".repeat(MAX_PAGE_BYTES + 20)), (2, 7, "neighbour")])?;
        let first = read(&fixture.db, &page(None, 1))?;
        let cursor = next(&first)?.context("partial cursor")?;
        let writer = fixture.writer()?;
        match action {
            "deleted" => { writer.execute("DELETE FROM messages WHERE id = 1")?; }
            "replaced" => { writer.execute("UPDATE messages SET id = 3 WHERE id = 1")?; }
            _ => { writer.execute("UPDATE messages SET content = 'different length' WHERE id = 1")?; }
        }
        writer.close()?;
        let before = fixture.image()?;
        let error = read(&fixture.db, &page(Some(&cursor), 2)).unwrap_err();
        assert_eq!(error_kind(&error), "canonical_cursor_changed", "{action}: {error}");
        assert_eq!(fixture.image()?, before);
    }
    Ok(())
}

#[test]
fn transcript_rejects_invalid_offsets_and_ambiguous_or_negative_indices() -> Result<()> {
    let fixture = Fixture::new(&[(1, 0, "😀suffix")])?;
    let cursor = Cursor { message_id: 1, message_index: 1, byte_offset: 1, content_bytes: Some(10) };
    let error = read(&fixture.db, &page(Some(&cursor), 1)).unwrap_err();
    assert_eq!(error_kind(&error), "canonical_invalid_content");
    for rows in [vec![(1, -1, "invalid")], vec![(1, 0, "first"), (2, 0, "duplicate")]] {
        let fixture = Fixture::new(&rows)?;
        let error = read(&fixture.db, &page(None, 1)).unwrap_err();
        assert_eq!(error_kind(&error), "canonical_invalid_coordinates");
    }
    Ok(())
}

#[test]
fn transcript_late_bad_body_fails_the_whole_page_and_leaves_archive_unchanged() -> Result<()> {
    let fixture = Fixture::new(&[(1, 0, "PRIVATE-TRANSCRIPT-FIRST"), (2, 1, "original")])?;
    let writer = fixture.writer()?;
    writer.execute("UPDATE messages SET content = x'ff00' WHERE id = 2")?;
    writer.close()?;
    let before = fixture.image()?;
    let error = read(&fixture.db, &page(None, 2)).unwrap_err();
    assert_eq!(error_kind(&error), "canonical_invalid_content");
    assert!(!error.to_string().contains("PRIVATE-TRANSCRIPT-FIRST"));
    assert_eq!(fixture.image()?, before);
    Ok(())
}

#[test]
fn transcript_bounds_empty_messages_and_never_returns_a_nonadvancing_cursor() -> Result<()> {
    let rows: Vec<_> = (1..=80).map(|id| (id, id * 7, "")).collect();
    let fixture = Fixture::new(&rows)?;
    let mut cursor = None;
    let mut seen = 0;
    for _ in 0..3 {
        let result = read(&fixture.db, &page(cursor.as_ref(), MAX_MESSAGES))?;
        assert_eq!(result["content_bytes"], 0);
        seen += result["count"].as_u64().context("count")?;
        let subsequent = next(&result)?;
        if let (Some(before), Some(after)) = (&cursor, &subsequent) {
            assert!(after.message_index > before.message_index);
        }
        cursor = subsequent;
    }
    assert_eq!(seen, 80);
    assert!(cursor.is_none());
    Ok(())
}

#[test]
fn transcript_validation_and_disabled_access_remain_lazy() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut session = Session::new(root.path().join("absent-index"));
    let disabled = session.handle(wire(None, 1)?).0;
    assert_eq!(disabled.error.context("error")?.kind, "canonical_access_disabled");
    session.archive = Some(root.path().join("absent.db"));
    for limit in [0, MAX_MESSAGES + 1, usize::MAX] {
        let reply = session.handle(wire(None, limit)?).0;
        assert_eq!(reply.error.context("error")?.kind, "invalid_request");
    }
    for cursor in [
        Cursor { message_id: 0, message_index: 1, byte_offset: 0, content_bytes: None },
        Cursor { message_id: 1, message_index: 0, byte_offset: 0, content_bytes: None },
        Cursor { message_id: 1, message_index: u64::MAX, byte_offset: 0, content_bytes: None },
        Cursor { message_id: 1, message_index: 1, byte_offset: u64::MAX, content_bytes: Some(u64::MAX) },
        Cursor { message_id: 1, message_index: 1, byte_offset: 1, content_bytes: None },
        Cursor { message_id: 1, message_index: 1, byte_offset: 2, content_bytes: Some(2) },
    ] {
        let reply = session.handle(wire(Some(cursor), 1)?).0;
        assert_eq!(reply.error.context("error")?.kind, "invalid_request");
    }
    assert_eq!(session.canonical_read_attempts, 0);
    assert_eq!(session.open_attempts, 0);
    assert_eq!(std::fs::read_dir(root.path())?.count(), 0);
    assert!(serde_json::from_value::<protocol::Request>(json!({
        "op":"transcript", "id":1, "source_path":SOURCE, "source_id":"local", "conversation_id":1,
        "cursor":{"message_id":1,"message_index":1,"byte_offset":0,"content_bytes":null,"db":"/other"}
    })).is_err());
    Ok(())
}

#[test]
fn transcript_service_releases_its_temporary_admission_after_success_and_failure() -> Result<()> {
    let fixture = Fixture::new(&[(1, 0, "exact canonical body")])?;
    let pool = admission::Pool::new(fixture.root.path().join("pool"), 1)?;
    let mut session = Session::new(fixture.root.path().join("absent-index"));
    session.archive = Some(fixture.db.clone());
    session.admission_pool = Some(pool.clone());
    let held = pool.acquire()?;
    let reply = session.handle(wire(None, 1)?).0;
    assert_eq!(reply.error.context("error")?.kind, "admission_busy");
    assert_eq!(session.canonical_read_attempts, 0);
    drop(held);
    assert!(session.handle(wire(None, 1)?).0.ok);
    drop(pool.acquire()?);
    let cursor = Cursor { message_id: 99, message_index: 1, byte_offset: 0, content_bytes: None };
    let failed = session.handle(wire(Some(cursor), 1)?).0;
    assert_eq!(failed.error.context("error")?.kind, "canonical_cursor_changed");
    drop(pool.acquire()?);
    assert_eq!(session.canonical_read_attempts, 2);
    assert_eq!(session.canonical_reads_completed, 1);
    assert_eq!(session.open_attempts, 0);
    assert!(session.client.is_none());
    Ok(())
}

#[test]
fn transcript_enforces_deadline_and_states_cross_page_consistency_honestly() -> Result<()> {
    let fixture = Fixture::new(&[(1, 0, "first"), (2, 7, "old")])?;
    let reader = super::super::open_archive(&fixture.db)?;
    let started = Instant::now() - std::time::Duration::from_secs(4);
    let error = read_page(&reader, &page(None, 1), started).unwrap_err();
    assert_eq!(error_kind(&error), "canonical_deadline");
    drop(reader);
    let first = read(&fixture.db, &page(None, 1))?;
    let cursor = next(&first)?.context("next")?;
    let writer = fixture.writer()?;
    writer.execute("UPDATE messages SET content = 'new' WHERE id = 2")?;
    writer.close()?;
    let second = read(&fixture.db, &page(Some(&cursor), 1))?;
    assert_eq!(second["messages"][0]["content"], "new");
    assert_eq!(second["cross_page_snapshot"], false);
    assert_eq!(second["cursor_validation"], "message_identity_and_length_only");
    assert!(second["matches_lexical_snapshot"].is_null());
    Ok(())
}
