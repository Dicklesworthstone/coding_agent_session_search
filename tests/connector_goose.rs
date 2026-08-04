//! Conformance harness for the Goose connector via CASS's FAD re-export.
//!
//! Goose stores sessions in SQLite from v1.20 onward at
//! `~/.local/share/goose/sessions/sessions.db`:
//!   `sessions`: id (TEXT PK), description, working_dir, created_at (INTEGER
//!               unix secs), updated_at, provider_name, model_config_json,
//!               session_type
//!   `messages`: session_id, role, content_json (JSON block array),
//!               created_timestamp (INTEGER unix secs), tokens, metadata_json,
//!               message_id (TEXT PK)
//! Pre-v1.20 releases used per-session `*.jsonl` files under the same sessions
//! directory (or legacy `~/.goose/sessions/`).
//!
//! Every scan here passes an explicit `ScanRoot` so the harness never falls
//! back to default detection: `local_default` leaves `scan_roots` empty, which
//! makes the connector *additionally* scan the developer's real
//! `~/.local/share/goose/sessions/`, breaking exact-count assertions on any
//! machine that actually runs Goose.

use coding_agent_search::connectors::goose::GooseConnector;
use coding_agent_search::connectors::{Connector, NormalizedConversation, ScanContext, ScanRoot};
use frankensqlite::Connection;
use frankensqlite::compat::ConnectionExt;
use frankensqlite::params;
use std::fs;
use std::path::{Path, PathBuf};
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

#[allow(clippy::too_many_arguments)]
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

fn insert_text_message(
    conn: &Connection,
    session_id: &str,
    message_id: &str,
    role: &str,
    text: &str,
    created_timestamp: i64,
) {
    let content_json = serde_json::json!([{"type": "text", "text": text}]).to_string();
    conn.execute_compat(
        "INSERT INTO messages
            (session_id, role, content_json, created_timestamp, tokens, metadata_json, message_id)
         VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5)",
        params![
            session_id,
            role,
            content_json,
            created_timestamp,
            message_id
        ],
    )
    .expect("insert goose message");
}

/// Scan with the db file itself as the explicit root, keeping the test
/// hermetic (no fallback to the developer's real Goose install).
fn scan_db(path: &Path) -> Vec<NormalizedConversation> {
    let connector = GooseConnector::new();
    let ctx = ScanContext::with_roots(
        PathBuf::new(),
        vec![ScanRoot::local(path.to_path_buf())],
        None,
    );
    connector.scan(&ctx).expect("goose scan should not error")
}

#[test]
fn goose_sqlite_happy_path_preserves_session_and_message_fields() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("sessions.db");
    let conn = create_goose_db(&db_path);

    insert_session(
        &conn,
        "sess-goose-1",
        Some("Refactor the billing module"),
        Some("/home/dev/billing"),
        1_700_000_000,
        1_700_000_060,
        Some("anthropic"),
        Some(r#"{"model": "claude-sonnet-4"}"#),
    );
    insert_text_message(
        &conn,
        "sess-goose-1",
        "msg-1",
        "user",
        "Split the invoice generator into its own crate",
        1_700_000_000,
    );
    insert_text_message(
        &conn,
        "sess-goose-1",
        "msg-2",
        "assistant",
        "I'll extract the invoice generator behind a trait first.",
        1_700_000_030,
    );
    drop(conn);

    let convs = scan_db(&db_path);
    assert_eq!(convs.len(), 1);
    let conv = &convs[0];

    assert_eq!(conv.agent_slug, "goose");
    assert_eq!(conv.external_id.as_deref(), Some("sess-goose-1"));
    assert_eq!(conv.title.as_deref(), Some("Refactor the billing module"));
    assert_eq!(
        conv.workspace.as_deref(),
        Some(Path::new("/home/dev/billing"))
    );
    // INTEGER unix seconds are normalized to unix millis.
    assert_eq!(conv.started_at, Some(1_700_000_000_000));
    assert_eq!(conv.ended_at, Some(1_700_000_060_000));
    // source_path is the db path joined with the url-encoded session id.
    assert_eq!(
        conv.source_path,
        db_path.join(urlencoding::encode("sess-goose-1").as_ref())
    );
    // Session-level metadata is preserved, including the parsed model name.
    assert_eq!(conv.metadata["session_id"], "sess-goose-1");
    assert_eq!(conv.metadata["provider_name"], "anthropic");
    assert_eq!(conv.metadata["model_name"], "claude-sonnet-4");
    assert_eq!(conv.metadata["source"], "sqlite");

    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].role, "user");
    assert!(conv.messages[0].content.contains("invoice generator"));
    assert_eq!(conv.messages[1].role, "assistant");
    assert!(conv.messages[1].content.contains("behind a trait"));
}

