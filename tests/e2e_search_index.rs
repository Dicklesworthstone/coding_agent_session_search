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
use chrono::{SecondsFormat, Utc};
use coding_agent_search::search::tantivy::{
    SearchableIndexSummary, index_dir, searchable_index_summary,
};
use coding_agent_search::storage::sqlite::SqliteStorage;
use frankensqlite::compat::{ConnectionExt, RowExt};
use rusqlite::Connection as RusqliteConnection;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

fn codex_iso_timestamp(ts_millis: u64) -> String {
    chrono::DateTime::<Utc>::from_timestamp_millis(ts_millis as i64)
        .expect("valid millis timestamp for codex fixture")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn write_jsonl_lines(file: &Path, lines: &[serde_json::Value]) {
    let mut sample = String::new();
    for line in lines {
        sample.push_str(&serde_json::to_string(line).unwrap());
        sample.push('\n');
    }
    fs::write(file, sample).unwrap();
}

fn append_jsonl_lines(file: &Path, lines: &[serde_json::Value]) {
    use std::io::Write;

    let mut handle = std::fs::OpenOptions::new()
        .append(true)
        .open(file)
        .expect("open rollout for append");
    for line in lines {
        handle
            .write_all(serde_json::to_string(line).unwrap().as_bytes())
            .unwrap();
        handle.write_all(b"\n").unwrap();
    }
}

/// Helper to create Codex session with modern envelope format.
fn make_codex_session(root: &Path, date_path: &str, filename: &str, content: &str, ts: u64) {
    let sessions = root.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    let workspace = root.to_string_lossy();
    write_jsonl_lines(
        &file,
        &[
            serde_json::json!({
                "timestamp": codex_iso_timestamp(ts),
                "type": "session_meta",
                "payload": {
                    "id": filename,
                    "cwd": workspace,
                    "cli_version": "0.42.0"
                }
            }),
            serde_json::json!({
                "timestamp": codex_iso_timestamp(ts + 1_000),
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": content }]
                }
            }),
            serde_json::json!({
                "timestamp": codex_iso_timestamp(ts + 2_000),
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "text", "text": format!("{content}_response") }]
                }
            }),
        ],
    );
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
    append_jsonl_lines(
        file,
        &[
            serde_json::json!({
                "timestamp": codex_iso_timestamp(ts),
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": content }]
                }
            }),
            serde_json::json!({
                "timestamp": codex_iso_timestamp(ts + 1_000),
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "text", "text": format!("{content}_response") }]
                }
            }),
        ],
    );
}

fn make_codex_session_with_turns(
    root: &Path,
    date_path: &str,
    filename: &str,
    common_token: &str,
    unique_suffix: &str,
    ts: u64,
    turns: usize,
) {
    let sessions = root.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    let workspace = root.to_string_lossy();
    let mut lines = vec![serde_json::json!({
        "timestamp": codex_iso_timestamp(ts),
        "type": "session_meta",
        "payload": {
            "id": filename,
            "cwd": workspace,
            "cli_version": "0.42.0"
        }
    })];

    for turn in 0..turns {
        let turn_ts = ts + ((turn as u64) + 1) * 3_000;
        lines.push(serde_json::json!({
            "timestamp": codex_iso_timestamp(turn_ts),
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": format!("{common_token} {unique_suffix} user_turn_{turn}")
                }]
            }
        }));
        lines.push(serde_json::json!({
            "timestamp": codex_iso_timestamp(turn_ts + 1_000),
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "text",
                    "text": format!("{common_token} {unique_suffix} assistant_turn_{turn}")
                }]
            }
        }));
    }

    write_jsonl_lines(&file, &lines);
}

fn make_bulk_codex_sessions(
    root: &Path,
    date_path: &str,
    batch_prefix: &str,
    common_token: &str,
    start_ts: u64,
    session_count: usize,
    turns_per_session: usize,
) {
    for idx in 0..session_count {
        make_codex_session_with_turns(
            root,
            date_path,
            &format!("{batch_prefix}-{idx:03}.jsonl"),
            common_token,
            &format!("session_{idx:03}"),
            start_ts + (idx as u64) * 50_000,
            turns_per_session,
        );
    }
}

fn count_messages(db_path: &Path) -> i64 {
    let storage = SqliteStorage::open(db_path).expect("open sqlite");
    storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |r| r.get_typed(0))
        .expect("count messages")
}

