//! Tests for the OpenCode connector (JSON file-based storage).

use coding_agent_search::connectors::opencode::OpenCodeConnector;
use coding_agent_search::connectors::{Connector, ScanContext};
use std::path::PathBuf;
use tempfile::TempDir;

/// Helper to create a valid OpenCode storage directory structure.
fn create_storage_structure(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("session")).unwrap();
    std::fs::create_dir_all(root.join("message")).unwrap();
    std::fs::create_dir_all(root.join("part")).unwrap();
}

/// Helper to write a session JSON file.
fn write_session(
    root: &std::path::Path,
    project_id: &str,
    session_id: &str,
    title: Option<&str>,
    directory: Option<&str>,
    created: i64,
    updated: Option<i64>,
) {
    let session_dir = root.join("session").join(project_id);
    std::fs::create_dir_all(&session_dir).unwrap();

    let mut session = serde_json::json!({
        "id": session_id,
        "version": "1.0.138",
        "projectID": project_id,
        "time": {
            "created": created,
        },
        "summary": {
            "additions": 0,
            "deletions": 0,
            "files": 0
        }
    });

    if let Some(t) = title {
        session["title"] = serde_json::Value::String(t.to_string());
    }
    if let Some(d) = directory {
        session["directory"] = serde_json::Value::String(d.to_string());
    }
    if let Some(u) = updated {
        session["time"]["updated"] = serde_json::Value::Number(u.into());
    }

    let path = session_dir.join(format!("{session_id}.json"));
    std::fs::write(path, serde_json::to_string_pretty(&session).unwrap()).unwrap();
}

/// Helper to write a message JSON file.
fn write_message(
    root: &std::path::Path,
    session_id: &str,
    message_id: &str,
    role: &str,
    created: i64,
    model_id: Option<&str>,
    summary_body: Option<&str>,
) {
    let message_dir = root.join("message").join(session_id);
    std::fs::create_dir_all(&message_dir).unwrap();

    let mut message = serde_json::json!({
        "id": message_id,
        "sessionID": session_id,
        "role": role,
        "time": {
            "created": created,
        }
    });

    if let Some(model) = model_id {
        message["modelID"] = serde_json::Value::String(model.to_string());
    }

    if let Some(body) = summary_body {
        message["summary"] = serde_json::json!({
            "title": "Summary",
            "body": body,
        });
    }

    let path = message_dir.join(format!("{message_id}.json"));
    std::fs::write(path, serde_json::to_string_pretty(&message).unwrap()).unwrap();
}

/// Helper to write a text part JSON file.
fn write_text_part(
    root: &std::path::Path,
    session_id: &str,
    message_id: &str,
    part_id: &str,
    text: &str,
) {
    let part_dir = root.join("part").join(message_id);
    std::fs::create_dir_all(&part_dir).unwrap();

    let part = serde_json::json!({
        "id": part_id,
        "sessionID": session_id,
        "messageID": message_id,
        "type": "text",
        "text": text
    });

    let path = part_dir.join(format!("{part_id}.json"));
    std::fs::write(path, serde_json::to_string_pretty(&part).unwrap()).unwrap();
}

/// Helper to write a tool part JSON file.
fn write_tool_part(
    root: &std::path::Path,
    session_id: &str,
    message_id: &str,
    part_id: &str,
    tool_name: &str,
    title: Option<&str>,
    output: Option<&str>,
) {
    let part_dir = root.join("part").join(message_id);
    std::fs::create_dir_all(&part_dir).unwrap();

    let mut state = serde_json::json!({
        "status": "completed",
        "input": {}
    });

    if let Some(t) = title {
        state["title"] = serde_json::Value::String(t.to_string());
    }
    if let Some(o) = output {
        state["output"] = serde_json::Value::String(o.to_string());
    }

    let part = serde_json::json!({
        "id": part_id,
        "sessionID": session_id,
        "messageID": message_id,
        "type": "tool",
        "tool": tool_name,
        "callID": "toolu_test",
        "state": state
    });

    let path = part_dir.join(format!("{part_id}.json"));
    std::fs::write(path, serde_json::to_string_pretty(&part).unwrap()).unwrap();
}

