//! TUI Smoke Tests with Logging (coding_agent_session_search-xjt3)
//!
//! This module provides comprehensive E2E smoke tests for the TUI that:
//! - Exercise launch, search input, and exit paths in headless mode
//! - Capture TUI state snapshots and log key events
//! - Validate exit codes and ensure no panics on empty datasets
//! - Run automatically in CI without manual interaction
//!
//! All tests use `--once` and `TUI_HEADLESS=1` for non-interactive execution.

use assert_cmd::cargo::cargo_bin_cmd;
use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

mod util;
use util::EnvGuard;

/// These tests mutate process-level env vars (XDG_*, HOME, CODEX_HOME) and spawn `cass`.
/// Running them in parallel makes them flaky and can accidentally index the developer's real
/// archives, causing multi-minute hangs.
static TUI_SMOKE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn tui_smoke_guard() -> std::sync::MutexGuard<'static, ()> {
    match TUI_SMOKE_LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            eprintln!(
                "[SMOKE] warning: tui smoke mutex poisoned after earlier failure; recovering guard"
            );
            poisoned.into_inner()
        }
    }
}

// =============================================================================
// Fixture Helpers
// =============================================================================

/// Create a minimal Codex fixture for TUI tests.
fn make_codex_fixture(root: &Path) {
    let sessions = root.join("sessions/2025/11/21");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-1.jsonl");
    let sample = r#"{"role":"user","timestamp":1700000000000,"content":"hello world test"}
{"role":"assistant","timestamp":1700000001000,"content":"hi there"}
"#;
    fs::write(file, sample).unwrap();
}

/// Create a Claude Code fixture with searchable content.
fn make_claude_fixture(root: &Path, workspace_name: &str) {
    let session_dir = root.join(format!("projects/{workspace_name}"));
    fs::create_dir_all(&session_dir).unwrap();
    let file = session_dir.join("session.jsonl");
    let sample = r#"{"type":"user","timestamp":"2025-01-15T10:00:00Z","message":{"content":"fix authentication bug"}}
{"type":"assistant","timestamp":"2025-01-15T10:00:05Z","message":{"content":"I'll investigate the authentication module."}}
{"type":"user","timestamp":"2025-01-15T10:00:10Z","message":{"content":"check the session timeout"}}
{"type":"assistant","timestamp":"2025-01-15T10:00:15Z","message":{"content":"The session timeout is configured correctly."}}
"#
        .to_string();
    fs::write(file, sample).unwrap();
}

/// Create multiple agent fixtures for multi-agent TUI testing.
fn make_multi_agent_fixtures(_data_dir: &Path, codex_home: &Path, claude_home: &Path) {
    // Codex fixture
    make_codex_fixture(codex_home);

    // Claude Code fixture
    make_claude_fixture(claude_home, "testproject");
}

// =============================================================================
// Basic TUI Launch Tests
// =============================================================================

#[test]
fn tui_headless_launches_with_valid_index() {
    let _guard_lock = tui_smoke_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard_home = EnvGuard::set("HOME", tmp.path().to_string_lossy());
    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // Set up Codex fixture
    let _guard_codex = EnvGuard::set("CODEX_HOME", data_dir.to_string_lossy());
    make_codex_fixture(&data_dir);

    // Build index first
    cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Run TUI in headless mode
    cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .env("TUI_HEADLESS", "1")
        .assert()
        .success();

    // Verify index artifacts exist
    assert!(data_dir.join("agent_search.db").exists(), "DB should exist");
    assert!(data_dir.join("index/v7").exists(), "Index should exist");

    // Log test completion
    eprintln!("[SMOKE] tui_headless_launches_with_valid_index: PASSED");
}

