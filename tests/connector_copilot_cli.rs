//! Integration-style tests for the Copilot CLI connector via CASS's
//! re-export of franken-agent-detection.
//!
//! Regression coverage for cass#187: Copilot CLI Chronicle session events
//! (`~/.copilot/session-state/<uuid>/events.jsonl`) nest message payloads under
//! a `data` object. Before the fix, these events yielded zero conversations.

use coding_agent_search::connectors::copilot_cli::CopilotCliConnector;
use coding_agent_search::connectors::{Connector, NormalizedConversation, ScanContext};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn write_file(dir: &Path, filename: &str, content: &str) -> PathBuf {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    path
}

fn scan_session_state(root: PathBuf) -> Vec<NormalizedConversation> {
    let connector = CopilotCliConnector::new();
    let ctx = ScanContext::local_default(root, None);
    connector.scan(&ctx).unwrap()
}

/// Canonical reproduction of the shape reported in cass#187:
/// Chronicle events.jsonl with `data.content` and ISO8601 timestamps.
#[test]
fn scan_parses_chronicle_nested_data_content() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join(".copilot/session-state/chronicle-187");
    fs::create_dir_all(&session_dir).unwrap();

    let events = r#"{"type":"session.start","data":{"sessionId":"chronicle-187","cwd":"/home/cc314/demo"},"timestamp":"2026-03-01T10:00:00.000Z"}
{"type":"user.message","data":{"content":"explain this repo"},"timestamp":"2026-03-01T10:00:01.000Z"}
{"type":"assistant.message","data":{"content":"Rust project.","toolRequests":[]},"timestamp":"2026-03-01T10:00:02.000Z"}
{"type":"user.message","data":{"content":"show me the exports"},"timestamp":"2026-03-01T10:00:03.000Z"}
{"type":"assistant.message","data":{"content":"Factory registry.","toolRequests":[{"name":"Read","input":{"path":"lib.rs"}}]},"timestamp":"2026-03-01T10:00:04.000Z"}
"#;

    write_file(&session_dir, "events.jsonl", events);

    let root = tmp.path().join(".copilot/session-state");
    let convs = scan_session_state(root);

    assert_eq!(
        convs.len(),
        1,
        "expected one conversation from chronicle events.jsonl"
    );
    let conv = &convs[0];
    assert_eq!(conv.agent_slug, "copilot_cli");
    assert_eq!(
        conv.workspace,
        Some(PathBuf::from("/home/cc314/demo")),
        "workspace must be extracted from nested data.cwd"
    );
    assert_eq!(conv.messages.len(), 4);
    assert_eq!(conv.messages[0].role, "user");
    assert!(conv.messages[0].content.contains("explain this repo"));
    assert_eq!(conv.messages[1].role, "assistant");
    assert!(conv.messages[1].content.contains("Rust project"));
    assert_eq!(conv.messages[2].role, "user");
    assert!(conv.messages[2].content.contains("exports"));
    assert_eq!(conv.messages[3].role, "assistant");
    assert!(conv.messages[3].content.contains("Factory registry"));
    // Bead 7k7pl: pin timestamp presence + ordering + per-message
    // containment in one block. Each of the 4 fixture messages
    // (user/assistant alternation above) should carry a timestamp
    // that falls inside [started_at, ended_at]; a parser regression
    // that assigned epoch-0 or clock-now() fallbacks would slip past
    // bare presence checks but fires against this window assertion.
    let started = conv
        .started_at
        .expect("conversation started_at must be parsed from ISO8601");
    let ended = conv
        .ended_at
        .expect("conversation ended_at must be parsed from ISO8601");
    assert!(
        started <= ended,
        "started_at ({started}) must precede or equal ended_at ({ended})"
    );
    for (idx, msg) in conv.messages.iter().enumerate() {
        if let Some(created) = msg.created_at {
            assert!(
                (started..=ended).contains(&created),
                "copilot-cli message #{idx} created_at ({created}) must fall within \
                 [started_at={started}, ended_at={ended}]"
            );
        }
    }
}

/// When the Chronicle event log contains no `sessionId` anywhere, we must
/// still assign a stable external id by falling back to the parent
/// directory UUID.
#[test]
fn scan_chronicle_uses_directory_uuid_for_session_id() {
    let tmp = TempDir::new().unwrap();
    let uuid = "4c5e9a9e-1234-4abc-9def-000000000042";
    let session_dir = tmp.path().join(format!(".copilot/session-state/{uuid}"));
    fs::create_dir_all(&session_dir).unwrap();

    let events = r#"{"type":"user.message","data":{"content":"hi"},"timestamp":"2026-03-01T10:00:00.000Z"}
{"type":"assistant.message","data":{"content":"hello"},"timestamp":"2026-03-01T10:00:01.000Z"}
"#;
    write_file(&session_dir, "events.jsonl", events);

    let root = tmp.path().join(".copilot/session-state");
    let convs = scan_session_state(root);

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some(uuid));
    assert_eq!(convs[0].messages.len(), 2);
}

