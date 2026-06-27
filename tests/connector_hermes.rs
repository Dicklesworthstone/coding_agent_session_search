//! Conformance harness for the Hermes connector via CASS's FAD re-export.
//!
//! Hermes stores sessions in `~/.hermes/state.db` (SQLite). Schema:
//!   `sessions`: id (TEXT PK), source, model, title, parent_session_id,
//!               started_at (REAL secs), ended_at (REAL secs), end_reason,
//!               message_count, tool_call_count, input_tokens, output_tokens
//!   `messages`: session_id (FK), role, content, tool_calls (JSON), tool_name,
//!               tool_call_id, reasoning, timestamp (REAL secs)
//!
//! Hermes was the one connector (of 20) with no test (gauntlet finding
//! CONF-cass-006 / F7-4). This closes that gap with a happy path plus the same
//! resilience edge cases the other SQLite connectors (crush) assert.

use coding_agent_search::connectors::hermes::HermesConnector;
use coding_agent_search::connectors::{Connector, NormalizedConversation, ScanContext};
use frankensqlite::Connection;
use frankensqlite::compat::ConnectionExt;
use frankensqlite::params;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn create_hermes_db(path: &Path) -> Connection {
    let conn = Connection::open(path.to_string_lossy().as_ref()).expect("open hermes db");
    conn.execute(
        "CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            source TEXT,
            model TEXT,
            title TEXT,
            parent_session_id TEXT,
            started_at REAL,
            ended_at REAL,
            end_reason TEXT,
            message_count INTEGER,
            tool_call_count INTEGER,
            input_tokens INTEGER,
            output_tokens INTEGER
        )",
    )
    .expect("create sessions");
    conn.execute(
        "CREATE TABLE messages (
            session_id TEXT,
            role TEXT,
            content TEXT,
            tool_calls TEXT,
            tool_name TEXT,
            tool_call_id TEXT,
            reasoning TEXT,
            timestamp REAL
        )",
    )
    .expect("create messages");
    conn
}

#[allow(clippy::too_many_arguments)]
fn insert_session(
    conn: &Connection,
    id: &str,
    source: Option<&str>,
    model: Option<&str>,
    title: Option<&str>,
    started_at: Option<f64>,
    ended_at: Option<f64>,
    message_count: i64,
    input_tokens: i64,
    output_tokens: i64,
) {
    conn.execute_compat(
        "INSERT INTO sessions
            (id, source, model, title, parent_session_id, started_at, ended_at,
             end_reason, message_count, tool_call_count, input_tokens, output_tokens)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, NULL, ?7, 0, ?8, ?9)",
        params![
            id,
            source,
            model,
            title,
            started_at,
            ended_at,
            message_count,
            input_tokens,
            output_tokens
        ],
    )
    .expect("insert hermes session");
}

fn insert_message(conn: &Connection, session_id: &str, role: &str, content: &str, timestamp: f64) {
    conn.execute_compat(
        "INSERT INTO messages
            (session_id, role, content, tool_calls, tool_name, tool_call_id, reasoning, timestamp)
         VALUES (?1, ?2, ?3, NULL, NULL, NULL, NULL, ?4)",
        params![session_id, role, content, timestamp],
    )
    .expect("insert hermes message");
}

fn scan_db(path: &Path) -> Vec<NormalizedConversation> {
    let connector = HermesConnector::new();
    let ctx = ScanContext::local_default(path.to_path_buf(), None);
    connector.scan(&ctx).expect("hermes scan should not panic")
}

