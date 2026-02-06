use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

use coding_agent_search::connectors::{Connector, ScanContext, codex::CodexConnector};
use serial_test::serial;

#[test]
#[serial]
fn codex_connector_reads_modern_envelope_jsonl() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/21");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-1.jsonl");

    // Modern envelope format with {type, timestamp, payload}
    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test/workspace","cli_version":"0.42.0"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"write a hello program"}]}}
{"timestamp":"2025-09-30T15:42:43.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"here is code"}]}}
"#;
    fs::write(&file, sample).unwrap();

    // Safe in test scope: we control process env.
    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];
    assert_eq!(c.agent_slug, "codex");
    assert_eq!(c.messages.len(), 2);
    assert!(c.title.as_ref().unwrap().contains("write a hello program"));
    // Verify workspace was extracted from session_meta
    assert_eq!(c.workspace, Some(PathBuf::from("/test/workspace")));
    // Verify timestamps were parsed from ISO-8601
    assert!(c.started_at.is_some());
    assert!(c.ended_at.is_some());
}

#[test]
#[serial]
fn codex_connector_includes_agent_reasoning() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/22");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-reasoning.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"solve this problem"}]}}
{"timestamp":"2025-09-30T15:42:40.000Z","type":"event_msg","payload":{"type":"agent_reasoning","text":"Let me think about this carefully..."}}
{"timestamp":"2025-09-30T15:42:43.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"here is solution"}]}}
{"timestamp":"2025-09-30T15:42:45.000Z","type":"event_msg","payload":{"type":"token_count","input_tokens":100,"output_tokens":200}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];

    // Should have 3 messages: user, reasoning, assistant
    // (token_count does not create a synthetic message)
    assert_eq!(c.messages.len(), 3);

    // Check reasoning is included with correct author tag
    let reasoning = c
        .messages
        .iter()
        .find(|m| m.author.as_deref() == Some("reasoning"));
    assert!(reasoning.is_some());
    assert!(
        reasoning
            .unwrap()
            .content
            .contains("think about this carefully")
    );

    let assistant = c.messages.iter().find(|m| {
        m.role == "assistant" && m.author.is_none() && m.content.contains("here is solution")
    });
    assert!(assistant.is_some());
    let assistant = assistant.unwrap();
    assert_eq!(
        assistant
            .extra
            .pointer("/cass/token_usage/input_tokens")
            .and_then(|v| v.as_i64()),
        Some(100)
    );
    assert_eq!(
        assistant
            .extra
            .pointer("/cass/token_usage/output_tokens")
            .and_then(|v| v.as_i64()),
        Some(200)
    );
}

#[test]
#[serial]
fn codex_connector_ignores_unmatched_token_count() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/23");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-filter.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}
{"timestamp":"2025-09-30T15:42:37.000Z","type":"event_msg","payload":{"type":"token_count","input_tokens":10,"output_tokens":20}}
{"timestamp":"2025-09-30T15:42:38.000Z","type":"turn_context","payload":{"turn":1}}
{"timestamp":"2025-09-30T15:42:39.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"world"}]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];

    // Should only have 2 messages (user, assistant).
    // token_count and turn_context do not create searchable messages.
    assert_eq!(c.messages.len(), 2);

    for msg in &c.messages {
        assert!(!msg.content.contains("token_count"));
        assert!(!msg.content.contains("turn_context"));
        assert!(!msg.content.trim().is_empty());
    }

    // token_count occurs before the first assistant turn and must not attach forward.
    let assistant = c.messages.iter().find(|m| m.role == "assistant").unwrap();
    assert!(
        assistant.extra.pointer("/cass/token_usage").is_none(),
        "unmatched token_count should be ignored"
    );
}

