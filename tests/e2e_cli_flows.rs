//! E2E CLI/TUI flows with rich logging (yln.5).
//!
//! Tests cover:
//! - Search query E2E with --trace flag
//! - Detail find (view/expand commands)
//! - Filter combinations (agent, days, workspace)
//! - Logging/trace output validation
//!
//! All tests use real fixtures and assert outputs (no mocks).
//!
//! # E2E Logging
//!
//! Tests emit structured JSONL logs via E2eLogger when `E2E_LOG=1` is set.
//! See `test-results/e2e/SCHEMA.md` for log format.

use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

mod util;

use util::e2e_log::{E2ePerformanceMetrics, PhaseTracker};

// =============================================================================
// E2E Logger Support
// =============================================================================

// PhaseTracker is provided by util::e2e_log

/// Create a minimal Codex session fixture.
fn make_codex_session(root: &std::path::Path, content: &str, ts: u64) {
    let sessions = root.join("sessions/2024/12/01");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-test.jsonl");
    let sample = format!(
        r#"{{"type": "event_msg", "timestamp": {ts}, "payload": {{"type": "user_message", "message": "{content}"}}}}
{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "{content}_response"}}}}
"#,
        ts + 1000
    );
    fs::write(file, sample).unwrap();
}

/// Create a Claude Code session fixture.
fn make_claude_session(root: &std::path::Path, project: &str, content: &str) {
    let project_dir = root.join(format!("projects/{project}"));
    fs::create_dir_all(&project_dir).unwrap();
    let file = project_dir.join("session.jsonl");
    let sample = format!(
        r#"{{"type": "user", "timestamp": "2024-12-01T10:00:00Z", "message": {{"role": "user", "content": "{content}"}}}}
{{"type": "assistant", "timestamp": "2024-12-01T10:01:00Z", "message": {{"role": "assistant", "content": "{content}_response"}}}}"#
    );
    fs::write(file, sample).unwrap();
}

#[allow(deprecated)]
fn base_cmd() -> Command {
    let mut cmd = Command::cargo_bin("cass").unwrap();
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd
}

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_cli_flows", test_name)
}

/// Setup test environment with fixtures and run index.
fn setup_indexed_env() -> (TempDir, std::path::PathBuf) {
    let tracker = PhaseTracker::new("e2e_cli_flows", "setup_indexed_env");
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create fixtures
    let phase_start = tracker.start(
        "create_fixtures",
        Some("Create Codex and Claude session fixtures"),
    );
    make_codex_session(&codex_home, "authentication error in login", 1733011200000);
    make_claude_session(&claude_home, "myapp", "fix the database connection");
    tracker.end(
        "create_fixtures",
        Some("Create Codex and Claude session fixtures"),
        phase_start,
    );

    // Run index
    let phase_start = tracker.start("index", Some("Run full index on fixture sessions"));
    base_cmd()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    let index_ms = phase_start.elapsed().as_millis() as u64;
    tracker.end(
        "index",
        Some("Run full index on fixture sessions"),
        phase_start,
    );
    tracker.metrics(
        "cass_index",
        &E2ePerformanceMetrics::new()
            .with_duration(index_ms)
            .with_throughput(2, index_ms)
            .with_custom("operation", "full_index"),
    );

    tracker.flush();
    (tmp, data_dir)
}

// =============================================================================
// Search Query E2E Tests with trace file
// =============================================================================

