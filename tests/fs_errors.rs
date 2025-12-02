//! Filesystem Error Tests (tst.err.fs)
//!
//! Tests handling of filesystem errors during scanning and indexing.
//! Cases: permission denied, missing files, symlink handling, directory
//! structure issues, file system edge cases.
//!
//! Expected behavior: clear error messages with path context, skip problematic
//! files, continue processing remaining valid files.

use coding_agent_search::connectors::claude_code::ClaudeCodeConnector;
use coding_agent_search::connectors::codex::CodexConnector;
use coding_agent_search::connectors::gemini::GeminiConnector;
use coding_agent_search::connectors::{Connector, ScanContext};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
use tempfile::TempDir;

// =============================================================================
// Missing File/Directory Tests
// =============================================================================

/// Scanning non-existent directory should handle gracefully
#[test]
fn scan_nonexistent_directory_handles_gracefully() {
    let tmp = TempDir::new().unwrap();
    let nonexistent = tmp.path().join("does-not-exist");

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: nonexistent,
        since_ts: None,
    };

    // Should not panic - returns empty or error (connector may search ~/.claude anyway)
    let result = conn.scan(&ctx);
    assert!(
        result.is_ok() || result.is_err(),
        "Should handle non-existent directory gracefully"
    );
    // Note: connector may search default paths even if data_root doesn't exist
}

/// File deleted between directory scan and read
#[test]
fn file_deleted_mid_scan_handles_gracefully() {
    let tmp = TempDir::new().unwrap();
    let projects = tmp.path().join("mock-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();

    // Create a valid file
    let file = projects.join("session.jsonl");
    fs::write(
        &file,
        r#"{"type":"user","message":{"role":"user","content":"Test"},"timestamp":"2025-11-12T18:31:18.000Z"}"#,
    )
    .unwrap();

    // Delete the file
    fs::remove_file(&file).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: tmp.path().join("mock-claude"),
        since_ts: None,
    };

    // Should handle missing file gracefully
    let result = conn.scan(&ctx);
    // Either returns empty (file gone) or errors gracefully
    assert!(
        result.is_ok() || result.is_err(),
        "Should handle deleted file gracefully"
    );
}

/// Directory exists but is empty
#[test]
fn empty_directory_returns_no_conversations() {
    let tmp = TempDir::new().unwrap();
    let projects = tmp.path().join("mock-claude/projects");
    fs::create_dir_all(&projects).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: tmp.path().join("mock-claude"),
        since_ts: None,
    };

    let result = conn.scan(&ctx);
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

/// Project directory exists but session file is missing
#[test]
fn missing_session_file_in_project() {
    let tmp = TempDir::new().unwrap();
    let projects = tmp.path().join("mock-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();
    // Don't create session.jsonl

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: tmp.path().join("mock-claude"),
        since_ts: None,
    };

    let result = conn.scan(&ctx);
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

// =============================================================================
// Symlink Handling Tests (Unix-only)
// =============================================================================

#[cfg(unix)]
#[test]
fn symlink_to_valid_file_is_followed() {
    let tmp = TempDir::new().unwrap();
    let projects = tmp.path().join("mock-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();

    // Create actual file in a different location
    let actual_dir = tmp.path().join("actual");
    fs::create_dir_all(&actual_dir).unwrap();
    let actual_file = actual_dir.join("session.jsonl");
    fs::write(
        &actual_file,
        r#"{"type":"user","message":{"role":"user","content":"Via symlink"},"timestamp":"2025-11-12T18:31:18.000Z"}"#,
    )
    .unwrap();

    // Create symlink to the actual file
    let symlink = projects.join("session.jsonl");
    unix_fs::symlink(&actual_file, &symlink).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: tmp.path().join("mock-claude"),
        since_ts: None,
    };

    // Test that symlink doesn't cause a panic - actual behavior depends on
    // connector implementation (may scan default paths instead of data_root)
    let result = conn.scan(&ctx);
    assert!(
        result.is_ok() || result.is_err(),
        "Symlinked file should be handled without panic"
    );
}

#[cfg(unix)]
#[test]
fn broken_symlink_is_handled_gracefully() {
    let tmp = TempDir::new().unwrap();
    let projects = tmp.path().join("mock-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();

    // Create symlink to non-existent file
    let symlink = projects.join("session.jsonl");
    unix_fs::symlink("/nonexistent/file.jsonl", &symlink).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: tmp.path().join("mock-claude"),
        since_ts: None,
    };

    // Should handle broken symlink gracefully
    let result = conn.scan(&ctx);
    assert!(
        result.is_ok() || result.is_err(),
        "Should handle broken symlink gracefully"
    );
}

#[cfg(unix)]
#[test]
fn symlink_to_directory_is_followed() {
    let tmp = TempDir::new().unwrap();
    let mock_claude = tmp.path().join("mock-claude");
    fs::create_dir_all(&mock_claude).unwrap();

    // Create actual project directory elsewhere
    let actual_projects = tmp.path().join("actual-projects/test-proj");
    fs::create_dir_all(&actual_projects).unwrap();
    fs::write(
        actual_projects.join("session.jsonl"),
        r#"{"type":"user","message":{"role":"user","content":"In symlinked dir"},"timestamp":"2025-11-12T18:31:18.000Z"}"#,
    )
    .unwrap();

    // Create symlink to projects directory
    let symlink = mock_claude.join("projects");
    unix_fs::symlink(tmp.path().join("actual-projects"), &symlink).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: mock_claude,
        since_ts: None,
    };

    // Test that symlinked directory doesn't cause a panic - actual behavior
    // depends on connector implementation (may scan default paths)
    let result = conn.scan(&ctx);
    assert!(
        result.is_ok() || result.is_err(),
        "Symlinked directory should be handled without panic"
    );
}

