use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

use coding_agent_search::connectors::{
    Connector, ScanContext, ScanRoot, pi_agent::PiAgentConnector,
};

#[test]
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

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation from session file"
    );
    let c = &convs[0];
    assert_eq!(
        c.agent_slug, "pi_agent",
        "agent_slug should be 'pi_agent' for PiAgentConnector"
    );
    assert_eq!(
        c.messages.len(),
        2,
        "expected 2 messages (1 user + 1 assistant) but got {}",
        c.messages.len()
    );
    assert!(
        c.title.as_ref().unwrap().contains("create a Rust struct"),
        "title should contain first user message text"
    );
    assert_eq!(
        c.workspace,
        Some(PathBuf::from("/Users/test/project")),
        "workspace should match cwd from session header"
    );
    assert!(
        c.started_at.is_some(),
        "started_at timestamp should be populated from session"
    );
    assert!(
        c.ended_at.is_some(),
        "ended_at timestamp should be populated from last message"
    );
}

#[test]
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

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation from session file"
    );
    let c = &convs[0];

    assert_eq!(
        c.messages.len(),
        2,
        "expected 2 messages (user + assistant with thinking)"
    );

    // Check thinking content is included
    let assistant = &c.messages[1];
    assert!(
        assistant.content.contains("[Thinking]"),
        "assistant message should include [Thinking] marker"
    );
    assert!(
        assistant.content.contains("think about this carefully"),
        "thinking content should be preserved in message"
    );
    assert!(
        assistant.content.contains("Here is the solution"),
        "text content should be preserved after thinking block"
    );
}

#[test]
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

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation from session file"
    );
    let c = &convs[0];

    assert_eq!(
        c.messages.len(),
        3,
        "expected 3 messages (user + assistant + tool result)"
    );

    // Check tool call is flattened
    let assistant = &c.messages[1];
    assert!(
        assistant.content.contains("[Tool: read]"),
        "tool call should be formatted with [Tool: name] marker"
    );
    assert!(
        assistant.content.contains("file_path=/src/main.rs"),
        "tool arguments should be flattened into content"
    );

    // Check tool result is included
    let tool_result = &c.messages[2];
    assert_eq!(
        tool_result.role, "tool",
        "tool result message should have role 'tool'"
    );
    assert!(
        tool_result.content.contains("fn main()"),
        "tool result content should be preserved"
    );
}

#[test]
fn pi_agent_connector_handles_model_change() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--model-change--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_model.jsonl");

    let sample = r#"{"type":"session","id":"model-test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude-sonnet-4","thinkingLevel":"off"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"hello"}}
{"type":"message","timestamp":"2024-01-15T10:30:02.000Z","message":{"role":"assistant","content":"Hello with Sonnet!"}}
{"type":"model_change","timestamp":"2024-01-15T10:31:00.000Z","provider":"anthropic","modelId":"claude-opus-4"}
{"type":"message","timestamp":"2024-01-15T10:31:05.000Z","message":{"role":"assistant","content":"Hello! I'm now using Opus."}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation from session with model change"
    );
    let c = &convs[0];

    assert_eq!(
        c.messages.len(),
        3,
        "expected 3 messages (user + 2 assistant)"
    );

    // Model change events are tracked in metadata (final model)
    assert_eq!(
        c.metadata.get("model_id").and_then(|v| v.as_str()),
        Some("claude-opus-4"),
        "metadata model_id should reflect final model after model_change events"
    );

    // First assistant message (before model_change) uses initial modelId
    assert_eq!(
        c.messages[1].author,
        Some("claude-sonnet-4".to_string()),
        "first assistant should use initial model from session header"
    );

    // Second assistant message (after model_change) uses updated modelId
    assert_eq!(
        c.messages[2].author,
        Some("claude-opus-4".to_string()),
        "second assistant should use updated model after model_change"
    );
}

