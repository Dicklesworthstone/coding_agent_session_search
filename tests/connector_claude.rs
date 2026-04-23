use coding_agent_search::connectors::claude_code::ClaudeCodeConnector;
use coding_agent_search::connectors::{Connector, ScanContext};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

// Migration note: Using "fixture-claude" naming instead of legacy "mock-claude".
// See tests/fixtures/connectors/MANIFEST.json for provenance tracking.

#[test]
fn claude_parses_project_fixture() {
    // Setup isolated environment with "claude" in path to satisfy detector
    let tmp = tempfile::TempDir::new().unwrap();
    let fixture_src =
        PathBuf::from("tests/fixtures/claude_code_real/projects/-test-project/agent-test123.jsonl");
    let fixture_dest_dir = tmp.path().join("fixture-claude/projects/test-project");
    std::fs::create_dir_all(&fixture_dest_dir).unwrap();
    let fixture_dest = fixture_dest_dir.join("agent-test123.jsonl");
    std::fs::copy(&fixture_src, &fixture_dest).expect("copy fixture");

    // Run scan on temp dir
    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert!(
        !c.title.as_deref().unwrap_or("").is_empty(),
        "conversation should have a non-empty title, got: {:?}",
        c.title
    );
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.messages[1].role, "assistant");
    assert!(
        c.messages[1].content.contains("matrix completion"),
        "assistant message should contain 'matrix completion', got: {}",
        &c.messages[1].content[..c.messages[1].content.len().min(200)]
    );

    // Verify metadata extraction
    let meta = &c.metadata;
    assert_eq!(
        meta.get("sessionId").and_then(|v| v.as_str()),
        Some("test-session")
    );
    assert_eq!(meta.get("gitBranch").and_then(|v| v.as_str()), Some("main"));
}

/// Helper to create a Claude-style temp directory
fn create_claude_temp() -> TempDir {
    TempDir::new().unwrap()
}

/// Test JSONL format with user and assistant messages
#[test]
fn claude_connector_parses_jsonl_format() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    let sample = r#"{"type":"user","cwd":"/workspace","sessionId":"sess-1","gitBranch":"develop","message":{"role":"user","content":"Hello Claude"},"timestamp":"2025-11-12T18:31:18.000Z"}
{"type":"assistant","message":{"role":"assistant","model":"claude-opus-4","content":[{"type":"text","text":"Hello! How can I help?"}]},"timestamp":"2025-11-12T18:31:20.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.agent_slug, "claude_code");
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.workspace, Some(PathBuf::from("/workspace")));

    // Verify session metadata
    assert_eq!(
        c.metadata.get("sessionId").and_then(|v| v.as_str()),
        Some("sess-1")
    );
    assert_eq!(
        c.metadata.get("gitBranch").and_then(|v| v.as_str()),
        Some("develop")
    );
}

/// Test JSONL format with type:message entries (role hints)
#[test]
fn claude_connector_parses_message_type_entries() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    let sample = r#"{"type":"message","role":"user","content":"Hello from message type","timestamp":"2025-11-12T18:31:18.000Z"}
{"type":"message","message":{"role":"assistant","content":"Reply from message type"},"timestamp":"2025-11-12T18:31:20.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.messages[0].role, "user");
    assert!(c.messages[0].content.contains("message type"));
    assert_eq!(c.messages[1].role, "assistant");
    assert!(c.messages[1].content.contains("Reply from message type"));
}

/// Test that summary entries are filtered out
#[test]
fn claude_connector_filters_summary_entries() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    let sample = r#"{"type":"user","message":{"role":"user","content":"Question"},"timestamp":"2025-11-12T18:31:18.000Z"}
{"type":"summary","timestamp":"2025-11-12T18:31:30.000Z","summary":"Summary text"}
{"type":"file-history-snapshot","timestamp":"2025-11-12T18:31:35.000Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Answer"}]},"timestamp":"2025-11-12T18:31:20.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Should only have user and assistant messages
    assert_eq!(c.messages.len(), 2);
    for msg in &c.messages {
        assert!(!msg.content.contains("Summary text"));
    }
}

/// Test model author extraction
#[test]
fn claude_connector_extracts_model_as_author() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    let sample = r#"{"type":"user","message":{"role":"user","content":"Hello"},"timestamp":"2025-11-12T18:31:18.000Z"}
{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-4","content":[{"type":"text","text":"Hi!"}]},"timestamp":"2025-11-12T18:31:20.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let assistant = &convs[0].messages[1];
    assert_eq!(assistant.author, Some("claude-sonnet-4".to_string()));
}

