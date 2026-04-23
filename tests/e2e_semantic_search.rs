//! E2E tests for semantic and hybrid search modes.
//!
//! Tests cover:
//! - Vector index build with hash embedder (always available)
//! - Semantic search mode (vector-only results)
//! - Hybrid search mode (combined lexical + semantic)
//! - HNSW approximate nearest neighbor search
//! - Fallback behavior when semantic unavailable
//!
//! Part of bead: coding_agent_session_search-2vvg

use assert_cmd::cargo::cargo_bin_cmd;
use std::fs;
use std::path::Path;

mod util;
use util::EnvGuard;
use util::e2e_log::PhaseTracker;

// =============================================================================
// E2E Logger Support
// =============================================================================

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_semantic_search", test_name)
}

/// Helper to create Claude Code session fixture.
#[allow(dead_code)]
fn make_claude_session(root: &Path, project: &str, filename: &str, content: &str, ts: &str) {
    let project_dir = root.join(format!("projects/{project}"));
    fs::create_dir_all(&project_dir).unwrap();
    let file = project_dir.join(filename);
    let sample = format!(
        r#"{{"type": "user", "timestamp": "{ts}", "message": {{"role": "user", "content": "{content}"}}}}
{{"type": "assistant", "timestamp": "{ts}", "message": {{"role": "assistant", "content": "{content}_response"}}}}"#
    );
    fs::write(file, sample).unwrap();
}

/// Helper to create Codex session fixture.
fn make_codex_session(root: &Path, date_path: &str, filename: &str, content: &str, ts: u64) {
    let sessions = root.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    let sample = format!(
        r#"{{"type": "event_msg", "timestamp": {ts}, "payload": {{"type": "user_message", "message": "{content}"}}}}
{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "{content}_response"}}}}
"#,
        ts + 1000
    );
    fs::write(file, sample).unwrap();
}

/// Check if any vector index (.fsvi) file exists in data_dir/vector_index/
fn has_vector_index(data_dir: &Path) -> bool {
    let vector_dir = data_dir.join("vector_index");
    if !vector_dir.exists() {
        return false;
    }
    fs::read_dir(&vector_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .any(|e| e.path().extension().is_some_and(|ext| ext == "fsvi"))
        })
        .unwrap_or(false)
}

/// Check if any HNSW index (.chsw) file exists in data_dir/vector_index/
fn has_hnsw_index(data_dir: &Path) -> bool {
    let vector_dir = data_dir.join("vector_index");
    if !vector_dir.exists() {
        return false;
    }
    fs::read_dir(&vector_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .any(|e| e.path().extension().is_some_and(|ext| ext == "chsw"))
        })
        .unwrap_or(false)
}

// =============================================================================
// Semantic Index Build Tests
// =============================================================================

