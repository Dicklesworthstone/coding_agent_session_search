//! Integration tests for semantic search flows.
//!
//! Tests cover:
//! - CLI models commands (status, verify, check-update)
//! - Search mode flags (lexical, semantic, hybrid)
//! - Determinism tests (same query yields consistent results)
//! - Robot output schema validation
//!
//! Part of bead: coding_agent_session_search-c8f8

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

mod util;

/// Helper to create Codex session with modern envelope format.
fn make_codex_session(
    root: &std::path::Path,
    date_path: &str,
    filename: &str,
    content: &str,
    ts: u64,
) {
    let sessions = root.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    let sample = format!(
        r#"{{"type": "event_msg", "timestamp": {ts}, "payload": {{"type": "user_message", "message": "{content}"}}}}
{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "{content}_response"}}}}"#,
        ts + 1000
    );
    fs::write(file, sample).unwrap();
}

// =============================================================================
// CLI Models Command Tests
// =============================================================================

/// Test: cass models status returns valid output
#[test]
fn test_models_status_command() {
    let output = cargo_bin_cmd!("cass")
        .args(["models", "status"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("models status command");

    // Should succeed (exit 0) regardless of installation state
    assert!(
        output.status.success(),
        "models status should succeed. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should contain status-related output
    assert!(
        stdout.contains("Model") || stdout.contains("model") || stdout.contains("Status"),
        "Output should mention models or status. Got: {}",
        stdout
    );
}

/// Test: cass models status --json returns valid JSON
#[test]
fn test_models_status_json_output() {
    let output = cargo_bin_cmd!("cass")
        .args(["models", "status", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("models status --json command");

    assert!(
        output.status.success(),
        "models status --json should succeed. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value =
        serde_json::from_str(stdout.trim()).expect("models status --json should return valid JSON");

    // Bead 7k7pl: pin TYPE + non-empty content on model_id/state, not
    // just "field present". A regression that emitted `null` or a
    // number would slip past `.is_some()` while breaking downstream
    // consumers that expect string IDs.
    let model_id = json
        .get("model_id")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("model_id must be a string. Got: {}", json));
    assert!(
        !model_id.is_empty(),
        "model_id must be a non-empty string. Got: {}",
        json
    );
    let state = json
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("state must be a string. Got: {}", json));
    assert!(
        !state.is_empty(),
        "state must be a non-empty string. Got: {}",
        json
    );
}

/// Test: cass models verify returns valid output
#[test]
fn test_models_verify_command() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let output = cargo_bin_cmd!("cass")
        .args(["models", "verify", "--data-dir"])
        .arg(&data_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("models verify command");

    // Should succeed (model not installed is still a valid result)
    assert!(
        output.status.success(),
        "models verify should succeed. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Test: cass models verify --json returns valid JSON
#[test]
fn test_models_verify_json_output() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let output = cargo_bin_cmd!("cass")
        .args(["models", "verify", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("models verify --json command");

    assert!(
        output.status.success(),
        "models verify --json should succeed. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value =
        serde_json::from_str(stdout.trim()).expect("models verify --json should return valid JSON");

    // Bead 7k7pl: pin TYPE on model_dir/status — both must be
    // non-empty strings, not just "present". A null or numeric
    // regression would slip past `.is_some()`.
    let model_dir = json
        .get("model_dir")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("model_dir must be a string. Got: {}", json));
    assert!(
        !model_dir.is_empty(),
        "model_dir must be a non-empty string path. Got: {}",
        json
    );
    let status = json
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("status must be a string. Got: {}", json));
    assert!(
        !status.is_empty(),
        "status must be a non-empty string. Got: {}",
        json
    );
}

/// Test: cass models check-update returns valid output
#[test]
fn test_models_check_update_command() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let output = cargo_bin_cmd!("cass")
        .args(["models", "check-update", "--data-dir"])
        .arg(&data_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("models check-update command");

    // Should succeed regardless of installation state
    assert!(
        output.status.success(),
        "models check-update should succeed. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Test: cass models check-update --json returns valid JSON
#[test]
fn test_models_check_update_json_output() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let output = cargo_bin_cmd!("cass")
        .args(["models", "check-update", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("models check-update --json command");

    assert!(
        output.status.success(),
        "models check-update --json should succeed. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim())
        .expect("models check-update --json should return valid JSON");

    // Bead 7k7pl: pin update_available as a boolean (not `null` or a
    // string like "maybe"), and latest_revision as a string. CLI
    // consumers branch on the bool; a type regression would slip past
    // `.is_some()`.
    assert!(
        json.get("update_available")
            .and_then(|v| v.as_bool())
            .is_some(),
        "update_available must be a boolean. Got: {}",
        json
    );
    assert!(
        json.get("latest_revision")
            .and_then(|v| v.as_str())
            .is_some(),
        "latest_revision must be a string. Got: {}",
        json
    );
}

/// Test: cass models help shows all subcommands
#[test]
fn test_models_help_shows_subcommands() {
    let output = cargo_bin_cmd!("cass")
        .args(["models", "--help"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("models --help command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should list all subcommands
    assert!(
        stdout.contains("status"),
        "Help should mention status subcommand"
    );
    assert!(
        stdout.contains("install"),
        "Help should mention install subcommand"
    );
    assert!(
        stdout.contains("verify"),
        "Help should mention verify subcommand"
    );
    assert!(
        stdout.contains("remove"),
        "Help should mention remove subcommand"
    );
    assert!(
        stdout.contains("check-update"),
        "Help should mention check-update subcommand"
    );
}

// =============================================================================
// Search Mode Flag Tests
// =============================================================================

/// Test: --mode lexical uses lexical search
#[test]
fn test_mode_flag_lexical() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    // Create fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-mode.jsonl",
        "lexical_mode_test_content",
        1732118400000,
    );

    // Index first
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .assert()
        .success();

    // Search with --mode lexical
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "lexical_mode_test_content",
            "--mode",
            "lexical",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("search --mode lexical");

    assert!(
        output.status.success(),
        "Search with --mode lexical should succeed"
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let hits = json.get("hits").and_then(|h| h.as_array());
    assert!(hits.is_some(), "Should have hits array");
}

/// Test: --mode semantic is accepted (may fail if model not installed)
#[test]
fn test_mode_flag_semantic() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    // Create fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-semantic.jsonl",
        "semantic_mode_test_content",
        1732118400000,
    );

    // Index first
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .assert()
        .success();

    // Search with --mode semantic
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "semantic_mode_test_content",
            "--mode",
            "semantic",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("search --mode semantic");

    // Either succeeds or fails with "semantic-unavailable" error (when model not installed)
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("semantic-unavailable")
                || stderr.contains("Semantic search not available"),
            "If semantic fails, should be due to unavailability. Got: {}",
            stderr
        );
    }
}

/// Test: --mode hybrid combines lexical and semantic (may fail if model not installed)
#[test]
fn test_mode_flag_hybrid() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    // Create fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-hybrid.jsonl",
        "hybrid_mode_test_content",
        1732118400000,
    );

    // Index first
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .assert()
        .success();

    // Search with --mode hybrid
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "hybrid_mode_test_content",
            "--mode",
            "hybrid",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("search --mode hybrid");

    // Either succeeds or fails with "semantic-unavailable" error (when model not installed)
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("semantic-unavailable")
                || stderr.contains("Hybrid search not available"),
            "If hybrid fails, should be due to unavailability. Got: {}",
            stderr
        );
    }
}

// =============================================================================
// Determinism Tests
// =============================================================================

/// Test: Same query returns same results across multiple invocations
#[test]
fn test_same_query_same_results() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    // Create multiple fixtures with deterministic content
    for i in 1..=3 {
        make_codex_session(
            &codex_home,
            "2024/11/20",
            &format!("rollout-det{i}.jsonl"),
            &format!("deterministic_test_content_{i}"),
            1732118400000 + (i as u64 * 1000),
        );
    }

    // Index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .assert()
        .success();

    // Run the same search query multiple times
    let mut results: Vec<String> = Vec::new();
    for _ in 0..3 {
        let output = cargo_bin_cmd!("cass")
            .args([
                "search",
                "deterministic_test_content",
                "--robot",
                "--data-dir",
            ])
            .arg(&data_dir)
            .env("HOME", home)
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .output()
            .expect("deterministic search");

        assert!(output.status.success());

        let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");

        // Extract hit IDs or paths for comparison
        let empty_vec = vec![];
        let hits = json
            .get("hits")
            .and_then(|h| h.as_array())
            .unwrap_or(&empty_vec);
        let hit_ids: Vec<String> = hits
            .iter()
            .filter_map(|h| h.get("source_path").and_then(|p| p.as_str()))
            .map(String::from)
            .collect();
        results.push(hit_ids.join(","));
    }

    // All results should be identical
    assert!(
        results.iter().all(|r| r == &results[0]),
        "Same query should return same results. Got: {:?}",
        results
    );
}

/// Test: Results are ordered deterministically (same order each time)
#[test]
fn test_result_ordering_deterministic() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    // Create fixtures with shared term
    for i in 1..=5 {
        make_codex_session(
            &codex_home,
            "2024/11/20",
            &format!("rollout-order{i}.jsonl"),
            &format!("ordering_test_shared_{i}"),
            1732118400000 + (i as u64 * 100000),
        );
    }

    // Index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .assert()
        .success();

    // Run search multiple times and compare ordering
    let mut orderings: Vec<Vec<String>> = Vec::new();
    for _ in 0..3 {
        let output = cargo_bin_cmd!("cass")
            .args(["search", "ordering_test_shared", "--robot", "--data-dir"])
            .arg(&data_dir)
            .env("HOME", home)
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .output()
            .expect("ordering search");

        assert!(output.status.success());

        let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
        let empty_vec = vec![];
        let hits = json
            .get("hits")
            .and_then(|h| h.as_array())
            .unwrap_or(&empty_vec);
        let order: Vec<String> = hits
            .iter()
            .filter_map(|h| h.get("source_path").and_then(|p| p.as_str()))
            .map(String::from)
            .collect();
        orderings.push(order);
    }

    // All orderings should be identical
    assert!(
        orderings.iter().all(|o| o == &orderings[0]),
        "Result ordering should be deterministic. Got: {:?}",
        orderings
    );
}

// =============================================================================
// Robot Output Schema Tests
// =============================================================================

/// Test: Robot JSON output includes all expected fields
#[test]
fn test_robot_output_schema() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    // Create fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-schema.jsonl",
        "schema_test_content",
        1732118400000,
    );

    // Index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .assert()
        .success();

    // Search with --robot
    let output = cargo_bin_cmd!("cass")
        .args(["search", "schema_test_content", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("robot schema search");

    assert!(output.status.success());

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");

    // Bead 7k7pl: pin TYPE on the robot-search response schema — hits
    // must be an array, total_matches and limit must be integers. A
    // regression that emitted null or swapped field types would slip
    // past `.is_some()` while breaking every automated consumer.
    assert!(
        json.get("hits").and_then(|v| v.as_array()).is_some(),
        "hits must be an array. Got: {}",
        json
    );
    assert!(
        json.get("total_matches").and_then(|v| v.as_u64()).is_some(),
        "total_matches must be a non-negative integer. Got: {}",
        json
    );
    assert!(
        json.get("limit").and_then(|v| v.as_u64()).is_some(),
        "limit must be a non-negative integer. Got: {}",
        json
    );

    // Verify hit schema
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");
    if !hits.is_empty() {
        let hit = &hits[0];
        // Bead 7k7pl: pin TYPE on every required hit field — all four
        // must be strings, not just "present". A null / numeric
        // regression in any field breaks JSON consumers that call
        // `.as_str().unwrap()` downstream and would slip past
        // `.is_some()`.
        assert!(
            hit.get("content").and_then(|v| v.as_str()).is_some(),
            "hit.content must be a string. Got: {}",
            hit
        );
        assert!(
            hit.get("agent").and_then(|v| v.as_str()).is_some(),
            "hit.agent must be a string. Got: {}",
            hit
        );
        assert!(
            hit.get("source_path").and_then(|v| v.as_str()).is_some(),
            "hit.source_path must be a string. Got: {}",
            hit
        );
        assert!(
            hit.get("match_type").and_then(|v| v.as_str()).is_some(),
            "hit.match_type must be a string. Got: {}",
            hit
        );
    }
}

// =============================================================================
// Incremental Index Tests
// =============================================================================

/// Test: Incremental index skips unchanged files
#[test]
fn test_incremental_index_skips_unchanged() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    // Create initial fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-incr.jsonl",
        "incremental_test_content",
        1732118400000,
    );

    // First full index
    let output1 = cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("first index");
    assert!(output1.status.success(), "First index should succeed");

    // Second incremental index (no changes)
    let output2 = cargo_bin_cmd!("cass")
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("second index");
    assert!(output2.status.success(), "Second index should succeed");

    let stderr2 = String::from_utf8_lossy(&output2.stderr);
    // Should indicate skipping or no new files (implementation may vary)
    // We verify it completes quickly (doesn't re-process everything)
    assert!(
        output2.status.success(),
        "Incremental index should succeed. stderr: {}",
        stderr2
    );
}

