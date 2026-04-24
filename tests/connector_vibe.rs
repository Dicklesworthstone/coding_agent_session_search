use coding_agent_search::connectors::vibe::VibeConnector;
use coding_agent_search::connectors::{Connector, ScanContext};
use std::fs::{self, OpenOptions};
use std::path::Path;
use tempfile::TempDir;

// ============================================================================
// Helper
// ============================================================================

fn write_session(root: &Path, session_id: &str, lines: &[&str]) -> std::path::PathBuf {
    let dir = root.join(session_id);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("messages.jsonl");
    fs::write(&path, lines.join("\n")).unwrap();
    path
}

fn write_session_bytes(root: &Path, session_id: &str, bytes: &[u8]) -> std::path::PathBuf {
    let dir = root.join(session_id);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("messages.jsonl");
    fs::write(&path, bytes).unwrap();
    path
}

// ============================================================================
// Detection tests
// ============================================================================

#[test]
fn detect_does_not_panic() {
    let connector = VibeConnector::new();
    let result = connector.detect();
    // On most test systems Vibe won't be installed — just verify no panic.
    let _ = result.detected;
}

// ============================================================================
// Scan — happy path
// ============================================================================

#[test]
fn scan_parses_basic_conversation() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".vibe/logs/session");
    fs::create_dir_all(&sessions).unwrap();

    write_session(
        &sessions,
        "sess-abc",
        &[
            r#"{"role":"user","content":"Hello Vibe","timestamp":"2025-06-15T10:00:00.000Z"}"#,
            r#"{"role":"assistant","content":"Hi! How can I help?","timestamp":"2025-06-15T10:00:05.000Z"}"#,
        ],
    );

    let connector = VibeConnector::new();
    let ctx = ScanContext::local_default(sessions.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].agent_slug, "vibe");
    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].role, "user");
    assert_eq!(convs[0].messages[0].content, "Hello Vibe");
    assert_eq!(convs[0].messages[1].role, "assistant");
    assert!(convs[0].started_at.is_some());
    assert!(convs[0].ended_at.is_some());
    assert_eq!(convs[0].title, Some("Hello Vibe".to_string()));
}

#[test]
fn scan_multiple_sessions() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".vibe/logs/session");
    fs::create_dir_all(&sessions).unwrap();

    write_session(
        &sessions,
        "sess-1",
        &[r#"{"role":"user","content":"First session","timestamp":"2025-06-15T09:00:00.000Z"}"#],
    );
    write_session(
        &sessions,
        "sess-2",
        &[r#"{"role":"user","content":"Second session","timestamp":"2025-06-15T10:00:00.000Z"}"#],
    );

    let connector = VibeConnector::new();
    let ctx = ScanContext::local_default(sessions.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 2);
    let contents: Vec<&str> = convs
        .iter()
        .map(|c| c.messages[0].content.as_str())
        .collect();
    assert!(contents.contains(&"First session"));
    assert!(contents.contains(&"Second session"));
}

// ============================================================================
// Scan — edge cases
// ============================================================================

#[test]
fn scan_skips_invalid_json_and_empty_lines() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".vibe/logs/session");
    fs::create_dir_all(&sessions).unwrap();

    write_session(
        &sessions,
        "sess-bad",
        &[
            "",
            "not-json-at-all",
            r#"{"role":"user","content":"Survived","timestamp":"2025-06-15T10:00:00.000Z"}"#,
            r#"{"role":"assistant","content":"","timestamp":"2025-06-15T10:00:01.000Z"}"#,
        ],
    );

    let connector = VibeConnector::new();
    let ctx = ScanContext::local_default(sessions.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].messages.len(),
        1,
        "empty content should be skipped"
    );
    assert_eq!(convs[0].messages[0].content, "Survived");
}

#[test]
fn scan_empty_directory_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".vibe/logs/session");
    fs::create_dir_all(&sessions).unwrap();

    let connector = VibeConnector::new();
    let ctx = ScanContext::local_default(sessions.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs.is_empty());
}

#[test]
fn scan_empty_messages_file_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".vibe/logs/session");
    fs::create_dir_all(&sessions).unwrap();
    write_session_bytes(&sessions, "sess-zero-byte", b"");

    let connector = VibeConnector::new();
    let ctx = ScanContext::local_default(sessions.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs.is_empty());
}

