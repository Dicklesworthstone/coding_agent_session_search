//! CLI contract tests for `cass release-verify` (bead 5u82n.14).
//!
//! Exercises the robot-safe offline `--from` path, which reuses the tested
//! `release_verify::verify_from_json` core. The live `--live` network path is
//! covered by CI-only E2E (it requires reaching GitHub/crates.io) and is not
//! asserted here.

use std::error::Error;

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;

fn cass() -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass")); // ubs:ignore — fixed test binary from assert_cmd.
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd
}

fn request_json() -> Value {
    json!({
        "expected_version": "0.6.13",
        "channels": [
            {"channel": "github_release", "configured": true, "reachable": true,
             "observed_version": "0.6.13", "checksum_ok": true},
            {"channel": "crates_io", "configured": true, "reachable": true,
             "observed_version": "0.6.13"},
            {"channel": "homebrew", "configured": true, "reachable": true,
             "observed_version": "0.6.12", "dispatch_ran": false},
            {"channel": "scoop", "configured": false},
            {"channel": "installer_script", "configured": true, "reachable": true,
             "observed_version": "0.6.13", "installer_ok": true}
        ]
    })
}

fn run_from_file(body: &Value) -> Result<Value, Box<dyn Error>> {
    let tmp = TempDir::new()?;
    let path = tmp.path().join("request.json");
    std::fs::write(&path, serde_json::to_vec_pretty(body)?)?;
    let assert = cass()
        .args(["release-verify", "--json", "--from"])
        .arg(&path)
        .assert()
        .success();
    let output = assert.get_output();
    assert!(
        output.stderr.is_empty(),
        "release-verify should not log to stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[test]
fn release_verify_from_file_evaluates_offline() -> Result<(), Box<dyn Error>> {
    let report = run_from_file(&request_json())?;
    assert_eq!(report["schema_version"], json!(1));
    assert_eq!(report["expected_version"], json!("0.6.13"));
    // The lagging dispatch-driven homebrew channel keeps the release not-ready.
    assert_eq!(report["overall_ready"], json!(false));
    assert_eq!(report["summary"]["total"], json!(5));
    let channels = report["channels"].as_array().expect("channels array");
    assert_eq!(channels.len(), 5);
    // A manual remediation must be surfaced for the stale homebrew channel.
    let homebrew = channels
        .iter()
        .find(|c| c["channel"] == json!("homebrew"))
        .expect("homebrew channel");
    // A dispatch-driven channel whose notify workflow_dispatch did not run is
    // reported as `missing` (not merely stale) with a manual remediation.
    assert_eq!(homebrew["state"], json!("missing"));
    assert!(
        homebrew["manual_next_action"].as_str().is_some(),
        "unrun dispatch-driven channel must declare a manual_next_action"
    );
    Ok(())
}

#[test]
fn release_verify_all_ready_reports_overall_ready() -> Result<(), Box<dyn Error>> {
    let body = json!({
        "expected_version": "0.6.13",
        "channels": [
            {"channel": "github_release", "configured": true, "reachable": true,
             "observed_version": "0.6.13", "checksum_ok": true},
            {"channel": "crates_io", "configured": true, "reachable": true,
             "observed_version": "0.6.13"}
        ]
    });
    let report = run_from_file(&body)?;
    assert_eq!(report["overall_ready"], json!(true));
    assert_eq!(report["summary"]["ready"], json!(2));
    Ok(())
}

#[test]
fn release_verify_reads_request_from_stdin() -> Result<(), Box<dyn Error>> {
    let body = request_json();
    let assert = cass()
        .args(["release-verify", "--json", "--from", "-"])
        .write_stdin(serde_json::to_vec(&body)?)
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&assert.get_output().stdout)?;
    assert_eq!(report["expected_version"], json!("0.6.13"));
    Ok(())
}

#[test]
fn release_verify_without_mode_is_usage_error() {
    // Neither --from nor --live: must be a clean usage error, not a panic.
    let assert = cass().args(["release-verify", "--json"]).assert().failure();
    let code = assert.get_output().status.code();
    assert!(
        code.is_some_and(|c| c != 0),
        "missing mode should exit non-zero (got {code:?})"
    );
}

#[test]
fn release_verify_rejects_malformed_json() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("bad.json");
    std::fs::write(&path, b"{not valid json").unwrap();
    cass()
        .args(["release-verify", "--json", "--from"])
        .arg(&path)
        .assert()
        .failure();
}