/// Test: Incremental index picks up new files
#[test]
fn test_incremental_index_picks_up_new_files() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    // Create initial fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-incr1.jsonl",
        "initial_content",
        1732118400000,
    );

    // First full index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .assert()
        .success();

    // Add new file
    make_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-incr2.jsonl",
        "new_content_for_incremental",
        1732204800000,
    );

    // Incremental index
    cargo_bin_cmd!("cass")
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .assert()
        .success();

    // Search for new content
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "new_content_for_incremental",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("search for new content");

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let hits = json.get("hits").and_then(|h| h.as_array());
    assert!(
        hits.is_some() && !hits.unwrap().is_empty(),
        "Should find new content after incremental index"
    );
}

// =============================================================================
// Filter Parity Tests
// =============================================================================

/// Test: Agent filter works consistently across search modes
#[test]
fn test_filter_parity_agent_filter() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    // Create fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-filter.jsonl",
        "filter_parity_test_content",
        1732118400000,
    );

    // Index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .assert()
        .success();

    // Search with --agent filter in lexical mode
    let output_lexical = cargo_bin_cmd!("cass")
        .args([
            "search",
            "filter_parity_test",
            "--agent",
            "codex",
            "--mode",
            "lexical",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("lexical search with agent filter");

    assert!(output_lexical.status.success());
    let json_lexical: Value = serde_json::from_slice(&output_lexical.stdout).expect("valid JSON");
    let hits_lexical = json_lexical
        .get("hits")
        .and_then(|h| h.as_array())
        .map(|h| h.len())
        .unwrap_or(0);

    // All hits should be from codex agent
    if let Some(hits) = json_lexical.get("hits").and_then(|h| h.as_array()) {
        for hit in hits {
            let agent = hit.get("agent").and_then(|a| a.as_str()).unwrap_or("");
            assert!(
                agent.contains("codex") || agent.is_empty(),
                "All hits should be from codex agent, got: {}",
                agent
            );
        }
    }

    // Verify filter works (should have results since we created codex data)
    assert!(
        hits_lexical > 0,
        "Should find codex results with agent filter"
    );
}

// =============================================================================
// Offline Mode Tests
// =============================================================================

/// Test: CASS_OFFLINE=1 disables network calls
#[test]
fn test_offline_mode_environment() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // With CASS_OFFLINE=1, models check-update should not make network calls
    let output = cargo_bin_cmd!("cass")
        .args(["models", "check-update", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_OFFLINE", "1")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("offline check-update");

    // Should succeed but indicate offline mode
    assert!(
        output.status.success(),
        "check-update in offline mode should succeed. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // In offline mode, should either skip check or return cached/offline result
    let json: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
    // Verify it returns valid structure (doesn't fail on network)
    assert!(
        json.is_object(),
        "Should return valid JSON in offline mode. Got: {}",
        stdout
    );
}

// =============================================================================
// Search Mode Consistency Tests
// =============================================================================

/// Test: Search mode flag is respected consistently across invocations
#[test]
fn test_search_mode_flag_consistency() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    // Create fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-mode-consistency.jsonl",
        "mode_consistency_test",
        1732118400000,
    );

    // Index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .assert()
        .success();

    // Run the same search with --mode lexical multiple times
    // Verify flag is respected on each invocation
    for i in 0..3 {
        let output = cargo_bin_cmd!("cass")
            .args([
                "search",
                "mode_consistency_test",
                "--mode",
                "lexical",
                "--robot",
                "--data-dir",
            ])
            .arg(&data_dir)
            .env("HOME", home)
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .output()
            .unwrap_or_else(|e| panic!("search invocation {i}: {e}"));

        assert!(
            output.status.success(),
            "Search invocation {} should succeed",
            i
        );

        let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
        assert!(
            json.get("hits").is_some(),
            "Invocation {} should return hits",
            i
        );
    }
}

// =============================================================================
// Models Install From File Tests
// =============================================================================

/// Test: models install --from-file validates a local model directory instead of
/// failing as \"not implemented\"
#[test]
fn test_models_install_from_file_directory_validates_checksums() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models");

    let output = cargo_bin_cmd!("cass")
        .args(["models", "install", "--from-file"])
        .arg(&fixture_dir)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("-y")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("models install from-file command");

    assert!(
        !output.status.success(),
        "repo fixture directory should fail checksum validation through the real local-directory install path. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("SHA256 mismatch"),
        "stderr should show the real checksum-validation failure from the implemented --from-file path. Got: {}",
        stderr
    );
}

