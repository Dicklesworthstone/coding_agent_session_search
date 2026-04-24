use coding_agent_search::connectors::openclaw::OpenClawConnector;
use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot};
use std::fs::{self, OpenOptions};
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

fn write_jsonl_bytes(dir: &Path, rel_path: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join(rel_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, bytes).unwrap();
    path
}

// ============================================================================
// Detection tests
// ============================================================================

#[test]
fn detect_does_not_panic() {
    let connector = OpenClawConnector::new();
    let result = connector.detect();
    let _ = result.detected;
}

// ============================================================================
// Scan — happy path
// ============================================================================

#[test]
fn scan_parses_basic_session() {
    let tmp = TempDir::new().unwrap();
    // Matches openclaw storage: agents/{agent}/sessions/*.jsonl
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();

    write_jsonl(
        &sessions,
        "sess-001.jsonl",
        &[
            r#"{"type":"session","cwd":"/home/user/project","timestamp":"2025-06-15T10:00:00.000Z"}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"Explain lifetimes"}]}}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Lifetimes express scope validity."}]}}"#,
        ],
    );

    let connector = OpenClawConnector::new();
    // Use scan_roots pointing to the temp home so openclaw finds .openclaw/agents/...
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].agent_slug, "openclaw");
    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].role, "user");
    assert!(convs[0].messages[0].content.contains("lifetimes"));
    assert_eq!(convs[0].messages[1].role, "assistant");
    assert!(convs[0].started_at.is_some());
    assert!(convs[0].ended_at.is_some());
}

#[test]
fn scan_multiple_sessions() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();

    write_jsonl(
        &sessions,
        "a.jsonl",
        &[
            r#"{"type":"message","timestamp":"2025-06-15T09:00:00.000Z","message":{"role":"user","content":"Session A"}}"#,
        ],
    );
    write_jsonl(
        &sessions,
        "b.jsonl",
        &[
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"Session B"}}"#,
        ],
    );

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 2);
    let contents: Vec<&str> = convs
        .iter()
        .map(|c| c.messages[0].content.as_str())
        .collect();
    assert!(contents.contains(&"Session A"));
    assert!(contents.contains(&"Session B"));
}

// ============================================================================
// Multi-agent support
// ============================================================================

#[test]
fn scan_discovers_multiple_agents() {
    let tmp = TempDir::new().unwrap();
    let agents_root = tmp.path().join(".openclaw/agents");

    // Agent: openclaw
    let oc_sessions = agents_root.join("openclaw/sessions");
    fs::create_dir_all(&oc_sessions).unwrap();
    write_jsonl(
        &oc_sessions,
        "s1.jsonl",
        &[
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"From default agent"}}"#,
        ],
    );

    // Agent: custombot
    let custom_sessions = agents_root.join("custombot/sessions");
    fs::create_dir_all(&custom_sessions).unwrap();
    write_jsonl(
        &custom_sessions,
        "s2.jsonl",
        &[
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"From custom agent"}}"#,
        ],
    );

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 2);
    let slugs: Vec<&str> = convs.iter().map(|c| c.agent_slug.as_str()).collect();
    assert!(slugs.contains(&"openclaw"));
    assert!(slugs.contains(&"openclaw/custombot"));
}

// ============================================================================
// Content flattening
// ============================================================================

#[test]
fn scan_flattens_text_and_tool_content_blocks() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();

    write_jsonl(
        &sessions,
        "mixed-content.jsonl",
        &[
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Let me check."},{"type":"toolCall","name":"read_file"},{"type":"text","text":"Done."}]}}"#,
        ],
    );

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    let content = &convs[0].messages[0].content;
    assert!(content.contains("Let me check."));
    assert!(content.contains("[tool: read_file]"));
    assert!(content.contains("Done."));
}

#[test]
fn scan_handles_string_content() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();

    // Content as a plain string rather than array
    write_jsonl(
        &sessions,
        "string-content.jsonl",
        &[
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"Plain string content"}}"#,
        ],
    );

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Plain string content");
}

// ============================================================================
// Edge cases
// ============================================================================

#[test]
fn scan_skips_invalid_json_lines() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();

    write_jsonl(
        &sessions,
        "bad.jsonl",
        &[
            "",
            "not-json",
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"Valid"}}"#,
        ],
    );

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Valid");
}

