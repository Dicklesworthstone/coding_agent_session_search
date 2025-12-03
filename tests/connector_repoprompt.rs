use std::fs;
use std::path::PathBuf;

use coding_agent_search::connectors::{Connector, ScanContext, repoprompt::RepoPromptConnector};
use serial_test::serial;
use tempfile::TempDir;

#[test]
#[serial]
fn repoprompt_connector_reads_session_json() {
    let dir = TempDir::new().unwrap();
    let workspaces_root = dir.path().join("RepoPrompt").join("Workspaces");
    let workspace_dir = workspaces_root.join("Workspace-test");
    let chats_dir = workspace_dir.join("Chats");
    fs::create_dir_all(&chats_dir).unwrap();
    let file = chats_dir.join("ChatSession-abc.json");

    let session = serde_json::json!({
        "id": "chat-1",
        "shortID": "short-1",
        "name": "Test Chat",
        "workspaceID": "WS-123",
        "preferredAIModel": "repoprompt-model",
        "selectedFilePaths": [
            "/tmp/project/README.md",
            "/tmp/project/src/main.rs",
        ],
        "messages": [
            {
                "sequenceIndex": 0,
                "rawText": "user question",
                "isUser": true,
                "timestamp": 784523530.0,
                "allowedFilePaths": serde_json::Value::Null,
                "modelName": serde_json::Value::Null,
                "delegateResults": [],
                "id": "msg-1",
            },
            {
                "sequenceIndex": 1,
                "rawText": "assistant answer",
                "isUser": false,
                "timestamp": 784523531.0,
                "allowedFilePaths": [
                    "/tmp/project/README.md",
                    "/tmp/project/src/main.rs",
                ],
                "modelName": "GPT-X",
                "delegateResults": [],
                "id": "msg-2",
            },
        ],
        "savedAt": 784523532.0,
    });
    fs::write(&file, serde_json::to_string(&session).unwrap()).unwrap();

    unsafe {
        std::env::set_var("REPOPROMPT_HOME", &workspaces_root);
    }

    let connector = RepoPromptConnector::new();
    let ctx = ScanContext {
        data_root: workspaces_root.clone(),
        since_ts: None,
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];

    assert_eq!(c.agent_slug, "repoprompt");
    assert_eq!(c.external_id, Some("chat-1".to_string()));
    assert_eq!(c.title, Some("Test Chat".to_string()));
    assert_eq!(c.workspace, Some(PathBuf::from("/tmp/project")));
    assert_eq!(c.source_path, file);
    assert_eq!(
        c.metadata.get("source").and_then(|v| v.as_str()),
        Some("repoprompt")
    );

    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.messages[0].role, "user");
    assert_eq!(c.messages[0].author, Some("user".to_string()));
    assert_eq!(c.messages[0].content, "user question".to_string());

    assert_eq!(c.messages[1].role, "assistant");
    assert_eq!(c.messages[1].author, Some("GPT-X".to_string()));
    assert_eq!(c.messages[1].content, "assistant answer".to_string());

    assert!(c.started_at.is_some());
    assert!(c.ended_at.is_some());
    assert!(c.started_at.unwrap() <= c.ended_at.unwrap());

    assert_eq!(c.messages[0].idx, 0);
    assert_eq!(c.messages[1].idx, 1);
}

#[test]
#[serial]
fn repoprompt_detect_with_workspaces_dir() {
    let dir = TempDir::new().unwrap();
    let workspaces_root = dir.path().join("RepoPrompt").join("Workspaces");
    fs::create_dir_all(&workspaces_root).unwrap();

    unsafe {
        std::env::set_var("REPOPROMPT_HOME", &workspaces_root);
    }

    let connector = RepoPromptConnector::new();
    let result = connector.detect();
    assert!(result.detected);
    assert!(!result.evidence.is_empty());
}

#[test]
#[serial]
fn repoprompt_connector_respects_since_ts_at_file_level() {
    let dir = TempDir::new().unwrap();
    let workspaces_root = dir.path().join("RepoPrompt").join("Workspaces");
    let workspace_dir = workspaces_root.join("Workspace-since");
    let chats_dir = workspace_dir.join("Chats");
    fs::create_dir_all(&chats_dir).unwrap();
    let file = chats_dir.join("ChatSession-since.json");

    let session = serde_json::json!({
        "id": "chat-since",
        "selectedFilePaths": ["/tmp/project/file.txt"],
        "messages": [
            {
                "sequenceIndex": 0,
                "rawText": "old msg",
                "isUser": true,
                "timestamp": 784523520.0,
                "allowedFilePaths": serde_json::Value::Null,
                "modelName": serde_json::Value::Null,
                "delegateResults": [],
                "id": "old",
            },
            {
                "sequenceIndex": 1,
                "rawText": "new msg",
                "isUser": false,
                "timestamp": 784523540.0,
                "allowedFilePaths": serde_json::Value::Null,
                "modelName": "GPT-X",
                "delegateResults": [],
                "id": "new",
            },
        ],
        "savedAt": 784523545.0,
    });
    fs::write(&file, serde_json::to_string(&session).unwrap()).unwrap();

    unsafe {
        std::env::set_var("REPOPROMPT_HOME", &workspaces_root);
    }

    let connector = RepoPromptConnector::new();
    let ctx = ScanContext {
        data_root: workspaces_root.clone(),
        since_ts: Some(1_700_000_000_000),
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];

    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.messages[0].content, "old msg".to_string());
    assert_eq!(c.messages[1].content, "new msg".to_string());
}