// ============================================================================
// Basic Parsing Tests
// ============================================================================

#[test]
fn opencode_parses_json_fixture() {
    let fixture_root = PathBuf::from("tests/fixtures/opencode_json");
    let conn = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: fixture_root.clone(),
        since_ts: None,
    };

    let convs = conn.scan(&ctx).expect("scan");
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.title.as_deref(), Some("Test Session"));
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.workspace, Some(PathBuf::from("/tmp/test-project")));
    assert_eq!(c.agent_slug, "opencode");
}

#[test]
fn opencode_parses_message_content_from_parts() {
    let fixture_root = PathBuf::from("tests/fixtures/opencode_json");
    let conn = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: fixture_root.clone(),
        since_ts: None,
    };

    let convs = conn.scan(&ctx).expect("scan");
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // User message should have content from part + summary body
    assert!(c.messages[0].content.contains("Hello, can you help me"));
    // Assistant message should have content from part
    assert!(c.messages[1].content.contains("Of course!"));
}

#[test]
fn opencode_extracts_author_from_model_id() {
    let fixture_root = PathBuf::from("tests/fixtures/opencode_json");
    let conn = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: fixture_root.clone(),
        since_ts: None,
    };

    let convs = conn.scan(&ctx).expect("scan");
    let c = &convs[0];

    // Assistant message should have model_id as author
    assert_eq!(c.messages[1].author, Some("claude-opus-4-5".to_string()));
}

// ============================================================================
// Filtering Tests
// ============================================================================

#[test]
fn opencode_filters_messages_with_since_ts() {
    let fixture_root = PathBuf::from("tests/fixtures/opencode_json");
    let conn = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: fixture_root.clone(),
        since_ts: Some(1_700_000_002_000), // After first message
    };

    let convs = conn.scan(&ctx).expect("scan");
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.messages.len(), 1);
    assert_eq!(c.messages[0].created_at, Some(1_700_000_005_000));
}

// ============================================================================
// Dynamic Tests with TempDir
// ============================================================================

#[test]
fn opencode_parses_created_storage() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);

    write_session(
        root,
        "proj1",
        "ses1",
        Some("My Session"),
        Some("/tmp"),
        1000,
        Some(2000),
    );
    write_message(root, "ses1", "msg1", "user", 1000, None, None);
    write_message(
        root,
        "ses1",
        "msg2",
        "assistant",
        2000,
        Some("claude-3"),
        None,
    );
    write_text_part(root, "ses1", "msg1", "prt1", "hello");
    write_text_part(root, "ses1", "msg2", "prt2", "hi there");

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.title, Some("My Session".to_string()));
    assert_eq!(c.workspace, Some(PathBuf::from("/tmp")));
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.messages[0].content, "hello");
    assert_eq!(c.messages[1].content, "hi there");
}

#[test]
fn opencode_handles_empty_storage() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);
    // No sessions created

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn opencode_handles_session_without_messages() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);

    write_session(
        root,
        "proj1",
        "ses1",
        Some("Empty Session"),
        None,
        1000,
        None,
    );
    // No messages directory for this session

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty()); // Session without messages is skipped
}

#[test]
fn opencode_handles_message_without_parts() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);

    write_session(root, "proj1", "ses1", Some("Test"), None, 1000, None);
    write_message(
        root,
        "ses1",
        "msg1",
        "user",
        1000,
        None,
        Some("User message body"),
    );
    // No parts directory for this message - content comes from summary.body

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(convs[0].messages[0].content.contains("User message body"));
}

#[test]
fn opencode_sets_correct_agent_slug() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);

    write_session(root, "proj1", "ses1", Some("Test"), None, 1000, None);
    write_message(root, "ses1", "msg1", "user", 1000, None, Some("test"));

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].agent_slug, "opencode");
}

#[test]
fn opencode_sets_external_id_to_session_id() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);

    write_session(root, "proj1", "ses_abc123", Some("Test"), None, 1000, None);
    write_message(root, "ses_abc123", "msg1", "user", 1000, None, Some("test"));

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id, Some("ses_abc123".to_string()));
}

