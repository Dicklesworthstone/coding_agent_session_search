use coding_agent_search::connectors::cursor::CursorConnector;
use coding_agent_search::connectors::{Connector, ScanContext};
use frankensqlite::Connection as FrankenConnection;
use frankensqlite::compat::ConnectionExt;
use serde_json::json;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

// ============================================================================
// Helper
// ============================================================================

/// Create a test SQLite database with the cursorDiskKV and ItemTable tables.
fn create_test_db(path: &Path) -> FrankenConnection {
    let conn = FrankenConnection::open(path.to_string_lossy().into_owned()).unwrap();
    conn.execute("CREATE TABLE IF NOT EXISTS cursorDiskKV (key TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    conn.execute("CREATE TABLE IF NOT EXISTS ItemTable (key TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    conn
}

fn insert_kv(conn: &FrankenConnection, key: &str, value: &str) {
    conn.execute_compat(
        "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
        frankensqlite::params![key, value],
    )
    .unwrap();
}

fn insert_item(conn: &FrankenConnection, key: &str, value: &str) {
    conn.execute_compat(
        "INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2)",
        frankensqlite::params![key, value],
    )
    .unwrap();
}

// ============================================================================
// Detection tests
// ============================================================================

#[test]
fn detect_does_not_panic() {
    let connector = CursorConnector::new();
    let result = connector.detect();
    let _ = result.detected;
}

// ============================================================================
// Scan — composerData with tabs/bubbles format (v0.3x)
// ============================================================================

#[test]
fn scan_parses_tabs_bubbles_format() {
    let tmp = TempDir::new().unwrap();
    let global_dir = tmp.path().join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();

    let db_path = global_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);

    let composer_data = json!({
        "createdAt": 1700000000000i64,
        "tabs": [{
            "bubbles": [
                {
                    "type": "user",
                    "text": "How do I sort a Vec?",
                    "timestamp": 1700000000000i64
                },
                {
                    "type": "ai",
                    "text": "Use .sort() or .sort_by().",
                    "model": "gpt-4",
                    "timestamp": 1700000001000i64
                }
            ]
        }]
    });

    insert_kv(
        &conn,
        "composerData:comp-001",
        &serde_json::to_string(&composer_data).unwrap(),
    );
    drop(conn);

    let connector = CursorConnector::new();
    let ctx = ScanContext::local_default(tmp.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].agent_slug, "cursor");
    assert_eq!(convs[0].external_id.as_deref(), Some("comp-001"));
    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].role, "user");
    assert!(convs[0].messages[0].content.contains("sort"));
    assert_eq!(convs[0].messages[1].role, "assistant");
    assert!(convs[0].started_at.is_some());
}

// ============================================================================
// Scan — numeric bubble types (v0.40+)
// ============================================================================

#[test]
fn scan_parses_numeric_bubble_types() {
    let tmp = TempDir::new().unwrap();
    let global_dir = tmp.path().join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();

    let db_path = global_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);

    let composer_data = json!({
        "createdAt": 1700000000000i64,
        "tabs": [{
            "bubbles": [
                {
                    "type": 1,
                    "text": "User question",
                    "timestamp": 1700000000000i64
                },
                {
                    "type": 2,
                    "text": "Assistant answer",
                    "modelType": "claude-3.5-sonnet",
                    "timestamp": 1700000001000i64
                }
            ]
        }]
    });

    insert_kv(
        &conn,
        "composerData:comp-numeric",
        &serde_json::to_string(&composer_data).unwrap(),
    );
    drop(conn);

    let connector = CursorConnector::new();
    let ctx = ScanContext::local_default(tmp.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].role, "user");
    assert_eq!(convs[0].messages[1].role, "assistant");
    assert_eq!(
        convs[0].messages[1].author.as_deref(),
        Some("claude-3.5-sonnet")
    );
}

// ============================================================================
// Scan — text/richText simple format
// ============================================================================

#[test]
fn scan_parses_simple_text_format() {
    let tmp = TempDir::new().unwrap();
    let global_dir = tmp.path().join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();

    let db_path = global_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);

    // Simple format: just text, no tabs/bubbles
    let composer_data = json!({
        "createdAt": 1700000000000i64,
        "text": "A simple user prompt"
    });

    insert_kv(
        &conn,
        "composerData:comp-simple",
        &serde_json::to_string(&composer_data).unwrap(),
    );
    drop(conn);

    let connector = CursorConnector::new();
    let ctx = ScanContext::local_default(tmp.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].role, "user");
    assert_eq!(convs[0].messages[0].content, "A simple user prompt");
}

// ============================================================================
// Scan — legacy aichat.chatdata format
// ============================================================================

#[test]
fn scan_parses_aichat_chatdata() {
    let tmp = TempDir::new().unwrap();
    let global_dir = tmp.path().join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();

    let db_path = global_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);

    let aichat_data = json!({
        "tabs": [{
            "timestamp": 1700000000000i64,
            "bubbles": [
                {
                    "type": "user",
                    "text": "Legacy question",
                    "timestamp": 1700000000000i64
                },
                {
                    "type": "ai",
                    "text": "Legacy answer",
                    "timestamp": 1700000001000i64
                }
            ]
        }]
    });

    insert_item(
        &conn,
        "workbench.panel.aichat.view.aichat.chatdata",
        &serde_json::to_string(&aichat_data).unwrap(),
    );
    drop(conn);

    let connector = CursorConnector::new();
    let ctx = ScanContext::local_default(tmp.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].role, "user");
    assert_eq!(convs[0].messages[0].content, "Legacy question");
    assert_eq!(convs[0].messages[1].role, "assistant");
}