/// Test that since_ts uses FILE-LEVEL filtering, not message-level.
///
/// NOTE: We intentionally removed message-level timestamp filtering because
/// it caused data loss during incremental re-indexing. When a file is modified,
/// ALL messages in that file are ingested, regardless of individual timestamps.
/// The since_ts is ONLY used to decide whether to process the file at all
/// (based on file mtime vs since_ts).
#[test]
#[serial]
fn codex_connector_respects_since_ts_at_file_level_only() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/24");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-since.jsonl");

    // Two messages with different timestamps - both should be included
    // since since_ts filtering happens at the FILE level, not message level.
    let sample = r#"{"timestamp":"2025-09-30T15:42:34.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"old msg"}]}}
{"timestamp":1700000100000,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"new msg"}]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    // since_ts does NOT filter individual messages anymore - only whole files
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: Some(1_700_000_000_000),
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];

    // BOTH messages should be present - we don't filter by message timestamp
    assert_eq!(
        c.messages.len(),
        2,
        "file-level filtering means all messages in a processed file are included"
    );
    // Messages should have correct roles
    assert_eq!(c.messages[0].role, "user");
    assert!(c.messages[0].content.contains("old msg"));
    assert_eq!(c.messages[1].role, "assistant");
    assert!(c.messages[1].content.contains("new msg"));
}

/// Test legacy .json format parsing
#[test]
#[serial]
fn codex_connector_reads_legacy_json_format() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/25");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-legacy.json");

    // Legacy format: single JSON object with session and items
    let sample = r#"{
        "session": {
            "id": "legacy-session",
            "cwd": "/legacy/workspace"
        },
        "items": [
            {
                "role": "user",
                "timestamp": "2025-09-30T15:42:36.190Z",
                "content": "legacy user message"
            },
            {
                "role": "assistant",
                "timestamp": "2025-09-30T15:42:43.000Z",
                "content": "legacy assistant response"
            }
        ]
    }"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.agent_slug, "codex");
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.workspace, Some(PathBuf::from("/legacy/workspace")));

    // Verify metadata indicates legacy format
    assert_eq!(
        c.metadata.get("source").and_then(|v| v.as_str()),
        Some("rollout_json")
    );

    // Check messages
    assert_eq!(c.messages[0].role, "user");
    assert!(c.messages[0].content.contains("legacy user message"));
    assert_eq!(c.messages[1].role, "assistant");
}

/// Test detection with existing sessions directory
#[test]
#[serial]
fn codex_detect_with_sessions_dir() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let result = connector.detect();
    assert!(result.detected);
    assert!(!result.evidence.is_empty());
}

/// Test detection without sessions directory
#[test]
#[serial]
fn codex_detect_without_sessions_dir() {
    let dir = TempDir::new().unwrap();
    // Don't create sessions directory

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let result = connector.detect();
    assert!(!result.detected);
}

/// Test `user_message` event type
#[test]
#[serial]
fn codex_connector_handles_user_message_event() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/26");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-user-event.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"event_msg","payload":{"type":"user_message","message":"user event message"}}
{"timestamp":"2025-09-30T15:42:43.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"assistant reply"}]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.messages.len(), 2);

    // First message should be the user event
    assert_eq!(c.messages[0].role, "user");
    assert!(c.messages[0].content.contains("user event message"));
}

/// Test malformed JSONL lines are skipped gracefully
#[test]
#[serial]
fn codex_connector_skips_malformed_lines() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/27");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-malformed.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{ this is not valid json
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"valid message"}]}}
also not valid
{"timestamp":"2025-09-30T15:42:43.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"valid response"}]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Should have 2 valid messages, malformed lines skipped
    assert_eq!(c.messages.len(), 2);
}

/// Test multiple sessions in separate files
#[test]
#[serial]
fn codex_connector_handles_multiple_sessions() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/28");
    fs::create_dir_all(&sessions).unwrap();

    for i in 1..=3 {
        let file = sessions.join(format!("rollout-{i}.jsonl"));
        let sample = format!(
            r#"{{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{{"id":"session-{i}","cwd":"/test/{i}"}}}}
{{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"message {i}"}}]}}}}
"#
        );
        fs::write(&file, sample).unwrap();
    }

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 3);
}