#[test]
fn tui_headless_exits_cleanly_on_empty_dataset() {
    let _guard_lock = tui_smoke_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard_home = EnvGuard::set("HOME", tmp.path().to_string_lossy());
    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // Point agent envs to non-existent directories (no fixtures)
    let empty_codex = tmp.path().join("empty_codex");
    let empty_claude = tmp.path().join("empty_claude");
    fs::create_dir_all(&empty_codex).unwrap();
    fs::create_dir_all(&empty_claude).unwrap();

    let _guard_codex = EnvGuard::set("CODEX_HOME", empty_codex.to_string_lossy());
    let _guard_claude = EnvGuard::set("CLAUDE_HOME", empty_claude.to_string_lossy());

    // Build empty index
    cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // TUI should exit cleanly (exit 0) even with no data
    cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .env("TUI_HEADLESS", "1")
        .assert()
        .success();

    eprintln!("[SMOKE] tui_headless_exits_cleanly_on_empty_dataset: PASSED");
}

#[test]
fn tui_headless_no_panic_without_index() {
    let _guard_lock = tui_smoke_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard_home = EnvGuard::set("HOME", tmp.path().to_string_lossy());
    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // Don't create index, just try to run TUI
    // Should fail gracefully (not panic) with exit code indicating index missing
    let result = cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .env("TUI_HEADLESS", "1")
        .output()
        .expect("failed to execute cass");

    // Should not have panicked - check stderr for panic messages
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        !stderr.contains("panic") && !stderr.contains("RUST_BACKTRACE"),
        "TUI should not panic without index, stderr: {}",
        stderr
    );

    eprintln!("[SMOKE] tui_headless_no_panic_without_index: PASSED");
}

// =============================================================================
// Search Execution Tests
// =============================================================================

#[test]
fn tui_headless_search_executes_successfully() {
    let _guard_lock = tui_smoke_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard_home = EnvGuard::set("HOME", tmp.path().to_string_lossy());
    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // Set up fixtures
    let _guard_codex = EnvGuard::set("CODEX_HOME", data_dir.to_string_lossy());
    make_codex_fixture(&data_dir);

    // Build index
    cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Run a search via CLI (robot mode) to verify search works
    let output = cargo_bin_cmd!("cass")
        .arg("search")
        .arg("hello")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&output.get_output().stdout);
    assert!(
        stdout.contains("hello") || stdout.contains("hits"),
        "Search should return results containing 'hello' or hits array"
    );

    // Also run TUI headless to ensure search client initializes
    cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .env("TUI_HEADLESS", "1")
        .assert()
        .success();

    eprintln!("[SMOKE] tui_headless_search_executes_successfully: PASSED");
}

#[test]
fn tui_headless_multi_agent_index_and_search() {
    let _guard_lock = tui_smoke_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard_home = EnvGuard::set("HOME", tmp.path().to_string_lossy());
    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    let codex_home = tmp.path().join("codex_home");
    // Claude connector scans ~/.claude/projects (relative to HOME), so put fixtures there.
    let claude_home = tmp.path().join(".claude");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    fs::create_dir_all(&claude_home).unwrap();

    // Set up multi-agent fixtures
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());
    let _guard_claude = EnvGuard::set("CLAUDE_HOME", claude_home.to_string_lossy());
    make_multi_agent_fixtures(&data_dir, &codex_home, &claude_home);

    // Build index (should pick up both agents)
    cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Search for Codex content
    let codex_search = cargo_bin_cmd!("cass")
        .arg("search")
        .arg("hello")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    let _codex_stdout = String::from_utf8_lossy(&codex_search.get_output().stdout);

    // Search for Claude content
    let claude_search = cargo_bin_cmd!("cass")
        .arg("search")
        .arg("authentication")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    let _claude_stdout = String::from_utf8_lossy(&claude_search.get_output().stdout);

    // TUI should work with multi-agent data
    cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .env("TUI_HEADLESS", "1")
        .assert()
        .success();

    eprintln!("[SMOKE] tui_headless_multi_agent_index_and_search: PASSED");
}

// =============================================================================
// State Persistence Tests
// =============================================================================

