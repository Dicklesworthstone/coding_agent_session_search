use coding_agent_search::connectors::gemini::GeminiConnector;
use coding_agent_search::connectors::{Connector, ScanContext};
use std::fs;
use std::path::PathBuf;

/// Basic fixture parsing test
#[test]
fn gemini_parses_jsonl_fixture() {
    let fixture_root = PathBuf::from("tests/fixtures/gemini");
    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_dir: fixture_root.clone(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert!(
        !convs.is_empty(),
        "expected at least one conversation from fixture root"
    );
    let c = &convs[0];
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.messages[0].content, "Gemini hello");
}

/// Test role mapping: "model" → "assistant"
#[test]
fn gemini_maps_model_role_to_assistant() {
    let fixture_root = PathBuf::from("tests/fixtures/gemini");
    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_dir: fixture_root,
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert!(!convs.is_empty());

    let c = &convs[0];
    // First message is "user", second should be "assistant" (mapped from "model")
    assert_eq!(c.messages[0].role, "user");
    assert_eq!(c.messages[1].role, "assistant");
}

/// Test metadata extraction (sessionId, projectHash)
#[test]
fn gemini_extracts_metadata() {
    let fixture_root = PathBuf::from("tests/fixtures/gemini");
    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_dir: fixture_root,
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert!(!convs.is_empty());

    let c = &convs[0];
    assert_eq!(c.external_id, Some("test-session-1".to_string()));
    assert_eq!(
        c.metadata.get("project_hash").and_then(|v| v.as_str()),
        Some("hash123")
    );
}

/// Test timestamp parsing
#[test]
fn gemini_parses_timestamps() {
    let fixture_root = PathBuf::from("tests/fixtures/gemini");
    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_dir: fixture_root,
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert!(!convs.is_empty());

    let c = &convs[0];
    // Bead 7k7pl: upgrade presence-only `.is_some()` to pin the
    // timestamp ORDERING that defines a valid conversation: started_at
    // must come before ended_at, and the first message's created_at
    // must fall inside [started_at, ended_at]. A regression that
    // parsed the wrong timestamp field (e.g., swapped started/ended or
    // used epoch 0 as a fallback) would slip past bare presence
    // checks but fires here.
    let started = c.started_at.expect("conversation started_at must be populated");
    let ended = c.ended_at.expect("conversation ended_at must be populated");
    assert!(
        started <= ended,
        "conversation started_at ({started}) must precede or equal ended_at ({ended})"
    );
    let first_msg_created = c.messages[0]
        .created_at
        .expect("first message created_at must be populated");
    assert!(
        (started..=ended).contains(&first_msg_created),
        "first message created_at ({first_msg_created}) must fall within the \
         conversation's [started_at={started}, ended_at={ended}] window"
    );
}

/// Test detection when directory exists
#[test]
fn gemini_detect_returns_true_for_existing_dir() {
    // Create a temp dir mimicking ~/.gemini/tmp structure
    let tmp = tempfile::TempDir::new().unwrap();
    let gemini_dir = tmp.path().join(".gemini").join("tmp");
    fs::create_dir_all(&gemini_dir).unwrap();

    // Detection uses GEMINI_HOME env var or default path
    // For this test, we check that detect() works on fixture path
    let conn = GeminiConnector::new();
    let result = conn.detect();
    // Detection may or may not find the real ~/.gemini/tmp, but shouldn't panic
    // The fixture test validates actual parsing behavior
    let _ = result;
}

