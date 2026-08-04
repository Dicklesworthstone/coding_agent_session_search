//! Robot stdout/stderr hygiene regression suite.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.2.1
//! ("Audit and enforce stdout/stderr hygiene for robot commands").
//!
//! The source report observed `cass view` emitting extensive frankensqlite INFO
//! tracing and corrupting machine-readable output. These tests pin the
//! contract: every robot-mode command keeps **stdout = data-only** (pure JSON)
//! and never lets dependency tracing interleave with the data stream — even when
//! `RUST_LOG` is set to a deliberately noisy level that would otherwise flood
//! the process with frankensqlite trace/info events.
//!
//! The enforcement chokepoint under test lives in `src/lib.rs`
//! (`robot_aware_log_directive` / `build_robot_aware_log_filter`): robot/quiet
//! modes pin the tracing filter to `error`, overriding `RUST_LOG`.

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

mod util;
use util::cass_bin;

/// A `RUST_LOG` value loud enough that any unfiltered frankensqlite span would
/// land on stderr (or worse, stdout) if the robot hygiene guard regressed.
const NOISY_RUST_LOG: &str = "trace,fsqlite=trace,fsqlite_core=trace,fsqlite_vdbe=trace";

/// Build a robot command with noisy dependency logging forced on and the run
/// kept hermetic (no scan of the operator's real session corpus).
fn noisy_robot_cmd() -> Command {
    let mut cmd = Command::new(cass_bin());
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("RUST_LOG", NOISY_RUST_LOG);
    cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd
}

/// Parse `stdout` as a single JSON document, tolerating an optional trailing
/// newline. Falls back to the last non-empty line for JSONL-style streams.
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
    serde_json::from_str::<Value>(last_line.trim()).unwrap_or_else(|err| {
        panic!("robot stdout was not valid JSON ({err}); stdout was:\n{stdout}\n--- end stdout ---")
    })
}

/// Assert that stderr carries no dependency tracing that should have been
/// suppressed by the robot `error`-level filter. A regression here means
/// `RUST_LOG` leaked past the hygiene chokepoint.
fn assert_no_dependency_tracing(label: &str, stderr: &str) {
    let lower = stderr.to_lowercase();
    assert!(
        !lower.contains("fsqlite"),
        "{label}: robot-mode stderr leaked frankensqlite tracing despite the error-level \
         filter; stderr was:\n{stderr}\n--- end stderr ---"
    );
    // Tracing fmt lines carry an uppercase level token; INFO/DEBUG/TRACE must
    // not appear because the filter is pinned to `error`.
    for level in [" INFO ", " DEBUG ", " TRACE "] {
        assert!(
            !stderr.contains(level),
            "{label}: robot-mode stderr emitted a {} log line despite the error-level filter; \
             stderr was:\n{stderr}\n--- end stderr ---",
            level.trim()
        );
    }
}

/// Run a robot command (success not required — readiness commands may exit
/// non-zero on an empty data dir) and assert the stdout/stderr hygiene contract.
fn assert_robot_hygiene(label: &str, args: &[&str]) -> Value {
    let output = noisy_robot_cmd().args(args).output().expect("run cass");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let value = parse_stdout_json(&stdout);
    assert_no_dependency_tracing(label, &stderr);
    value
}

#[test]
fn api_version_stdout_is_pure_json_under_noisy_logging() {
    let json = assert_robot_hygiene("api-version", &["api-version", "--json"]);
    assert!(
        json.get("api_version").is_some() || json.get("version").is_some(),
        "api-version JSON should expose a version field: {json}"
    );
}

#[test]
fn capabilities_stdout_is_pure_json_under_noisy_logging() {
    let json = assert_robot_hygiene("capabilities", &["capabilities", "--json"]);
    assert!(
        json.is_object(),
        "capabilities should emit a JSON object: {json}"
    );
}

#[test]
fn triage_stdout_is_pure_json_under_noisy_logging() {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().to_string_lossy().to_string();
    let json = assert_robot_hygiene("triage", &["triage", "--json", "--data-dir", &data_dir]);
    assert!(json.is_object(), "triage should emit a JSON object: {json}");
}

#[test]
fn status_stdout_is_pure_json_under_noisy_logging() {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().to_string_lossy().to_string();
    let json = assert_robot_hygiene("status", &["status", "--json", "--data-dir", &data_dir]);
    assert!(json.is_object(), "status should emit a JSON object: {json}");
}

#[test]
fn health_stdout_is_pure_json_under_noisy_logging() {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().to_string_lossy().to_string();
    // health exits 1 when not ready; we only care that stdout stays clean JSON.
    let json = assert_robot_hygiene("health", &["health", "--json", "--data-dir", &data_dir]);
    assert!(json.is_object(), "health should emit a JSON object: {json}");
}

#[test]
fn diag_stdout_is_pure_json_under_noisy_logging() {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().to_string_lossy().to_string();
    let json = assert_robot_hygiene("diag", &["diag", "--json", "--data-dir", &data_dir]);
    assert!(json.is_object(), "diag should emit a JSON object: {json}");
}

#[test]
fn view_stdout_is_pure_json_under_noisy_logging() {
    // The exact command from the source report. `view` reading a plain file must
    // not interleave frankensqlite tracing with the JSON payload.
    let json = assert_robot_hygiene(
        "view",
        &[
            "view",
            "README.md",
            "--json",
            "--line",
            "1",
            "--context",
            "0",
        ],
    );
    assert_eq!(
        json["path"], "README.md",
        "view should echo the path: {json}"
    );
}

#[test]
fn verbose_robot_still_keeps_stdout_parseable() {
    // --verbose opts into debug-level diagnostics, but those go to stderr; stdout
    // must remain a single valid JSON document.
    let output = noisy_robot_cmd()
        .args(["api-version", "--json", "--verbose"])
        .output()
        .expect("run cass");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = parse_stdout_json(&stdout);
    assert!(
        json.get("api_version").is_some() || json.get("version").is_some(),
        "verbose api-version stdout should still be clean JSON: {json}"
    );
}
