//! Conformance harness for the Kimi connector via CASS's FAD re-export.

use coding_agent_search::connectors::kimi::KimiConnector;
use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn kimi_storage(tmp: &TempDir) -> PathBuf {
    let storage = tmp.path().join(".kimi/sessions");
    fs::create_dir_all(&storage).unwrap();
    storage
}

fn write_wire_file(
    storage: &Path,
    workspace_hash: &str,
    session_id: &str,
    bytes: &[u8],
) -> PathBuf {
    let session_dir = storage.join(workspace_hash).join(session_id);
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join("state.json"),
        r#"{"cwd":"/workspace/kimi-real-service"}"#,
    )
    .unwrap();
    let wire_path = session_dir.join("wire.jsonl");
    fs::write(&wire_path, bytes).unwrap();
    wire_path
}

fn write_wire_file_without_state(
    storage: &Path,
    workspace_hash: &str,
    session_id: &str,
    bytes: &[u8],
) -> PathBuf {
    let session_dir = storage.join(workspace_hash).join(session_id);
    fs::create_dir_all(&session_dir).unwrap();
    let wire_path = session_dir.join("wire.jsonl");
    fs::write(&wire_path, bytes).unwrap();
    wire_path
}

fn scan_storage(storage: &Path) -> Vec<coding_agent_search::connectors::NormalizedConversation> {
    let connector = KimiConnector::new();
    let ctx = ScanContext::with_roots(
        PathBuf::new(),
        vec![ScanRoot::local(storage.to_path_buf())],
        None,
    );
    connector.scan(&ctx).expect("kimi scan should not panic")
}

#[test]
fn kimi_happy_path_preserves_wire_session_fields() {
    let tmp = TempDir::new().unwrap();
    let storage = kimi_storage(&tmp);
    let wire = r#"{"type":"metadata","protocol_version":"1.3"}
{"timestamp":1772857971.158,"message":{"type":"TurnBegin","payload":{"role":"human","content":"Read the Kimi session format"}}}
{"timestamp":1772857972.250,"message":{"type":"ToolCall","payload":{"name":"Read","input":{"file_path":"/workspace/kimi-real-service/wire.jsonl"}}}}
{"timestamp":1772857973.500,"message":{"type":"ContentPart","payload":{"content":"Kimi uses wire.jsonl content parts."}}}
"#;
    let wire_path = write_wire_file(
        &storage,
        "workspace-hash",
        "session-kimi-1",
        wire.as_bytes(),
    );

    let convs = scan_storage(&storage);
    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.agent_slug, "kimi");
    assert_eq!(conv.external_id.as_deref(), Some("session-kimi-1"));
    assert_eq!(conv.title.as_deref(), Some("Read the Kimi session format"));
    assert_eq!(
        conv.workspace,
        Some(PathBuf::from("/workspace/kimi-real-service"))
    );
    assert_eq!(conv.source_path, wire_path);
    assert_eq!(conv.started_at, Some(1_772_857_971_158));
    assert_eq!(conv.ended_at, Some(1_772_857_973_500));
    assert_eq!(conv.metadata["sessionId"], "session-kimi-1");

    assert_eq!(conv.messages.len(), 3);
    assert_eq!(conv.messages[0].idx, 0);
    assert_eq!(conv.messages[0].role, "user");
    assert!(conv.messages[0].content.contains("Kimi session"));
    assert_eq!(conv.messages[1].idx, 1);
    assert_eq!(conv.messages[1].role, "assistant");
    assert_eq!(conv.messages[1].invocations.len(), 1);
    assert_eq!(conv.messages[1].invocations[0].name, "Read");
    assert!(conv.messages[1].content.contains("[Tool: Read"));
    assert_eq!(conv.messages[2].idx, 2);
    assert_eq!(conv.messages[2].role, "assistant");
    assert!(conv.messages[2].content.contains("wire.jsonl"));
}

#[test]
fn kimi_empty_wire_file_returns_empty_result() {
    let tmp = TempDir::new().unwrap();
    let storage = kimi_storage(&tmp);
    write_wire_file(&storage, "hash", "empty-session", b"");

    assert!(scan_storage(&storage).is_empty());
}

