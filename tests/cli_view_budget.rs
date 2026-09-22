//! `cass view` bounded-budget signal regression suite.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.2.6
//! (wire bounded execution budget into the remaining robot surfaces) — view.
//!
//! The report saw `cass view` fail under a 10s cap. File/DB/archive resolution
//! now runs on a read-only worker behind a hard deadline. On deadline, robot
//! mode returns valid partial JSON with the completed request identity rather
//! than waiting for the stalled read.

use assert_cmd::Command;
use serde_json::Value;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

mod util;
use util::cass_bin;

// Exercise the exact bounded scanner, including cancellation in mid-record.
#[path = "../src/followup_coordinates/stream.rs"]
mod stream;

struct ViewRun {
    json: Value,
    status: ExitStatus,
    elapsed: Duration,
}

fn view_json(budget_ms: &str, test_delay_ms: Option<&str>) -> ViewRun {
    // README.md is a real file at the repo root, so view takes the file fast path.
    let mut command = Command::new(cass_bin());
    command
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_VIEW_BUDGET_MS", budget_ms);
    if let Some(test_delay_ms) = test_delay_ms {
        command.env("CASS_TEST_VIEW_SLOW_MS", test_delay_ms);
    }
    let started = Instant::now();
    let output = command
        .args([
            "view",
            "README.md",
            "--json",
            "--line",
            "1",
            "--context",
            "0",
        ])
        .output()
        .expect("run cass view");
    let elapsed = started.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = serde_json::from_str::<Value>(stdout.trim())
        .unwrap_or_else(|e| panic!("view stdout not valid JSON ({e}); stdout:\n{stdout}"));
    ViewRun {
        json,
        status: output.status,
        elapsed,
    }
}

#[test]
fn view_emits_budget_block_within_budget() {
    let run = view_json("60000", None);
    assert!(run.status.success(), "complete view should exit zero");
    let json = run.json;
    let budget = &json["budget"];
    assert!(
        budget.is_object(),
        "view JSON should carry a budget block: {json}"
    );
    assert_eq!(
        budget["timed_out"], false,
        "generous budget => not timed_out: {budget}"
    );
    assert_eq!(
        budget["budget_ms"].as_u64(),
        Some(60_000),
        "budget_ms reflects override: {budget}"
    );
    assert!(
        budget["elapsed_ms"].as_u64().is_some(),
        "elapsed_ms present: {budget}"
    );
    assert_eq!(budget["skipped_sections"], serde_json::json!([]));
    assert_eq!(budget["recommended_next_probe"], Value::Null);
    // The view payload is otherwise intact.
    assert_eq!(
        json["path"], "README.md",
        "view still echoes the path: {json}"
    );
}

#[test]
fn stalled_view_returns_partial_json_within_the_hard_deadline() {
    const SIMULATED_READ_STALL_MS: u64 = 2_000;
    const VIEW_BUDGET_MS: u64 = 50;
    const MAX_WALL_TIME_MS: u64 = 1_200;

    let run = view_json("50", Some("2000"));
    assert!(
        run.status.success(),
        "bounded partial view should exit zero"
    );
    assert!(
        run.elapsed < Duration::from_millis(MAX_WALL_TIME_MS),
        "view took {:?}; a {SIMULATED_READ_STALL_MS}ms read stall with a \
         {VIEW_BUDGET_MS}ms budget must return before the stalled worker",
        run.elapsed
    );
    let json = run.json;
    let budget = &json["budget"];
    assert_eq!(
        budget["timed_out"], true,
        "the hard deadline must be reported: {budget}"
    );
    assert_eq!(
        budget["budget_ms"].as_u64(),
        Some(VIEW_BUDGET_MS),
        "budget_ms reflects override: {budget}"
    );
    assert_eq!(
        budget["skipped_sections"],
        serde_json::json!(["view_content", "source_provenance"])
    );
    let retry = budget["recommended_next_probe"]
        .as_str()
        .expect("timed-out view should carry a bounded retry command");
    assert_eq!(
        json["lines"],
        serde_json::json!([]),
        "unfinished content must not be fabricated"
    );
    assert!(
        retry.starts_with("cass --db ")
            && retry.contains(" view README.md --line 1 --context 0 --json --timeout 10000")
            && !retry.contains("CASS_VIEW_BUDGET_MS="),
        "retry must preserve identity and use a CLI timeout: {retry}"
    );
    assert_eq!(json["path"], "README.md");
    assert_eq!(json["target_line"], 1);
    assert_eq!(json["context"], 0);
    assert!(
        json.get("total_lines").is_none()
            && json.get("source_exists").is_none()
            && json.get("archive_only").is_none(),
        "unfinished provenance fields must be omitted: {json}"
    );
}

#[test]
fn stalled_view_projection_is_bounded_and_preserves_compact_retry_format() {
    const VIEW_BUDGET_MS: u64 = 500;
    let mut command = Command::new(cass_bin());
    command
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_TEST_VIEW_PROJECTION_SLOW_MS", "2000");
    let started = Instant::now();
    let output = command
        .args([
            "view",
            "README.md",
            "--robot-format",
            "compact",
            "--line",
            "1",
            "--context",
            "0",
            "--timeout",
            "500",
        ])
        .output()
        .expect("run cass view with a stalled projection");
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "projection stall escaped the configured view budget"
    );
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 compact output");
    assert_eq!(stdout.lines().count(), 1, "compact output: {stdout}");
    let payload: Value = serde_json::from_str(stdout.trim()).expect("valid compact JSON");
    assert_eq!(payload["budget"]["timed_out"], true);
    assert!(
        payload["budget"]["skipped_sections"]
            .as_array()
            .is_some_and(|sections| sections
                .iter()
                .any(|section| section == "output_projection")),
        "projection timeout must name omitted work: {payload}"
    );
    let retry = payload["budget"]["recommended_next_probe"]
        .as_str()
        .expect("bounded retry");
    assert!(
        retry.contains("--robot-format compact")
            && retry.contains("--timeout 10000")
            && !retry.contains("--json"),
        "retry must preserve compact encoding: {retry}"
    );
    assert_eq!(payload["budget"]["budget_ms"], VIEW_BUDGET_MS);
}

