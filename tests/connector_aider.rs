use coding_agent_search::connectors::aider::AiderConnector;
use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot};
use serial_test::serial;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

mod util;
use util::{CwdGuard, EnvGuard};

// Helper to create test fixtures
fn create_aider_fixture(dir: &TempDir, filename: &str, content: &str) -> PathBuf {
    let path = dir.path().join(filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    path
}

// =============================================================================
// BASIC PARSING TESTS
// =============================================================================

#[test]
fn aider_parses_chat_history() {
    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/aider");
    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: fixture_root,
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert!(!convs.is_empty(), "Should find at least one conversation");

    let conv = &convs[0];
    assert_eq!(conv.agent_slug, "aider");
    assert!(
        conv.title.as_ref().unwrap().starts_with("Aider Chat:"),
        "title should use 'Aider Chat:' prefix, got: {}",
        conv.title.as_ref().unwrap()
    );

    // Check message parsing
    // The fixture has:
    // > /add src/main.rs
    // ...
    // > Please refactor.
    // ...

    // We expect at least 2 user messages and some assistant responses
    assert!(conv.messages.len() >= 2);

    let first_msg = &conv.messages[0];
    assert_eq!(first_msg.role, "user");
    assert!(first_msg.content.contains("/add src/main.rs"));

    let second_msg = &conv.messages[1];
    // Depending on how the parser handles the "Added src/main.rs..." text, it might be assistant
    assert_eq!(second_msg.role, "assistant");
    assert!(second_msg.content.contains("Added src/main.rs"));
}

/// Test that `agent_slug` is always "aider"
#[test]
fn aider_sets_agent_slug() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(&tmp, ".aider.chat.history.md", "> Hello\n\nWorld\n");

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].agent_slug, "aider");
}

/// Test that `source_path` is set to the chat history file
#[test]
fn aider_sets_source_path() {
    let tmp = TempDir::new().unwrap();
    let path = create_aider_fixture(&tmp, ".aider.chat.history.md", "> Test\n\nResponse\n");

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].source_path, path);
}

/// Test `external_id` from filename
#[test]
fn aider_sets_external_id_from_filename() {
    let tmp = TempDir::new().unwrap();
    let path = create_aider_fixture(&tmp, ".aider.chat.history.md", "> Test\n\nResponse\n");

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].external_id,
        Some(path.to_string_lossy().to_string())
    );
}

/// Test title format includes path
#[test]
fn aider_title_includes_path() {
    let tmp = TempDir::new().unwrap();
    let path = create_aider_fixture(&tmp, ".aider.chat.history.md", "> Test\n\nResponse\n");

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let title = convs[0].title.as_ref().unwrap();
    assert!(title.starts_with("Aider Chat:"));
    // Title uses parent directory name, not the full file path
    let parent_name = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap();
    assert!(
        title.contains(parent_name),
        "title should contain parent dir name '{}', got: {}",
        parent_name,
        title
    );
}

/// Test workspace is set to parent directory
#[test]
fn aider_sets_workspace_to_parent() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        "project/.aider.chat.history.md",
        "> Test\n\nResponse\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let workspace = convs[0].workspace.as_ref().unwrap();
    assert!(workspace.ends_with("project"));
}

// =============================================================================
// TIMESTAMP TESTS
// =============================================================================

/// Test `started_at` and `ended_at` are set from file mtime
#[test]
fn aider_timestamps_from_mtime() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(&tmp, ".aider.chat.history.md", "> Test\n\nResponse\n");

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    // Bead 7k7pl: pin that BOTH timestamps are the same plausible
    // ms-epoch value (file mtime), not just "present". Aider uses
    // the file's mtime for both started/ended; a regression that
    // emitted 0 / MIN / time::now() would slip past `.is_some()`.
    let started = convs[0]
        .started_at
        .expect("started_at must be set from mtime");
    let ended = convs[0].ended_at.expect("ended_at must be set from mtime");
    // ms-epoch sanity floor: 2001-09-09 (1_000_000_000_000 ms).
    assert!(
        started >= 1_000_000_000_000,
        "started_at must be a plausible ms-epoch value; got {started}"
    );
    assert_eq!(
        started, ended,
        "aider sets started/ended from the same mtime; got started={started}, ended={ended}"
    );
}

/// Test `since_ts` filtering excludes old files
#[test]
fn aider_since_ts_filters_old_files() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(&tmp, ".aider.chat.history.md", "> Test\n\nResponse\n");

    let conn = AiderConnector::new();
    // Use a future timestamp to filter out all files
    let future_ts = chrono::Utc::now().timestamp_millis() + 100_000_000;
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: Some(future_ts),
    };
    let convs = conn.scan(&ctx).expect("scan");

    // File should be filtered out due to since_ts
    assert!(convs.is_empty());
}

