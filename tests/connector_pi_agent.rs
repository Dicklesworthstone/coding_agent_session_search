use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

use coding_agent_search::connectors::{Connector, ScanContext, pi_agent::PiAgentConnector};
use serial_test::serial;

#[test]
#[serial]
fn pi_agent_connector_reads_session_jsonl() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--test-project--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00-000Z_abc12345-1234-5678-9abc-def012345678.jsonl");

    let sample = r#"{"type":"session","id":"abc12345-1234-5678-9abc-def012345678","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/Users/test/project","provider":"anthropic","modelId":"claude-sonnet-4-20250514","thinkingLevel":"medium"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":[{"type":"text","text":"How do I create a Rust struct?"}],"timestamp":1705315801000}}
{"type":"message","timestamp":"2024-01-15T10:30:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Here's how to create a Rust struct:\n\n```rust\nstruct MyStruct {\n    field: i32,\n}\n```"}],"timestamp":1705315805000}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let ctx = ScanContext {
        data_root: dir.path().to_path_buf(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];
    assert_eq!(c.agent_slug, "pi_agent");
    assert_eq!(c.messages.len(), 2);
    assert!(c.title.as_ref().unwrap().contains("create a Rust struct"));
    assert_eq!(c.workspace, Some(PathBuf::from("/Users/test/project")));
    assert!(c.started_at.is_some());
    assert!(c.ended_at.is_some());
}

#[test]
#[serial]
fn pi_agent_connector_includes_thinking_content() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--test--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_uuid.jsonl");

    let sample = r#"{"type":"session","id":"test-id","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude-sonnet-4","thinkingLevel":"high"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"solve this problem"}}
{"type":"message","timestamp":"2024-01-15T10:30:05.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me think about this carefully..."},{"type":"text","text":"Here is the solution"}]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let ctx = ScanContext {
        data_root: dir.path().to_path_buf(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];

    assert_eq!(c.messages.len(), 2);

    // Check thinking content is included
    let assistant = &c.messages[1];
    assert!(assistant.content.contains("[Thinking]"));
    assert!(assistant.content.contains("think about this carefully"));
    assert!(assistant.content.contains("Here is the solution"));
}

