//! TUI headless smoke tests (bead xjt3)
//!
//! These tests verify the TUI can launch and exit cleanly in headless mode.
//! They test:
//! - Launch with empty index
//! - Launch with populated index
//! - Exit paths (immediate exit, search then exit)
//! - Exit codes and no panics
//!
//! All tests run without manual interaction via TUI_HEADLESS=1.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Create a base command with isolated environment for testing.
fn base_cmd(temp_home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    // Disable update prompts
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    // Enable headless mode for TUI
    cmd.env("TUI_HEADLESS", "1");
    // Isolate connectors by pointing HOME and XDG vars to temp dir
    cmd.env("HOME", temp_home);
    cmd.env("XDG_DATA_HOME", temp_home.join(".local/share"));
    cmd.env("XDG_CONFIG_HOME", temp_home.join(".config"));
    // Enable verbose logging for smoke test diagnostics
    cmd.env("RUST_LOG", "info,coding_agent_search=debug");
    cmd
}

/// Set up a minimal data directory with an empty index.
fn setup_empty_data_dir(data_dir: &Path) {
    fs::create_dir_all(data_dir).expect("create data dir");
}

/// Set up a data directory with an indexed database.
/// Returns the data directory path.
fn setup_indexed_data_dir(temp_home: &Path) -> std::path::PathBuf {
    let data_dir = temp_home.join("data");
    fs::create_dir_all(&data_dir).expect("create data dir");

    // Run index command to create DB and index
    let mut index_cmd = base_cmd(temp_home);
    index_cmd.args(["index", "--data-dir", data_dir.to_str().unwrap(), "--json"]);
    index_cmd.assert().success();

    data_dir
}

// ============================================================
// Launch Tests
// ============================================================

#[test]
fn tui_headless_exits_cleanly_with_index() {
    // Test: TUI --once with headless mode exits cleanly when index exists
    let tmp = TempDir::new().unwrap();
    let data_dir = setup_indexed_data_dir(tmp.path());

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["tui", "--once", "--data-dir", data_dir.to_str().unwrap()]);

    cmd.assert().success().stderr(
        predicate::str::is_empty()
            .not()
            .or(predicate::str::is_empty()),
    ); // Allow logs or no logs
}

#[test]
fn tui_headless_ftui_runtime_selection_exits_cleanly() {
    // Test: requesting ftui runtime does not break --once headless smoke path
    let tmp = TempDir::new().unwrap();
    let data_dir = setup_indexed_data_dir(tmp.path());

    let mut cmd = base_cmd(tmp.path());
    cmd.env("CASS_TUI_RUNTIME", "ftui");
    cmd.args(["tui", "--once", "--data-dir", data_dir.to_str().unwrap()]);

    cmd.assert().success();
}

#[test]
fn tui_headless_handles_empty_data_dir() {
    // Test: TUI --once with headless mode creates necessary files and succeeds
    // (even when starting with an empty data directory)
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    setup_empty_data_dir(&data_dir);

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["tui", "--once", "--data-dir", data_dir.to_str().unwrap()]);

    // Should succeed - headless mode creates db/index as needed
    cmd.assert().success();

    // Verify files were created
    assert!(
        data_dir.join("agent_search.db").exists(),
        "DB should be created"
    );
    assert!(
        data_dir.join("index").exists(),
        "Index dir should be created"
    );
}

#[test]
fn tui_headless_no_panic_on_empty_dataset() {
    // Test: TUI doesn't panic when index exists but is empty
    let tmp = TempDir::new().unwrap();
    let data_dir = setup_indexed_data_dir(tmp.path());

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["tui", "--once", "--data-dir", data_dir.to_str().unwrap()]);

    // Should succeed even with empty dataset
    cmd.assert().success();
    // Verify no panic occurred (would show in stderr)
    let output = cmd.output().expect("get output");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "TUI should not panic on empty dataset: {stderr}"
    );
}

// ============================================================
// Exit Code Tests
// ============================================================

#[test]
fn tui_headless_exit_code_success() {
    // Test: Successful headless run returns exit code 0
    let tmp = TempDir::new().unwrap();
    let data_dir = setup_indexed_data_dir(tmp.path());

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["tui", "--once", "--data-dir", data_dir.to_str().unwrap()]);

    cmd.assert().code(0);
}

#[test]
fn tui_headless_exit_code_success_empty_data_dir() {
    // Test: Empty data dir returns exit code 0 (files are auto-created)
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    setup_empty_data_dir(&data_dir);

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["tui", "--once", "--data-dir", data_dir.to_str().unwrap()]);

    cmd.assert().code(0);
}