#[test]
fn goose_messages_are_ordered_by_created_timestamp() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("sessions.db");
    let conn = create_goose_db(&db_path);

    insert_session(
        &conn,
        "sess-order",
        Some("Ordering"),
        None,
        1_700_000_000,
        1_700_000_100,
        None,
        None,
    );
    // Insert out of chronological order; scan must sort ascending.
    insert_text_message(
        &conn,
        "sess-order",
        "m-late",
        "assistant",
        "third",
        1_700_000_090,
    );
    insert_text_message(
        &conn,
        "sess-order",
        "m-first",
        "user",
        "first",
        1_700_000_010,
    );
    insert_text_message(
        &conn,
        "sess-order",
        "m-mid",
        "assistant",
        "second",
        1_700_000_050,
    );
    drop(conn);

    let convs = scan_db(&db_path);
    assert_eq!(convs.len(), 1);
    let contents: Vec<&str> = convs[0]
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(contents, vec!["first", "second", "third"]);
}

#[test]
fn goose_session_without_messages_is_skipped() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("sessions.db");
    let conn = create_goose_db(&db_path);

    insert_session(
        &conn,
        "empty-sess",
        Some("No messages yet"),
        None,
        1_700_000_000,
        1_700_000_000,
        None,
        None,
    );
    drop(conn);

    assert!(
        scan_db(&db_path).is_empty(),
        "a session with zero messages must be skipped"
    );
}

#[test]
fn goose_zero_byte_db_yields_no_conversations() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("sessions.db");
    fs::write(&db_path, b"").unwrap();

    assert!(
        scan_db(&db_path).is_empty(),
        "a zero-byte db must be tolerated with no results"
    );
}

#[test]
fn goose_malformed_schema_db_yields_no_conversations() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("sessions.db");
    let conn = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();
    // Valid SQLite file, wrong schema entirely.
    conn.execute("CREATE TABLE unrelated (x TEXT)").unwrap();
    drop(conn);

    assert!(
        scan_db(&db_path).is_empty(),
        "a db without goose tables must be tolerated with no results"
    );
}

#[test]
fn goose_non_utf8_garbage_db_yields_no_conversations() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("sessions.db");
    fs::write(&db_path, [0xFFu8, 0xFE, 0x00, 0x9C, 0x80, 0x01]).unwrap();

    assert!(
        scan_db(&db_path).is_empty(),
        "non-SQLite garbage bytes must be tolerated with no results"
    );
}

#[test]
fn goose_legacy_jsonl_session_is_parsed() {
    let tmp = TempDir::new().unwrap();
    // Legacy pre-v1.20 layout: <root>/.goose/sessions/*.jsonl
    let sessions_dir = tmp.path().join(".goose").join("sessions");
    fs::create_dir_all(&sessions_dir).unwrap();
    let jsonl_path = sessions_dir.join("legacy-session.jsonl");
    fs::write(
        &jsonl_path,
        concat!(
            r#"{"role":"user","created_at":1700000000,"content":[{"type":"text","text":"Legacy goose question"}]}"#,
            "\n",
            r#"{"role":"assistant","created_at":1700000005,"content":[{"type":"text","text":"Legacy goose answer"}]}"#,
            "\n",
        ),
    )
    .unwrap();

    let connector = GooseConnector::new();
    let ctx = ScanContext::with_roots(
        PathBuf::new(),
        vec![ScanRoot::local(sessions_dir.clone())],
        None,
    );
    let convs = connector.scan(&ctx).expect("goose jsonl scan");

    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.agent_slug, "goose");
    assert_eq!(conv.source_path, jsonl_path);
    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].role, "user");
    assert!(conv.messages[0].content.contains("Legacy goose question"));
    assert_eq!(conv.messages[1].role, "assistant");
}

#[test]
fn goose_connector_is_registered_in_factory_registry() {
    let slugs: Vec<&str> = coding_agent_search::indexer::get_connector_factories()
        .into_iter()
        .map(|(slug, _)| slug)
        .collect();
    assert!(
        slugs.contains(&"goose"),
        "goose must be a registered ingest connector, got: {slugs:?}"
    );
}
