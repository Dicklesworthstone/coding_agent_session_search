use coding_agent_search::connectors::cline::ClineConnector;
use coding_agent_search::connectors::{Connector, ScanContext};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

// ============================================================================
// Fixture-based tests
// ============================================================================

#[test]
fn cline_parses_fixture_task() {
    let fixture_root = PathBuf::from("tests/fixtures/cline");
    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: fixture_root.clone(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation from fixture"
    );
    let c = &convs[0];
    assert_eq!(
        c.title.as_deref(),
        Some("Cline fixture task"),
        "title should match fixture's task metadata"
    );
    // We now prefer ui_messages.json (2 msgs) over api_conversation_history.json (1 msg)
    // to avoid duplicates and prefer user-facing content.
    assert_eq!(
        c.messages.len(),
        2,
        "expected 2 messages from ui_messages.json"
    );
    assert!(
        c.messages.iter().any(|m| m.content.contains("Hello Cline")),
        "should contain 'Hello Cline' message from fixture"
    );
}

#[test]
fn cline_respects_since_ts_and_resequences_indices() {
    let dir = tempfile::TempDir::new().unwrap();
    let storage_root = dir.path().join("saoudrizwan.claude-dev");
    let root = storage_root.join("task-123");
    std::fs::create_dir_all(&root).unwrap();

    let ui_messages_path = root.join("ui_messages.json");

    // Two messages: older (timestamp=1_000) and newer (timestamp=2_000).
    let msgs = serde_json::json!([
        {
            "timestamp": 1_000,
            "role": "user",
            "content": "old msg"
        },
        {
            "timestamp": 2_000,
            "role": "assistant",
            "content": "new msg"
        }
    ]);
    std::fs::write(&ui_messages_path, serde_json::to_string(&msgs).unwrap()).unwrap();

    let connector = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: storage_root,
        scan_roots: Vec::new(),
        since_ts: Some(1_500),
    };

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation after since_ts filtering"
    );
    let c = &convs[0];

    // Incremental filtering for Cline is file-level, not per-message.
    // Since the file is newer than since_ts, we ingest all messages and resequence.
    assert_eq!(
        c.messages.len(),
        2,
        "expected file-level since_ts filtering to keep full conversation payload"
    );
    assert_eq!(
        c.messages[0].idx, 0,
        "first message idx should be 0 after re-sequencing"
    );
    assert!(
        c.messages[0].content.contains("old msg"),
        "first message should contain 'old msg'"
    );
    assert_eq!(
        c.messages[1].idx, 1,
        "second message idx should be 1 after re-sequencing"
    );
    assert_eq!(
        c.messages[1].role, "assistant",
        "second message should be assistant role"
    );
    assert!(
        c.messages[1].content.contains("new msg"),
        "second message should contain 'new msg'"
    );
}

#[test]
fn cline_skips_unmodified_files_for_since_ts() {
    let dir = tempfile::TempDir::new().unwrap();
    let storage_root = dir.path().join("saoudrizwan.claude-dev");
    let root = storage_root.join("task-older");
    std::fs::create_dir_all(&root).unwrap();

    let ui_messages_path = root.join("ui_messages.json");
    let msgs = serde_json::json!([
        {
            "timestamp": 1_000,
            "role": "user",
            "content": "persisted msg"
        }
    ]);
    std::fs::write(&ui_messages_path, serde_json::to_string(&msgs).unwrap()).unwrap();

    let modified_ms = std::fs::metadata(&ui_messages_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);

    let connector = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: storage_root,
        scan_roots: Vec::new(),
        since_ts: Some(modified_ms.saturating_add(2_000)),
    };

    let convs = connector.scan(&ctx).unwrap();
    assert!(
        convs.is_empty(),
        "expected conversation to be skipped when file mtime is older than since_ts threshold"
    );
}

// ============================================================================
// Unit tests with temp directories
// ============================================================================

/// Helper to create a Cline-style task directory
fn create_task_dir(root: &std::path::Path, task_id: &str) -> PathBuf {
    let task_dir = root.join(task_id);
    fs::create_dir_all(&task_dir).unwrap();
    task_dir
}

/// Test ui_messages.json is preferred over api_conversation_history.json
#[test]
fn cline_prefers_ui_messages() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-prefer");

    // Create both files with different content
    let ui_msgs = serde_json::json!([
        {"role": "user", "content": "UI message", "timestamp": 1000}
    ]);
    let api_msgs = serde_json::json!([
        {"role": "user", "content": "API message", "timestamp": 1000}
    ]);
    fs::write(task.join("ui_messages.json"), ui_msgs.to_string()).unwrap();
    fs::write(
        task.join("api_conversation_history.json"),
        api_msgs.to_string(),
    )
    .unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation when ui_messages.json exists"
    );
    assert!(
        convs[0].messages[0].content.contains("UI message"),
        "should prefer ui_messages.json content over api_conversation_history.json"
    );
}