fn physical_command(root: &std::path::Path, path: &std::path::Path, subcommand: &str) -> Command {
    let mut command = Command::new(cass_bin());
    command
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_VIEW_BUDGET_MS", "60000")
        .env_remove("CASS_OUTPUT_FORMAT")
        .env_remove("TOON_DEFAULT_FORMAT")
        .env_remove("CASS_TEST_VIEW_SLOW_MS")
        .env_remove("CASS_TEST_VIEW_PROJECTION_SLOW_MS")
        .args(["--color=never", subcommand])
        .arg(path)
        .arg("--json")
        .timeout(Duration::from_secs(10));
    command
}

#[test]
fn physical_view_skips_oversized_unselected_lines_and_counts_the_tail() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("large.txt");
    let mut source = std::fs::File::create(&path).unwrap();
    let chunk = [b'x'; 8192];
    for _ in 0..(stream::MAX_RECORD_BYTES / chunk.len() + 1) {
        source.write_all(&chunk).unwrap();
    }
    source.write_all(b"\ntarget\r\n").unwrap();
    for _ in 0..(stream::MAX_RECORD_BYTES / chunk.len() + 1) {
        source.write_all(&chunk).unwrap();
    }
    source.write_all(b"\ntail").unwrap();
    drop(source);
    let before = std::fs::metadata(&path).unwrap().len();
    let output = physical_command(root.path(), &path, "view")
        .args(["--line", "2", "-C", "0"])
        .assert()
        .success()
        .get_output()
        .clone();
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(payload["total_lines"], 4);
    assert_eq!(payload["lines"].as_array().unwrap().len(), 1);
    assert_eq!(payload["lines"][0]["content"], "target");
    assert_eq!(payload["lines"][0]["file_line"], 2);
    assert_eq!(payload["lines"][0]["is_target"], true);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
}

#[test]
fn requested_oversized_physical_record_fails_without_emitting_a_target() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("large.jsonl");
    let content = format!(
        "{{\"content\":\"{}\"}}\n",
        "x".repeat(stream::MAX_RECORD_BYTES)
    );
    std::fs::write(&path, content.as_bytes()).unwrap();
    for subcommand in ["view", "expand"] {
        let output = physical_command(root.path(), &path, subcommand)
            .args(["--line", "1", "-C", "0"])
            .assert()
            .failure()
            .get_output()
            .clone();
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "followup-resource-limit");
        assert_eq!(error["error"]["retryable"], false);
    }
    assert_eq!(std::fs::read(&path).unwrap(), content.as_bytes());
}

#[test]
fn physical_window_count_is_bounded_without_rejecting_large_context_on_small_files() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("many.jsonl");
    for subcommand in ["view", "expand"] {
        std::fs::write(&path, b"{\"content\":\"one\"}\n").unwrap();
        let context = usize::MAX.to_string();
        let output = physical_command(root.path(), &path, subcommand)
            .args(["--line", "1", "-C", &context])
            .assert()
            .success()
            .get_output()
            .clone();
        let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
        let rows = if subcommand == "view" {
            &payload["lines"]
        } else {
            &payload
        };
        assert_eq!(rows.as_array().unwrap().len(), 1);

        std::fs::write(
            &path,
            "{\"content\":\"one\"}\n".repeat(stream::MAX_WINDOW_RECORDS + 1),
        )
        .unwrap();
        let output = physical_command(root.path(), &path, subcommand)
            .args(["--line", "1", "-C", &context])
            .assert()
            .failure()
            .get_output()
            .clone();
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "followup-resource-limit");
    }
}

#[test]
fn discarded_tail_still_reports_invalid_utf8_instead_of_certifying_a_full_view() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("invalid.txt");
    std::fs::write(&path, b"target\n\xff\n").unwrap();
    let output = physical_command(root.path(), &path, "view")
        .args(["--line", "1", "-C", "0"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "file-read");
}

#[cfg(unix)]
#[test]
fn fifo_is_refused_without_waiting_for_a_writer_but_regular_symlinks_work() {
    let root = tempfile::tempdir().unwrap();
    let fifo = root.path().join("blocked.jsonl");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    for subcommand in ["view", "expand"] {
        let output = physical_command(root.path(), &fifo, subcommand)
            .args(["--line", "1", "-C", "0"])
            .assert()
            .failure()
            .get_output()
            .clone();
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "file-read");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not a regular file")
        );
    }
    let regular = root.path().join("regular.jsonl");
    std::fs::write(&regular, b"{\"content\":\"valid\"}\n").unwrap();
    let link = root.path().join("link.jsonl");
    std::os::unix::fs::symlink(&regular, &link).unwrap();
    for subcommand in ["view", "expand"] {
        physical_command(root.path(), &link, subcommand)
            .args(["--line", "1", "-C", "0"])
            .assert()
            .success();
    }
}
