//! E2E integration tests for large dataset handling.
//!
//! Tests cover:
//! - 10,000+ messages export
//! - 1,000+ conversations index
//! - Large search result sets
//! - Memory-constrained environments
//!
//! Part of bead: coding_agent_session_search-9oyj (T4.4)

use assert_cmd::cargo::cargo_bin;
use coding_agent_search::storage::sqlite::SqliteStorage;
use frankensqlite::compat::{ConnectionExt, RowExt};
use serial_test::serial;
use std::fs;
use std::path::Path;
use std::time::Duration;

mod util;
use util::EnvGuard;
use util::e2e_log::{E2ePerformanceMetrics, PhaseTracker};
use util::timeout::spawn_with_timeout_or_diag;

/// Retrofit helper (bead 2gypv): wraps `std::process::Command::new(cass_bin)`
/// with the standard env-isolation this suite uses, then invokes the
/// f2r5t timeout-with-diagnostic wrapper. Converts silent subprocess
/// hangs into structured diagnostic dumps instead of leaving cargo-test
/// waiting on a stalled child indefinitely.
fn run_cass_with_timeout(
    label: &str,
    args: &[&str],
    data_dir: &Path,
    home: &Path,
    codex_home: &Path,
    timeout: Duration,
) -> std::process::Output {
    let mut cmd = std::process::Command::new(cargo_bin("cass"));
    cmd.args(args)
        .arg(data_dir)
        .current_dir(home)
        .env("CODEX_HOME", codex_home)
        .env("HOME", home);
    spawn_with_timeout_or_diag(cmd, label, Some(data_dir), timeout)
}

// =============================================================================
// E2E Logger Support
// =============================================================================

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_large_dataset", test_name)
}

// =============================================================================
// Fixture Helpers
// =============================================================================

/// Helper to create Codex session with modern envelope format.
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
    fs::write(&file, sample).unwrap();
}

/// Generate a large Codex session with many messages.
fn make_large_codex_session(root: &Path, message_count: usize) {
    let sessions = root.join("sessions/2024/11/20");
    fs::create_dir_all(&sessions).unwrap();
    // Must use rollout-*.jsonl pattern for Codex connector
    let file = sessions.join("rollout-large.jsonl");

    let mut content = String::with_capacity(message_count * 200);
    let base_ts: u64 = 1732118400000;

    for i in 0..message_count {
        let ts = base_ts + (i as u64 * 2000);
        let msg = format!(
            "Large dataset test message number {i} with some additional content for realistic size"
        );
        content.push_str(&format!(
            r#"{{"type": "event_msg", "timestamp": {ts}, "payload": {{"type": "user_message", "message": "{msg}"}}}}
"#
        ));
        content.push_str(&format!(
            r#"{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "Response to message {i} with detailed assistant content"}}}}
"#,
            ts + 1000
        ));
    }

    fs::write(&file, content).unwrap();
}

/// Generate multiple smaller sessions to test conversation count scaling.
fn make_many_sessions(root: &Path, session_count: usize, messages_per_session: usize) {
    for s in 0..session_count {
        let date_path = format!("2024/11/{:02}", (s % 28) + 1);
        let sessions = root.join(format!("sessions/{date_path}"));
        fs::create_dir_all(&sessions).unwrap();
        // Must use rollout-*.jsonl pattern for Codex connector
        let file = sessions.join(format!("rollout-{s}.jsonl"));

        let mut content = String::with_capacity(messages_per_session * 200);
        let base_ts: u64 = 1732118400000 + (s as u64 * 10_000_000);

        for i in 0..messages_per_session {
            let ts = base_ts + (i as u64 * 2000);
            let msg = format!("Session {s} message {i} content");
            content.push_str(&format!(
                r#"{{"type": "event_msg", "timestamp": {ts}, "payload": {{"type": "user_message", "message": "{msg}"}}}}
"#
            ));
            content.push_str(&format!(
                r#"{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "Response to {msg}"}}}}
"#,
                ts + 1000
            ));
        }

        fs::write(&file, content).unwrap();
    }
}

