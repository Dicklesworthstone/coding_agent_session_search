use coding_agent_search::connectors::factory::FactoryConnector;
use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

// ============================================================================
// Helper
// ============================================================================

fn write_jsonl(dir: &Path, rel_path: &str, lines: &[&str]) -> std::path::PathBuf {
    let path = dir.join(rel_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, lines.join("\n")).unwrap();
    path
}

fn explicit_scan_ctx(root: &Path, since_ts: Option<i64>) -> ScanContext {
    ScanContext::with_roots(
        std::path::PathBuf::new(),
        vec![ScanRoot::local(root.to_path_buf())],
        since_ts,
    )
}

// ============================================================================
// Detection tests
// ============================================================================

#[test]
fn detect_does_not_panic() {
    let connector = FactoryConnector::new();
    let result = connector.detect();
    let _ = result.detected;
}

// ============================================================================
// Scan — happy path
// ============================================================================

#[test]
fn scan_parses_basic_session() {
    let tmp = TempDir::new().unwrap();
    // Path must contain "factory" and "sessions" to look like factory storage
    let root = tmp.path().join("factory/sessions");
    fs::create_dir_all(&root).unwrap();

    write_jsonl(
        &root,
        "-Users-alice-project/sess-001.jsonl",
        &[
            r#"{"type":"session_start","id":"sess-001","title":"Fix bug","cwd":"/Users/alice/project","owner":"alice"}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"Fix the login bug"}}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:05.000Z","message":{"role":"assistant","content":"I'll fix the login bug now."}}"#,
        ],
    );

    let connector = FactoryConnector::new();
    let ctx = explicit_scan_ctx(&root, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].agent_slug, "factory");
    assert_eq!(convs[0].external_id.as_deref(), Some("sess-001"));
    assert_eq!(convs[0].title.as_deref(), Some("Fix bug"));
    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].role, "user");
    assert!(convs[0].messages[0].content.contains("login bug"));
    assert_eq!(convs[0].messages[1].role, "assistant");
    assert!(convs[0].started_at.is_some());
    assert!(convs[0].ended_at.is_some());
}

#[test]
fn scan_multiple_sessions() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("factory/sessions");
    fs::create_dir_all(&root).unwrap();

    write_jsonl(
        &root,
        "workspace-a/sess-a.jsonl",
        &[
            r#"{"type":"session_start","id":"sess-a"}"#,
            r#"{"type":"message","timestamp":"2025-06-15T09:00:00.000Z","message":{"role":"user","content":"Session A"}}"#,
        ],
    );
    write_jsonl(
        &root,
        "workspace-b/sess-b.jsonl",
        &[
            r#"{"type":"session_start","id":"sess-b"}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"Session B"}}"#,
        ],
    );

    let connector = FactoryConnector::new();
    let ctx = explicit_scan_ctx(&root, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 2);
    let ids: Vec<&str> = convs
        .iter()
        .filter_map(|c| c.external_id.as_deref())
        .collect();
    assert!(ids.contains(&"sess-a"));
    assert!(ids.contains(&"sess-b"));
}

// ============================================================================
// Scan — title inference
// ============================================================================

#[test]
fn scan_infers_title_from_first_user_message() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("factory/sessions");
    fs::create_dir_all(&root).unwrap();

    // No title in session_start, should be inferred from first user message
    write_jsonl(
        &root,
        "workspace/no-title.jsonl",
        &[
            r#"{"type":"session_start","id":"no-title"}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"What is Rust?"}}"#,
        ],
    );

    let connector = FactoryConnector::new();
    let ctx = explicit_scan_ctx(&root, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title.as_deref(), Some("What is Rust?"));
}

// ============================================================================
// Scan — edge cases
// ============================================================================

#[test]
fn scan_empty_dir_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("factory/sessions");
    fs::create_dir_all(&root).unwrap();

    let connector = FactoryConnector::new();
    let ctx = explicit_scan_ctx(&root, None);
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn scan_skips_invalid_jsonl_lines() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("factory/sessions");
    fs::create_dir_all(&root).unwrap();

    write_jsonl(
        &root,
        "workspace/bad-lines.jsonl",
        &[
            r#"{"type":"session_start","id":"bad-lines"}"#,
            "not valid json {{{",
            "",
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"Survived"}}"#,
        ],
    );

    let connector = FactoryConnector::new();
    let ctx = explicit_scan_ctx(&root, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Survived");
}