// ============================================================================
// Scan — multiple conversations
// ============================================================================

#[test]
fn scan_parses_multiple_composers() {
    let tmp = TempDir::new().unwrap();
    let global_dir = tmp.path().join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();

    let db_path = global_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);

    for i in 1..=3 {
        let data = json!({
            "createdAt": 1700000000000i64 + i * 1000,
            "text": format!("Composer {i}")
        });
        insert_kv(
            &conn,
            &format!("composerData:comp-{i}"),
            &serde_json::to_string(&data).unwrap(),
        );
    }
    drop(conn);

    let connector = CursorConnector::new();
    let ctx = ScanContext::local_default(tmp.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 3);
}

// ============================================================================
// Scan — workspace storage
// ============================================================================

#[test]
fn scan_finds_workspace_storage_dbs() {
    let tmp = TempDir::new().unwrap();
    let ws_dir = tmp.path().join("workspaceStorage/ws-abc");
    fs::create_dir_all(&ws_dir).unwrap();

    let db_path = ws_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);

    let data = json!({
        "createdAt": 1700000000000i64,
        "text": "From workspace storage"
    });
    insert_kv(
        &conn,
        "composerData:comp-ws",
        &serde_json::to_string(&data).unwrap(),
    );
    drop(conn);

    let connector = CursorConnector::new();
    let ctx = ScanContext::local_default(tmp.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].content, "From workspace storage");
}

// ============================================================================
// Edge cases
// ============================================================================

#[test]
fn scan_empty_dir_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let connector = CursorConnector::new();
    let ctx = ScanContext::local_default(tmp.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn scan_skips_empty_text_composers() {
    let tmp = TempDir::new().unwrap();
    let global_dir = tmp.path().join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();

    let db_path = global_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);

    // Empty text should result in no messages, so it should be skipped
    let data = json!({
        "createdAt": 1700000000000i64,
        "text": ""
    });
    insert_kv(
        &conn,
        "composerData:comp-empty",
        &serde_json::to_string(&data).unwrap(),
    );

    // Valid one
    let data2 = json!({
        "createdAt": 1700000001000i64,
        "text": "Valid prompt"
    });
    insert_kv(
        &conn,
        "composerData:comp-valid",
        &serde_json::to_string(&data2).unwrap(),
    );
    drop(conn);

    let connector = CursorConnector::new();
    let ctx = ScanContext::local_default(tmp.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Valid prompt");
}

#[test]
fn scan_skips_empty_bubbles() {
    let tmp = TempDir::new().unwrap();
    let global_dir = tmp.path().join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();

    let db_path = global_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);

    let data = json!({
        "createdAt": 1700000000000i64,
        "tabs": [{
            "bubbles": [
                {"type": "user", "text": ""},
                {"type": "ai", "text": "   "},
                {"type": "user", "text": "Real content"}
            ]
        }]
    });
    insert_kv(
        &conn,
        "composerData:comp-empty-bubbles",
        &serde_json::to_string(&data).unwrap(),
    );
    drop(conn);

    let connector = CursorConnector::new();
    let ctx = ScanContext::local_default(tmp.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Real content");
}

// ============================================================================
// Message ordering
// ============================================================================

#[test]
fn scan_preserves_bubble_ordering() {
    let tmp = TempDir::new().unwrap();
    let global_dir = tmp.path().join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();

    let db_path = global_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);

    let data = json!({
        "createdAt": 1700000000000i64,
        "tabs": [{
            "bubbles": [
                {"type": 1, "text": "First", "timestamp": 1700000000000i64},
                {"type": 2, "text": "Second", "timestamp": 1700000001000i64},
                {"type": 1, "text": "Third", "timestamp": 1700000002000i64}
            ]
        }]
    });
    insert_kv(
        &conn,
        "composerData:comp-order",
        &serde_json::to_string(&data).unwrap(),
    );
    drop(conn);

    let connector = CursorConnector::new();
    let ctx = ScanContext::local_default(tmp.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages[0].idx, 0);
    assert_eq!(convs[0].messages[0].content, "First");
    assert_eq!(convs[0].messages[1].idx, 1);
    assert_eq!(convs[0].messages[1].content, "Second");
    assert_eq!(convs[0].messages[2].idx, 2);
    assert_eq!(convs[0].messages[2].content, "Third");
}

// ============================================================================
// Title extraction
// ============================================================================

#[test]
fn scan_extracts_name_as_title() {
    let tmp = TempDir::new().unwrap();
    let global_dir = tmp.path().join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();

    let db_path = global_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);

    let data = json!({
        "name": "My Composer Session",
        "createdAt": 1700000000000i64,
        "text": "Hello world"
    });
    insert_kv(
        &conn,
        "composerData:comp-named",
        &serde_json::to_string(&data).unwrap(),
    );
    drop(conn);

    let connector = CursorConnector::new();
    let ctx = ScanContext::local_default(tmp.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].title.as_deref(), Some("My Composer Session"));
}

// ============================================================================
// Incremental scanning (since_ts)
// ============================================================================

#[test]
fn scan_respects_since_ts() {
    let tmp = TempDir::new().unwrap();
    let global_dir = tmp.path().join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();

    let db_path = global_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);

    let data = json!({
        "createdAt": 1700000000000i64,
        "text": "Old composer"
    });
    insert_kv(
        &conn,
        "composerData:comp-old",
        &serde_json::to_string(&data).unwrap(),
    );
    drop(conn);

    let connector = CursorConnector::new();
    let far_future = chrono::Utc::now().timestamp_millis() + 86_400_000;
    let ctx = ScanContext::local_default(tmp.path().to_path_buf(), Some(far_future));
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}