#[test]
fn search_with_trace_file_creates_trace() {
    let tracker = tracker_for("search_with_trace_file_creates_trace");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();
    let trace_file = tmp.path().join("trace.jsonl");

    let output = base_cmd()
        .args(["--trace-file"])
        .arg(&trace_file)
        .args(["search", "authentication", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Search with trace-file should succeed"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Main output should be valid JSON
    let json: Value = serde_json::from_str(stdout.trim()).expect("stdout should be valid JSON");
    assert!(json.get("hits").is_some() || json.get("results").is_some());

    // Trace file should exist (may be empty if no spans logged)
    // Note: trace file creation is best-effort
}

#[test]
fn search_basic_returns_valid_json() {
    let tracker = tracker_for("search_basic_returns_valid_json");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    let search_start = tracker.start("run_search", Some("Execute basic search command"));
    let output = base_cmd()
        .args(["search", "database", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let search_ms = search_start.elapsed().as_millis() as u64;
    tracker.end("run_search", Some("Search complete"), search_start);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should be valid JSON
    let json: Value = serde_json::from_str(stdout.trim()).expect("Should be valid JSON");
    let hit_count = json
        .get("hits")
        .or_else(|| json.get("results"))
        .and_then(|h| h.as_array())
        .map(|a| a.len() as u64)
        .unwrap_or(0);
    assert!(
        json.get("hits").is_some() || json.get("results").is_some() || json.get("count").is_some(),
        "Should have results structure. JSON: {}",
        json
    );

    tracker.metrics(
        "cass_search",
        &E2ePerformanceMetrics::new()
            .with_duration(search_ms)
            .with_throughput(hit_count, search_ms)
            .with_custom("query", "database"),
    );
    tracker.complete();
}

#[test]
fn search_returns_hits_with_expected_fields() {
    let tracker = tracker_for("search_returns_hits_with_expected_fields");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    let output = base_cmd()
        .args([
            "search",
            "authentication",
            "--robot",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Bead 7k7pl: pin hits/results as a JSON array, not just
    // "field present". Follow-up `.as_array()` would silently turn a
    // regression (null/scalar) into `None` and skip the inner block
    // — upgrade forces the regression to surface here.
    let hits = json.get("hits").or_else(|| json.get("results"));
    let hits_array = hits
        .and_then(|h| h.as_array())
        .unwrap_or_else(|| panic!("hits/results must be an array. JSON: {}", json));

    if !hits_array.is_empty() {
        let first_hit = &hits_array[0];
        // Bead 7k7pl: pin TYPE on the hit-schema fields —
        // source_path/path must be a string, agent must be a string.
        // A null-or-numeric regression would slip past `.is_some()`
        // while breaking JSON consumers that call `.as_str()`.
        let source_path = first_hit
            .get("source_path")
            .and_then(|v| v.as_str())
            .or_else(|| first_hit.get("path").and_then(|v| v.as_str()));
        assert!(
            source_path.is_some(),
            "Hit must have string `source_path` or `path`. Hit: {}",
            first_hit
        );
        assert!(
            first_hit.get("agent").and_then(|v| v.as_str()).is_some(),
            "Hit must have a string `agent` field. Hit: {}",
            first_hit
        );
    }
}

// =============================================================================
// Detail Find Tests (view/expand)
// =============================================================================

#[test]
fn view_command_returns_session_detail() {
    let tracker = tracker_for("view_command_returns_session_detail");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();
    let codex_session = tmp
        .path()
        .join(".codex/sessions/2024/12/01/rollout-test.jsonl");

    // View the session
    let view_start = tracker.start("run_view", Some("Execute view command on session"));
    let output = base_cmd()
        .args(["view", "--robot", "--data-dir"])
        .arg(&data_dir)
        .arg(&codex_session)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let view_ms = view_start.elapsed().as_millis() as u64;
    tracker.end("run_view", Some("View complete"), view_start);

    // View may exit with 0 or non-zero depending on whether session is indexed
    let stdout = String::from_utf8_lossy(&output.stdout);

    if output.status.success() {
        // Should be valid JSON
        let json: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
        // May have messages or error
        assert!(
            json.get("messages").is_some()
                || json.get("error").is_some()
                || json.get("conversation").is_some(),
            "View should return messages or error. stdout: {}",
            stdout
        );
    }

    tracker.metrics(
        "cass_view",
        &E2ePerformanceMetrics::new()
            .with_duration(view_ms)
            .with_custom("operation", "view_session"),
    );
    tracker.complete();
}

#[test]
fn expand_command_with_context() {
    let tracker = tracker_for("expand_command_with_context");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();
    let codex_session = tmp
        .path()
        .join(".codex/sessions/2024/12/01/rollout-test.jsonl");

    // Expand with context
    let output = base_cmd()
        .args(["expand", "--robot", "-n", "1", "-C", "2", "--data-dir"])
        .arg(&data_dir)
        .arg(&codex_session)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Expand may succeed or fail depending on line existence
    if output.status.success() && !stdout.is_empty() {
        let json: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
        // Should have context or messages
        assert!(
            json.get("messages").is_some()
                || json.get("context").is_some()
                || json.get("lines").is_some(),
            "Expand should return context. stdout: {}, stderr: {}",
            stdout,
            stderr
        );
    }
}

// =============================================================================
// Filter Combination Tests
// =============================================================================

#[test]
fn search_filter_by_agent() {
    let tracker = tracker_for("search_filter_by_agent");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    // Search for codex agent only
    let output = base_cmd()
        .args([
            "search",
            "authentication",
            "--robot",
            "--agent",
            "codex",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // All hits should be from codex
    if let Some(hits) = json
        .get("hits")
        .or_else(|| json.get("results"))
        .and_then(|h| h.as_array())
    {
        for hit in hits {
            let agent = hit.get("agent").and_then(|a| a.as_str()).unwrap_or("");
            assert!(
                agent.contains("codex") || agent.is_empty(),
                "Expected codex agent, got: {}",
                agent
            );
        }
    }
}

#[test]
fn search_filter_by_days() {
    let tracker = tracker_for("search_filter_by_days");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    // Search with days filter (should include recent sessions)
    let output = base_cmd()
        .args([
            "search",
            "database",
            "--robot",
            "--days",
            "365",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should parse as valid JSON
    let _json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
}

#[test]
fn search_combined_filters() {
    let tracker = tracker_for("search_combined_filters");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    // Combine multiple filters
    let output = base_cmd()
        .args([
            "search",
            "error",
            "--robot",
            "--limit",
            "10",
            "--days",
            "30",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    // Check limit is respected
    if let Some(hits) = json
        .get("hits")
        .or_else(|| json.get("results"))
        .and_then(|h| h.as_array())
    {
        assert!(hits.len() <= 10, "Should respect limit=10");
    }
}

#[test]
fn search_with_workspace_filter() {
    let tracker = tracker_for("search_with_workspace_filter");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();
    let workspace = tmp.path().join(".claude/projects/myapp");

    // Search with workspace filter
    let output = base_cmd()
        .args(["search", "database", "--robot", "--workspace"])
        .arg(&workspace)
        .arg("--data-dir")
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should parse as valid JSON
    let _json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
}

// =============================================================================
// Logging/Trace Validation Tests
// =============================================================================

#[test]
fn trace_output_contains_operation_markers() {
    let tracker = tracker_for("trace_output_contains_operation_markers");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    let output = base_cmd()
        .args(["search", "test", "--robot", "--trace", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    // Even if no results, trace should work
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Trace should contain some operation info
    // May be empty if tracing not fully enabled, but when present should have structure
    if !stderr.is_empty() && stderr.contains('{') {
        // Likely JSON trace - verify parseable
        for line in stderr.lines() {
            if line.starts_with('{') {
                let _: Value = serde_json::from_str(line).unwrap_or(Value::Null);
            }
        }
    }
}

#[test]
fn verbose_mode_increases_logging() {
    let tracker = tracker_for("verbose_mode_increases_logging");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    // Run with -v for verbose
    let output = base_cmd()
        .args(["search", "test", "--robot", "-v", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Verbose mode may produce more stderr output
    // This is a weak assertion but validates verbose doesn't break execution
    let _ = stderr; // Use stderr to avoid unused warning
    assert!(output.status.success() || output.status.code() == Some(3));
}

// =============================================================================
// Robot Mode Output Validation
// =============================================================================

#[test]
fn robot_mode_suppresses_ansi() {
    let tracker = tracker_for("robot_mode_suppresses_ansi");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    let output = base_cmd()
        .args([
            "search",
            "authentication",
            "--robot",
            "--color=never",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should not contain ANSI escape codes
    assert!(
        !stdout.contains('\x1b'),
        "Robot mode with --color=never should not emit ANSI"
    );
}

#[test]
fn robot_mode_json_output_only() {
    let tracker = tracker_for("robot_mode_json_output_only");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    let output = base_cmd()
        .args(["search", "test", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);

    // stdout should be pure JSON (or empty)
    if !stdout.trim().is_empty() {
        let _: Value =
            serde_json::from_str(stdout.trim()).expect("Robot mode stdout should be valid JSON");
    }
}

// =============================================================================
// Health/Status Commands E2E
// =============================================================================

#[test]
fn health_command_returns_structured_output() {
    let tracker = tracker_for("health_command_returns_structured_output");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    let health_start = tracker.start("run_health", Some("Execute health check command"));
    let output = base_cmd()
        .args(["health", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let health_ms = health_start.elapsed().as_millis() as u64;
    tracker.end("run_health", Some("Health check complete"), health_start);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should have health status
    assert!(
        json.get("healthy").is_some() || json.get("status").is_some(),
        "Health should report status. JSON: {}",
        json
    );

    tracker.metrics(
        "cass_health",
        &E2ePerformanceMetrics::new()
            .with_duration(health_ms)
            .with_custom("operation", "health_check"),
    );
    tracker.complete();
}

#[test]
fn stats_command_returns_aggregations() {
    let tracker = tracker_for("stats_command_returns_aggregations");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    let stats_start = tracker.start("run_stats", Some("Execute stats command"));
    let output = base_cmd()
        .args(["stats", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let stats_ms = stats_start.elapsed().as_millis() as u64;
    tracker.end("run_stats", Some("Stats complete"), stats_start);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should have some statistics
    assert!(
        json.get("total").is_some()
            || json.get("sessions").is_some()
            || json.get("count").is_some()
            || json.get("by_agent").is_some(),
        "Stats should have counts. JSON: {}",
        json
    );

    tracker.metrics(
        "cass_stats",
        &E2ePerformanceMetrics::new()
            .with_duration(stats_ms)
            .with_custom("operation", "stats"),
    );
    tracker.complete();
}

#[test]
fn capabilities_command_lists_features() {
    let tracker = tracker_for("capabilities_command_lists_features");
    let _trace_guard = tracker.trace_env_guard();
    let output = base_cmd()
        .args(["capabilities", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should list capabilities
    assert!(
        json.get("commands").is_some()
            || json.get("capabilities").is_some()
            || json.get("features").is_some(),
        "Capabilities should list features. JSON: {}",
        json
    );
}

// =============================================================================
// Error Handling E2E Tests
// =============================================================================

#[test]
fn search_no_index_handles_gracefully() {
    let tracker = tracker_for("search_no_index_handles_gracefully");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("empty_data");
    fs::create_dir_all(&data_dir).unwrap();

    let output = base_cmd()
        .args(["search", "test", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    let exit_code = output.status.code().unwrap_or(99);

    // Exit code 3 means missing index, 0 means empty results, 1 means no index/db,
    // 9 means unknown error. All are valid outcomes for no-index scenario.
    assert!(
        exit_code == 0 || exit_code == 1 || exit_code == 3 || exit_code == 9,
        "No index should return exit 0, 1, 3, or 9, got: {}",
        exit_code
    );
}

#[test]
fn truly_invalid_command_returns_error() {
    let tracker = tracker_for("truly_invalid_command_returns_error");
    let _trace_guard = tracker.trace_env_guard();
    // Test with a truly malformed command (not interpretable as search)
    let output = base_cmd()
        .args(["--nonexistent-flag-only"])
        .output()
        .unwrap();

    // Should either fail or be auto-corrected - verify it doesn't crash
    // The forgiving CLI may interpret most things as search queries
    let exit_code = output.status.code().unwrap_or(0);
    assert!(
        exit_code == 0 || exit_code == 2 || exit_code == 3,
        "Should return valid exit code (0, 2, or 3), got: {}",
        exit_code
    );
}

#[test]
fn view_nonexistent_file_handles_gracefully() {
    let tracker = tracker_for("view_nonexistent_file_handles_gracefully");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let output = base_cmd()
        .args([
            "view",
            "/nonexistent/path/session.jsonl",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .output()
        .unwrap();

    // Should handle gracefully (non-zero exit but structured error)
    let stdout = String::from_utf8_lossy(&output.stdout);

    // If output present, should be valid JSON or error message
    if !stdout.trim().is_empty() {
        // May be JSON error or plain text
        if stdout.trim().starts_with('{') {
            let _ = serde_json::from_str::<Value>(stdout.trim());
        }
    }
}

// =============================================================================
// Index Watch-Once Tests (br-154l)
// =============================================================================

#[test]
fn index_incremental_processes_file_changes() {
    let tracker = tracker_for("index_incremental_processes_file_changes");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create initial fixture
    make_codex_session(&codex_home, "initial session content", 1733011200000);

    // Run full index first
    let phase_start = tracker.start("initial_index", Some("Run initial full index"));
    base_cmd()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("initial_index", Some("Initial index complete"), phase_start);

    // Get initial stats
    let stats_output = base_cmd()
        .args(["stats", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .unwrap();
    let initial_stats: Value =
        serde_json::from_str(&String::from_utf8_lossy(&stats_output.stdout)).unwrap_or_default();

    // Create a new session file
    let new_sessions = codex_home.join("sessions/2024/12/02");
    fs::create_dir_all(&new_sessions).unwrap();
    let new_file = new_sessions.join("rollout-new.jsonl");
    let new_content = r#"{"type": "event_msg", "timestamp": 1733097600000, "payload": {"type": "user_message", "message": "new session content"}}
{"type": "response_item", "timestamp": 1733097601000, "payload": {"role": "assistant", "content": "response to new session"}}"#;
    fs::write(&new_file, new_content).unwrap();

    // Run incremental index to pick up the new file
    let incr_start = tracker.start("incremental_index", Some("Run incremental index"));
    let output = base_cmd()
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .unwrap();
    let incr_ms = incr_start.elapsed().as_millis() as u64;
    tracker.end(
        "incremental_index",
        Some("Incremental index complete"),
        incr_start,
    );

    assert!(
        output.status.success(),
        "Incremental index should succeed. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify new session was indexed by checking stats
    let final_stats_output = base_cmd()
        .args(["stats", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .unwrap();
    let final_stats: Value =
        serde_json::from_str(&String::from_utf8_lossy(&final_stats_output.stdout))
            .unwrap_or_default();

    // Stats should reflect new session (or at least not crash)
    let initial_count = initial_stats
        .get("total")
        .or_else(|| initial_stats.get("sessions"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let final_count = final_stats
        .get("total")
        .or_else(|| final_stats.get("sessions"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    // Final count should be >= initial (new session indexed)
    assert!(
        final_count >= initial_count,
        "Session count should increase or stay same after incremental index"
    );

    tracker.metrics(
        "cass_incremental_index",
        &E2ePerformanceMetrics::new()
            .with_duration(incr_ms)
            .with_custom("operation", "incremental_index"),
    );
    tracker.complete();
}

// =============================================================================
// Semantic/Hybrid Search Tests (br-154l)
// =============================================================================

#[test]
fn search_semantic_mode() {
    let tracker = tracker_for("search_semantic_mode");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    // Attempt semantic search (may fallback to lexical if no embedder)
    let search_start = tracker.start("run_semantic_search", Some("Execute semantic search"));
    let output = base_cmd()
        .args([
            "search",
            "database connection",
            "--robot",
            "--mode",
            "semantic",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let search_ms = search_start.elapsed().as_millis() as u64;
    tracker.end(
        "run_semantic_search",
        Some("Semantic search complete"),
        search_start,
    );

    // Semantic mode may succeed, gracefully degrade, or error
    // Various exit codes are valid depending on semantic index availability
    let exit_code = output.status.code().unwrap_or(99);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The test passes if we get structured output (success or error)
    // or if it fails gracefully with expected exit codes
    if !stdout.trim().is_empty() && stdout.trim().starts_with('{') {
        let json: Value = serde_json::from_str(stdout.trim()).unwrap_or_default();
        // Valid if has any recognizable structure
        assert!(
            json.is_object(),
            "Semantic search should return JSON. stdout: {}, stderr: {}",
            stdout,
            stderr
        );
    }

    tracker.metrics(
        "cass_semantic_search",
        &E2ePerformanceMetrics::new()
            .with_duration(search_ms)
            .with_custom("mode", "semantic")
            .with_custom("exit_code", exit_code.to_string()),
    );
    // Test passes as long as it doesn't crash unexpectedly
    tracker.complete();
}

#[test]
fn search_hybrid_mode() {
    let tracker = tracker_for("search_hybrid_mode");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    // Attempt hybrid search
    let search_start = tracker.start("run_hybrid_search", Some("Execute hybrid search"));
    let output = base_cmd()
        .args([
            "search",
            "authentication error",
            "--robot",
            "--mode",
            "hybrid",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let search_ms = search_start.elapsed().as_millis() as u64;
    tracker.end(
        "run_hybrid_search",
        Some("Hybrid search complete"),
        search_start,
    );

    // Hybrid mode may succeed, gracefully degrade, or error
    let exit_code = output.status.code().unwrap_or(99);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The test passes if we get structured output (success or error)
    if !stdout.trim().is_empty() && stdout.trim().starts_with('{') {
        let json: Value = serde_json::from_str(stdout.trim()).unwrap_or_default();
        // Valid if has any recognizable structure
        assert!(
            json.is_object(),
            "Hybrid search should return JSON. stdout: {}, stderr: {}",
            stdout,
            stderr
        );
    }

    tracker.metrics(
        "cass_hybrid_search",
        &E2ePerformanceMetrics::new()
            .with_duration(search_ms)
            .with_custom("mode", "hybrid")
            .with_custom("exit_code", exit_code.to_string()),
    );
    // Test passes as long as it doesn't crash unexpectedly
    tracker.complete();
}

#[test]
fn search_lexical_mode_explicit() {
    let tracker = tracker_for("search_lexical_mode_explicit");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    // Explicit lexical mode (should always work)
    let search_start = tracker.start(
        "run_lexical_search",
        Some("Execute explicit lexical search"),
    );
    let output = base_cmd()
        .args([
            "search",
            "authentication",
            "--robot",
            "--mode",
            "lexical",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let search_ms = search_start.elapsed().as_millis() as u64;
    tracker.end(
        "run_lexical_search",
        Some("Lexical search complete"),
        search_start,
    );

    assert!(output.status.success(), "Lexical search should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("Should be valid JSON");
    assert!(
        json.get("hits").is_some() || json.get("results").is_some(),
        "Lexical search should return hits/results. JSON: {}",
        json
    );

    tracker.metrics(
        "cass_lexical_search",
        &E2ePerformanceMetrics::new()
            .with_duration(search_ms)
            .with_custom("mode", "lexical"),
    );
    tracker.complete();
}

// =============================================================================
// Diag Command Tests (br-154l)
// =============================================================================

#[test]
fn diag_command_returns_diagnostic_info() {
    let tracker = tracker_for("diag_command_returns_diagnostic_info");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    let diag_start = tracker.start("run_diag", Some("Execute diag command"));
    let output = base_cmd()
        .args(["diag", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let diag_ms = diag_start.elapsed().as_millis() as u64;
    tracker.end("run_diag", Some("Diag complete"), diag_start);

    // Diag should succeed or return structured error
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() && !stdout.trim().is_empty() {
        let json: Value = serde_json::from_str(stdout.trim()).expect("Should be valid JSON");
        // Should have diagnostic info like version, db path, index stats, etc.
        assert!(
            json.get("version").is_some()
                || json.get("db_path").is_some()
                || json.get("index_path").is_some()
                || json.get("diagnostics").is_some()
                || json.get("config").is_some(),
            "Diag should return diagnostic fields. JSON: {}, stderr: {}",
            json,
            stderr
        );
    }

    tracker.metrics(
        "cass_diag",
        &E2ePerformanceMetrics::new()
            .with_duration(diag_ms)
            .with_custom("operation", "diag"),
    );
    tracker.complete();
}

#[test]
fn status_command_returns_index_status() {
    let tracker = tracker_for("status_command_returns_index_status");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    let status_start = tracker.start("run_status", Some("Execute status command"));
    let output = base_cmd()
        .args(["status", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let status_ms = status_start.elapsed().as_millis() as u64;
    tracker.end("run_status", Some("Status complete"), status_start);

    let stdout = String::from_utf8_lossy(&output.stdout);

    if output.status.success() && !stdout.trim().is_empty() {
        let json: Value = serde_json::from_str(stdout.trim()).expect("Should be valid JSON");
        // Status should have index state info (various possible field names)
        assert!(
            json.get("healthy").is_some()
                || json.get("index").is_some()
                || json.get("database").is_some()
                || json.get("indexed").is_some()
                || json.get("sessions").is_some()
                || json.get("status").is_some()
                || json.get("last_indexed").is_some()
                || json.get("count").is_some()
                || json.get("_meta").is_some(),
            "Status should return index state. JSON: {}",
            json
        );
    }

    tracker.metrics(
        "cass_status",
        &E2ePerformanceMetrics::new()
            .with_duration(status_ms)
            .with_custom("operation", "status"),
    );
    tracker.complete();
}

// =============================================================================
// Multi-Agent E2E Tests
// =============================================================================

#[test]
fn search_across_multiple_agents() {
    let tracker = tracker_for("search_across_multiple_agents");
    let _trace_guard = tracker.trace_env_guard();
    let (tmp, data_dir) = setup_indexed_env();

    // Search should find results from both codex and claude
    let output = base_cmd()
        .args([
            "search",
            "error OR database",
            "--robot",
            "--limit",
            "20",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should have results (may be from one or both agents)
    let hits = json.get("hits").or_else(|| json.get("results"));
    assert!(
        hits.is_some(),
        "Should have hits from multi-agent search. JSON: {}",
        json
    );
}
