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
