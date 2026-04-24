//! Conformance harness for the Qwen connector via CASS's FAD re-export.

use coding_agent_search::connectors::qwen::QwenConnector;
use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn qwen_storage(tmp: &TempDir) -> PathBuf {
    let storage = tmp.path().join(".qwen/tmp");
    fs::create_dir_all(&storage).unwrap();
    storage
}

fn write_session_file(storage: &Path, project_hash: &str, filename: &str, bytes: &[u8]) -> PathBuf {
    let project_dir = storage.join(project_hash);
    let chats_dir = project_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();
    fs::write(
        project_dir.join("config.json"),
        r#"{"workspace":"/workspace/qwen-real-service"}"#,
    )
    .unwrap();
    let session_path = chats_dir.join(filename);
    fs::write(&session_path, bytes).unwrap();
    session_path
}

fn write_session_file_without_config(
    storage: &Path,
    project_hash: &str,
    filename: &str,
    bytes: &[u8],
) -> PathBuf {
    let chats_dir = storage.join(project_hash).join("chats");
    fs::create_dir_all(&chats_dir).unwrap();
    let session_path = chats_dir.join(filename);
    fs::write(&session_path, bytes).unwrap();
    session_path
}

fn scan_storage(storage: &Path) -> Vec<coding_agent_search::connectors::NormalizedConversation> {
    let connector = QwenConnector::new();
    let ctx = ScanContext::with_roots(
        PathBuf::new(),
        vec![ScanRoot::local(storage.to_path_buf())],
        None,
    );
    connector.scan(&ctx).expect("qwen scan should not panic")
}

#[test]
fn qwen_happy_path_preserves_session_json_fields() {
    let tmp = TempDir::new().unwrap();
    let storage = qwen_storage(&tmp);
    let session_json = r#"{
        "sessionId": "qwen-session-1",
        "projectHash": "project-hash-1",
        "startTime": "2025-11-08T23:19:10.138Z",
        "lastUpdated": "2025-11-08T23:19:13.706Z",
        "messages": [
            {
                "id": "msg-001",
                "timestamp": "2025-11-08T23:19:10.138Z",
                "type": "user",
                "content": "Explain the Qwen session format"
            },
            {
                "id": "msg-002",
                "timestamp": "2025-11-08T23:19:13.706Z",
                "type": "qwen",
                "content": [{"type":"text","text":"Qwen stores complete JSON sessions."}]
            }
        ]
    }"#;
    let session_path = write_session_file(
        &storage,
        "project-hash-1",
        "session-1731107950138-qwen.json",
        session_json.as_bytes(),
    );

    let convs = scan_storage(&storage);
    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.agent_slug, "qwen");
    assert_eq!(conv.external_id.as_deref(), Some("qwen-session-1"));
    assert_eq!(
        conv.title.as_deref(),
        Some("Explain the Qwen session format")
    );
    assert_eq!(
        conv.workspace,
        Some(PathBuf::from("/workspace/qwen-real-service"))
    );
    assert_eq!(conv.source_path, session_path);
    assert_eq!(conv.started_at, Some(1_762_643_950_138));
    assert_eq!(conv.ended_at, Some(1_762_643_953_706));
    assert_eq!(conv.metadata["sessionId"], "qwen-session-1");
    assert_eq!(conv.metadata["projectHash"], "project-hash-1");

    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].idx, 0);
    assert_eq!(conv.messages[0].role, "user");
    assert!(conv.messages[0].content.contains("Qwen session"));
    assert_eq!(conv.messages[1].idx, 1);
    assert_eq!(conv.messages[1].role, "assistant");
    assert!(conv.messages[1].content.contains("complete JSON"));
}

#[test]
fn qwen_empty_session_file_returns_empty_result() {
    let tmp = TempDir::new().unwrap();
    let storage = qwen_storage(&tmp);
    write_session_file(&storage, "hash", "session-empty.json", b"");

    assert!(scan_storage(&storage).is_empty());
}

#[test]
fn qwen_malformed_json_returns_empty_result_without_panic() {
    let tmp = TempDir::new().unwrap();
    let storage = qwen_storage(&tmp);
    write_session_file(
        &storage,
        "hash",
        "session-malformed.json",
        br#"{"sessionId":"bad","messages":[{"type":"user","content":"unterminated"}"#,
    );

    assert!(scan_storage(&storage).is_empty());
}