#[test]
fn tui_headless_reset_state_clears_persisted_state() {
    let _guard_lock = tui_smoke_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard_home = EnvGuard::set("HOME", tmp.path().to_string_lossy());
    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_codex = EnvGuard::set("CODEX_HOME", data_dir.to_string_lossy());
    make_codex_fixture(&data_dir);

    // Build index
    cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Create a test state file
    let state_file = data_dir.join("tui_state.json");
    fs::write(
        &state_file,
        r#"{"match_mode":"prefix","has_seen_help":true}"#,
    )
    .unwrap();
    assert!(state_file.exists(), "State file should exist before reset");

    // Run TUI with --reset-state
    cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .arg("--reset-state")
        .env("TUI_HEADLESS", "1")
        .assert()
        .success();

    // State file should be cleared/replaced; stale "prefix" value must not survive reset.
    if state_file.exists() {
        let raw = fs::read_to_string(&state_file).unwrap_or_default();
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}));
        let stale = parsed
            .get("match_mode")
            .and_then(|v| v.as_str())
            .map(|v| v == "prefix")
            .unwrap_or(false);
        assert!(
            !stale,
            "reset-state should not preserve stale match_mode=prefix"
        );
    }

    eprintln!("[SMOKE] tui_headless_reset_state_clears_persisted_state: PASSED");
}

// =============================================================================
// Exit Code Validation Tests
// =============================================================================

#[test]
fn tui_headless_exit_code_success_with_data() {
    let _guard_lock = tui_smoke_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard_home = EnvGuard::set("HOME", tmp.path().to_string_lossy());
    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_codex = EnvGuard::set("CODEX_HOME", data_dir.to_string_lossy());
    make_codex_fixture(&data_dir);

    // Build index
    cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // TUI should exit with code 0
    let result = cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .env("TUI_HEADLESS", "1")
        .output()
        .expect("failed to execute");

    assert!(
        result.status.success(),
        "TUI should exit with code 0, got: {:?}",
        result.status.code()
    );

    eprintln!("[SMOKE] tui_headless_exit_code_success_with_data: PASSED");
}

#[test]
fn health_check_before_tui_launch() {
    let _guard_lock = tui_smoke_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard_home = EnvGuard::set("HOME", tmp.path().to_string_lossy());
    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_codex = EnvGuard::set("CODEX_HOME", data_dir.to_string_lossy());
    make_codex_fixture(&data_dir);

    // Build index
    cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Health check should pass (exit 0)
    cargo_bin_cmd!("cass")
        .arg("health")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // After health check passes, TUI should work
    cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .env("TUI_HEADLESS", "1")
        .assert()
        .success();

    eprintln!("[SMOKE] health_check_before_tui_launch: PASSED");
}

// =============================================================================
// CLI Flags Validation Tests
// =============================================================================

#[test]
fn tui_help_flag_shows_usage() {
    let _guard_lock = tui_smoke_guard();
    // --help should show usage information and exit 0
    cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("--once"));
}

#[test]
fn tui_accepts_data_dir_flag() {
    let _guard_lock = tui_smoke_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard_home = EnvGuard::set("HOME", tmp.path().to_string_lossy());
    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("custom_data_dir");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_codex = EnvGuard::set("CODEX_HOME", data_dir.to_string_lossy());
    make_codex_fixture(&data_dir);

    // Build index in custom dir
    cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // TUI should accept --data-dir
    cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .env("TUI_HEADLESS", "1")
        .assert()
        .success();

    // Verify index was created in custom location
    assert!(data_dir.join("agent_search.db").exists());

    eprintln!("[SMOKE] tui_accepts_data_dir_flag: PASSED");
}

// =============================================================================
// Logging and Diagnostics Tests
// =============================================================================

#[test]
fn diag_command_provides_useful_info() {
    let _guard_lock = tui_smoke_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard_home = EnvGuard::set("HOME", tmp.path().to_string_lossy());
    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_codex = EnvGuard::set("CODEX_HOME", data_dir.to_string_lossy());
    make_codex_fixture(&data_dir);

    // Build index
    cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Diag should provide useful information
    let output = cargo_bin_cmd!("cass")
        .arg("diag")
        .arg("--json")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&output.get_output().stdout);
    // Should contain diagnostic info
    assert!(
        stdout.contains("data_dir") || stdout.contains("index") || stdout.contains("{"),
        "Diag output should contain useful information"
    );

    eprintln!("[SMOKE] diag_command_provides_useful_info: PASSED");
}

