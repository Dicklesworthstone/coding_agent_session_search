//! Test fixtures for HTML export testing.
//!
//! This module provides comprehensive test fixtures for the html_export module:
//!
//! ## Directory Structure
//!
//! ```text
//! tests/fixtures/html_export/
//! ├── real_sessions/          # Realistic multi-turn conversations
//! │   ├── claude_code_auth_fix.jsonl     # Auth debugging with code
//! │   ├── cursor_refactoring.jsonl       # Refactoring with large code blocks
//! │   ├── codex_api_design.jsonl         # API design with diagrams
//! │   └── gemini_debugging.jsonl         # Debugging with stack traces
//! │
//! ├── edge_cases/             # Edge case scenarios
//! │   ├── empty_session.jsonl            # No messages
//! │   ├── single_message.jsonl           # Single user message
//! │   ├── unicode_heavy.jsonl            # Japanese, Chinese, emoji, RTL, math
//! │   ├── all_message_types.jsonl        # system, user, assistant, tool
//! │   └── large_session.jsonl            # 1000 messages
//! │
//! └── malformed/              # Invalid/corrupt data for error handling tests
//!     ├── truncated.jsonl                # Valid JSON cut mid-stream
//!     ├── invalid_json.jsonl             # Syntax errors
//!     ├── missing_fields.jsonl           # Required fields absent
//!     └── wrong_types.jsonl              # Type mismatches
//! ```
//!
//! ## Usage in Tests
//!
//! ```rust,ignore
//! use std::path::PathBuf;
//!
//! fn fixture_path(category: &str, name: &str) -> PathBuf {
//!     PathBuf::from(env!("CARGO_MANIFEST_DIR"))
//!         .join("tests/fixtures/html_export")
//!         .join(category)
//!         .join(name)
//! }
//!
//! #[test]
//! fn test_render_real_session() {
//!     let path = fixture_path("real_sessions", "claude_code_auth_fix.jsonl");
//!     let content = std::fs::read_to_string(&path).unwrap();
//!     // ... test rendering
//! }
//! ```

use std::path::PathBuf;

/// Get the path to a fixture file.
pub fn fixture_path(category: &str, name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/html_export")
        .join(category)
        .join(name)
}

/// Get all fixture files in a category.
pub fn fixtures_in_category(category: &str) -> Vec<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/html_export")
        .join(category);

    if !dir.exists() {
        return Vec::new();
    }

    std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .map(|ext| ext == "jsonl")
                .unwrap_or(false)
        })
        .map(|entry| entry.path())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Validate that all real session fixtures are valid JSONL.
    #[test]
    fn validate_real_session_fixtures() {
        let fixtures = fixtures_in_category("real_sessions");
        assert!(!fixtures.is_empty(), "No real session fixtures found");

        for path in fixtures {
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("Failed to read {:?}: {}", path, e));

            let mut line_count = 0;
            for (line_num, line) in content.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                line_count += 1;

                serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|e| {
                    panic!(
                        "Invalid JSON in {:?} at line {}: {}",
                        path.file_name().unwrap(),
                        line_num + 1,
                        e
                    )
                });
            }

            assert!(
                line_count >= 5,
                "Real session {:?} should have at least 5 messages, got {}",
                path.file_name().unwrap(),
                line_count
            );
        }
    }

    /// Validate edge case fixtures exist and are readable.
    #[test]
    fn validate_edge_case_fixtures() {
        let expected = [
            "empty_session.jsonl",
            "single_message.jsonl",
            "unicode_heavy.jsonl",
            "all_message_types.jsonl",
            "large_session.jsonl",
        ];

        for name in expected {
            let path = fixture_path("edge_cases", name);
            assert!(path.exists(), "Missing edge case fixture: {}", name);

            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("Failed to read {}: {}", name, e));

            // At least one valid JSON line (except empty_session which may have metadata only)
            let valid_json_count = content
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
                .count();

            assert!(
                valid_json_count >= 1,
                "Edge case {} should have at least 1 valid JSON line",
                name
            );
        }
    }

    /// Validate malformed fixtures trigger expected parse errors.
    #[test]
    fn validate_malformed_fixtures() {
        let malformed = fixtures_in_category("malformed");
        assert!(!malformed.is_empty(), "No malformed fixtures found");

        for path in malformed {
            let content = std::fs::read_to_string(&path).unwrap();

            // Malformed fixtures should have at least one invalid line
            let has_invalid = content.lines().any(|line| {
                !line.trim().is_empty() && serde_json::from_str::<serde_json::Value>(line).is_err()
            });

            assert!(
                has_invalid,
                "Malformed fixture {:?} should contain at least one invalid JSON line",
                path.file_name().unwrap()
            );
        }
    }

    /// Validate the large session has 1000+ messages.
    #[test]
    fn validate_large_session_size() {
        let path = fixture_path("edge_cases", "large_session.jsonl");
        let content = std::fs::read_to_string(&path).expect("large_session.jsonl should exist");

        let line_count = content.lines().filter(|l| !l.trim().is_empty()).count();
        assert!(
            line_count >= 1000,
            "Large session should have at least 1000 messages, got {}",
            line_count
        );
    }

    /// Validate unicode fixture contains multi-byte characters.
    #[test]
    fn validate_unicode_fixture_content() {
        let path = fixture_path("edge_cases", "unicode_heavy.jsonl");
        let content = std::fs::read_to_string(&path).expect("unicode_heavy.jsonl should exist");

        // Check for various Unicode categories
        assert!(content.contains("日本語"), "Should contain Japanese");
        assert!(content.contains("中文"), "Should contain Chinese");
        assert!(content.contains("🎉"), "Should contain emoji");
        assert!(content.contains("مرحبا"), "Should contain Arabic");
        assert!(
            content.contains("∫∑∏"),
            "Should contain mathematical symbols"
        );
    }

    /// Validate all message types are represented.
    #[test]
    fn validate_all_message_types() {
        let path = fixture_path("edge_cases", "all_message_types.jsonl");
        let content = std::fs::read_to_string(&path).expect("all_message_types.jsonl should exist");

        let roles = ["user", "assistant", "tool", "system"];
        for role in roles {
            assert!(
                content.contains(&format!("\"role\":\"{}", role))
                    || content.contains(&format!("\"role\": \"{}\"", role)),
                "all_message_types.jsonl should contain role: {}",
                role
            );
        }
    }
}