#[test]
fn scan_skips_empty_content_messages() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("factory/sessions");
    fs::create_dir_all(&root).unwrap();

    write_jsonl(
        &root,
        "workspace/empties.jsonl",
        &[
            r#"{"type":"session_start","id":"empties"}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":""}}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:01.000Z","message":{"role":"assistant","content":"   "}}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:02.000Z","message":{"role":"user","content":"Real message"}}"#,
        ],
    );

    let connector = FactoryConnector::new();
    let ctx = explicit_scan_ctx(&root, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Real message");
}

#[test]
fn scan_skips_session_with_no_messages() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("factory/sessions");
    fs::create_dir_all(&root).unwrap();

    // Only session_start, no message entries
    write_jsonl(
        &root,
        "workspace/no-msgs.jsonl",
        &[r#"{"type":"session_start","id":"no-msgs","title":"Empty"}"#],
    );

    let connector = FactoryConnector::new();
    let ctx = explicit_scan_ctx(&root, None);
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn scan_skips_settings_json() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("factory/sessions");
    fs::create_dir_all(&root).unwrap();

    // Write a .settings.json file that should be ignored
    write_jsonl(
        &root,
        "workspace/sess.settings.json",
        &[r#"{"model":"gpt-4"}"#],
    );
    // And a valid session
    write_jsonl(
        &root,
        "workspace/sess.jsonl",
        &[
            r#"{"type":"session_start","id":"sess"}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"Hello"}}"#,
        ],
    );

    let connector = FactoryConnector::new();
    let ctx = explicit_scan_ctx(&root, None);
    let convs = connector.scan(&ctx).unwrap();

    // Should only pick up the .jsonl, not the .settings.json
    assert_eq!(convs.len(), 1);
}

// ============================================================================
// Message ordering
// ============================================================================

#[test]
fn scan_preserves_message_ordering() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("factory/sessions");
    fs::create_dir_all(&root).unwrap();

    write_jsonl(
        &root,
        "workspace/order.jsonl",
        &[
            r#"{"type":"session_start","id":"order"}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"First"}}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:01.000Z","message":{"role":"assistant","content":"Second"}}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:02.000Z","message":{"role":"user","content":"Third"}}"#,
        ],
    );

    let connector = FactoryConnector::new();
    let ctx = explicit_scan_ctx(&root, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages[0].idx, 0);
    assert_eq!(convs[0].messages[0].content, "First");
    assert_eq!(convs[0].messages[1].idx, 1);
    assert_eq!(convs[0].messages[1].content, "Second");
    assert_eq!(convs[0].messages[2].idx, 2);
    assert_eq!(convs[0].messages[2].content, "Third");
}

// ============================================================================
// Incremental scanning (since_ts)
// ============================================================================

#[test]
fn scan_respects_since_ts() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("factory/sessions");
    fs::create_dir_all(&root).unwrap();

    write_jsonl(
        &root,
        "workspace/old.jsonl",
        &[
            r#"{"type":"session_start","id":"old"}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"old msg"}}"#,
        ],
    );

    let connector = FactoryConnector::new();
    let far_future = chrono::Utc::now().timestamp_millis() + 86_400_000;
    let ctx = explicit_scan_ctx(&root, Some(far_future));
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

// ============================================================================
// Workspace extraction
// ============================================================================

#[test]
fn scan_extracts_workspace_from_session_start() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("factory/sessions");
    fs::create_dir_all(&root).unwrap();

    write_jsonl(
        &root,
        "workspace/ws.jsonl",
        &[
            r#"{"type":"session_start","id":"ws","cwd":"/home/user/myproject"}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"Hello"}}"#,
        ],
    );

    let connector = FactoryConnector::new();
    let ctx = explicit_scan_ctx(&root, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0]
            .workspace
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        Some("/home/user/myproject".to_string())
    );
}

// ============================================================================
// Author / model extraction
// ============================================================================

#[test]
fn scan_extracts_model_as_author() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("factory/sessions");
    fs::create_dir_all(&root).unwrap();

    write_jsonl(
        &root,
        "workspace/model.jsonl",
        &[
            r#"{"type":"session_start","id":"model"}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"assistant","content":"Hi","model":"gpt-4o"}}"#,
        ],
    );

    let connector = FactoryConnector::new();
    let ctx = explicit_scan_ctx(&root, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages[0].author.as_deref(), Some("gpt-4o"));
}
