//! E2E coverage for `cass schedule` (background-index OS scheduler wave).
//!
//! Everything runs hermetically: a temp HOME (so provider discovery finds no
//! real sessions and no real LaunchAgents/systemd user dir is touched), a
//! temp data dir, and scrubbed CASS_* env. `install` is only exercised with
//! `--dry-run` — registering real launchd/systemd jobs from a test suite
//! would leak state onto the developer machine.

use serde_json::Value;
use std::path::Path;
use std::process::Command;

fn cass_bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_cass")
}

/// A cass invocation isolated from the developer's real environment.
fn cass_cmd(home: &Path) -> Command {
    let mut cmd = Command::new(cass_bin_path());
    cmd.env("HOME", home);
    cmd.env("TUI_HEADLESS", "1");
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1");
    // Deterministic gates: never load-gate or idle-gate a test run.
    cmd.env("CASS_RESPONSIVENESS_DISABLE", "1");
    cmd.env_remove("CASS_DATA_DIR");
    cmd.env_remove("CASS_DB_PATH");
    cmd.env_remove("XDG_CONFIG_HOME");
    cmd.env_remove("XDG_DATA_HOME");
    cmd.env_remove("CASS_AUTO_REFRESH");
    cmd.env_remove("CASS_SCHEDULE_MAX_BACKFILL_BATCHES");
    cmd.env_remove("CASS_RESPONSIVENESS_MIN_USER_IDLE_SECS");
    cmd
}

/// Seed one real Claude Code session so indexing has something to publish.
/// `cass index --full` on a machine with zero sessions fails its own
/// post-publish validation (no lexical index exists to read back), so the
/// nightly tests need a non-empty corpus; the incremental test deliberately
/// keeps an empty corpus to cover that path too.
fn seed_claude_code_fixture(home: &Path) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("claude_code_real")
        .join("projects")
        .join("-test-project")
        .join("agent-test123.jsonl");
    let dst = home
        .join(".claude")
        .join("projects")
        .join("-test-project")
        .join("agent-test123.jsonl");
    std::fs::create_dir_all(dst.parent().expect("fixture parent")).expect("create fixture dir");
    std::fs::copy(&src, &dst).expect("copy claude_code fixture");
}

fn parse_single_json_document(stdout: &[u8]) -> Value {
    let text = String::from_utf8_lossy(stdout);
    serde_json::from_str::<Value>(text.trim())
        .unwrap_or_else(|e| panic!("stdout must be exactly one JSON document ({e}); got:\n{text}"))
}

#[test]
fn schedule_install_dry_run_reports_units_without_executing_anything() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data_dir = tmp.path().join("dd");
    std::fs::create_dir_all(&home).unwrap();

    let output = cass_cmd(&home)
        .args(["schedule", "install", "--dry-run", "--json", "--data-dir"])
        .arg(&data_dir)
        .output()
        .expect("run cass schedule install --dry-run");

    if cfg!(any(target_os = "macos", target_os = "linux")) {
        assert!(
            output.status.success(),
            "dry-run install must succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report = parse_single_json_document(&output.stdout);
        assert_eq!(report["dry_run"], Value::Bool(true));
        let units = report["units"].as_array().expect("units array");
        let expected_units = if cfg!(target_os = "macos") { 2 } else { 4 };
        assert_eq!(units.len(), expected_units, "incremental + nightly units");
        for unit in units {
            let path = unit["path"].as_str().expect("unit path");
            assert!(
                path.starts_with(home.to_str().unwrap()) || path.contains("systemd"),
                "unit paths must live under the (fake) HOME: {path}"
            );
            assert!(
                !Path::new(path).exists(),
                "dry-run must not write unit files: {path}"
            );
        }
        for command in report["commands"].as_array().expect("commands array") {
            assert_eq!(
                command["executed"],
                Value::Bool(false),
                "dry-run must not execute scheduler commands: {command}"
            );
        }
        assert_eq!(
            report["stale_removed"].as_array().map(Vec::len),
            Some(0),
            "fresh dry-run install has nothing stale to remove"
        );
        // The rendered units must invoke `schedule run` against this data dir
        // at background priority.
        let rendered: String = units
            .iter()
            .filter_map(|u| u["contents"].as_str())
            .collect();
        assert!(rendered.contains("schedule"));
        assert!(rendered.contains("--job"));
        assert!(rendered.contains(data_dir.to_str().unwrap()));
    } else {
        assert!(
            !output.status.success(),
            "unsupported platforms fail closed"
        );
    }
}