/// Test fallback to api_conversation_history.json when ui_messages.json is missing
#[test]
fn cline_fallback_to_api_history() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-fallback");

    // Only create api_conversation_history.json
    let api_msgs = serde_json::json!([
        {"role": "user", "content": "API only message", "timestamp": 1000}
    ]);
    fs::write(
        task.join("api_conversation_history.json"),
        api_msgs.to_string(),
    )
    .unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected exactly 1 conversation from api_conversation_history fallback"
    );
    assert!(
        convs[0].messages[0].content.contains("API only message"),
        "should fallback to api_conversation_history.json when ui_messages.json is missing"
    );
}

/// Test multiple task directories
#[test]
fn cline_handles_multiple_tasks() {
    let dir = TempDir::new().unwrap();

    for i in 1..=3 {
        let task = create_task_dir(dir.path(), &format!("task-{i}"));
        let msgs = serde_json::json!([
            {"role": "user", "content": format!("Message {i}"), "timestamp": i * 1000}
        ]);
        fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();
    }

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        3,
        "expected 3 conversations from 3 task directories"
    );
}

/// Test taskHistory.json is skipped
#[test]
fn cline_skips_task_history_json() {
    let dir = TempDir::new().unwrap();

    // Create a real task
    let task = create_task_dir(dir.path(), "task-real");
    let msgs = serde_json::json!([{"role": "user", "content": "Real task", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    // Create taskHistory.json directory (should be skipped)
    let task_history = create_task_dir(dir.path(), "taskHistory.json");
    let msgs = serde_json::json!([{"role": "user", "content": "Should skip", "timestamp": 1000}]);
    fs::write(task_history.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected 1 conversation - taskHistory.json dir should be skipped"
    );
    assert!(
        convs[0].messages[0].content.contains("Real task"),
        "should only contain real task, not taskHistory.json"
    );
}

/// Test title extraction from metadata
#[test]
fn cline_extracts_title_from_metadata() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-title");

    let meta = serde_json::json!({"title": "Custom Task Title"});
    fs::write(task.join("task_metadata.json"), meta.to_string()).unwrap();

    let msgs = serde_json::json!([{"role": "user", "content": "Hello", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected 1 conversation for title metadata test"
    );
    assert_eq!(
        convs[0].title,
        Some("Custom Task Title".to_string()),
        "title should be extracted from task_metadata.json"
    );
}

/// Test title fallback to first message
#[test]
fn cline_title_fallback_to_first_message() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-no-title");

    // No metadata file
    let msgs = serde_json::json!([
        {"role": "user", "content": "First line for title\nSecond line", "timestamp": 1000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected 1 conversation for title fallback test"
    );
    assert_eq!(
        convs[0].title,
        Some("First line for title".to_string()),
        "title should fallback to first line of first user message"
    );
}

/// Test workspace extraction from metadata (rootPath)
#[test]
fn cline_extracts_workspace_from_rootpath() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-workspace");

    let meta = serde_json::json!({"rootPath": "/home/user/project"});
    fs::write(task.join("task_metadata.json"), meta.to_string()).unwrap();

    let msgs = serde_json::json!([{"role": "user", "content": "Hello", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "expected 1 conversation for rootPath workspace test"
    );
    assert_eq!(
        convs[0].workspace,
        Some(PathBuf::from("/home/user/project")),
        "workspace should be extracted from rootPath in task_metadata.json"
    );
}

/// Test workspace extraction from cwd field
#[test]
fn cline_extracts_workspace_from_cwd() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-cwd");

    let meta = serde_json::json!({"cwd": "/workspace/myproject"});
    fs::write(task.join("task_metadata.json"), meta.to_string()).unwrap();

    let msgs = serde_json::json!([{"role": "user", "content": "Hello", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].workspace,
        Some(PathBuf::from("/workspace/myproject"))
    );
}

/// Test empty content is filtered
#[test]
fn cline_filters_empty_content() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-empty");

    let msgs = serde_json::json!([
        {"role": "user", "content": "   ", "timestamp": 1000},
        {"role": "user", "content": "Valid content", "timestamp": 2000},
        {"role": "assistant", "content": "", "timestamp": 3000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert!(convs[0].messages[0].content.contains("Valid content"));
}

/// Test messages are sorted by timestamp
#[test]
fn cline_sorts_messages_by_timestamp() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-sort");

    // Messages in wrong order
    let msgs = serde_json::json!([
        {"role": "assistant", "content": "Third", "timestamp": 3000},
        {"role": "user", "content": "First", "timestamp": 1000},
        {"role": "assistant", "content": "Second", "timestamp": 2000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.messages.len(), 3);
    assert!(c.messages[0].content.contains("First"));
    assert!(c.messages[1].content.contains("Second"));
    assert!(c.messages[2].content.contains("Third"));

    // Indices should be sequential after sorting
    assert_eq!(c.messages[0].idx, 0);
    assert_eq!(c.messages[1].idx, 1);
    assert_eq!(c.messages[2].idx, 2);
}

/// Test external_id comes from task directory name
#[test]
fn cline_sets_external_id_from_directory() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "unique-task-123");

    let msgs = serde_json::json!([{"role": "user", "content": "Test", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id, Some("unique-task-123".to_string()));
}

/// Test source_path is the task directory
#[test]
fn cline_sets_source_path_to_task_dir() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-path");

    let msgs = serde_json::json!([{"role": "user", "content": "Test", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].source_path, task);
}

/// Test empty directory returns no conversations
#[test]
fn cline_handles_empty_directory() {
    let dir = TempDir::new().unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

/// Test task directory without message files is skipped
#[test]
fn cline_skips_task_without_messages() {
    let dir = TempDir::new().unwrap();
    let _task = create_task_dir(dir.path(), "task-no-msgs");
    // Don't create any message files

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

/// Test started_at and ended_at timestamps
#[test]
fn cline_sets_started_and_ended_at() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-times");

    let msgs = serde_json::json!([
        {"role": "user", "content": "First", "timestamp": 1000},
        {"role": "assistant", "content": "Last", "timestamp": 5000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].started_at, Some(1000000)); // 1000 seconds -> 1000000 ms
    assert_eq!(convs[0].ended_at, Some(5000000)); // 5000 seconds -> 5000000 ms
}

/// Test agent_slug is "cline"
#[test]
fn cline_sets_agent_slug() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-slug");

    let msgs = serde_json::json!([{"role": "user", "content": "Test", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].agent_slug, "cline");
}

/// Test alternate content fields (text, message)
#[test]
fn cline_parses_alternate_content_fields() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-alt-fields");

    let msgs = serde_json::json!([
        {"role": "user", "text": "Text field content", "timestamp": 1000},
        {"role": "assistant", "message": "Message field content", "timestamp": 2000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
    assert!(convs[0].messages[0].content.contains("Text field content"));
    assert!(
        convs[0].messages[1]
            .content
            .contains("Message field content")
    );
}

/// Test alternate timestamp fields (created_at, ts)
#[test]
fn cline_parses_alternate_timestamp_fields() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-alt-ts");

    let msgs = serde_json::json!([
        {"role": "user", "content": "First", "created_at": 1000},
        {"role": "assistant", "content": "Second", "ts": 2000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].created_at, Some(1000000)); // 1000 seconds -> 1000000 ms
    assert_eq!(convs[0].messages[1].created_at, Some(2000000)); // 2000 seconds -> 2000000 ms
}

/// Test type field used as role when role is missing
#[test]
fn cline_uses_type_as_role_fallback() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-type-role");

    let msgs = serde_json::json!([
        {"type": "user", "content": "User message", "timestamp": 1000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].role, "user");
}

/// Test long title is truncated
#[test]
fn cline_truncates_long_title() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-long");

    let long_text = "A".repeat(200);
    let msgs = serde_json::json!([
        {"role": "user", "content": long_text, "timestamp": 1000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    // Bead 7k7pl: collapse `.is_some()` + `.unwrap().len() == 100`
    // into one `assert_eq!` that captures both preconditions — a
    // regression producing None or the wrong truncation length both
    // fail with a single actionable message.
    assert_eq!(
        convs[0].title.as_ref().map(|t| t.len()),
        Some(100),
        "title must be truncated to exactly 100 chars; got {:?}",
        convs[0].title
    );
}

/// Test metadata source is "cline"
#[test]
fn cline_sets_metadata_source() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-meta");

    let msgs = serde_json::json!([{"role": "user", "content": "Test", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].metadata.get("source").and_then(|v| v.as_str()),
        Some("cline")
    );
}

/// Test files in root (not directories) are ignored
#[test]
fn cline_ignores_files_in_root() {
    let dir = TempDir::new().unwrap();

    // Create a valid task
    let task = create_task_dir(dir.path(), "task-valid");
    let msgs = serde_json::json!([{"role": "user", "content": "Valid", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    // Create files in root (should be ignored)
    fs::write(dir.path().join("some_file.json"), "{}").unwrap();
    fs::write(dir.path().join("another.txt"), "text").unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
}

/// Test ISO-8601 timestamp parsing
#[test]
fn cline_parses_iso_timestamps() {
    let dir = TempDir::new().unwrap();
    let task = create_task_dir(dir.path(), "task-iso");

    let msgs = serde_json::json!([
        {"role": "user", "content": "ISO timestamp", "timestamp": "2025-11-12T18:31:18.000Z"}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string()).unwrap();

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(convs[0].messages[0].created_at.is_some());
}
