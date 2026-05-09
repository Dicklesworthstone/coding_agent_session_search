//! Contract assertions for the TUI e2e smoke runner.
//!
//! Per `coding_agent_session_search-8m208`. The bead's substance is the
//! `scripts/tests/run_e2e_smoke.sh` runner script (added in this PR) and
//! the wiring of TestLogger into the existing 6 e2e_scenario_* tests in
//! src/ui/app.rs.
//!
//! This contract test file pins the smoke runner's surface and asserts the
//! expected scenarios are still present in src/ui/app.rs.

use std::path::PathBuf;

#[test]
fn smoke_runner_script_exists_and_is_executable() {
    tracing::info!(target: "8m208_test", check = "script_present");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("tests")
        .join("run_e2e_smoke.sh");
    assert!(path.is_file(), "scripts/tests/run_e2e_smoke.sh must exist");
    let metadata = std::fs::metadata(&path).expect("metadata");
    let permissions = metadata.permissions();
    use std::os::unix::fs::PermissionsExt;
    let mode = permissions.mode();
    assert!(
        mode & 0o111 != 0,
        "scripts/tests/run_e2e_smoke.sh must be executable; mode = {mode:o}"
    );
}

#[test]
fn smoke_runner_uses_rch_for_cargo_invocations() {
    tracing::info!(target: "8m208_test", check = "uses_rch");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("tests")
        .join("run_e2e_smoke.sh");
    let body = std::fs::read_to_string(&path).expect("readable");
    assert!(
        body.contains("rch exec --"),
        "smoke runner must rch-wrap cargo invocations per AGENTS.md"
    );
}

#[test]
fn smoke_runner_emits_structured_summary() {
    tracing::info!(target: "8m208_test", check = "structured_summary");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("tests")
        .join("run_e2e_smoke.sh");
    let body = std::fs::read_to_string(&path).expect("readable");
    // The bead's AC.2 requires `e2e_smoke: TOTAL=N PASS=M FAIL=K WALL_S=...`.
    assert!(
        body.contains("TOTAL=") && body.contains("PASS=") && body.contains("FAIL="),
        "smoke runner must emit a TOTAL/PASS/FAIL summary line"
    );
}

#[test]
fn smoke_runner_provides_failure_repro_hint() {
    tracing::info!(target: "8m208_test", check = "repro_hint");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("tests")
        .join("run_e2e_smoke.sh");
    let body = std::fs::read_to_string(&path).expect("readable");
    // On failure, the bead's AC.2 requires a reproduction one-liner.
    assert!(
        body.contains("Reproduce") || body.contains("reproduce") || body.contains("--test-threads"),
        "smoke runner must surface reproduction guidance on failure"
    );
}

#[test]
fn existing_e2e_scenarios_remain_in_source() {
    tracing::info!(target: "8m208_test", check = "scenarios_present");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("ui")
        .join("app.rs");
    let body = std::fs::read_to_string(&path).expect("readable");
    // The 6 scenarios documented at bead body. Must stay present so the
    // smoke runner has work to do.
    let expected_min_count = 5; // tolerate some renames; require at least 5
    let count = body.matches("fn e2e_scenario_").count();
    assert!(
        count >= expected_min_count,
        "src/ui/app.rs must contain at least {expected_min_count} e2e_scenario_* tests; found {count}"
    );
}
