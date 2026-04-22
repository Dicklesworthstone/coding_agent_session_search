//! Lifecycle validation matrix — concrete early rows.
//!
//! Bead `ibuuh.23` scopes out a dedicated validation matrix for long-running
//! maintenance lifecycle behavior (scheduler, cleanup, quarantine, retention,
//! multi-actor coordination). The full matrix needs scheduler / pause-resume
//! / quarantine subsystems that are multi-day scope downstream of in-flight
//! ibuuh.30 / ibuuh.32 work.
//!
//! The early rows pin prerequisites the rest of the lifecycle tail needs:
//! idempotent readiness reads under process-level concurrency, cross-surface
//! robot contract agreement, and deterministic scheduler trace artifacts.
//!
//! Later rows will need their own fixtures and cannot ship until the
//! upstream features they validate exist; see bead ibuuh.23 comments for
//! the remainder of the matrix plan.

mod util;

use assert_cmd::Command;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use util::search_asset_simulation::{
    ContentionPlan, LoadSample, LoadScript, SearchAssetSimulationHarness, SimulationActor,
};

/// Invoke `cass health --json` against an isolated data dir and return
/// scrubbed canonical JSON (identical rules to tests/golden_robot_json.rs
/// so outputs are comparable across tests and threads).
fn isolated_health_json(test_home: Arc<tempfile::TempDir>) -> String {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.args(["health", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1");
    let out = cmd.output().expect("run cass health");
    // cass health exits 1 for unhealthy — that's part of the contract.
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let canonical = serde_json::to_string_pretty(&parsed).expect("pretty");
    scrub(&canonical, test_home.path())
}

/// Scrub dynamic values. Mirrors the union of scrubs used by
/// tests/golden_robot_json.rs::scrub_robot_json. Kept local so this test
/// file is independent of the robot-json file's private helpers.
fn scrub(input: &str, test_home: &Path) -> String {
    let mut out = input.to_string();
    let crate_version_re = regex::Regex::new(r#""crate_version"\s*:\s*"[^"]*""#).unwrap();
    out = crate_version_re
        .replace_all(&out, r#""crate_version": "[VERSION]""#)
        .to_string();
    let ts_re =
        regex::Regex::new(r#"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})?"#)
            .unwrap();
    out = ts_re.replace_all(&out, "[TIMESTAMP]").to_string();
    let home_str = test_home.display().to_string();
    if !home_str.is_empty() {
        out = out.replace(&home_str, "[TEST_HOME]");
    }
    let uuid_re =
        regex::Regex::new(r#"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"#)
            .unwrap();
    out = uuid_re.replace_all(&out, "[UUID]").to_string();
    let latency_re = regex::Regex::new(r#""latency_ms"\s*:\s*\d+"#).unwrap();
    out = latency_re
        .replace_all(&out, r#""latency_ms": "[LATENCY_MS]""#)
        .to_string();
    for key in ["load_per_core", "psi_cpu_some_avg10"] {
        let re = regex::Regex::new(&format!(
            r#""{key}"\s*:\s*(-?\d+(\.\d+)?([eE][+-]?\d+)?|null)"#
        ))
        .unwrap();
        out = re
            .replace_all(&out, format!(r#""{key}": "[LIVE_METRIC]""#).as_str())
            .to_string();
    }
    out
}

#[test]
fn concurrent_health_readings_agree_on_readiness_snapshot() {
    let test_home = Arc::new(tempfile::tempdir().expect("tempdir"));
    // Spawn three concurrent cass health --json readings against the same
    // isolated home.  They must all return byte-identical scrubbed JSON:
    // the readiness snapshot has no shared writer and must not drift under
    // process-level concurrency.  If this ever fails, it signals a racy
    // read somewhere in the readiness computation — exactly the class of
    // multi-actor coordination bug the ibuuh.23 matrix exists to catch.
    let handles: Vec<_> = (0..3)
        .map(|_| {
            let home = Arc::clone(&test_home);
            thread::spawn(move || isolated_health_json(home))
        })
        .collect();

    let outputs: Vec<String> = handles
        .into_iter()
        .map(|h| h.join().expect("thread panicked"))
        .collect();

    let first = &outputs[0];
    for (i, other) in outputs.iter().enumerate().skip(1) {
        assert_eq!(
            other, first,
            "health --json output #{i} diverged from output #0 under concurrent reads"
        );
    }
}

#[test]
fn cross_surface_version_agreement() {
    // Row 2 of the matrix: cross-surface version-string invariant. The
    // string that `cass --version` prints must match the `crate_version`
    // field of `cass capabilities --json`. A drift here signals that one
    // of the two surfaces picked up a stale build-time constant — the
    // exact class of mysterious mismatch that agents and operators
    // otherwise only discover in production.
    let test_home = tempfile::tempdir().expect("tempdir");

    let version_out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["--version"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass --version");
    assert!(
        version_out.status.success(),
        "cass --version exited non-zero: {:?}",
        version_out.status
    );
    let version_stdout = String::from_utf8(version_out.stdout).expect("utf8");
    // `cass --version` emits `cass <semver>`; extract the token after the
    // first whitespace and trim any trailing newline.
    let version_flag_version = version_stdout
        .split_whitespace()
        .nth(1)
        .expect("cass --version should be `cass X.Y.Z`")
        .to_string();

    let caps_out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["capabilities", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass capabilities --json");
    assert!(
        caps_out.status.success(),
        "cass capabilities exited non-zero"
    );
    let caps_stdout = String::from_utf8(caps_out.stdout).expect("utf8");
    let caps_json: serde_json::Value = serde_json::from_str(&caps_stdout).expect("JSON");
    let caps_version = caps_json
        .get("crate_version")
        .and_then(|v| v.as_str())
        .expect("capabilities.crate_version is a string")
        .to_string();

    assert_eq!(
        version_flag_version, caps_version,
        "cass --version ({version_flag_version:?}) disagrees with capabilities.crate_version \
         ({caps_version:?}) — one surface picked up a stale build-time constant"
    );
}

#[test]
fn capabilities_surface_is_home_independent() {
    // Row 3 of the matrix: the capabilities surface is a compile-time
    // contract (feature list, connector list, limits) and MUST NOT vary
    // based on the resolved data-dir. Two independent isolated HOMEs
    // must produce byte-identical capabilities JSON.
    //
    // If a future change accidentally reads a runtime config file from
    // the data dir during capabilities resolution (e.g., "which features
    // are enabled in this workspace"), this test starts failing — surfacing
    // the leak before downstream agents see inconsistent capability views.
    fn caps_json(home: &Path) -> String {
        let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
            .args(["capabilities", "--json"])
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("XDG_DATA_HOME", home)
            .env("HOME", home)
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .output()
            .expect("run cass capabilities --json");
        assert!(
            out.status.success(),
            "cass capabilities --json exited non-zero under home {}",
            home.display(),
        );
        let stdout = String::from_utf8(out.stdout).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
        // Re-serialize for canonical comparison. The capabilities output
        // has no dynamic values outside crate_version, and crate_version
        // is a compile-time constant that's identical across tempdirs —
        // so no scrubbing is needed here.
        serde_json::to_string_pretty(&parsed).expect("pretty")
    }

    let home_a = tempfile::tempdir().expect("tempdir a");
    let home_b = tempfile::tempdir().expect("tempdir b");
    assert_ne!(
        home_a.path(),
        home_b.path(),
        "tempdir a and tempdir b must be distinct paths"
    );

    let caps_a = caps_json(home_a.path());
    let caps_b = caps_json(home_b.path());

    assert_eq!(
        caps_a, caps_b,
        "cass capabilities --json is HOME-dependent — this is a contract leak"
    );
}

#[test]
fn scheduler_pause_resume_trace_is_artifact_backed() {
    // Row 4 of the matrix: deterministic lifecycle traces must preserve
    // pause/resume ordering, the pressure reason, and artifact-backed robot
    // evidence. This is intentionally a harness-level row until the full
    // scheduler/cleanup/quarantine subsystems are complete.
    let mut harness = SearchAssetSimulationHarness::new(
        "lifecycle_matrix_pause_resume_trace",
        LoadScript::new(vec![
            LoadSample::idle("scheduler_start_idle"),
            LoadSample::busy("foreground_pressure"),
            LoadSample::idle("pressure_cleared"),
        ]),
    );

    let plan = ContentionPlan::new()
        .turn(SimulationActor::BackgroundSemantic, "start_backfill")
        .turn(SimulationActor::ForegroundSearch, "foreground_pressure")
        .turn(SimulationActor::BackgroundSemantic, "resume_backfill");

    let results =
        harness.run_contention_plan(&plan, |turn, sim| match (turn.actor, turn.label.as_str()) {
            (SimulationActor::BackgroundSemantic, "start_backfill") => {
                sim.phase("scheduler", "background backfill starts under idle budget");
                sim.snapshot_json(
                    "scheduler_start",
                    &json!({
                        "scheduler_state": "running",
                        "reason": "idle_budget_available",
                        "work": "semantic_backfill",
                        "generation_state": "current"
                    }),
                );
                Ok(())
            }
            (SimulationActor::ForegroundSearch, "foreground_pressure") => {
                sim.phase(
                    "foreground_search",
                    "foreground pressure requests scheduler yield",
                );
                sim.snapshot_json(
                    "scheduler_pause",
                    &json!({
                        "scheduler_state": "paused",
                        "reason": "foreground_pressure",
                        "yielded": true,
                        "foreground_searches": 2
                    }),
                );
                Ok(())
            }
            (SimulationActor::BackgroundSemantic, "resume_backfill") => {
                sim.phase(
                    "scheduler",
                    "background backfill resumes after pressure clears",
                );
                sim.snapshot_json(
                    "scheduler_resume",
                    &json!({
                        "scheduler_state": "running",
                        "reason": "pressure_cleared",
                        "yielded": false,
                        "work": "semantic_backfill"
                    }),
                );
                Ok(())
            }
            _ => unreachable!("unexpected deterministic lifecycle turn"),
        });

    assert!(
        results.iter().all(Result::is_ok),
        "pause/resume trace should not inject failures: {results:?}"
    );

    let summary = harness.summary();
    assert_eq!(summary.actor_traces.len(), 3);
    assert_eq!(
        summary.actor_traces[0].actor,
        SimulationActor::BackgroundSemantic
    );
    assert_eq!(summary.actor_traces[0].load.label, "scheduler_start_idle");
    assert_eq!(
        summary.actor_traces[1].actor,
        SimulationActor::ForegroundSearch
    );
    assert_eq!(summary.actor_traces[1].load.label, "foreground_pressure");
    assert!(summary.actor_traces[1].load.user_active);
    assert_eq!(
        summary.actor_traces[2].actor,
        SimulationActor::BackgroundSemantic
    );
    assert_eq!(summary.actor_traces[2].load.label, "pressure_cleared");
    assert!(!summary.actor_traces[2].load.user_active);

    for expected in [
        "001-scheduler_start.json",
        "002-scheduler_pause.json",
        "003-scheduler_resume.json",
    ] {
        assert!(
            summary.snapshot_digests.contains_key(expected),
            "missing lifecycle snapshot digest for {expected}"
        );
    }

    let artifacts = harness
        .write_artifacts()
        .expect("write lifecycle artifacts");
    assert!(artifacts.phase_log_path.exists());
    assert!(artifacts.actor_traces_path.exists());
    assert!(artifacts.summary_path.exists());

    let phase_log = std::fs::read_to_string(&artifacts.phase_log_path).expect("read phase log");
    assert!(
        phase_log.contains("foreground pressure requests scheduler yield"),
        "phase log should preserve the pause reason"
    );
    let pause_snapshot = artifacts.snapshot_dir.join("002-scheduler_pause.json");
    let pause_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&pause_snapshot).expect("read pause snapshot"),
    )
    .expect("pause snapshot JSON");
    assert_eq!(pause_json["scheduler_state"], "paused");
    assert_eq!(pause_json["reason"], "foreground_pressure");
    assert_eq!(pause_json["yielded"], true);
}
