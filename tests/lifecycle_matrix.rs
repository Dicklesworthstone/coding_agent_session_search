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

#[test]
fn cleanup_quarantine_inventory_trace_is_artifact_backed() {
    // Row 5 of the matrix: cleanup/quarantine proof must preserve a
    // machine-readable inventory, quarantine reason, pause reason, and dry-run
    // reclamation verdict. This stays harness-level until the full cleanup
    // worker is unblocked, but it freezes the evidence format the worker must
    // emit.
    let mut harness = SearchAssetSimulationHarness::new(
        "lifecycle_matrix_cleanup_quarantine_inventory",
        LoadScript::new(vec![
            LoadSample::idle("cleanup_inventory"),
            LoadSample::idle("quarantine_detected"),
            LoadSample::busy("foreground_pressure"),
            LoadSample::idle("cleanup_resume"),
        ]),
    );

    let plan = ContentionPlan::new()
        .turn(SimulationActor::LexicalRepair, "inventory")
        .turn(SimulationActor::LexicalRepair, "quarantine")
        .turn(SimulationActor::ForegroundSearch, "pause_cleanup")
        .turn(SimulationActor::LexicalRepair, "dry_run_resume");

    let results =
        harness.run_contention_plan(&plan, |turn, sim| match (turn.actor, turn.label.as_str()) {
            (SimulationActor::LexicalRepair, "inventory") => {
                sim.phase(
                    "cleanup",
                    "capture derivative asset inventory before cleanup decision",
                );
                sim.snapshot_json(
                    "cleanup_inventory_before",
                    &json!({
                        "current_generation": "lexical-gen-004",
                        "superseded_generations": ["lexical-gen-002", "lexical-gen-003"],
                        "quarantine_candidates": ["lexical-gen-003/shard-0002"],
                        "published_generation_available": true,
                        "dry_run": true
                    }),
                );
                Ok(())
            }
            (SimulationActor::LexicalRepair, "quarantine") => {
                sim.phase(
                    "cleanup",
                    "quarantine corrupt superseded shard and keep it out of pruning",
                );
                sim.snapshot_json(
                    "cleanup_quarantine",
                    &json!({
                        "generation": "lexical-gen-003",
                        "shard": "shard-0002",
                        "state": "quarantined",
                        "reason": "manifest_checksum_mismatch",
                        "reclaimable": false,
                        "published_generation_available": true
                    }),
                );
                Ok(())
            }
            (SimulationActor::ForegroundSearch, "pause_cleanup") => {
                sim.phase(
                    "foreground_search",
                    "foreground pressure pauses cleanup before reclaiming superseded assets",
                );
                sim.snapshot_json(
                    "cleanup_paused",
                    &json!({
                        "cleanup_state": "paused",
                        "reason": "foreground_pressure",
                        "published_generation_available": true,
                        "reclaim_started": false
                    }),
                );
                Ok(())
            }
            (SimulationActor::LexicalRepair, "dry_run_resume") => {
                sim.phase(
                    "cleanup",
                    "cleanup resumes as dry-run and reports retained versus reclaimable bytes",
                );
                sim.snapshot_json(
                    "cleanup_resume_preview",
                    &json!({
                        "cleanup_state": "dry_run_complete",
                        "retained_quarantined_bytes": 4096,
                        "reclaimable_superseded_bytes": 16384,
                        "would_prune": ["lexical-gen-002"],
                        "would_retain": ["lexical-gen-003/shard-0002"],
                        "published_generation_available": true
                    }),
                );
                Ok(())
            }
            _ => unreachable!("unexpected deterministic cleanup turn"),
        });

    assert!(
        results.iter().all(Result::is_ok),
        "cleanup/quarantine trace should not inject failures: {results:?}"
    );

    let summary = harness.summary();
    assert_eq!(summary.actor_traces.len(), 4);
    assert_eq!(summary.actor_traces[0].load.label, "cleanup_inventory");
    assert_eq!(summary.actor_traces[1].load.label, "quarantine_detected");
    assert_eq!(summary.actor_traces[2].load.label, "foreground_pressure");
    assert!(summary.actor_traces[2].load.user_active);
    assert_eq!(summary.actor_traces[3].load.label, "cleanup_resume");

    for expected in [
        "001-cleanup_inventory_before.json",
        "002-cleanup_quarantine.json",
        "003-cleanup_paused.json",
        "004-cleanup_resume_preview.json",
    ] {
        assert!(
            summary.snapshot_digests.contains_key(expected),
            "missing cleanup snapshot digest for {expected}"
        );
    }

    let artifacts = harness.write_artifacts().expect("write cleanup artifacts");
    assert!(artifacts.phase_log_path.exists());
    assert!(artifacts.actor_traces_path.exists());
    assert!(artifacts.summary_path.exists());

    let phase_log = std::fs::read_to_string(&artifacts.phase_log_path).expect("read phase log");
    assert!(
        phase_log.contains("quarantine corrupt superseded shard"),
        "phase log should preserve quarantine context"
    );
    assert!(
        phase_log.contains("foreground pressure pauses cleanup"),
        "phase log should preserve cleanup pause context"
    );

    let quarantine_path = artifacts.snapshot_dir.join("002-cleanup_quarantine.json");
    let quarantine_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&quarantine_path).expect("read quarantine snapshot"),
    )
    .expect("quarantine snapshot JSON");
    assert_eq!(quarantine_json["state"], "quarantined");
    assert_eq!(quarantine_json["reason"], "manifest_checksum_mismatch");
    assert_eq!(quarantine_json["reclaimable"], false);

    let preview_path = artifacts
        .snapshot_dir
        .join("004-cleanup_resume_preview.json");
    let preview_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&preview_path).expect("read cleanup preview"),
    )
    .expect("cleanup preview JSON");
    assert_eq!(preview_json["cleanup_state"], "dry_run_complete");
    assert_eq!(preview_json["published_generation_available"], true);
    assert_eq!(preview_json["would_prune"][0], "lexical-gen-002");
    assert_eq!(
        preview_json["would_retain"][0],
        "lexical-gen-003/shard-0002"
    );
}

#[test]
fn derivative_retention_dry_run_keeps_protected_assets_out_of_reclaim_plan() {
    // Bead ibuuh.19 slice: cleanup must prove its inventory and dry-run
    // decisions before any destructive reclaim step. This row freezes the
    // protection set that the real worker must honor: canonical DB,
    // published generation, pinned semantic/model assets, quarantines, and
    // active scratch work are retained; only safely superseded derivatives are
    // reclaimable.
    let mut harness = SearchAssetSimulationHarness::new(
        "lifecycle_matrix_derivative_retention_dry_run",
        LoadScript::new(vec![
            LoadSample::idle("inventory_scan"),
            LoadSample::idle("policy_classification"),
            LoadSample::idle("dry_run_reclaim_plan"),
        ]),
    );

    let plan = ContentionPlan::new()
        .turn(SimulationActor::LexicalRepair, "scan_derivatives")
        .turn(SimulationActor::LexicalRepair, "classify_retention")
        .turn(SimulationActor::LexicalRepair, "dry_run_plan");

    let results =
        harness.run_contention_plan(&plan, |turn, sim| match (turn.actor, turn.label.as_str()) {
            (SimulationActor::LexicalRepair, "scan_derivatives") => {
                sim.phase(
                    "cleanup",
                    "scan derivative assets without deleting canonical or active files",
                );
                sim.snapshot_json(
                    "derivative_inventory",
                    &json!({
                        "canonical_db": {
                            "path": "agent_search.db",
                            "state": "canonical",
                            "protected": true,
                            "reclaimable": false
                        },
                        "lexical_generations": [
                            {"id": "lexical-gen-010", "state": "published", "bytes": 32768},
                            {"id": "lexical-gen-009", "state": "superseded", "bytes": 16384},
                            {"id": "lexical-gen-008", "state": "quarantined", "bytes": 8192},
                            {"id": "lexical-gen-scratch-011", "state": "active_scratch", "bytes": 4096}
                        ],
                        "semantic_assets": [
                            {"id": "semantic-fast-current", "state": "pinned", "bytes": 8192},
                            {"id": "semantic-quality-old", "state": "superseded", "bytes": 4096}
                        ],
                        "model_caches": [
                            {"id": "fastembed-default", "state": "pinned", "bytes": 65536},
                            {"id": "fastembed-old", "state": "stale_optional", "bytes": 32768}
                        ],
                        "dry_run": true
                    }),
                );
                Ok(())
            }
            (SimulationActor::LexicalRepair, "classify_retention") => {
                sim.phase(
                    "cleanup",
                    "classify retention states before building reclaim plan",
                );
                sim.snapshot_json(
                    "retention_classification",
                    &json!({
                        "retained": [
                            {
                                "id": "agent_search.db",
                                "state": "canonical",
                                "reason": "canonical_sqlite_source_of_truth"
                            },
                            {
                                "id": "lexical-gen-010",
                                "state": "current_published",
                                "reason": "published_lexical_generation"
                            },
                            {
                                "id": "lexical-gen-008",
                                "state": "quarantined",
                                "reason": "operator_inspection_required"
                            },
                            {
                                "id": "lexical-gen-scratch-011",
                                "state": "active_scratch",
                                "reason": "active_or_resumable_work"
                            },
                            {
                                "id": "semantic-fast-current",
                                "state": "pinned",
                                "reason": "current_semantic_fast_tier"
                            },
                            {
                                "id": "fastembed-default",
                                "state": "pinned",
                                "reason": "current_model_cache"
                            }
                        ],
                        "reclaimable": [
                            {
                                "id": "lexical-gen-009",
                                "state": "superseded",
                                "bytes": 16384,
                                "reason": "outside_retention_window"
                            },
                            {
                                "id": "semantic-quality-old",
                                "state": "superseded",
                                "bytes": 4096,
                                "reason": "newer_quality_generation_available"
                            },
                            {
                                "id": "fastembed-old",
                                "state": "stale_optional",
                                "bytes": 32768,
                                "reason": "optional_model_cache_budget"
                            }
                        ]
                    }),
                );
                Ok(())
            }
            (SimulationActor::LexicalRepair, "dry_run_plan") => {
                sim.phase(
                    "cleanup",
                    "emit dry-run reclaim plan with protected assets excluded",
                );
                sim.snapshot_json(
                    "retention_dry_run_plan",
                    &json!({
                        "cleanup_state": "dry_run_complete",
                        "reclaim_started": false,
                        "would_prune": [
                            "lexical-gen-009",
                            "semantic-quality-old",
                            "fastembed-old"
                        ],
                        "would_retain": [
                            "agent_search.db",
                            "lexical-gen-010",
                            "lexical-gen-008",
                            "lexical-gen-scratch-011",
                            "semantic-fast-current",
                            "fastembed-default"
                        ],
                        "reclaimable_bytes": 53248,
                        "retained_bytes": 118784,
                        "published_generation_available": true,
                        "canonical_db_protected": true
                    }),
                );
                Ok(())
            }
            _ => unreachable!("unexpected deterministic retention turn"),
        });

    assert!(
        results.iter().all(Result::is_ok),
        "retention dry-run trace should not inject failures: {results:?}"
    );

    let summary = harness.summary();
    assert_eq!(summary.actor_traces.len(), 3);
    assert_eq!(summary.actor_traces[0].load.label, "inventory_scan");
    assert_eq!(summary.actor_traces[1].load.label, "policy_classification");
    assert_eq!(summary.actor_traces[2].load.label, "dry_run_reclaim_plan");

    for expected in [
        "001-derivative_inventory.json",
        "002-retention_classification.json",
        "003-retention_dry_run_plan.json",
    ] {
        assert!(
            summary.snapshot_digests.contains_key(expected),
            "missing retention dry-run snapshot digest for {expected}"
        );
    }

    let artifacts = harness
        .write_artifacts()
        .expect("write retention dry-run artifacts");
    assert!(artifacts.phase_log_path.exists());
    assert!(artifacts.actor_traces_path.exists());
    assert!(artifacts.summary_path.exists());

    let plan_path = artifacts
        .snapshot_dir
        .join("003-retention_dry_run_plan.json");
    let plan_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&plan_path).expect("read retention dry-run plan"),
    )
    .expect("retention dry-run plan JSON");

    let would_prune = plan_json["would_prune"]
        .as_array()
        .expect("would_prune is an array");
    let would_retain = plan_json["would_retain"]
        .as_array()
        .expect("would_retain is an array");

    for protected in [
        "agent_search.db",
        "lexical-gen-010",
        "lexical-gen-008",
        "lexical-gen-scratch-011",
        "semantic-fast-current",
        "fastembed-default",
    ] {
        assert!(
            would_retain
                .iter()
                .any(|item| item.as_str() == Some(protected)),
            "protected asset {protected} must appear in would_retain"
        );
        assert!(
            would_prune
                .iter()
                .all(|item| item.as_str() != Some(protected)),
            "protected asset {protected} must not appear in would_prune"
        );
    }

    assert_eq!(plan_json["cleanup_state"], "dry_run_complete");
    assert_eq!(plan_json["reclaim_started"], false);
    assert_eq!(plan_json["canonical_db_protected"], true);
    assert_eq!(plan_json["published_generation_available"], true);
    assert_eq!(plan_json["reclaimable_bytes"], 53248);
}