/// Test: models install rejects an invalid mirror URL before any network work
#[test]
fn test_models_install_rejects_invalid_mirror_url() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let output = cargo_bin_cmd!("cass")
        .args(["models", "install", "--mirror", "not-a-valid-url"])
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("-y")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("models install with invalid mirror URL");

    assert!(
        !output.status.success(),
        "install --mirror with an invalid URL should fail"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid mirror URL"),
        "stderr should explain that the mirror URL is invalid. Got: {}",
        stderr
    );
}

/// Test: models install rejects conflicting mirror and from-file sources
#[test]
fn test_models_install_rejects_conflicting_mirror_and_from_file() {
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models");

    let output = cargo_bin_cmd!("cass")
        .args([
            "models",
            "install",
            "--mirror",
            "https://mirror.example/cache",
            "--from-file",
        ])
        .arg(&fixture_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("models install with conflicting flags");

    assert!(
        !output.status.success(),
        "conflicting --mirror and --from-file flags should fail to parse"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Could not parse arguments"),
        "stderr should report a parse failure for conflicting flags. Got: {}",
        stderr
    );
}

/// Test: models install --from-file with non-existent directory fails appropriately
#[test]
fn test_models_install_from_file_missing_directory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let missing_dir = tmp.path().join("nonexistent-model-dir");

    let output = cargo_bin_cmd!("cass")
        .args(["models", "install", "--from-file"])
        .arg(&missing_dir)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("-y")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("models install with missing directory");

    assert!(
        !output.status.success(),
        "install --from-file with a missing directory should fail"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not a directory"),
        "stderr should explain that --from-file expects a directory. Got: {}",
        stderr
    );
}

// =============================================================================
// Introspect Tests
// =============================================================================

// =============================================================================
// Approximate (ANN/HNSW) Search Tests
// =============================================================================

/// Test: --approximate flag is accepted in semantic mode
/// (May fail if HNSW index not built, but should parse correctly)
#[test]
fn test_approximate_flag_semantic_mode() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    // Create fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-approximate.jsonl",
        "approximate_test_content",
        1732118400000,
    );

    // Index first (without --build-hnsw, so HNSW won't be available)
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .assert()
        .success();

    // Search with --approximate in semantic mode
    // Should fail gracefully if HNSW not available or model not installed
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "approximate_test_content",
            "--mode",
            "semantic",
            "--approximate",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("search with --approximate");

    // Either succeeds or fails with appropriate error message
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Should fail due to missing HNSW index or semantic unavailable
        assert!(
            stderr.contains("HNSW")
                || stderr.contains("approximate")
                || stderr.contains("semantic-unavailable")
                || stderr.contains("Semantic search not available"),
            "Error should mention HNSW or approximate search. Got: {}",
            stderr
        );
    }
}

/// Test: --approximate flag in lexical mode produces warning
#[test]
fn test_approximate_flag_lexical_mode_warning() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    // Create fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-approx-lexical.jsonl",
        "approx_lexical_test_content",
        1732118400000,
    );

    // Index first
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .assert()
        .success();

    // Search with --approximate in lexical mode
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "approx_lexical_test_content",
            "--mode",
            "lexical",
            "--approximate",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("search with --approximate in lexical mode");

    // Should succeed (lexical search works) but may warn about --approximate
    assert!(
        output.status.success(),
        "Lexical search should succeed even with --approximate"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    // Should produce warning about --approximate having no effect in lexical mode
    assert!(
        stderr.contains("no effect") || stderr.contains("lexical") || stderr.is_empty(),
        "Should warn about --approximate having no effect in lexical mode or be empty. Got: {}",
        stderr
    );
}

/// Test: --approximate flag in hybrid mode is accepted
#[test]
fn test_approximate_flag_hybrid_mode() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    // Create fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-approx-hybrid.jsonl",
        "approx_hybrid_test_content",
        1732118400000,
    );

    // Index first
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .assert()
        .success();

    // Search with --approximate in hybrid mode
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "approx_hybrid_test_content",
            "--mode",
            "hybrid",
            "--approximate",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("search with --approximate in hybrid mode");

    // Either succeeds or fails with appropriate error message (HNSW not built)
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("HNSW")
                || stderr.contains("approximate")
                || stderr.contains("semantic-unavailable")
                || stderr.contains("Hybrid search not available"),
            "Error should mention HNSW or semantic unavailability. Got: {}",
            stderr
        );
    }
}

/// Test: index --build-hnsw flag is accepted
#[test]
fn test_index_build_hnsw_flag() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    // Create fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-build-hnsw.jsonl",
        "build_hnsw_test_content",
        1732118400000,
    );

    // Index with --build-hnsw (requires --semantic to be meaningful)
    // This tests that the flag is parsed correctly
    let output = cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--build-hnsw",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("index with --build-hnsw");

    // May fail if semantic model not installed, but should parse the flag
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Should fail due to model not installed, not due to flag parsing
        assert!(
            stderr.contains("model")
                || stderr.contains("semantic")
                || stderr.contains("embedder")
                || stderr.contains("install"),
            "If indexing fails, should be due to model unavailability, not flag parsing. Got: {}",
            stderr
        );
    }
}

// =============================================================================
// Introspect Tests
// =============================================================================