/// Test empty content messages are filtered
///
/// set CODEX_HOME env var without proper serialization, causing parallel
/// tests to interfere with each other. Works locally but fails on CI.
/// Consider adding serial_test or refactoring to avoid env var mutation.
#[test]
#[serial]
fn codex_connector_filters_empty_content() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/29");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-empty.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"   "}]}}
{"timestamp":"2025-09-30T15:42:37.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"valid content"}]}}
{"timestamp":"2025-09-30T15:42:43.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Only the message with "valid content" should be included
    assert_eq!(c.messages.len(), 1);
    assert!(c.messages[0].content.contains("valid content"));
}

/// Test title extraction from first user message
///
#[test]
#[serial]
fn codex_connector_extracts_title() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/30");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-title.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:35.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"assistant first"}]}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"This is the user's question\nWith a second line"}]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Title should be first line of first user message
    assert_eq!(c.title, Some("This is the user's question".to_string()));
}

/// Test sequential index assignment
#[test]
#[serial]
fn codex_connector_assigns_sequential_indices() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/01");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-idx.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}}
{"timestamp":"2025-09-30T15:42:37.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"second"}]}}
{"timestamp":"2025-09-30T15:42:38.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"third"}]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.messages.len(), 3);
    assert_eq!(c.messages[0].idx, 0);
    assert_eq!(c.messages[1].idx, 1);
    assert_eq!(c.messages[2].idx, 2);
}

/// Test `external_id` comes from filename
#[test]
#[serial]
fn codex_connector_sets_external_id_from_filename() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/02");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-unique-id-123.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"test"}]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // external_id is now the relative path from sessions dir for uniqueness across directories
    assert_eq!(
        c.external_id,
        Some("2025/12/02/rollout-unique-id-123".to_string())
    );
}

/// Test empty sessions directory returns no conversations
#[test]
#[serial]
fn codex_connector_handles_empty_sessions() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    // No files in sessions

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

/// Test integer (milliseconds) timestamp format
#[test]
#[serial]
fn codex_connector_parses_millis_timestamp() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/03");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-millis.jsonl");

    // Timestamps as i64 milliseconds instead of ISO-8601 strings
    let sample = r#"{"timestamp":1700000000000,"type":"session_meta","payload":{"id":"millis-test","cwd":"/millis"}}
{"timestamp":1700000001000,"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"millis timestamp test"}]}}
{"timestamp":1700000002000,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"response with millis"}]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.messages.len(), 2);
    // Verify timestamps were parsed from i64 millis
    // started_at comes from session_meta timestamp (1700000000000)
    assert_eq!(c.started_at, Some(1700000000000));
    // ended_at comes from the last message timestamp (1700000002000)
    assert_eq!(c.ended_at, Some(1700000002000));
}

/// Test `tool_use` blocks in content are flattened properly
#[test]
#[serial]
fn codex_connector_flattens_tool_use_blocks() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/04");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-tools.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"tool-test","cwd":"/tools"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"read a file"}]}}
{"timestamp":"2025-09-30T15:42:43.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"Let me read that file"},{"type":"tool_use","name":"Read","input":{"file_path":"/test/file.rs"}}]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.messages.len(), 2);

    // Assistant message should contain flattened tool_use
    let assistant = &c.messages[1];
    assert!(assistant.content.contains("Let me read that file"));
    assert!(assistant.content.contains("[Tool: Read"));
    assert!(assistant.content.contains("/test/file.rs"));
}

/// Test missing cwd in `session_meta` results in None workspace
#[test]
#[serial]
fn codex_connector_handles_missing_cwd() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/05");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-no-cwd.jsonl");

    // session_meta without cwd field
    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"no-cwd","cli_version":"0.42.0"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"test without cwd"}]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert!(
        c.workspace.is_none(),
        "workspace should be None when cwd missing"
    );
}

/// Test files without rollout- prefix are ignored
#[test]
#[serial]
fn codex_connector_ignores_non_rollout_files() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/06");
    fs::create_dir_all(&sessions).unwrap();

    // Valid rollout file
    let rollout = sessions.join("rollout-valid.jsonl");
    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"valid","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"valid"}]}}
"#;
    fs::write(&rollout, sample).unwrap();

    // Non-rollout files that should be ignored
    let other1 = sessions.join("session-123.jsonl");
    let other2 = sessions.join("backup.json");
    let other3 = sessions.join("config.jsonl");
    fs::write(&other1, sample).unwrap();
    fs::write(&other2, sample).unwrap();
    fs::write(&other3, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    // Only the rollout- prefixed file should be processed
    assert_eq!(convs.len(), 1);
    // external_id is now the relative path from sessions dir for uniqueness across directories
    assert_eq!(
        convs[0].external_id,
        Some("2025/12/06/rollout-valid".to_string())
    );
}