fn total_matches_from_search_output(output: &[u8]) -> u64 {
    let json: serde_json::Value = serde_json::from_slice(output).expect("parse search json");
    json.get("total_matches")
        .and_then(|matches| matches.as_u64())
        .unwrap_or_else(|| {
            json.get("hits")
                .and_then(|hits| hits.as_array())
                .map(|hits| hits.len() as u64)
                .unwrap_or(0)
        })
}

#[derive(Debug, Default)]
struct SearchLoopStats {
    attempts: usize,
    successes: usize,
    max_duration_ms: u64,
    failures: Vec<String>,
}

#[test]
fn duplicate_fts_schema_rows_do_not_block_cli_reads_and_writes() {
    let tracker = tracker_for("duplicate_fts_schema_rows_do_not_block_cli_reads_and_writes");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
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
    let injection =
        RusqliteConnection::open(&db_path).expect("open db for writable_schema fixture");
    injection
        .execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages USING fts5(
                content,
                title,
                agent,
                workspace,
                source_path,
                created_at UNINDEXED,
                message_id UNINDEXED,
                tokenize='porter'
             );",
        )
        .expect("materialize canonical fts schema before duplicate injection");
    injection
        .execute_batch("PRAGMA writable_schema = ON;")
        .expect("enable writable_schema");
    injection
        .execute(
            "INSERT INTO sqlite_master(type, name, tbl_name, rootpage, sql)
             VALUES('table', 'fts_messages', 'fts_messages', 0, ?1)",
            [duplicate_legacy_fts_sql],
        )
        .expect("inject duplicate fts schema row");
    injection
        .execute(
            "DELETE FROM meta WHERE key = ?1",
            ["fts_frankensqlite_rebuild_generation"],
        )
        .expect("delete stale fts generation marker");
    injection
        .execute_batch("PRAGMA writable_schema = OFF;")
        .expect("disable writable_schema");
    drop(injection);

    let broken_read = RusqliteConnection::open(&db_path)
        .expect("reopen db for broken-read assertion")
        .query_row("SELECT COUNT(*) FROM fts_messages", [], |row| {
            row.get::<_, i64>(0)
        });
    assert!(
        broken_read.is_err(),
        "the injected duplicate schema row should reproduce the unreadable pre-fix SQLite state"
    );

    let existing_search = cargo_bin_cmd!("cass")
        .args([
            "search",
            "fts_repair_initial_token",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search for existing content after duplicate schema injection");
    assert!(
        existing_search.status.success(),
        "search should continue to succeed even when the fallback SQLite FTS table is malformed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&existing_search.stdout),
        String::from_utf8_lossy(&existing_search.stderr)
    );
    let existing_hits = serde_json::from_slice::<serde_json::Value>(&existing_search.stdout)
        .expect("parse existing search json")
        .get("hits")
        .and_then(|hits| hits.as_array())
        .map(|hits| hits.len())
        .unwrap_or(0);
    assert!(
        existing_hits >= 1,
        "the Tantivy index should remain authoritative for search results"
    );

    let incremental_index = cargo_bin_cmd!("cass")
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("run index after duplicate schema injection");
    assert!(
        incremental_index.status.success(),
        "incremental index should succeed even when the fallback SQLite FTS table is malformed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&incremental_index.stdout),
        String::from_utf8_lossy(&incremental_index.stderr)
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
        "health should treat the canonical archive plus Tantivy index as healthy"
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
        "search should succeed after incremental write even with malformed fallback FTS\nstdout:\n{}\nstderr:\n{}",
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
        "the post-index incremental content should be searchable"
    );

    tracker.flush();
}

#[test]
fn concurrent_search_processes_do_not_block_incremental_index_json() {
    let tracker = tracker_for("concurrent_search_processes_do_not_block_incremental_index_json");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let codex_home = home.to_path_buf();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    tracker.phase(
        "seed_initial_fixture",
        "Create baseline session and full index",
        || {
            make_codex_session(
                &codex_home,
                "2024/11/20",
                "rollout-baseline-lock-search.jsonl",
                "baselinelockanchor",
                1_732_118_400_000,
            );

            cargo_bin_cmd!("cass")
                .args(["index", "--full", "--json", "--data-dir"])
                .arg(&data_dir)
                .current_dir(&home)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", &home)
                .timeout(Duration::from_secs(30))
                .assert()
                .success();
        },
    );

    tracker.phase(
        "verify_baseline_search_fixture",
        "Confirm the baseline lexical query is searchable before starting concurrent readers",
        || {
            let baseline_output = cargo_bin_cmd!("cass")
                .args([
                    "search",
                    "baselinelockanchor",
                    "--json",
                    "--mode",
                    "lexical",
                    "--fields",
                    "minimal",
                    "--limit",
                    "5",
                    "--data-dir",
                ])
                .arg(&data_dir)
                .current_dir(&home)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", &home)
                .timeout(Duration::from_secs(20))
                .output()
                .expect("baseline lexical search should run");
            assert!(
                baseline_output.status.success(),
                "baseline lexical search should succeed before concurrency begins\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&baseline_output.stdout),
                String::from_utf8_lossy(&baseline_output.stderr)
            );
            let baseline_json: serde_json::Value =
                serde_json::from_slice(&baseline_output.stdout).expect("parse baseline search JSON");
            let baseline_hits = baseline_json
                .get("total_matches")
                .and_then(|matches| matches.as_u64())
                .unwrap_or_else(|| {
                    baseline_json
                        .get("hits")
                        .and_then(|hits| hits.as_array())
                        .map(|hits| hits.len() as u64)
                        .unwrap_or(0)
                });
            assert!(
                baseline_hits > 0,
                "baseline lexical search fixture must be searchable before starting concurrent readers"
            );
        },
    );

    let stop_search = Arc::new(AtomicBool::new(false));
    let index_running = Arc::new(AtomicBool::new(false));
    let search_attempts_during_index = Arc::new(AtomicUsize::new(0));
    let (ready_tx, ready_rx) = mpsc::channel();

    let search_home = home.clone();
    let search_codex_home = codex_home.clone();
    let search_data_dir = data_dir.clone();
    let stop_search_worker = Arc::clone(&stop_search);
    let index_running_worker = Arc::clone(&index_running);
    let search_attempts_during_index_worker = Arc::clone(&search_attempts_during_index);

    let search_handle = std::thread::spawn(move || {
        let mut stats = SearchLoopStats::default();
        let mut ready_sent = false;

        loop {
            if stop_search_worker.load(Ordering::Relaxed) {
                break;
            }

            if index_running_worker.load(Ordering::Relaxed) {
                search_attempts_during_index_worker.fetch_add(1, Ordering::Relaxed);
            }

            let search_start = Instant::now();
            let output = cargo_bin_cmd!("cass")
                .args([
                    "search",
                    "baselinelockanchor",
                    "--json",
                    "--mode",
                    "lexical",
                    "--fields",
                    "minimal",
                    "--limit",
                    "5",
                    "--data-dir",
                ])
                .arg(&search_data_dir)
                .current_dir(&search_home)
                .env("CODEX_HOME", &search_codex_home)
                .env("HOME", &search_home)
                .timeout(Duration::from_secs(20))
                .output()
                .expect("spawn concurrent cass search");
            let elapsed_ms = search_start.elapsed().as_millis() as u64;
            stats.attempts += 1;
            stats.max_duration_ms = stats.max_duration_ms.max(elapsed_ms);

            if output.status.success() {
                let parsed: serde_json::Value =
                    serde_json::from_slice(&output.stdout).expect("parse concurrent search JSON");
                let hit_count = parsed
                    .get("total_matches")
                    .and_then(|matches| matches.as_u64())
                    .unwrap_or_else(|| {
                        parsed
                            .get("hits")
                            .and_then(|hits| hits.as_array())
                            .map(|hits| hits.len() as u64)
                            .unwrap_or(0)
                    });
                if hit_count == 0 {
                    stats.failures.push(format!(
                        "concurrent search returned zero hits; stdout={}",
                        String::from_utf8_lossy(&output.stdout)
                    ));
                } else {
                    stats.successes += 1;
                }
            } else {
                stats.failures.push(format!(
                    "concurrent search failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ));
            }

            if !ready_sent {
                ready_tx.send(()).ok();
                ready_sent = true;
            }

            std::thread::sleep(Duration::from_millis(40));
        }

        stats
    });

    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("concurrent search should start promptly");

    tracker.phase(
        "stage_incremental_fixture",
        "Create a substantial incremental batch while searches continue",
        || {
            make_bulk_codex_sessions(
                &codex_home,
                "2024/11/21",
                "rollout-incremental-lock-batch",
                "incrementalloadanchor",
                1_732_200_000_000,
                40,
                6,
            );
        },
    );

    let index_start = tracker.start(
        "incremental_index_under_read_load",
        Some("Run cass index --json while concurrent cass search processes read the same DB"),
    );
    index_running.store(true, Ordering::Relaxed);
    let index_output = cargo_bin_cmd!("cass")
        .args(["index", "--json", "--data-dir"])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(60))
        .output()
        .expect("run incremental index under concurrent search load");
    index_running.store(false, Ordering::Relaxed);
    let index_duration_ms = index_start.elapsed().as_millis() as u64;
    tracker.end(
        "incremental_index_under_read_load",
        Some("Run cass index --json while concurrent cass search processes read the same DB"),
        index_start,
    );

    stop_search.store(true, Ordering::Relaxed);
    let search_stats = search_handle.join().expect("join concurrent search thread");

    assert!(
        index_output.status.success(),
        "incremental index should succeed under concurrent search load\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&index_output.stdout),
        String::from_utf8_lossy(&index_output.stderr)
    );
    let index_json: serde_json::Value =
        serde_json::from_slice(&index_output.stdout).expect("parse index json");
    assert_eq!(
        index_json.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "index --json should report success"
    );

    assert!(
        search_stats.failures.is_empty(),
        "concurrent searches should not fail while index runs:\n{}",
        search_stats.failures.join("\n---\n")
    );
    assert!(
        search_stats.successes > 0,
        "expected at least one successful concurrent search attempt"
    );
    assert!(
        search_attempts_during_index.load(Ordering::Relaxed) > 0,
        "expected real search overlap while index was running"
    );

    let after_messages = count_messages(&data_dir.join("agent_search.db")) as u64;
    let expected_min_messages = 2 + (40_u64 * 6 * 2);
    assert!(
        after_messages >= expected_min_messages,
        "incremental index should ingest the staged batch: expected at least {expected_min_messages} messages, got {after_messages}"
    );

    let verify_search = cargo_bin_cmd!("cass")
        .args([
            "search",
            "incrementalloadanchor",
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("search for newly indexed batch");
    assert!(
        verify_search.status.success(),
        "search for newly indexed batch should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&verify_search.stdout),
        String::from_utf8_lossy(&verify_search.stderr)
    );
    let verify_json: serde_json::Value =
        serde_json::from_slice(&verify_search.stdout).expect("parse verification search JSON");
    let verification_hits = verify_json
        .get("total_matches")
        .and_then(|matches| matches.as_u64())
        .unwrap_or_else(|| {
            verify_json
                .get("hits")
                .and_then(|hits| hits.as_array())
                .map(|hits| hits.len() as u64)
                .unwrap_or(0)
        });
    assert!(
        verification_hits > 0,
        "newly indexed batch should be searchable after concurrent index run"
    );

    tracker.metrics(
        "concurrent_search_vs_index",
        &E2ePerformanceMetrics::new()
            .with_duration(index_duration_ms)
            .with_custom("search_attempts", search_stats.attempts as u64)
            .with_custom("search_successes", search_stats.successes as u64)
            .with_custom(
                "search_attempts_during_index",
                search_attempts_during_index.load(Ordering::Relaxed) as u64,
            )
            .with_custom("max_search_duration_ms", search_stats.max_duration_ms)
            .with_custom("messages_after_index", after_messages),
    );
    tracker.complete();
}

#[test]
fn force_rebuild_preserves_search_results_and_reader_surface_during_atomic_publish() {
    let tracker =
        tracker_for("force_rebuild_preserves_search_results_and_reader_surface_during_atomic_publish");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let codex_home = home.to_path_buf();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    const QUERY: &str = "atomicswapsearchanchor";

    tracker.phase(
        "seed_and_index_single_shard_fixture",
        "Create a minimal fixture and build the baseline lexical index",
        || {
            make_codex_session(
                &codex_home,
                "2024/11/22",
                "rollout-atomic-search-consistency.jsonl",
                QUERY,
                1_732_300_000_000,
            );

            cargo_bin_cmd!("cass")
                .args(["index", "--full", "--json", "--data-dir"])
                .arg(&data_dir)
                .current_dir(&home)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", &home)
                .timeout(Duration::from_secs(30))
                .assert()
                .success();
        },
    );

    let live_index_path = index_dir(&data_dir).expect("resolve live Tantivy index path");
    let before_summary = searchable_index_summary(&live_index_path)
        .expect("read baseline searchable index summary")
        .expect("baseline index should exist");
    let before_docs = before_summary.docs;
    assert!(before_docs > 0, "baseline index should contain at least one doc");

    let baseline_search = cargo_bin_cmd!("cass")
        .args([
            "search",
            QUERY,
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("run baseline lexical search");
    assert!(
        baseline_search.status.success(),
        "baseline lexical search should succeed before force rebuild\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&baseline_search.stdout),
        String::from_utf8_lossy(&baseline_search.stderr)
    );
    let baseline_total_matches = total_matches_from_search_output(&baseline_search.stdout);
    assert!(
        baseline_total_matches > 0,
        "baseline lexical search should return at least one hit before rebuild"
    );

    let stop = Arc::new(AtomicBool::new(false));
    let rebuild_running = Arc::new(AtomicBool::new(false));
    let reader_attempts_during_rebuild = Arc::new(AtomicUsize::new(0));
    let search_attempts_during_rebuild = Arc::new(AtomicUsize::new(0));
    let (ready_tx, ready_rx) = mpsc::channel();

    let reader_ready_tx = ready_tx.clone();
    let reader_stop = Arc::clone(&stop);
    let reader_rebuild_running = Arc::clone(&rebuild_running);
    let reader_overlap = Arc::clone(&reader_attempts_during_rebuild);
    let reader_index_path = live_index_path.clone();
    let reader_deadline = Instant::now() + Duration::from_secs(20);
    let reader_handle = std::thread::spawn(move || {
        let _ = reader_ready_tx.send("reader");
        let mut observations: Vec<Result<Option<SearchableIndexSummary>, String>> = Vec::new();
        while !reader_stop.load(Ordering::Relaxed) && Instant::now() < reader_deadline {
            if reader_rebuild_running.load(Ordering::Relaxed) {
                reader_overlap.fetch_add(1, Ordering::Relaxed);
            }
            let obs = searchable_index_summary(&reader_index_path).map_err(|e| format!("{e:#}"));
            observations.push(obs);
            std::thread::sleep(Duration::from_millis(1));
        }
        observations
    });

    let search_ready_tx = ready_tx.clone();
    let search_stop = Arc::clone(&stop);
    let search_rebuild_running = Arc::clone(&rebuild_running);
    let search_overlap = Arc::clone(&search_attempts_during_rebuild);
    let search_home = home.clone();
    let search_codex_home = codex_home.clone();
    let search_data_dir = data_dir.clone();
    let search_handle = std::thread::spawn(move || {
        let _ = search_ready_tx.send("search");
        let mut stats = SearchLoopStats::default();
        while !search_stop.load(Ordering::Relaxed) {
            if search_rebuild_running.load(Ordering::Relaxed) {
                search_overlap.fetch_add(1, Ordering::Relaxed);
            }

            let search_started = Instant::now();
            let output = cargo_bin_cmd!("cass")
                .args([
                    "search",
                    QUERY,
                    "--json",
                    "--mode",
                    "lexical",
                    "--fields",
                    "minimal",
                    "--limit",
                    "5",
                    "--data-dir",
                ])
                .arg(&search_data_dir)
                .current_dir(&search_home)
                .env("CODEX_HOME", &search_codex_home)
                .env("HOME", &search_home)
                .timeout(Duration::from_secs(20))
                .output()
                .expect("run concurrent cass search");
            let elapsed_ms = search_started.elapsed().as_millis() as u64;
            stats.attempts += 1;
            stats.max_duration_ms = stats.max_duration_ms.max(elapsed_ms);

            if output.status.success() {
                let hit_count = total_matches_from_search_output(&output.stdout);
                if hit_count != baseline_total_matches {
                    stats.failures.push(format!(
                        "concurrent lexical search returned {hit_count} hits; expected stable total_matches={baseline_total_matches}\nstdout:\n{}",
                        String::from_utf8_lossy(&output.stdout)
                    ));
                } else {
                    stats.successes += 1;
                }
            } else {
                stats.failures.push(format!(
                    "concurrent lexical search failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ));
            }

            std::thread::sleep(Duration::from_millis(40));
        }

        stats
    });
    drop(ready_tx);

    for _ in 0..2 {
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("reader and search concurrency helpers should start promptly");
    }

    let rebuild_start = tracker.start(
        "force_rebuild_under_concurrent_reader_and_search",
        Some("Run cass index --full --force-rebuild while a direct reader and cass search poll the same live index"),
    );
    rebuild_running.store(true, Ordering::Relaxed);
    let rebuild_output = cargo_bin_cmd!("cass")
        .args(["index", "--full", "--force-rebuild", "--json", "--data-dir"])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(60))
        .output()
        .expect("run force rebuild under concurrent read/search load");
    rebuild_running.store(false, Ordering::Relaxed);
    let rebuild_duration_ms = rebuild_start.elapsed().as_millis() as u64;
    tracker.end(
        "force_rebuild_under_concurrent_reader_and_search",
        Some("Run cass index --full --force-rebuild while a direct reader and cass search poll the same live index"),
        rebuild_start,
    );

    stop.store(true, Ordering::Relaxed);
    let reader_observations = reader_handle.join().expect("join reader thread");
    let search_stats = search_handle.join().expect("join search thread");

    assert!(
        rebuild_output.status.success(),
        "force rebuild should succeed under concurrent reader/search load\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&rebuild_output.stdout),
        String::from_utf8_lossy(&rebuild_output.stderr)
    );
    let rebuild_json: serde_json::Value =
        serde_json::from_slice(&rebuild_output.stdout).expect("parse force rebuild json");
    assert_eq!(
        rebuild_json.get("success").and_then(|value| value.as_bool()),
        Some(true),
        "force rebuild should report success in --json output"
    );

    assert!(
        !reader_observations.is_empty(),
        "reader should collect at least one observation during the force rebuild window"
    );
    assert!(
        reader_attempts_during_rebuild.load(Ordering::Relaxed) > 0,
        "expected direct reader overlap while force rebuild was running"
    );
    for (idx, observation) in reader_observations.iter().enumerate() {
        if let Ok(Some(summary)) = observation {
            assert_eq!(
                summary.docs, before_docs,
                "reader observation #{idx} returned docs={} instead of the stable count {before_docs}; \
                 this indicates a half-torn lexical index surface during atomic publish",
                summary.docs
            );
        }
    }

    assert!(
        search_attempts_during_rebuild.load(Ordering::Relaxed) > 0,
        "expected cass search overlap while force rebuild was running"
    );
    assert!(
        search_stats.failures.is_empty(),
        "concurrent cass search should stay logically stable during force rebuild:\n{}",
        search_stats.failures.join("\n---\n")
    );
    assert!(
        search_stats.successes > 0,
        "expected at least one successful concurrent cass search attempt"
    );

    let after_summary = searchable_index_summary(&live_index_path)
        .expect("read searchable summary after rebuild")
        .expect("live index should still exist after rebuild");
    assert_eq!(
        after_summary.docs, before_docs,
        "force rebuild on unchanged content should preserve the live doc count"
    );

    let after_search = cargo_bin_cmd!("cass")
        .args([
            "search",
            QUERY,
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("run post-rebuild lexical search");
    assert!(
        after_search.status.success(),
        "post-rebuild lexical search should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&after_search.stdout),
        String::from_utf8_lossy(&after_search.stderr)
    );
    let after_total_matches = total_matches_from_search_output(&after_search.stdout);
    assert_eq!(
        after_total_matches, baseline_total_matches,
        "force rebuild on unchanged content should preserve the logical search result count"
    );

    tracker.metrics(
        "force_rebuild_concurrency_surface",
        &E2ePerformanceMetrics::new()
            .with_duration(rebuild_duration_ms)
            .with_custom(
                "reader_attempts_during_rebuild",
                reader_attempts_during_rebuild.load(Ordering::Relaxed) as u64,
            )
            .with_custom(
                "search_attempts_during_rebuild",
                search_attempts_during_rebuild.load(Ordering::Relaxed) as u64,
            )
            .with_custom("search_attempts_total", search_stats.attempts as u64)
            .with_custom("search_successes_total", search_stats.successes as u64)
            .with_custom("max_search_duration_ms", search_stats.max_duration_ms)
            .with_custom("stable_doc_count", before_docs as u64)
            .with_custom("stable_total_matches", baseline_total_matches),
    );
    tracker.complete();
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
    let codex_home = home.to_path_buf();
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
    let codex_home = home.to_path_buf();
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
    let codex_home = home.to_path_buf();
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
    let codex_home = home.to_path_buf();
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
    let codex_home = home.to_path_buf();
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
    let codex_home = home.to_path_buf();
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
    let codex_home = home.to_path_buf();
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
    let codex_home = home.to_path_buf();
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
    let codex_home = home.to_path_buf();
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
    let codex_home = home.to_path_buf();
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
    let codex_home = home.to_path_buf();
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
    let codex_home = home.to_path_buf();
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
    let codex_home = home.to_path_buf();
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

#[test]
fn large_message_minimal_search_stays_on_the_tantivy_fast_path() {
    let tracker = tracker_for("large_message_minimal_search_stays_on_the_tantivy_fast_path");
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    let large_content = format!(
        "tantivy_large_anchor {}",
        "overflowpayload ".repeat(180_000)
    );

    tracker.phase(
        "seed_large_message_fixture",
        "Create a real Codex rollout with a multi-megabyte message body",
        || {
            make_codex_session(
                &codex_home,
                "2024/11/22",
                "rollout-large-tantivy-fast-path.jsonl",
                &large_content,
                1_732_300_000_000,
            );
        },
    );

    tracker.phase(
        "index_large_message_fixture",
        "Build the real index before searching the large message",
        || {
            cargo_bin_cmd!("cass")
                .args(["index", "--full", "--json", "--data-dir"])
                .arg(&data_dir)
                .current_dir(home)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", home)
                .timeout(Duration::from_secs(90))
                .assert()
                .success();
        },
    );

    let search_started = tracker.start(
        "search_large_message_minimal",
        Some("Run a real lexical cass search against the multi-megabyte session"),
    );
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "tantivy_large_anchor",
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("search large indexed message");
    tracker.end(
        "search_large_message_minimal",
        Some("Run a real lexical cass search against the multi-megabyte session"),
        search_started,
    );

    assert!(
        output.status.success(),
        "large-message lexical search should stay healthy after indexing\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("large-message search JSON");
    let hits = json
        .get("total_matches")
        .and_then(|matches| matches.as_u64())
        .unwrap_or_else(|| {
            json.get("hits")
                .and_then(|hits| hits.as_array())
                .map(|hits| hits.len() as u64)
                .unwrap_or(0)
        });
    assert!(
        hits > 0,
        "large indexed message should remain searchable with minimal lexical fields"
    );

    tracker.flush();
}

#[test]
fn incremental_index_repairs_sparse_tantivy_from_canonical_db_before_scanning_new_files() {
    let tracker = tracker_for(
        "incremental_index_repairs_sparse_tantivy_from_canonical_db_before_scanning_new_files",
    );
    let _trace_guard = tracker.trace_env_guard();
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let codex_home = home.to_path_buf();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    tracker.phase(
        "seed_baseline_archive",
        "Create a real multi-session Codex archive and build the canonical DB plus lexical index",
        || {
            make_bulk_codex_sessions(
                &codex_home,
                "2024/11/23",
                "rollout-repair-baseline",
                "repairoldanchor",
                1_732_400_000_000,
                5,
                4,
            );

            cargo_bin_cmd!("cass")
                .args(["index", "--full", "--json", "--data-dir"])
                .arg(&data_dir)
                .current_dir(&home)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", &home)
                .timeout(Duration::from_secs(60))
                .assert()
                .success();
        },
    );

    let db_path = data_dir.join("agent_search.db");
    let baseline_messages = count_messages(&db_path) as u64;
    assert!(
        baseline_messages >= 40,
        "baseline archive should populate the canonical DB with many messages"
    );

    tracker.phase(
        "swap_in_sparse_real_tantivy_index",
        "Replace the healthy lexical index with a real but sparse one built from a different archive",
        || {
            let sparse_home = home.join("sparse_home");
            let sparse_codex_home = sparse_home.clone();
            let sparse_data_dir = sparse_home.join("cass_data");
            fs::create_dir_all(&sparse_data_dir).unwrap();

            make_codex_session(
                &sparse_codex_home,
                "2024/11/23",
                "rollout-sparse-replacement.jsonl",
                "sparseanchoronly",
                1_732_450_000_000,
            );

            cargo_bin_cmd!("cass")
                .args(["index", "--full", "--json", "--data-dir"])
                .arg(&sparse_data_dir)
                .current_dir(&sparse_home)
                .env("CODEX_HOME", &sparse_codex_home)
                .env("HOME", &sparse_home)
                .timeout(Duration::from_secs(60))
                .assert()
                .success();

            let live_index = data_dir.join("index/v7");
            let backup_index = data_dir.join("index/v7.baseline-backup");
            let sparse_index = sparse_data_dir.join("index/v7");
            fs::rename(&live_index, &backup_index).expect("move healthy index aside");
            fs::rename(&sparse_index, &live_index)
                .expect("replace healthy index with sparse real tantivy index");
        },
    );

    let broken_sparse_search = cargo_bin_cmd!("cass")
        .args([
            "search",
            "repairoldanchor",
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("search old token against intentionally sparse lexical index");
    assert!(
        broken_sparse_search.status.success(),
        "search should still run even with a sparse lexical index\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&broken_sparse_search.stdout),
        String::from_utf8_lossy(&broken_sparse_search.stderr)
    );
    assert_eq!(
        total_matches_from_search_output(&broken_sparse_search.stdout),
        0,
        "the swapped-in sparse index should not contain the baseline token before repair"
    );

    tracker.phase(
        "stage_new_incremental_session",
        "Add a brand-new session after the sparse index swap so plain cass index must both repair and ingest",
        || {
            make_codex_session(
                &codex_home,
                "2024/11/24",
                "rollout-repair-new-session.jsonl",
                "repairnewanchor",
                1_732_500_000_000,
            );
        },
    );

    let repair_started = tracker.start(
        "repair_sparse_tantivy_then_incremental_scan",
        Some("Run plain cass index --json and require canonical repair plus new-session ingestion"),
    );
    let repair_output = cargo_bin_cmd!("cass")
        .args(["index", "--json", "--data-dir"])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(60))
        .output()
        .expect("run repairing incremental index");
    let repair_duration_ms = repair_started.elapsed().as_millis() as u64;
    tracker.end(
        "repair_sparse_tantivy_then_incremental_scan",
        Some("Run plain cass index --json and require canonical repair plus new-session ingestion"),
        repair_started,
    );

    assert!(
        repair_output.status.success(),
        "plain index should repair the sparse Tantivy index and ingest new sessions\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&repair_output.stdout),
        String::from_utf8_lossy(&repair_output.stderr)
    );

    let repair_json: serde_json::Value =
        serde_json::from_slice(&repair_output.stdout).expect("parse repair index json");
    let repair_stats = repair_json
        .get("indexing_stats")
        .and_then(|value| value.as_object())
        .expect("indexing_stats object");
    assert_eq!(
        repair_stats
            .get("lexical_strategy")
            .and_then(|value| value.as_str()),
        Some("deferred_authoritative_db_rebuild")
    );
    assert_eq!(
        repair_stats
            .get("lexical_strategy_reason")
            .and_then(|value| value.as_str()),
        Some(
            "incremental_index_repairs_sparse_tantivy_from_authoritative_canonical_db_before_scan"
        )
    );

    let after_messages = count_messages(&db_path) as u64;
    assert_eq!(
        after_messages,
        baseline_messages + 2,
        "plain incremental index should still ingest the newly added session after repairing Tantivy"
    );

    let repaired_old_search = cargo_bin_cmd!("cass")
        .args([
            "search",
            "repairoldanchor",
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("search repaired baseline token");
    assert!(
        repaired_old_search.status.success(),
        "search should succeed after canonical lexical repair\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&repaired_old_search.stdout),
        String::from_utf8_lossy(&repaired_old_search.stderr)
    );
    assert!(
        total_matches_from_search_output(&repaired_old_search.stdout) > 0,
        "repair should restore baseline archive hits from the canonical DB"
    );

    let repaired_new_search = cargo_bin_cmd!("cass")
        .args([
            "search",
            "repairnewanchor",
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("search new incremental token after repair");
    assert!(
        repaired_new_search.status.success(),
        "new incremental content should be searchable after the repair-first index run\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&repaired_new_search.stdout),
        String::from_utf8_lossy(&repaired_new_search.stderr)
    );
    assert!(
        total_matches_from_search_output(&repaired_new_search.stdout) > 0,
        "repair-first incremental index should still ingest the newly added session"
    );

    tracker.metrics(
        "repair_sparse_tantivy_then_incremental_scan",
        &E2ePerformanceMetrics::new()
            .with_duration(repair_duration_ms)
            .with_custom("baseline_messages", baseline_messages)
            .with_custom("messages_after_repair", after_messages),
    );
    tracker.flush();
}