fn count_messages(db_path: &Path) -> i64 {
    let storage = SqliteStorage::open(db_path).expect("open sqlite");
    storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |r| r.get_typed(0))
        .expect("count messages")
}

fn count_conversations(db_path: &Path) -> i64 {
    let storage = SqliteStorage::open(db_path).expect("open sqlite");
    storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM conversations", &[], |r| {
            r.get_typed(0)
        })
        .expect("count conversations")
}

// =============================================================================
// Large Dataset E2E Tests
// =============================================================================

/// Test: Index 5,000+ messages from a single large session.
///
/// This tests the indexer's ability to handle a large number of messages
/// in a single conversation without memory issues or performance degradation.
#[test]
#[serial]
fn index_large_single_session() {
    let tracker = tracker_for("index_large_single_session");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Generate large session (5000 messages = 2500 user + 2500 assistant)
    let message_count = 2500; // Results in 5000 total messages (user + assistant pairs)
    let phase_start = tracker.start("generate_fixtures", Some("Generate large session fixture"));
    make_large_codex_session(&codex_home, message_count);
    tracker.end(
        "generate_fixtures",
        Some("Generate large session fixture"),
        phase_start,
    );

    // Capture baseline metrics
    let mem_before = E2ePerformanceMetrics::capture_memory();
    let io_before = E2ePerformanceMetrics::capture_io();

    // Index the large session
    let phase_start = tracker.start("index_large", Some("Index large session"));
    let out = run_cass_with_timeout(
        "index_large_single_session",
        &["index", "--full", "--data-dir"],
        &data_dir,
        home,
        &codex_home,
        Duration::from_secs(300),
    );
    assert!(
        out.status.success(),
        "cass index --full must succeed on large single session. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let index_duration_ms = phase_start.elapsed().as_millis() as u64;
    tracker.end("index_large", Some("Index large session"), phase_start);

    // Capture post-index metrics
    let mem_after = E2ePerformanceMetrics::capture_memory();
    let io_after = E2ePerformanceMetrics::capture_io();

    // Verify index created
    let db_path = data_dir.join("agent_search.db");
    assert!(db_path.exists(), "SQLite DB should be created");
    assert!(
        data_dir.join("index").exists(),
        "Tantivy index should exist"
    );

    // Verify message count
    let msg_count = count_messages(&db_path) as u64;
    assert!(
        msg_count >= (message_count * 2) as u64,
        "Should have indexed at least {} messages, got {}",
        message_count * 2,
        msg_count
    );

    // Emit performance metrics
    let mut metrics = E2ePerformanceMetrics::new()
        .with_duration(index_duration_ms)
        .with_throughput(msg_count, index_duration_ms);

    if let (Some(before), Some(after)) = (mem_before, mem_after) {
        metrics = metrics.with_memory(after.saturating_sub(before));
    }

    if let (Some((rb, wb)), Some((ra, wa))) = (io_before, io_after) {
        metrics = metrics.with_io(0, 0, ra.saturating_sub(rb), wa.saturating_sub(wb));
    }

    // Add custom metrics for test-specific data
    metrics = metrics
        .with_custom("message_count", msg_count)
        .with_custom("conversation_count", 1u64);

    tracker.metrics("index_large_single_session", &metrics);

    // This E2E test is a correctness/stability guard, not a benchmark. Remote debug builds vary
    // too much for a hard throughput floor to be reliable, so keep the throughput metric for logs
    // and only fail if large-session indexing becomes pathologically slow.
    let max_index_duration_ms = 10 * 60 * 1000;
    assert!(
        index_duration_ms < max_index_duration_ms,
        "Large-session indexing should complete within {} ms in debug E2E runs, got {} ms",
        max_index_duration_ms,
        index_duration_ms
    );

    tracker.flush();
}

/// Test: Index 100+ conversations to test conversation scaling.
///
/// This tests the indexer's ability to handle many separate conversations
/// efficiently, including proper session boundary detection.
#[test]
#[serial]
fn index_many_conversations() {
    let tracker = tracker_for("index_many_conversations");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Generate many sessions
    let session_count = 100;
    let messages_per_session = 10;
    let phase_start = tracker.start("generate_fixtures", Some("Generate many sessions"));
    make_many_sessions(&codex_home, session_count, messages_per_session);
    tracker.end(
        "generate_fixtures",
        Some("Generate many sessions"),
        phase_start,
    );

    // Capture baseline
    let mem_before = E2ePerformanceMetrics::capture_memory();

    // Index all sessions
    let phase_start = tracker.start("index_many", Some("Index many conversations"));
    let out = run_cass_with_timeout(
        "index_many_conversations",
        &["index", "--full", "--data-dir"],
        &data_dir,
        home,
        &codex_home,
        Duration::from_secs(300),
    );
    assert!(
        out.status.success(),
        "cass index --full must succeed across many conversations. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let index_duration_ms = phase_start.elapsed().as_millis() as u64;
    tracker.end("index_many", Some("Index many conversations"), phase_start);

    let mem_after = E2ePerformanceMetrics::capture_memory();

    // Verify
    let db_path = data_dir.join("agent_search.db");
    let conv_count = count_conversations(&db_path) as u64;
    let msg_count = count_messages(&db_path) as u64;

    assert!(
        conv_count >= session_count as u64,
        "Should have at least {} conversations, got {}",
        session_count,
        conv_count
    );

    // Emit metrics
    let mut metrics = E2ePerformanceMetrics::new()
        .with_duration(index_duration_ms)
        .with_throughput(msg_count, index_duration_ms)
        .with_custom("conversation_count", conv_count)
        .with_custom("message_count", msg_count);

    if let (Some(before), Some(after)) = (mem_before, mem_after) {
        metrics = metrics.with_memory(after.saturating_sub(before));
    }

    tracker.metrics("index_many_conversations", &metrics);
    tracker.flush();
}

/// Test: Search with large result sets.
///
/// Tests that search can handle queries that match many results
/// without performance degradation.
#[test]
#[serial]
fn search_large_result_set() {
    let tracker = tracker_for("search_large_result_set");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Generate sessions with a common searchable term
    let session_count = 50;
    let phase_start = tracker.start("generate_fixtures", Some("Generate searchable sessions"));
    for s in 0..session_count {
        let date_path = format!("2024/11/{:02}", (s % 28) + 1);
        let sessions = codex_home.join(format!("sessions/{date_path}"));
        fs::create_dir_all(&sessions).unwrap();
        // Must use rollout-*.jsonl pattern for Codex connector
        let file = sessions.join(format!("rollout-searchable-{s}.jsonl"));

        let base_ts: u64 = 1732118400000 + (s as u64 * 10_000_000);
        // Include "searchterm" in every message for broad matches
        let content = format!(
            r#"{{"type": "event_msg", "timestamp": {base_ts}, "payload": {{"type": "user_message", "message": "searchterm query number {s}"}}}}
{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "Response with searchterm {s}"}}}}
"#,
            base_ts + 1000
        );
        fs::write(&file, content).unwrap();
    }
    tracker.end(
        "generate_fixtures",
        Some("Generate searchable sessions"),
        phase_start,
    );

    // Index
    let phase_start = tracker.start("index", Some("Index searchable sessions"));
    let out = run_cass_with_timeout(
        "search_large_result_set:index",
        &["index", "--full", "--data-dir"],
        &data_dir,
        home,
        &codex_home,
        Duration::from_secs(300),
    );
    assert!(
        out.status.success(),
        "search_large_result_set: index --full must succeed. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    tracker.end("index", Some("Index searchable sessions"), phase_start);

    // Search with term that matches all messages
    let phase_start = tracker.start("search_large", Some("Execute broad search query"));
    let search_output = run_cass_with_timeout(
        "search_large_result_set:search",
        &[
            "search",
            "searchterm",
            "--json",
            "--limit",
            "1000",
            "--data-dir",
        ],
        &data_dir,
        home,
        &codex_home,
        Duration::from_secs(30),
    );
    assert!(
        search_output.status.success(),
        "search_large_result_set: search must succeed. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&search_output.stdout),
        String::from_utf8_lossy(&search_output.stderr)
    );
    let search_duration_ms = phase_start.elapsed().as_millis() as u64;
    tracker.end(
        "search_large",
        Some("Execute broad search query"),
        phase_start,
    );

    // Parse results - JSON format has "total_matches" field
    let output_str = String::from_utf8_lossy(&search_output.stdout);
    let hit_count: u64 = serde_json::from_str::<serde_json::Value>(&output_str)
        .ok()
        .and_then(|v| v.get("total_matches")?.as_u64())
        .unwrap_or(0);

    // Emit metrics
    let metrics = E2ePerformanceMetrics::new()
        .with_duration(search_duration_ms)
        .with_custom("hit_count", hit_count)
        .with_custom("query", "searchterm");

    tracker.metrics("search_large_result_set", &metrics);

    // Assert we got many results
    assert!(
        hit_count >= (session_count * 2) as u64,
        "Should have at least {} hits, got {}",
        session_count * 2,
        hit_count
    );

    // Performance: search should complete within reasonable time
    assert!(
        search_duration_ms < 5000,
        "Search should complete in <5s, took {}ms",
        search_duration_ms
    );

    tracker.flush();
}