/// Test legacy JSON with missing optional fields
#[test]
#[serial]
fn codex_connector_handles_legacy_json_missing_session() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/07");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-minimal.json");

    // Minimal legacy format without session object
    let sample = r#"{
        "items": [
            {
                "role": "user",
                "content": "minimal legacy message"
            }
        ]
    }"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert!(c.workspace.is_none());
    assert_eq!(c.messages.len(), 1);
    assert!(c.messages[0].content.contains("minimal legacy message"));
}

/// Test title fallback to first message when no user message exists
#[test]
#[serial]
fn codex_connector_title_fallback_to_first_message() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/08");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-no-user.jsonl");

    // Only assistant messages, no user message
    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"no-user","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"Assistant first line\nSecond line"}]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Title should fallback to first line of first message
    assert_eq!(c.title, Some("Assistant first line".to_string()));
}

/// Test deeply nested directory structure
#[test]
#[serial]
fn codex_connector_handles_nested_directories() {
    let dir = TempDir::new().unwrap();
    let deep_sessions = dir.path().join("sessions/2025/12/09/sub1/sub2");
    fs::create_dir_all(&deep_sessions).unwrap();
    let file = deep_sessions.join("rollout-nested.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"nested","cwd":"/nested"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"deeply nested"}]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(convs[0].source_path.to_string_lossy().contains("sub2"));
}

/// Test `turn_aborted` event is filtered out
#[test]
#[serial]
fn codex_connector_filters_turn_aborted() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/10");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-aborted.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"test"}]}}
{"timestamp":"2025-09-30T15:42:37.000Z","type":"event_msg","payload":{"type":"turn_aborted","reason":"user cancelled"}}
{"timestamp":"2025-09-30T15:42:38.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"response"}]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Should only have 2 messages (user, assistant) - turn_aborted filtered
    assert_eq!(c.messages.len(), 2);
    for msg in &c.messages {
        assert!(!msg.content.contains("turn_aborted"));
    }
}

/// Test long title is truncated to 100 chars
#[test]
#[serial]
fn codex_connector_truncates_long_title() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/11");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-long-title.jsonl");

    let long_text = "A".repeat(200);
    let sample = format!(
        r#"{{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{{"id":"long","cwd":"/test"}}}}
{{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{long_text}"}}]}}}}
"#
    );
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert!(c.title.is_some());
    assert_eq!(c.title.as_ref().unwrap().len(), 100);
}

/// Test `source_path` matches actual file path
#[test]
#[serial]
fn codex_connector_sets_source_path() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/12");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-source-path.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"path-test","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"test source path"}]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.source_path, file);
}

/// Test metadata indicates correct source format
#[test]
#[serial]
fn codex_connector_metadata_indicates_format() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/13");
    fs::create_dir_all(&sessions).unwrap();

    // Create both JSONL and JSON files
    let jsonl_file = sessions.join("rollout-jsonl.jsonl");
    let json_file = sessions.join("rollout-json.json");

    let jsonl_sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"jsonl","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"jsonl format"}]}}
"#;
    fs::write(&jsonl_file, jsonl_sample).unwrap();

    let json_sample = r#"{"session":{"id":"json","cwd":"/test"},"items":[{"role":"user","content":"json format"}]}"#;
    fs::write(&json_file, json_sample).unwrap();

    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 2);

    // Find each conversation and verify metadata
    let jsonl_conv = convs.iter().find(|c| c.source_path == jsonl_file).unwrap();
    let json_conv = convs.iter().find(|c| c.source_path == json_file).unwrap();

    assert_eq!(
        jsonl_conv.metadata.get("source").and_then(|v| v.as_str()),
        Some("rollout")
    );
    assert_eq!(
        json_conv.metadata.get("source").and_then(|v| v.as_str()),
        Some("rollout_json")
    );
}