#[test]
fn scan_truncated_tail_preserves_complete_messages() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".vibe/logs/session");
    fs::create_dir_all(&sessions).unwrap();

    write_session(
        &sessions,
        "sess-truncated-tail",
        &[
            r#"{"role":"user","content":"Complete before truncation","timestamp":"2025-06-15T10:00:00.000Z"}"#,
            r#"{"role":"assistant","content":"unterminated tail""#,
        ],
    );

    let connector = VibeConnector::new();
    let ctx = ScanContext::local_default(sessions.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Complete before truncation");
    assert_eq!(convs[0].started_at, Some(1_749_981_600_000));
    assert_eq!(convs[0].ended_at, Some(1_749_981_600_000));
}

#[test]
fn scan_non_utf8_messages_file_returns_empty_without_panic() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".vibe/logs/session");
    fs::create_dir_all(&sessions).unwrap();
    write_session_bytes(&sessions, "sess-non-utf8", &[0xff, 0xfe, b'\n', 0xfd, 0x80]);

    let connector = VibeConnector::new();
    let ctx = ScanContext::local_default(sessions.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs.is_empty());
}

#[test]
fn scan_oversized_sparse_messages_file_returns_empty_without_panic() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".vibe/logs/session");
    let session_dir = sessions.join("sess-huge");
    fs::create_dir_all(&session_dir).unwrap();
    let path = session_dir.join("messages.jsonl");
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.set_len(101 * 1024 * 1024).unwrap();
    drop(file);

    let connector = VibeConnector::new();
    let ctx = ScanContext::local_default(sessions.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs.is_empty());
}

#[test]
fn scan_ignores_non_messages_jsonl_files() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".vibe/logs/session");
    let session_dir = sessions.join("sess-wrong-file");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join("events.jsonl"),
        r#"{"role":"user","content":"wrong file name","timestamp":"2025-06-15T10:00:00.000Z"}"#,
    )
    .unwrap();

    let connector = VibeConnector::new();
    let ctx = ScanContext::local_default(sessions.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs.is_empty());
}

#[test]
fn scan_skips_session_with_only_empty_content() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".vibe/logs/session");
    fs::create_dir_all(&sessions).unwrap();

    write_session(
        &sessions,
        "sess-empty",
        &[
            r#"{"role":"user","content":"","timestamp":"2025-06-15T10:00:00.000Z"}"#,
            r#"{"role":"assistant","content":"   ","timestamp":"2025-06-15T10:00:01.000Z"}"#,
        ],
    );

    let connector = VibeConnector::new();
    let ctx = ScanContext::local_default(sessions.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(
        convs.is_empty(),
        "all-empty-content session should be skipped"
    );
}

#[test]
fn scan_preserves_message_ordering() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".vibe/logs/session");
    fs::create_dir_all(&sessions).unwrap();

    write_session(
        &sessions,
        "sess-order",
        &[
            r#"{"role":"user","content":"First","timestamp":"2025-06-15T10:00:00.000Z"}"#,
            r#"{"role":"assistant","content":"Second","timestamp":"2025-06-15T10:00:01.000Z"}"#,
            r#"{"role":"user","content":"Third","timestamp":"2025-06-15T10:00:02.000Z"}"#,
        ],
    );

    let connector = VibeConnector::new();
    let ctx = ScanContext::local_default(sessions.clone(), None);
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
fn scan_incremental_since_ts() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".vibe/logs/session");
    fs::create_dir_all(&sessions).unwrap();

    write_session(
        &sessions,
        "sess-ts",
        &[r#"{"role":"user","content":"Recent","timestamp":"2025-06-15T10:00:00.000Z"}"#],
    );

    let connector = VibeConnector::new();

    // Use a far-future since_ts to filter out everything (file mtime < since_ts).
    let far_future = chrono::Utc::now().timestamp_millis() + 86_400_000;
    let ctx = ScanContext::local_default(sessions.clone(), Some(far_future));
    let convs = connector.scan(&ctx).unwrap();

    assert!(
        convs.is_empty(),
        "far-future since_ts should filter out old files"
    );
}

// ============================================================================
// Alternative role/content extraction
// ============================================================================

#[test]
fn scan_extracts_role_from_speaker_or_nested_message() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".vibe/logs/session");
    fs::create_dir_all(&sessions).unwrap();

    write_session(
        &sessions,
        "sess-alt",
        &[
            r#"{"speaker":"user","text":"Via text field","timestamp":"2025-06-15T10:00:00.000Z"}"#,
            r#"{"message":{"role":"assistant","content":"Nested content"},"timestamp":"2025-06-15T10:00:01.000Z"}"#,
        ],
    );

    let connector = VibeConnector::new();
    let ctx = ScanContext::local_default(sessions.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].role, "user");
    assert_eq!(convs[0].messages[0].content, "Via text field");
    assert_eq!(convs[0].messages[1].role, "assistant");
    assert_eq!(convs[0].messages[1].content, "Nested content");
}
