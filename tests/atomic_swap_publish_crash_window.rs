//! Bead coding_agent_session_search-ghw60 (child of ibuuh.10):
//! concurrent-reader crash-window regression for the atomic-swap
//! lexical publish contract from commits 109560e5
//! (renameat2(RENAME_EXCHANGE) / rename-fallback) and a699f55b (stage
//! generation artifacts before swap).
//!
//! Invariant under test: while `cass index --full` is swapping a
//! newly staged lexical index into the live path, an external reader
//! that opens the live path in a tight loop must observe EXACTLY one
//! of:
//!
//!   1. the prior-live content (doc count == `BEFORE_DOCS`)
//!   2. the newly published content (doc count == `AFTER_DOCS`)
//!   3. a transient read error (Err) or a transiently absent path (Ok(None))
//!
//! Any other observation — a readable summary with a doc count that
//! matches NEITHER `BEFORE_DOCS` nor `AFTER_DOCS` — means a reader saw
//! a half-torn intermediate filesystem state. That is exactly what
//! the atomic-swap publish path exists to prevent.
//!
//! The sibling in-process tests
//! `publish_staged_lexical_index_recovers_from_crash_between_park_and_swap`
//! and `publish_staged_lexical_index_retains_stale_in_progress_backup_when_live_present`
//! cover the sequential RECOVERY side of the invariant by manually
//! constructing the filesystem state a crash would leave behind. This
//! test covers the CONCURRENT-READER side by exercising the real
//! `cass index --full` binary and polling the live index path while
//! the publish is in flight.

use assert_cmd::Command;
use coding_agent_search::search::tantivy::{
    SearchableIndexSummary, open_federated_search_readers, searchable_index_summary,
};
use frankensearch::lexical::ReloadPolicy;
use serde_json::json;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn cass_cmd(home: &std::path::Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd.env("HOME", home);
    cmd.env("XDG_DATA_HOME", home.join(".local/share"));
    cmd.env("XDG_CONFIG_HOME", home.join(".config"));
    cmd.env("CODEX_HOME", home.join(".codex"));
    cmd
}

fn iso_ts(ts_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts_ms)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339()
}

fn seed_codex_session(
    codex_home: &std::path::Path,
    date_path: &str,
    filename: &str,
    ts_ms: i64,
    keyword: &str,
) {
    let sessions = codex_home.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let workspace = codex_home.to_string_lossy().into_owned();
    let lines = [
        json!({
            "timestamp": iso_ts(ts_ms),
            "type": "session_meta",
            "payload": { "id": filename, "cwd": workspace, "cli_version": "0.42.0" },
        }),
        json!({
            "timestamp": iso_ts(ts_ms + 1_000),
            "type": "response_item",
            "payload": {
                "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": keyword }],
            },
        }),
        json!({
            "timestamp": iso_ts(ts_ms + 2_000),
            "type": "response_item",
            "payload": {
                "type": "message", "role": "assistant",
                "content": [{ "type": "text", "text": format!("{keyword} response") }],
            },
        }),
    ];
    let mut body = String::new();
    for line in lines {
        body.push_str(&serde_json::to_string(&line).unwrap());
        body.push('\n');
    }
    fs::write(sessions.join(filename), body).unwrap();
}

fn force_federated_publish_env(cmd: &mut Command) {
    cmd.env("CASS_TANTIVY_REBUILD_WORKERS", "6");
    cmd.env("CASS_TANTIVY_MAX_WRITER_THREADS", "2");
    cmd.env("CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS", "1");
    cmd.env(
        "CASS_TANTIVY_REBUILD_INITIAL_BATCH_FETCH_CONVERSATIONS",
        "1",
    );
    cmd.env("CASS_TANTIVY_REBUILD_COMMIT_EVERY_CONVERSATIONS", "1");
    cmd.env(
        "CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_CONVERSATIONS",
        "1",
    );
    cmd.env("CASS_TANTIVY_REBUILD_COMMIT_EVERY_MESSAGES", "2");
    cmd.env("CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_MESSAGES", "2");
    cmd.env("CASS_TANTIVY_REBUILD_COMMIT_EVERY_MESSAGE_BYTES", "4096");
    cmd.env(
        "CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_MESSAGE_BYTES",
        "4096",
    );
}

