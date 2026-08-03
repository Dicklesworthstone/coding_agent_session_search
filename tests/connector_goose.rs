//! Conformance harness for the Goose connector via CASS's FAD re-export.
//!
//! Goose v1.20+ stores sessions in a SQLite database at
//! `~/.local/share/goose/sessions/sessions.db`. Schema:
//!   `sessions`:  id (TEXT PK), description, working_dir,
//!                created_at / updated_at (INTEGER epoch secs),
//!                provider_name, model_config_json, session_type
//!   `messages`:  message_id (TEXT PK), session_id, role,
//!                content_json (JSON block array), created_timestamp
//!                (INTEGER epoch secs), tokens, metadata_json
//!
//! Pre-v1.20 installs use `*.jsonl` files under the same sessions directory
//! (or legacy `~/.goose/sessions/`); the connector reads both shapes.
//!
//! Goose was detection-only in cass until the FAD `goose` feature was enabled.
//! This mirrors the resilience edge cases the other SQLite-backed connectors
//! (crush, hermes) assert, so a malformed or partial Goose database degrades to
//! an empty scan instead of failing the whole index run.

use coding_agent_search::connectors::goose::GooseConnector;
use coding_agent_search::connectors::{Connector, NormalizedConversation, ScanContext, ScanRoot};
use frankensqlite::Connection;
use frankensqlite::compat::ConnectionExt;
use frankensqlite::params;
use serde_json::json;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn create_goose_db(path: &Path) -> Connection {
    let conn = Connection::open(path.to_string_lossy().as_ref()).expect("open goose db");
    conn.execute(
        "CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            description TEXT,
            working_dir TEXT,
            created_at INTEGER,
            updated_at INTEGER,
            provider_name TEXT,
            model_config_json TEXT,
            session_type TEXT
        )",
    )
    .expect("create sessions");
    conn.execute(
        "CREATE TABLE messages (
            session_id TEXT,
            role TEXT,
            content_json TEXT,
            created_timestamp INTEGER,
            tokens INTEGER,
            metadata_json TEXT,
            message_id TEXT PRIMARY KEY
        )",
    )
    .expect("create messages");
    conn
}

fn insert_session(
    conn: &Connection,
    id: &str,
    description: Option<&str>,
    working_dir: Option<&str>,
    created_at: i64,
    updated_at: i64,
    provider_name: Option<&str>,
    model_config_json: Option<&str>,
) {
    conn.execute_compat(
        "INSERT INTO sessions
            (id, description, working_dir, created_at, updated_at,
             provider_name, model_config_json, session_type)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
        params![
            id,
            description,
            working_dir,
            created_at,
            updated_at,
            provider_name,
            model_config_json
        ],
    )
    .expect("insert goose session");
}

fn insert_message(
    conn: &Connection,
    message_id: &str,
    session_id: &str,
    role: &str,
    text: &str,
    created_timestamp: i64,
) {
    let content_json = json!([{ "type": "text", "text": text }]).to_string();
    conn.execute_compat(
        "INSERT INTO messages
            (session_id, role, content_json, created_timestamp, tokens, metadata_json, message_id)
         VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5)",
        params![session_id, role, content_json, created_timestamp, message_id],
    )
    .expect("insert goose message");
}

/// Scan a single Goose database with an explicit scan root.
///
/// The explicit root matters for hermeticity: `ScanContext::local_default`
/// leaves `scan_roots` empty, which puts the connector in default-detection
/// mode and makes it additionally probe the *developer's real*
/// `~/.local/share/goose/sessions/`. Passing the db file as an explicit
/// `ScanRoot` keeps the scan confined to the fixture — `append_db_candidates`
/// takes any `*.db` root verbatim, so fixtures need not be named
/// `sessions.db`.
fn scan_db(path: &Path) -> Vec<NormalizedConversation> {
    let connector = GooseConnector::new();
    let ctx = ScanContext::with_roots(
        std::path::PathBuf::new(),
        vec![ScanRoot::local(path.to_path_buf())],
        None,
    );
    connector.scan(&ctx).expect("goose scan should not panic")
}

#[test]
fn goose_happy_path_preserves_session_and_message_fields() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("sessions.db");
    let conn = create_goose_db(&db_path);

    insert_session(
        &conn,
        "sess-goose-1",
        Some("Refactor the indexer"),
        Some("/home/user/proj"),
        1_700_000_000,
        1_700_000_200,
        Some("anthropic"),
        Some(r#"{"model": "claude-sonnet-4"}"#),
    );
    insert_message(
        &conn,
        "msg-1",
        "sess-goose-1",
        "user",
        "How do I split this module?",
        1_700_000_010,
    );
    insert_message(
        &conn,
        "msg-2",
        "sess-goose-1",
        "assistant",
        "Start by extracting the parser.",
        1_700_000_020,
    );
    drop(conn);

    let convs = scan_db(&db_path);
    assert_eq!(convs.len(), 1, "one session must yield one conversation");

    let conv = &convs[0];
    assert_eq!(conv.agent_slug, "goose");
    assert_eq!(conv.external_id.as_deref(), Some("sess-goose-1"));
    assert_eq!(conv.title.as_deref(), Some("Refactor the indexer"));
    assert_eq!(
        conv.workspace.as_deref(),
        Some(Path::new("/home/user/proj")),
        "working_dir must survive as the conversation workspace"
    );
    assert_eq!(conv.metadata["session_id"], "sess-goose-1");
    assert_eq!(conv.metadata["provider_name"], "anthropic");
    assert_eq!(
        conv.metadata["model_name"], "claude-sonnet-4",
        "model must be parsed out of model_config_json"
    );
    assert_eq!(conv.metadata["source"], "sqlite");

    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].role, "user");
    assert!(conv.messages[0].content.contains("split this module"));
    assert_eq!(conv.messages[1].role, "assistant");
    assert!(conv.messages[1].content.contains("extracting the parser"));
}

#[test]
fn goose_messages_are_ordered_by_timestamp_not_insertion() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("sessions.db");
    let conn = create_goose_db(&db_path);

    insert_session(
        &conn,
        "sess-order",
        None,
        None,
        1_700_000_000,
        1_700_000_300,
        None,
        None,
    );
    // Inserted out of order on purpose: the connector orders by timestamp.
    insert_message(&conn, "msg-late", "sess-order", "assistant", "second", 200);
    insert_message(&conn, "msg-early", "sess-order", "user", "first", 100);
    drop(conn);

    let convs = scan_db(&db_path);
    assert_eq!(convs.len(), 1);
    let contents: Vec<&str> = convs[0]
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(
        contents,
        vec!["first", "second"],
        "messages must be ordered by created_timestamp"
    );
}

#[test]
fn goose_session_without_messages_is_skipped() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("sessions.db");
    let conn = create_goose_db(&db_path);

    insert_session(
        &conn,
        "sess-empty",
        Some("No messages here"),
        None,
        1_700_000_000,
        1_700_000_000,
        None,
        None,
    );
    drop(conn);

    assert!(
        scan_db(&db_path).is_empty(),
        "a session with zero messages must not produce a conversation"
    );
}

#[test]
fn goose_empty_zero_byte_db_returns_empty_result() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("empty.db");
    fs::write(&db_path, b"").unwrap();

    assert!(scan_db(&db_path).is_empty());
}

#[test]
fn goose_malformed_schema_returns_empty_result_without_panic() {
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
fn goose_non_utf8_bytes_return_empty_result_without_panic() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("non_utf8.db");
    fs::write(&db_path, [0xff, 0xfe, 0xfd, 0x00, 0x80]).unwrap();

    assert!(scan_db(&db_path).is_empty());
}