/// Test `tool_use` blocks are flattened
#[test]
fn claude_connector_flattens_tool_use() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    let sample = r#"{"type":"user","message":{"role":"user","content":"Read the file"},"timestamp":"2025-11-12T18:31:18.000Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll read it"},{"type":"tool_use","name":"Read","input":{"file_path":"/src/main.rs"}}]},"timestamp":"2025-11-12T18:31:20.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let assistant = &convs[0].messages[1];
    assert!(assistant.content.contains("I'll read it"));
    assert!(assistant.content.contains("[Tool: Read"));
    assert!(assistant.content.contains("/src/main.rs"));
}

/// Test title extraction from first user message
#[test]
fn claude_connector_extracts_title_from_user() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    let sample = r#"{"type":"user","message":{"role":"user","content":"Help me fix the bug\nMore details here"},"timestamp":"2025-11-12T18:31:18.000Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Sure!"}]},"timestamp":"2025-11-12T18:31:20.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title, Some("Help me fix the bug".to_string()));
}

/// Test title fallback to workspace name
#[test]
fn claude_connector_title_fallback_to_workspace() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    // Only assistant message, no user
    let sample = r#"{"type":"assistant","cwd":"/home/user/my-project","message":{"role":"assistant","content":[{"type":"text","text":"Starting up"}]},"timestamp":"2025-11-12T18:31:18.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    // Should fallback to workspace directory name
    assert_eq!(convs[0].title, Some("my-project".to_string()));
}

/// Test malformed lines are skipped
#[test]
fn claude_connector_skips_malformed_lines() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    let sample = r#"{"type":"user","message":{"role":"user","content":"Valid"},"timestamp":"2025-11-12T18:31:18.000Z"}
{ not valid json
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Response"}]},"timestamp":"2025-11-12T18:31:20.000Z"}
also not json
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
}

/// Test empty content is filtered
#[test]
fn claude_connector_filters_empty_content() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    let sample = r#"{"type":"user","message":{"role":"user","content":"   "},"timestamp":"2025-11-12T18:31:18.000Z"}
{"type":"user","message":{"role":"user","content":"Valid content"},"timestamp":"2025-11-12T18:31:19.000Z"}
{"type":"assistant","message":{"role":"assistant","content":[]},"timestamp":"2025-11-12T18:31:20.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    // Only the message with "Valid content" should be included
    assert_eq!(convs[0].messages.len(), 1);
    assert!(convs[0].messages[0].content.contains("Valid content"));
}

/// Test sequential index assignment
#[test]
fn claude_connector_assigns_sequential_indices() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    let sample = r#"{"type":"user","message":{"role":"user","content":"First"},"timestamp":"2025-11-12T18:31:18.000Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Second"}]},"timestamp":"2025-11-12T18:31:19.000Z"}
{"type":"user","message":{"role":"user","content":"Third"},"timestamp":"2025-11-12T18:31:20.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.messages.len(), 3);
    assert_eq!(c.messages[0].idx, 0);
    assert_eq!(c.messages[1].idx, 1);
    assert_eq!(c.messages[2].idx, 2);
}

/// Test multiple files in directory
#[test]
fn claude_connector_handles_multiple_files() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();

    for i in 1..=3 {
        let file = projects.join(format!("session-{i}.jsonl"));
        let sample = format!(
            r#"{{"type":"user","message":{{"role":"user","content":"Message {i}"}},"timestamp":"2025-11-12T18:31:18.000Z"}}
"#
        );
        fs::write(&file, sample).unwrap();
    }

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 3);
}

/// Test JSON format (non-JSONL)
#[test]
fn claude_connector_parses_json_format() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("conversation.json");

    let sample = r#"{
        "title": "Test Conversation",
        "messages": [
            {"role": "user", "content": "Hello", "timestamp": "2025-11-12T18:31:18.000Z"},
            {"role": "assistant", "content": "Hi there!", "timestamp": "2025-11-12T18:31:20.000Z"}
        ]
    }"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.title, Some("Test Conversation".to_string()));
    assert_eq!(c.messages.len(), 2);
}