#[test]
fn pi_agent_connector_detection_with_sessions_dir() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // qu81y: detection is exercised through FAD's explicit root-override
    // input (the same <dir>/sessions root PI_CODING_AGENT_DIR used to
    // resolve to) instead of mutating process-global env.
    let report = franken_agent_detection::detect_installed_agents(
        &franken_agent_detection::AgentDetectOptions {
            only_connectors: Some(vec!["pi_agent".to_string()]),
            include_undetected: true,
            root_overrides: vec![franken_agent_detection::AgentDetectRootOverride {
                slug: "pi_agent".to_string(),
                root: sessions,
            }],
        },
    )
    .expect("pi_agent detection report");
    let entry = &report.installed_agents[0];
    assert!(
        entry.detected,
        "connector should detect pi_agent when sessions dir exists"
    );
    assert!(
        !entry.evidence.is_empty(),
        "detection evidence should be non-empty when detected"
    );
}

#[test]
fn pi_agent_connector_detection_without_sessions_dir() {
    let dir = TempDir::new().unwrap();
    // Don't create the sessions directory: the overridden probe root does
    // not exist, so detection must report not-found.
    let report = franken_agent_detection::detect_installed_agents(
        &franken_agent_detection::AgentDetectOptions {
            only_connectors: Some(vec!["pi_agent".to_string()]),
            include_undetected: true,
            root_overrides: vec![franken_agent_detection::AgentDetectRootOverride {
                slug: "pi_agent".to_string(),
                root: dir.path().join("sessions"),
            }],
        },
    )
    .expect("pi_agent detection report");
    let entry = &report.installed_agents[0];
    assert!(
        !entry.detected,
        "connector should not detect pi_agent when sessions dir is missing"
    );
}

#[test]
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

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation despite malformed lines"
    );

    let c = &convs[0];
    // Should have 2 valid messages, malformed lines skipped
    assert_eq!(
        c.messages.len(),
        2,
        "expected 2 messages - malformed JSON lines should be skipped"
    );
}

#[test]
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

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation with string content"
    );

    let c = &convs[0];
    assert_eq!(
        c.messages.len(),
        2,
        "expected 2 messages (user + assistant) with string content format"
    );
    assert!(
        c.messages[0].content.contains("simple string content"),
        "user message string content should be preserved"
    );
    assert!(
        c.messages[1].content.contains("simple response"),
        "assistant message string content should be preserved"
    );
}

#[test]
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

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation when filtering empty content"
    );

    let c = &convs[0];
    // Only the message with "valid content" should be included
    assert_eq!(
        c.messages.len(),
        1,
        "expected only 1 message - empty/whitespace-only content should be filtered"
    );
    assert!(
        c.messages[0].content.contains("valid content"),
        "only message with actual content should be preserved"
    );
}

#[test]
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

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation for title extraction test"
    );

    let c = &convs[0];
    // Title should be first line of first user message
    assert_eq!(
        c.title,
        Some("This is the user's question".to_string()),
        "title should be extracted from first line of first user message"
    );
}

#[test]
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

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation for long title test"
    );

    let c = &convs[0];
    assert!(
        c.title.is_some(),
        "title should be present even for long content"
    );
    assert_eq!(
        c.title.as_ref().unwrap().len(),
        100,
        "long titles should be truncated to 100 characters"
    );
}

#[test]
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

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation for sequential indices test"
    );

    let c = &convs[0];
    assert_eq!(
        c.messages.len(),
        3,
        "expected 3 messages for index assignment test"
    );
    assert_eq!(c.messages[0].idx, 0, "first message should have index 0");
    assert_eq!(c.messages[1].idx, 1, "second message should have index 1");
    assert_eq!(c.messages[2].idx, 2, "third message should have index 2");
}