#[test]
fn scan_empty_jsonl_file_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();
    write_jsonl_bytes(&sessions, "zero-byte.jsonl", b"");

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs.is_empty());
}

#[test]
fn scan_truncated_tail_preserves_complete_messages() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();

    write_jsonl(
        &sessions,
        "truncated-tail.jsonl",
        &[
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"Complete before truncation"}}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:01.000Z","message":{"role":"assistant","content":"unterminated tail"}"#,
        ],
    );

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Complete before truncation");
    assert_eq!(convs[0].started_at, Some(1_749_981_600_000));
    assert_eq!(convs[0].ended_at, Some(1_749_981_600_000));
}

#[test]
fn scan_non_utf8_jsonl_returns_empty_without_panic() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();
    write_jsonl_bytes(
        &sessions,
        "non-utf8.jsonl",
        &[0xff, 0xfe, b'\n', 0xfd, 0x80],
    );

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs.is_empty());
}

#[test]
fn scan_oversized_sparse_jsonl_returns_empty_without_panic() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("huge.jsonl");
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.set_len(101 * 1024 * 1024).unwrap();
    drop(file);

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs.is_empty());
}

#[test]
fn scan_ignores_non_jsonl_session_files() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("transcript.txt"),
        r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"wrong extension"}}"#,
    )
    .unwrap();

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs.is_empty());
}

#[test]
fn scan_skips_empty_content() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();

    write_jsonl(
        &sessions,
        "empties.jsonl",
        &[
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":""}}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:01.000Z","message":{"role":"user","content":"Real"}}"#,
        ],
    );

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Real");
}

#[test]
fn scan_empty_directory_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn scan_skips_non_message_types() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();

    write_jsonl(
        &sessions,
        "mixed-types.jsonl",
        &[
            r#"{"type":"session","cwd":"/tmp","timestamp":"2025-06-15T10:00:00.000Z"}"#,
            r#"{"type":"model_change","model":"gpt-4o"}"#,
            r#"{"type":"thinking_level_change","level":"high"}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:01.000Z","message":{"role":"user","content":"Only message"}}"#,
        ],
    );

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Only message");
}

// ============================================================================
// Incremental scanning (since_ts)
// ============================================================================

#[test]
fn scan_respects_since_ts() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();

    write_jsonl(
        &sessions,
        "old.jsonl",
        &[
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"Old"}}"#,
        ],
    );

    let connector = OpenClawConnector::new();
    let far_future = chrono::Utc::now().timestamp_millis() + 86_400_000;
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], Some(far_future));
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

// ============================================================================
// Title and workspace extraction
// ============================================================================

#[test]
fn scan_extracts_workspace_from_session_line() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();

    write_jsonl(
        &sessions,
        "ws.jsonl",
        &[
            r#"{"type":"session","cwd":"/home/dev/myrepo","timestamp":"2025-06-15T10:00:00.000Z"}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:01.000Z","message":{"role":"user","content":"Hello"}}"#,
        ],
    );

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0]
            .workspace
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        Some("/home/dev/myrepo".to_string())
    );
}

#[test]
fn scan_derives_title_from_first_user_message() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();

    write_jsonl(
        &sessions,
        "title.jsonl",
        &[
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"How do I sort a Vec?"}}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:01.000Z","message":{"role":"assistant","content":"Use .sort()"}}"#,
        ],
    );

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].title.as_deref(), Some("How do I sort a Vec?"));
}

// ============================================================================
// Message ordering
// ============================================================================

#[test]
fn scan_preserves_message_ordering() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions).unwrap();

    write_jsonl(
        &sessions,
        "order.jsonl",
        &[
            r#"{"type":"message","timestamp":"2025-06-15T10:00:00.000Z","message":{"role":"user","content":"First"}}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:01.000Z","message":{"role":"assistant","content":"Second"}}"#,
            r#"{"type":"message","timestamp":"2025-06-15T10:00:02.000Z","message":{"role":"user","content":"Third"}}"#,
        ],
    );

    let connector = OpenClawConnector::new();
    let scan_root = ScanRoot::local(tmp.path().to_path_buf());
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![scan_root], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages[0].idx, 0);
    assert_eq!(convs[0].messages[0].content, "First");
    assert_eq!(convs[0].messages[1].idx, 1);
    assert_eq!(convs[0].messages[1].content, "Second");
    assert_eq!(convs[0].messages[2].idx, 2);
    assert_eq!(convs[0].messages[2].content, "Third");
}