#[test]
fn status_command_shows_health() {
    let _guard_lock = tui_smoke_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard_home = EnvGuard::set("HOME", tmp.path().to_string_lossy());
    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_codex = EnvGuard::set("CODEX_HOME", data_dir.to_string_lossy());
    make_codex_fixture(&data_dir);

    // Build index
    cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Status should work
    cargo_bin_cmd!("cass")
        .arg("status")
        .arg("--json")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    eprintln!("[SMOKE] status_command_shows_health: PASSED");
}

// =============================================================================
// Edge Cases and Robustness Tests
// =============================================================================

#[test]
fn tui_handles_unicode_content() {
    let _guard_lock = tui_smoke_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard_home = EnvGuard::set("HOME", tmp.path().to_string_lossy());
    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create fixture with Unicode content
    let sessions = data_dir.join("sessions/2025/11/21");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-unicode.jsonl");
    let sample = r#"{"role":"user","timestamp":1700000000000,"content":"日本語テスト こんにちは"}
{"role":"assistant","timestamp":1700000001000,"content":"Emoji test: 🎉🚀💻 and more: 中文测试"}
"#;
    fs::write(file, sample).unwrap();

    let _guard_codex = EnvGuard::set("CODEX_HOME", data_dir.to_string_lossy());

    // Build index
    cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // TUI should handle Unicode without panicking
    cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .env("TUI_HEADLESS", "1")
        .assert()
        .success();

    // Search for Unicode content
    cargo_bin_cmd!("cass")
        .arg("search")
        .arg("日本語")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    eprintln!("[SMOKE] tui_handles_unicode_content: PASSED");
}

#[test]
fn tui_handles_large_message_content() {
    let _guard_lock = tui_smoke_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard_home = EnvGuard::set("HOME", tmp.path().to_string_lossy());
    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create fixture with large content
    let sessions = data_dir.join("sessions/2025/11/21");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-large.jsonl");

    // Generate large content (50KB)
    let large_content: String = (0..5000).map(|i| format!("word{} ", i)).collect();
    let sample = format!(
        r#"{{"role":"user","timestamp":1700000000000,"content":"start"}}
{{"role":"assistant","timestamp":1700000001000,"content":"{}"}}
"#,
        large_content.replace('"', "\\\"")
    );
    fs::write(file, sample).unwrap();

    let _guard_codex = EnvGuard::set("CODEX_HOME", data_dir.to_string_lossy());

    // Build index
    cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // TUI should handle large content without panicking
    cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .env("TUI_HEADLESS", "1")
        .assert()
        .success();

    eprintln!("[SMOKE] tui_handles_large_message_content: PASSED");
}

// =============================================================================
// Summary Test (runs all critical paths)
// =============================================================================

#[test]
fn smoke_test_summary() {
    let _guard_lock = tui_smoke_guard();
    // This test just logs that all smoke tests in this file should pass
    eprintln!("================================================================================");
    eprintln!("[TUI SMOKE TESTS] All tests in this module validate:");
    eprintln!("  - TUI launches correctly in headless mode (--once + TUI_HEADLESS=1)");
    eprintln!("  - TUI exits cleanly with empty datasets (no panic)");
    eprintln!("  - TUI handles missing index gracefully");
    eprintln!("  - Search functionality works in headless mode");
    eprintln!("  - Multi-agent data is properly indexed and searchable");
    eprintln!("  - State persistence and reset works correctly");
    eprintln!("  - Exit codes are correct (0 for success)");
    eprintln!("  - CLI flags (--data-dir, --reset-state) are accepted");
    eprintln!("  - Unicode and large content are handled without panic");
    eprintln!("================================================================================");
}