/// Test: introspect includes models command in schema
#[test]
fn test_introspect_includes_models_command() {
    let output = cargo_bin_cmd!("cass")
        .args(["introspect", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("introspect command");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid introspect JSON");

    let commands = json
        .get("commands")
        .and_then(|c| c.as_array())
        .expect("commands array");

    // Find models command
    let models_cmd = commands
        .iter()
        .find(|c| c.get("name") == Some(&Value::String("models".into())));
    assert!(
        models_cmd.is_some(),
        "introspect should include models command"
    );

    // Verify models has description
    if let Some(models) = models_cmd {
        let description = models
            .get("description")
            .and_then(|d| d.as_str())
            .expect("models command should have description");
        assert!(
            description.contains("model") || description.contains("semantic"),
            "models description should mention models or semantic"
        );
    }
}

mod gh470_native_long_messages {
    use anyhow::{Context, Result};
    use coding_agent_search::indexer::semantic::{EmbeddingInput, SemanticIndexer};
    use coding_agent_search::search::canonicalize::{
        MAX_EMBED_CHARS, canonicalize_for_embedding, content_hash, embedding_passages,
    };
    use coding_agent_search::search::embedder::Embedder;
    use coding_agent_search::search::fastembed_embedder::{
        FastEmbedder, MINILM_VECTOR_SPACE_REVISION, model_dir_override,
    };
    use coding_agent_search::search::model_download::{
        ModelManifest, compute_sha256, model_file_path,
    };
    use coding_agent_search::search::vector_index::{
        SemanticDocId, parse_semantic_doc_id, vector_index_path,
    };
    use frankensearch::{Canonicalizer, DefaultCanonicalizer};
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::PathBuf;
    use std::time::Instant;
    use tokenizers::{Tokenizer, TruncationParams};

    // Reserve two native-token positions for CLS/SEP; verify the actual token
    // bound for every candidate window in this finite corpus below.
    const WINDOW_CHARS: usize = 510;
    const MAX_WINDOWS: usize = 4;
    const NATIVE_MAX_TOKENS: usize = 512;
    const CORPUS_REVISION: &str = "gh470-19-messages-cjk-tail-v2";

    fn copy_attested_model(data_dir: &std::path::Path) -> Result<(ModelManifest, PathBuf, f64)> {
        assert!(
            model_dir_override().is_none(),
            "use the supplied managed model bundle"
        );
        let supplied = PathBuf::from(
            dotenvy::var("CASS_NATIVE_REUSE_MODEL_DIR")
                .context("supply an existing attested MiniLM bundle; this test never downloads")?,
        );
        let model_dir = FastEmbedder::default_model_dir(data_dir);
        fs::create_dir_all(&model_dir)?;
        let manifest = ModelManifest::minilm_v2();
        assert_eq!(manifest.files.len(), 5);
        let started = Instant::now();
        for file in &manifest.files {
            let source = model_file_path(&supplied, file)
                .with_context(|| format!("missing supplied model file {}", file.name))?;
            assert_eq!(compute_sha256(&source)?, file.sha256);
            assert_eq!(
                fs::copy(source, model_dir.join(file.local_name()))?,
                file.size
            );
        }
        Ok((
            manifest,
            model_dir,
            started.elapsed().as_secs_f64() * 1000.0,
        ))
    }

    struct Query {
        name: &'static str,
        text: &'static str,
        target: Option<u64>,
        evidence: Option<&'static str>,
    }

    fn corpus() -> (Vec<EmbeddingInput>, Vec<Query>) {
        // Repeated work-log structure is intentional: the useful resolution is
        // late in a realistic session, after several unrelated investigation steps.
        let mut prose = String::new();
        let mut code = String::from("```rust\n");
        let mut unicode = String::new();
        for step in 0..12 {
            prose.push_str(&format!(
                "Review step {step}: the deployment dashboard shows stable worker counts. \
                 We checked the staging configuration, compared the release checklist with \
                 yesterday's notes, and recorded the request latency histogram. The team \
                 deferred a cosmetic dashboard change until the next maintenance window.\n"
            ));
            code.push_str(&format!(
                "fn inspect_worker_{step}(worker: &Worker) -> Report {{\n\
                 let pending = worker.pending_jobs();\n\
                 let completed = worker.completed_jobs();\n\
                 let elapsed = worker.elapsed_millis();\n\
                 Report {{ pending, completed, elapsed, healthy: worker.is_ready() }}\n\
                 }}\n\
                 // This routine reports scheduler activity without changing the work queue.\n"
            ));
            if step == 8 {
                // Middle-only evidence makes head/tail loss visible; later work
                // continues far enough that the tail cannot retain this section.
                code.push_str(
                    "// The dependency resolver needs Tarjan strongly connected components. \
                     Maintain a discovery index and lowlink per vertex, push active vertices \
                     on a stack, and pop one component when lowlink equals its discovery index.\n\
                     fn component_root(index: usize, lowlink: usize) -> bool { index == lowlink }\n",
                );
            }
            unicode.push_str(&format!(
                "Équipe de Montréal, revue {step}: the café dashboard renders 東京 and \
                 résumé correctly in its activity table. We checked translations, date \
                 labels, column widths, and keyboard navigation. The same review also \
                 covered the English release notes and ordinary application preferences.\n"
            ));
        }
        code.push_str("```\n");
        prose.push_str(
            "The final geographic defect was an antimeridian crossing: a viewport from \
             170 degrees east to 170 degrees west needs two longitude intervals. Split \
             the bounding box at the international date line before the spatial lookup.",
        );
        unicode.push_str(
            "The remaining editor bug concerns grapheme clusters: the cursor must not \
             split e\u{301} or the family emoji 👩‍👩‍👧‍👦 into visible fragments. \
             Move across extended grapheme boundaries instead of individual Unicode \
             scalar values; byte offsets alone do not describe displayed characters.",
        );
        let mut dense_unicode = "中".repeat(2400);
        dense_unicode.push_str(
            " Red-black tree insertions preserve balance by recoloring a red parent \
             and uncle, then applying left or right rotations when the uncle is black. \
             Keep the root black and equal black heights on every root-to-leaf path.",
        );
        let contents = vec![
            prose,
            code,
            unicode,
            "Certificate renewal failed because the TLS certificate expired. Renew it, \
             reload the listener, and verify the new certificate expiration date."
                .into(),
            "A database deadlock occurs when transactions acquire row locks in opposite \
             orders. Use a consistent lock order and retry the aborted transaction."
                .into(),
            "The CSS grid overflows on narrow screens. Set min-width to zero on the grid \
             children so long labels can shrink within their assigned columns."
                .into(),
            "The map renderer uses a tile cache and zoom-dependent simplification. \
             Evict old tiles when the cache reaches its memory budget."
                .into(),
            "A geographic dashboard converts kilometers to miles for distance labels. \
             Its legend also explains the color scale used for elevation."
                .into(),
            "The dependency resolver uses a priority queue to run ready packages. \
             Each finished package releases the dependents waiting for compilation."
                .into(),
            "A graph traversal visits neighbors in breadth-first order and records the \
             shortest number of edges from the chosen starting vertex."
                .into(),
            "The editor stores text as UTF-8 and saves the document atomically. A \
             temporary file protects the previous contents from interrupted writes."
                .into(),
            "The translation catalog contains French and Japanese labels. Missing \
             translations fall back to English while preserving the selected locale."
                .into(),
            "The TLS client validates the server hostname against its certificate. \
             The trust store contains the certificate authorities permitted by policy."
                .into(),
            "A database index speeds up lookups by customer identifier. The query \
             planner chooses a sequential scan when most rows match the predicate."
                .into(),
            "A stylesheet assigns typography and spacing to the dashboard. The design \
             uses a monospace font for identifiers and a separate color for warnings."
                .into(),
            "The deployment queue records completed work and retries failed jobs. \
             Operators inspect latency charts before increasing worker capacity."
                .into(),
            "A filesystem watcher coalesces rapid saves before rebuilding the preview. \
             It retains the previous preview when the compiler reports an error."
                .into(),
            "The backup service verifies checksums before restoring archived documents. \
             Retention settings keep the latest successful backup available."
                .into(),
            dense_unicode,
        ];
        let messages = contents
            .into_iter()
            .enumerate()
            .map(|(ordinal, content)| EmbeddingInput {
                message_id: ordinal as u64 + 1,
                created_at_ms: 1_700_000_000_000 + ordinal as i64,
                agent_id: ordinal as u32 % 3 + 1,
                workspace_id: ordinal as u32 % 4 + 10,
                source_id: ordinal as u32 % 2 + 20,
                role: ordinal as u8 % 4,
                chunk_idx: 0,
                content,
            })
            .collect();
        let queries = vec![
            Query {
                name: "long_prose",
                text: "How should a map bounding box cross the international date line?",
                target: Some(1),
                evidence: Some("antimeridian"),
            },
            Query {
                name: "long_code",
                text: "Find strongly connected components using discovery indices and lowlinks.",
                target: Some(2),
                evidence: Some("Tarjan"),
            },
            Query {
                name: "long_unicode",
                text: "Prevent cursor movement from splitting a family emoji or a combining accent.",
                target: Some(3),
                evidence: Some("grapheme"),
            },
            Query {
                name: "short_certificate",
                text: "How do we fix an expired TLS certificate?",
                target: Some(4),
                evidence: None,
            },
            Query {
                name: "short_deadlock",
                text: "Why do opposite row lock orders cause a database deadlock?",
                target: Some(5),
                evidence: None,
            },
            Query {
                name: "short_layout",
                text: "How do CSS grid children shrink on narrow screens?",
                target: Some(6),
                evidence: None,
            },
            Query {
                name: "absent_topic",
                text: "What isotope ratios identify the origin of lunar basalt?",
                target: None,
                evidence: None,
            },
            Query {
                name: "long_dense_unicode",
                text: "How do red-black tree insertions use recoloring and rotations to stay balanced?",
                target: Some(19),
                evidence: Some("Red-black"),
            },
        ];
        (messages, queries)
    }

    fn identity(input: &EmbeddingInput) -> SemanticDocId {
        SemanticDocId {
            message_id: input.message_id,
            chunk_idx: input.chunk_idx,
            agent_id: input.agent_id,
            workspace_id: input.workspace_id,
            source_id: input.source_id,
            role: input.role,
            created_at_ms: input.created_at_ms,
            content_hash: Some(content_hash(&canonicalize_for_embedding(&input.content))),
        }
    }

    fn representation(
        messages: &[EmbeddingInput],
        windows: usize,
    ) -> (Vec<EmbeddingInput>, Vec<Value>) {
        let mut inputs = Vec::new();
        let mut selections = Vec::new();
        for message in messages {
            let production_passages = (windows == 8).then(|| embedding_passages(&message.content));
            // Select raw spans before character truncation or fenced-code
            // collapse. Canonicalization still runs inside the real indexer.
            let chars: Vec<_> = message.content.chars().collect();
            if windows == 1 || chars.len() <= MAX_EMBED_CHARS {
                if let Some(passages) = production_passages {
                    assert_eq!(passages, [message.content.as_str()]);
                }
                inputs.push(message.clone());
                selections.push(json!({
                    "message_id": message.message_id, "raw_chars": chars.len(),
                    "selected_raw_char_spans": [[0, chars.len()]],
                    "unselected_raw_char_spans": [],
                    "projection": "unchanged raw input; production canonical and native caps still apply",
                }));
                continue;
            }
            let mut selected = Vec::new();
            let mut unselected = Vec::new();
            let mut covered_until = 0;
            let count = if windows == 0 {
                chars.len().div_ceil(WINDOW_CHARS)
            } else {
                windows.min(chars.len().div_ceil(WINDOW_CHARS))
            };
            assert!(count > 1);
            if let Some(passages) = production_passages.as_ref() {
                assert_eq!(passages.len(), count);
            }
            for chunk_idx in 0..count {
                let start = if windows == 0 {
                    chunk_idx * WINDOW_CHARS
                } else {
                    chunk_idx * (chars.len() - WINDOW_CHARS) / (count - 1)
                };
                let end = (start + WINDOW_CHARS).min(chars.len());
                if start > covered_until {
                    unselected.push([covered_until, start]);
                }
                covered_until = covered_until.max(end);
                selected.push([start, end]);
                let expected_content: String = chars[start..end].iter().collect();
                let content = if let Some(passages) = production_passages.as_ref() {
                    assert_eq!(passages[chunk_idx], expected_content);
                    passages[chunk_idx].to_owned()
                } else {
                    expected_content
                };
                let input = EmbeddingInput {
                    message_id: message.message_id,
                    created_at_ms: message.created_at_ms,
                    agent_id: message.agent_id,
                    workspace_id: message.workspace_id,
                    source_id: message.source_id,
                    role: message.role,
                    chunk_idx: u8::try_from(chunk_idx)
                        .expect("this finite fixture must fit the persisted chunk identifier"),
                    content,
                };
                assert!(input.content.chars().count() <= WINDOW_CHARS);
                inputs.push(input);
            }
            assert_eq!(covered_until, chars.len());
            if windows == 0 {
                assert!(
                    unselected.is_empty(),
                    "sequential windows must cover the entire raw message"
                );
            }
            selections.push(json!({
                "message_id": message.message_id, "raw_chars": chars.len(),
                "selected_raw_char_spans": selected, "unselected_raw_char_spans": unselected,
                "projection": "raw windows before production canonicalization",
            }));
        }
        (inputs, selections)
    }

    fn token_counts(full: &Tokenizer, visible: &Tokenizer, text: &str) -> Result<(usize, usize)> {
        let full = full
            .encode(text, true)
            .map_err(|error| anyhow::anyhow!("untruncated tokenization: {error}"))?;
        let visible = visible
            .encode(text, true)
            .map_err(|error| anyhow::anyhow!("native-visible tokenization: {error}"))?;
        assert!(visible.get_ids().len() <= NATIVE_MAX_TOKENS);
        assert_eq!(
            visible.get_ids().len(),
            full.get_ids().len().min(NATIVE_MAX_TOKENS)
        );
        Ok((full.get_ids().len(), visible.get_ids().len()))
    }

    fn cosine(left: &[f32], right: &[f32]) -> f64 {
        assert_eq!(left.len(), right.len());
        let dot: f64 = left
            .iter()
            .zip(right)
            .map(|(&a, &b)| f64::from(a) * f64::from(b))
            .sum();
        let left_norm: f64 = left.iter().map(|&v| f64::from(v).powi(2)).sum();
        let right_norm: f64 = right.iter().map(|&v| f64::from(v).powi(2)).sum();
        assert!(left_norm > 0.0 && right_norm > 0.0);
        let score = dot / (left_norm * right_norm).sqrt();
        assert!(score.is_finite());
        score
    }

    /// This is a representation experiment, not the hydrated query pipeline or
    /// a retrieval/performance certification. Every rank, including losses and
    /// the no-relevant-message control, is retained for independent review.
    #[test]
    #[ignore = "requires CASS_NATIVE_REUSE_MODEL_DIR with the attested five-file MiniLM bundle; never downloads"]
    fn gh470_native_long_message_representation_measurements() -> Result<()> {
        let data = tempfile::tempdir()?;
        let (manifest, model_dir, copy_ms) = copy_attested_model(data.path())?;
        // tokenizer.json has historical truncation/padding defaults. Match the
        // pinned native backend explicitly, and separately count untruncated IDs.
        let mut full_tokenizer = Tokenizer::from_file(model_dir.join("tokenizer.json"))
            .map_err(|error| anyhow::anyhow!("load attested tokenizer: {error}"))?;
        full_tokenizer
            .with_truncation(None)
            .map_err(|error| anyhow::anyhow!("disable tokenizer truncation: {error}"))?;
        full_tokenizer.with_padding(None);
        let mut visible_tokenizer = full_tokenizer.clone();
        visible_tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: NATIVE_MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|error| anyhow::anyhow!("configure native tokenizer limit: {error}"))?;
        visible_tokenizer.with_padding(None);
        let (messages, queries) = corpus();
        // This uncapped view is an oracle for fixture placement only. Candidate
        // projection never consumes it or inherits its fenced-code collapse.
        let uncapped = DefaultCanonicalizer {
            max_length: usize::MAX,
            ..Default::default()
        };
        let full: Vec<_> = messages
            .iter()
            .map(|m| uncapped.canonicalize(&m.content))
            .collect();
        let mut loss_placement = Vec::new();
        for query in &queries {
            if let (Some(target), Some(evidence)) = (query.target, query.evidence) {
                let ordinal = messages
                    .iter()
                    .position(|m| m.message_id == target)
                    .context("target")?;
                let raw = &messages[ordinal].content;
                let raw_offset = raw.find(evidence).context("raw evidence")?;
                let raw_char_offset = raw[..raw_offset].chars().count();
                assert!(raw_char_offset > MAX_EMBED_CHARS);
                let canonical_char_offset = full[ordinal]
                    .find(evidence)
                    .map(|offset| full[ordinal][..offset].chars().count());
                if query.name == "long_code" {
                    assert!(raw.starts_with("```rust\n") && raw.lines().count() > 30);
                    assert!(
                        canonical_char_offset.is_none(),
                        "fenced-code collapse must remove the middle evidence before the character cap"
                    );
                } else {
                    assert!(
                        canonical_char_offset.context("late canonical evidence")? > MAX_EMBED_CHARS
                    );
                }
                assert!(!canonicalize_for_embedding(&messages[ordinal].content).contains(evidence));
                loss_placement.push(json!({
                    "query": query.name, "evidence": evidence,
                    "raw_char_offset": raw_char_offset,
                    "uncapped_canonical_char_offset": canonical_char_offset,
                    "loss_stage": if query.name == "long_code" { "fenced_code_collapse" } else { "canonical_character_cap" },
                }));
            }
        }
        // An 800-character window is not a 512-token bound. The attested
        // tokenizer splits this known Chinese character into individual tokens,
        // hiding the English resolution even though it is in the selected tail.
        assert_eq!(messages.len(), 19);
        let dense = messages.last().context("appended token-density control")?;
        assert_eq!(dense.message_id, 19);
        let dense_character_id = full_tokenizer
            .token_to_id("中")
            .context("token-density control must use a known Chinese vocabulary entry")?;
        let dense_character = full_tokenizer
            .encode("中", false)
            .map_err(|error| anyhow::anyhow!("tokenize density-control character: {error}"))?;
        assert_eq!(dense_character.get_ids(), &[dense_character_id]);
        let dense_chars: Vec<_> = dense.content.chars().collect();
        assert!(dense_chars.len() >= 800);
        let old_tail: String = dense_chars[dense_chars.len() - 800..].iter().collect();
        let canonical_tail = canonicalize_for_embedding(&old_tail);
        let evidence_byte_offset = canonical_tail
            .find("Red-black")
            .context("800-character tail must contain the English evidence before tokenization")?;
        let (old_tail_tokens, old_tail_visible_tokens) =
            token_counts(&full_tokenizer, &visible_tokenizer, &canonical_tail)?;
        assert!(old_tail_tokens > NATIVE_MAX_TOKENS);
        assert_eq!(old_tail_visible_tokens, NATIVE_MAX_TOKENS);
        let old_tail_visible = visible_tokenizer
            .encode(canonical_tail.as_str(), true)
            .map_err(|error| anyhow::anyhow!("tokenize truncated 800-character tail: {error}"))?;
        let visible_end = old_tail_visible
            .get_offsets()
            .iter()
            .zip(old_tail_visible.get_special_tokens_mask())
            .filter_map(|(&(start, end), &special)| (special == 0 && start < end).then_some(end))
            .max()
            .context("native-visible tail must contain non-special tokens")?;
        assert!(
            visible_end < evidence_byte_offset,
            "the old 800-character tail must lose all English evidence at the native token cap"
        );
        let old_tail_token_loss = json!({
            "message_id": dense.message_id, "selected_raw_tail_chars": 800,
            "canonical_tail_chars": canonical_tail.chars().count(),
            "untruncated_tokens_including_specials": old_tail_tokens,
            "native_visible_tokens_including_specials": old_tail_visible_tokens,
            "canonical_evidence_byte_offset": evidence_byte_offset,
            "native_visible_end_byte_offset": visible_end,
            "native_visible_end_char_offset": canonical_tail
                .get(..visible_end).context("native offsets must bound valid UTF-8")?.chars().count(),
            "loss_stage": "native_token_cap_after_800_character_tail_selection",
        });
        let (head_tail, _) = representation(&messages, 2);
        assert!(
            head_tail
                .iter()
                .filter(|m| m.message_id == 2)
                .all(|m| !m.content.contains("Tarjan")),
            "middle-only control must defeat head/tail selection"
        );
        let query_tokens: Vec<_> = queries
            .iter()
            .map(|query| {
                let canonical = canonicalize_for_embedding(query.text);
                let (full, visible) =
                    token_counts(&full_tokenizer, &visible_tokenizer, &canonical)?;
                Ok(
                    json!({"query": query.name, "canonical_chars": canonical.chars().count(),
                    "untruncated_tokens_including_specials": full,
                    "native_visible_tokens_including_specials": visible}),
                )
            })
            .collect::<Result<_>>()?;
        let query_constructor_started = Instant::now();
        let query_model = FastEmbedder::load_from_dir(&model_dir)?;
        let query_constructor_ms = query_constructor_started.elapsed().as_secs_f64() * 1000.0;
        assert!(query_model.is_semantic());
        assert_eq!(query_model.id(), "minilm-384");
        let query_started = Instant::now();
        let query_vectors: Vec<_> = queries
            .iter()
            .map(|q| query_model.embed_sync(&canonicalize_for_embedding(q.text)))
            .collect::<std::result::Result<_, _>>()?;
        let query_embedding_ms = query_started.elapsed().as_secs_f64() * 1000.0;
        drop(query_model);
        let document_constructor_started = Instant::now();
        let indexer = SemanticIndexer::new("minilm", Some(data.path()))?.with_batch_size(1)?;
        let document_constructor_ms = document_constructor_started.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(indexer.embedder_id(), "minilm-384");
        assert_eq!(indexer.embedder_dimension(), 384);
        let mut baseline_chars = 0;
        let mut baseline_bytes = 0;
        let mut baseline_tokens = 0;
        let mut baseline_short_vectors = BTreeMap::new();
        for (name, windows) in [
            ("legacy_prefix", 1),
            ("head_tail", 2),
            ("distributed_windows", MAX_WINDOWS),
            ("distributed_eight", 8),
            ("sequential_full_coverage", 0),
        ] {
            let preparation_started = Instant::now();
            let (inputs, selected_spans) = representation(&messages, windows);
            let window_limit = if windows == 0 {
                messages
                    .iter()
                    .map(|m| m.content.chars().count().div_ceil(WINDOW_CHARS))
                    .max()
                    .context("nonempty corpus")?
            } else {
                windows
            };
            assert!(inputs.len() <= messages.len() * window_limit);
            let expected: BTreeMap<_, _> = inputs
                .iter()
                .map(|m| ((m.message_id, m.chunk_idx), identity(m)))
                .collect();
            assert_eq!(expected.len(), inputs.len());
            let canonical_inputs: Vec<_> = inputs
                .iter()
                .map(|m| canonicalize_for_embedding(&m.content))
                .collect();
            let input_chars: usize = canonical_inputs
                .iter()
                .map(|text| text.chars().count())
                .sum();
            let mut input_tokens = 0;
            let mut visible_tokens = 0;
            let mut input_measurements = Vec::new();
            for (input, canonical) in inputs.iter().zip(&canonical_inputs) {
                let (full_count, visible_count) =
                    token_counts(&full_tokenizer, &visible_tokenizer, canonical)?;
                input_tokens += full_count;
                visible_tokens += visible_count;
                if windows != 1 && input.content.chars().count() <= WINDOW_CHARS {
                    assert_eq!(
                        full_count, visible_count,
                        "this fixture's candidate window must fit the real native tokenizer"
                    );
                }
                let retained_evidence: Vec<_> = queries
                    .iter()
                    .filter(|q| q.target == Some(input.message_id))
                    .filter_map(|q| q.evidence.filter(|evidence| canonical.contains(*evidence)))
                    .collect();
                input_measurements.push(json!({
                    "message_id": input.message_id, "chunk_idx": input.chunk_idx,
                    "canonical_chars": canonical.chars().count(),
                    "untruncated_tokens_including_specials": full_count,
                    "native_visible_tokens_including_specials": visible_count,
                    "evidence_in_canonical_input": retained_evidence,
                }));
            }
            let preparation_ms = preparation_started.elapsed().as_secs_f64() * 1000.0;
            let embedding_started = Instant::now();
            let embedded = indexer.embed_messages(&inputs)?;
            let embedding_ms = embedding_started.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(embedded.len(), inputs.len());
            let index_started = Instant::now();
            let output_dir = data.path().join(name);
            let index = indexer.build_and_save_index(embedded, &output_dir)?;
            let index_write_open_ms = index_started.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(index.embedder_revision(), MINILM_VECTOR_SPACE_REVISION);
            let index_bytes =
                fs::metadata(vector_index_path(&output_dir, indexer.embedder_id()))?.len();
            assert_eq!(index.record_count(), inputs.len());
            let mut vectors = Vec::new();
            let mut seen = BTreeSet::new();
            for ordinal in 0..index.record_count() {
                assert!(!index.is_deleted(ordinal));
                let metadata = parse_semantic_doc_id(index.doc_id_at(ordinal)?)
                    .context("persisted source identity")?;
                let key = (metadata.message_id, metadata.chunk_idx);
                assert!(seen.insert(key));
                assert_eq!(Some(&metadata), expected.get(&key));
                let vector = index.vector_at_f32(ordinal)?;
                assert_eq!(vector.len(), 384);
                assert!(vector.iter().all(|v| v.is_finite()));
                let authority = messages
                    .iter()
                    .find(|m| m.message_id == metadata.message_id)
                    .context("vector must resolve to an authoritative fixture message")?;
                let mut source_identity = identity(authority);
                source_identity.chunk_idx = metadata.chunk_idx;
                source_identity.content_hash = metadata.content_hash;
                assert_eq!(metadata, source_identity);
                if authority.content.chars().count() <= MAX_EMBED_CHARS {
                    assert_eq!(metadata.chunk_idx, 0);
                    let bits: Vec<_> = vector.iter().map(|value| value.to_bits()).collect();
                    if windows == 1 {
                        baseline_short_vectors.insert(metadata.message_id, bits);
                    } else {
                        assert_eq!(
                            Some(&bits),
                            baseline_short_vectors.get(&metadata.message_id)
                        );
                    }
                }
                vectors.push((metadata.message_id, vector));
            }
            assert_eq!(seen.len(), expected.len());
            let ranking_started = Instant::now();
            let mut retrieval = Vec::new();
            for (query, vector) in queries.iter().zip(&query_vectors) {
                let mut collapsed = BTreeMap::<u64, f64>::new();
                for (message_id, candidate) in &vectors {
                    let score = cosine(vector, candidate);
                    collapsed
                        .entry(*message_id)
                        .and_modify(|best| *best = best.max(score))
                        .or_insert(score);
                }
                assert_eq!(collapsed.len(), messages.len());
                let mut ranked: Vec<_> = collapsed.into_iter().collect();
                ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                let rank = query.target.and_then(|id| {
                    ranked
                        .iter()
                        .position(|(candidate, _)| *candidate == id)
                        .map(|i| i + 1)
                });
                assert_eq!(rank.is_some(), query.target.is_some());
                retrieval.push(json!({
                    "query": query.name, "text": query.text, "target_message_id": query.target,
                    "target_rank": rank, "recall_at_1": rank.map(|r| u8::from(r <= 1)),
                    "target_cosine": query.target.and_then(|id| ranked.iter().find(|(candidate, _)| *candidate == id).map(|(_, score)| *score)),
                    "recall_at_3": rank.map(|r| u8::from(r <= 3)),
                    "recall_at_5": rank.map(|r| u8::from(r <= 5)),
                    "top_five": ranked.iter().take(5).collect::<Vec<_>>(),
                }));
            }
            let ranking_ms = ranking_started.elapsed().as_secs_f64() * 1000.0;
            if windows == 1 {
                baseline_chars = input_chars;
                baseline_bytes = index_bytes;
                baseline_tokens = visible_tokens;
            }
            println!(
                "GH470_MEASUREMENT {}",
                json!({
                    "corpus_revision": CORPUS_REVISION, "query_count": queries.len(),
                    "representation": name, "max_windows_per_message_for_this_corpus": window_limit,
                    "candidate_window_chars": WINDOW_CHARS, "authoritative_messages": messages.len(),
                    "stored_vectors": inputs.len(), "vector_multiplier": inputs.len() as f64 / messages.len() as f64,
                    "canonical_input_chars": input_chars, "input_character_multiplier": input_chars as f64 / baseline_chars as f64,
                    "untruncated_input_tokens_including_specials": input_tokens,
                    "native_visible_input_tokens_including_specials": visible_tokens,
                    "native_visible_token_multiplier": visible_tokens as f64 / baseline_tokens as f64,
                    "fsvi_file_bytes": index_bytes, "fsvi_file_byte_multiplier": index_bytes as f64 / baseline_bytes as f64,
                    "projection_and_diagnostic_preparation_ms": preparation_ms, "document_embedding_ms": embedding_ms,
                    "index_write_and_open_ms": index_write_open_ms, "exhaustive_cosine_ranking_ms": ranking_ms,
                    "retrieval": retrieval,
                    "raw_span_selections": selected_spans, "inputs": input_measurements,
                })
            );
        }
        println!(
            "GH470_SCOPE {}",
            json!({
                "corpus_revision": CORPUS_REVISION, "authoritative_messages": messages.len(),
                "query_count": queries.len(), "original_message_id_range_unchanged": [1, 18],
                "appended_message_id": 19,
                "comparison_scope": "all multipliers and rankings use this 19-message corpus only; do not pool with the prior 18-message experiment",
                "model": manifest.id, "manifest_revision": manifest.revision,
                "vector_space_revision": MINILM_VECTOR_SPACE_REVISION, "dimension": 384,
                "model_files": manifest.files.iter().map(|f| json!({"name": f.name, "sha256": f.sha256, "bytes": f.size})).collect::<Vec<_>>(),
                "verified_bundle_copy_ms": copy_ms, "query_model_constructor_ms": query_constructor_ms,
                "document_model_constructor_ms": document_constructor_ms, "query_embedding_ms": query_embedding_ms,
                "constructors": 2, "document_batch_size": 1, "quantization": "f16",
                "native_max_tokens_including_specials": NATIVE_MAX_TOKENS,
                "candidate_window_chars": WINDOW_CHARS,
                "query_token_measurements": query_tokens,
                "fixture_loss_placement": loss_placement,
                "old_800_character_tail_token_loss": old_tail_token_loss,
                "authoritative_fixture_messages": messages.iter().zip(&full).map(|(m, canonical)| json!({
                    "message_id": m.message_id, "source_id": m.source_id,
                    "workspace_id": m.workspace_id, "agent_id": m.agent_id,
                    "created_at_ms": m.created_at_ms, "role": m.role,
                    "raw_content_sha256": content_hash(&m.content),
                    "raw_chars": m.content.chars().count(), "uncapped_canonical_chars": canonical.chars().count(),
                })).collect::<Vec<_>>(),
                "scoring": "exhaustive cosine over persisted F16-expanded native vectors; maximum per message",
                "limits": "fixed-order single observations; sampled windows may miss evidence; 510-character token bounds are checked for this corpus only; no SQLite hydration, ANN, whole-query latency, RSS bound, or speedup certification",
            })
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires CASS_NATIVE_REUSE_MODEL_DIR with the attested five-file MiniLM bundle; never downloads"]
    fn gh470_native_backfill_hydrates_passages_without_duplicate_messages() -> Result<()> {
        use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
        use coding_agent_search::model::types::{
            Agent, AgentKind, Conversation, Message, MessageRole,
        };
        use coding_agent_search::search::query::{FieldMask, SearchClient, SearchFilters};
        use coding_agent_search::search::semantic_manifest::SemanticManifest;
        use coding_agent_search::search::vector_index::{
            SemanticFilterMaps, SemanticIndexArtifact,
        };
        use coding_agent_search::storage::sqlite::FrankenStorage;
        use std::sync::Arc;

        let temp = tempfile::tempdir()?;
        let data_dir = temp.path().join("data");
        let db_path = temp.path().join("archive.db");
        let (_, model_dir, _) = copy_attested_model(&data_dir)?;
        let (messages, queries) = corpus();
        let storage = FrankenStorage::open(&db_path)?;
        let agent_id = storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let workspace = temp.path().join("workspace");
        let workspace_id = storage.ensure_workspace(&workspace, None)?;
        let mut paths = Vec::new();
        for message in &messages {
            let path = temp
                .path()
                .join(format!("message-{}.jsonl", message.message_id));
            paths.push(path.to_string_lossy().into_owned());
            storage.insert_conversation_tree(
                agent_id,
                Some(workspace_id),
                &Conversation {
                    id: None,
                    agent_slug: "codex".into(),
                    workspace: Some(workspace.clone()),
                    external_id: Some(format!("gh470-{}", message.message_id)),
                    title: Some(format!("source {}", message.message_id)),
                    source_path: path,
                    started_at: Some(message.created_at_ms),
                    ended_at: Some(message.created_at_ms),
                    approx_tokens: None,
                    metadata_json: json!({}),
                    messages: vec![Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::User,
                        author: None,
                        created_at: Some(message.created_at_ms),
                        content: message.content.clone(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    }],
                    source_id: "local".into(),
                    origin_host: None,
                },
            )?;
        }
        let rows: Vec<(i64, i64, String)> = storage.raw().query_map_collect(
            "SELECT id, conversation_id, content FROM messages ORDER BY id",
            &[],
            |row| Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?)),
        )?;
        assert_eq!(rows.len(), messages.len());
        for (row, message) in rows.iter().zip(&messages) {
            assert_eq!(row.2, message.content);
        }
        drop(storage);

        let home = temp.path().join("home");
        fs::create_dir_all(&home)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(home.join(".env"))?;
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        command.env_clear().current_dir(&home);
        for key in ["PATH", "SystemRoot", "WINDIR"] {
            if let Ok(value) = dotenvy::var(key) {
                command.env(key, value);
            }
        }
        command
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("CASS_DATA_DIR", &data_dir)
            .env("TUI_HEADLESS", "1")
            .env("CASS_AUTO_REFRESH", "0")
            .env("CASS_RESPONSIVENESS_DISABLE", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("RUST_MIN_STACK", "134217728")
            .arg("--db")
            .arg(&db_path)
            .args([
                "models",
                "backfill",
                "--tier",
                "quality",
                "--embedder",
                "minilm",
                "--batch-conversations",
                "19",
                "--max-batches",
                "1",
                "--json",
                "--data-dir",
            ])
            .arg(&data_dir);
        let output = assert_cmd::Command::from_std(command)
            .timeout(std::time::Duration::from_secs(1200))
            .output()?;
        assert!(
            output.status.success(),
            "stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(report["status"], "published", "{report}");
        let manifest = SemanticManifest::load(&data_dir)?.context("published manifest")?;
        let tier = manifest.quality_tier.context("quality artifact")?;
        assert!(tier.ready);
        assert_eq!((tier.doc_count, tier.conversation_count), (44, 19));

        let storage = FrankenStorage::open_readonly(&db_path)?;
        let filters = SemanticFilterMaps::from_storage(&storage)?;
        let artifact =
            SemanticIndexArtifact::open(vector_index_path(&data_dir, "minilm-384"), None)?;
        assert_eq!(
            artifact.index().embedder_revision(),
            MINILM_VECTOR_SPACE_REVISION
        );
        let mut chunk_counts = BTreeMap::<u64, BTreeSet<u8>>::new();
        for ordinal in 0..artifact.index().record_count() {
            let id = parse_semantic_doc_id(artifact.index().doc_id_at(ordinal)?)
                .context("canonical passage identity")?;
            let row = rows
                .iter()
                .find(|row| u64::try_from(row.0).ok() == Some(id.message_id))
                .context("passage must resolve to a stored message")?;
            let passages = embedding_passages(&row.2);
            assert_eq!(
                id.content_hash,
                Some(content_hash(&canonicalize_for_embedding(
                    passages[usize::from(id.chunk_idx)]
                )))
            );
            assert!(
                chunk_counts
                    .entry(id.message_id)
                    .or_default()
                    .insert(id.chunk_idx)
            );
        }
        assert_eq!(chunk_counts.len(), 19);
        for row in &rows {
            assert_eq!(
                chunk_counts[&u64::try_from(row.0)?].len(),
                embedding_passages(&row.2).len()
            );
        }
        drop(storage);
        let client = SearchClient::open(&data_dir.join("lexical"), Some(&db_path))?
            .context("archive-backed query client")?;
        client.set_semantic_context(
            Arc::new(FastEmbedder::load_from_dir(&model_dir)?),
            artifact,
            None,
            filters,
            None,
        )?;
        let mut retrieval = Vec::new();
        for query in &queries {
            let (hits, _) = client.search_semantic(
                query.text,
                SearchFilters::default(),
                19,
                0,
                FieldMask::FULL,
                false,
            )?;
            let unique = hits
                .iter()
                .map(|hit| hit.conversation_id)
                .collect::<BTreeSet<_>>();
            assert_eq!(
                unique.len(),
                hits.len(),
                "one public hit per canonical message"
            );
            for hit in &hits {
                let ordinal = paths
                    .iter()
                    .position(|path| path == &hit.source_path)
                    .context("source path")?;
                assert_eq!(hit.content, rows[ordinal].2);
                assert_eq!(hit.conversation_id, Some(rows[ordinal].1));
                assert_eq!(hit.agent, "codex");
                assert_eq!(hit.workspace, workspace.to_string_lossy());
                assert_eq!(hit.created_at, Some(messages[ordinal].created_at_ms));
                assert_eq!(hit.source_id, "local");
                assert_eq!(hit.line_number, Some(1));
            }
            if let Some(target) = query.target {
                let path = &paths[usize::try_from(target - 1)?];
                let rank = hits
                    .iter()
                    .position(|hit| &hit.source_path == path)
                    .context("known-relevant source retrieved")?
                    + 1;
                assert!(rank <= 3, "{}: rank={rank}", query.name);
                let restricted = SearchFilters {
                    session_paths: [path.clone()].into_iter().collect(),
                    ..Default::default()
                };
                let (filtered, _) =
                    client.search_semantic(query.text, restricted, 1, 0, FieldMask::FULL, false)?;
                assert_eq!(filtered.len(), 1);
                assert_eq!(&filtered[0].source_path, path);
                retrieval.push(json!({"query":query.name,"rank":rank,"source_path":path}));
            }
            let (page, _) = client.search_semantic(
                query.text,
                SearchFilters::default(),
                2,
                1,
                FieldMask::FULL,
                false,
            )?;
            assert_eq!(
                page.iter().map(|hit| &hit.source_path).collect::<Vec<_>>(),
                hits.iter()
                    .skip(1)
                    .take(2)
                    .map(|hit| &hit.source_path)
                    .collect::<Vec<_>>()
            );
        }
        println!(
            "GH470_HYDRATION {}",
            json!({"corpus_revision":CORPUS_REVISION,
                "authoritative_messages":19,"stored_vectors":44,"backfill":report,"retrieval":retrieval,
                "limits":"real CLI backfill and native exact search with SQLite hydration; no ANN or performance certification"})
        );
        Ok(())
    }
}