#[test]
fn api_and_contract_versions_agree_across_capabilities_and_api_version() {
    // Cross-surface invariant: cass ships TWO places where an agent can
    // ask "what api + contract version am I talking to" — the full
    // capabilities block and the dedicated api-version command. Both
    // must agree on api_version AND contract_version. A silent bump in
    // one surface without the other breaks agents that negotiate via
    // the short command and then rely on the capabilities contract.
    let test_home = tempfile::tempdir().expect("tempdir");
    fn json_out(home: &Path, args: &[&str]) -> serde_json::Value {
        let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
            .args(args)
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("XDG_DATA_HOME", home)
            .env("HOME", home)
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .output()
            .expect("run cass");
        assert!(out.status.success(), "cass {args:?} exited non-zero");
        let stdout = String::from_utf8(out.stdout).expect("utf8");
        serde_json::from_str(&stdout).expect("valid JSON")
    }
    let caps = json_out(test_home.path(), &["capabilities", "--json"]);
    let api = json_out(test_home.path(), &["api-version", "--json"]);

    // Both surfaces emit integer api_version + string contract_version.
    // Pull them out and compare.
    assert_eq!(
        caps["api_version"], api["api_version"],
        "capabilities.api_version ({}) disagrees with api-version.api_version ({})",
        caps["api_version"], api["api_version"],
    );
    assert_eq!(
        caps["contract_version"], api["contract_version"],
        "capabilities.contract_version ({}) disagrees with api-version.contract_version ({})",
        caps["contract_version"], api["contract_version"],
    );
    assert_eq!(
        caps["crate_version"], api["crate_version"],
        "capabilities.crate_version ({}) disagrees with api-version.crate_version ({})",
        caps["crate_version"], api["crate_version"],
    );
}

#[test]
fn capabilities_and_diag_connectors_enumerate_the_same_set() {
    // Cross-surface invariant: cass capabilities --json exposes a
    // `connectors` string-array listing every connector cass can scan;
    // cass diag --json exposes a `connectors` object-array with
    // per-connector detection status. Both enumerate the same underlying
    // connector registry. A drift — e.g. a newly-added connector that
    // lands in capabilities but not in diag, or vice versa — is a real
    // contract bug: agents that discover capabilities and then call diag
    // to plan ingestion will silently skip the mismatched connector.
    let test_home = tempfile::tempdir().expect("tempdir");
    fn json_out(home: &Path, args: &[&str]) -> serde_json::Value {
        let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
            .args(args)
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("XDG_DATA_HOME", home)
            .env("HOME", home)
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .output()
            .expect("run cass");
        assert!(out.status.success(), "cass {args:?} exited non-zero");
        let stdout = String::from_utf8(out.stdout).expect("utf8");
        serde_json::from_str(&stdout).expect("valid JSON")
    }

    let caps = json_out(test_home.path(), &["capabilities", "--json"]);
    let diag = json_out(test_home.path(), &["diag", "--json"]);

    let mut caps_names: Vec<String> = caps["connectors"]
        .as_array()
        .expect("capabilities.connectors is an array")
        .iter()
        .map(|v| v.as_str().expect("connector name is string").to_string())
        .collect();
    let mut diag_names: Vec<String> = diag["connectors"]
        .as_array()
        .expect("diag.connectors is an array")
        .iter()
        .map(|entry| {
            entry["name"]
                .as_str()
                .expect("diag.connectors[].name is string")
                .to_string()
        })
        .collect();
    caps_names.sort();
    diag_names.sort();

    assert_eq!(
        caps_names, diag_names,
        "capabilities.connectors and diag.connectors enumerate different sets — \
         a connector landed in one surface but not the other"
    );
}

#[test]
fn health_and_diag_agree_on_db_and_index_presence() {
    // Cross-surface invariant: cass health --json and cass diag --json
    // both report whether the DB and lexical index are present on disk.
    // When a fresh isolated HOME has neither, both surfaces MUST report
    // exists=false in their respective fields. If the two surfaces
    // disagree, one of them is reading stale or cached state — a class
    // of bug that otherwise only surfaces after operators run
    // contradictory diagnostic commands and can't tell which to trust.
    let test_home = tempfile::tempdir().expect("tempdir");

    fn cass_stdout_json(
        home: &Path,
        args: &[&str],
    ) -> (serde_json::Value, std::process::ExitStatus) {
        let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
            .args(args)
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("XDG_DATA_HOME", home)
            .env("HOME", home)
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .output()
            .expect("run cass");
        let stdout = String::from_utf8(out.stdout).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
        (parsed, out.status)
    }

    let (health, _) = cass_stdout_json(test_home.path(), &["health", "--json"]);
    let (diag, diag_status) = cass_stdout_json(test_home.path(), &["diag", "--json"]);
    assert!(diag_status.success(), "cass diag --json must succeed");

    let health_db_exists = health["db"]["exists"]
        .as_bool()
        .expect("health.db.exists is bool");
    let diag_db_exists = diag["database"]["exists"]
        .as_bool()
        .expect("diag.database.exists is bool");
    assert_eq!(
        health_db_exists, diag_db_exists,
        "health.db.exists ({health_db_exists}) disagrees with diag.database.exists ({diag_db_exists})"
    );

    let health_index_exists = health["state"]["index"]["exists"]
        .as_bool()
        .expect("health.state.index.exists is bool");
    let diag_index_exists = diag["index"]["exists"]
        .as_bool()
        .expect("diag.index.exists is bool");
    assert_eq!(
        health_index_exists, diag_index_exists,
        "health.state.index.exists ({health_index_exists}) disagrees with diag.index.exists ({diag_index_exists})"
    );

    // In the isolated-empty-HOME shape both surfaces must report false
    // (the DB/index genuinely do not exist on disk).
    assert!(
        !health_db_exists && !health_index_exists,
        "isolated empty HOME should report DB and index as absent; got db={health_db_exists}, index={health_index_exists}"
    );
}