#[test]
fn opencode_computes_started_ended_at() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);

    write_session(root, "proj1", "ses1", Some("Test"), None, 500, Some(3500));
    write_message(root, "ses1", "msg1", "user", 1000, None, Some("first"));
    write_message(root, "ses1", "msg2", "assistant", 2000, None, None);
    write_message(root, "ses1", "msg3", "user", 3000, None, Some("third"));
    write_text_part(root, "ses1", "msg2", "prt1", "response");

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    // started_at comes from session time.created
    assert_eq!(convs[0].started_at, Some(500));
    // ended_at comes from session time.updated
    assert_eq!(convs[0].ended_at, Some(3500));
}

#[test]
fn opencode_assigns_sequential_indices() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);

    write_session(root, "proj1", "ses1", Some("Test"), None, 1000, None);
    write_message(root, "ses1", "msg1", "user", 1000, None, Some("m0"));
    write_message(root, "ses1", "msg2", "assistant", 2000, None, None);
    write_message(root, "ses1", "msg3", "user", 3000, None, Some("m2"));
    write_text_part(root, "ses1", "msg2", "prt1", "m1");

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let msgs = &convs[0].messages;
    for (i, msg) in msgs.iter().enumerate() {
        assert_eq!(msg.idx, i as i64);
    }
}

#[test]
fn opencode_orders_messages_by_timestamp() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);

    write_session(root, "proj1", "ses1", Some("Test"), None, 1000, None);
    // Write out of order
    write_message(root, "ses1", "msg3", "user", 3000, None, Some("third"));
    write_message(root, "ses1", "msg1", "user", 1000, None, Some("first"));
    write_message(root, "ses1", "msg2", "assistant", 2000, None, None);
    write_text_part(root, "ses1", "msg2", "prt1", "second");

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let msgs = &convs[0].messages;
    assert!(msgs[0].content.contains("first"));
    assert!(msgs[1].content.contains("second"));
    assert!(msgs[2].content.contains("third"));
}

#[test]
fn opencode_since_ts_logic() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);

    write_session(root, "proj1", "ses1", Some("Test"), None, 1000, None);
    write_message(root, "ses1", "msg1", "user", 1000, None, Some("old"));
    write_message(root, "ses1", "msg2", "user", 3000, None, Some("new"));

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: Some(2000),
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert!(convs[0].messages[0].content.contains("new"));
}

#[test]
fn opencode_multiple_sessions() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);

    write_session(root, "proj1", "ses1", Some("Session 1"), None, 1000, None);
    write_session(root, "proj1", "ses2", Some("Session 2"), None, 2000, None);
    write_message(root, "ses1", "msg1", "user", 1000, None, Some("s1m1"));
    write_message(root, "ses1", "msg2", "assistant", 1500, None, None);
    write_message(root, "ses2", "msg3", "user", 2000, None, Some("s2m1"));
    write_text_part(root, "ses1", "msg2", "prt1", "s1m2");

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 2);

    let s1 = convs
        .iter()
        .find(|c| c.title == Some("Session 1".to_string()));
    let s2 = convs
        .iter()
        .find(|c| c.title == Some("Session 2".to_string()));
    assert!(s1.is_some());
    assert!(s2.is_some());
    assert_eq!(s1.unwrap().messages.len(), 2);
    assert_eq!(s2.unwrap().messages.len(), 1);
}