#[test]
fn kimi_truncated_tail_line_preserves_complete_messages() {
    let tmp = TempDir::new().unwrap();
    let storage = kimi_storage(&tmp);
    let wire = r#"{"timestamp":1772857971.0,"message":{"type":"TurnBegin","payload":{"role":"human","content":"complete request before truncation"}}}
{"timestamp":1772857972.0,"message":{"type":"ContentPart","payload":{"content":"unterminated tail"}
"#;
    write_wire_file(&storage, "hash", "truncated-session", wire.as_bytes());

    let convs = scan_storage(&storage);
    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.external_id.as_deref(), Some("truncated-session"));
    assert_eq!(conv.messages.len(), 1);
    assert!(conv.messages[0].content.contains("complete request"));
    assert_eq!(conv.started_at, Some(1_772_857_971_000));
    assert_eq!(conv.ended_at, Some(1_772_857_971_000));
}

#[test]
fn kimi_missing_state_json_keeps_session_without_workspace() {
    let tmp = TempDir::new().unwrap();
    let storage = kimi_storage(&tmp);
    let wire = r#"{"timestamp":1772857971.0,"message":{"type":"TurnBegin","payload":{"role":"human","content":"state file is absent"}}}
"#;
    let wire_path = write_wire_file_without_state(
        &storage,
        "workspace-hash",
        "missing-state-session",
        wire.as_bytes(),
    );

    let convs = scan_storage(&storage);
    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.external_id.as_deref(), Some("missing-state-session"));
    assert_eq!(conv.source_path, wire_path);
    assert_eq!(conv.workspace, None);
    assert_eq!(conv.messages.len(), 1);
}

#[test]
fn kimi_multiple_sessions_remain_isolated_and_sorted() {
    let tmp = TempDir::new().unwrap();
    let storage = kimi_storage(&tmp);
    write_wire_file(
        &storage,
        "hash-b",
        "session-b",
        br#"{"timestamp":1772857972.0,"message":{"type":"TurnBegin","payload":{"role":"human","content":"second session content"}}}
"#,
    );
    write_wire_file(
        &storage,
        "hash-a",
        "session-a",
        br#"{"timestamp":1772857971.0,"message":{"type":"TurnBegin","payload":{"role":"human","content":"first session content"}}}
"#,
    );

    let convs = scan_storage(&storage);
    assert_eq!(convs.len(), 2);
    assert_eq!(convs[0].external_id.as_deref(), Some("session-a"));
    assert_eq!(convs[1].external_id.as_deref(), Some("session-b"));
    assert!(convs[0].messages[0].content.contains("first session"));
    assert!(convs[1].messages[0].content.contains("second session"));
}

#[test]
fn kimi_ignores_non_wire_files_and_empty_session_dirs() {
    let tmp = TempDir::new().unwrap();
    let storage = kimi_storage(&tmp);
    let session_dir = storage.join("hash").join("empty-session");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join("context.jsonl"),
        br#"{"message":{"type":"TurnBegin","payload":{"content":"not the wire file"}}}"#,
    )
    .unwrap();
    fs::write(
        session_dir.join("state.json"),
        r#"{"cwd":"/workspace/ignored"}"#,
    )
    .unwrap();

    assert!(scan_storage(&storage).is_empty());
}

#[test]
fn kimi_malformed_mid_file_is_skipped_without_panic() {
    let tmp = TempDir::new().unwrap();
    let storage = kimi_storage(&tmp);
    let wire = r#"{"timestamp":1772857971.0,"message":{"type":"TurnBegin","payload":{"role":"human","content":"valid before corruption"}}}
{ this is not valid json
{"timestamp":1772857972.0,"message":{"type":"ContentPart","payload":{"content":"valid after corruption"}}}
"#;
    write_wire_file(&storage, "hash", "malformed-session", wire.as_bytes());

    let convs = scan_storage(&storage);
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
    assert!(convs[0].messages[0].content.contains("valid before"));
    assert!(convs[0].messages[1].content.contains("valid after"));
}

#[test]
fn kimi_non_utf8_bytes_return_empty_result_without_panic() {
    let tmp = TempDir::new().unwrap();
    let storage = kimi_storage(&tmp);
    write_wire_file(
        &storage,
        "hash",
        "non-utf8-session",
        &[0xff, 0xfe, 0xfd, b'\n', 0x80],
    );

    assert!(scan_storage(&storage).is_empty());
}

#[test]
fn kimi_oversized_sparse_wire_file_returns_empty_result_without_panic() {
    let tmp = TempDir::new().unwrap();
    let storage = kimi_storage(&tmp);
    let session_dir = storage.join("hash").join("huge-session");
    fs::create_dir_all(&session_dir).unwrap();
    let wire_path = session_dir.join("wire.jsonl");
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&wire_path)
        .unwrap();
    file.set_len(101 * 1024 * 1024).unwrap();
    drop(file);

    assert!(scan_storage(&storage).is_empty());
}