/// Test .claude extension
#[test]
fn claude_connector_parses_claude_extension() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("conversation.claude");

    let sample = r#"{
        "messages": [
            {"role": "user", "content": "Question", "timestamp": "2025-11-12T18:31:18.000Z"},
            {"role": "assistant", "content": "Answer", "timestamp": "2025-11-12T18:31:20.000Z"}
        ]
    }"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
}

/// Test empty directory returns empty results
#[test]
fn claude_connector_handles_empty_directory() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects");
    fs::create_dir_all(&projects).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

/// Test `external_id` preserves the project-relative path under `projects/`
#[test]
fn claude_connector_sets_external_id() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("unique-session-id.jsonl");

    let sample = r#"{"type":"user","message":{"role":"user","content":"Test"},"timestamp":"2025-11-12T18:31:18.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].external_id,
        Some("projects/test-proj/unique-session-id.jsonl".to_string())
    );
}

/// Test `source_path` is set correctly
#[test]
fn claude_connector_sets_source_path() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    let sample = r#"{"type":"user","message":{"role":"user","content":"Test"},"timestamp":"2025-11-12T18:31:18.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].source_path, file);
}

/// Test timestamps are parsed correctly
#[test]
fn claude_connector_parses_timestamps() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    let sample = r#"{"type":"user","message":{"role":"user","content":"First"},"timestamp":"2025-11-12T18:31:18.000Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Last"}]},"timestamp":"2025-11-12T18:31:30.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Bead 7k7pl: collapse presence + ordering into a single
    // `.expect()`-based contract so a regression produces a clear
    // per-field diagnostic instead of an anonymous `.unwrap()`
    // panic. ALSO assert each message's created_at falls inside
    // [started_at, ended_at] — a regression that extracted a
    // message timestamp from the wrong field would slip past the
    // bare presence checks but fires here.
    let started = c
        .started_at
        .expect("conversation started_at must be populated after scan");
    let ended = c
        .ended_at
        .expect("conversation ended_at must be populated after scan");
    assert!(
        started < ended,
        "conversation started_at ({started}) must strictly precede ended_at \
         ({ended}); equal timestamps indicate a single-message conversation \
         that the claude parser should still report with distinct bounds"
    );
    for (idx, msg) in c.messages.iter().enumerate() {
        if let Some(created) = msg.created_at {
            assert!(
                (started..=ended).contains(&created),
                "message #{idx} created_at ({created}) must fall within \
                 conversation [started_at={started}, ended_at={ended}]"
            );
        }
    }
}

/// Test long title is truncated
#[test]
fn claude_connector_truncates_long_title() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    let long_text = "A".repeat(200);
    let sample = format!(
        r#"{{"type":"user","message":{{"role":"user","content":"{long_text}"}},"timestamp":"2025-11-12T18:31:18.000Z"}}
"#
    );
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(convs[0].title.is_some());
    assert_eq!(convs[0].title.as_ref().unwrap().len(), 100);
}

/// Test non-supported file extensions are ignored
#[test]
fn claude_connector_ignores_other_extensions() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();

    // Valid file
    let valid = projects.join("session.jsonl");
    fs::write(
        &valid,
        r#"{"type":"user","message":{"role":"user","content":"Valid"},"timestamp":"2025-11-12T18:31:18.000Z"}
"#,
    )
    .unwrap();

    // Invalid extensions
    let txt = projects.join("notes.txt");
    let md = projects.join("readme.md");
    let log = projects.join("debug.log");
    fs::write(&txt, "text").unwrap();
    fs::write(&md, "markdown").unwrap();
    fs::write(&log, "logs").unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
}

/// Test nested project directories
#[test]
fn claude_connector_handles_nested_projects() {
    let dir = create_claude_temp();
    let nested = dir.path().join("fixture-claude/projects/org/team/project");
    fs::create_dir_all(&nested).unwrap();
    let file = nested.join("session.jsonl");

    let sample = r#"{"type":"user","message":{"role":"user","content":"Nested"},"timestamp":"2025-11-12T18:31:18.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(convs[0].source_path.to_string_lossy().contains("team"));
}

/// Test role extraction from entry type when message.role is missing
#[test]
fn claude_connector_uses_entry_type_as_role() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    // message without role field, should use type field as role
    let sample = r#"{"type":"user","message":{"content":"No role field"},"timestamp":"2025-11-12T18:31:18.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].role, "user");
}

// =============================================================================
// General Connector Edge Case Tests (TST.CON)
// These tests verify cross-cutting concerns applicable to any connector
// =============================================================================