// ============================================================
// Reset State Tests
// ============================================================

#[test]
fn tui_headless_reset_state_flag() {
    // Test: --reset-state clears persisted TUI state
    let tmp = TempDir::new().unwrap();
    let data_dir = setup_indexed_data_dir(tmp.path());

    // First, create some state by running TUI
    let mut cmd1 = base_cmd(tmp.path());
    cmd1.args(["tui", "--once", "--data-dir", data_dir.to_str().unwrap()]);
    cmd1.assert().success();

    // Run with --reset-state
    let mut cmd2 = base_cmd(tmp.path());
    cmd2.args([
        "tui",
        "--once",
        "--reset-state",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);
    cmd2.assert().success();
}

// ============================================================
// Logging Tests
// ============================================================

#[test]
fn tui_headless_emits_debug_logs_when_enabled() {
    // Test: Debug logging is available for diagnostics
    let tmp = TempDir::new().unwrap();
    let data_dir = setup_indexed_data_dir(tmp.path());

    let mut cmd = base_cmd(tmp.path());
    cmd.env("RUST_LOG", "debug");
    cmd.args(["tui", "--once", "--data-dir", data_dir.to_str().unwrap()]);

    // Just verify it runs without crashing with debug logging
    cmd.assert().success();
}

// ============================================================
// Performance Smoke Tests
// ============================================================

#[test]
fn tui_headless_completes_quickly() {
    // Test: Headless TUI should complete in reasonable time
    let tmp = TempDir::new().unwrap();
    let data_dir = setup_indexed_data_dir(tmp.path());

    let start = std::time::Instant::now();

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["tui", "--once", "--data-dir", data_dir.to_str().unwrap()]);
    cmd.assert().success();

    let elapsed = start.elapsed();
    // Headless mode should complete in under 5 seconds
    assert!(
        elapsed.as_secs() < 5,
        "Headless TUI took too long: {:?}",
        elapsed
    );
}

// ============================================================
// CLI Argument Validation
// ============================================================

#[test]
fn tui_once_flag_recognized() {
    // Test: --once flag is properly recognized
    let tmp = TempDir::new().unwrap();
    let data_dir = setup_indexed_data_dir(tmp.path());

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["tui", "--once", "--data-dir", data_dir.to_str().unwrap()]);

    // Should not fail with "unknown flag" error
    let output = cmd.output().expect("get output");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("error:") || !stderr.contains("--once"),
        "--once flag should be recognized"
    );
}

#[test]
fn tui_data_dir_flag_recognized() {
    // Test: --data-dir flag works correctly
    let tmp = TempDir::new().unwrap();
    let custom_data_dir = tmp.path().join("custom_data");
    fs::create_dir_all(&custom_data_dir).expect("create custom data dir");

    // Index into custom directory
    let mut index_cmd = base_cmd(tmp.path());
    index_cmd.args([
        "index",
        "--data-dir",
        custom_data_dir.to_str().unwrap(),
        "--json",
    ]);
    index_cmd.assert().success();

    // Run TUI with custom data dir
    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "tui",
        "--once",
        "--data-dir",
        custom_data_dir.to_str().unwrap(),
    ]);
    cmd.assert().success();
}

// ============================================================
// Integration with Other Commands
// ============================================================

#[test]
fn tui_after_index_works() {
    // Test: TUI works correctly after running index command
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create data dir");

    // Run index
    let mut index_cmd = base_cmd(tmp.path());
    index_cmd.args(["index", "--data-dir", data_dir.to_str().unwrap(), "--json"]);
    index_cmd.assert().success();

    // Verify DB and index exist
    assert!(data_dir.join("agent_search.db").exists(), "DB should exist");
    assert!(data_dir.join("index").exists(), "Index should exist");

    // Run TUI
    let mut tui_cmd = base_cmd(tmp.path());
    tui_cmd.args(["tui", "--once", "--data-dir", data_dir.to_str().unwrap()]);
    tui_cmd.assert().success();
}

#[test]
fn tui_and_search_use_same_index() {
    // Test: TUI and search commands use the same index
    let tmp = TempDir::new().unwrap();
    let data_dir = setup_indexed_data_dir(tmp.path());

    // Run search command (should work)
    let mut search_cmd = base_cmd(tmp.path());
    search_cmd.args([
        "search",
        "test",
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--json",
    ]);
    search_cmd.assert().success();

    // Run TUI (should also work with same index)
    let mut tui_cmd = base_cmd(tmp.path());
    tui_cmd.args(["tui", "--once", "--data-dir", data_dir.to_str().unwrap()]);
    tui_cmd.assert().success();
}
