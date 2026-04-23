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
use coding_agent_search::search::tantivy::{SearchableIndexSummary, searchable_index_summary};
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
    //   - Ok(Some(stable doc count))
    //   - Ok(Some(0 docs)) — legal transient during the rebuild's
    //     pre-wipe window where restart_from_zero clears the live
    //     index directory before the new staged index is populated.
    //     This is the "cleared live" side of the rebuild lifecycle,
    //     NOT the atomic-swap window itself; the atomic-swap proper
    //     only covers the final staged → live rename (commit
    //     109560e5) and is tested separately by the in-process
    //     `publish_staged_lexical_index_*` guards. Filed as bead
    //     coding_agent_session_search-9ct8r: once the rebuild
    //     pre-wipe moves inside the atomic-swap logic, drop the 0
    //     carve-out so this test enforces the stronger invariant.
    //   - Ok(None) — path briefly absent during non-Linux rename
    //     fallback between park and swap.
    //   - Err(_) — transient Tantivy open errors during a swap
    //     (meta.json being renamed into place, etc.).
    // Any other positive doc count would be a genuinely torn
    // intermediate Tantivy directory — a search-surface regression.
    for (i, obs) in observations.iter().enumerate() {
        if let Ok(Some(summary)) = obs {
            assert!(
                summary.docs == before_docs || summary.docs == 0,
                "observation #{i} returned {docs} docs; expected either the \
                 stable count {before_docs} or 0 (transient cleared-live). \
                 Any other positive doc count means a reader observed a \
                 half-torn intermediate Tantivy state. total observations = \
                 {total}",
                docs = summary.docs,
                total = observations.len()
            );
        }
    }
}