#[test]
fn hermes_happy_path_preserves_session_and_message_fields() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("state.db");
    let conn = create_hermes_db(&db_path);

    insert_session(
        &conn,
        "sess-hermes-1",
        Some("cli"),
        Some("claude-3-5-sonnet"),
        Some("Hermes fixture"),
        Some(1_700_000_000.0), // REAL unix seconds -> 1_700_000_000_000 ms
        Some(1_700_000_002.0),
        2,
        11,
        7,
    );
    insert_message(
        &conn,
        "sess-hermes-1",
        "user",
        "Explain the Hermes state.db format",
        1_700_000_000.0,
    );
    insert_message(
        &conn,
        "sess-hermes-1",
        "assistant",
        "Hermes stores sessions and messages in SQLite.",
        1_700_000_002.0,
    );
    drop(conn);

    let convs = scan_db(&db_path);
    assert_eq!(convs.len(), 1);
    let conv = &convs[0];

    assert_eq!(conv.agent_slug, "hermes");
    assert_eq!(conv.external_id.as_deref(), Some("sess-hermes-1"));
    assert_eq!(conv.title.as_deref(), Some("Hermes fixture"));
    // REAL seconds are converted to unix millis.
    assert_eq!(conv.started_at, Some(1_700_000_000_000));
    assert_eq!(conv.ended_at, Some(1_700_000_002_000));
    // source_path is db_path joined with the url-encoded session id.
    assert_eq!(
        conv.source_path,
        db_path.join(urlencoding::encode("sess-hermes-1").as_ref())
    );
    // Session-level metadata is preserved.
    assert_eq!(conv.metadata["session_id"], "sess-hermes-1");
    assert_eq!(conv.metadata["source"], "cli");
    assert_eq!(conv.metadata["model"], "claude-3-5-sonnet");
    assert_eq!(conv.metadata["input_tokens"], 11);
    assert_eq!(conv.metadata["output_tokens"], 7);

    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].idx, 0);
    assert_eq!(conv.messages[0].role, "user");
    assert_eq!(conv.messages[0].author.as_deref(), Some("user"));
    assert!(conv.messages[0].content.contains("Hermes state.db"));
    assert_eq!(conv.messages[1].idx, 1);
    assert_eq!(conv.messages[1].role, "assistant");
    // Assistant author is None at message level (model lives at session level).
    assert_eq!(conv.messages[1].author, None);
    assert!(conv.messages[1].content.contains("SQLite"));
}

#[test]
fn hermes_session_without_messages_is_skipped() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("state.db");
    let conn = create_hermes_db(&db_path);

    // A session row with no message rows must not synthesize a conversation.
    insert_session(
        &conn,
        "empty-sess",
        Some("cli"),
        None,
        Some("No messages"),
        Some(1_700_000_000.0),
        Some(1_700_000_000.0),
        0,
        0,
        0,
    );
    drop(conn);

    assert!(
        scan_db(&db_path).is_empty(),
        "a session with zero messages must be skipped"
    );
}

#[test]
fn hermes_session_meta_role_messages_are_skipped() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("state.db");
    let conn = create_hermes_db(&db_path);

    insert_session(
        &conn,
        "meta-sess",
        Some("cli"),
        None,
        Some("Meta filtering"),
        Some(1_700_000_000.0),
        Some(1_700_000_001.0),
        2,
        0,
        0,
    );
    // `session_meta` rows carry bookkeeping, not conversation content; the
    // connector contract drops them.
    insert_message(
        &conn,
        "meta-sess",
        "session_meta",
        "this metadata record must not appear as a message",
        1_700_000_000.0,
    );
    insert_message(
        &conn,
        "meta-sess",
        "user",
        "this is the only real message",
        1_700_000_001.0,
    );
    drop(conn);

    let convs = scan_db(&db_path);
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].messages.len(),
        1,
        "session_meta row must be dropped"
    );
    assert_eq!(convs[0].messages[0].role, "user");
    assert!(convs[0].messages[0].content.contains("only real message"));
}

#[test]
fn hermes_empty_zero_byte_db_returns_empty_result() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("empty.db");
    fs::write(&db_path, b"").unwrap();

    assert!(scan_db(&db_path).is_empty());
}

#[test]
fn hermes_malformed_schema_returns_empty_result_without_panic() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("malformed.db");
    let conn = Connection::open(db_path.to_string_lossy().as_ref()).expect("open db");
    // `sessions` exists but `messages` is missing — scan must degrade to empty.
    conn.execute("CREATE TABLE sessions (id TEXT PRIMARY KEY)")
        .expect("create incomplete sessions table");
    drop(conn);

    assert!(scan_db(&db_path).is_empty());
}

#[test]
fn hermes_non_utf8_bytes_return_empty_result_without_panic() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("non_utf8.db");
    fs::write(&db_path, [0xff, 0xfe, 0xfd, 0x00, 0x80]).unwrap();

    assert!(scan_db(&db_path).is_empty());
}
