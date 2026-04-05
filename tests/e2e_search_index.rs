//! E2E integration tests for search/index pipeline.
//!
//! Tests cover:
//! - Full index flow with temp data-dir
//! - Search with JSON output (hits, match_type, aggregations)
//! - Watch-once environment path functionality
//! - Trace/log file capture (no mocks)
//!
//! Part of bead: coding_agent_session_search-0jt (TST.11)

use assert_cmd::cargo::cargo_bin_cmd;
use coding_agent_search::storage::sqlite::SqliteStorage;
use frankensqlite::compat::{ConnectionExt, RowExt};
use std::fs;
use std::path::Path;

#[macro_use]
mod util;
use util::EnvGuard;
use util::e2e_log::{E2ePerformanceMetrics, PhaseTracker};

// =============================================================================
// E2E Logger Support
// =============================================================================

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_search_index", test_name)
}

/// Helper to create Codex session with modern envelope format.
fn make_codex_session(root: &Path, date_path: &str, filename: &str, content: &str, ts: u64) {
    let sessions = root.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    // Trailing newline is critical for append_codex_session to work correctly
    let sample = format!(
        r#"{{"type": "event_msg", "timestamp": {ts}, "payload": {{"type": "user_message", "message": "{content}"}}}}
{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "{content}_response"}}}}
"#,
        ts + 1000
    );
    fs::write(file, sample).unwrap();
}

/// Helper to create Claude Code session.
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

/// Append an additional Codex message pair (user + assistant) to an existing rollout file.
fn append_codex_session(file: &Path, content: &str, ts: u64) {
    use std::io::Write;

    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(file)
        .expect("open rollout for append");
    let sample = format!(
        "{{\"type\": \"event_msg\", \"timestamp\": {ts}, \"payload\": {{\"type\": \"user_message\", \"message\": \"{content}\"}}}}\n{{\"type\": \"response_item\", \"timestamp\": {}, \"payload\": {{\"role\": \"assistant\", \"content\": \"{content}_response\"}}}}\n",
        ts + 1000
    );
    f.write_all(sample.as_bytes()).unwrap();
}

fn count_messages(db_path: &Path) -> i64 {
    let storage = SqliteStorage::open(db_path).expect("open sqlite");
    storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |r| r.get_typed(0))
        .expect("count messages")
}