#[test]
fn schedule_install_dry_run_no_nightly_registers_only_incremental() {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data_dir = tmp.path().join("dd");
    std::fs::create_dir_all(&home).unwrap();

    let output = cass_cmd(&home)
        .args([
            "schedule",
            "install",
            "--dry-run",
            "--no-nightly",
            "--json",
            "--data-dir",
        ])
        .arg(&data_dir)
        .output()
        .expect("run cass schedule install --dry-run --no-nightly");
    assert!(output.status.success());
    let report = parse_single_json_document(&output.stdout);
    let units = report["units"].as_array().expect("units array");
    let expected_units = if cfg!(target_os = "macos") { 1 } else { 2 };
    assert_eq!(units.len(), expected_units);
    for unit in units {
        assert!(
            !unit["path"]
                .as_str()
                .unwrap_or_default()
                .contains("nightly"),
            "--no-nightly must not render nightly units"
        );
    }
}

#[test]
fn schedule_run_incremental_force_records_state_and_history() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data_dir = tmp.path().join("dd");
    std::fs::create_dir_all(&home).unwrap();

    let run = |label: &str| {
        let output = cass_cmd(&home)
            .args([
                "schedule",
                "run",
                "--job",
                "incremental",
                "--force",
                "--json",
                "--data-dir",
            ])
            .arg(&data_dir)
            .output()
            .expect("run cass schedule run");
        assert!(
            output.status.success(),
            "{label}: incremental job on an empty corpus must succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        parse_single_json_document(&output.stdout)
    };

    let report = run("first");
    assert_eq!(report["job"], "incremental");
    assert_eq!(report["ok"], Value::Bool(true));
    assert!(report["skipped_reason"].is_null(), "--force bypasses gates");
    let steps = report["steps"].as_array().expect("steps");
    let index_step = steps
        .iter()
        .find(|s| s["name"] == "index")
        .expect("an index step");
    assert_eq!(index_step["ok"], Value::Bool(true));
    assert_eq!(index_step["exit_code"], Value::from(0));

    let state_path = data_dir.join("schedule").join("state.json");
    let runs_path = data_dir.join("schedule").join("runs.jsonl");
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).expect("state.json"))
            .expect("state.json parses");
    assert_eq!(state["last_incremental"]["ok"], Value::Bool(true));
    assert!(state["last_nightly"].is_null());
    let history = std::fs::read_to_string(&runs_path).expect("runs.jsonl");
    assert_eq!(history.lines().count(), 1);

    run("second");
    let history = std::fs::read_to_string(&runs_path).expect("runs.jsonl");
    assert_eq!(history.lines().count(), 2, "history is append-only");
}