/// Test: Index with --semantic builds vector index alongside text index.
#[test]
fn semantic_index_builds_vector_file() {
    let tracker = tracker_for("semantic_index_builds_vector_file");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    let ps = tracker.start(
        "create_fixtures",
        Some("Create Codex sessions for semantic indexing"),
    );
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "machine learning neural networks deep learning",
        1732118400000,
    );
    make_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-2.jsonl",
        "database optimization query performance tuning",
        1732204800000,
    );
    tracker.end(
        "create_fixtures",
        Some("Create Codex sessions for semantic indexing"),
        ps,
    );

    let ps = tracker.start("run_semantic_index", Some("Run index --full --semantic"));
    let output = cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("index command");
    tracker.end(
        "run_semantic_index",
        Some("Run index --full --semantic"),
        ps,
    );

    assert!(
        output.status.success(),
        "index --semantic failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify vector index file was created
    let ps = tracker.start(
        "verify_vector_index",
        Some("Check vector index file exists"),
    );
    // Hash embedder creates index at vector_index/index-fnv1a-384.fsvi
    let vector_dir = data_dir.join("vector_index");
    assert!(
        vector_dir.exists(),
        "Vector index directory should exist at {:?}",
        vector_dir
    );
    // Find the actual index file (contains embedder ID in name)
    let vector_files: Vec<_> = fs::read_dir(&vector_dir)
        .expect("read vector_index dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "fsvi"))
        .collect();
    assert!(
        !vector_files.is_empty(),
        "Vector index directory should contain .fsvi files"
    );
    let metadata = fs::metadata(vector_files[0].path()).unwrap();
    // A real .fsvi vector index file for a hash-embedder corpus will be
    // at least a few KiB (vector headers + padding + embedding rows for
    // the seeded messages). A 1-byte file would pass `> 0` while
    // clearly indicating the file was created but never populated —
    // exactly the regression a presence-only check fails to catch.
    let file_bytes = metadata.len();
    assert!(
        file_bytes >= 1024,
        "vector index file at {} must be at least 1 KiB to carry a \
         meaningful set of embeddings; got {} bytes (presence-only \
         `> 0` check would have missed this)",
        vector_files[0].path().display(),
        file_bytes
    );
    tracker.end(
        "verify_vector_index",
        Some("Check vector index file exists"),
        ps,
    );

    tracker.complete();
}

/// Test: Index with --semantic --build-hnsw builds HNSW index.
///
/// Note: Building HNSW with hash embedder can fail due to non-normalized vectors.
/// This test is ignored until proper ML model fixtures are available.
#[test]
#[ignore = "hash embedder vectors can cause HNSW build panics; requires real ML model"]
fn semantic_index_builds_hnsw() {
    let tracker = tracker_for("semantic_index_builds_hnsw");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    let ps = tracker.start("create_fixtures", Some("Create sessions for HNSW index"));
    // Create enough sessions for meaningful HNSW testing
    for i in 0..5 {
        make_codex_session(
            &codex_home,
            &format!("2024/11/{:02}", 20 + i),
            &format!("rollout-{i}.jsonl"),
            &format!("session {i} with unique content for hnsw test topic{i}"),
            1732118400000 + (i as u64 * 86400000),
        );
    }
    tracker.end(
        "create_fixtures",
        Some("Create sessions for HNSW index"),
        ps,
    );

    let ps = tracker.start("run_hnsw_index", Some("Run index --semantic --build-hnsw"));
    let output = cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--build-hnsw",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("index command");
    tracker.end(
        "run_hnsw_index",
        Some("Run index --semantic --build-hnsw"),
        ps,
    );

    assert!(
        output.status.success(),
        "index --semantic --build-hnsw failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify HNSW index file was created
    let ps = tracker.start("verify_hnsw_index", Some("Check HNSW index file"));
    assert!(
        has_hnsw_index(&data_dir),
        "HNSW index file (.chsw) should exist in {:?}",
        data_dir.join("vector_index")
    );
    tracker.end("verify_hnsw_index", Some("Check HNSW index file"), ps);

    tracker.complete();
}

// =============================================================================
// Semantic Search Mode Tests
// =============================================================================

/// Test: Search with --mode semantic returns results.
#[test]
fn search_semantic_mode_returns_results() {
    let tracker = tracker_for("search_semantic_mode_returns_results");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Setup: Create and index content
    let ps = tracker.start("setup", Some("Create and index semantic content"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "python machine learning tensorflow neural network training",
        1732118400000,
    );
    make_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-2.jsonl",
        "rust programming systems performance optimization",
        1732204800000,
    );

    cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("setup", Some("Create and index semantic content"), ps);

    // Test semantic search
    let ps = tracker.start("search_semantic", Some("Search with --mode semantic"));
    let output = cargo_bin_cmd!("cass")
        .args(["search", "--robot", "--mode", "semantic", "--data-dir"])
        .arg(&data_dir)
        .arg("deep learning AI")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search command");
    tracker.end("search_semantic", Some("Search with --mode semantic"), ps);

    assert!(
        output.status.success(),
        "semantic search failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("search output should be valid JSON");

    // Semantic search should return results (hash embedder provides basic similarity)
    let ps = tracker.start(
        "verify_results",
        Some("Verify semantic search returns hits"),
    );
    // Bead 7k7pl: pin TYPE on hits (must be a JSON array), not just
    // "field present". A regression that emitted `null` or a scalar
    // would slip past `.is_some()` while breaking every downstream
    // consumer that calls `.as_array()`.
    assert!(
        json.get("hits").and_then(|v| v.as_array()).is_some(),
        "hits must be an array. Got: {}",
        json
    );
    tracker.end(
        "verify_results",
        Some("Verify semantic search returns hits"),
        ps,
    );

    tracker.complete();
}

// =============================================================================
// Hybrid Search Mode Tests
// =============================================================================

/// Test: Search with --mode hybrid combines lexical and semantic.
#[test]
fn search_hybrid_mode_combines_results() {
    let tracker = tracker_for("search_hybrid_mode_combines_results");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Setup
    let ps = tracker.start("setup", Some("Create and index hybrid content"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "hybrid search combines lexical and semantic signals",
        1732118400000,
    );
    make_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-2.jsonl",
        "another session about unrelated database queries",
        1732204800000,
    );

    cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("setup", Some("Create and index hybrid content"), ps);

    // Test hybrid search
    let ps = tracker.start("search_hybrid", Some("Search with --mode hybrid"));
    let output = cargo_bin_cmd!("cass")
        .args(["search", "--robot", "--mode", "hybrid", "--data-dir"])
        .arg(&data_dir)
        .arg("hybrid search lexical semantic")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search command");
    tracker.end("search_hybrid", Some("Search with --mode hybrid"), ps);

    assert!(
        output.status.success(),
        "hybrid search failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("search output should be valid JSON");

    // Hybrid search should return results with exact term matches ranked highly
    let ps = tracker.start("verify_results", Some("Verify hybrid returns hits"));
    let hits = json.get("hits").and_then(|h| h.as_array());
    assert!(hits.is_some(), "Response should have hits array");
    let hits = hits.unwrap();
    assert!(
        !hits.is_empty(),
        "Hybrid search should find matching content"
    );
    tracker.end("verify_results", Some("Verify hybrid returns hits"), ps);

    tracker.complete();
}

// =============================================================================
// HNSW Approximate Search Tests
// =============================================================================

/// Test: Search with --approximate uses HNSW index.
///
/// Note: This test requires a proper ML embedder (e.g., minilm) to work correctly.
/// The hash embedder can produce non-normalized vectors that cause panics in the
/// HNSW distance computation (negative dot products). Run with `--ignored` when
/// the real model fixture is available.
#[test]
#[ignore = "hash embedder vectors can cause HNSW distance panics; requires real ML model"]
fn search_approximate_uses_hnsw() {
    let tracker = tracker_for("search_approximate_uses_hnsw");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Setup: Create sessions and build HNSW index
    let ps = tracker.start("setup", Some("Create sessions and build HNSW"));
    for i in 0..10 {
        make_codex_session(
            &codex_home,
            &format!("2024/11/{:02}", 15 + i),
            &format!("rollout-{i}.jsonl"),
            &format!("approximate nearest neighbor search test session {i}"),
            1731628800000 + (i as u64 * 86400000),
        );
    }

    cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--build-hnsw",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("setup", Some("Create sessions and build HNSW"), ps);

    // Verify HNSW file exists
    assert!(
        has_hnsw_index(&data_dir),
        "HNSW index should exist before approximate search"
    );

    // Test approximate search
    let ps = tracker.start("search_approximate", Some("Search with --approximate"));
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "--robot",
            "--mode",
            "semantic",
            "--approximate",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("nearest neighbor")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search command");
    tracker.end("search_approximate", Some("Search with --approximate"), ps);

    assert!(
        output.status.success(),
        "approximate search failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("search output should be valid JSON");

    let ps = tracker.start("verify_results", Some("Verify approximate search results"));
    // Bead 7k7pl: pin TYPE on hits (must be a JSON array), not just
    // "field present". A regression that emitted `null` or a scalar
    // would slip past `.is_some()` while breaking downstream
    // `.as_array()` consumers.
    assert!(
        json.get("hits").and_then(|v| v.as_array()).is_some(),
        "hits must be an array for approximate search. Got: {}",
        json
    );
    tracker.end(
        "verify_results",
        Some("Verify approximate search results"),
        ps,
    );

    tracker.complete();
}

// =============================================================================
// Fallback Behavior Tests
// =============================================================================

/// Test: Semantic search without vector index reports informative error.
#[test]
fn semantic_without_index_reports_error() {
    let tracker = tracker_for("semantic_without_index_reports_error");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Setup: Index WITHOUT semantic
    let ps = tracker.start("setup", Some("Index without --semantic"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "content for lexical only index",
        1732118400000,
    );

    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("setup", Some("Index without --semantic"), ps);

    // Verify no vector index
    assert!(
        !has_vector_index(&data_dir),
        "Vector index should not exist"
    );

    // Test semantic search fails gracefully
    let ps = tracker.start("search_semantic", Some("Try semantic search without index"));
    let output = cargo_bin_cmd!("cass")
        .args(["search", "--robot", "--mode", "semantic", "--data-dir"])
        .arg(&data_dir)
        .arg("query")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search command");
    tracker.end(
        "search_semantic",
        Some("Try semantic search without index"),
        ps,
    );

    // Should report error (non-zero exit or error in JSON)
    let ps = tracker.start("verify_error", Some("Verify informative error"));
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Either exits with error or returns JSON with error info
    if output.status.success() {
        // If it succeeded, the JSON should indicate the issue
        let json: serde_json::Value = serde_json::from_str(&stdout).expect("output should be JSON");
        // Check for error field or empty results with explanation
        let has_error = json.get("error").is_some();
        let has_hint = stdout.contains("index --semantic") || stdout.contains("lexical");
        assert!(
            has_error
                || has_hint
                || json
                    .get("hits")
                    .is_none_or(|h| { h.as_array().is_none_or(|a| a.is_empty()) }),
            "Should indicate semantic index missing or return empty results"
        );
    }
    // Non-zero exit is acceptable - it means the error was reported
    tracker.end("verify_error", Some("Verify informative error"), ps);

    tracker.complete();
}

/// Test: Approximate search without HNSW index reports informative error.
#[test]
fn approximate_without_hnsw_reports_error() {
    let tracker = tracker_for("approximate_without_hnsw_reports_error");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Setup: Index with semantic but WITHOUT HNSW
    let ps = tracker.start("setup", Some("Index with semantic but no HNSW"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "content for vector index without hnsw",
        1732118400000,
    );

    cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("setup", Some("Index with semantic but no HNSW"), ps);

    // Verify vector exists but HNSW does not
    assert!(has_vector_index(&data_dir), "Vector index should exist");
    assert!(!has_hnsw_index(&data_dir), "HNSW index should not exist");

    // Test approximate search
    let ps = tracker.start("search_approximate", Some("Try approximate without HNSW"));
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "--robot",
            "--mode",
            "semantic",
            "--approximate",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("query")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search command");
    tracker.end(
        "search_approximate",
        Some("Try approximate without HNSW"),
        ps,
    );

    // Should report error about missing HNSW
    let ps = tracker.start("verify_error", Some("Verify HNSW error message"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Should mention HNSW or build-hnsw in error
    let combined = format!("{}{}", stdout, stderr);
    assert!(
        combined.contains("HNSW")
            || combined.contains("hnsw")
            || combined.contains("approximate")
            || combined.contains("build-hnsw")
            || !output.status.success(),
        "Should mention HNSW requirement or fail: stdout={}, stderr={}",
        stdout,
        stderr
    );
    tracker.end("verify_error", Some("Verify HNSW error message"), ps);

    tracker.complete();
}

// =============================================================================
// Performance Metrics Tests
// =============================================================================

/// Test: Semantic search captures timing metrics.
#[test]
fn semantic_search_emits_timing() {
    let tracker = tracker_for("semantic_search_emits_timing");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Setup
    let ps = tracker.start("setup", Some("Create and index content"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "performance timing test content",
        1732118400000,
    );

    cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("setup", Some("Create and index content"), ps);

    // Run search with timing
    let ps = tracker.start("search_timed", Some("Search and capture timing"));
    let start = std::time::Instant::now();
    let output = cargo_bin_cmd!("cass")
        .args(["search", "--robot", "--mode", "semantic", "--data-dir"])
        .arg(&data_dir)
        .arg("timing test")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search command");
    let duration_ms = start.elapsed().as_millis() as u64;
    tracker.end("search_timed", Some("Search and capture timing"), ps);

    assert!(output.status.success());

    // Emit performance metric
    tracker.metrics(
        "semantic_search_latency",
        &util::e2e_log::E2ePerformanceMetrics::new().with_duration(duration_ms),
    );

    tracker.complete();
}
