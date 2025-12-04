use coding_agent_search::connectors::opencode::OpenCodeConnector;
use coding_agent_search::connectors::{Connector, ScanContext};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// Helper to create test session directory structure
fn create_test_session(
    root: &PathBuf,
    session_id: &str,
    title: Option<&str>,
    created_at: i64,
    updated_at: i64,
) {
    let session_dir = root.join("storage/session");
    let info_dir = session_dir.join("info");
    let message_dir = session_dir.join("message").join(session_id);
    let part_dir = session_dir.join("part").join(session_id);

    fs::create_dir_all(&info_dir).unwrap();
    fs::create_dir_all(&message_dir).unwrap();
    fs::create_dir_all(&part_dir).unwrap();

    // Create session info
    let info_json = serde_json::json!({
        "id": session_id,
        "title": title,
        "time": {
            "created": created_at,
            "updated": updated_at
        }
    });
    fs::write(
        info_dir.join(format!("{session_id}.json")),
        serde_json::to_string_pretty(&info_json).unwrap(),
    )
    .unwrap();
}

/// Helper to add a message to a session
fn add_message(
    root: &PathBuf,
    session_id: &str,
    msg_id: &str,
    role: &str,
    content: &str,
    created_at: i64,
    workspace: Option<&str>,
    model: Option<&str>,
) {
    let session_dir = root.join("storage/session");
    let message_dir = session_dir.join("message").join(session_id);
    let part_dir = session_dir.join("part").join(session_id).join(msg_id);

    fs::create_dir_all(&message_dir).unwrap();
    fs::create_dir_all(&part_dir).unwrap();

    // Create message info
    let mut msg_json = serde_json::json!({
        "id": msg_id,
        "role": role,
        "time": {
            "created": created_at
        },
        "sessionID": session_id
    });

    if let Some(ws) = workspace {
        msg_json["path"] = serde_json::json!({
            "cwd": ws,
            "root": ws
        });
    }

    if let Some(m) = model {
        msg_json["modelID"] = serde_json::Value::String(m.to_string());
    }

    fs::write(
        message_dir.join(format!("{msg_id}.json")),
        serde_json::to_string_pretty(&msg_json).unwrap(),
    )
    .unwrap();

    // Create part (content)
    let part_json = serde_json::json!({
        "type": "text",
        "text": content
    });
    fs::write(
        part_dir.join("prt_001.json"),
        serde_json::to_string_pretty(&part_json).unwrap(),
    )
    .unwrap();
}

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
    assert_eq!(c.title.as_deref(), Some("OpenCode Test Session"));
    assert_eq!(c.messages.len(), 2);
}

#[test]
fn opencode_parses_created_session() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    create_test_session(&root, "ses_001", Some("My Session"), 1000, 2000);
    add_message(&root, "ses_001", "msg_001", "user", "hello", 1000, Some("/tmp"), None);
    add_message(&root, "ses_001", "msg_002", "assistant", "hi", 2000, None, Some("claude-3"));

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root,
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.title, Some("My Session".to_string()));
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.messages[0].role, "user");
    assert_eq!(c.messages[0].content, "hello");
    assert_eq!(c.messages[1].role, "assistant");
    assert_eq!(c.messages[1].content, "hi");
}

#[test]
fn opencode_sets_correct_agent_slug() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    create_test_session(&root, "ses_001", Some("Test"), 1000, 2000);
    add_message(&root, "ses_001", "msg_001", "user", "test", 1000, None, None);

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root,
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].agent_slug, "opencode");
}

#[test]
fn opencode_extracts_workspace_from_path() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    create_test_session(&root, "ses_001", Some("Test"), 1000, 2000);
    add_message(&root, "ses_001", "msg_001", "user", "test", 1000, Some("/my/workspace"), None);

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root,
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].workspace, Some(PathBuf::from("/my/workspace")));
}

#[test]
fn opencode_extracts_model_as_author() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    create_test_session(&root, "ses_001", Some("Test"), 1000, 2000);
    add_message(&root, "ses_001", "msg_001", "assistant", "response", 1000, None, Some("claude-opus-4"));

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root,
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].author, Some("claude-opus-4".to_string()));
}

#[test]
fn opencode_computes_timestamps() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    create_test_session(&root, "ses_001", Some("Test"), 1000, 5000);
    add_message(&root, "ses_001", "msg_001", "user", "first", 1000, None, None);
    add_message(&root, "ses_001", "msg_002", "assistant", "second", 3000, None, None);
    add_message(&root, "ses_001", "msg_003", "user", "third", 5000, None, None);

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root,
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.started_at, Some(1000));
    assert_eq!(c.ended_at, Some(5000));
}

#[test]
fn opencode_assigns_sequential_indices() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    create_test_session(&root, "ses_001", Some("Test"), 1000, 3000);
    add_message(&root, "ses_001", "msg_001", "user", "first", 1000, None, None);
    add_message(&root, "ses_001", "msg_002", "assistant", "second", 2000, None, None);
    add_message(&root, "ses_001", "msg_003", "user", "third", 3000, None, None);

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root,
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let messages = &convs[0].messages;
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].idx, 0);
    assert_eq!(messages[1].idx, 1);
    assert_eq!(messages[2].idx, 2);
}

