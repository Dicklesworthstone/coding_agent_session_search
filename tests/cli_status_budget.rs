//! `cass status` and `cass triage` bounded-budget / partial-envelope regression suite.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.2.2
//! ("Add bounded execution budgets and partial/error envelopes for slow robot
//! surfaces").
//!
//! The report observed `cass status` timing out under an 8s cap. `status` now
//! carries a bounded budget (env `CASS_STATUS_BUDGET_MS`): when the
//! optional/expensive sections (quarantine FS scan, coverage risk, remote sync,
//! doctor summary) would exceed it, they are shed and status returns a parseable
//! PARTIAL result — a `budget` block with `elapsed_ms`, `budget_ms`,
//! `timed_out`, `skipped_sections`, and `recommended_next_probe` — instead of
//! blocking. A deterministic test slowdown (`CASS_TEST_STATUS_SLOW_MS`) trips the
//! budget so this is reproducible without a real slow archive. The probe stays
//! read-only either way.

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

mod util;
use util::cass_bin;

fn status_cmd(data_dir: &str) -> Command {
    let mut cmd = Command::new(cass_bin());
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd.args(["status", "--json", "--data-dir", data_dir]);
    cmd
}

fn triage_cmd(data_dir: &str) -> Command {
    let mut cmd = Command::new(cass_bin());
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd.args(["triage", "--json", "--data-dir", data_dir]);
    cmd
}

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_string())
    }
}

fn parse_stdout_json(stdout: &str) -> Value {
    let trimmed = stdout.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return value;
    }
    let last_line = trimmed
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    serde_json::from_str::<Value>(last_line.trim())
        .unwrap_or_else(|err| panic!("status stdout not valid JSON ({err}); stdout:\n{stdout}"))
}

#[test]
fn status_emits_budget_block_when_healthy() {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().to_string_lossy().to_string();

    // Generous budget, no induced delay → complete result, nothing skipped.
    let output = status_cmd(&data_dir)
        .env("CASS_STATUS_BUDGET_MS", "60000")
        .output()
        .expect("run cass status");
    let json = parse_stdout_json(&String::from_utf8_lossy(&output.stdout));

    let budget = &json["budget"];
    assert!(
        budget.is_object(),
        "status JSON should carry a budget block: {json}"
    );
    assert_eq!(
        budget["timed_out"], false,
        "healthy run must not be timed_out"
    );
    assert_eq!(
        budget["skipped_sections"].as_array().map(Vec::len),
        Some(0),
        "healthy run should skip nothing: {budget}"
    );
    assert!(budget["budget_ms"].as_u64().is_some(), "budget_ms present");
    assert!(
        budget["elapsed_ms"].as_u64().is_some(),
        "elapsed_ms present"
    );
    // Optional sections are present (not shed) on a healthy run.
    assert!(
        !json["quarantine"].is_null(),
        "quarantine present on healthy run"
    );
}

#[test]
fn status_returns_partial_envelope_when_budget_tripped() {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().to_string_lossy().to_string();

    // Tiny budget + an induced slowdown that exceeds it → optional sections shed,
    // partial result with timed_out=true and a recommended next probe.
    let output = status_cmd(&data_dir)
        .env("CASS_STATUS_BUDGET_MS", "1")
        .env("CASS_TEST_STATUS_SLOW_MS", "150")
        .output()
        .expect("run cass status");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = parse_stdout_json(&stdout);

    let budget = &json["budget"];
    assert!(
        budget.is_object(),
        "status JSON should carry a budget block: {json}"
    );
    assert_eq!(
        budget["timed_out"], true,
        "tripped budget must set timed_out: {budget}"
    );
    let skipped = budget["skipped_sections"]
        .as_array()
        .expect("skipped_sections array");
    assert!(
        !skipped.is_empty(),
        "tripped budget must record skipped sections: {budget}"
    );
    assert!(
        skipped.iter().any(|s| s == "quarantine"),
        "quarantine should be shed when slow: {budget}"
    );
    assert_eq!(
        budget["recommended_next_probe"], "cass doctor check --json",
        "partial result should point to doctor: {budget}"
    );
    assert!(
        budget["elapsed_ms"].as_u64().unwrap_or(0) >= 1,
        "elapsed_ms should reflect the induced delay: {budget}"
    );

    // The shed sections are null, but core readiness facts are still present —
    // enough for an agent to act safely.
    assert!(
        json["quarantine"].is_null(),
        "shed quarantine is null on partial result"
    );
    assert!(
        json.get("status").is_some(),
        "core status field still present"
    );
    assert!(
        json.get("index").is_some(),
        "core index facts still present"
    );
    assert!(
        json.get("database").is_some(),
        "core database facts still present"
    );
}