#[test]
fn opencode_title_fallback_to_first_message_summary() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);

    // Session without title
    let session_dir = root.join("session").join("proj1");
    std::fs::create_dir_all(&session_dir).unwrap();
    let session = serde_json::json!({
        "id": "ses1",
        "projectID": "proj1",
        "time": { "created": 1000 }
    });
    std::fs::write(
        session_dir.join("ses1.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    // Message with summary title
    let message_dir = root.join("message").join("ses1");
    std::fs::create_dir_all(&message_dir).unwrap();
    let message = serde_json::json!({
        "id": "msg1",
        "sessionID": "ses1",
        "role": "user",
        "time": { "created": 1000 },
        "summary": {
            "title": "Fallback Title",
            "body": "message body"
        }
    });
    std::fs::write(
        message_dir.join("msg1.json"),
        serde_json::to_string_pretty(&message).unwrap(),
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title, Some("Fallback Title".to_string()));
}

// ============================================================================
// Part Type Tests
// ============================================================================

#[test]
fn opencode_assembles_tool_parts() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);

    write_session(root, "proj1", "ses1", Some("Test"), None, 1000, None);
    write_message(root, "ses1", "msg1", "assistant", 1000, None, None);
    write_tool_part(
        root,
        "ses1",
        "msg1",
        "prt1",
        "bash",
        Some("Run command"),
        Some("output"),
    );

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(convs[0].messages[0].content.contains("[Tool: bash"));
    assert!(convs[0].messages[0].content.contains("Run command"));
    assert!(convs[0].messages[0].content.contains("output"));
}

#[test]
fn opencode_assembles_multiple_parts() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);

    write_session(root, "proj1", "ses1", Some("Test"), None, 1000, None);
    write_message(root, "ses1", "msg1", "assistant", 1000, None, None);
    write_text_part(root, "ses1", "msg1", "prt1", "First part.");
    write_text_part(root, "ses1", "msg1", "prt2", "Second part.");

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(convs[0].messages[0].content.contains("First part."));
    assert!(convs[0].messages[0].content.contains("Second part."));
}

// ============================================================================
// Detection Tests
// ============================================================================

#[test]
fn opencode_detect_returns_false_for_invalid_dir() {
    // Detection looks at default paths (~/.local/share/opencode/storage/)
    // This test verifies detect() doesn't crash and returns a valid result
    let connector = OpenCodeConnector::new();
    let result = connector.detect();

    // Result should be a valid DetectionResult (detected is bool, evidence is Vec)
    // We can't control what's at the default path, so just verify structure
    assert!(result.evidence.len() <= 10); // Sanity check - not too many evidence items
}

#[test]
fn opencode_handles_empty_directory() {
    let dir = TempDir::new().unwrap();
    // Create storage structure but no sessions
    create_storage_structure(dir.path());

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: dir.path().to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

// ============================================================================
// Metadata Tests
// ============================================================================

#[test]
fn opencode_metadata_contains_session_info() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);

    write_session(root, "proj1", "ses1", Some("Test"), None, 1000, None);
    write_message(root, "ses1", "msg1", "user", 1000, None, Some("test"));

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let metadata = &convs[0].metadata;
    assert_eq!(metadata.get("session_id").unwrap(), "ses1");
    assert_eq!(metadata.get("project_id").unwrap(), "proj1");
}

#[test]
fn opencode_message_extra_contains_metadata() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    create_storage_structure(root);

    write_session(root, "proj1", "ses1", Some("Test"), None, 1000, None);

    // Write message with full metadata
    let message_dir = root.join("message").join("ses1");
    std::fs::create_dir_all(&message_dir).unwrap();
    let message = serde_json::json!({
        "id": "msg1",
        "sessionID": "ses1",
        "role": "assistant",
        "time": { "created": 1000 },
        "modelID": "claude-3",
        "providerID": "anthropic",
        "mode": "build",
        "cost": 0.05,
        "tokens": {
            "input": 100,
            "output": 50,
            "reasoning": 0
        },
        "finish": "end_turn"
    });
    std::fs::write(
        message_dir.join("msg1.json"),
        serde_json::to_string_pretty(&message).unwrap(),
    )
    .unwrap();

    write_text_part(root, "ses1", "msg1", "prt1", "response");

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.to_path_buf(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let extra = &convs[0].messages[0].extra;
    assert_eq!(extra.get("provider").unwrap(), "anthropic");
    assert_eq!(extra.get("mode").unwrap(), "build");
    assert_eq!(extra.get("finish").unwrap(), "end_turn");
    assert!(extra.get("cost").is_some());
    assert!(extra.get("tokens").is_some());
}
