//! Integration tests for HTML export pipeline.
//!
//! These tests verify the complete HTML export pipeline works end-to-end with
//! real session data, comprehensive content verification, and CLI integration.
//!
//! Test categories:
//! - Export pipeline (complete success flow)
//! - Message type preservation
//! - Large session handling
//! - Encrypted export flow
//! - CLI integration (robot mode)
//! - Cross-platform path handling
//! - Performance benchmarks

#![allow(clippy::collapsible_if)]

use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Get the path to a fixture file.
fn fixture_path(category: &str, name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/html_export")
        .join(category)
        .join(name)
}

/// Create a minimal test session JSONL file.
fn create_test_session(dir: &Path, messages: &[(&str, &str)]) -> PathBuf {
    let file = dir.join("test_session.jsonl");
    let mut content = String::new();
    for (i, (role, text)) in messages.iter().enumerate() {
        let ts = 1705334400000i64 + (i as i64 * 60000);
        content.push_str(&format!(
            r#"{{"type":"{role}","timestamp":{ts},"message":{{"role":"{role}","content":"{text}"}}}}"#
        ));
        content.push('\n');
    }
    fs::write(&file, content).unwrap();
    file
}

#[allow(deprecated)]
fn base_cmd() -> Command {
    let mut cmd = Command::cargo_bin("cass").unwrap();
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd
}

// =============================================================================
// Export Pipeline Tests
// =============================================================================

#[test]
fn test_export_pipeline_complete_success() {
    let session_path = fixture_path("real_sessions", "claude_code_auth_fix.jsonl");
    let tmp = TempDir::new().unwrap();
    let output_dir = tmp.path();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            output_dir.to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Export should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Parse JSON output
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).expect("Output should be valid JSON");

    assert_eq!(json["success"], true);
    assert!(json["exported"]["output_path"].as_str().is_some());
    assert!(json["exported"]["size_bytes"].as_u64().unwrap() > 0);

    // Verify file exists and is valid HTML
    let output_path = json["exported"]["output_path"].as_str().unwrap();
    let html = fs::read_to_string(output_path).expect("Should be able to read output file");

    assert!(
        html.starts_with("<!DOCTYPE html>"),
        "Should start with doctype"
    );
    assert!(html.contains("</html>"), "Should be complete HTML");
    assert!(html.contains("<title>"), "Should have title element");
}

#[test]
fn test_export_preserves_message_content() {
    let session_path = fixture_path("real_sessions", "claude_code_auth_fix.jsonl");
    let tmp = TempDir::new().unwrap();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();
    let output_path = json["exported"]["output_path"].as_str().unwrap();
    let html = fs::read_to_string(output_path).unwrap();

    // Check for content from the fixture
    assert!(
        html.contains("JWT") || html.contains("token"),
        "Should contain JWT/token content from auth fix session"
    );
}

#[test]
fn test_export_all_message_types() {
    let session_path = fixture_path("edge_cases", "all_message_types.jsonl");
    let tmp = TempDir::new().unwrap();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
            "--include-tools",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();
    let output_path = json["exported"]["output_path"].as_str().unwrap();
    let html = fs::read_to_string(output_path).unwrap();

    // Verify different message types are rendered with appropriate classes
    assert!(
        html.contains("message-user") || html.contains("user"),
        "Should render user messages"
    );
    assert!(
        html.contains("message-assistant") || html.contains("assistant"),
        "Should render assistant messages"
    );
}