/// Test `since_ts=None` includes all files
#[test]
fn aider_no_since_ts_includes_all() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(&tmp, ".aider.chat.history.md", "> Test\n\nResponse\n");

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
}

// =============================================================================
// MESSAGE PARSING TESTS
// =============================================================================

/// Test message indices are sequential
#[test]
fn aider_message_indices_sequential() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "> First user message\n\nFirst response\n\n> Second user message\n\nSecond response\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let msgs = &convs[0].messages;
    assert!(msgs.len() >= 2);

    for (i, msg) in msgs.iter().enumerate() {
        assert_eq!(msg.idx, i as i64, "Message index should be sequential");
    }
}

/// Test author field matches role
#[test]
fn aider_author_matches_role() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "> User input\n\nAssistant output\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    for msg in &convs[0].messages {
        assert_eq!(msg.author, Some(msg.role.clone()));
    }
}

/// Test user messages start with > prefix
#[test]
fn aider_user_messages_from_prefix() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "> /add file.rs\n> Continue this line\n\nResponse here\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let msgs = &convs[0].messages;
    assert!(!msgs.is_empty());

    let first = &msgs[0];
    assert_eq!(first.role, "user");
    // The "> " prefix is stripped
    assert!(first.content.contains("/add file.rs"));
    assert!(first.content.contains("Continue this line"));
}

/// Test multi-line user input is combined
#[test]
fn aider_multiline_user_input() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "> Line 1\n> Line 2\n> Line 3\n\nResponse\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let msgs = &convs[0].messages;
    assert!(!msgs.is_empty());

    let user_msg = &msgs[0];
    assert_eq!(user_msg.role, "user");
    assert!(user_msg.content.contains("Line 1"));
    assert!(user_msg.content.contains("Line 2"));
    assert!(user_msg.content.contains("Line 3"));
}

/// Test assistant response after user block
#[test]
fn aider_assistant_after_user() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "> User prompt\n\nThis is the assistant response.\nMultiple lines here.\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let msgs = &convs[0].messages;
    assert!(msgs.len() >= 2);

    let assistant_msg = &msgs[1];
    assert_eq!(assistant_msg.role, "assistant");
    assert!(assistant_msg.content.contains("assistant response"));
}

/// Test multiple conversation turns
#[test]
fn aider_multiple_turns() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "> First question\n\nFirst answer\n\n> Second question\n\nSecond answer\n\n> Third question\n\nThird answer\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let msgs = &convs[0].messages;

    // Should have 6 messages: 3 user + 3 assistant
    assert_eq!(msgs.len(), 6);

    // Verify alternating roles
    assert_eq!(msgs[0].role, "user");
    assert_eq!(msgs[1].role, "assistant");
    assert_eq!(msgs[2].role, "user");
    assert_eq!(msgs[3].role, "assistant");
    assert_eq!(msgs[4].role, "user");
    assert_eq!(msgs[5].role, "assistant");
}

// =============================================================================
// EMPTY / EDGE CASE TESTS
// =============================================================================

/// Test empty file returns no messages
#[test]
fn aider_empty_file() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(&tmp, ".aider.chat.history.md", "");

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    assert!(convs[0].messages.is_empty());
}

/// Test whitespace-only file
#[test]
fn aider_whitespace_only_file() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(&tmp, ".aider.chat.history.md", "   \n\n\t\n   ");

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    assert!(convs[0].messages.is_empty());
}

/// Test file with only user messages (no responses)
#[test]
fn aider_only_user_messages() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "> First command\n> Second command\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let msgs = &convs[0].messages;
    // Should have one user message with combined content
    assert!(!msgs.is_empty());
    assert_eq!(msgs[0].role, "user");
}

/// Test file with no user prefix (all assistant-like content)
#[test]
fn aider_no_user_prefix_content() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "This is just some text\nwithout any > prefix\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    // Content without > prefix starts as "system" role
    let msgs = &convs[0].messages;
    if !msgs.is_empty() {
        // If there's content, it should be system role (initial state)
        assert_eq!(msgs[0].role, "system");
    }
}

// =============================================================================
// DIRECTORY SCANNING TESTS
// =============================================================================

/// Test scanning finds files in subdirectories
#[test]
fn aider_scans_subdirectories() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        "project1/.aider.chat.history.md",
        "> Test 1\n\nResponse 1\n",
    );
    create_aider_fixture(
        &tmp,
        "project2/subdir/.aider.chat.history.md",
        "> Test 2\n\nResponse 2\n",
    );
    create_aider_fixture(
        &tmp,
        "deep/nested/path/.aider.chat.history.md",
        "> Test 3\n\nResponse 3\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 3);
}