#[test]
fn status_stdout_stays_pure_json_even_when_budget_tripped() {
    // The whole point: a tripped budget must not produce truncated/garbage output.
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().to_string_lossy().to_string();
    let output = status_cmd(&data_dir)
        .env("CASS_STATUS_BUDGET_MS", "1")
        .env("CASS_TEST_STATUS_SLOW_MS", "120")
        .output()
        .expect("run cass status");
    let json = parse_stdout_json(&String::from_utf8_lossy(&output.stdout));
    assert!(
        json.is_object(),
        "partial status must still be a single JSON object"
    );
}

#[test]
fn triage_emits_complete_budget_block_with_optional_guidance() -> Result<(), String> {
    let tmp = TempDir::new().map_err(|error| error.to_string())?;
    let data_dir = tmp.path().to_string_lossy().to_string();
    let output = triage_cmd(&data_dir)
        .env("CASS_TRIAGE_BUDGET_MS", "60000")
        .output()
        .map_err(|error| error.to_string())?;
    let json = parse_stdout_json(&String::from_utf8_lossy(&output.stdout));
    let budget = &json["budget"];

    require(budget.is_object(), "triage budget block is missing")?;
    require(
        budget["timed_out"] == false,
        "generous triage budget timed out",
    )?;
    require(
        budget["skipped_sections"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "complete triage skipped optional sections",
    )?;
    require(
        json["starter_workflows"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "complete triage omitted starter workflows",
    )?;
    require(
        json["mistake_recoveries"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "complete triage omitted mistake recoveries",
    )
}

#[test]
fn triage_timeout_keeps_core_readiness_and_names_every_skip() -> Result<(), String> {
    let tmp = TempDir::new().map_err(|error| error.to_string())?;
    let data_dir = tmp.path().to_string_lossy().to_string();
    let output = triage_cmd(&data_dir)
        .env("CASS_TRIAGE_BUDGET_MS", "1")
        .env("CASS_TEST_TRIAGE_SLOW_MS", "50")
        .output()
        .map_err(|error| error.to_string())?;
    let json = parse_stdout_json(&String::from_utf8_lossy(&output.stdout));
    let budget = &json["budget"];
    let skipped = budget["skipped_sections"]
        .as_array()
        .ok_or_else(|| "triage skipped_sections is not an array".to_string())?;

    require(
        budget["timed_out"] == true,
        "triage timeout was not reported",
    )?;
    for section in ["database_counts", "starter_workflows", "mistake_recoveries"] {
        require(
            skipped.iter().any(|value| value == section),
            "triage timeout omitted a skipped section",
        )?;
    }
    require(
        budget["recommended_next_probe"] == "cass health --json",
        "triage timeout did not recommend the cheap health probe",
    )?;
    require(
        budget["elapsed_ms"]
            .as_u64()
            .is_some_and(|value| value >= 1),
        "triage timeout did not report elapsed time",
    )?;
    require(
        json["readiness"].is_object() && json["status"].is_string(),
        "triage timeout dropped core readiness facts",
    )?;
    require(
        json["starter_workflows"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "triage timeout retained skipped starter workflows",
    )?;
    require(
        json["mistake_recoveries"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "triage timeout retained skipped mistake recoveries",
    )
}