#[test]
fn health_status_and_healthy_flag_are_internally_consistent() {
    // Internal-consistency row of the lifecycle matrix: within a single
    // `cass health --json` payload the three top-level fields
    // (status/healthy/initialized) MUST agree according to the robot-mode
    // contract. A silent drift where e.g. status="healthy" but
    // healthy=false breaks every agent branching on either field alone.
    //
    // Documented contract (from run_health / robot-docs):
    //   healthy == true  <=> status is a "healthy/ok"-family string
    //   initialized == false => status == "not_initialized" (and healthy=false)
    //   healthy == false requires a non-empty errors array OR non-healthy status
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["health", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass health --json");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let health: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    let status = health["status"].as_str().expect("status is string");
    let healthy = health["healthy"].as_bool().expect("healthy is bool");
    let initialized = health["initialized"]
        .as_bool()
        .expect("initialized is bool");
    let errors_len = health["errors"].as_array().expect("errors is array").len();

    if !initialized {
        assert_eq!(
            status, "not_initialized",
            "initialized=false but status is {status:?} (expected \"not_initialized\")"
        );
        assert!(
            !healthy,
            "initialized=false but healthy=true — impossible per robot-mode contract"
        );
    }

    let healthy_family = matches!(status, "healthy" | "ok");
    assert_eq!(
        healthy_family,
        healthy,
        "status={status:?} and healthy={healthy} — status is {} a healthy-family string but healthy is {healthy}",
        if healthy_family { "" } else { "not" }
    );

    if !healthy {
        assert!(
            errors_len > 0 || status != "healthy",
            "healthy=false but status={status:?} with empty errors array — no explanation surface"
        );
    }
}

#[test]
fn health_and_status_agree_on_readiness_contract() {
    // Cross-surface row: `cass health --json` is the fast preflight
    // surface, while `cass status --json` is the richer operator surface.
    // For an isolated HOME, both must agree on readiness booleans and the
    // basic artifact-presence facts that agents branch on before search.
    let test_home = tempfile::tempdir().expect("tempdir");

    fn json_out(home: &Path, args: &[&str]) -> serde_json::Value {
        let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
            .args(args)
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("XDG_DATA_HOME", home)
            .env("HOME", home)
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .output()
            .expect("run cass");
        let stdout = String::from_utf8(out.stdout).expect("utf8");
        serde_json::from_str(&stdout).expect("valid JSON")
    }

    let health = json_out(test_home.path(), &["health", "--json"]);
    let status = json_out(test_home.path(), &["status", "--json"]);

    assert_eq!(
        health["initialized"], status["initialized"],
        "health.initialized and status.initialized diverged"
    );
    assert_eq!(
        health["healthy"], status["healthy"],
        "health.healthy and status.healthy diverged"
    );
    assert_eq!(
        health["db"]["exists"], status["database"]["exists"],
        "health.db.exists and status.database.exists diverged"
    );
    assert_eq!(
        health["state"]["index"]["exists"], status["index"]["exists"],
        "health.state.index.exists and status.index.exists diverged"
    );
    assert_eq!(
        health["recommended_action"], status["recommended_action"],
        "health.recommended_action and status.recommended_action diverged"
    );
}

#[test]
fn health_and_status_agree_on_semantic_fallback_state() {
    // Cross-surface row: health nests semantic readiness under
    // state.semantic, while status promotes the same object to top-level
    // semantic. When semantic assets are absent, both surfaces must tell
    // agents the same fail-open story before they choose whether to wait
    // for semantic refinement or continue with lexical-only results.
    let test_home = tempfile::tempdir().expect("tempdir");

    fn json_out(home: &Path, args: &[&str]) -> serde_json::Value {
        let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
            .args(args)
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("XDG_DATA_HOME", home)
            .env("HOME", home)
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .output()
            .expect("run cass");
        let stdout = String::from_utf8(out.stdout).expect("utf8");
        serde_json::from_str(&stdout).expect("valid JSON")
    }

    let health = json_out(test_home.path(), &["health", "--json"]);
    let status = json_out(test_home.path(), &["status", "--json"]);
    let health_semantic = &health["state"]["semantic"];
    let status_semantic = &status["semantic"];

    for key in ["available", "can_search", "fallback_mode", "status", "hint"] {
        assert_eq!(
            health_semantic[key], status_semantic[key],
            "health.state.semantic.{key} and status.semantic.{key} diverged"
        );
    }
    assert_eq!(
        health_semantic["fallback_mode"], "lexical",
        "semantic fallback must remain lexical when assets are absent"
    );
}

#[test]
fn semantic_readiness_reports_lexical_fallback_when_models_absent() {
    // ibuuh.11 contract row: 'Bootstrap semantic assets and verify live
    // default-hybrid behavior'. The core fail-open contract: when the
    // semantic model is NOT installed (isolated empty HOME), cass health
    // --json must report state.semantic as available=false with
    // fallback_mode="lexical". Agents decide whether to wait for
    // semantic or proceed with lexical based on this signal; silent
    // drift breaks every hybrid-preferred flow.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["health", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass health --json");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let health: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    let sem = &health["state"]["semantic"];
    assert!(
        sem.is_object(),
        "health.state.semantic must be an object; got {sem:?}"
    );

    let available = sem["available"].as_bool().expect("available is bool");
    let can_search = sem["can_search"].as_bool().expect("can_search is bool");
    let fallback = sem["fallback_mode"]
        .as_str()
        .expect("fallback_mode is string");
    let status = sem["status"].as_str().expect("semantic.status is string");

    // With an empty HOME the semantic model CANNOT be available.
    assert!(
        !available,
        "isolated empty HOME: semantic.available must be false; got true with status={status:?}"
    );
    assert!(
        !can_search,
        "isolated empty HOME: semantic.can_search must be false; got true with status={status:?}"
    );
    // The fail-open contract: fallback_mode MUST be lexical (not e.g.
    // empty or an unhelpful placeholder) so agents know search still
    // works via the lexical tier.
    assert_eq!(
        fallback, "lexical",
        "semantic.fallback_mode must be \"lexical\" when model is absent; got {fallback:?}"
    );
    // And there MUST be an operator-facing hint explaining what to do
    // (install the model, or proceed with lexical).
    let hint = sem["hint"].as_str().expect("semantic.hint is a string");
    assert!(
        !hint.is_empty(),
        "semantic.hint must be a non-empty user-facing guidance string"
    );
}

#[test]
fn diag_reports_zero_sizes_for_absent_db_and_index() {
    // ibuuh.19 retention-invariant row: when `cass diag --json` reports
    // database/index as absent on a fresh isolated HOME, their
    // size_bytes MUST be 0. A retention/quarantine bug where cached
    // size from a prior run leaks into a fresh HOME would manifest
    // here; this test pins the expected "clean slate = zero bytes"
    // invariant so regressions fail CI instead of silently
    // misreporting disk usage to operators.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["diag", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass diag --json");
    assert!(out.status.success(), "cass diag --json exited non-zero");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let diag: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    let db_exists = diag["database"]["exists"]
        .as_bool()
        .expect("database.exists is bool");
    let db_size = diag["database"]["size_bytes"]
        .as_u64()
        .expect("database.size_bytes is unsigned int");
    let index_exists = diag["index"]["exists"]
        .as_bool()
        .expect("index.exists is bool");
    let index_size = diag["index"]["size_bytes"]
        .as_u64()
        .expect("index.size_bytes is unsigned int");

    // Fresh isolated HOME: neither artifact should exist.
    assert!(!db_exists, "fresh HOME: database.exists must be false");
    assert!(!index_exists, "fresh HOME: index.exists must be false");

    // And the retention invariant: absent => zero bytes reported.
    assert_eq!(
        db_size, 0,
        "database.exists=false but database.size_bytes={db_size} — retention/cache leak"
    );
    assert_eq!(
        index_size, 0,
        "index.exists=false but index.size_bytes={index_size} — retention/cache leak"
    );

    // Bonus: database.conversations / database.messages must also read
    // as 0 (or null-absent), not inherit stale counts from elsewhere.
    let conversations = diag["database"]["conversations"].as_u64().unwrap_or(0);
    let messages = diag["database"]["messages"].as_u64().unwrap_or(0);
    assert_eq!(
        conversations, 0,
        "database absent but conversations={conversations}"
    );
    assert_eq!(messages, 0, "database absent but messages={messages}");
}

#[test]
fn concurrent_diag_readings_agree_on_inventory_snapshot() {
    // Parallel to concurrent_health_readings_agree_on_readiness_snapshot
    // but for the diag surface. cass diag --json reports version,
    // platform, paths, database/index inventory, and per-connector
    // detection. Under process-level concurrency three invocations
    // against the same isolated HOME MUST return byte-identical output
    // after scrubbing — any drift signals a racy read in the inventory
    // computation (e.g. a stat() call that races connector detection).
    let test_home = Arc::new(tempfile::tempdir().expect("tempdir"));

    fn isolated_diag(home: Arc<tempfile::TempDir>) -> String {
        let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
            .args(["diag", "--json"])
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("XDG_DATA_HOME", home.path())
            .env("HOME", home.path())
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .output()
            .expect("run cass diag --json");
        assert!(out.status.success(), "cass diag --json exited non-zero");
        let stdout = String::from_utf8(out.stdout).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
        let canonical = serde_json::to_string_pretty(&parsed).expect("pretty");
        scrub(&canonical, home.path())
    }

    let handles: Vec<_> = (0..3)
        .map(|_| {
            let home = Arc::clone(&test_home);
            thread::spawn(move || isolated_diag(home))
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
            "diag --json output #{i} diverged from output #0 under concurrent reads"
        );
    }
}

#[test]
fn concurrent_introspect_readings_agree_after_btreemap_fix() {
    // Regression gate for bead 8sl73 (fixed in commit 6a5f159b). The
    // introspect schema registry used to be std::collections::HashMap,
    // which iterates in random order per-run — two back-to-back
    // invocations produced byte-different response_schemas blocks and
    // broke every downstream typed-client generator. After the fix to
    // BTreeMap (deterministic sorted iteration), independent runs must
    // produce byte-identical output.
    //
    // This row spawns three concurrent cass introspect --json invocations
    // against the same isolated HOME. If any of them drift in future (or
    // the HashMap regression is reintroduced), this fails the build
    // immediately.
    let test_home = Arc::new(tempfile::tempdir().expect("tempdir"));

    fn isolated_introspect(home: Arc<tempfile::TempDir>) -> String {
        let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
            .args(["introspect", "--json"])
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("XDG_DATA_HOME", home.path())
            .env("HOME", home.path())
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .output()
            .expect("run cass introspect --json");
        assert!(
            out.status.success(),
            "cass introspect --json exited non-zero"
        );
        let stdout = String::from_utf8(out.stdout).expect("utf8");
        // Parse-and-reserialize canonicalizes whitespace; scrub paths for
        // host independence.  Any remaining drift means the registry is
        // non-deterministic again.
        let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
        let canonical = serde_json::to_string_pretty(&parsed).expect("pretty");
        scrub(&canonical, home.path())
    }

    let handles: Vec<_> = (0..3)
        .map(|_| {
            let home = Arc::clone(&test_home);
            thread::spawn(move || isolated_introspect(home))
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
            "introspect --json output #{i} diverged from output #0 — \
             HashMap/registry non-determinism may have regressed (bead 8sl73)"
        );
    }
}

#[test]
fn capabilities_features_and_connectors_contain_no_duplicates() {
    // Registry-invariant row: cass capabilities --json enumerates the
    // feature set and the connector set as string arrays. Each entry must
    // be unique — a duplicate signals double-registration (e.g. a feature
    // flag accidentally inserted twice during refactor, or a connector
    // registered in two modules). Downstream agents dedupe by hashing
    // into sets, so a duplicate silently skews feature-count metrics and
    // can mask an unregistered dependency.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["capabilities", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass capabilities --json");
    assert!(
        out.status.success(),
        "cass capabilities --json exited non-zero"
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let caps: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    for field in ["features", "connectors"] {
        let arr = caps[field]
            .as_array()
            .expect("capabilities field must be an array");
        let names: Vec<&str> = arr
            .iter()
            .map(|v| v.as_str().expect("capability entries must be strings"))
            .collect();
        let unique: std::collections::BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(
            names.len(),
            unique.len(),
            "capabilities.{field} contains duplicate entries: {names:?} vs unique {unique:?}"
        );
        assert!(
            names.len() > 0,
            "capabilities.{field} must not be empty — sanity check"
        );
    }

    // Bonus invariant: limits is an object with the four documented
    // integer fields, each non-negative.
    let limits = &caps["limits"];
    for key in [
        "max_limit",
        "max_content_length",
        "max_fields",
        "max_agg_buckets",
    ] {
        let n = limits[key]
            .as_i64()
            .expect("limits field must be an integer");
        assert!(n >= 0, "limits.{key} must be non-negative; got {n}");
    }
}

#[test]
fn semantic_readiness_block_has_expected_shape() {
    // ibuuh.11 shape-contract row: the `state.semantic` block in
    // `cass health --json` is a stable LLM-contract surface that agents
    // parse to decide whether to wait for semantic catch-up, proceed
    // with lexical-only, or prompt the operator. This test asserts each
    // documented field is present with the expected type; a silent
    // field rename like fallback_mode to fallback would degrade every
    // agent's hybrid-planning branch without necessarily breaking the
    // wider health golden.
    //
    // Separate-from-golden shape assertions catch the REAL intent
    // (contract preservation) while leaving the golden free to change
    // for cosmetic reasons like new added fields.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["health", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass health --json");
    assert!(
        matches!(out.status.code(), Some(0 | 1)),
        "cass health --json exited with unexpected code {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let health: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let sem = &health["state"]["semantic"];
    assert!(sem.is_object(), "state.semantic must be an object");

    // String-valued fields that must always be present.
    for key in [
        "status",
        "availability",
        "summary",
        "fallback_mode",
        "preferred_backend",
        "hint",
    ] {
        assert!(
            sem[key].is_string(),
            "state.semantic.{key} must be a string; got {:?}",
            sem[key]
        );
    }

    // Bool-valued fields.
    for key in ["available", "can_search", "hnsw_ready", "progressive_ready"] {
        assert!(
            sem[key].is_boolean(),
            "state.semantic.{key} must be a bool; got {:?}",
            sem[key]
        );
    }

    // Nullable-path fields (must exist as either a string or null,
    // present in every readiness payload regardless of install state).
    for key in ["embedder_id", "vector_index_path", "model_dir", "hnsw_path"] {
        let v = &sem[key];
        assert!(
            v.is_string() || v.is_null(),
            "state.semantic.{key} must be string or null; got {v:?}"
        );
    }
}

#[test]
fn index_readiness_exposes_stale_refresh_config() {
    // ibuuh.24 stale-refresh row: the world-class stale-refresh
    // architecture depends on agents being able to read the stale
    // threshold from cass health so they can reason about when a
    // refresh is warranted vs imminent vs overdue. A drift that drops
    // stale_threshold_seconds from the contract would force agents to
    // guess the threshold and either over-refresh (machine load) or
    // under-refresh (stale data).
    //
    // This row asserts the index.* sub-block has the stale-refresh
    // config surface that ibuuh.24's "explain stale-refresh timing"
    // requirement relies on, with sane default bounds.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["health", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass health --json");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let health: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let idx = &health["state"]["index"];
    assert!(idx.is_object(), "state.index must be an object");

    // The stale-refresh knob — must be a positive integer, bounded by
    // sane defaults (5 minutes min, 1 day max — catches flipped-sign or
    // unit-confusion bugs like milliseconds misread as seconds).
    let stale = idx["stale_threshold_seconds"]
        .as_i64()
        .expect("state.index.stale_threshold_seconds must be an integer");
    assert!(
        stale >= 60 && stale <= 86_400,
        "stale_threshold_seconds={stale} is outside sane bounds [60, 86400]"
    );

    // Bool-typed flags the stale-refresh planner branches on.
    for key in ["fresh", "stale", "exists", "rebuilding"] {
        assert!(
            idx[key].is_boolean(),
            "state.index.{key} must be a bool; got {:?}",
            idx[key]
        );
    }

    // status is the authoritative stale/fresh classification that
    // agents key on.  Always present, always a string.
    let status = idx["status"]
        .as_str()
        .expect("state.index.status must be a string");
    assert!(
        matches!(
            status,
            "missing" | "fresh" | "stale" | "rebuilding" | "unknown"
        ),
        "state.index.status={status:?} is outside the documented enum"
    );
}

#[test]
fn diag_artifact_paths_nest_inside_data_dir_for_safe_gc() {
    // ibuuh.19 retention-safety row: derivative asset retention /
    // quarantine / garbage-collection can only operate safely if every
    // cass-managed artifact path lives inside the declared data_dir.
    // If an artifact escapes (e.g. db_path points somewhere outside
    // data_dir because a flag default changed), GC would either miss
    // the artifact (retention leak) or delete something outside its
    // jurisdiction (data loss). This row pins the invariant that every
    // diag-advertised artifact path nests under data_dir.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["diag", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass diag --json");
    assert!(out.status.success(), "cass diag --json exited non-zero");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let diag: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    let data_dir = diag["paths"]["data_dir"]
        .as_str()
        .expect("paths.data_dir must be a string");
    let db_path = diag["paths"]["db_path"]
        .as_str()
        .expect("paths.db_path must be a string");
    let index_path = diag["paths"]["index_path"]
        .as_str()
        .expect("paths.index_path must be a string");

    let data_dir_path = Path::new(data_dir);
    let db_path = Path::new(db_path);
    let index_path = Path::new(index_path);

    // Retention invariant: both artifact paths must live inside the
    // declared data_dir so GC can reason about them without relying on
    // fragile string-prefix checks.
    assert!(
        db_path.starts_with(data_dir_path),
        "db_path ({}) escapes data_dir ({}) - GC jurisdiction leak",
        db_path.display(),
        data_dir_path.display()
    );
    assert!(
        index_path.starts_with(data_dir_path),
        "index_path ({}) escapes data_dir ({}) - GC jurisdiction leak",
        index_path.display(),
        data_dir_path.display()
    );

    // And data_dir itself must live inside the isolated test HOME
    // so the retention sandbox is honored.
    assert!(
        data_dir_path.starts_with(test_home.path()),
        "data_dir ({}) escapes test HOME ({}) - XDG_DATA_HOME/HOME pin bypassed",
        data_dir_path.display(),
        test_home.path().display()
    );
}

#[test]
fn index_subcommand_exposes_all_entrypoint_flags() {
    // tin8o migration-safety row. The bead's scope is "migrate watch,
    // import, salvage, and incremental entrypoints onto the same
    // streaming packet pipeline" — a refactor that touches every cass
    // index entrypoint flag. If the refactor accidentally drops or
    // renames any entrypoint flag (--full, --watch, --watch-once,
    // --semantic, --force-rebuild) during migration, every downstream
    // automation breaks. This row pins the CLI contract by parsing
    // `cass index --help` and asserting each required flag is still
    // advertised.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["index", "--help"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass index --help");
    assert!(out.status.success(), "cass index --help exited non-zero");
    let help = String::from_utf8(out.stdout).expect("utf8");

    // Every documented entrypoint flag must be advertised in the help
    // text. Missing any of these signals a refactor that accidentally
    // dropped the flag — every automation downstream breaks silently.
    for flag in [
        "--full",
        "--watch",
        "--watch-once",
        "--semantic",
        "--force-rebuild",
    ] {
        assert!(
            help.contains(flag),
            "cass index --help is missing documented flag {flag:?} — entrypoint drift detected\n\nhelp output:\n{help}"
        );
    }

    // And --force-rebuild must still advertise its --force alias per
    // the current flag contract, so existing scripts keep working.
    assert!(
        help.contains("--force"),
        "cass index --help dropped the --force alias for --force-rebuild"
    );
}

#[test]
fn diag_connector_entries_have_uniform_shape() {
    // ibuuh.19 connector-inventory contract row. cass diag --json
    // reports per-connector detection status as an array of
    // {name, path, found} objects. Every entry must have all three
    // keys with the expected types — a missing or mis-typed field in
    // one entry silently skews retention / GC logic that enumerates
    // connector outputs. The empty-HOME shape gives us 19 entries all
    // with found=false and path="(not detected)", making this a strong
    // stable invariant check.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["diag", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass diag --json");
    assert!(out.status.success(), "cass diag --json exited non-zero");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let diag: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let connectors = diag["connectors"]
        .as_array()
        .expect("diag.connectors is an array");

    assert!(
        !connectors.is_empty(),
        "diag.connectors must not be empty — sanity check"
    );

    for (i, entry) in connectors.iter().enumerate() {
        assert!(
            entry.is_object(),
            "diag.connectors[{i}] must be an object; got {entry:?}"
        );
        let name = entry["name"]
            .as_str()
            .unwrap_or_else(|| panic!("diag.connectors[{i}].name must be a string"));
        assert!(
            !name.is_empty(),
            "diag.connectors[{i}].name must be non-empty"
        );
        let path = entry["path"]
            .as_str()
            .unwrap_or_else(|| panic!("diag.connectors[{i}].path must be a string"));
        assert!(
            !path.is_empty(),
            "diag.connectors[{i}].path must be non-empty (use \"(not detected)\" for absent)"
        );
        let _found = entry["found"]
            .as_bool()
            .unwrap_or_else(|| panic!("diag.connectors[{i}].found must be a bool"));
        // NB: we intentionally DO NOT assert !found here. Some connector
        // detectors scan the CWD (e.g. aider looks at
        // ./.aider.chat.history.md) in addition to HOME, so an isolated
        // XDG_DATA_HOME/HOME pin can still see CWD-rooted hits. The
        // shape/type invariants above are the stable part of the
        // contract ibuuh.19's retention / GC depends on — an agent
        // enumerating connectors must be able to trust every entry has
        // name (non-empty string) + path (non-empty string) + found
        // (bool) regardless of which connector happens to fire.
    }
}

#[test]
fn db_and_index_surface_flags_match_actual_filesystem() {
    // ibuuh.19 retention-ground-truth row. Both health.db.exists and
    // diag.database.exists claim to report on-disk artifact presence.
    // Verify those reports match the ACTUAL filesystem — if a surface
    // caches a stale exists=true while the file is gone (or claims
    // exists=false when the file is still on disk), retention/GC
    // operates on fiction and either deletes real data or leaks
    // orphaned artifacts.
    //
    // Under the isolated empty HOME we know the filesystem truth
    // (no db, no index). Pin both surfaces to that truth.
    let test_home = tempfile::tempdir().expect("tempdir");

    fn cass_json(home: &Path, args: &[&str]) -> serde_json::Value {
        let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
            .args(args)
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("XDG_DATA_HOME", home)
            .env("HOME", home)
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .output()
            .expect("run cass");
        let stdout = String::from_utf8(out.stdout).expect("utf8");
        serde_json::from_str(&stdout).expect("valid JSON")
    }

    let diag = cass_json(test_home.path(), &["diag", "--json"]);
    let health = cass_json(test_home.path(), &["health", "--json"]);

    let db_path = diag["paths"]["db_path"]
        .as_str()
        .expect("paths.db_path is string");
    let index_path = diag["paths"]["index_path"]
        .as_str()
        .expect("paths.index_path is string");

    let db_fs_exists = Path::new(db_path).exists();
    let index_fs_exists = Path::new(index_path).exists();

    let diag_db = diag["database"]["exists"].as_bool().unwrap();
    let diag_idx = diag["index"]["exists"].as_bool().unwrap();
    let health_db = health["db"]["exists"].as_bool().unwrap();
    let health_idx = health["state"]["index"]["exists"].as_bool().unwrap();

    // Three-way agreement: filesystem ↔ diag ↔ health.
    assert_eq!(
        db_fs_exists, diag_db,
        "diag.database.exists ({diag_db}) disagrees with filesystem ({db_fs_exists}) at {db_path}"
    );
    assert_eq!(
        db_fs_exists, health_db,
        "health.db.exists ({health_db}) disagrees with filesystem ({db_fs_exists}) at {db_path}"
    );
    assert_eq!(
        index_fs_exists, diag_idx,
        "diag.index.exists ({diag_idx}) disagrees with filesystem ({index_fs_exists}) at {index_path}"
    );
    assert_eq!(
        index_fs_exists, health_idx,
        "health.state.index.exists ({health_idx}) disagrees with filesystem ({index_fs_exists}) at {index_path}"
    );

    // And — the isolated-empty-HOME invariant: both should actually
    // be absent on disk so the three-way agreement isn't trivially
    // satisfied by two matching lies.
    assert!(
        !db_fs_exists,
        "isolated empty HOME still has DB on disk at {db_path}"
    );
    assert!(
        !index_fs_exists,
        "isolated empty HOME still has index on disk at {index_path}"
    );
}

#[test]
fn index_checkpoint_and_fingerprint_blocks_have_stable_shape() {
    // ibuuh.24 crash-safety row. The stale-refresh architecture promises
    // crash-safe resume: a rebuild that crashed mid-way can be resumed
    // because state.index.checkpoint + state.index.fingerprint carry
    // enough info to decide whether to resume or start over. If any of
    // those fields rename or drop, the resume logic silently loses the
    // signal it needs and either re-starts from scratch (wasted work)
    // or resumes against a mismatched DB (correctness risk).
    //
    // Pin the shape of both sub-blocks so contract drift fails fast.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["health", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass health --json");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let health: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let idx = &health["state"]["index"];

    // checkpoint sub-block: present is always a bool. The other four
    // boolean-semantic fields are bool-or-null (null when no checkpoint
    // exists; bool when one does). Rename/drop of any of these loses
    // the resume-vs-restart signal.
    let cp = &idx["checkpoint"];
    assert!(cp.is_object(), "state.index.checkpoint must be an object");
    let present = cp["present"]
        .as_bool()
        .expect("checkpoint.present must be a bool");
    for key in [
        "completed",
        "db_matches",
        "schema_matches",
        "page_size_matches",
        "page_size_compatible",
    ] {
        let v = &cp[key];
        assert!(
            v.is_boolean() || v.is_null(),
            "state.index.checkpoint.{key} must be bool or null; got {v:?}"
        );
        // When present=false, every bool-or-null field must be null
        // (no checkpoint to describe) — this is the crash-safe resume
        // invariant: absent checkpoint => absent checkpoint metadata.
        if !present {
            assert!(
                v.is_null(),
                "checkpoint.present=false but checkpoint.{key}={v:?}; expected null"
            );
        }
    }

    // fingerprint sub-block: three string-or-null fields, all
    // nullable when no fingerprint exists yet.
    let fp = &idx["fingerprint"];
    assert!(fp.is_object(), "state.index.fingerprint must be an object");
    for key in ["current_db_fingerprint", "checkpoint_fingerprint"] {
        let v = &fp[key];
        assert!(
            v.is_string() || v.is_null(),
            "state.index.fingerprint.{key} must be string or null; got {v:?}"
        );
    }
    let matches_v = &fp["matches_current_db_fingerprint"];
    assert!(
        matches_v.is_boolean() || matches_v.is_null(),
        "state.index.fingerprint.matches_current_db_fingerprint must be bool or null; got {matches_v:?}"
    );
}

#[test]
fn diag_paths_use_canonical_filename_and_index_parent() {
    // ibuuh.19 retention-layout row. The existing
    // diag_artifact_paths_nest_inside_data_dir_for_safe_gc row pins the
    // jurisdiction invariant (artifacts stay inside data_dir) but does
    // not pin the *shape* of the layout inside data_dir. Retention/GC
    // code and external ops scripts both rely on two conventions:
    //
    //   1. db_path ends with the canonical file name `agent_search.db`.
    //      Several tools, migrations, and backup recipes reference this
    //      name directly; a silent rename would break them even though
    //      the nest-check would still pass.
    //   2. index_path lives under a directory literally named `index/`
    //      inside data_dir. This is what the GC policy uses to find
    //      superseded lexical generations, scratch rebuild dirs, etc.
    //      A flat layout would still nest, but would invalidate the
    //      "everything under data_dir/index/ is index-owned" rule.
    //
    // Pin both so accidental layout refactors fail loudly.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["diag", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass diag --json");
    assert!(out.status.success(), "cass diag --json exited non-zero");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let diag: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    let data_dir = diag["paths"]["data_dir"]
        .as_str()
        .expect("paths.data_dir must be a string");
    let db_path = diag["paths"]["db_path"]
        .as_str()
        .expect("paths.db_path must be a string");
    let index_path = diag["paths"]["index_path"]
        .as_str()
        .expect("paths.index_path must be a string");

    let data_dir_p = Path::new(data_dir);
    let db_p = Path::new(db_path);
    let index_p = Path::new(index_path);

    // Convention 1: canonical DB filename. Referenced by name in
    // backup/migration/retention recipes — rename detection.
    let db_file_name = db_p
        .file_name()
        .and_then(|s| s.to_str())
        .expect("db_path must have a UTF-8 filename component");
    assert_eq!(
        db_file_name, "agent_search.db",
        "db_path filename ({db_file_name}) diverged from canonical 'agent_search.db'; \
         retention and backup recipes that reference this name will silently break"
    );

    // Convention 2: index dir lives under `<data_dir>/index/...`.
    // Walk up from index_path until the immediate parent equals
    // `<data_dir>/index`. We allow arbitrary versioned subdirs (e.g.
    // `v7`, future `v8`) but require the `index` parent layer to
    // preserve the GC ownership rule.
    let expected_index_root = data_dir_p.join("index");
    let index_root_found = index_p
        .ancestors()
        .any(|ancestor| ancestor == expected_index_root);
    assert!(
        index_root_found,
        "index_path ({}) does not live under the canonical '{}' layer; \
         retention rules that sweep `<data_dir>/index/` for superseded \
         generations will lose track of this artifact",
        index_p.display(),
        expected_index_root.display()
    );

    // And the index subtree must be strictly below that `index/`
    // directory (not equal to it) — a degenerate layout where
    // index_path == data_dir/index would leak generation management
    // into the root index folder itself.
    assert!(
        index_p.starts_with(&expected_index_root) && index_p != expected_index_root.as_path(),
        "index_path ({}) must be a strict descendant of '{}', not the directory itself",
        index_p.display(),
        expected_index_root.display()
    );
}

#[test]
fn diag_absent_artifacts_report_zero_counters_and_sizes() {
    // ibuuh.19 retention-coherence row. GC and retention planning read
    // three signals from `cass diag --json` for each artifact:
    //
    //   - database: { exists, size_bytes, conversations, messages }
    //   - index:    { exists, size_bytes }
    //
    // Retention decides "skip vs reclaim" by fusing these. An absent
    // artifact must report *coherently* absent: exists=false AND
    // size_bytes=0 AND (for the DB) conversations=0 AND messages=0.
    // If any counter drifts (e.g. exists=false but messages=N from a
    // stale in-memory cache), retention will either:
    //   - see phantom live data and refuse to reclaim, or
    //   - see phantom reclaimable bytes and try to delete nothing.
    // Both outcomes silently degrade the retention contract.
    //
    // An isolated HOME guarantees both artifacts are truly absent, so
    // the "coherently absent" state is the one under test.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["diag", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass diag --json");
    assert!(out.status.success(), "cass diag --json exited non-zero");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let diag: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    // Database block: absent ⇒ every counter must be the zero value.
    let db = &diag["database"];
    assert!(db.is_object(), "diag.database must be an object");
    let db_exists = db["exists"]
        .as_bool()
        .expect("database.exists must be a bool");
    assert!(
        !db_exists,
        "isolated HOME unexpectedly has database.exists=true"
    );
    let db_size = db["size_bytes"]
        .as_u64()
        .expect("database.size_bytes must be a u64");
    let db_conv = db["conversations"]
        .as_u64()
        .expect("database.conversations must be a u64");
    let db_msgs = db["messages"]
        .as_u64()
        .expect("database.messages must be a u64");
    assert_eq!(
        db_size, 0,
        "database.exists=false but size_bytes={db_size} — stale size reading would mislead retention reclaim plans"
    );
    assert_eq!(
        db_conv, 0,
        "database.exists=false but conversations={db_conv} — phantom row count would block retention reclaim of 'live' data that is not actually there"
    );
    assert_eq!(
        db_msgs, 0,
        "database.exists=false but messages={db_msgs} — phantom row count would block retention reclaim of 'live' data that is not actually there"
    );

    // Index block: absent ⇒ size must be 0.
    let idx = &diag["index"];
    assert!(idx.is_object(), "diag.index must be an object");
    let idx_exists = idx["exists"]
        .as_bool()
        .expect("index.exists must be a bool");
    assert!(
        !idx_exists,
        "isolated HOME unexpectedly has index.exists=true"
    );
    let idx_size = idx["size_bytes"]
        .as_u64()
        .expect("index.size_bytes must be a u64");
    assert_eq!(
        idx_size, 0,
        "index.exists=false but size_bytes={idx_size} — phantom reclaimable bytes would mislead retention budget accounting"
    );
}

#[test]
fn models_status_model_dir_nests_under_data_dir_and_coheres_on_absence() {
    // ibuuh.19 model-cache retention row. The bead explicitly names
    // "stale model caches as first-class cleanup candidates". Model
    // cache hygiene depends on three retention invariants that
    // nothing else in the matrix currently pins:
    //
    //   1. `model_dir` (the model-cache root) must live inside the
    //      declared data_dir — GC jurisdiction. If the model cache
    //      escapes data_dir, retention either misses it (cache bloat)
    //      or would need to sweep outside its sandbox (data-loss risk).
    //
    //   2. `model_dir` must be the same value on the top-level surface
    //      and inside `cache_lifecycle`. Those are two code paths that
    //      retention and acquisition both consult; silent divergence
    //      means one layer could try to clean up a dir the other layer
    //      still considers authoritative.
    //
    //   3. When `installed=false`, the byte counters retention would
    //      use to decide "reclaim vs keep" must all be zero
    //      (installed_size_bytes + observed_file_bytes). A stale
    //      non-zero value would produce phantom reclaimable bytes and
    //      mislead budget accounting.
    //
    // Isolated HOME guarantees the model is not installed, so the
    // coherently-absent case is the one under test.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["models", "status", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass models status --json");
    assert!(
        out.status.success(),
        "cass models status --json exited non-zero"
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let status: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    // Re-derive data_dir from diag so we do not hard-code the layout.
    let diag_out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["diag", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass diag --json");
    assert!(
        diag_out.status.success(),
        "cass diag --json exited non-zero"
    );
    let diag: serde_json::Value =
        serde_json::from_str(&String::from_utf8(diag_out.stdout).expect("utf8"))
            .expect("valid JSON");
    let data_dir = diag["paths"]["data_dir"]
        .as_str()
        .expect("paths.data_dir must be a string");

    // Invariant 1: model_dir nests under data_dir (GC jurisdiction).
    let model_dir = status["model_dir"]
        .as_str()
        .expect("models status must expose model_dir as a string");
    assert!(
        Path::new(model_dir).starts_with(data_dir),
        "model_dir ({model_dir}) escapes data_dir ({data_dir}) — retention GC cannot safely reach this model-cache root"
    );

    // Invariant 2: model_dir == cache_lifecycle.model_dir.
    let cl = &status["cache_lifecycle"];
    assert!(
        cl.is_object(),
        "models status must expose cache_lifecycle as an object"
    );
    let cl_model_dir = cl["model_dir"]
        .as_str()
        .expect("cache_lifecycle.model_dir must be a string");
    assert_eq!(
        model_dir, cl_model_dir,
        "top-level model_dir ({model_dir}) diverged from cache_lifecycle.model_dir ({cl_model_dir}); acquisition and retention would target different directories"
    );

    // Invariant 3: installed=false ⇒ byte counters all zero.
    let installed = status["installed"]
        .as_bool()
        .expect("models status must expose installed as bool");
    assert!(
        !installed,
        "isolated HOME unexpectedly reports installed=true — test assumption broken"
    );
    let installed_size = status["installed_size_bytes"]
        .as_u64()
        .expect("installed_size_bytes must be u64");
    let observed = status["observed_file_bytes"]
        .as_u64()
        .expect("observed_file_bytes must be u64");
    assert_eq!(
        installed_size, 0,
        "installed=false but installed_size_bytes={installed_size} — phantom reclaimable bytes would mislead model-cache retention budgets"
    );
    assert_eq!(
        observed, 0,
        "installed=false but observed_file_bytes={observed} — phantom cached bytes would mislead model-cache retention budgets"
    );

    // And the cache_lifecycle mirror of the same counter must agree.
    let cl_installed_size = cl["installed_size_bytes"]
        .as_u64()
        .expect("cache_lifecycle.installed_size_bytes must be u64");
    assert_eq!(
        cl_installed_size, 0,
        "installed=false but cache_lifecycle.installed_size_bytes={cl_installed_size} — retention layer would see phantom cached bytes"
    );
}

#[test]
fn absent_db_drives_null_checkpoint_and_fingerprint_state() {
    // ibuuh.24 crash-safety row. Crash-safe resume relies on two
    // blocks in `cass health --json`:
    //
    //   state.index.checkpoint   — describes a paused rebuild pass
    //   state.index.fingerprint  — binds that pass to a specific DB
    //
    // The resume decision reads both: if the checkpoint says "still
    // in progress" AND the fingerprint matches the current DB, resume;
    // otherwise restart from scratch. That logic only works if the
    // "no DB exists" case collapses both blocks to fully-null state.
    // If any checkpoint or fingerprint field were to carry leftover
    // non-null values when `state.db == null`, crash-safe resume would
    // either:
    //   - spuriously resume against a non-existent DB (corruption
    //     risk), or
    //   - compare against stale fingerprints and fail to resume when
    //     resumption was actually valid (wasted work).
    //
    // The existing index_checkpoint_and_fingerprint_blocks_have_stable_shape
    // row pins intra-checkpoint shape only (present=false ⇒ checkpoint
    // fields null). This row adds the cross-block invariant that
    // db-absence drives checkpoint.present=false AND every fingerprint
    // field null.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["health", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass health --json");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let health: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    // Precondition: isolated HOME has no DB, so state.db is null.
    // This is the specific case the crash-safety invariant constrains.
    let db = &health["state"]["db"];
    assert!(
        db.is_null(),
        "isolated HOME unexpectedly has non-null state.db: {db:?}"
    );

    let idx = &health["state"]["index"];
    assert!(idx.is_object(), "state.index must be an object");

    // Invariant A: state.db absent ⇒ checkpoint.present = false.
    let cp = &idx["checkpoint"];
    let present = cp["present"]
        .as_bool()
        .expect("checkpoint.present must be a bool");
    assert!(
        !present,
        "state.db is null but checkpoint.present=true — a checkpoint cannot describe progress against a DB that does not exist; crash-safe resume would target phantom state"
    );

    // Invariant B: state.db absent ⇒ every fingerprint field is null.
    // Fingerprinting requires a real DB to hash; no DB means no
    // fingerprint machinery should produce any non-null value.
    let fp = &idx["fingerprint"];
    assert!(fp.is_object(), "state.index.fingerprint must be an object");
    for key in [
        "current_db_fingerprint",
        "checkpoint_fingerprint",
        "matches_current_db_fingerprint",
    ] {
        let v = &fp[key];
        assert!(
            v.is_null(),
            "state.db is null but fingerprint.{key}={v:?} — stale fingerprint would poison resume decision; expected null"
        );
    }

    // Invariant C: the already-shape-pinned checkpoint bool-or-null
    // fields must also be null when state.db is null (redundant with
    // the existing shape row's `!present ⇒ null` rule, but we assert
    // it again here so this row stands on its own against cross-block
    // regressions — if present gets flipped to true without the
    // cascade updating the DB state, this arm still fires).
    for key in [
        "completed",
        "db_matches",
        "schema_matches",
        "page_size_matches",
        "page_size_compatible",
    ] {
        let v = &cp[key];
        assert!(
            v.is_null(),
            "state.db is null but checkpoint.{key}={v:?} — checkpoint sub-field must be null when no DB exists"
        );
    }
}

#[test]
fn absent_index_collapses_timestamp_and_document_fields_to_null() {
    // ibuuh.24 crash-safety row. The index block of `cass health --json`
    // carries several "last seen" signals that downstream consumers
    // (retention, freshness dashboards, resume logic) use to infer
    // partial-rebuild state:
    //
    //   last_indexed_at  — when the last rebuild *completed*
    //   age_seconds      — derived freshness
    //   activity_at      — when the last rebuild *started* or was active
    //   documents        — how many docs the index currently reports
    //   empty_with_messages — "index exists but has zero docs while the
    //                         DB has messages" signal
    //   rebuilding       — is a rebuild running right now
    //
    // When exists=false there is no index to describe. A crashed
    // rebuild must not leave any of these signals carrying stale
    // non-null values, because:
    //   - stale `last_indexed_at` / `age_seconds` would make retention
    //     think a rebuild completed (never rebuild again)
    //   - stale `documents` > 0 would make retention think the index
    //     holds content that can be queried (lexical-ready lies)
    //   - `rebuilding=true` with no actual rebuild would block other
    //     rebuild attempts (deadlock)
    //   - `empty_with_messages=true` with no index is a logic error
    //     (the signal requires an index to exist)
    //
    // Pin the absent-index null/false collapse so crash-recovery-
    // induced half-state can never leak these fields past the absent
    // gate.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["health", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass health --json");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let health: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    let idx = &health["state"]["index"];
    assert!(idx.is_object(), "state.index must be an object");

    // Precondition: isolated HOME has no index.
    let exists = idx["exists"]
        .as_bool()
        .expect("state.index.exists must be a bool");
    assert!(
        !exists,
        "isolated HOME unexpectedly reports index.exists=true"
    );

    // Nullable fields that must be null when index is absent.
    for key in ["last_indexed_at", "age_seconds", "activity_at", "documents"] {
        let v = &idx[key];
        assert!(
            v.is_null(),
            "index.exists=false but {key}={v:?} — stale signal would mislead retention/freshness/resume logic"
        );
    }

    // Boolean fields whose true-semantics require an index to exist.
    let rebuilding = idx["rebuilding"]
        .as_bool()
        .expect("index.rebuilding must be a bool");
    assert!(
        !rebuilding,
        "index.exists=false but rebuilding=true — phantom rebuild-in-progress would deadlock later rebuild attempts"
    );
    let ewm = idx["empty_with_messages"]
        .as_bool()
        .expect("index.empty_with_messages must be a bool");
    assert!(
        !ewm,
        "index.exists=false but empty_with_messages=true — this signal requires an index to exist (degenerate precondition)"
    );

    // And stale_threshold_seconds is a configuration invariant: it
    // must be positive regardless of index existence, because it is
    // the policy knob that drives every freshness decision. A zero
    // threshold would collapse "stale vs fresh" into a single always-
    // stale state; a negative one is nonsensical.
    let threshold = idx["stale_threshold_seconds"]
        .as_u64()
        .expect("index.stale_threshold_seconds must be a u64");
    assert!(
        threshold > 0,
        "stale_threshold_seconds={threshold} but must be positive — zero/negative collapses freshness policy"
    );
}

#[test]
fn models_status_aggregates_equal_component_sums_and_files_cohere_on_absence() {
    // ibuuh.19 model-cache retention row (derived-value consistency).
    // Retention budget accounting reads three aggregates and a per-
    // file breakdown from `cass models status --json`:
    //
    //   total_size_bytes                      (top level)
    //   installed_size_bytes                  (top level)
    //   cache_lifecycle.required_size_bytes   (lifecycle block)
    //   files[].{expected_size, actual_size,
    //            exists, size_match, actual_path}
    //
    // The aggregate-vs-component invariants the retention layer
    // depends on:
    //
    //   A. sum(files[].expected_size) == total_size_bytes
    //      A silent file-list refactor that adds/drops a file without
    //      updating the aggregate would produce a wrong reclaim-vs-
    //      keep budget.
    //
    //   B. cache_lifecycle.required_size_bytes == total_size_bytes
    //      These are two surfaces that acquisition and retention both
    //      consult; silent drift means one layer under-reserves and
    //      the other over-reserves.
    //
    //   C. installed=false ⇒ every files[i] in a coherently-absent
    //      state: exists=false, actual_size=0, size_match=false,
    //      actual_path=null. A per-file stale signal would fool the
    //      retention layer into treating the file as partially
    //      cached (partial reclaim risk) or fully cached (phantom
    //      reclaimable bytes).
    //
    // The earlier row models_status_model_dir_nests_under_data_dir_...
    // covers top-level aggregates and `model_dir`; this one extends
    // coverage to derived-aggregate consistency and per-file coherence.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["models", "status", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass models status --json");
    assert!(
        out.status.success(),
        "cass models status --json exited non-zero"
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let status: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    let total = status["total_size_bytes"]
        .as_u64()
        .expect("total_size_bytes must be u64");
    let installed_size = status["installed_size_bytes"]
        .as_u64()
        .expect("installed_size_bytes must be u64");
    let cl_required = status["cache_lifecycle"]["required_size_bytes"]
        .as_u64()
        .expect("cache_lifecycle.required_size_bytes must be u64");
    let files = status["files"].as_array().expect("files must be an array");
    assert!(
        !files.is_empty(),
        "files array is empty — retention cannot enumerate the cache"
    );

    // Invariant A: aggregate = sum of per-file expected sizes.
    let sum_expected: u64 = files
        .iter()
        .map(|f| {
            f["expected_size"]
                .as_u64()
                .expect("files[].expected_size must be u64")
        })
        .sum();
    assert_eq!(
        sum_expected, total,
        "sum(files[].expected_size)={sum_expected} != total_size_bytes={total} — retention budget diverged from the file-list it should reflect"
    );

    // Invariant B: cache_lifecycle aggregate agrees with top-level.
    assert_eq!(
        cl_required, total,
        "cache_lifecycle.required_size_bytes={cl_required} != total_size_bytes={total} — acquisition and retention would plan against different sizes"
    );

    // Precondition for invariant C: isolated HOME means not installed.
    let installed = status["installed"]
        .as_bool()
        .expect("installed must be a bool");
    assert!(!installed, "isolated HOME unexpectedly installed=true");
    assert_eq!(
        installed_size, 0,
        "installed=false but installed_size_bytes={installed_size}"
    );

    // Invariant C: per-file absence coherence.
    let sum_actual: u64 = files
        .iter()
        .map(|f| {
            f["actual_size"]
                .as_u64()
                .expect("files[].actual_size must be u64")
        })
        .sum();
    assert_eq!(
        sum_actual, 0,
        "installed=false but sum(files[].actual_size)={sum_actual} — phantom cached bytes at file level"
    );
    for (i, f) in files.iter().enumerate() {
        let name = f["name"].as_str().unwrap_or("<unnamed>");
        let exists = f["exists"]
            .as_bool()
            .expect("files[].exists must be a bool");
        let size_match = f["size_match"]
            .as_bool()
            .expect("files[].size_match must be a bool");
        let actual_path = &f["actual_path"];
        assert!(
            !exists,
            "installed=false but files[{i}] ({name}) reports exists=true — stale per-file presence signal"
        );
        assert!(
            !size_match,
            "installed=false but files[{i}] ({name}) reports size_match=true — stale per-file size-match signal"
        );
        assert!(
            actual_path.is_null(),
            "installed=false but files[{i}] ({name}) has actual_path={actual_path:?} — a non-null path cannot exist when installed=false"
        );
    }

    // Also: observed_file_bytes must equal sum(actual_size) — the
    // observed aggregate cannot diverge from the per-file breakdown
    // it was (presumably) derived from. In the installed=false case
    // both are 0, but the equality is the structural invariant.
    let observed = status["observed_file_bytes"]
        .as_u64()
        .expect("observed_file_bytes must be u64");
    assert_eq!(
        observed, sum_actual,
        "observed_file_bytes={observed} != sum(files[].actual_size)={sum_actual} — aggregate drifted from component breakdown"
    );
}

#[test]
fn models_status_and_cache_lifecycle_agree_on_state_machine_identity() {
    // ibuuh.19 cross-block agreement row. `cass models status --json`
    // exposes the same state-machine identity on two surfaces:
    //
    //   top-level:           model_id, state, policy_source
    //   cache_lifecycle:     model_id, state.state, policy_source
    //
    // Acquisition code reads the top level; retention may consult
    // cache_lifecycle for richer detail (missing_files, needs_consent).
    // If the two surfaces diverge on any of these identity/state
    // fields, the layers would disagree about *which* model they are
    // managing and *what phase* that model is in:
    //
    //   - model_id drift => acquisition fetches a different model than
    //                       retention is tracking (leak + miss)
    //   - state drift   => one layer thinks "not_acquired" and
    //                       re-fetches while the other thinks
    //                       "cached" and tries to reclaim
    //   - policy_source drift => different retention budgets applied
    //                            simultaneously
    //
    // Plus a derived-value check: when installed=false, the
    // cache_lifecycle.state.missing_files list must enumerate every
    // files[].local_name — the machinery that produced "all files
    // are missing" must not silently drop entries.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["models", "status", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass models status --json");
    assert!(
        out.status.success(),
        "cass models status --json exited non-zero"
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let status: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    let cl = &status["cache_lifecycle"];
    assert!(cl.is_object(), "cache_lifecycle must be an object");

    // Invariant A: top-level model_id == cache_lifecycle.model_id.
    let top_mid = status["model_id"]
        .as_str()
        .expect("top-level model_id must be a string");
    let cl_mid = cl["model_id"]
        .as_str()
        .expect("cache_lifecycle.model_id must be a string");
    assert_eq!(
        top_mid, cl_mid,
        "top-level model_id ({top_mid}) diverged from cache_lifecycle.model_id ({cl_mid}) — acquisition and retention would manage different models"
    );

    // Invariant B: top-level state string == cache_lifecycle.state.state.
    let top_state = status["state"]
        .as_str()
        .expect("top-level state must be a string");
    let cl_state = cl["state"]["state"]
        .as_str()
        .expect("cache_lifecycle.state.state must be a string");
    assert_eq!(
        top_state, cl_state,
        "top-level state ({top_state}) diverged from cache_lifecycle.state.state ({cl_state}) — acquisition and retention would see different phases"
    );

    // Invariant C: policy_source agreement.
    let top_ps = status["policy_source"]
        .as_str()
        .expect("top-level policy_source must be a string");
    let cl_ps = cl["policy_source"]
        .as_str()
        .expect("cache_lifecycle.policy_source must be a string");
    assert_eq!(
        top_ps, cl_ps,
        "top-level policy_source ({top_ps}) diverged from cache_lifecycle.policy_source ({cl_ps}) — different retention budgets would apply"
    );

    // Invariant D: installed=false ⇒ missing_files enumerates every
    // file in files[] (by local_name). If the list drifted, the
    // acquisition layer would under-fetch and retention would see
    // phantom "already cached" files.
    let installed = status["installed"]
        .as_bool()
        .expect("installed must be a bool");
    assert!(!installed, "isolated HOME unexpectedly installed=true");

    let files = status["files"].as_array().expect("files must be an array");
    let mut file_local_names: Vec<String> = files
        .iter()
        .map(|f| {
            f["local_name"]
                .as_str()
                .expect("files[].local_name must be a string")
                .to_string()
        })
        .collect();
    file_local_names.sort();

    let missing = cl["state"]["missing_files"]
        .as_array()
        .expect("cache_lifecycle.state.missing_files must be an array when not_acquired");
    let mut missing_names: Vec<String> = missing
        .iter()
        .map(|m| {
            m.as_str()
                .expect("missing_files entries must be strings")
                .to_string()
        })
        .collect();
    missing_names.sort();

    assert_eq!(
        missing_names, file_local_names,
        "cache_lifecycle.state.missing_files drifted from files[].local_name — acquisition would under-fetch or over-fetch"
    );

    // Invariant E: needs_consent=true ⇒ state=='not_acquired'. A model
    // cannot simultaneously need consent AND be cached/installed; the
    // state-machine precondition must hold.
    let needs_consent = cl["state"]["needs_consent"]
        .as_bool()
        .expect("state.needs_consent must be a bool");
    if needs_consent {
        assert_eq!(
            cl_state, "not_acquired",
            "needs_consent=true but state={cl_state} — needs_consent only makes sense in the not_acquired phase"
        );
    }
}

#[test]
fn models_status_fail_open_and_manifest_integrity_invariants() {
    // ibuuh.19 operator-safety + manifest-integrity row. Model-cache
    // retention has knock-on effects on the user-visible fail-open
    // promise (lexical works even without semantic) and on the
    // content-addressing used to key versioned caches. This row pins
    // four invariants on `cass models status --json` that, if
    // violated, would let retention or acquisition silently break
    // user-visible guarantees:
    //
    //   A. state="not_acquired" ⇒ lexical_fail_open=true
    //      The fail-open policy guarantees users still get lexical
    //      search when the semantic model is absent. If retention
    //      reclaimed the model cache but lexical_fail_open stopped
    //      being true, users would see "search unavailable" instead
    //      of the intended graceful degradation.
    //
    //   B. next_step is a non-empty string. Operator guidance must
    //      always be actionable — an empty next_step defeats the
    //      purpose of the surface.
    //
    //   C. revision and license are non-empty strings. revision is
    //      the content-addressing key retention uses to key
    //      versioned model caches (two revisions of the same model
    //      are distinct retention candidates); license is a
    //      compliance-retention invariant (retention must preserve
    //      license strings through reclamation).
    //
    //   D. files[].name and files[].local_name values are unique
    //      within the manifest. Duplicate names would cause
    //      retention to double-count bytes or collide on the same
    //      filesystem location during acquisition.
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["models", "status", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass models status --json");
    assert!(
        out.status.success(),
        "cass models status --json exited non-zero"
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let status: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    // Invariant A: fail-open guarantee under not_acquired.
    let state = status["state"]
        .as_str()
        .expect("status.state must be a string");
    let fail_open = status["lexical_fail_open"]
        .as_bool()
        .expect("lexical_fail_open must be a bool");
    if state == "not_acquired" {
        assert!(
            fail_open,
            "state=not_acquired but lexical_fail_open=false — retention/reclamation of the model cache would break the lexical-search fail-open guarantee"
        );
    }

    // Invariant B: next_step is non-empty actionable guidance.
    let next_step = status["next_step"]
        .as_str()
        .expect("status.next_step must be a string");
    assert!(
        !next_step.trim().is_empty(),
        "next_step is empty — operator has no actionable guidance on how to progress the state machine"
    );

    // Invariant C: revision and license are non-empty.
    let revision = status["revision"]
        .as_str()
        .expect("status.revision must be a string");
    assert!(
        !revision.trim().is_empty(),
        "revision is empty — retention cannot key versioned model caches by content address"
    );
    let license = status["license"]
        .as_str()
        .expect("status.license must be a string");
    assert!(
        !license.trim().is_empty(),
        "license is empty — retention must preserve license strings for compliance"
    );

    // Invariant D: files[].name and files[].local_name uniqueness.
    let files = status["files"].as_array().expect("files must be an array");
    let mut names: Vec<&str> = files
        .iter()
        .map(|f| f["name"].as_str().expect("files[].name must be a string"))
        .collect();
    names.sort();
    let mut dedup = names.clone();
    dedup.dedup();
    assert_eq!(
        names.len(),
        dedup.len(),
        "duplicate files[].name detected in manifest {names:?} — retention would double-count bytes or acquisition would collide on fetch"
    );
    let mut local_names: Vec<&str> = files
        .iter()
        .map(|f| {
            f["local_name"]
                .as_str()
                .expect("files[].local_name must be a string")
        })
        .collect();
    local_names.sort();
    let mut dedup_local = local_names.clone();
    dedup_local.dedup();
    assert_eq!(
        local_names.len(),
        dedup_local.len(),
        "duplicate files[].local_name detected in manifest {local_names:?} — two manifest entries point at the same filesystem location"
    );
}

#[test]
fn models_verify_and_status_agree_on_cache_identity_and_phase() {
    // ibuuh.19 cross-command model-cache agreement row.
    // `cass models status --json` and `cass models verify --json` are
    // two retention-critical surfaces that both read the same
    // model-cache state:
    //
    //   status  — general retention inventory (what's cached, sizes)
    //   verify  — integrity check (SHA-256 file validity)
    //
    // Both surfaces advertise `cache_lifecycle` and `model_dir`; if
    // they disagree on *which* cache or *what phase* it's in, the
    // retention/verification layers would operate on different
    // assumptions. Specifically:
    //
    //   - model_dir drift between commands => verify could check
    //     one directory while retention reclaims another
    //   - cache_lifecycle.state drift => one command thinks
    //     "not_acquired" while the other thinks "partial"
    //   - lexical_fail_open drift => the fail-open guarantee would
    //     depend on which command the operator happened to run
    //
    // Plus the verify-specific invariant: all_valid=false must hold
    // when no files exist on disk (cannot validate hashes of absent
    // files), and an error string must be present explaining why.
    let test_home = tempfile::tempdir().expect("tempdir");

    let s_out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["models", "status", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass models status --json");
    assert!(s_out.status.success(), "cass models status --json failed");
    let status: serde_json::Value =
        serde_json::from_str(&String::from_utf8(s_out.stdout).expect("utf8")).expect("valid JSON");

    let v_out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["models", "verify", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass models verify --json");
    // verify exits 0 with a JSON envelope even on verification failure
    // when there is no model to verify yet.
    assert!(v_out.status.success(), "cass models verify --json failed");
    let verify: serde_json::Value =
        serde_json::from_str(&String::from_utf8(v_out.stdout).expect("utf8")).expect("valid JSON");

    // Invariant A: model_dir agrees between status and verify.
    let s_mdir = status["model_dir"]
        .as_str()
        .expect("status.model_dir must be a string");
    let v_mdir = verify["model_dir"]
        .as_str()
        .expect("verify.model_dir must be a string");
    assert_eq!(
        s_mdir, v_mdir,
        "status.model_dir ({s_mdir}) diverged from verify.model_dir ({v_mdir}) — verify and retention would target different directories"
    );

    // Invariant B: cache_lifecycle.model_dir agrees across commands.
    let s_cl_mdir = status["cache_lifecycle"]["model_dir"]
        .as_str()
        .expect("status.cache_lifecycle.model_dir must be a string");
    let v_cl_mdir = verify["cache_lifecycle"]["model_dir"]
        .as_str()
        .expect("verify.cache_lifecycle.model_dir must be a string");
    assert_eq!(
        s_cl_mdir, v_cl_mdir,
        "cache_lifecycle.model_dir diverged across commands: status={s_cl_mdir}, verify={v_cl_mdir}"
    );

    // Invariant C: cache_lifecycle.state.state agrees across commands.
    let s_state = status["cache_lifecycle"]["state"]["state"]
        .as_str()
        .expect("status.cache_lifecycle.state.state must be a string");
    let v_state = verify["cache_lifecycle"]["state"]["state"]
        .as_str()
        .expect("verify.cache_lifecycle.state.state must be a string");
    assert_eq!(
        s_state, v_state,
        "cache_lifecycle.state.state diverged across commands: status={s_state}, verify={v_state} — two retention-adjacent commands see different phases"
    );

    // Invariant D: lexical_fail_open agrees across commands (both
    // surfaces advertise the fail-open promise; both must honor it).
    let s_fo = status["lexical_fail_open"]
        .as_bool()
        .expect("status.lexical_fail_open must be a bool");
    let v_fo = verify["lexical_fail_open"]
        .as_bool()
        .expect("verify.lexical_fail_open must be a bool");
    assert_eq!(
        s_fo, v_fo,
        "lexical_fail_open diverged: status={s_fo}, verify={v_fo} — the fail-open guarantee must not depend on which command the operator runs"
    );

    // Invariant E: when no model is on disk (installed=false in the
    // status surface), all_valid=false in the verify surface — you
    // cannot validate absent files.
    let installed = status["installed"]
        .as_bool()
        .expect("status.installed must be a bool");
    assert!(!installed, "isolated HOME unexpectedly installed=true");
    let all_valid = verify["all_valid"]
        .as_bool()
        .expect("verify.all_valid must be a bool");
    assert!(
        !all_valid,
        "installed=false but verify.all_valid=true — cannot validate absent files; spurious 'ok' would let retention skip re-acquisition"
    );

    // And verify.error must be a non-empty string explaining why the
    // verification did not succeed. An empty or null error here means
    // operators cannot triage why the model is unusable.
    let err = verify["error"]
        .as_str()
        .expect("verify.error must be a string when all_valid=false");
    assert!(
        !err.trim().is_empty(),
        "verify.error is empty despite all_valid=false — operators lose the reason why verification failed"
    );
}

#[test]
fn models_check_update_and_status_agree_on_revision_when_absent() {
    // ibuuh.19 cross-command revision-agreement row.
    //
    // `cass models check-update --json` and `cass models status --json`
    // both advertise a revision string that keys the model cache for
    // retention and acquisition:
    //
    //   status:        status.revision                 — canonical content-addressing key
    //   check-update:  check-update.latest_revision    — upstream target revision
    //                  check-update.current_revision   — locally-installed revision (null if none)
    //
    // For retention to reason about "what version we have vs what
    // version upstream advertises," the two commands MUST agree on
    // the identity of the upstream model. If `status.revision` and
    // `check-update.latest_revision` drifted, retention would
    // compare the installed revision against the wrong target and
    // either falsely decide "up to date" or falsely decide "stale."
    //
    // Plus the absent-gate coherence: when `status.installed=false`,
    // `check-update.current_revision` must be null (nothing is
    // installed to report a revision for) and `update_available`
    // must be false (you cannot "update" something that isn't
    // installed — the operator should `install` first), with a
    // non-empty `reason` explaining why.
    //
    // This is the first lifecycle-matrix coverage of
    // `cass models check-update --json`.
    let test_home = tempfile::tempdir().expect("tempdir");

    let s_out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["models", "status", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass models status --json");
    assert!(s_out.status.success(), "cass models status --json failed");
    let status: serde_json::Value =
        serde_json::from_str(&String::from_utf8(s_out.stdout).expect("utf8")).expect("valid JSON");

    let u_out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["models", "check-update", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass models check-update --json");
    assert!(
        u_out.status.success(),
        "cass models check-update --json failed"
    );
    let check: serde_json::Value =
        serde_json::from_str(&String::from_utf8(u_out.stdout).expect("utf8")).expect("valid JSON");

    // Invariant A: cross-command revision identity.
    let s_rev = status["revision"]
        .as_str()
        .expect("status.revision must be a string");
    let latest_rev = check["latest_revision"]
        .as_str()
        .expect("check-update.latest_revision must be a string");
    assert_eq!(
        s_rev, latest_rev,
        "status.revision ({s_rev}) diverged from check-update.latest_revision ({latest_rev}) — the two commands disagree on which upstream revision is canonical"
    );

    // Precondition: isolated HOME, nothing installed.
    let installed = status["installed"]
        .as_bool()
        .expect("status.installed must be a bool");
    assert!(!installed, "isolated HOME unexpectedly installed=true");

    // Invariant B: installed=false ⇒ check-update.current_revision=null.
    let current_rev = &check["current_revision"];
    assert!(
        current_rev.is_null(),
        "installed=false but check-update.current_revision={current_rev:?} — there is no installed revision to report"
    );

    // Invariant C: installed=false ⇒ update_available=false.
    let update_available = check["update_available"]
        .as_bool()
        .expect("check-update.update_available must be a bool");
    assert!(
        !update_available,
        "installed=false but check-update.update_available=true — you cannot 'update' a model that is not installed; operator should 'install' first"
    );

    // Invariant D: reason is a non-empty string explaining why
    // (e.g. 'model_not_installed'). Operators lose triage info if
    // the reason is empty or null.
    let reason = check["reason"]
        .as_str()
        .expect("check-update.reason must be a string");
    assert!(
        !reason.trim().is_empty(),
        "check-update.reason is empty — operator has no explanation for update_available={update_available}"
    );
}