/// Legacy top-level `content` events (pre-Chronicle) must keep working
/// alongside the new nested format — a mixed JSONL file should still index.
#[test]
fn scan_handles_mixed_legacy_and_chronicle_events() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join(".copilot/session-state/mixed-sess");
    fs::create_dir_all(&session_dir).unwrap();

    let events = r#"{"type":"user.message","content":"legacy top-level","timestamp":1700000001000}
{"type":"assistant.message","data":{"content":"nested reply"},"timestamp":"2026-03-01T10:00:02.000Z"}
"#;
    write_file(&session_dir, "events.jsonl", events);

    let root = tmp.path().join(".copilot/session-state");
    let convs = scan_session_state(root);

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
    assert!(convs[0].messages[0].content.contains("legacy top-level"));
    assert!(convs[0].messages[1].content.contains("nested reply"));
}

#[test]
fn scan_empty_session_state_returns_no_conversations() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join(".copilot/session-state");
    fs::create_dir_all(root.join("empty-session")).unwrap();
    write_file(
        &root.join("only-control-events"),
        "events.jsonl",
        r#"{"type":"session.start","timestamp":"2026-03-01T10:00:00.000Z"}
{"type":"session.end","timestamp":"2026-03-01T10:00:01.000Z"}
"#,
    );

    let convs = scan_session_state(root);

    assert!(
        convs.is_empty(),
        "empty directories and control-only logs must not synthesize conversations"
    );
}

#[test]
fn scan_skips_malformed_lines_without_losing_valid_messages() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join(".copilot/session-state/malformed-lines");
    fs::create_dir_all(&session_dir).unwrap();

    let events = r#"not valid json
{"type":"user.message","data":{"content":"valid before corrupt line"},"timestamp":"2026-03-01T10:00:00.000Z"}
{"type":"assistant.message","data":{"content":
{"type":"assistant.message","data":{"content":"valid after corrupt line"},"timestamp":"2026-03-01T10:00:02.000Z"}
"#;
    write_file(&session_dir, "events.jsonl", events);

    let root = tmp.path().join(".copilot/session-state");
    let convs = scan_session_state(root);

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some("malformed-lines"));
    assert_eq!(convs[0].messages.len(), 2);
    assert!(
        convs[0].messages[0]
            .content
            .contains("valid before corrupt line")
    );
    assert!(
        convs[0].messages[1]
            .content
            .contains("valid after corrupt line")
    );
}

#[test]
fn scan_truncated_session_without_complete_json_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join(".copilot/session-state/truncated-only");
    fs::create_dir_all(&session_dir).unwrap();
    write_file(
        &session_dir,
        "events.jsonl",
        r#"{"type":"user.message","data":{"content":"unterminated message"},"#,
    );

    let root = tmp.path().join(".copilot/session-state");
    let convs = scan_session_state(root);

    assert!(
        convs.is_empty(),
        "fully truncated logs must be ignored rather than producing partial phantom sessions"
    );
}

#[test]
fn scan_large_chronicle_session_preserves_all_messages_in_order() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join(".copilot/session-state/large-session");
    fs::create_dir_all(&session_dir).unwrap();

    let mut events = String::new();
    events.push_str(
        r#"{"type":"session.start","data":{"sessionId":"large-session","cwd":"/workspace/large"},"timestamp":"2026-03-01T10:00:00.000Z"}
"#,
    );
    for idx in 0..512 {
        let role = if idx % 2 == 0 {
            "user.message"
        } else {
            "assistant.message"
        };
        events.push_str(&format!(
            r#"{{"type":"{role}","data":{{"content":"message {idx:03}"}},"timestamp":"2026-03-01T10:00:01.000Z"}}
"#
        ));
    }
    write_file(&session_dir, "events.jsonl", &events);

    let root = tmp.path().join(".copilot/session-state");
    let convs = scan_session_state(root);

    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.external_id.as_deref(), Some("large-session"));
    assert_eq!(conv.workspace, Some(PathBuf::from("/workspace/large")));
    assert_eq!(conv.messages.len(), 512);
    assert_eq!(conv.messages[0].role, "user");
    assert!(conv.messages[0].content.contains("message 000"));
    assert_eq!(conv.messages[511].role, "assistant");
    assert!(conv.messages[511].content.contains("message 511"));
    for (idx, message) in conv.messages.iter().enumerate() {
        assert_eq!(message.idx, i64::try_from(idx).unwrap());
        assert!(
            message.content.contains(&format!("message {idx:03}")),
            "large session message order changed at index {idx}"
        );
    }
}