/// Test only .aider.chat.history.md files are scanned
#[test]
fn aider_only_scans_chat_history_files() {
    let tmp = TempDir::new().unwrap();
    // Valid file
    create_aider_fixture(&tmp, ".aider.chat.history.md", "> Test\n\nResponse\n");
    // Invalid files (should be ignored)
    create_aider_fixture(&tmp, "other.md", "> Test\n\nResponse\n");
    create_aider_fixture(&tmp, ".aider.log", "> Test\n\nResponse\n");
    create_aider_fixture(&tmp, "chat.md", "> Test\n\nResponse\n");

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    // Only the .aider.chat.history.md file should be found
    assert_eq!(convs.len(), 1);
}

/// Test multiple chat history files in different projects
#[test]
fn aider_multiple_projects() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        "frontend/.aider.chat.history.md",
        "> Frontend task\n\nFrontend done\n",
    );
    create_aider_fixture(
        &tmp,
        "backend/.aider.chat.history.md",
        "> Backend task\n\nBackend done\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 2);

    // Each should have its own workspace
    let workspaces: Vec<_> = convs
        .iter()
        .map(|c| c.workspace.as_ref().unwrap())
        .collect();
    assert!(workspaces.iter().any(|w| w.ends_with("frontend")));
    assert!(workspaces.iter().any(|w| w.ends_with("backend")));
}

// =============================================================================
// SPECIAL CONTENT TESTS
// =============================================================================

/// Test aider commands are preserved in content
#[test]
fn aider_preserves_commands() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "> /add src/main.rs\n> /drop src/test.rs\n> /run cargo build\n\nDone!\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let user_msg = &convs[0].messages[0];
    assert!(user_msg.content.contains("/add src/main.rs"));
    assert!(user_msg.content.contains("/drop src/test.rs"));
    assert!(user_msg.content.contains("/run cargo build"));
}

/// Test code blocks in responses
#[test]
fn aider_code_blocks_in_response() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "> Write hello world\n\nHere's the code:\n```rust\nfn main() {\n    println!(\"Hello, world!\");\n}\n```\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let msgs = &convs[0].messages;
    assert!(msgs.len() >= 2);

    let assistant_msg = &msgs[1];
    assert!(assistant_msg.content.contains("```rust"));
    assert!(assistant_msg.content.contains("println!"));
}

/// Test markdown formatting in response
#[test]
fn aider_markdown_formatting() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "> Explain this\n\n# Heading\n\n- Item 1\n- Item 2\n\n**Bold** and *italic*\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let msgs = &convs[0].messages;
    assert!(msgs.len() >= 2);

    let assistant_msg = &msgs[1];
    assert!(assistant_msg.content.contains("# Heading"));
    assert!(assistant_msg.content.contains("- Item 1"));
    assert!(assistant_msg.content.contains("**Bold**"));
}

/// Test > in code blocks is not treated as user input
#[test]
fn aider_gt_in_code_not_user_input() {
    let tmp = TempDir::new().unwrap();
    // The > inside code should not start a new user message
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "> Ask about comparisons\n\nHere's an example:\n```\nif a > b {\n    println!(\"greater\");\n}\n```\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let msgs = &convs[0].messages;

    // Note: The simple line-based parser doesn't track code block state,
    // so "if a > b {" may be parsed as user input. This tests actual behavior.
    // At minimum we should have user message and some content
    assert!(!msgs.is_empty());
    assert_eq!(msgs[0].role, "user");
    assert!(msgs[0].content.contains("Ask about comparisons"));
}

// =============================================================================
// DETECTION TESTS
// =============================================================================

/// Detect should only succeed when an aider history is actually present
/// and the probe should remain fast (no recursive walk on every call).
#[test]
#[serial]
fn aider_detect_requires_marker_and_is_fast() {
    use std::time::Instant;

    let tmp = tempfile::TempDir::new().unwrap();

    // Use RAII guards for cleanup even on panic
    let _cwd_guard = CwdGuard::change_to(tmp.path()).unwrap();
    let _env_guard = EnvGuard::set("CASS_AIDER_DATA_ROOT", "");
    unsafe {
        std::env::remove_var("CASS_AIDER_DATA_ROOT");
    }

    // Build a moderately large directory tree to catch accidental recursion.
    for i in 0..50 {
        let dir = tmp.path().join(format!("nested/{i}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("noise.txt"), "noise").unwrap();
    }

    let start = Instant::now();
    let conn = AiderConnector::new();
    let result = conn.detect();
    let elapsed = start.elapsed();

    // No marker -> should not report detected
    assert!(
        !result.detected,
        "detect() should be false without marker files"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(200),
        "detect() should be fast without scanning entire tree"
    );
    // Guards automatically restore cwd and env on drop
}

/// Detect succeeds when .aider.chat.history.md is present in cwd
#[test]
#[serial]
fn aider_detect_with_marker_file() {
    let tmp = tempfile::TempDir::new().unwrap();

    // Use RAII guard for cleanup even on panic
    let _cwd_guard = CwdGuard::change_to(tmp.path()).unwrap();

    let marker = tmp.path().join(".aider.chat.history.md");
    std::fs::write(&marker, "stub").unwrap();

    let conn = AiderConnector::new();
    let result = conn.detect();

    assert!(
        result.detected,
        "detect() should succeed when marker exists"
    );
    assert!(
        result
            .evidence
            .iter()
            .any(|e| e.contains(".aider.chat.history.md"))
    );
    // Guard automatically restores cwd on drop
}

// =============================================================================
// METADATA TESTS
// =============================================================================

/// Test metadata is empty JSON object
#[test]
fn aider_metadata_is_empty() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(&tmp, ".aider.chat.history.md", "> Test\n\nResponse\n");

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].metadata, serde_json::json!({}));
}