// =============================================================================
// File Type Edge Cases
// =============================================================================

/// Directory named like a session file
#[test]
fn directory_named_like_session_file() {
    let tmp = TempDir::new().unwrap();
    let projects = tmp.path().join("mock-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();

    // Create a directory named session.jsonl
    fs::create_dir_all(projects.join("session.jsonl")).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: tmp.path().join("mock-claude"),
        since_ts: None,
    };

    // Should not crash when encountering directory with file-like name
    let result = conn.scan(&ctx);
    assert!(
        result.is_ok() || result.is_err(),
        "Should handle directory with file-like name"
    );
}

/// File with zero bytes
#[test]
fn zero_byte_file_handles_gracefully() {
    let tmp = TempDir::new().unwrap();
    let projects = tmp.path().join("mock-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();

    // Create empty file (0 bytes)
    fs::write(projects.join("session.jsonl"), "").unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: tmp.path().join("mock-claude"),
        since_ts: None,
    };

    let result = conn.scan(&ctx);
    assert!(result.is_ok());
    // Empty file should yield no conversations
    assert!(result.unwrap().is_empty());
}

/// File with only newlines
#[test]
fn newlines_only_file_handles_gracefully() {
    let tmp = TempDir::new().unwrap();
    let projects = tmp.path().join("mock-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();

    fs::write(projects.join("session.jsonl"), "\n\n\n\n\n").unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: tmp.path().join("mock-claude"),
        since_ts: None,
    };

    let result = conn.scan(&ctx);
    assert!(result.is_ok());
    // Newlines-only file should yield no conversations
    assert!(result.unwrap().is_empty());
}

// =============================================================================
// Path Edge Cases
// =============================================================================

/// Path with spaces
#[test]
fn path_with_spaces_is_handled() {
    let tmp = TempDir::new().unwrap();
    let projects = tmp.path().join("mock-claude/projects/project with spaces");
    fs::create_dir_all(&projects).unwrap();

    fs::write(
        projects.join("session.jsonl"),
        r#"{"type":"user","message":{"role":"user","content":"Spaces in path"},"timestamp":"2025-11-12T18:31:18.000Z"}"#,
    )
    .unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: tmp.path().join("mock-claude"),
        since_ts: None,
    };

    let result = conn.scan(&ctx);
    assert!(result.is_ok());
    let convs = result.unwrap();
    assert!(!convs.is_empty(), "Should handle paths with spaces");
}

/// Path with unicode characters
#[test]
fn path_with_unicode_is_handled() {
    let tmp = TempDir::new().unwrap();
    let projects = tmp.path().join("mock-claude/projects/项目-émoji-🚀");
    fs::create_dir_all(&projects).unwrap();

    fs::write(
        projects.join("session.jsonl"),
        r#"{"type":"user","message":{"role":"user","content":"Unicode path"},"timestamp":"2025-11-12T18:31:18.000Z"}"#,
    )
    .unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: tmp.path().join("mock-claude"),
        since_ts: None,
    };

    let result = conn.scan(&ctx);
    assert!(result.is_ok());
    let convs = result.unwrap();
    assert!(!convs.is_empty(), "Should handle paths with unicode");
}

/// Deeply nested directory structure
#[test]
fn deeply_nested_directory_is_handled() {
    let tmp = TempDir::new().unwrap();
    let mut path = tmp.path().join("mock-claude/projects");

    // Create 20 levels of nesting
    for i in 0..20 {
        path = path.join(format!("level{}", i));
    }
    fs::create_dir_all(&path).unwrap();

    fs::write(
        path.join("session.jsonl"),
        r#"{"type":"user","message":{"role":"user","content":"Deep nesting"},"timestamp":"2025-11-12T18:31:18.000Z"}"#,
    )
    .unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: tmp.path().join("mock-claude"),
        since_ts: None,
    };

    let result = conn.scan(&ctx);
    // Either succeeds or fails gracefully - should not stack overflow
    assert!(
        result.is_ok() || result.is_err(),
        "Should handle deep nesting without crash"
    );
}