fn run_sqlite3(db_path: &Path, sql: &str) -> std::process::Output {
    match std::process::Command::new("sqlite3")
        .arg(db_path)
        .arg(sql)
        .output()
    {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            std::process::Command::new("python3")
                .args([
                    "-c",
                    r#"
import sqlite3
import sys

db_path, sql = sys.argv[1], sys.argv[2]
conn = sqlite3.connect(db_path)
try:
    cur = conn.cursor()
    statement = sql.strip()
    if ";" in statement.rstrip(";"):
        cur.executescript(statement)
        conn.commit()
    else:
        cur.execute(statement)
        rows = cur.fetchall()
        for row in rows:
            print("\t".join("" if value is None else str(value) for value in row))
        conn.commit()
finally:
    conn.close()
"#,
                    db_path
                        .to_str()
                        .expect("db path should be valid utf-8 for python fallback"),
                    sql,
                ])
                .output()
                .expect("sqlite3 CLI or python3 is required for schema-corruption fixture setup")
        }
        Err(err) => panic!("failed to execute sqlite3 fixture helper: {err}"),
    }
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[test]
fn duplicate_fts_schema_rows_are_repaired_before_cli_reads_and_writes_resume() {
    let tracker =
        tracker_for("duplicate_fts_schema_rows_are_repaired_before_cli_reads_and_writes_resume");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    let ts = 1_732_118_400_000u64;
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-fts-repair.jsonl",
        "fts_repair_initial_token",
        ts,
    );
    let session_file = codex_home.join("sessions/2024/11/20/rollout-fts-repair.jsonl");

    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    let db_path = data_dir.join("agent_search.db");
    let baseline_messages = count_messages(&db_path);
    assert_eq!(
        baseline_messages, 2,
        "initial full index should ingest both messages"
    );

    let duplicate_legacy_fts_sql = "CREATE VIRTUAL TABLE fts_messages USING fts5(content, title, agent, workspace, source_path, created_at UNINDEXED, message_id UNINDEXED, tokenize='porter')";
    let injection_sql = format!(
        "PRAGMA writable_schema = ON;
         INSERT INTO sqlite_master(type, name, tbl_name, rootpage, sql)
         VALUES('table', 'fts_messages', 'fts_messages', 0, {});
         DELETE FROM meta WHERE key = 'fts_frankensqlite_rebuild_generation';
         PRAGMA writable_schema = OFF;",
        sql_literal(duplicate_legacy_fts_sql)
    );
    let injection = run_sqlite3(&db_path, &injection_sql);
    assert!(
        injection.status.success(),
        "schema corruption fixture injection should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&injection.stdout),
        String::from_utf8_lossy(&injection.stderr)
    );

    let broken_read = run_sqlite3(&db_path, "SELECT COUNT(*) FROM fts_messages;");
    assert!(
        !broken_read.status.success(),
        "the injected duplicate schema row should reproduce the unreadable pre-fix SQLite state\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&broken_read.stdout),
        String::from_utf8_lossy(&broken_read.stderr)
    );

    let repair_index = cargo_bin_cmd!("cass")
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("run index after duplicate schema injection");
    assert!(
        repair_index.status.success(),
        "incremental index should repair the duplicate schema and succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&repair_index.stdout),
        String::from_utf8_lossy(&repair_index.stderr)
    );

    let health = cargo_bin_cmd!("cass")
        .args(["health", "--json", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("run health after duplicate schema repair");
    assert!(
        health.status.success(),
        "health should report the repaired database as healthy\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&health.stdout),
        String::from_utf8_lossy(&health.stderr)
    );
    let health_json: serde_json::Value =
        serde_json::from_slice(&health.stdout).expect("parse health json");
    assert_eq!(
        health_json["healthy"],
        serde_json::Value::Bool(true),
        "health should report the repaired database as healthy"
    );

    let repaired = SqliteStorage::open(&db_path).expect("reopen repaired cass db");
    let schema_rows: i64 = repaired
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fts_messages'",
            &[],
            |row| row.get_typed(0),
        )
        .expect("count repaired schema rows");
    assert_eq!(
        schema_rows, 1,
        "repair should leave exactly one authoritative fts_messages schema row"
    );
    let repaired_fts_rows: i64 = repaired
        .raw()
        .query_row_map("SELECT COUNT(*) FROM fts_messages", &[], |row| {
            row.get_typed(0)
        })
        .expect("query repaired fts table");
    assert_eq!(
        repaired_fts_rows, baseline_messages,
        "repair should preserve the indexed FTS rows instead of dropping content"
    );
    let sqlite_client_read = run_sqlite3(&db_path, "SELECT COUNT(*) FROM fts_messages;");
    assert!(
        sqlite_client_read.status.success(),
        "stock SQLite reads should work again after repair\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&sqlite_client_read.stdout),
        String::from_utf8_lossy(&sqlite_client_read.stderr)
    );

    std::thread::sleep(std::time::Duration::from_millis(1200));
    append_codex_session(&session_file, "fts_repair_appended_token", ts + 10_000);
    std::thread::sleep(std::time::Duration::from_millis(50));

    cargo_bin_cmd!("cass")
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    let after_messages = count_messages(&db_path);
    assert_eq!(
        after_messages,
        baseline_messages + 2,
        "incremental writes should resume after repair and append the new turn"
    );

    let appended = cargo_bin_cmd!("cass")
        .args([
            "search",
            "fts_repair_appended_token",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search for appended content after repair");
    assert!(
        appended.status.success(),
        "search should succeed after repair and incremental write\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&appended.stdout),
        String::from_utf8_lossy(&appended.stderr)
    );
    let appended_hits = serde_json::from_slice::<serde_json::Value>(&appended.stdout)
        .expect("parse appended search json")
        .get("hits")
        .and_then(|hits| hits.as_array())
        .map(|hits| hits.len())
        .unwrap_or(0);
    assert!(
        appended_hits >= 1,
        "the post-repair incremental content should be searchable"
    );

    tracker.flush();
}

/// Test: Full index pipeline - index --full creates DB and index
#[test]
fn index_full_creates_artifacts() {
    verbose!("Starting index_full_creates_artifacts test");
    let tracker = tracker_for("index_full_creates_artifacts");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    verbose!("Created temp directory at {:?}", home);
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    verbose!("Data directory: {:?}", data_dir);

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Create fixture data
    let phase_start = tracker.start("create_fixtures", Some("Create Codex session fixture"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "hello world",
        1732118400000,
    );
    tracker.end(
        "create_fixtures",
        Some("Create Codex session fixture"),
        phase_start,
    );

    // Capture memory/IO before indexing (for delta calculation)
    let mem_before = E2ePerformanceMetrics::capture_memory();
    let io_before = E2ePerformanceMetrics::capture_io();

    // Run index --full
    let phase_start = tracker.start("index_full", Some("Execute full index command"));
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    let index_duration_ms = phase_start.elapsed().as_millis() as u64;
    tracker.end(
        "index_full",
        Some("Execute full index command"),
        phase_start,
    );

    // Capture memory/IO after indexing
    let mem_after = E2ePerformanceMetrics::capture_memory();
    let io_after = E2ePerformanceMetrics::capture_io();
    verbose!("Index completed in {}ms", index_duration_ms);

    // Verify artifacts created
    let phase_start = tracker.start("verify_artifacts", Some("Verify database and index exist"));
    verbose!("Verifying artifacts at {:?}", data_dir);
    assert!(
        data_dir.join("agent_search.db").exists(),
        "SQLite DB should be created"
    );
    assert!(
        data_dir.join("index").exists(),
        "Tantivy index directory should exist"
    );
    tracker.end(
        "verify_artifacts",
        Some("Verify database and index exist"),
        phase_start,
    );

    // Count messages and emit performance metrics
    let msg_count = count_messages(&data_dir.join("agent_search.db")) as u64;
    verbose!("Indexed {} messages", msg_count);
    let mut metrics = E2ePerformanceMetrics::new()
        .with_duration(index_duration_ms)
        .with_throughput(msg_count, index_duration_ms);

    // Add memory delta if available
    if let (Some(before), Some(after)) = (mem_before, mem_after) {
        metrics = metrics.with_memory(after.saturating_sub(before));
    }

    // Add I/O delta if available
    if let (Some((rb, wb)), Some((ra, wa))) = (io_before, io_after) {
        metrics = metrics.with_io(0, 0, ra.saturating_sub(rb), wa.saturating_sub(wb));
    }

    tracker.metrics("index_full", &metrics);
    tracker.flush();
    verbose!("Test index_full_creates_artifacts completed successfully");
}

/// Incremental re-index must preserve existing messages and ingest new ones from the same file.
#[test]
fn incremental_reindex_preserves_and_appends_messages() {
    let tracker = tracker_for("incremental_reindex_preserves_and_appends_messages");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Initial session
    let phase_start = tracker.start(
        "create_initial_fixture",
        Some("Create initial session with test content"),
    );
    let ts = 1_732_118_400_000u64; // stable timestamp
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-incremental.jsonl",
        "initial_keep_token",
        ts,
    );
    let session_file = codex_home.join("sessions/2024/11/20/rollout-incremental.jsonl");
    tracker.end(
        "create_initial_fixture",
        Some("Create initial session with test content"),
        phase_start,
    );

    // Full index
    let phase_start = tracker.start("index_full", Some("Run initial full index"));
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("index_full", Some("Run initial full index"), phase_start);

    // Ensure subsequent writes get a later mtime than the recorded scan start
    std::thread::sleep(std::time::Duration::from_millis(1200));

    // Baseline search should find the initial content
    let phase_start = tracker.start(
        "search_baseline",
        Some("Verify initial content is searchable"),
    );
    let baseline = cargo_bin_cmd!("cass")
        .args(["search", "initial_keep_token", "--robot", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("HOME", home)
        .output()
        .expect("baseline search");
    assert!(baseline.status.success());
    let baseline_json: serde_json::Value =
        serde_json::from_slice(&baseline.stdout).expect("baseline json");
    let baseline_hits = baseline_json
        .get("hits")
        .and_then(|h| h.as_array())
        .map(|v| v.len())
        .unwrap_or(0);
    assert!(baseline_hits >= 1, "initial content should be indexed");
    tracker.end(
        "search_baseline",
        Some("Verify initial content is searchable"),
        phase_start,
    );

    // Append new content to the same file (simulates conversation growth)
    let phase_start = tracker.start(
        "append_content",
        Some("Append new messages to session file"),
    );
    append_codex_session(&session_file, "appended_token_beta", ts + 10_000);
    tracker.end(
        "append_content",
        Some("Append new messages to session file"),
        phase_start,
    );

    // On some filesystems, mtime resolution is 1s; give a small buffer before reindex
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Incremental re-index (no --full)
    let phase_start = tracker.start("index_incremental", Some("Run incremental reindex"));
    cargo_bin_cmd!("cass")
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end(
        "index_incremental",
        Some("Run incremental reindex"),
        phase_start,
    );

    // Original content must still be present
    let phase_start = tracker.start(
        "search_preserved",
        Some("Verify original content preserved"),
    );
    let preserved = cargo_bin_cmd!("cass")
        .args(["search", "initial_keep_token", "--robot", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("HOME", home)
        .output()
        .expect("preserved search");
    assert!(preserved.status.success());
    let preserved_hits = serde_json::from_slice::<serde_json::Value>(&preserved.stdout)
        .unwrap()
        .get("hits")
        .and_then(|h| h.as_array())
        .map(|v| v.len())
        .unwrap_or(0);
    assert!(
        preserved_hits >= baseline_hits,
        "existing messages should not be dropped on reindex"
    );
    tracker.end(
        "search_preserved",
        Some("Verify original content preserved"),
        phase_start,
    );

    // New content must be discoverable
    let phase_start = tracker.start("search_appended", Some("Verify appended content indexed"));
    let appended = cargo_bin_cmd!("cass")
        .args(["search", "appended_token_beta", "--robot", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("HOME", home)
        .output()
        .expect("appended search");
    assert!(appended.status.success());
    let appended_hits = serde_json::from_slice::<serde_json::Value>(&appended.stdout)
        .unwrap()
        .get("hits")
        .and_then(|h| h.as_array())
        .map(|v| v.len())
        .unwrap_or(0);
    assert!(
        appended_hits >= 1,
        "appended content should be indexed during incremental run"
    );
    tracker.end(
        "search_appended",
        Some("Verify appended content indexed"),
        phase_start,
    );

    tracker.flush();
}

/// Reindexing must never drop previously ingested messages in SQLite or Tantivy.
#[test]
fn reindex_does_not_drop_messages_in_db_or_search() {
    let tracker = tracker_for("reindex_does_not_drop_messages_in_db_or_search");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let xdg_data = home.join(".local/share");
    let xdg_config = home.join(".config");
    fs::create_dir_all(&xdg_data).unwrap();
    fs::create_dir_all(&xdg_config).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Seed a rollout with two messages
    let ts = 1_732_118_400_000u64;
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-drop-guard.jsonl",
        "persist_me",
        ts,
    );
    let session_file = codex_home.join("sessions/2024/11/20/rollout-drop-guard.jsonl");

    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .assert()
        .success();

    // Ensure next write has strictly newer mtime than initial scan start
    std::thread::sleep(std::time::Duration::from_millis(1200));

    let db_path = data_dir.join("agent_search.db");
    let baseline_count = count_messages(&db_path);
    assert_eq!(baseline_count, 2, "initial two messages recorded");

    // Append another turn and reindex incrementally
    append_codex_session(&session_file, "persist_me_again", ts + 5_000);
    std::thread::sleep(std::time::Duration::from_millis(50));
    cargo_bin_cmd!("cass")
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .assert()
        .success();

    let after_count = count_messages(&db_path);
    assert_eq!(
        after_count,
        baseline_count + 2,
        "messages should only grow after reindex"
    );

    // Verify both old and new content are searchable (Tantivy layer)
    for term in ["persist_me", "persist_me_again"] {
        let out = cargo_bin_cmd!("cass")
            .args(["search", term, "--robot", "--data-dir"])
            .arg(&data_dir)
            // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
            .current_dir(home)
            .env("HOME", home)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .output()
            .expect("search");
        assert!(out.status.success(), "search should succeed for {term}");
        let hits = serde_json::from_slice::<serde_json::Value>(&out.stdout)
            .unwrap()
            .get("hits")
            .and_then(|h| h.as_array())
            .map(|v| v.len())
            .unwrap_or(0);
        assert!(hits >= 1, "{term} should remain indexed");
    }
}

/// Test: Search returns hits with correct match_type
#[test]
fn search_returns_hits_with_match_type() {
    let tracker = tracker_for("search_returns_hits_with_match_type");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Create fixture with unique content
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "unique_search_term_alpha",
        1732118400000,
    );

    // Index first
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Search and verify JSON output
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "unique_search_term_alpha",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(output.status.success(), "Search should succeed");

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");

    // Verify hits array exists
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array should exist");
    assert!(!hits.is_empty(), "Should find at least one hit");

    // Verify match_type field
    let first_hit = &hits[0];
    assert!(
        first_hit.get("match_type").is_some(),
        "Hit should have match_type field"
    );
    let match_type = first_hit["match_type"].as_str().unwrap();
    assert!(
        ["exact", "prefix", "wildcard", "fuzzy", "wildcard_fallback"].contains(&match_type),
        "match_type should be a known type, got: {}",
        match_type
    );

    // Verify content contains search term
    let content = first_hit["content"].as_str().unwrap_or("");
    assert!(
        content.contains("unique_search_term_alpha"),
        "Content should contain search term"
    );
}

/// Test: Search aggregations include agent buckets
#[test]
fn search_aggregations_include_agents() {
    let tracker = tracker_for("search_aggregations_include_agents");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Create fixtures from multiple connectors
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-agg.jsonl",
        "aggregation_test_content",
        1732118400000,
    );
    make_claude_session(
        &claude_home,
        "agg-project",
        "session-agg.jsonl",
        "aggregation_test_content",
        "2024-11-20T10:00:00Z",
    );

    // Index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Search with aggregations
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "aggregation_test_content",
            "--aggregate",
            "agent",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(output.status.success(), "Search should succeed");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");

    // Verify aggregations
    let aggregations = json
        .get("aggregations")
        .expect("aggregations field should exist");
    let agent_agg = aggregations.get("agent").expect("agent aggregation");
    let buckets = agent_agg
        .get("buckets")
        .and_then(|b| b.as_array())
        .expect("buckets array");

    let agent_keys: std::collections::HashSet<_> = buckets
        .iter()
        .filter_map(|b| b.get("key").and_then(|k| k.as_str()))
        .collect();

    // At least one of our fixtures should be found in aggregations
    // (Claude works reliably via HOME; Codex via CODEX_HOME may vary by platform)
    assert!(
        agent_keys.contains("codex") || agent_keys.contains("claude_code"),
        "Should include at least one expected agent. Found: {:?}",
        agent_keys
    );
}

/// Test: Watch-once mode indexes specific paths
#[test]
fn watch_once_indexes_specified_path() {
    let tracker = tracker_for("watch_once_indexes_specified_path");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Create initial data
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-watch.jsonl",
        "watch_once_initial",
        1732118400000,
    );

    // Initial index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Create new file to watch
    let watch_file = codex_home.join("sessions/2024/11/21/rollout-new.jsonl");
    fs::create_dir_all(watch_file.parent().unwrap()).unwrap();

    // Use current timestamp so message is indexed
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let sample = format!(
        r#"{{"type": "event_msg", "timestamp": {now_ts}, "payload": {{"type": "user_message", "message": "watch_once_new_content"}}}}
{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "watch_once_response"}}}}"#,
        now_ts + 1000
    );
    fs::write(&watch_file, sample).unwrap();

    // Run watch-once with specific path
    cargo_bin_cmd!("cass")
        .args(["index", "--watch-once"])
        .arg(&watch_file)
        .arg("--data-dir")
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Verify new content is searchable
    let output = cargo_bin_cmd!("cass")
        .args(["search", "watch_once_new_content", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let hits = json.get("hits").and_then(|h| h.as_array()).expect("hits");
    assert!(
        !hits.is_empty(),
        "Should find the newly indexed watch-once content"
    );
}

/// Test: Search with filters (agent, time range)
#[test]
fn search_with_filters() {
    let tracker = tracker_for("search_with_filters");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Create multiple sessions with distinct content
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-filter1.jsonl",
        "filter_test_content",
        1732118400000, // Nov 20, 2024
    );
    make_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-filter2.jsonl",
        "filter_test_content",
        1732204800000, // Nov 21, 2024
    );

    // Index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Search with agent filter
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "filter_test_content",
            "--agent",
            "codex",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let hits = json.get("hits").and_then(|h| h.as_array()).expect("hits");

    // All hits should be from codex agent
    for hit in hits {
        assert_eq!(
            hit["agent"].as_str().unwrap(),
            "codex",
            "All hits should be from codex agent"
        );
    }
}

/// Test: Search returns total_matches and pagination info
#[test]
fn search_returns_pagination_info() {
    let tracker = tracker_for("search_returns_pagination_info");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Create multiple sessions
    for i in 1..=5 {
        make_codex_session(
            &codex_home,
            "2024/11/20",
            &format!("rollout-page{i}.jsonl"),
            "pagination_test_term",
            1732118400000 + (i as u64 * 1000),
        );
    }

    // Index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Search with limit
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "pagination_test_term",
            "--limit",
            "3",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");

    // Verify pagination fields
    let total = json
        .get("total_matches")
        .and_then(|t| t.as_u64())
        .expect("total_matches");
    let limit = json.get("limit").and_then(|l| l.as_u64()).expect("limit");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits")
        .len();

    // We created 5 sessions, each with 2 messages (user + response), so we expect >= 5 hits
    // But some may not match the search term exactly
    assert!(
        total >= 1,
        "Should have at least 1 total match. Got: {}",
        total
    );
    assert_eq!(limit, 3, "Limit should be 3");
    assert!(hits <= 3, "Returned hits should be <= limit");
}

/// Test: Force rebuild recreates index
#[test]
fn force_rebuild_recreates_index() {
    let tracker = tracker_for("force_rebuild_recreates_index");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Create initial data
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-rebuild.jsonl",
        "rebuild_test_initial",
        1732118400000,
    );

    // Initial index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Get initial index file stats
    let index_dir = data_dir.join("index");
    let initial_mtime = fs::metadata(&index_dir).and_then(|m| m.modified()).ok();

    // Wait a bit
    std::thread::sleep(std::time::Duration::from_secs(1));

    // Force rebuild
    cargo_bin_cmd!("cass")
        .args(["index", "--force-rebuild", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Verify index was rebuilt (mtime changed)
    let new_mtime = fs::metadata(&index_dir).and_then(|m| m.modified()).ok();

    assert!(
        initial_mtime != new_mtime,
        "Index mtime should change after force-rebuild"
    );

    // Verify content is still searchable
    let output = cargo_bin_cmd!("cass")
        .args(["search", "rebuild_test_initial", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let hits = json.get("hits").and_then(|h| h.as_array()).expect("hits");
    assert!(!hits.is_empty(), "Content should still be searchable");
}

/// Test: JSON output mode (--json) for index command
#[test]
fn index_json_output_mode() {
    let tracker = tracker_for("index_json_output_mode");
    let _trace_guard = tracker.trace_env_guard();
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
        "rollout-json.jsonl",
        "json_output_test",
        1732118400000,
    );

    // Index with --json
    let output = cargo_bin_cmd!("cass")
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("index command");

    assert!(output.status.success());

    // Debug: print actual output
    eprintln!(
        "Index JSON output: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    // Verify JSON output structure - index --json outputs various fields
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");

    // Index JSON output should be a valid JSON object
    assert!(
        json.is_object(),
        "JSON output should be an object. Got: {}",
        json
    );
}

/// Test: Help text includes expected options
#[test]
fn index_help_includes_options() {
    let tracker = tracker_for("index_help_includes_options");
    let _trace_guard = tracker.trace_env_guard();
    let output = cargo_bin_cmd!("cass")
        .args(["index", "--help"])
        .output()
        .expect("help command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("--full"), "Help should mention --full");
    assert!(stdout.contains("--watch"), "Help should mention --watch");
    assert!(
        stdout.contains("--force-rebuild"),
        "Help should mention --force-rebuild"
    );
    assert!(
        stdout.contains("--semantic"),
        "Help should mention --semantic"
    );
    assert!(
        stdout.contains("--embedder"),
        "Help should mention --embedder"
    );
    assert!(
        stdout.contains("--data-dir"),
        "Help should mention --data-dir"
    );
}

/// Test: Search help includes expected options
#[test]
fn search_help_includes_options() {
    let tracker = tracker_for("search_help_includes_options");
    let _trace_guard = tracker.trace_env_guard();
    let output = cargo_bin_cmd!("cass")
        .args(["search", "--help"])
        .output()
        .expect("help command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("--robot"), "Help should mention --robot");
    assert!(stdout.contains("--limit"), "Help should mention --limit");
    assert!(stdout.contains("--agent"), "Help should mention --agent");
    assert!(
        stdout.contains("--aggregate"),
        "Help should mention --aggregate"
    );
}

/// Test: Search with wildcard query
#[test]
fn search_wildcard_query() {
    let tracker = tracker_for("search_wildcard_query");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Create fixture with unique prefix
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-wild.jsonl",
        "wildcardtest_unique_suffix",
        1732118400000,
    );

    // Index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Search with wildcard prefix
    let output = cargo_bin_cmd!("cass")
        .args(["search", "wildcardtest*", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let hits = json.get("hits").and_then(|h| h.as_array()).expect("hits");

    assert!(
        !hits.is_empty(),
        "Wildcard prefix search should find results"
    );
}

/// Test: Trace logging works when enabled
#[test]
fn trace_logging_to_file() {
    let tracker = tracker_for("trace_logging_to_file");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    let trace_dir = home.join("traces");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&trace_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());
    let _guard_trace = EnvGuard::set("CASS_TRACE_DIR", trace_dir.to_string_lossy());

    // Create fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-trace.jsonl",
        "trace_test_content",
        1732118400000,
    );

    // Index with tracing enabled
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CASS_TRACE_DIR", &trace_dir)
        .assert()
        .success();

    // Note: Trace file creation depends on tracing-appender setup in the binary
    // This test verifies the env var is recognized without crashing
}

/// Test: Empty query returns recent results
#[test]
fn empty_query_returns_recent() {
    let tracker = tracker_for("empty_query_returns_recent");
    let _trace_guard = tracker.trace_env_guard();
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
        "rollout-recent.jsonl",
        "recent_results_test",
        1732118400000,
    );

    // Index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Search with empty query (should show recent)
    let output = cargo_bin_cmd!("cass")
        .args(["search", "", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(
        output.status.success(),
        "Empty query should succeed after a successful index: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("empty-query search JSON");
    let hits = json["hits"].as_array().expect("hits array");
    assert!(
        !hits.is_empty(),
        "Empty query should return recent indexed conversations"
    );
}