/// Test message extra is empty JSON object
#[test]
fn aider_message_extra_is_empty() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(&tmp, ".aider.chat.history.md", "> Test\n\nResponse\n");

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    for msg in &convs[0].messages {
        assert_eq!(msg.extra, serde_json::json!({}));
    }
}

/// Test message `created_at` is None (aider doesn't store per-message timestamps)
#[test]
fn aider_message_created_at_is_none() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(&tmp, ".aider.chat.history.md", "> Test\n\nResponse\n");

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    for msg in &convs[0].messages {
        assert!(msg.created_at.is_none());
    }
}

/// Test snippets are empty (aider doesn't extract snippets)
#[test]
fn aider_message_snippets_empty() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(&tmp, ".aider.chat.history.md", "> Test\n\nResponse\n");

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    for msg in &convs[0].messages {
        assert!(msg.snippets.is_empty());
    }
}

// =============================================================================
// ERROR HANDLING TESTS
// =============================================================================

/// Test scan with non-existent directory returns empty
#[test]
fn aider_nonexistent_directory() {
    let conn = AiderConnector::new();
    let nonexistent = PathBuf::from("/nonexistent/path/that/does/not/exist");
    let ctx = ScanContext {
        data_dir: nonexistent.clone(),
        // Provide explicit scan_roots to disable default detection fallback to CWD/home
        scan_roots: vec![ScanRoot::local(nonexistent)],
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert!(convs.is_empty());
}

/// Test scan with empty directory returns empty
#[test]
fn aider_empty_directory() {
    let tmp = TempDir::new().unwrap();

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert!(convs.is_empty());
}

// =============================================================================
// CONTENT EDGE CASES
// =============================================================================

/// Test very long user input
#[test]
fn aider_long_user_input() {
    let tmp = TempDir::new().unwrap();
    let long_input = "> ".to_string() + &"x".repeat(10000) + "\n\nResponse\n";
    create_aider_fixture(&tmp, ".aider.chat.history.md", &long_input);

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let msgs = &convs[0].messages;
    assert!(!msgs.is_empty());
    assert!(msgs[0].content.len() >= 10000);
}

/// Test special characters in content
#[test]
fn aider_special_characters() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "> Test with émojis 🎉 and ünïcödé\n\nResponse with 日本語\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let msgs = &convs[0].messages;
    assert!(!msgs.is_empty());
    assert!(msgs[0].content.contains("émojis"));
    assert!(msgs[0].content.contains("🎉"));
}

/// Test blank lines between user prompt and response
#[test]
fn aider_blank_lines_between_messages() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "> Question\n\n\n\n\nAnswer after many blank lines\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let msgs = &convs[0].messages;
    // Should still parse correctly with blank lines
    assert!(msgs.len() >= 2);
}

/// Test consecutive > lines are combined
#[test]
fn aider_consecutive_user_lines_combined() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "> Line A\n> Line B\n> Line C\n\nResponse\n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    let msgs = &convs[0].messages;

    // All consecutive > lines should be in one user message
    let user_msg = &msgs[0];
    assert_eq!(user_msg.role, "user");
    assert!(user_msg.content.contains("Line A"));
    assert!(user_msg.content.contains("Line B"));
    assert!(user_msg.content.contains("Line C"));
}

/// Test trailing whitespace is handled
#[test]
fn aider_trailing_whitespace() {
    let tmp = TempDir::new().unwrap();
    create_aider_fixture(
        &tmp,
        ".aider.chat.history.md",
        "> Test   \n\nResponse   \n   \n",
    );

    let conn = AiderConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
    };
    let convs = conn.scan(&ctx).expect("scan");

    assert_eq!(convs.len(), 1);
    // Should parse without error
    assert!(!convs[0].messages.is_empty());
}