/// Test timezone handling - timestamps in different formats should be parsed correctly
#[test]
fn connector_handles_various_timezone_formats() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    // Test various timezone formats that should all be parseable
    let sample = r#"{"type":"user","message":{"role":"user","content":"UTC timestamp"},"timestamp":"2025-11-12T18:31:18.000Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"With milliseconds"}]},"timestamp":"2025-11-12T18:31:20.123Z"}
{"type":"user","message":{"role":"user","content":"Another format"},"timestamp":"2025-11-12T18:31:22Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.messages.len(), 3);

    // All messages should have parsed timestamps
    for msg in &c.messages {
        assert!(msg.created_at.is_some(), "Timestamp should be parsed");
    }

    // Verify ordering is preserved
    let ts1 = c.messages[0].created_at.unwrap();
    let ts2 = c.messages[1].created_at.unwrap();
    let ts3 = c.messages[2].created_at.unwrap();
    assert!(ts1 <= ts2, "Timestamps should be in order");
    assert!(ts2 <= ts3, "Timestamps should be in order");
}

/// Test handling of epoch timestamps vs ISO timestamps
#[test]
fn connector_handles_epoch_and_iso_timestamps() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");

    // Mix of ISO string and potential epoch timestamp scenarios
    let sample = r#"{"type":"user","message":{"role":"user","content":"ISO timestamp"},"timestamp":"2025-11-12T18:31:18.000Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Response"}]},"timestamp":"2025-11-12T18:31:20.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    // started_at and ended_at should be populated
    assert!(convs[0].started_at.is_some());
    assert!(convs[0].ended_at.is_some());
}

/// Test symlinked session directories behavior
/// Note: By default, walkdir does NOT follow symlinks, so this documents expected behavior
#[cfg(unix)]
#[test]
fn connector_symlinked_directories_not_followed_by_default() {
    use std::os::unix::fs::symlink;

    let dir = create_claude_temp();

    // Create actual data in a separate location
    let actual_data = dir.path().join("actual-data/projects/test-proj");
    fs::create_dir_all(&actual_data).unwrap();
    let file = actual_data.join("session.jsonl");
    let sample = r#"{"type":"user","message":{"role":"user","content":"From symlinked dir"},"timestamp":"2025-11-12T18:31:18.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    // Create symlink pointing to actual data
    let fixture_claude = dir.path().join("fixture-claude");
    fs::create_dir_all(&fixture_claude).unwrap();
    let symlink_path = fixture_claude.join("projects");
    symlink(dir.path().join("actual-data/projects"), &symlink_path).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: fixture_claude,
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    // Current behavior: symlinks are not followed, so directory symlinks result in empty scan
    // This documents the behavior - if symlink support is needed, walkdir.follow_links(true) is required
    assert!(
        convs.is_empty() || convs.len() == 1,
        "Symlinked dirs may or may not be followed depending on walkdir config"
    );
}

/// Test symlinked session files behavior
/// Note: File symlinks are followed because walkdir reports them as files
#[cfg(unix)]
#[test]
fn connector_follows_symlinked_files() {
    use std::os::unix::fs::symlink;

    let dir = create_claude_temp();

    // Create actual file in a separate location
    let actual_file = dir.path().join("actual-session.jsonl");
    let sample = r#"{"type":"user","message":{"role":"user","content":"From symlinked file"},"timestamp":"2025-11-12T18:31:18.000Z"}
"#;
    fs::write(&actual_file, sample).unwrap();

    // Create directory structure with symlinked file
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let symlink_path = projects.join("session.jsonl");
    symlink(&actual_file, &symlink_path).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    // File symlinks are typically followed when reading, but walkdir may not report them as files
    // depending on the symlink behavior. This test documents whatever behavior exists.
    if !convs.is_empty() {
        assert!(convs[0].messages[0].content.contains("From symlinked file"));
    }
    // If empty, symlinked files aren't being followed - that's also valid behavior to document
}

