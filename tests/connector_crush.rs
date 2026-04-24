//! Conformance harness for the Crush connector via CASS's FAD re-export.

use coding_agent_search::connectors::crush::CrushConnector;
use coding_agent_search::connectors::{Connector, ScanContext};
use frankensqlite::Connection;
use frankensqlite::compat::ConnectionExt;
use frankensqlite::params;
use std::fs::{self, OpenOptions};
use std::path::Path;
use tempfile::TempDir;

fn create_crush_db(path: &Path) -> Connection {
    let conn = Connection::open(path.to_string_lossy().as_ref()).expect("open crush db");
    conn.execute(
        "CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            title TEXT,
            prompt_tokens INTEGER,
            completion_tokens INTEGER,
            cost REAL
        )",
    )
    .expect("create sessions");
    conn.execute(
        "CREATE TABLE messages (
            session_id TEXT,
            role TEXT,
            parts TEXT,
            created_at INTEGER,
            model TEXT,
            provider TEXT
        )",
    )
    .expect("create messages");
    conn
}

fn scan_db(path: &Path) -> Vec<coding_agent_search::connectors::NormalizedConversation> {
    let connector = CrushConnector::new();
    let ctx = ScanContext::local_default(path.to_path_buf(), None);
    connector.scan(&ctx).expect("crush scan should not panic")
}

#[test]
fn crush_happy_path_preserves_sqlite_session_fields() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("crush.db");
    let conn = create_crush_db(&db_path);

    conn.execute_compat(
        "INSERT INTO sessions (id, title, prompt_tokens, completion_tokens, cost)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params!["sess-crush-1", "Crush fixture", 11_i64, 7_i64, 0.42_f64],
    )
    .expect("insert session");
    conn.execute_compat(
        "INSERT INTO messages (session_id, role, parts, created_at, model, provider)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            "sess-crush-1",
            "user",
            r#"[{"type":"text","text":"Explain the Crush database format"}]"#,
            1_700_000_000_000_i64,
            Option::<String>::None,
            Option::<String>::None
        ],
    )
    .expect("insert user message");
    conn.execute_compat(
        "INSERT INTO messages (session_id, role, parts, created_at, model, provider)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            "sess-crush-1",
            "assistant",
            r#"[{"type":"text","text":"Crush stores sessions and message parts in SQLite."},{"type":"tool_use","text":"ignored"}]"#,
            1_700_000_001_000_i64,
            "claude-3-5-sonnet",
            "anthropic"
        ],
    )
    .expect("insert assistant message");
    drop(conn);

    let convs = scan_db(&db_path);
    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.agent_slug, "crush");
    assert_eq!(conv.external_id.as_deref(), Some("sess-crush-1"));
    assert_eq!(conv.title.as_deref(), Some("Crush fixture"));
    assert_eq!(conv.source_path, db_path);
    assert_eq!(conv.started_at, Some(1_700_000_000_000));
    assert_eq!(conv.ended_at, Some(1_700_000_001_000));
    assert_eq!(conv.metadata["prompt_tokens"], 11);
    assert_eq!(conv.metadata["completion_tokens"], 7);
    assert_eq!(conv.metadata["cost"], 0.42);

    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].idx, 0);
    assert_eq!(conv.messages[0].role, "user");
    assert_eq!(conv.messages[0].author.as_deref(), Some("user"));
    assert!(conv.messages[0].content.contains("Crush database"));
    assert_eq!(conv.messages[1].idx, 1);
    assert_eq!(conv.messages[1].role, "assistant");
    assert_eq!(
        conv.messages[1].author.as_deref(),
        Some("claude-3-5-sonnet")
    );
    assert!(conv.messages[1].content.contains("SQLite"));
    assert!(!conv.messages[1].content.contains("ignored"));
}

#[test]
fn crush_empty_zero_byte_db_returns_empty_result() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("empty.db");
    fs::write(&db_path, b"").unwrap();

    assert!(scan_db(&db_path).is_empty());
}

#[test]
fn crush_malformed_schema_returns_empty_result_without_panic() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("malformed.db");
    let conn = Connection::open(db_path.to_string_lossy().as_ref()).expect("open db");
    conn.execute("CREATE TABLE sessions (id TEXT PRIMARY KEY)")
        .expect("create incomplete sessions table");
    drop(conn);

    assert!(scan_db(&db_path).is_empty());
}

#[test]
fn crush_non_utf8_bytes_return_empty_result_without_panic() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("non_utf8.db");
    fs::write(&db_path, [0xff, 0xfe, 0xfd, 0x00, 0x80]).unwrap();

    assert!(scan_db(&db_path).is_empty());
}

#[test]
fn crush_oversized_sparse_db_returns_empty_result_without_panic() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("huge.db");
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&db_path)
        .unwrap();
    file.set_len(101 * 1024 * 1024).unwrap();
    drop(file);

    assert!(scan_db(&db_path).is_empty());
}
