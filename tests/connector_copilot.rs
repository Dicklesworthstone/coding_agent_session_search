use coding_agent_search::connectors::copilot::CopilotConnector;
use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

// ============================================================================
// Helper
// ============================================================================

fn write_json(dir: &Path, filename: &str, content: &str) -> std::path::PathBuf {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    path
}

fn load_fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("copilot")
        .join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read copilot fixture {}: {err}", path.display()))
}

// ============================================================================
// Detection tests
// ============================================================================

#[test]
fn detect_does_not_panic() {
    let connector = CopilotConnector::new();
    let result = connector.detect();
    let _ = result.detected;
}

// ============================================================================
// Scan — turns format
// ============================================================================

#[test]
fn scan_parses_turns_format() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("copilot-chat");
    fs::create_dir_all(&root).unwrap();

    let json = r#"[{
        "id": "conv-001",
        "workspaceFolder": "/home/user/project",
        "turns": [
            {
                "request": { "message": "How do I sort?", "timestamp": 1700000000000 },
                "response": { "message": "Use .sort().", "timestamp": 1700000001000 }
            }
        ]
    }]"#;

    write_json(&root, "conversations.json", json);

    let connector = CopilotConnector::new();
    let ctx = ScanContext::local_default(root, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].agent_slug, "copilot");
    assert_eq!(convs[0].external_id.as_deref(), Some("conv-001"));
    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].role, "user");
    assert!(convs[0].messages[0].content.contains("sort"));
    assert_eq!(convs[0].messages[1].role, "assistant");
    assert!(convs[0].started_at.is_some());
    assert!(convs[0].ended_at.is_some());
}

// ============================================================================
// Scan — messages format
// ============================================================================

#[test]
fn scan_parses_messages_format() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("copilot-chat");
    fs::create_dir_all(&root).unwrap();

    let json = r#"{
        "id": "conv-002",
        "title": "Explain lifetimes",
        "messages": [
            { "role": "user", "content": "Explain lifetimes", "timestamp": 1700000010000 },
            { "role": "assistant", "content": "Lifetimes express scope validity.", "timestamp": 1700000011000 }
        ]
    }"#;

    write_json(&root, "session.json", json);

    let connector = CopilotConnector::new();
    let ctx = ScanContext::local_default(root, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title.as_deref(), Some("Explain lifetimes"));
    assert_eq!(convs[0].messages.len(), 2);
}

// ============================================================================
// Scan — conversations wrapper
// ============================================================================

#[test]
fn scan_parses_conversations_wrapper() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("copilot-chat");
    fs::create_dir_all(&root).unwrap();

    let json = r#"{
        "conversations": [
            { "id": "w1", "messages": [{"role": "user", "content": "Hello"}] },
            { "id": "w2", "messages": [{"role": "user", "content": "World"}] }
        ]
    }"#;

    write_json(&root, "all.json", json);

    let connector = CopilotConnector::new();
    let ctx = ScanContext::local_default(root, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 2);
}

// ============================================================================
// Edge cases
// ============================================================================

#[test]
fn scan_empty_dir_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("copilot-chat");
    fs::create_dir_all(&root).unwrap();

    let connector = CopilotConnector::new();
    let ctx = ScanContext::local_default(root, None);
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn scan_skips_invalid_json() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("copilot-chat");
    fs::create_dir_all(&root).unwrap();

    write_json(&root, "invalid.json", "not valid json {{{");

    let connector = CopilotConnector::new();
    let ctx = ScanContext::local_default(root, None);
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn scan_skips_empty_conversations() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("copilot-chat");
    fs::create_dir_all(&root).unwrap();

    let json = r#"[
        {"id": "empty", "turns": []},
        {"id": "valid", "turns": [{"request": {"message": "Hi"}, "response": {"message": "Hello"}}]}
    ]"#;

    write_json(&root, "mixed.json", json);

    let connector = CopilotConnector::new();
    let ctx = ScanContext::local_default(root, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some("valid"));
}

#[test]
fn scan_respects_since_ts() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("copilot-chat");
    fs::create_dir_all(&root).unwrap();

    write_json(
        &root,
        "old.json",
        r#"[{"id":"old","turns":[{"request":{"message":"old"},"response":{"message":"reply"}}]}]"#,
    );

    let connector = CopilotConnector::new();
    let far_future = chrono::Utc::now().timestamp_millis() + 86_400_000;
    let ctx = ScanContext::local_default(root, Some(far_future));
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn scan_with_scan_roots() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("fakehome");
    let copilot_dir = home.join(".config/Code/User/globalStorage/github.copilot-chat");
    fs::create_dir_all(&copilot_dir).unwrap();

    let json = r#"[{
        "id": "remote-001",
        "turns": [{"request": {"message": "test"}, "response": {"message": "reply"}}]
    }]"#;

    write_json(&copilot_dir, "conversations.json", json);

    let connector = CopilotConnector::new();
    let scan_root = ScanRoot::local(home);
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some("remote-001"));
}

#[test]
fn scan_parses_cli_jsonl_prompt_output_unicode_fixture() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join(".copilot/session-state");
    fs::create_dir_all(&root).unwrap();

    write_json(
        &root,
        "cli-session-001/events.jsonl",
        &load_fixture("cli_prompt_output_unicode.events.jsonl"),
    );

    let connector = CopilotConnector::new();
    let ctx = ScanContext::local_default(root, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.external_id.as_deref(), Some("cli-session-001"));
    assert_eq!(
        conv.workspace.as_deref(),
        Some(Path::new("/workspaces/demo-unicode"))
    );
    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].role, "user");
    assert_eq!(
        conv.messages[0].content,
        "How should Copilot handle cafe\u{301} ✅ and emoji?"
    );
    assert_eq!(conv.messages[1].role, "assistant");
    assert_eq!(
        conv.messages[1].content,
        "Keep Unicode intact: cafe\u{301} ✅ should round-trip."
    );
}

#[test]
fn scan_cli_jsonl_skips_truncated_line_and_keeps_valid_messages() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join(".copilot/session-state");
    fs::create_dir_all(&root).unwrap();

    write_json(
        &root,
        "cli-session-truncated/events.jsonl",
        &load_fixture("cli_truncated_resume.events.jsonl"),
    );

    let connector = CopilotConnector::new();
    let ctx = ScanContext::local_default(root, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.external_id.as_deref(), Some("cli-session-truncated"));
    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].role, "user");
    assert_eq!(conv.messages[1].role, "assistant");
    assert_eq!(conv.messages[1].content, "Recovered after truncation.");
    assert_eq!(conv.started_at, Some(1_700_002_000_000));
    assert_eq!(conv.ended_at, Some(1_700_002_002_000));
}

#[test]
fn scan_parses_cli_history_json_with_human_role_and_file_stem_id() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join(".copilot/history-session-state");
    fs::create_dir_all(&root).unwrap();

    write_json(
        &root,
        "legacy-human.json",
        &load_fixture("legacy_history_human.json"),
    );

    let connector = CopilotConnector::new();
    let ctx = ScanContext::local_default(root, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.external_id.as_deref(), Some("legacy-human"));
    assert_eq!(conv.title.as_deref(), Some("Summarize unicode Ω handling 🚀"));
    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].role, "user");
    assert_eq!(conv.messages[1].role, "assistant");
    assert_eq!(
        conv.messages[1].content,
        "Unicode stays normalized and searchable."
    );
}
