//! `cass view` bounded-budget signal regression suite.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.2.6
//! (wire bounded execution budget into the remaining robot surfaces) — view.
//!
//! The report saw `cass view` fail under a 10s cap. View is a single bounded read
//! (file fast-path or DB/archive fallback), so it sheds nothing; it now emits a
//! stable `budget` block so an agent can tell whether the read exceeded its
//! budget and which cheaper probe to run next. Overridable via
//! CASS_VIEW_BUDGET_MS, which these tests use to exercise both the within-budget
//! and exceeded cases. The timeout case also sets `CASS_TEST_VIEW_SLOW_MS`,
//! matching the deterministic slowdown pattern used by the status budget suite
//! instead of assuming a file read must take longer than one millisecond on
//! every host.

use assert_cmd::Command;
use serde_json::Value;

mod util;
use util::cass_bin;

fn view_json(budget_ms: &str, test_delay_ms: Option<&str>) -> Value {
    // README.md is a real file at the repo root, so view takes the file fast path.
    let mut command = Command::new(cass_bin());
    command
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_VIEW_BUDGET_MS", budget_ms);
    if let Some(test_delay_ms) = test_delay_ms {
        command.env("CASS_TEST_VIEW_SLOW_MS", test_delay_ms);
    }
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<Value>(stdout.trim())
        .unwrap_or_else(|e| panic!("view stdout not valid JSON ({e}); stdout:\n{stdout}"))
}

#[test]
fn view_emits_budget_block_within_budget() {
    let json = view_json("60000", None);
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
fn view_reports_timed_out_when_budget_exceeded() {
    let json = view_json("1", Some("25"));
    let budget = &json["budget"];
    assert_eq!(
        budget["timed_out"], true,
        "1ms budget must be exceeded: {budget}"
    );
    assert_eq!(
        budget["budget_ms"].as_u64(),
        Some(1),
        "budget_ms reflects override: {budget}"
    );
    assert_eq!(budget["skipped_sections"], serde_json::json!([]));
    assert_eq!(
        budget["recommended_next_probe"], "cass health --json",
        "timed-out view should point at a cheaper bounded probe: {budget}"
    );
    // stdout stays a single valid JSON object even when the budget is exceeded.
    assert!(
        json.is_object(),
        "view output must remain valid JSON: {json}"
    );
    assert_eq!(
        json["path"], "README.md",
        "content still returned under exceeded budget: {json}"
    );
}