#[test]
fn concurrent_reader_never_sees_half_torn_lexical_index_during_publish_swap() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Phase A: seed one session, build the initial live index.
    seed_codex_session(
        &codex_home,
        "2026/04/23",
        "swap-before.jsonl",
        1_714_000_000_000,
        "alphabet",
    );
    cass_cmd(&home)
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let index_path = coding_agent_search::search::tantivy::index_dir(&data_dir)
        .expect("resolve versioned tantivy index path");
    let before = searchable_index_summary(&index_path)
        .expect("initial summary readable")
        .expect("initial index present");
    let before_docs = before.docs;
    assert!(
        before_docs >= 1,
        "precondition: live index has at least 1 doc"
    );

    // Phase B: concurrent reader polling tight loop until either the
    // publish-triggering `cass index --full --force-rebuild` returns
    // or the deadline lapses. We use `--force-rebuild` on the SAME
    // seeded content so the invariant becomes "every reader
    // observation sees the stable doc count, an Err, or a missing
    // path — never a DIFFERENT positive doc count". This is a
    // strictly stronger assertion than "doc count is one of two
    // values" because any other positive count would be a torn
    // intermediate state. Record every observation so assertions
    // below can inspect the full history.
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let reader_index_path = index_path.clone();
    let deadline = Instant::now() + Duration::from_secs(20);
    let reader = thread::spawn(move || {
        let mut observations: Vec<Result<Option<SearchableIndexSummary>, String>> = Vec::new();
        while !reader_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            let obs = searchable_index_summary(&reader_index_path).map_err(|e| format!("{e:#}"));
            observations.push(obs);
            // Don't burn a full core — 1ms polling is enough to
            // blanket any swap that takes >1ms, and every real
            // publish does. Keeps the test from being CI-noisy.
            thread::sleep(Duration::from_millis(1));
        }
        observations
    });

    // Phase C: trigger the rebuild + atomic-swap publish.
    // `--force-rebuild` is the load-bearing flag: it forces the
    // authoritative serial rebuild path to re-emit the staged index
    // even when the canonical DB hasn't changed, which is exactly the
    // atomic swap we want a concurrent reader to observe.
    cass_cmd(&home)
        .args(["index", "--full", "--force-rebuild", "--json", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    stop.store(true, Ordering::Relaxed);
    let observations = reader.join().expect("reader thread panicked");

    let after = searchable_index_summary(&index_path)
        .expect("post-publish summary readable")
        .expect("post-publish index present");
    assert_eq!(
        after.docs, before_docs,
        "--force-rebuild on unchanged content must produce the same doc count; \
         any discrepancy here means the test's premise is off, not that the \
         atomic-swap invariant was violated"
    );

    assert!(
        !observations.is_empty(),
        "reader must have collected at least one observation during the publish window"
    );

    // Invariant: every observation is one of:
    //   - Ok(Some(stable doc count)) — the rebuild must not wipe the
    //     live index. Bead 9ct8r's fix guards the pre-wipe behind a
    //     `!will_use_atomic_staged_publish` check so the live index
    //     stays intact until publish_staged_lexical_index atomically
    //     swaps the new one in.
    //   - Ok(None) — path briefly absent during non-Linux rename
    //     fallback between park and swap.
    //   - Err(_) — transient Tantivy open errors during a swap
    //     (meta.json being renamed into place, etc.).
    // Any other doc count — including `Ok(Some(0))` — would mean a
    // reader observed a half-torn intermediate Tantivy state, which is
    // exactly what the atomic-swap publish path exists to prevent. If
    // this test starts failing with `Ok(Some(0))` observations, bead
    // 9ct8r regressed: the staged-shards delegation stopped running
    // (e.g., total_conversations dropped to 0 or the shard plan
    // collapsed to a single shard), or a new non-atomic wipe snuck
    // into the rebuild lifecycle.
    for (i, obs) in observations.iter().enumerate() {
        if let Ok(Some(summary)) = obs {
            assert_eq!(
                summary.docs,
                before_docs,
                "observation #{i} returned {docs} docs; expected the stable \
                 count {before_docs}. An intermediate doc count means a \
                 reader observed a half-torn Tantivy state — the atomic-swap \
                 rebuild invariant from bead 9ct8r has regressed. total \
                 observations = {total}",
                docs = summary.docs,
                total = observations.len()
            );
        }
    }
}

#[test]
fn concurrent_reader_never_sees_half_torn_federated_lexical_index_during_publish_swap() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    for (filename, ts_ms, keyword) in [
        ("swap-fed-1.jsonl", 1_714_100_000_000_i64, "federated alpha"),
        ("swap-fed-2.jsonl", 1_714_100_100_000_i64, "federated beta"),
        ("swap-fed-3.jsonl", 1_714_100_200_000_i64, "federated gamma"),
    ] {
        seed_codex_session(&codex_home, "2026/04/23", filename, ts_ms, keyword);
    }

    let mut initial_index = cass_cmd(&home);
    force_federated_publish_env(&mut initial_index);
    initial_index
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let index_path = coding_agent_search::search::tantivy::index_dir(&data_dir)
        .expect("resolve versioned tantivy index path");
    let before = searchable_index_summary(&index_path)
        .expect("initial federated summary readable")
        .expect("initial federated index present");
    let before_docs = before.docs;
    assert!(
        before_docs >= 3,
        "precondition: live federated index should contain multiple docs"
    );
    let before_federated_readers = open_federated_search_readers(&index_path, ReloadPolicy::Manual)
        .expect("load federated readers before rebuild")
        .expect("federated manifest should exist before rebuild");
    assert!(
        before_federated_readers.len() > 1,
        "forced shard planner settings should materialize a federated live index before rebuild"
    );

    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let reader_index_path = index_path.clone();
    let deadline = Instant::now() + Duration::from_secs(20);
    let reader = thread::spawn(move || {
        let mut observations: Vec<Result<Option<SearchableIndexSummary>, String>> = Vec::new();
        while !reader_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            let obs = searchable_index_summary(&reader_index_path).map_err(|e| format!("{e:#}"));
            observations.push(obs);
            thread::sleep(Duration::from_millis(1));
        }
        observations
    });

    let mut rebuild = cass_cmd(&home);
    force_federated_publish_env(&mut rebuild);
    rebuild
        .args(["index", "--full", "--force-rebuild", "--json", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    stop.store(true, Ordering::Relaxed);
    let observations = reader.join().expect("reader thread panicked");

    let after = searchable_index_summary(&index_path)
        .expect("post-publish federated summary readable")
        .expect("post-publish federated index present");
    assert_eq!(
        after.docs, before_docs,
        "forced federated --force-rebuild on unchanged content must preserve the doc count"
    );
    let after_federated_readers = open_federated_search_readers(&index_path, ReloadPolicy::Manual)
        .expect("load federated readers after rebuild")
        .expect("federated manifest should still exist after rebuild");
    assert!(
        after_federated_readers.len() > 1,
        "post-rebuild live index should remain a federated publish bundle"
    );

    assert!(
        !observations.is_empty(),
        "reader must collect observations during the federated publish window"
    );
    for (i, obs) in observations.iter().enumerate() {
        if let Ok(Some(summary)) = obs {
            assert_eq!(
                summary.docs,
                before_docs,
                "federated observation #{i} returned {docs} docs; expected the stable count {before_docs}. \
                 Any other readable doc count indicates a half-torn federated lexical publish surface",
                docs = summary.docs
            );
        }
    }
}

/// Bead coding_agent_session_search-mux5k:
/// E2E regression proving that a SIGKILL during the atomic publish
/// window (after swap, while the canonical sidecar is parked) is
/// recovered cleanly on the next cass invocation.
///
/// Uses the `CASS_TEST_LEXICAL_PUBLISH_KILL_RELAUNCH_SENTINEL` env gate
/// so we don't rely on race timing.
#[cfg(target_os = "linux")]
#[test]
fn kill_relaunch_recovers_lexical_publish_and_search_stays_stable() {
    use std::process::{Command as StdCommand, Stdio};

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let base_ts = 1_700_000_000_000i64;
    seed_codex_session(
        &codex_home,
        "2023-11-14",
        "s1.jsonl",
        base_ts,
        "killrelaunch",
    );

    // Phase 1: build the initial index so there's a live generation.
    let mut cmd = cass_cmd(&home);
    cmd.args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    force_federated_publish_env(&mut cmd);
    cmd.assert().success();

    let index_path = data_dir.join("index/tantivy");

    // Confirm there IS a live index now.
    let before_summary =
        searchable_index_summary(&index_path).expect("summary before kill-relaunch");
    assert!(
        before_summary.is_some(),
        "live index must exist before kill-relaunch test"
    );
    let _before_docs = before_summary.unwrap().docs;

    // Phase 2: seed a second session so --force-rebuild builds a NEW index.
    seed_codex_session(
        &codex_home,
        "2023-11-15",
        "s2.jsonl",
        base_ts + 86_400_000,
        "killrelaunch extra",
    );

    // Prepare the sentinel path that the publish gate will write to.
    let sentinel_path = data_dir.join("kill_relaunch_sentinel.json");

    // Spawn cass index --full --force-rebuild with the pause sentinel.
    let cass_bin = assert_cmd::cargo::cargo_bin!("cass");
    let mut child = StdCommand::new(cass_bin)
        .args(["index", "--full", "--force-rebuild", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("HOME", &home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CODEX_HOME", &codex_home)
        .env(
            "CASS_TEST_LEXICAL_PUBLISH_KILL_RELAUNCH_SENTINEL",
            &sentinel_path,
        )
        .env("CASS_TEST_LEXICAL_PUBLISH_KILL_RELAUNCH_SLEEP_MS", "30000")
        .env("CASS_TANTIVY_REBUILD_WORKERS", "6")
        .env("CASS_TANTIVY_MAX_WRITER_THREADS", "2")
        .env("CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS", "1")
        .env(
            "CASS_TANTIVY_REBUILD_INITIAL_BATCH_FETCH_CONVERSATIONS",
            "1",
        )
        .env("CASS_TANTIVY_REBUILD_COMMIT_EVERY_CONVERSATIONS", "1")
        .env(
            "CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_CONVERSATIONS",
            "1",
        )
        .env("CASS_TANTIVY_REBUILD_COMMIT_EVERY_MESSAGES", "2")
        .env("CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_MESSAGES", "2")
        .env("CASS_TANTIVY_REBUILD_COMMIT_EVERY_MESSAGE_BYTES", "4096")
        .env(
            "CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_MESSAGE_BYTES",
            "4096",
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cass index for kill-relaunch");

    // Wait for the sentinel file to appear (process is paused inside publish).
    let deadline = Instant::now() + Duration::from_secs(120);
    while !sentinel_path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for kill-relaunch sentinel — cass may have exited before reaching the publish gate"
        );
        thread::sleep(Duration::from_millis(100));
    }

    // Read the sentinel to verify structure and get PID.
    let sentinel_raw = fs::read_to_string(&sentinel_path).expect("read sentinel JSON");
    let sentinel: serde_json::Value =
        serde_json::from_str(&sentinel_raw).expect("parse sentinel JSON");
    assert_eq!(
        sentinel["stage"].as_str(),
        Some("linux_swap_committed_prior_live_parked"),
        "sentinel stage must indicate the process paused after swap+park"
    );
    let pid = sentinel["pid"].as_u64().expect("sentinel must contain pid");
    assert_eq!(
        pid,
        u64::from(child.id()),
        "sentinel PID must match spawned child"
    );

    // Verify the canonical sidecar exists (OLD generation parked).
    let canonical_sidecar = sentinel["canonical_sidecar_path"]
        .as_str()
        .expect("sentinel must contain canonical_sidecar_path");
    assert!(
        std::path::Path::new(canonical_sidecar).exists(),
        "canonical sidecar must exist while process is paused"
    );

    // SIGKILL the child — simulates a hard crash mid-publish.
    child.kill().expect("SIGKILL child process");
    let exit = child.wait().expect("wait for killed child");
    assert!(
        !exit.success(),
        "killed process must exit with failure status"
    );

    // The canonical sidecar should still be on disk after the crash.
    assert!(
        std::path::Path::new(canonical_sidecar).exists(),
        "canonical sidecar must survive the SIGKILL"
    );

    // Phase 3: relaunch cass — recovery should finalize the interrupted backup.
    let mut cmd = cass_cmd(&home);
    cmd.args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    force_federated_publish_env(&mut cmd);
    let relaunch_output = cmd.output().expect("relaunch cass index");
    assert!(
        relaunch_output.status.success(),
        "relaunched cass index must succeed after crash recovery; stderr: {}",
        String::from_utf8_lossy(&relaunch_output.stderr)
    );

    // After recovery: canonical sidecar should be gone (moved to retained backups).
    assert!(
        !std::path::Path::new(canonical_sidecar).exists(),
        "canonical sidecar must be cleaned up after recovery"
    );

    // Search must still work and return results.
    let mut search_cmd = cass_cmd(&home);
    search_cmd
        .args(["search", "killrelaunch", "--robot", "--data-dir"])
        .arg(&data_dir);
    let search_output = search_cmd.output().expect("search after recovery");
    assert!(
        search_output.status.success(),
        "search after kill-relaunch recovery must succeed; stderr: {}",
        String::from_utf8_lossy(&search_output.stderr)
    );

    let search_json: serde_json::Value = serde_json::from_slice(&search_output.stdout)
        .unwrap_or_else(|_| {
            panic!(
                "search output must be valid JSON: {}",
                String::from_utf8_lossy(&search_output.stdout)
            )
        });
    let results = search_json["results"]
        .as_array()
        .or_else(|| search_json.as_array());
    assert!(
        results.is_some_and(|r| !r.is_empty()),
        "search after recovery must return at least one result"
    );
}