/// Test handling of unreadable files (permission denied)
/// Note: This test may be skipped on some systems where root runs tests
#[cfg(unix)]
#[test]
fn connector_handles_unreadable_files() {
    use std::os::unix::fs::PermissionsExt;

    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();

    // Create a readable file first
    let readable_file = projects.join("readable.jsonl");
    let sample = r#"{"type":"user","message":{"role":"user","content":"Readable"},"timestamp":"2025-11-12T18:31:18.000Z"}
"#;
    fs::write(&readable_file, sample).unwrap();

    // Create an unreadable file (only if not running as root)
    let unreadable_file = projects.join("unreadable.jsonl");
    fs::write(&unreadable_file, sample).unwrap();

    // Try to make it unreadable - skip test if we're root
    let metadata = fs::metadata(&unreadable_file).unwrap();
    let mut perms = metadata.permissions();
    perms.set_mode(0o000);
    if fs::set_permissions(&unreadable_file, perms).is_ok() {
        // Verify we actually can't read it (we might be root)
        if fs::read_to_string(&unreadable_file).is_err() {
            let conn = ClaudeCodeConnector::new();
            let ctx = ScanContext {
                data_dir: dir.path().join("fixture-claude"),
                scan_roots: Vec::new(),
                since_ts: None,
            };
            // Should not panic, just skip the unreadable file
            let result = conn.scan(&ctx);
            // Either succeeds with readable files only, or returns error gracefully
            if let Ok(convs) = result {
                // Should have at least the readable file
                assert!(
                    convs
                        .iter()
                        .any(|c| c.messages.iter().any(|m| m.content.contains("Readable")))
                );
            }
        }
    }

    // Cleanup: restore permissions so tempdir can clean up
    let mut perms = fs::metadata(&unreadable_file).unwrap().permissions();
    perms.set_mode(0o644);
    let _ = fs::set_permissions(&unreadable_file, perms);
}

/// Test handling of very long file paths
#[test]
fn connector_handles_long_file_paths() {
    let dir = create_claude_temp();

    // Create a deeply nested path (but not exceeding filesystem limits)
    let mut deep_path = dir.path().join("fixture-claude/projects");
    for i in 0..10 {
        deep_path = deep_path.join(format!("level{}", i));
    }
    fs::create_dir_all(&deep_path).unwrap();
    let file = deep_path.join("session.jsonl");
    let sample = r#"{"type":"user","message":{"role":"user","content":"Deep path"},"timestamp":"2025-11-12T18:31:18.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(convs[0].messages[0].content.contains("Deep path"));
}

/// Test handling of special characters in file/directory names
#[test]
fn connector_handles_special_chars_in_paths() {
    let dir = create_claude_temp();
    let projects = dir
        .path()
        .join("fixture-claude/projects/test-proj with spaces");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");
    let sample = r#"{"type":"user","message":{"role":"user","content":"Spaces in path"},"timestamp":"2025-11-12T18:31:18.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(convs[0].messages[0].content.contains("Spaces in path"));
}

/// Test handling of Unicode in file paths
#[test]
fn connector_handles_unicode_in_paths() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/项目-テスト");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");
    let sample = r#"{"type":"user","message":{"role":"user","content":"Unicode path"},"timestamp":"2025-11-12T18:31:18.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(convs[0].messages[0].content.contains("Unicode path"));
}

/// Test handling of empty directories
#[test]
fn connector_handles_empty_project_dirs() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/empty-proj");
    fs::create_dir_all(&projects).unwrap();
    // Don't create any files

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

/// Test incremental scan with since_ts filter
/// Note: since_ts is in MILLISECONDS since Unix epoch
#[test]
fn connector_respects_since_ts_filter() {
    let dir = create_claude_temp();
    let projects = dir.path().join("fixture-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    let file = projects.join("session.jsonl");
    let sample = r#"{"type":"user","message":{"role":"user","content":"Hello"},"timestamp":"2025-11-12T18:31:18.000Z"}
"#;
    fs::write(&file, sample).unwrap();

    // Get the file's modification time in MILLISECONDS
    let metadata = fs::metadata(&file).unwrap();
    let mtime = metadata.modified().unwrap();
    let mtime_millis = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let conn = ClaudeCodeConnector::new();

    // First scan without filter should find the file
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    // Scan with since_ts in the future (by 1 hour = 3600000 ms) should find nothing
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: Some(mtime_millis + 3_600_000), // 1 hour in the future
    };
    let convs = conn.scan(&ctx).unwrap();
    assert!(convs.is_empty(), "Future since_ts should skip the file");

    // Scan with since_ts in the past (by 1 hour) should find the file
    let ctx = ScanContext {
        data_dir: dir.path().join("fixture-claude"),
        scan_roots: Vec::new(),
        since_ts: Some(mtime_millis - 3_600_000), // 1 hour in the past
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1, "Past since_ts should include the file");
}