#[test]
fn test_export_large_session_performance() {
    let session_path = fixture_path("edge_cases", "large_session.jsonl");
    let tmp = TempDir::new().unwrap();

    let start = std::time::Instant::now();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    let elapsed = start.elapsed();

    assert!(
        output.status.success(),
        "Large session export should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Performance: should complete in reasonable time
    assert!(
        elapsed.as_secs() < 30,
        "Export took too long: {:?}",
        elapsed
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();
    let output_path = json["exported"]["output_path"].as_str().unwrap();
    let html = fs::read_to_string(output_path).unwrap();

    // Verify file is substantial (1000 messages should produce sizable output)
    assert!(
        html.len() > 50000,
        "Large session should produce substantial HTML output"
    );
}

#[test]
fn test_export_unicode_content() {
    let session_path = fixture_path("edge_cases", "unicode_heavy.jsonl");
    let tmp = TempDir::new().unwrap();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();
    let output_path = json["exported"]["output_path"].as_str().unwrap();
    let html = fs::read_to_string(output_path).unwrap();

    // Verify unicode content is preserved and properly escaped
    assert!(html.contains("UTF-8"), "Should declare UTF-8 charset");
    // The actual unicode content may be HTML-escaped
    assert!(
        html.contains("日本語") || html.contains("&#"),
        "Should contain Japanese or HTML entities"
    );
}

// =============================================================================
// Encrypted Export Tests
// =============================================================================

#[test]
fn test_encrypted_export_flow() {
    let session_path = fixture_path("real_sessions", "claude_code_auth_fix.jsonl");
    let tmp = TempDir::new().unwrap();
    let password = "test-password-secure-123";

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
            "--encrypt",
            "--password",
            password,
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(json["success"], true);
    assert_eq!(json["exported"]["encrypted"], true);

    let output_path = json["exported"]["output_path"].as_str().unwrap();
    let html = fs::read_to_string(output_path).unwrap();

    // Verify decryption infrastructure present
    assert!(
        html.contains("crypto.subtle") || html.contains("SubtleCrypto"),
        "Web Crypto API code should be present"
    );
    assert!(
        html.contains("decrypt"),
        "Decrypt function should be present"
    );
    assert!(
        html.contains("password") || html.contains("Password"),
        "Password input should be present"
    );

    // Verify encrypted payload structure
    assert!(
        html.contains("salt") || html.contains("iv") || html.contains("ciphertext"),
        "Encryption payload markers should be present"
    );

    // Verify plaintext content is NOT directly visible
    // (The actual message content should be encrypted)
    let session_content = fs::read_to_string(&session_path).unwrap();
    let first_line: Value = serde_json::from_str(session_content.lines().next().unwrap()).unwrap();
    if let Some(msg) = first_line.get("message") {
        if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
            // Long plaintext content shouldn't appear verbatim
            if content.len() > 50 {
                assert!(
                    !html.contains(&content[..50]),
                    "Plaintext content should not be directly visible"
                );
            }
        }
    }
}

#[test]
fn test_encrypted_export_requires_password() {
    let session_path = fixture_path("real_sessions", "claude_code_auth_fix.jsonl");
    let tmp = TempDir::new().unwrap();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
            "--encrypt",
            // Missing --password
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(json["success"], false);
    assert_eq!(json["error"]["kind"], "password_required");
}

// =============================================================================
// CLI Integration Tests
// =============================================================================

#[test]
fn test_cli_export_basic() {
    let session_path = fixture_path("real_sessions", "claude_code_auth_fix.jsonl");
    let tmp = TempDir::new().unwrap();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(json["success"], true);
    assert!(json["exported"]["output_path"].as_str().is_some());
    assert!(json["exported"]["size_bytes"].as_u64().unwrap() > 0);
    assert!(json["exported"]["messages_count"].as_u64().unwrap() > 0);
}

#[test]
fn test_cli_export_dry_run() {
    let session_path = fixture_path("real_sessions", "claude_code_auth_fix.jsonl");
    let tmp = TempDir::new().unwrap();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
            "--dry-run",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();

    assert!(json["dry_run"].as_bool().unwrap());
    assert!(json["valid"].as_bool().unwrap());

    // Verify no file actually written
    let output_path = json["output_path"].as_str().unwrap();
    assert!(
        !Path::new(output_path).exists(),
        "Dry run should not create file"
    );
}

#[test]
fn test_cli_export_explain() {
    let session_path = fixture_path("real_sessions", "claude_code_auth_fix.jsonl");

    let output = base_cmd()
        .args(["export-html", session_path.to_str().unwrap(), "--explain"])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();

    assert!(json["plan"].is_object());
    assert!(json["plan"]["session_path"].as_str().is_some());
    assert!(json["plan"]["messages"].as_u64().unwrap() > 0);
}

#[test]
fn test_cli_export_session_not_found() {
    let output = base_cmd()
        .args(["export-html", "/nonexistent/path/session.jsonl", "--robot"])
        .output()
        .unwrap();

    assert!(!output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(json["success"], false);
    assert_eq!(json["error"]["kind"], "session_not_found");
}

#[test]
fn test_cli_export_with_custom_filename() {
    let session_path = fixture_path("real_sessions", "claude_code_auth_fix.jsonl");
    let tmp = TempDir::new().unwrap();
    let custom_name = "my_export.html";

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--filename",
            custom_name,
            "--robot",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();

    let output_path = json["exported"]["output_path"].as_str().unwrap();
    assert!(
        output_path.ends_with(custom_name),
        "Output should use custom filename"
    );
    assert!(Path::new(output_path).exists());
}

#[test]
fn test_cli_export_with_options() {
    let session_path = fixture_path("real_sessions", "claude_code_auth_fix.jsonl");
    let tmp = TempDir::new().unwrap();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
            "--include-tools",
            "--show-timestamps",
            "--theme",
            "dark",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();
    let output_path = json["exported"]["output_path"].as_str().unwrap();
    let html = fs::read_to_string(output_path).unwrap();

    // Timestamps should be visible
    assert!(
        html.contains("timestamp") || html.contains("time"),
        "Should include timestamp styling/content"
    );
}

#[test]
fn test_cli_export_no_cdn() {
    let session_path = fixture_path("real_sessions", "claude_code_auth_fix.jsonl");
    let tmp = TempDir::new().unwrap();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
            "--no-cdns",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();
    let output_path = json["exported"]["output_path"].as_str().unwrap();
    let html = fs::read_to_string(output_path).unwrap();

    // Should still be valid HTML without CDN
    assert!(html.contains("<!DOCTYPE html>"));
    assert!(html.contains("</html>"));
    // Critical styles should still be inlined
    assert!(html.contains("<style>"));
}

// =============================================================================
// Edge Case Tests
// =============================================================================

#[test]
fn test_export_empty_session() {
    let session_path = fixture_path("edge_cases", "empty_session.jsonl");
    let tmp = TempDir::new().unwrap();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    // Empty session might fail or produce minimal output
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() {
        let json: Value = serde_json::from_str(&stdout).unwrap();
        // If success, file should exist
        if let Some(path) = json["exported"]["output_path"].as_str() {
            assert!(Path::new(path).exists());
        }
    } else {
        // If failure, should have appropriate error
        let json: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(json["success"], false);
    }
}

#[test]
fn test_export_single_message() {
    let session_path = fixture_path("edge_cases", "single_message.jsonl");
    let tmp = TempDir::new().unwrap();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(json["success"], true);
    assert_eq!(json["exported"]["messages_count"], 1);
}

// =============================================================================
// Malformed Input Tests
// =============================================================================

#[test]
fn test_export_malformed_json_graceful_handling() {
    let session_path = fixture_path("malformed", "invalid_json.jsonl");
    let tmp = TempDir::new().unwrap();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    // Should either fail gracefully or skip invalid lines
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let json: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(json["success"], false);
        // Should have a meaningful error kind
        assert!(json["error"]["kind"].as_str().is_some());
    }
}

#[test]
fn test_export_truncated_file() {
    let session_path = fixture_path("malformed", "truncated.jsonl");
    let tmp = TempDir::new().unwrap();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    // Truncated file handling - should process valid lines
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() {
        let json: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(json["success"], true);
    }
}

// =============================================================================
// Performance Benchmarks
// =============================================================================

#[test]
fn benchmark_export_small_session() {
    let session_path = fixture_path("edge_cases", "single_message.jsonl");
    let tmp = TempDir::new().unwrap();

    let start = std::time::Instant::now();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    let elapsed = start.elapsed();

    assert!(output.status.success());
    assert!(
        elapsed.as_millis() < 2000,
        "Small session took too long: {:?}",
        elapsed
    );
}

#[test]
fn benchmark_export_medium_session() {
    let session_path = fixture_path("real_sessions", "claude_code_auth_fix.jsonl");
    let tmp = TempDir::new().unwrap();

    let start = std::time::Instant::now();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    let elapsed = start.elapsed();

    assert!(output.status.success());
    assert!(
        elapsed.as_secs() < 10,
        "Medium session took too long: {:?}",
        elapsed
    );
}

// =============================================================================
// Cross-Platform Tests
// =============================================================================

#[test]
fn test_export_creates_output_directory() {
    let session_path = fixture_path("edge_cases", "single_message.jsonl");
    let tmp = TempDir::new().unwrap();
    let nested_dir = tmp.path().join("nested").join("output").join("dir");

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            nested_dir.to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();
    let output_path = json["exported"]["output_path"].as_str().unwrap();

    assert!(Path::new(output_path).exists());
    assert!(nested_dir.exists());
}

#[test]
fn test_export_handles_special_characters_in_path() {
    let tmp = TempDir::new().unwrap();
    let special_dir = tmp.path().join("test with spaces");
    fs::create_dir_all(&special_dir).unwrap();

    // Create a test session in the special directory
    let session_path = create_test_session(&special_dir, &[("user", "Hello world")]);

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
}

// =============================================================================
// HTML Content Validation Tests
// =============================================================================

#[test]
fn test_export_html_structure() {
    let session_path = fixture_path("real_sessions", "claude_code_auth_fix.jsonl");
    let tmp = TempDir::new().unwrap();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();
    let output_path = json["exported"]["output_path"].as_str().unwrap();
    let html = fs::read_to_string(output_path).unwrap();

    // Verify HTML structure
    assert!(html.contains("<!DOCTYPE html>"));
    assert!(html.contains("<html"));
    assert!(html.contains("<head>"));
    assert!(html.contains("</head>"));
    assert!(html.contains("<body"));
    assert!(html.contains("</body>"));
    assert!(html.contains("</html>"));

    // Verify meta tags
    assert!(html.contains("charset"));
    assert!(html.contains("viewport"));

    // Verify styles are present
    assert!(html.contains("<style>"));
}

#[test]
fn test_export_has_theme_toggle() {
    let session_path = fixture_path("edge_cases", "single_message.jsonl");
    let tmp = TempDir::new().unwrap();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();
    let output_path = json["exported"]["output_path"].as_str().unwrap();
    let html = fs::read_to_string(output_path).unwrap();

    // Theme toggle should be present (button or functionality)
    assert!(
        html.contains("theme") || html.contains("dark") || html.contains("light"),
        "Should have theme-related content"
    );
}

#[test]
fn test_export_has_print_styles() {
    let session_path = fixture_path("edge_cases", "single_message.jsonl");
    let tmp = TempDir::new().unwrap();

    let output = base_cmd()
        .args([
            "export-html",
            session_path.to_str().unwrap(),
            "--output-dir",
            tmp.path().to_str().unwrap(),
            "--robot",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).unwrap();
    let output_path = json["exported"]["output_path"].as_str().unwrap();
    let html = fs::read_to_string(output_path).unwrap();

    // Print media query should be present
    assert!(html.contains("@media print"), "Should have print styles");
}

// =============================================================================
// Batch Export Simulation Tests
// =============================================================================

#[test]
fn test_export_multiple_sessions_sequentially() {
    let fixtures = [
        ("real_sessions", "claude_code_auth_fix.jsonl"),
        ("real_sessions", "cursor_refactoring.jsonl"),
        ("edge_cases", "single_message.jsonl"),
    ];

    let tmp = TempDir::new().unwrap();
    let mut output_paths = Vec::new();

    for (category, name) in &fixtures {
        let session_path = fixture_path(category, name);
        if !session_path.exists() {
            continue;
        }

        let output = base_cmd()
            .args([
                "export-html",
                session_path.to_str().unwrap(),
                "--output-dir",
                tmp.path().to_str().unwrap(),
                "--robot",
            ])
            .output()
            .unwrap();

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(json) = serde_json::from_str::<Value>(&stdout) {
                if let Some(path) = json["exported"]["output_path"].as_str() {
                    output_paths.push(path.to_string());
                }
            }
        }
    }

    // Verify at least some exports succeeded
    assert!(
        !output_paths.is_empty(),
        "At least one export should succeed"
    );

    // Verify all output files are distinct
    let unique_paths: std::collections::HashSet<_> = output_paths.iter().collect();
    assert_eq!(
        unique_paths.len(),
        output_paths.len(),
        "All output filenames should be unique"
    );
}
