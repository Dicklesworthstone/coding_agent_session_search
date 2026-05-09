//! Freshness check for `test-results/connector_edge_cases.log`.
//!
//! Per `coding_agent_session_search-4z5uc`. The log artifact is regenerated
//! on demand via `scripts/tests/connector_edge_cases_regenerate_log.sh`. This
//! test validates the log has the required header structure when present and
//! that its embedded commit SHA is reachable from HEAD.

use std::path::PathBuf;
use std::process::Command;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn log_path() -> PathBuf {
    project_root()
        .join("test-results")
        .join("connector_edge_cases.log")
}

#[test]
fn log_file_or_regenerate_script_present() {
    tracing::info!(target: "4z5uc_test", check = "artifact_path");
    // EITHER the log file exists OR the regenerate script does. Both is best;
    // the script alone is acceptable per AC.4 ("the regenerate script alone
    // is the artifact"). At least one MUST be present.
    let log = log_path();
    let script = project_root()
        .join("scripts")
        .join("tests")
        .join("connector_edge_cases_regenerate_log.sh");
    assert!(
        log.exists() || script.is_file(),
        "expected either {} or {} to exist",
        log.display(),
        script.display()
    );
    if !log.exists() {
        tracing::info!(
            target: "4z5uc_test",
            verdict = "log_absent_but_script_present",
            note = "regenerate script available; log will be created on first run"
        );
    }
}

#[test]
fn log_file_has_commit_header_when_present() {
    tracing::info!(target: "4z5uc_test", check = "header");
    let log = log_path();
    if !log.exists() {
        // Skip — covered by log_file_or_regenerate_script_present.
        eprintln!(
            "[4z5uc_test] log file absent; skipping header check (regenerate via scripts/tests/connector_edge_cases_regenerate_log.sh)"
        );
        return;
    }
    let body = std::fs::read_to_string(&log).expect("readable");
    assert!(
        body.contains("# commit:"),
        "log must include `# commit:` header line"
    );
    assert!(
        body.contains("# rustc:"),
        "log must include `# rustc:` header line"
    );
    assert!(
        body.contains("# date_utc:"),
        "log must include `# date_utc:` header line"
    );
}

#[test]
fn log_file_commit_is_reachable_from_head_when_present() {
    tracing::info!(target: "4z5uc_test", check = "commit_reachable");
    let log = log_path();
    if !log.exists() {
        eprintln!("[4z5uc_test] log file absent; skipping commit-reachability check");
        return;
    }
    let body = std::fs::read_to_string(&log).expect("readable");
    let mut commit: Option<String> = None;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("# commit:") {
            let val = rest.trim();
            if !val.is_empty() {
                commit = Some(val.to_string());
                break;
            }
        }
    }
    let commit = commit.expect("log header must include `# commit: <sha>`");
    // Verify the commit is reachable from HEAD via `git merge-base --is-ancestor`.
    // If not, the log is stale (commit was rewritten or rebased away).
    let out = Command::new("git")
        .arg("-C")
        .arg(project_root())
        .arg("merge-base")
        .arg("--is-ancestor")
        .arg(&commit)
        .arg("HEAD")
        .output()
        .expect("git available");
    assert!(
        out.status.success(),
        "log's embedded commit {commit} is not reachable from HEAD; regenerate the log via scripts/tests/connector_edge_cases_regenerate_log.sh"
    );
    tracing::info!(
        target: "4z5uc_test",
        commit = commit.as_str(),
        verdict = "reachable"
    );
}