#[test]
fn qwen_truncated_session_json_returns_empty_result_without_panic() {
    let tmp = TempDir::new().unwrap();
    let storage = qwen_storage(&tmp);
    write_session_file(
        &storage,
        "hash",
        "session-truncated.json",
        br#"{"sessionId":"truncated","projectHash":"hash","messages":[{"type":"user","content":"complete until tail"}"#,
    );

    assert!(scan_storage(&storage).is_empty());
}

#[test]
fn qwen_missing_config_json_keeps_session_without_workspace() {
    let tmp = TempDir::new().unwrap();
    let storage = qwen_storage(&tmp);
    let session_json = r#"{
        "sessionId": "qwen-no-config",
        "projectHash": "project-without-config",
        "startTime": "2025-11-08T23:19:10.138Z",
        "lastUpdated": "2025-11-08T23:19:10.138Z",
        "messages": [
            {
                "id": "msg-001",
                "timestamp": "2025-11-08T23:19:10.138Z",
                "type": "user",
                "content": "Config file is absent"
            }
        ]
    }"#;
    let session_path = write_session_file_without_config(
        &storage,
        "project-without-config",
        "session-1731107950138-no-config.json",
        session_json.as_bytes(),
    );

    let convs = scan_storage(&storage);
    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.external_id.as_deref(), Some("qwen-no-config"));
    assert_eq!(conv.source_path, session_path);
    assert_eq!(conv.workspace, None);
}

#[test]
fn qwen_multiple_projects_remain_isolated_and_sorted() {
    let tmp = TempDir::new().unwrap();
    let storage = qwen_storage(&tmp);
    let first = r#"{
        "sessionId": "qwen-session-a",
        "projectHash": "project-a",
        "messages": [{"type": "user", "content": "first project content"}]
    }"#;
    let second = r#"{
        "sessionId": "qwen-session-b",
        "projectHash": "project-b",
        "messages": [{"type": "user", "content": "second project content"}]
    }"#;
    write_session_file(
        &storage,
        "project-b",
        "session-1731107950139-b.json",
        second.as_bytes(),
    );
    write_session_file(
        &storage,
        "project-a",
        "session-1731107950138-a.json",
        first.as_bytes(),
    );

    let convs = scan_storage(&storage);
    assert_eq!(convs.len(), 2);
    assert_eq!(convs[0].external_id.as_deref(), Some("qwen-session-a"));
    assert_eq!(convs[1].external_id.as_deref(), Some("qwen-session-b"));
    assert_eq!(convs[0].metadata["projectHash"], "project-a");
    assert_eq!(convs[1].metadata["projectHash"], "project-b");
    assert!(convs[0].messages[0].content.contains("first project"));
    assert!(convs[1].messages[0].content.contains("second project"));
}

#[test]
fn qwen_ignores_non_session_json_files_and_empty_project_dirs() {
    let tmp = TempDir::new().unwrap();
    let storage = qwen_storage(&tmp);
    let chats_dir = storage.join("hash").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();
    fs::write(
        chats_dir.join("conversation.json"),
        br#"{"messages":[{"type":"user","content":"wrong file name"}]}"#,
    )
    .unwrap();
    fs::write(chats_dir.join("session-not-json.txt"), b"ignored").unwrap();

    assert!(scan_storage(&storage).is_empty());
}

#[test]
fn qwen_non_utf8_bytes_return_empty_result_without_panic() {
    let tmp = TempDir::new().unwrap();
    let storage = qwen_storage(&tmp);
    write_session_file(
        &storage,
        "hash",
        "session-non-utf8.json",
        &[0xff, 0xfe, 0xfd, 0x80],
    );

    assert!(scan_storage(&storage).is_empty());
}

#[test]
fn qwen_oversized_sparse_session_returns_empty_result_without_panic() {
    let tmp = TempDir::new().unwrap();
    let storage = qwen_storage(&tmp);
    let chats_dir = storage.join("hash").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();
    let session_path = chats_dir.join("session-huge.json");
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&session_path)
        .unwrap();
    file.set_len(101 * 1024 * 1024).unwrap();
    drop(file);

    assert!(scan_storage(&storage).is_empty());
}
