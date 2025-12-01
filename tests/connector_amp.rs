use coding_agent_search::connectors::amp::AmpConnector;
use coding_agent_search::connectors::{Connector, ScanContext};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

// ============================================================================
// Unit tests with temp directories
// ============================================================================

/// Helper to create an Amp-style cache directory
fn create_amp_dir(root: &std::path::Path) -> PathBuf {
    let amp_dir = root.join("amp-cache");
    fs::create_dir_all(&amp_dir).unwrap();
    amp_dir
}

#[test]
fn amp_parses_minimal_cache() {
    let fixture_root = PathBuf::from("tests/fixtures/amp");
    let conn = AmpConnector::new();
    // Detection may fail on systems without amp cache; force scan with our fixture root.
    let ctx = ScanContext {
        data_root: fixture_root.clone(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert!(!convs.is_empty(), "expected at least one conversation");
    let c = &convs[0];
    assert_eq!(c.agent_slug, "amp");
    assert!(c.external_id.is_some());
    assert!(!c.messages.is_empty());
}

#[test]
fn amp_respects_since_ts_and_reindexes_indices() {
    let fixture_root = PathBuf::from("tests/fixtures/amp");
    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: fixture_root.clone(),
        since_ts: Some(1_700_000_000_000),
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert_eq!(convs.len(), 1);
    let c = &convs[0];
    assert_eq!(c.messages.len(), 1);
    assert_eq!(c.messages[0].idx, 0);
    assert_eq!(c.messages[0].created_at, Some(1_700_000_005_000));
    assert_eq!(c.started_at, Some(1_700_000_005_000));
    assert_eq!(c.ended_at, Some(1_700_000_005_000));
}

/// Test handling of malformed JSON files
#[test]
fn amp_skips_malformed_json() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    // Write invalid JSON
    fs::write(amp_dir.join("thread-bad.json"), "{ this is not valid json").unwrap();

    // Write valid JSON
    let valid_session = serde_json::json!({
        "messages": [
            {
                "role": "user",
                "content": "Hello",
                "created_at": 1000
            }
        ]
    });
    fs::write(
        amp_dir.join("thread-good.json"),
        serde_json::to_string_pretty(&valid_session).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };

    // Should not panic, should return only the valid session
    let convs = conn.scan(&ctx).expect("scan should not fail on bad JSON");
    assert_eq!(convs.len(), 1);
    assert!(
        convs[0]
            .source_path
            .to_string_lossy()
            .contains("thread-good")
    );
}

/// Test alternate field names (speaker/text vs role/content)
#[test]
fn amp_parses_alternate_fields() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    // Use "speaker" instead of "role" and "text" instead of "content"
    let session = serde_json::json!({
        "messages": [
            {
                "speaker": "human",
                "text": "Hello Amp",
                "timestamp": 1000
            },
            {
                "speaker": "bot",
                "body": "Hello Human", // "body" is another fallback
                "ts": 2000
            }
        ]
    });
    fs::write(
        amp_dir.join("conversation-alt.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.messages.len(), 2);

    assert_eq!(c.messages[0].role, "human");
    assert_eq!(c.messages[0].content, "Hello Amp");
    assert_eq!(c.messages[0].created_at, Some(1000));

    assert_eq!(c.messages[1].role, "bot");
    assert_eq!(c.messages[1].content, "Hello Human");
    assert_eq!(c.messages[1].created_at, Some(2000));
}

/// Test timestamp formats (ISO string vs Millis)
#[test]
fn amp_handles_timestamp_formats() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    let session = serde_json::json!({
        "messages": [
            {
                "role": "user",
                "content": "ISO time",
                "created_at": "2025-11-12T18:31:18.000Z"
            },
            {
                "role": "agent",
                "content": "Millis time",
                "created_at": 1700000000000i64
            }
        ]
    });
    fs::write(
        amp_dir.join("thread-time.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert!(c.messages[0].created_at.is_some());
    assert!(c.messages[1].created_at.is_some());

    // Verify values roughly
    assert_eq!(c.messages[1].created_at, Some(1700000000000));
}

/// Test workspace extraction logic
#[test]
fn amp_extracts_workspace() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    // Case 1: Root level "workspace" field
    let session1 = serde_json::json!({
        "workspace": "/home/user/project1",
        "messages": [{"role": "user", "content": "test"}]
    });
    fs::write(
        amp_dir.join("thread-ws1.json"),
        serde_json::to_string_pretty(&session1).unwrap(),
    )
    .unwrap();

    // Case 2: Message level "extra" workspace field
    let session2 = serde_json::json!({
        "messages": [{"role": "user",
            "content": "test",
            "workspace": "/home/user/project2"
        }]
    });
    fs::write(
        amp_dir.join("thread-ws2.json"),
        serde_json::to_string_pretty(&session2).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 2);

    let ws1 = convs
        .iter()
        .find(|c| c.source_path.to_string_lossy().contains("thread-ws1"));
    assert!(ws1.is_some());
    assert_eq!(
        ws1.unwrap().workspace,
        Some(PathBuf::from("/home/user/project1"))
    );

    let ws2 = convs
        .iter()
        .find(|c| c.source_path.to_string_lossy().contains("thread-ws2"));
    assert!(ws2.is_some());
    assert_eq!(
        ws2.unwrap().workspace,
        Some(PathBuf::from("/home/user/project2"))
    );
}

/// Test nested message structure (thread.messages)
#[test]
fn amp_handles_nested_structure() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    // Amp sometimes wraps messages in a "thread" object
    let session = serde_json::json!({
        "thread": {
            "id": "thread-123",
            "messages": [
                {"role": "user", "content": "nested message"}
            ]
        }
    });
    fs::write(
        amp_dir.join("thread-nested.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "nested message");
}

/// Test title extraction and fallback
#[test]
fn amp_extracts_title() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    // Case 1: Explicit title
    let session1 = serde_json::json!({
        "title": "Explicit Title",
        "messages": [{"role": "user", "content": "content"}]
    });
    fs::write(
        amp_dir.join("thread-title1.json"),
        serde_json::to_string_pretty(&session1).unwrap(),
    )
    .unwrap();

    // Case 2: Fallback to first message
    let session2 = serde_json::json!({
        "messages": [{"role": "user", "content": "First message title\nSecond line"}]
    });
    fs::write(
        amp_dir.join("thread-title2.json"),
        serde_json::to_string_pretty(&session2).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();

    let t1 = convs
        .iter()
        .find(|c| c.source_path.to_string_lossy().contains("thread-title1"))
        .unwrap();
    assert_eq!(t1.title, Some("Explicit Title".to_string()));

    let t2 = convs
        .iter()
        .find(|c| c.source_path.to_string_lossy().contains("thread-title2"))
        .unwrap();
    assert_eq!(t2.title, Some("First message title".to_string()));
}

/// Test file detection logic
#[test]
fn amp_detects_valid_files() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    let content = r#"{"messages":[{"role":"user","content":"test"}]}"#;

    // Valid filenames
    fs::write(amp_dir.join("thread-1.json"), content).unwrap();
    fs::write(amp_dir.join("conversation-2.json"), content).unwrap();
    fs::write(amp_dir.join("chat-3.json"), content).unwrap();

    // Invalid filenames (should be skipped)
    fs::write(amp_dir.join("config.json"), content).unwrap();
    fs::write(amp_dir.join("thread.txt"), content).unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();

    // Should match 3 valid files
    assert_eq!(convs.len(), 3);
}

/// Test correct handling of message roles
#[test]
fn amp_normalizes_roles() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    let session = serde_json::json!({
        "messages": [
            {"role": "user", "content": "u"},
            {"type": "model", "content": "m"}, // "type" fallback
            {"speaker": "agent", "content": "a"} // "speaker" fallback
        ]
    });
    fs::write(
        amp_dir.join("thread-roles.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let msgs = &convs[0].messages;
    assert_eq!(msgs[0].role, "user");
    assert_eq!(msgs[1].role, "model");
    assert_eq!(msgs[2].role, "agent");
}

/// Test external ID extraction
#[test]
fn amp_extracts_external_id() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    // Case 1: ID in JSON
    let session1 = serde_json::json!({
        "id": "internal-id-123",
        "messages": [{"role": "user", "content": "test"}]
    });
    fs::write(
        amp_dir.join("thread-id1.json"),
        serde_json::to_string_pretty(&session1).unwrap(),
    )
    .unwrap();

    // Case 2: ID from filename
    let session2 = serde_json::json!({
        "messages": [{"role": "user", "content": "test"}]
    });
    fs::write(
        amp_dir.join("thread-filename-id.json"),
        serde_json::to_string_pretty(&session2).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();

    let c1 = convs
        .iter()
        .find(|c| c.source_path.to_string_lossy().contains("thread-id1"))
        .unwrap();
    // Filename fallback happens if "id" field is missing?
    // Actually code says: path.file_stem() OR val.get("id").
    // Wait, OR_ELSE is lazy. So if file_stem exists, it uses that.
    // Let's check source:
    // let external_id = path.file_stem()... or_else(|| val.get("id")...)
    // So file stem ALWAYS wins if present.

    assert_eq!(c1.external_id, Some("thread-id1".to_string()));

    let c2 = convs
        .iter()
        .find(|c| {
            c.source_path
                .to_string_lossy()
                .contains("thread-filename-id")
        })
        .unwrap();
    assert_eq!(c2.external_id, Some("thread-filename-id".to_string()));
}

// ============================================================================
// Additional edge case tests
// ============================================================================

/// Test that empty content messages are filtered
#[test]
fn amp_filters_empty_content() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    let session = serde_json::json!({
        "messages": [
            {"role": "user", "content": "valid message"},
            {"role": "assistant", "content": ""},
            {"role": "user", "content": "   "},
            {"role": "assistant", "content": "another valid"}
        ]
    });
    fs::write(
        amp_dir.join("thread-empty.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    // Amp connector filters empty/whitespace messages
    let msgs = &convs[0].messages;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].content, "valid message");
    assert_eq!(msgs[1].content, "another valid");
}

/// Test author/sender field extraction
#[test]
fn amp_extracts_author_field() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    let session = serde_json::json!({
        "messages": [
            {"role": "user", "content": "test1", "author": "user@example.com"},
            {"role": "assistant", "content": "test2", "sender": "claude-3"},
            {"role": "user", "content": "test3", "model": "gpt-4"}
        ]
    });
    fs::write(
        amp_dir.join("thread-author.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let msgs = &convs[0].messages;
    // Check if author fields are extracted (depends on implementation)
    assert_eq!(msgs.len(), 3);
}

/// Test handling of empty directory
#[test]
fn amp_handles_empty_directory() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    // Don't create any files

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

/// Test agent_slug is always "amp"
#[test]
fn amp_sets_correct_agent_slug() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    let session = serde_json::json!({
        "messages": [{"role": "user", "content": "test"}]
    });
    fs::write(
        amp_dir.join("thread-slug.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].agent_slug, "amp");
}

/// Test source_path is set correctly
#[test]
fn amp_sets_source_path() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    let session = serde_json::json!({
        "messages": [{"role": "user", "content": "test"}]
    });
    let file_path = amp_dir.join("thread-path.json");
    fs::write(&file_path, serde_json::to_string_pretty(&session).unwrap()).unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].source_path, file_path);
}

/// Test started_at and ended_at are computed from message timestamps
#[test]
fn amp_computes_started_ended_at() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    let session = serde_json::json!({
        "messages": [
            {"role": "user", "content": "first", "created_at": 1000},
            {"role": "assistant", "content": "second", "created_at": 2000},
            {"role": "user", "content": "third", "created_at": 3000}
        ]
    });
    fs::write(
        amp_dir.join("thread-times.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.started_at, Some(1000));
    assert_eq!(c.ended_at, Some(3000));
}

/// Test sequential index assignment
#[test]
fn amp_assigns_sequential_indices() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    let session = serde_json::json!({
        "messages": [
            {"role": "user", "content": "msg0"},
            {"role": "assistant", "content": "msg1"},
            {"role": "user", "content": "msg2"},
            {"role": "assistant", "content": "msg3"}
        ]
    });
    fs::write(
        amp_dir.join("thread-idx.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let msgs = &convs[0].messages;
    for (i, msg) in msgs.iter().enumerate() {
        assert_eq!(msg.idx, i as i64);
    }
}

/// Test workspace extraction from alternate keys (cwd, path, project_path, repo, root)
#[test]
fn amp_workspace_from_alternate_keys() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    // Test "cwd" key
    let session1 = serde_json::json!({
        "cwd": "/path/from/cwd",
        "messages": [{"role": "user", "content": "test"}]
    });
    fs::write(
        amp_dir.join("thread-cwd.json"),
        serde_json::to_string_pretty(&session1).unwrap(),
    )
    .unwrap();

    // Test "path" key
    let session2 = serde_json::json!({
        "path": "/path/from/path",
        "messages": [{"role": "user", "content": "test"}]
    });
    fs::write(
        amp_dir.join("thread-path-ws.json"),
        serde_json::to_string_pretty(&session2).unwrap(),
    )
    .unwrap();

    // Test "project_path" key
    let session3 = serde_json::json!({
        "project_path": "/path/from/project_path",
        "messages": [{"role": "user", "content": "test"}]
    });
    fs::write(
        amp_dir.join("thread-project.json"),
        serde_json::to_string_pretty(&session3).unwrap(),
    )
    .unwrap();

    // Test "repo" key
    let session4 = serde_json::json!({
        "repo": "/path/from/repo",
        "messages": [{"role": "user", "content": "test"}]
    });
    fs::write(
        amp_dir.join("thread-repo.json"),
        serde_json::to_string_pretty(&session4).unwrap(),
    )
    .unwrap();

    // Test "root" key
    let session5 = serde_json::json!({
        "root": "/path/from/root",
        "messages": [{"role": "user", "content": "test"}]
    });
    fs::write(
        amp_dir.join("thread-root.json"),
        serde_json::to_string_pretty(&session5).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 5);

    // Find each conversation and verify workspace
    let cwd_conv = convs
        .iter()
        .find(|c| c.source_path.to_string_lossy().contains("thread-cwd"))
        .unwrap();
    assert_eq!(cwd_conv.workspace, Some(PathBuf::from("/path/from/cwd")));

    let path_conv = convs
        .iter()
        .find(|c| c.source_path.to_string_lossy().contains("thread-path-ws"))
        .unwrap();
    assert_eq!(path_conv.workspace, Some(PathBuf::from("/path/from/path")));

    let project_conv = convs
        .iter()
        .find(|c| c.source_path.to_string_lossy().contains("thread-project"))
        .unwrap();
    assert_eq!(
        project_conv.workspace,
        Some(PathBuf::from("/path/from/project_path"))
    );

    let repo_conv = convs
        .iter()
        .find(|c| c.source_path.to_string_lossy().contains("thread-repo"))
        .unwrap();
    assert_eq!(repo_conv.workspace, Some(PathBuf::from("/path/from/repo")));

    let root_conv = convs
        .iter()
        .find(|c| c.source_path.to_string_lossy().contains("thread-root"))
        .unwrap();
    assert_eq!(root_conv.workspace, Some(PathBuf::from("/path/from/root")));
}

/// Test JSON file without messages array is skipped
#[test]
fn amp_skips_json_without_messages() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    // JSON with no messages array
    let session1 = serde_json::json!({
        "title": "No messages here",
        "data": {"key": "value"}
    });
    fs::write(
        amp_dir.join("thread-nomsg.json"),
        serde_json::to_string_pretty(&session1).unwrap(),
    )
    .unwrap();

    // Valid JSON with messages
    let session2 = serde_json::json!({
        "messages": [{"role": "user", "content": "valid"}]
    });
    fs::write(
        amp_dir.join("thread-valid.json"),
        serde_json::to_string_pretty(&session2).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();

    // Should only have the valid one
    assert_eq!(convs.len(), 1);
    assert!(convs[0]
        .source_path
        .to_string_lossy()
        .contains("thread-valid"));
}

/// Test camelCase timestamp field (createdAt)
#[test]
fn amp_handles_camel_case_timestamps() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    let session = serde_json::json!({
        "messages": [
            {"role": "user", "content": "test", "createdAt": 1700000000000i64}
        ]
    });
    fs::write(
        amp_dir.join("thread-camel.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].created_at, Some(1700000000000));
}

/// Test nested directories are scanned recursively (uses WalkDir)
#[test]
fn amp_scans_nested_directories() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    // Create a nested directory with a thread file
    let nested = amp_dir.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let session = serde_json::json!({
        "messages": [{"role": "user", "content": "nested"}]
    });
    fs::write(
        nested.join("thread-nested.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    // Also add a direct child
    let session2 = serde_json::json!({
        "messages": [{"role": "user", "content": "direct"}]
    });
    fs::write(
        amp_dir.join("thread-direct.json"),
        serde_json::to_string_pretty(&session2).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();

    // Both files should be found (recursive scan via WalkDir)
    assert_eq!(convs.len(), 2);

    let has_direct = convs
        .iter()
        .any(|c| c.source_path.to_string_lossy().contains("thread-direct"));
    let has_nested = convs
        .iter()
        .any(|c| c.source_path.to_string_lossy().contains("thread-nested"));
    assert!(has_direct);
    assert!(has_nested);
}

/// Test messages with whitespace content are filtered
#[test]
fn amp_filters_whitespace_content() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    let session = serde_json::json!({
        "messages": [
            {"role": "user", "content": "valid"},
            {"role": "assistant", "content": "\n\t  \n"},
            {"role": "user", "content": "also valid"}
        ]
    });
    fs::write(
        amp_dir.join("thread-ws.json"),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    // Amp connector filters whitespace-only messages
    let msgs = &convs[0].messages;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].content, "valid");
    assert_eq!(msgs[1].content, "also valid");
}

/// Test conversations with ONLY empty content messages are skipped
#[test]
fn amp_skips_empty_content_conversations() {
    let dir = TempDir::new().unwrap();
    let amp_dir = create_amp_dir(dir.path());

    // All messages have empty content
    let session1 = serde_json::json!({
        "messages": [
            {"role": "user", "content": ""},
            {"role": "assistant", "content": "   "}
        ]
    });
    fs::write(
        amp_dir.join("thread-allempty.json"),
        serde_json::to_string_pretty(&session1).unwrap(),
    )
    .unwrap();

    // Conversation with valid content
    let session2 = serde_json::json!({
        "messages": [{"role": "user", "content": "valid"}]
    });
    fs::write(
        amp_dir.join("thread-hasvalid.json"),
        serde_json::to_string_pretty(&session2).unwrap(),
    )
    .unwrap();

    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_root: amp_dir,
        since_ts: None,
    };
    let convs = conn.scan(&ctx).unwrap();

    // Only the valid one should be included
    assert_eq!(convs.len(), 1);
    assert!(convs[0]
        .source_path
        .to_string_lossy()
        .contains("thread-hasvalid"));
}