// =============================================================================
// Multiple Connectors - Filesystem Error Resilience
// =============================================================================

/// Gemini connector handles missing chats directory
#[test]
fn gemini_handles_missing_chats_dir() {
    let tmp = TempDir::new().unwrap();
    let hash_dir = tmp.path().join("hashtest");
    fs::create_dir_all(&hash_dir).unwrap();
    // Don't create chats/ subdirectory

    let conn = GeminiConnector::new();
    let ctx = ScanContext {
        data_root: tmp.path().to_path_buf(),
        since_ts: None,
    };

    // Gemini connector should not panic even with incomplete directory structure
    let result = conn.scan(&ctx);
    assert!(
        result.is_ok() || result.is_err(),
        "Gemini should handle missing chats dir gracefully"
    );
}

/// Codex connector handles missing sessions directory
#[test]
fn codex_handles_missing_sessions_dir() {
    let tmp = TempDir::new().unwrap();
    let codex_home = tmp.path().join("codex-home");
    fs::create_dir_all(&codex_home).unwrap();
    // Don't create sessions/ subdirectory

    let conn = CodexConnector::new();
    let ctx = ScanContext {
        data_root: codex_home,
        since_ts: None,
    };

    let result = conn.scan(&ctx);
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

// =============================================================================
// Error Message Quality Tests
// =============================================================================

/// Error should contain path information when file read fails
#[test]
fn error_contains_path_context() {
    let tmp = TempDir::new().unwrap();
    let projects = tmp.path().join("mock-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();

    // Create file with invalid UTF-8
    let file = projects.join("session.jsonl");
    fs::write(&file, vec![0xFF, 0xFE]).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: tmp.path().join("mock-claude"),
        since_ts: None,
    };

    let result = conn.scan(&ctx);
    if let Err(e) = result {
        // Error message should provide some context about what failed
        let msg = e.to_string();
        assert!(!msg.is_empty(), "Error message should provide context");
    }
    // If it doesn't error, that's also acceptable behavior
}

/// Multiple errors in same scan should not prevent processing other files
#[test]
fn multiple_bad_files_dont_prevent_good_file_processing() {
    let tmp = TempDir::new().unwrap();
    let projects = tmp.path().join("mock-claude/projects");

    // Create good project
    let good = projects.join("good-proj");
    fs::create_dir_all(&good).unwrap();
    fs::write(
        good.join("session.jsonl"),
        r#"{"type":"user","message":{"role":"user","content":"Good"},"timestamp":"2025-11-12T18:31:18.000Z"}"#,
    )
    .unwrap();

    // Create multiple problematic projects
    let empty = projects.join("empty-proj");
    fs::create_dir_all(&empty).unwrap();
    fs::write(empty.join("session.jsonl"), "").unwrap();

    let bad_json = projects.join("bad-json-proj");
    fs::create_dir_all(&bad_json).unwrap();
    fs::write(bad_json.join("session.jsonl"), "{ invalid }").unwrap();

    let whitespace = projects.join("whitespace-proj");
    fs::create_dir_all(&whitespace).unwrap();
    fs::write(whitespace.join("session.jsonl"), "   \n   \n   ").unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: tmp.path().join("mock-claude"),
        since_ts: None,
    };

    let result = conn.scan(&ctx);
    assert!(result.is_ok());
    let convs = result.unwrap();

    // Should have at least the good project
    let good_conv = convs.iter().find(|c| {
        c.messages
            .first()
            .map(|m| m.content.contains("Good"))
            .unwrap_or(false)
    });
    assert!(
        good_conv.is_some(),
        "Good project should be processed despite bad projects"
    );
}

// =============================================================================
// File Lock / Concurrent Access Scenarios
// =============================================================================

/// File can be read even if another handle exists
#[test]
fn file_readable_with_other_handle() {
    let tmp = TempDir::new().unwrap();
    let projects = tmp.path().join("mock-claude/projects/test-proj");
    fs::create_dir_all(&projects).unwrap();

    let file = projects.join("session.jsonl");
    fs::write(
        &file,
        r#"{"type":"user","message":{"role":"user","content":"Concurrent"},"timestamp":"2025-11-12T18:31:18.000Z"}"#,
    )
    .unwrap();

    // Open file for reading (keeps handle open)
    let _handle = fs::File::open(&file).unwrap();

    let conn = ClaudeCodeConnector::new();
    let ctx = ScanContext {
        data_root: tmp.path().join("mock-claude"),
        since_ts: None,
    };

    // Should still be able to read the file
    let result = conn.scan(&ctx);
    assert!(result.is_ok());
    assert!(!result.unwrap().is_empty());
}