#[test]
#[serial]
fn pi_agent_connector_handles_tool_calls() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--tools-test--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_tools.jsonl");

    let sample = r#"{"type":"session","id":"tools-test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude-sonnet-4","thinkingLevel":"off"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"read the main.rs file"}}
{"type":"message","timestamp":"2024-01-15T10:30:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Let me read that file for you"},{"type":"toolCall","id":"call_123","name":"read","arguments":{"file_path":"/src/main.rs"}}]}}
{"type":"message","timestamp":"2024-01-15T10:30:06.000Z","message":{"role":"toolResult","toolCallId":"call_123","toolName":"read","content":[{"type":"text","text":"fn main() { println!(\"Hello\"); }"}],"isError":false}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let ctx = ScanContext {
        data_root: dir.path().to_path_buf(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];

    assert_eq!(c.messages.len(), 3);

    // Check tool call is flattened
    let assistant = &c.messages[1];
    assert!(assistant.content.contains("[Tool: read]"));
    assert!(assistant.content.contains("file_path=/src/main.rs"));

    // Check tool result is included
    let tool_result = &c.messages[2];
    assert_eq!(tool_result.role, "tool");
    assert!(tool_result.content.contains("fn main()"));
}

#[test]
#[serial]
fn pi_agent_connector_handles_model_change() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--model-change--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_model.jsonl");

    let sample = r#"{"type":"session","id":"model-test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude-sonnet-4","thinkingLevel":"off"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"hello"}}
{"type":"model_change","timestamp":"2024-01-15T10:31:00.000Z","provider":"anthropic","modelId":"claude-opus-4"}
{"type":"message","timestamp":"2024-01-15T10:31:05.000Z","message":{"role":"assistant","content":"Hello! I'm now using Opus."}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let ctx = ScanContext {
        data_root: dir.path().to_path_buf(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];

    // Model change events are tracked in metadata
    assert_eq!(
        c.metadata.get("model_id").and_then(|v| v.as_str()),
        Some("claude-opus-4")
    );
}

#[test]
#[serial]
fn pi_agent_connector_detection_with_sessions_dir() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let result = connector.detect();
    assert!(result.detected);
    assert!(!result.evidence.is_empty());
}

#[test]
#[serial]
fn pi_agent_connector_detection_without_sessions_dir() {
    let dir = TempDir::new().unwrap();
    // Don't create sessions directory

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let result = connector.detect();
    assert!(!result.detected);
}

#[test]
#[serial]
fn pi_agent_connector_skips_malformed_lines() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--malformed--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_malformed.jsonl");

    let sample = r#"{"type":"session","id":"test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude","thinkingLevel":"off"}
{ this is not valid json
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"valid message"}}
also not valid
{"type":"message","timestamp":"2024-01-15T10:30:05.000Z","message":{"role":"assistant","content":"valid response"}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let ctx = ScanContext {
        data_root: dir.path().to_path_buf(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Should have 2 valid messages, malformed lines skipped
    assert_eq!(c.messages.len(), 2);
}

#[test]
#[serial]
fn pi_agent_connector_handles_string_content() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--string-content--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_string.jsonl");

    // User message with direct string content (not array)
    let sample = r#"{"type":"session","id":"test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude","thinkingLevel":"off"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"simple string content"}}
{"type":"message","timestamp":"2024-01-15T10:30:05.000Z","message":{"role":"assistant","content":"simple response"}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let ctx = ScanContext {
        data_root: dir.path().to_path_buf(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.messages.len(), 2);
    assert!(c.messages[0].content.contains("simple string content"));
    assert!(c.messages[1].content.contains("simple response"));
}

#[test]
#[serial]
fn pi_agent_connector_filters_empty_content() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--empty--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_empty.jsonl");

    let sample = r#"{"type":"session","id":"test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude","thinkingLevel":"off"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"   "}}
{"type":"message","timestamp":"2024-01-15T10:30:02.000Z","message":{"role":"user","content":"valid content"}}
{"type":"message","timestamp":"2024-01-15T10:30:05.000Z","message":{"role":"assistant","content":[]}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let ctx = ScanContext {
        data_root: dir.path().to_path_buf(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Only the message with "valid content" should be included
    assert_eq!(c.messages.len(), 1);
    assert!(c.messages[0].content.contains("valid content"));
}

#[test]
#[serial]
fn pi_agent_connector_extracts_title_from_first_user_message() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--title--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_title.jsonl");

    let sample = r#"{"type":"session","id":"test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude","thinkingLevel":"off"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"assistant","content":"I'm ready to help"}}
{"type":"message","timestamp":"2024-01-15T10:30:02.000Z","message":{"role":"user","content":"This is the user's question\nWith a second line"}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let ctx = ScanContext {
        data_root: dir.path().to_path_buf(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Title should be first line of first user message
    assert_eq!(c.title, Some("This is the user's question".to_string()));
}

#[test]
#[serial]
fn pi_agent_connector_truncates_long_title() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--long-title--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_long.jsonl");

    let long_text = "A".repeat(200);
    let sample = format!(
        r#"{{"type":"session","id":"test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude","thinkingLevel":"off"}}
{{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{{"role":"user","content":"{long_text}"}}}}
"#
    );
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let ctx = ScanContext {
        data_root: dir.path().to_path_buf(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert!(c.title.is_some());
    assert_eq!(c.title.as_ref().unwrap().len(), 100);
}

#[test]
#[serial]
fn pi_agent_connector_assigns_sequential_indices() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--indices--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_idx.jsonl");

    let sample = r#"{"type":"session","id":"test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude","thinkingLevel":"off"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"first"}}
{"type":"message","timestamp":"2024-01-15T10:30:02.000Z","message":{"role":"assistant","content":"second"}}
{"type":"message","timestamp":"2024-01-15T10:30:03.000Z","message":{"role":"user","content":"third"}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let ctx = ScanContext {
        data_root: dir.path().to_path_buf(),
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

#[test]
#[serial]
fn pi_agent_connector_metadata_includes_provider_info() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--metadata--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_meta.jsonl");

    let sample = r#"{"type":"session","id":"meta-session-id","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude-sonnet-4","thinkingLevel":"high"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"test"}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let ctx = ScanContext {
        data_root: dir.path().to_path_buf(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(
        c.metadata.get("source").and_then(|v| v.as_str()),
        Some("pi_agent")
    );
    assert_eq!(
        c.metadata.get("session_id").and_then(|v| v.as_str()),
        Some("meta-session-id")
    );
    assert_eq!(
        c.metadata.get("provider").and_then(|v| v.as_str()),
        Some("anthropic")
    );
    assert_eq!(
        c.metadata.get("model_id").and_then(|v| v.as_str()),
        Some("claude-sonnet-4")
    );
}

#[test]
#[serial]
fn pi_agent_connector_ignores_files_without_underscore() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--filter--");
    fs::create_dir_all(&sessions).unwrap();

    // Valid pi-agent session file (has timestamp_uuid format)
    let valid = sessions.join("2024-01-15T10-30-00_abc123.jsonl");
    let sample = r#"{"type":"session","id":"valid","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude","thinkingLevel":"off"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"valid"}}
"#;
    fs::write(&valid, sample).unwrap();

    // Non-pi-agent files that should be ignored (no underscore)
    let other1 = sessions.join("notes.jsonl");
    let other2 = sessions.join("backup.jsonl");
    fs::write(&other1, sample).unwrap();
    fs::write(&other2, sample).unwrap();

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let ctx = ScanContext {
        data_root: dir.path().to_path_buf(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    // Only the file with underscore pattern should be processed
    assert_eq!(convs.len(), 1);
}

#[test]
#[serial]
fn pi_agent_connector_handles_empty_sessions() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    // No files in sessions

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let ctx = ScanContext {
        data_root: dir.path().to_path_buf(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
#[serial]
fn pi_agent_connector_skips_thinking_level_change() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--thinking--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_thinking.jsonl");

    let sample = r#"{"type":"session","id":"test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude","thinkingLevel":"off"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"test"}}
{"type":"thinking_level_change","timestamp":"2024-01-15T10:31:00.000Z","thinkingLevel":"high"}
{"type":"message","timestamp":"2024-01-15T10:31:05.000Z","message":{"role":"assistant","content":"response"}}
"#;
    fs::write(&file, sample).unwrap();

    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
    }

    let connector = PiAgentConnector::new();
    let ctx = ScanContext {
        data_root: dir.path().to_path_buf(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Should have 2 messages - thinking_level_change is not a message
    assert_eq!(c.messages.len(), 2);
    for msg in &c.messages {
        assert!(!msg.content.contains("thinking_level_change"));
    }
}