#[test]
fn pi_agent_connector_metadata_includes_provider_info() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--metadata--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_meta.jsonl");

    let sample = r#"{"type":"session","id":"meta-session-id","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude-sonnet-4","thinkingLevel":"high"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"test"}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation for metadata test"
    );

    let c = &convs[0];
    assert_eq!(
        c.metadata.get("source").and_then(|v| v.as_str()),
        Some("pi_agent"),
        "metadata source should be 'pi_agent' for PiAgentConnector"
    );
    assert_eq!(
        c.metadata.get("session_id").and_then(|v| v.as_str()),
        Some("meta-session-id"),
        "metadata session_id should match id from session header"
    );
    assert_eq!(
        c.metadata.get("provider").and_then(|v| v.as_str()),
        Some("anthropic"),
        "metadata provider should match provider from session header"
    );
    assert_eq!(
        c.metadata.get("model_id").and_then(|v| v.as_str()),
        Some("claude-sonnet-4"),
        "metadata model_id should match modelId from session header"
    );
}

#[test]
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

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    // Only the file with underscore pattern should be processed
    assert_eq!(
        convs.len(),
        1,
        "should only process files with timestamp_uuid pattern, ignoring others"
    );
}

#[test]
fn pi_agent_connector_handles_empty_sessions() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    // No files in sessions

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert!(
        convs.is_empty(),
        "empty sessions directory should yield no conversations"
    );
}

#[test]
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

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation when skipping thinking_level_change"
    );

    let c = &convs[0];
    // Should have 2 messages - thinking_level_change is not a message
    assert_eq!(
        c.messages.len(),
        2,
        "thinking_level_change events should not be counted as messages"
    );
    for msg in &c.messages {
        assert!(
            !msg.content.contains("thinking_level_change"),
            "message content should not contain thinking_level_change event type"
        );
    }
}

#[test]
fn pi_agent_connector_populates_author_for_assistant_messages() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--author--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_author.jsonl");

    let sample = r#"{"type":"session","id":"test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude-sonnet-4","thinkingLevel":"off"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"test question"}}
{"type":"message","timestamp":"2024-01-15T10:30:02.000Z","message":{"role":"assistant","content":"response without explicit model"}}
{"type":"message","timestamp":"2024-01-15T10:30:03.000Z","message":{"role":"assistant","model":"claude-opus-4-5","content":"response with explicit model"}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation for author population test"
    );

    let c = &convs[0];
    assert_eq!(
        c.messages.len(),
        3,
        "expected 3 messages (user + 2 assistant)"
    );

    // User message should have no author
    assert_eq!(
        c.messages[0].role, "user",
        "first message should be from user"
    );
    assert!(
        c.messages[0].author.is_none(),
        "user messages should not have author field set"
    );

    // First assistant message uses modelId from session header
    assert_eq!(
        c.messages[1].role, "assistant",
        "second message should be from assistant"
    );
    assert_eq!(
        c.messages[1].author,
        Some("claude-sonnet-4".to_string()),
        "assistant message should use modelId from session header"
    );

    // Second assistant message uses explicit model from message
    assert_eq!(
        c.messages[2].role, "assistant",
        "third message should be from assistant"
    );
    assert_eq!(
        c.messages[2].author,
        Some("claude-opus-4-5".to_string()),
        "assistant message with explicit model should use that model"
    );
}

// =============================================================================
// Edge Case Tests (TST.CON)
// =============================================================================