#[test]
fn schedule_run_nightly_skips_semantic_tiers_it_cannot_serve() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data_dir = tmp.path().join("dd");
    std::fs::create_dir_all(&home).unwrap();
    seed_claude_code_fixture(&home);

    let output = cass_cmd(&home)
        .args([
            "schedule",
            "run",
            "--job",
            "nightly",
            "--force",
            "--json",
            "--data-dir",
        ])
        .arg(&data_dir)
        .output()
        .expect("run cass schedule run --job nightly");
    assert!(
        output.status.success(),
        "nightly on an empty corpus without models must still succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = parse_single_json_document(&output.stdout);
    assert_eq!(report["ok"], Value::Bool(true));
    let steps = report["steps"].as_array().expect("steps");
    let names: Vec<&str> = steps.iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(names.contains(&"index-full"), "steps: {names:?}");
    assert!(names.contains(&"models-status"), "steps: {names:?}");
    let quality_skip = steps
        .iter()
        .find(|s| s["name"] == "semantic-backfill:quality")
        .expect("quality tier step");
    assert!(
        quality_skip["skipped_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("models install"),
        "quality tier must be skipped (not failed) without the model: {quality_skip}"
    );
    // Every step is ok (skips count as ok); nothing failed.
    for step in steps {
        assert_eq!(step["ok"], Value::Bool(true), "step failed: {step}");
    }
}

#[test]
fn schedule_run_nightly_no_semantic_skips_backfill_entirely() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data_dir = tmp.path().join("dd");
    std::fs::create_dir_all(&home).unwrap();
    seed_claude_code_fixture(&home);

    let output = cass_cmd(&home)
        .args([
            "schedule",
            "run",
            "--job",
            "nightly",
            "--no-semantic",
            "--force",
            "--json",
            "--data-dir",
        ])
        .arg(&data_dir)
        .output()
        .expect("run cass schedule run --job nightly --no-semantic");
    assert!(output.status.success());
    let report = parse_single_json_document(&output.stdout);
    let steps = report["steps"].as_array().expect("steps");
    let backfill = steps
        .iter()
        .find(|s| s["name"] == "semantic-backfill")
        .expect("aggregate backfill skip step");
    assert!(
        backfill["skipped_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("--no-semantic")
    );
    assert!(
        !steps.iter().any(|s| s["name"] == "models-status"),
        "--no-semantic must not even probe the model"
    );
}

#[test]
fn schedule_run_json_failure_stays_a_single_document_with_exit_9() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data_dir = tmp.path().join("dd");
    std::fs::create_dir_all(&home).unwrap();

    // An impossible --db path makes the index step fail (spawnable binary,
    // unusable database), which must fail the job — but the report already
    // printed to stdout must remain the ONLY JSON document there.
    let output = cass_cmd(&home)
        .args(["--db", "/dev/null/not-a-db/agent_search.db"])
        .args([
            "schedule",
            "run",
            "--job",
            "incremental",
            "--force",
            "--json",
            "--data-dir",
        ])
        .arg(&data_dir)
        .output()
        .expect("run cass schedule run with a broken --db");
    assert_eq!(
        output.status.code(),
        Some(9),
        "failing job must exit 9: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = parse_single_json_document(&output.stdout);
    assert_eq!(report["ok"], Value::Bool(false));
    let index_step = report["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .find(|s| s["name"] == "index")
        .cloned()
        .expect("index step");
    assert_eq!(index_step["ok"], Value::Bool(false));
}

#[test]
fn schedule_status_json_reports_never_run_then_last_run() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data_dir = tmp.path().join("dd");
    std::fs::create_dir_all(&home).unwrap();

    let status = |label: &str| {
        let output = cass_cmd(&home)
            .args(["schedule", "status", "--json", "--data-dir"])
            .arg(&data_dir)
            .output()
            .expect("run cass schedule status");
        assert!(
            output.status.success(),
            "{label}: status must succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        parse_single_json_document(&output.stdout)
    };

    let before = status("before");
    assert!(before["state"]["last_incremental"].is_null());
    // With a fake HOME no units are installed.
    for unit in before["units"].as_array().unwrap_or(&Vec::new()) {
        assert_eq!(unit["installed"], Value::Bool(false));
    }

    let run_output = cass_cmd(&home)
        .args([
            "schedule",
            "run",
            "--job",
            "incremental",
            "--force",
            "--json",
            "--data-dir",
        ])
        .arg(&data_dir)
        .output()
        .expect("run incremental job");
    assert!(run_output.status.success());

    let after = status("after");
    assert_eq!(after["state"]["last_incremental"]["ok"], Value::Bool(true));
}
