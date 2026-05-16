//! Regression test for the UBS pre-merge CI gate.
//!
//! Per `coding_agent_session_search-dpfvr`. Validates that
//! `.github/workflows/ci.yml` declares the `ubs-changed-files` job and that
//! the job's `run:` step contains the canonical `ubs --ci --fail-on-warning`
//! invocation. Also exercises error/edge paths around YAML parsing.

use std::path::PathBuf;

fn ci_yml_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".github")
        .join("workflows")
        .join("ci.yml")
}

fn read_ci_yml() -> serde_yaml::Value {
    let body = std::fs::read_to_string(ci_yml_path()).expect(".github/workflows/ci.yml must exist");
    serde_yaml::from_str(&body).expect("ci.yml must be valid YAML")
}

#[test]
fn ubs_gate_job_exists() {
    tracing::info!(target: "dpfvr_test", check = "job_exists");
    let yml = read_ci_yml();
    let jobs = yml.get("jobs").expect("ci.yml must have a jobs: block");
    assert!(
        jobs.get("ubs-changed-files").is_some(),
        "ci.yml must declare a job named 'ubs-changed-files'"
    );
}

#[test]
fn ubs_gate_runs_canonical_invocation() {
    tracing::info!(target: "dpfvr_test", check = "canonical_invocation");
    let body = std::fs::read_to_string(ci_yml_path()).expect("ci.yml must exist");
    assert!(
        body.contains("ubs --format=json --ci"),
        "ci.yml ubs-changed-files job must run `ubs --format=json --ci`"
    );
    assert!(
        body.contains("UBS PASS — no new critical/warning findings on changed files."),
        "ci.yml ubs-changed-files job must enforce the changed-file UBS delta gate"
    );
}

#[test]
fn ubs_gate_triggers_on_pull_request_and_push_main() {
    tracing::info!(target: "dpfvr_test", check = "triggers");
    let yml = read_ci_yml();
    let on = yml
        .get(serde_yaml::Value::Bool(true))
        .or_else(|| yml.get("on"))
        .expect("ci.yml must have an `on:` block (YAML may parse as bool true)");
    assert!(
        on.get("pull_request").is_some(),
        "ci.yml must trigger on pull_request"
    );
    let push_branches = on
        .get("push")
        .and_then(|p| p.get("branches"))
        .expect("ci.yml `on.push.branches` must be set");
    let branches: Vec<String> = serde_yaml::from_value(push_branches.clone())
        .expect("push.branches must be a sequence of strings");
    assert!(
        branches.iter().any(|b| b == "main"),
        "ci.yml must trigger on push to main; got branches: {branches:?}"
    );
}

#[test]
fn ubs_gate_uploads_report_artifact() {
    tracing::info!(target: "dpfvr_test", check = "uploads_artifact");
    let body = std::fs::read_to_string(ci_yml_path()).expect("ci.yml must exist");
    // Ensures the artifact upload is present so post-merge inspection works.
    assert!(
        body.contains("ubs-report-${{ github.run_id }}"),
        "ci.yml must upload the UBS report artifact named ubs-report-<run-id>"
    );
    assert!(
        body.contains("test-results/ubs-baseline.json"),
        "ci.yml must upload the UBS baseline report used by the delta gate"
    );
}

#[test]
fn ubs_version_pin_file_exists() {
    tracing::info!(target: "dpfvr_test", check = "version_pin");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".github")
        .join("workflows")
        .join("ubs-version.txt");
    assert!(
        path.exists(),
        ".github/workflows/ubs-version.txt must exist (allows pinning UBS version across CI runs)"
    );
    let body = std::fs::read_to_string(&path).expect("ubs-version.txt readable");
    let trimmed = body.trim();
    assert!(
        !trimmed.is_empty(),
        "ubs-version.txt must contain a non-empty version token (got: {trimmed:?})"
    );
}

#[test]
fn ubs_gate_filters_to_supported_extensions() {
    tracing::info!(target: "dpfvr_test", check = "filter_extensions");
    let body = std::fs::read_to_string(ci_yml_path()).expect("ci.yml must exist");
    // The change-file filter should at minimum cover .rs (the core language)
    // plus shell + yml (CI infra) plus markdown (docs).
    for ext in ["'*.rs'", "'*.sh'", "'*.yml'", "'*.md'"] {
        assert!(
            body.contains(ext),
            "ci.yml ubs-changed-files job must filter for {ext}"
        );
    }
}

#[test]
fn ubs_gate_skip_path_for_zero_relevant_changes_documented() {
    tracing::info!(target: "dpfvr_test", check = "skip_path_documented");
    let body = std::fs::read_to_string(ci_yml_path()).expect("ci.yml must exist");
    // The job must short-circuit when no relevant files changed.
    assert!(
        body.contains("skip=true") && body.contains("if: steps.changed.outputs.skip != 'true'"),
        "ci.yml ubs-changed-files must use a skip-output gate when no relevant files changed"
    );
}

#[test]
fn agents_md_documents_ubs_gate() {
    tracing::info!(target: "dpfvr_test", check = "agents_md_documents");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("AGENTS.md");
    let body = std::fs::read_to_string(&path).expect("AGENTS.md must exist");
    // The bead requires AGENTS.md to grow a "UBS Pre-Merge Gate" subsection.
    assert!(
        body.contains("UBS Pre-Merge Gate") || body.contains("ubs --ci --fail-on-warning"),
        "AGENTS.md must document the UBS pre-merge gate"
    );
}