#[test]
fn pi_agent_connector_handles_multiple_model_changes() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--multi-model--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_multi.jsonl");

    // Test multiple model changes within a single session
    let sample = r#"{"type":"session","id":"multi-model-test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude-sonnet-4","thinkingLevel":"off"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"first question"}}
{"type":"message","timestamp":"2024-01-15T10:30:02.000Z","message":{"role":"assistant","content":"answer with sonnet"}}
{"type":"model_change","timestamp":"2024-01-15T10:31:00.000Z","provider":"anthropic","modelId":"claude-opus-4"}
{"type":"message","timestamp":"2024-01-15T10:31:05.000Z","message":{"role":"assistant","content":"answer with opus"}}
{"type":"model_change","timestamp":"2024-01-15T10:32:00.000Z","provider":"openai","modelId":"gpt-4-turbo"}
{"type":"message","timestamp":"2024-01-15T10:32:05.000Z","message":{"role":"assistant","content":"answer with gpt-4"}}
{"type":"model_change","timestamp":"2024-01-15T10:33:00.000Z","provider":"anthropic","modelId":"claude-sonnet-4"}
{"type":"message","timestamp":"2024-01-15T10:33:05.000Z","message":{"role":"assistant","content":"back to sonnet"}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation with multiple model changes"
    );

    let c = &convs[0];
    assert_eq!(
        c.messages.len(),
        5,
        "expected 5 messages (user + 4 assistant) across model changes"
    );

    // Verify each assistant message has the correct model based on most recent model_change
    assert_eq!(
        c.messages[1].author,
        Some("claude-sonnet-4".to_string()),
        "msg 1 should use initial model before any model_change"
    ); // Before any model_change
    assert_eq!(
        c.messages[2].author,
        Some("claude-opus-4".to_string()),
        "msg 2 should use claude-opus-4 after first model_change"
    ); // After first model_change
    assert_eq!(
        c.messages[3].author,
        Some("gpt-4-turbo".to_string()),
        "msg 3 should use gpt-4-turbo after second model_change"
    ); // After second model_change
    assert_eq!(
        c.messages[4].author,
        Some("claude-sonnet-4".to_string()),
        "msg 4 should use claude-sonnet-4 after third model_change"
    ); // After third model_change

    // Final metadata should reflect last model state
    assert_eq!(
        c.metadata.get("model_id").and_then(|v| v.as_str()),
        Some("claude-sonnet-4"),
        "final metadata model_id should reflect last model after all changes"
    );
}

#[test]
fn pi_agent_connector_handles_empty_thinking_block() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--empty-thinking--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_empty_think.jsonl");

    // Test empty thinking content - should be handled gracefully
    let sample = r#"{"type":"session","id":"empty-thinking-test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude","thinkingLevel":"high"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"analyze this"}}
{"type":"message","timestamp":"2024-01-15T10:30:05.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":""},{"type":"text","text":"Here is my response"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation with empty thinking block"
    );

    let c = &convs[0];
    assert_eq!(
        c.messages.len(),
        2,
        "expected 2 messages (user + assistant with empty thinking)"
    );

    // The assistant message should still be parsed correctly
    let assistant = &c.messages[1];
    assert!(
        assistant.content.contains("Here is my response"),
        "text content should be preserved even with empty thinking block"
    );
    // Empty thinking blocks may be included as "[Thinking] " or omitted entirely
    // depending on connector implementation - both are valid behaviors
}