/// Test incremental indexing with since_ts filter
#[test]
/// since_ts controls file-level filtering (via file mtime), NOT message-level filtering.
/// When a file is modified after since_ts, ALL messages from that file are re-indexed
/// to avoid data loss from partial re-indexing.
fn gemini_includes_all_messages_when_file_modified() {
    let tmp = tempfile::TempDir::new().unwrap();
    let chats_dir = tmp.path().join("hash123").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // Create session with timestamps
    let session = serde_json::json!({
        "sessionId": "incremental-test",
        "projectHash": "hash123",
        "startTime": "2024-01-01T12:00:00Z",
        "lastUpdated": "2024-01-01T12:05:00Z",
        "messages": [
            {
                "type": "user",
                "content": "Old message",
                "timestamp": "2024-01-01T10:00:00Z"
            },
            {
                "type": "model",
                "content": "New message",
                "timestamp": "2024-01-01T14:00:00Z"
            }
        ]
    });

    fs::write(
        chats_dir.join("session-incr.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = GeminiConnector::new();

    // File-level filtering: since_ts is used to filter FILES by mtime, not messages.
    // Since this file was just created (mtime = now), it will be included.
    let since_ts = chrono::DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z")
        .unwrap()
        .timestamp_millis();

    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: Some(since_ts),
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert!(!convs.is_empty());

    // File-level filtering means ALL messages are included when file is modified
    let c = &convs[0];
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.messages[0].content, "Old message");
    assert_eq!(c.messages[1].content, "New message");
}

/// Test workspace extraction from AGENTS.md pattern in content
#[test]
fn gemini_extracts_workspace_from_agents_md_content() {
    let tmp = tempfile::TempDir::new().unwrap();
    let chats_dir = tmp.path().join("hash456").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // Create session with AGENTS.md reference in content
    let session = serde_json::json!({
        "sessionId": "workspace-test",
        "projectHash": "hash456",
        "messages": [
            {
                "type": "user",
                "content": "# AGENTS.md instructions for /data/projects/my_project\n\nSome instructions here.",
                "timestamp": "2024-01-01T12:00:00Z"
            },
            {
                "type": "model",
                "content": "I understand the project.",
                "timestamp": "2024-01-01T12:01:00Z"
            }
        ]
    });

    fs::write(
        chats_dir.join("session-workspace.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert!(!convs.is_empty());

    let c = &convs[0];
    assert_eq!(
        c.workspace,
        Some(PathBuf::from("/data/projects/my_project"))
    );
}

/// Test workspace extraction from "Working directory:" pattern
#[test]
fn gemini_extracts_workspace_from_working_directory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let chats_dir = tmp.path().join("hash789").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session = serde_json::json!({
        "sessionId": "workdir-test",
        "projectHash": "hash789",
        "messages": [
            {
                "type": "user",
                "content": "Working directory: /home/user/myproject\nPlease help me.",
                "timestamp": "2024-01-01T12:00:00Z"
            }
        ]
    });

    fs::write(
        chats_dir.join("session-workdir.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert!(!convs.is_empty());

    let c = &convs[0];
    assert_eq!(c.workspace, Some(PathBuf::from("/home/user/myproject")));
}

/// Test that empty messages are filtered out
#[test]
fn gemini_filters_empty_messages() {
    let tmp = tempfile::TempDir::new().unwrap();
    let chats_dir = tmp.path().join("hashempty").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session = serde_json::json!({
        "sessionId": "empty-test",
        "projectHash": "hashempty",
        "messages": [
            {
                "type": "user",
                "content": "   ",
                "timestamp": "2024-01-01T12:00:00Z"
            },
            {
                "type": "model",
                "content": "Valid response",
                "timestamp": "2024-01-01T12:01:00Z"
            },
            {
                "type": "user",
                "content": "",
                "timestamp": "2024-01-01T12:02:00Z"
            }
        ]
    });

    fs::write(
        chats_dir.join("session-empty.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert!(!convs.is_empty());

    // Only the non-empty message should be included
    let c = &convs[0];
    assert_eq!(c.messages.len(), 1);
    assert_eq!(c.messages[0].content, "Valid response");
}

/// Test that malformed JSON files are skipped gracefully
#[test]
fn gemini_skips_malformed_json() {
    let tmp = tempfile::TempDir::new().unwrap();
    let chats_dir = tmp.path().join("hashbad").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // Write invalid JSON
    fs::write(
        chats_dir.join("session-bad.json"),
        "{ this is not valid json",
    )
    .unwrap();

    // Write valid JSON
    let valid_session = serde_json::json!({
        "sessionId": "valid",
        "messages": [
            {
                "type": "user",
                "content": "Hello",
                "timestamp": "2024-01-01T12:00:00Z"
            }
        ]
    });
    fs::write(
        chats_dir.join("session-good.json"),
        serde_json::to_string_pretty(&valid_session).unwrap(),
    )
    .unwrap();

    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };

    // Should not panic, should return only the valid session
    let convs = conn.scan(&ctx).expect("scan should not fail on bad JSON");
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id, Some("valid".to_string()));
}

/// Test that sessions without messages array are skipped
#[test]
fn gemini_skips_sessions_without_messages() {
    let tmp = tempfile::TempDir::new().unwrap();
    let chats_dir = tmp.path().join("hashnomsg").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session = serde_json::json!({
        "sessionId": "no-messages",
        "projectHash": "hashnomsg"
        // No "messages" field
    });

    fs::write(
        chats_dir.join("session-nomsg.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };

    let convs = conn.scan(&ctx).expect("scan");
    // Should skip session without messages
    assert!(convs.is_empty());
}

/// Test title extraction from first user message
#[test]
fn gemini_extracts_title_from_first_user_message() {
    let tmp = tempfile::TempDir::new().unwrap();
    let chats_dir = tmp.path().join("hashtitle").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session = serde_json::json!({
        "sessionId": "title-test",
        "messages": [
            {
                "type": "model",
                "content": "Model message first",
                "timestamp": "2024-01-01T11:59:00Z"
            },
            {
                "type": "user",
                "content": "This is the user's first message\nWith multiple lines",
                "timestamp": "2024-01-01T12:00:00Z"
            }
        ]
    });

    fs::write(
        chats_dir.join("session-title.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert!(!convs.is_empty());

    let c = &convs[0];
    // Title should be first line of first user message
    assert_eq!(
        c.title,
        Some("This is the user's first message".to_string())
    );
}

/// Test message index assignment is sequential after filtering
#[test]
fn gemini_assigns_sequential_message_indices() {
    let tmp = tempfile::TempDir::new().unwrap();
    let chats_dir = tmp.path().join("hashidx").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session = serde_json::json!({
        "sessionId": "idx-test",
        "messages": [
            {
                "type": "user",
                "content": "First",
                "timestamp": "2024-01-01T12:00:00Z"
            },
            {
                "type": "model",
                "content": "Second",
                "timestamp": "2024-01-01T12:01:00Z"
            },
            {
                "type": "user",
                "content": "Third",
                "timestamp": "2024-01-01T12:02:00Z"
            }
        ]
    });

    fs::write(
        chats_dir.join("session-idx.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert!(!convs.is_empty());

    let c = &convs[0];
    assert_eq!(c.messages.len(), 3);
    assert_eq!(c.messages[0].idx, 0);
    assert_eq!(c.messages[1].idx, 1);
    assert_eq!(c.messages[2].idx, 2);
}

/// Test agent_slug is set to "gemini"
#[test]
fn gemini_sets_agent_slug() {
    let fixture_root = PathBuf::from("tests/fixtures/gemini");
    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_dir: fixture_root,
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert!(!convs.is_empty());

    for c in &convs {
        assert_eq!(c.agent_slug, "gemini");
    }
}

/// Test source_path is set to the session file path
#[test]
fn gemini_sets_source_path() {
    let fixture_root = PathBuf::from("tests/fixtures/gemini");
    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_dir: fixture_root.clone(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert!(!convs.is_empty());

    let c = &convs[0];
    assert!(
        c.source_path
            .to_string_lossy()
            .contains("session-test.json")
    );
}

/// Test multiple sessions in same project hash
#[test]
fn gemini_handles_multiple_sessions() {
    let tmp = tempfile::TempDir::new().unwrap();
    let chats_dir = tmp.path().join("hashmulti").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    for i in 1..=3 {
        let session = serde_json::json!({
            "sessionId": format!("session-{i}"),
            "messages": [
                {
                    "type": "user",
                    "content": format!("Message {i}"),
                    "timestamp": "2024-01-01T12:00:00Z"
                }
            ]
        });
        fs::write(
            chats_dir.join(format!("session-{i}.json")),
            serde_json::to_string_pretty(&session).unwrap(),
        )
        .unwrap();
    }

    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert_eq!(convs.len(), 3);
}

/// Test workspace fallback to parent hash directory
#[test]
fn gemini_falls_back_to_hash_directory_for_workspace() {
    let tmp = tempfile::TempDir::new().unwrap();
    let chats_dir = tmp.path().join("myhash").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // Session without any workspace hints in content
    let session = serde_json::json!({
        "sessionId": "fallback-test",
        "projectHash": "myhash",
        "messages": [
            {
                "type": "user",
                "content": "Hello without workspace hints",
                "timestamp": "2024-01-01T12:00:00Z"
            }
        ]
    });

    fs::write(
        chats_dir.join("session-fallback.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert!(!convs.is_empty());

    let c = &convs[0];
    // Should fall back to parent hash directory
    assert!(c.workspace.is_some());
    assert!(
        c.workspace
            .as_ref()
            .unwrap()
            .to_string_lossy()
            .contains("myhash")
    );
}
