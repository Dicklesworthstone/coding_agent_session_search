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