#[test]
fn pi_agent_connector_handles_nested_tool_calls() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--nested-tools--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_nested.jsonl");

    // Test tool calls that result in more tool calls (nested pattern)
    let sample = r#"{"type":"session","id":"nested-tools","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude","thinkingLevel":"off"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"search and read files"}}
{"type":"message","timestamp":"2024-01-15T10:30:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"I'll search for files first"},{"type":"toolCall","id":"call_1","name":"search","arguments":{"query":"main.rs"}}]}}
{"type":"message","timestamp":"2024-01-15T10:30:03.000Z","message":{"role":"toolResult","toolCallId":"call_1","toolName":"search","content":[{"type":"text","text":"Found: /src/main.rs"}],"isError":false}}
{"type":"message","timestamp":"2024-01-15T10:30:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Found the file, reading it now"},{"type":"toolCall","id":"call_2","name":"read","arguments":{"file_path":"/src/main.rs"}}]}}
{"type":"message","timestamp":"2024-01-15T10:30:05.000Z","message":{"role":"toolResult","toolCallId":"call_2","toolName":"read","content":[{"type":"text","text":"fn main() { println!(\"Hello\"); }"}],"isError":false}}
{"type":"message","timestamp":"2024-01-15T10:30:06.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Here's the contents of main.rs"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation with nested tool calls"
    );

    let c = &convs[0];
    assert_eq!(
        c.messages.len(),
        6,
        "expected 6 messages in nested tool call sequence"
    );

    // Verify all messages are properly parsed in sequence
    assert_eq!(c.messages[0].role, "user", "msg 0 should be user request");
    assert_eq!(
        c.messages[1].role, "assistant",
        "msg 1 should be assistant with search tool"
    );
    assert!(
        c.messages[1].content.contains("[Tool: search]"),
        "assistant should have search tool call formatted"
    );
    assert_eq!(
        c.messages[2].role, "tool",
        "msg 2 should be search tool result"
    );
    assert!(
        c.messages[2].content.contains("/src/main.rs"),
        "search result should contain found file path"
    );
    assert_eq!(
        c.messages[3].role, "assistant",
        "msg 3 should be assistant with read tool"
    );
    assert!(
        c.messages[3].content.contains("[Tool: read]"),
        "assistant should have read tool call formatted"
    );
    assert_eq!(
        c.messages[4].role, "tool",
        "msg 4 should be read tool result"
    );
    assert!(
        c.messages[4].content.contains("fn main()"),
        "read result should contain file content"
    );
    assert_eq!(
        c.messages[5].role, "assistant",
        "msg 5 should be final assistant response"
    );
}

#[test]
fn pi_agent_connector_handles_very_long_session() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--long-session--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_long.jsonl");

    // Test performance with 1000+ messages
    let mut lines = vec![
        r#"{"type":"session","id":"long-session","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude","thinkingLevel":"off"}"#.to_string()
    ];

    // Add 500 user-assistant pairs (1000 messages)
    for i in 0..500 {
        lines.push(format!(
            r#"{{"type":"message","timestamp":"2024-01-15T{:02}:{:02}:00.000Z","message":{{"role":"user","content":"Question number {}"}}}}"#,
            10 + (i / 60),
            i % 60,
            i
        ));
        lines.push(format!(
            r#"{{"type":"message","timestamp":"2024-01-15T{:02}:{:02}:01.000Z","message":{{"role":"assistant","content":"Answer number {}"}}}}"#,
            10 + (i / 60),
            i % 60,
            i
        ));
    }

    fs::write(&file, lines.join("\n")).unwrap();

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );

    let start = std::time::Instant::now();
    let convs = connector.scan(&ctx).unwrap();
    let elapsed = start.elapsed();

    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation for 1000-message stress test"
    );
    let c = &convs[0];
    assert_eq!(
        c.messages.len(),
        1000,
        "expected all 1000 messages to be parsed"
    );

    // Verify first and last messages
    assert!(
        c.messages[0].content.contains("Question number 0"),
        "first message should be 'Question number 0'"
    );
    assert!(
        c.messages[999].content.contains("Answer number 499"),
        "last message should be 'Answer number 499'"
    );

    // Indices should be sequential
    assert_eq!(c.messages[0].idx, 0, "first message should have index 0");
    assert_eq!(
        c.messages[999].idx, 999,
        "last message should have index 999"
    );

    // Should complete in reasonable time (< 5 seconds)
    assert!(
        elapsed.as_secs() < 5,
        "Parsing 1000 messages took too long: {:?}",
        elapsed
    );
}