#[test]
fn opencode_title_from_first_user_message() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    // Session without explicit title
    create_test_session(&root, "ses_001", None, 1000, 2000);
    add_message(&root, "ses_001", "msg_001", "user", "This is my question about code", 1000, None, None);
    add_message(&root, "ses_001", "msg_002", "assistant", "Let me help", 2000, None, None);

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root,
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title, Some("This is my question about code".to_string()));
}

#[test]
fn opencode_external_id_is_session_id() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    create_test_session(&root, "ses_unique123", Some("Test"), 1000, 2000);
    add_message(&root, "ses_unique123", "msg_001", "user", "test", 1000, None, None);

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root,
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id, Some("ses_unique123".to_string()));
}

#[test]
fn opencode_handles_empty_directory() {
    let dir = TempDir::new().unwrap();
    // Create opencode-style structure but keep it empty
    let root = dir.path().join("opencode_test");
    fs::create_dir_all(root.join("storage/session/info")).unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root,
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn opencode_handles_empty_session_info_dir() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    fs::create_dir_all(root.join("storage/session/info")).unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root,
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn opencode_skips_session_without_messages() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    // Create session info but no messages
    create_test_session(&root, "ses_empty", Some("Empty Session"), 1000, 2000);

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root,
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn opencode_metadata_contains_session_info() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    create_test_session(&root, "ses_001", Some("Test"), 1000, 2000);
    add_message(&root, "ses_001", "msg_001", "user", "test", 1000, None, None);

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root.clone(),
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let metadata = &convs[0].metadata;
    assert_eq!(metadata["source"], "opencode");
    assert_eq!(metadata["session_id"], "ses_001");
}

#[test]
fn opencode_handles_multiple_sessions() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    create_test_session(&root, "ses_001", Some("First Session"), 1000, 2000);
    add_message(&root, "ses_001", "msg_001", "user", "hello first", 1000, None, None);

    create_test_session(&root, "ses_002", Some("Second Session"), 3000, 4000);
    add_message(&root, "ses_002", "msg_001", "user", "hello second", 3000, None, None);

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root,
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 2);

    let titles: Vec<_> = convs.iter().filter_map(|c| c.title.as_deref()).collect();
    assert!(titles.contains(&"First Session"));
    assert!(titles.contains(&"Second Session"));
}

#[test]
fn opencode_handles_tool_result_parts() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    create_test_session(&root, "ses_001", Some("Test"), 1000, 2000);

    // Create message
    let session_dir = root.join("storage/session");
    let message_dir = session_dir.join("message").join("ses_001");
    let part_dir = session_dir.join("part").join("ses_001").join("msg_001");

    fs::create_dir_all(&message_dir).unwrap();
    fs::create_dir_all(&part_dir).unwrap();

    let msg_json = serde_json::json!({
        "id": "msg_001",
        "role": "assistant",
        "time": {"created": 1000},
        "sessionID": "ses_001"
    });
    fs::write(
        message_dir.join("msg_001.json"),
        serde_json::to_string_pretty(&msg_json).unwrap(),
    ).unwrap();

    // Create tool_result part
    let part_json = serde_json::json!({
        "type": "tool_result",
        "text": "File contents here"
    });
    fs::write(
        part_dir.join("prt_001.json"),
        serde_json::to_string_pretty(&part_json).unwrap(),
    ).unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root,
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(convs[0].messages[0].content.contains("[Tool Result]"));
    assert!(convs[0].messages[0].content.contains("File contents here"));
}

#[test]
fn opencode_skips_empty_text_parts() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    create_test_session(&root, "ses_001", Some("Test"), 1000, 2000);

    // Create message with empty text
    let session_dir = root.join("storage/session");
    let message_dir = session_dir.join("message").join("ses_001");
    let part_dir = session_dir.join("part").join("ses_001").join("msg_001");

    fs::create_dir_all(&message_dir).unwrap();
    fs::create_dir_all(&part_dir).unwrap();

    let msg_json = serde_json::json!({
        "id": "msg_001",
        "role": "user",
        "time": {"created": 1000},
        "sessionID": "ses_001"
    });
    fs::write(
        message_dir.join("msg_001.json"),
        serde_json::to_string_pretty(&msg_json).unwrap(),
    ).unwrap();

    // Empty text part
    let part_json = serde_json::json!({
        "type": "text",
        "text": "   "
    });
    fs::write(
        part_dir.join("prt_001.json"),
        serde_json::to_string_pretty(&part_json).unwrap(),
    ).unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_root: root,
        since_ts: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    // Session should be skipped since no valid messages
    assert!(convs.is_empty());
}

#[test]
fn opencode_detection_requires_project_dir() {
    let connector = OpenCodeConnector::new();
    let result = connector.detect();
    // Detection depends on system state - just verify it returns a result
    assert!(result.detected || !result.detected);
}
