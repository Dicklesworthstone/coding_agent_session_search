//! Real-binary gate for bead `doctor-check-no-progress-output-siekg`:
//! `cass doctor --check` must show signs of life on stderr while it runs
//! (it legitimately takes minutes on multi-GB/corrupt archives and used to be
//! silent until the end), and that liveness must never touch stdout.
//!
//! Timing-honest: with a 1s cadence the heartbeat only fires if the run
//! lasts ≥1s, so the positive assertion is conditional on measured wall
//! time; the negative assertions (`CASS_DOCTOR_HEARTBEAT_SECS=0` is silent,
//! stdout stays valid JSON, no heartbeat text on stdout) are unconditional.

mod util;

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;
use serde_json::Value;

use util::timeout::spawn_with_timeout_or_diag;

const HEARTBEAT_MARKER: &str = "[cass doctor --check] still running";
const STATUS_MARKER: &str = "[cass status] still running";
const TIMEOUT: Duration = Duration::from_secs(180);

struct Fixture {
    _home: tempfile::TempDir,
    home: PathBuf,
    data_dir: PathBuf,
    codex_home: PathBuf,
}

fn fixture() -> Result<Fixture, String> {
    let home = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let home_path = home.path().to_path_buf();
    let data_dir = home_path.join("cass-data");
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("data dir: {e}"))?;
    let codex_home = home_path.join(".codex");
    util::seed_codex_session(
        &codex_home,
        "rollout-2026-04-23T10-00-00-heartbeat.jsonl",
        "doctor heartbeat probe keyword",
        true,
    );
    let fixture = Fixture {
        _home: home,
        home: home_path,
        data_dir,
        codex_home,
    };
    let out = run(
        &fixture,
        "heartbeat_index",
        &["index", "--full", "--json", "--no-progress-events"],
        &[],
    );
    let value: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .map_err(|e| format!("index stdout not JSON: {e}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(format!("index failed: {value}"));
    }
    Ok(fixture)
}

fn run(fixture: &Fixture, label: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(cargo_bin("cass"));
    cmd.args(args)
        .arg("--data-dir")
        .arg(&fixture.data_dir)
        .current_dir(&fixture.home)
        .env("HOME", &fixture.home)
        .env("XDG_DATA_HOME", fixture.home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", fixture.home.join("xdg-config"))
        .env("XDG_CACHE_HOME", fixture.home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("CODEX_HOME", &fixture.codex_home)
        .env("NO_COLOR", "1")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CASS_DOCTOR_HEARTBEAT_SECS");
    for (key, value) in env {
        cmd.env(key, value);
    }
    spawn_with_timeout_or_diag(cmd, label, Some(&fixture.data_dir), TIMEOUT)
}

fn check() -> Result<(), String> {
    let fixture = fixture()?;

    // 1s cadence: liveness text on stderr iff the run outlived one interval.
    let started = Instant::now();
    let live = run(
        &fixture,
        "doctor_check_heartbeat_1s",
        &["doctor", "--check", "--json"],
        &[("CASS_DOCTOR_HEARTBEAT_SECS", "1")],
    );
    let wall = started.elapsed();
    let stdout = String::from_utf8_lossy(&live.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&live.stderr).into_owned();
    serde_json::from_str::<Value>(stdout.trim()).map_err(|e| {
        format!(
            "doctor --check stdout not JSON: {e}; head: {}",
            stdout.chars().take(300).collect::<String>()
        )
    })?;
    if stdout.contains(HEARTBEAT_MARKER) {
        return Err("heartbeat leaked onto stdout".to_string());
    }
    if wall >= Duration::from_secs(3) && !stderr.contains(HEARTBEAT_MARKER) {
        return Err(format!(
            "doctor ran {:.1}s with a 1s heartbeat but stderr carried no liveness line; stderr: {stderr}",
            wall.as_secs_f64()
        ));
    }
    if stderr.contains(HEARTBEAT_MARKER) && !stderr.contains("elapsed") {
        return Err(format!("heartbeat line lacks elapsed time: {stderr}"));
    }

    // Disabled cadence: never a heartbeat, however long the run takes.
    let silent = run(
        &fixture,
        "doctor_check_heartbeat_off",
        &["doctor", "--check", "--json"],
        &[("CASS_DOCTOR_HEARTBEAT_SECS", "0")],
    );
    let silent_err = String::from_utf8_lossy(&silent.stderr);
    if silent_err.contains(HEARTBEAT_MARKER) {
        return Err(format!(
            "CASS_DOCTOR_HEARTBEAT_SECS=0 still emitted a heartbeat: {silent_err}"
        ));
    }

    // k2k20 ask #1: `cass status` shares the heartbeat (it opens the DB and
    // was the silent surface in the 9.3GB field report). Same contract:
    // stdout stays valid JSON, liveness only on stderr, `0` is silent.
    let started = Instant::now();
    let status = run(
        &fixture,
        "status_heartbeat_1s",
        &["status", "--json"],
        &[("CASS_DOCTOR_HEARTBEAT_SECS", "1")],
    );
    let wall = started.elapsed();
    let status_out = String::from_utf8_lossy(&status.stdout).into_owned();
    let status_err = String::from_utf8_lossy(&status.stderr).into_owned();
    serde_json::from_str::<Value>(status_out.trim())
        .map_err(|e| format!("status --json stdout not JSON: {e}"))?;
    if status_out.contains(STATUS_MARKER) {
        return Err("status heartbeat leaked onto stdout".to_string());
    }
    if wall >= Duration::from_secs(3) && !status_err.contains(STATUS_MARKER) {
        return Err(format!(
            "status ran {:.1}s with a 1s heartbeat but stderr carried no liveness line; stderr: {status_err}",
            wall.as_secs_f64()
        ));
    }
    let status_silent = run(
        &fixture,
        "status_heartbeat_off",
        &["status", "--json"],
        &[("CASS_DOCTOR_HEARTBEAT_SECS", "0")],
    );
    if String::from_utf8_lossy(&status_silent.stderr).contains(STATUS_MARKER) {
        return Err("CASS_DOCTOR_HEARTBEAT_SECS=0 still emitted a status heartbeat".to_string());
    }
    Ok(())
}

#[test]
fn doctor_check_reports_liveness_on_stderr_only() -> Result<(), String> {
    check()
}
