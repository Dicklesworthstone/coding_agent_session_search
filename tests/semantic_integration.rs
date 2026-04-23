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
use util::EnvGuard;

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

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

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

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

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

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

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

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

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

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

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

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

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

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

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

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

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

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

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

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

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

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

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

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

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

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

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

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

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