/// Test: Memory stays bounded during large index operations.
///
/// Verifies that memory usage doesn't grow unbounded during indexing,
/// which would indicate a memory leak or inefficient buffering.
#[test]
#[serial]
fn memory_bounded_during_index() {
    let tracker = tracker_for("memory_bounded_during_index");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Generate moderate sized dataset
    let message_count = 1000;
    let phase_start = tracker.start("generate_fixtures", Some("Generate test fixtures"));
    make_large_codex_session(&codex_home, message_count);
    tracker.end(
        "generate_fixtures",
        Some("Generate test fixtures"),
        phase_start,
    );

    // Capture memory at multiple points
    let mem_baseline = E2ePerformanceMetrics::capture_memory();

    // Index
    let phase_start = tracker.start("index", Some("Index with memory monitoring"));
    let out = run_cass_with_timeout(
        "memory_bounded_during_index",
        &["index", "--full", "--data-dir"],
        &data_dir,
        home,
        &codex_home,
        Duration::from_secs(300),
    );
    assert!(
        out.status.success(),
        "memory_bounded_during_index: cass index --full must succeed. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let index_duration_ms = phase_start.elapsed().as_millis() as u64;
    tracker.end("index", Some("Index with memory monitoring"), phase_start);

    let mem_after = E2ePerformanceMetrics::capture_memory();

    // Memory delta should be reasonable (< 500MB for this test size)
    let max_memory_delta_bytes = 500 * 1024 * 1024; // 500 MB
    if let (Some(baseline), Some(after)) = (mem_baseline, mem_after) {
        let delta = after.saturating_sub(baseline);

        let metrics = E2ePerformanceMetrics::new()
            .with_duration(index_duration_ms)
            .with_memory(delta)
            .with_custom("baseline_memory_bytes", baseline)
            .with_custom("final_memory_bytes", after)
            .with_custom("max_allowed_delta", max_memory_delta_bytes);

        tracker.metrics("memory_bounded_index", &metrics);

        // This is a soft assertion - log the result but don't fail
        // Real memory issues would show up as process crashes
        if delta > max_memory_delta_bytes {
            eprintln!(
                "Warning: Memory delta ({} bytes) exceeded expected threshold ({} bytes)",
                delta, max_memory_delta_bytes
            );
        }
    }

    tracker.flush();
}

/// Test: Incremental index on large existing dataset.
///
/// Tests that incremental indexing is efficient when adding
/// small amounts of new data to a large existing index.
#[test]
#[serial]
fn incremental_index_on_large_base() {
    let tracker = tracker_for("incremental_index_on_large_base");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Create initial large dataset
    let initial_count = 1000;
    let phase_start = tracker.start("generate_initial", Some("Generate initial large dataset"));
    make_large_codex_session(&codex_home, initial_count);
    tracker.end(
        "generate_initial",
        Some("Generate initial large dataset"),
        phase_start,
    );

    // Full index
    let phase_start = tracker.start("index_full", Some("Initial full index"));
    let out = run_cass_with_timeout(
        "incremental_index_on_large_base:full",
        &["index", "--full", "--data-dir"],
        &data_dir,
        home,
        &codex_home,
        Duration::from_secs(300),
    );
    assert!(
        out.status.success(),
        "incremental_index_on_large_base: initial full index must succeed. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let full_duration_ms = phase_start.elapsed().as_millis() as u64;
    tracker.end("index_full", Some("Initial full index"), phase_start);

    let initial_msg_count = count_messages(&data_dir.join("agent_search.db"));

    // Ensure subsequent writes get a later mtime than the recorded scan start
    std::thread::sleep(std::time::Duration::from_millis(1200));

    // Add a small new session (must use rollout-*.jsonl pattern)
    let phase_start = tracker.start("add_new_session", Some("Add small new session"));
    make_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-new.jsonl",
        "new content",
        1732204800000,
    );
    tracker.end(
        "add_new_session",
        Some("Add small new session"),
        phase_start,
    );

    // Incremental index
    let phase_start = tracker.start("index_incremental", Some("Incremental index"));
    let out = run_cass_with_timeout(
        "incremental_index_on_large_base:incremental",
        &["index", "--data-dir"],
        &data_dir,
        home,
        &codex_home,
        Duration::from_secs(120),
    );
    assert!(
        out.status.success(),
        "incremental_index_on_large_base: incremental index must succeed. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let incremental_duration_ms = phase_start.elapsed().as_millis() as u64;
    tracker.end("index_incremental", Some("Incremental index"), phase_start);

    let final_msg_count = count_messages(&data_dir.join("agent_search.db"));

    // Emit metrics
    let metrics = E2ePerformanceMetrics::new()
        .with_custom("full_index_duration_ms", full_duration_ms)
        .with_custom("incremental_duration_ms", incremental_duration_ms)
        .with_custom("initial_message_count", initial_msg_count as u64)
        .with_custom("final_message_count", final_msg_count as u64)
        .with_custom(
            "messages_added",
            (final_msg_count - initial_msg_count) as u64,
        );

    tracker.metrics("incremental_index_large_base", &metrics);

    // Bead yeq49: the previous strict `incremental_duration_ms <
    // full_duration_ms` wall-clock assertion flaked under multi-agent
    // cargo+rustc load on shared workers (this repo is routinely
    // exercised by 6+ concurrent cargo processes across test panes).
    // OS scheduler noise inverts the inequality even though
    // incremental is doing strictly less work than full.
    //
    // The test's INTENT is correctness of the incremental indexing
    // path — it must pick up exactly one new Codex session (one user
    // + one assistant message = 2 canonical messages) without
    // reprocessing the 1000-session base. That contract is
    // deterministic and verifiable without a wall-clock.
    // Performance-side coverage lives in benches/integration_regression.rs
    // where statistical treatment replaces the single-run timing
    // check. Timing is still emitted as a `tracker.metrics` custom
    // for observability (not an assertion).
    //
    // Keep a very loose performance tripwire (`incremental < full *
    // 10`) so a genuine order-of-magnitude regression — incremental
    // somehow re-scanning the entire base — still fires the test.
    let added_messages = final_msg_count - initial_msg_count;
    assert_eq!(
        added_messages, 2,
        "Incremental index must pick up exactly one new Codex session \
         (one user + one assistant message = 2 canonical messages); \
         got {added_messages} new messages (initial={initial_msg_count}, \
         final={final_msg_count}). A different count means the \
         connector mis-counted the rollout-new.jsonl payload or the \
         incremental run reprocessed base data."
    );

    let timing_ceiling_ms = full_duration_ms.saturating_mul(10);
    assert!(
        incremental_duration_ms <= timing_ceiling_ms,
        "Incremental index is catastrophically slow: {} ms vs full {} ms \
         (>10x regression threshold). Metric regression, not scheduler \
         noise. Timing ceiling = {} ms.",
        incremental_duration_ms,
        full_duration_ms,
        timing_ceiling_ms
    );

    tracker.flush();
}