#[test]
fn pi_agent_connector_handles_unicode_content() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--unicode--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_unicode.jsonl");

    // Test various Unicode content: emojis, CJK, RTL, combining characters
    let sample = r#"{"type":"session","id":"unicode-test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude","thinkingLevel":"off"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"Hello 你好 مرحبا שלום 🎉🦀"}}
{"type":"message","timestamp":"2024-01-15T10:30:02.000Z","message":{"role":"assistant","content":"Response with émojis: 👍✅🚀 and Ümlauts"}}
{"type":"message","timestamp":"2024-01-15T10:30:03.000Z","message":{"role":"user","content":"Combined characters: café ñ ü"}}
{"type":"message","timestamp":"2024-01-15T10:30:04.000Z","message":{"role":"assistant","content":"Math symbols: ∑ ∫ π ∞ √"}}
{"type":"message","timestamp":"2024-01-15T10:30:05.000Z","message":{"role":"user","content":"Japanese: 日本語テスト Korean: 한국어 Thai: ภาษาไทย"}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation with Unicode content"
    );

    let c = &convs[0];
    assert_eq!(
        c.messages.len(),
        5,
        "expected 5 messages with various Unicode content"
    );

    // Verify Unicode content is preserved
    assert!(
        c.messages[0].content.contains("你好"),
        "Chinese characters should be preserved"
    );
    assert!(
        c.messages[0].content.contains("مرحبا"),
        "Arabic characters should be preserved"
    );
    assert!(
        c.messages[0].content.contains("🎉🦀"),
        "emojis should be preserved"
    );
    assert!(
        c.messages[1].content.contains("👍✅🚀"),
        "emoji sequences should be preserved"
    );
    assert!(
        c.messages[2].content.contains("café"),
        "combining characters should be preserved"
    );
    assert!(
        c.messages[3].content.contains("∑"),
        "math symbols should be preserved"
    );
    assert!(
        c.messages[3].content.contains("π"),
        "Greek letters should be preserved"
    );
    assert!(
        c.messages[4].content.contains("日本語"),
        "Japanese characters should be preserved"
    );
    assert!(
        c.messages[4].content.contains("한국어"),
        "Korean characters should be preserved"
    );
    assert!(
        c.messages[4].content.contains("ภาษาไทย"),
        "Thai characters should be preserved"
    );

    // Title should handle Unicode
    assert!(
        c.title.as_ref().unwrap().contains("你好") || c.title.as_ref().unwrap().contains("Hello"),
        "title should preserve Unicode characters from first user message"
    );
}

#[test]
fn pi_agent_connector_handles_null_thinking_content() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--null-thinking--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_null_think.jsonl");

    // Test null thinking content (different from empty string)
    let sample = r#"{"type":"session","id":"null-thinking-test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude","thinkingLevel":"high"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"analyze this"}}
{"type":"message","timestamp":"2024-01-15T10:30:05.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":null},{"type":"text","text":"Here is my response"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation with null thinking content"
    );

    let c = &convs[0];
    assert_eq!(
        c.messages.len(),
        2,
        "expected 2 messages (user + assistant with null thinking)"
    );

    // The assistant message should still be parsed correctly with null thinking
    let assistant = &c.messages[1];
    assert!(
        assistant.content.contains("Here is my response"),
        "text content should be preserved even with null thinking"
    );
}

#[test]
fn pi_agent_connector_handles_tool_call_with_null_arguments() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/--null-args--");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("2024-01-15T10-30-00_null_args.jsonl");

    // Test tool calls with null arguments
    let sample = r#"{"type":"session","id":"null-args-test","timestamp":"2024-01-15T10:30:00.000Z","cwd":"/test","provider":"anthropic","modelId":"claude","thinkingLevel":"off"}
{"type":"message","timestamp":"2024-01-15T10:30:01.000Z","message":{"role":"user","content":"get status"}}
{"type":"message","timestamp":"2024-01-15T10:30:02.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_1","name":"get_status","arguments":null}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = PiAgentConnector::new();
    // qu81y: the tempdir pi-agent home is passed as an EXPLICIT scan root
    // instead of mutating process-global PI_CODING_AGENT_DIR.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation with null tool arguments"
    );

    let c = &convs[0];
    assert_eq!(
        c.messages.len(),
        2,
        "expected 2 messages (user + assistant with tool call)"
    );

    // Tool call with null arguments should still be parsed
    let assistant = &c.messages[1];
    assert!(
        assistant.content.contains("[Tool: get_status]"),
        "tool call should be formatted even with null arguments"
    );
}
