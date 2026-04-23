mod util;

use std::fs;

use serde_json::json;
use util::search_asset_simulation::{
    AcquisitionStage, ContentionPlan, FailpointEffect, FailpointId, LoadSample, LoadScript,
    PublishCrashWindow, SearchAssetSimulationHarness, SimulationActor, SimulationFailure,
};

fn run_robot_style_demo() -> (
    util::search_asset_simulation::SimulationSummary,
    util::search_asset_simulation::SimulationArtifacts,
    Vec<Result<(), SimulationFailure>>,
) {
    let mut harness = SearchAssetSimulationHarness::new(
        "robot_style_publish_and_acquisition_demo",
        LoadScript::new(vec![
            LoadSample::idle("startup_idle"),
            LoadSample::busy("interactive_spike"),
            LoadSample::loaded("publish_pressure"),
            LoadSample::idle("steady_state_idle"),
            LoadSample::idle("post_crash_recovery"),
        ]),
    );

    harness.install_failpoint_once(
        FailpointId::Acquisition(AcquisitionStage::VerifyChecksum),
        FailpointEffect::ErrorOnce {
            reason: "checksum mismatch".to_owned(),
        },
    );
    harness.install_failpoint_once(
        FailpointId::Publish(PublishCrashWindow::SaveGenerationManifest),
        FailpointEffect::CrashOnce,
    );

    let plan = ContentionPlan::new()
        .turn(SimulationActor::ForegroundSearch, "initial_fail_open_query")
        .turn(SimulationActor::SemanticAcquire, "prepare_model_staging")
        .turn(SimulationActor::SemanticAcquire, "verify_model_checksum")
        .turn(
            SimulationActor::BackgroundSemantic,
            "resume_backfill_after_acquire_failure",
        )
        .turn(SimulationActor::LexicalRepair, "publish_generation")
        .turn(
            SimulationActor::ForegroundSearch,
            "attach_after_publish_crash",
        );

    let results =
        harness.run_contention_plan(&plan, |turn, sim| match (turn.actor, turn.label.as_str()) {
            (SimulationActor::ForegroundSearch, "initial_fail_open_query") => {
                sim.phase(
                    "foreground_search",
                    "lexical search remains available while maintenance is pending",
                );
                sim.snapshot_json(
                    "foreground_status_initial",
                    &json!({
                        "visible_generation": "old_good",
                        "semantic_state": "not_ready",
                        "requested_search_mode": "hybrid",
                        "realized_search_mode": "lexical",
                        "semantic_refinement": false,
                        "fallback_tier": "lexical",
                        "fallback_reason": "semantic assets not ready; lexical fail-open served old-good generation"
                    }),
                );
                Ok(())
            }
            (SimulationActor::SemanticAcquire, "prepare_model_staging") => {
                sim.phase("model_acquisition", "staging semantic model assets");
                sim.snapshot_json(
                    "model_staging_state",
                    &json!({
                        "stage": "prepare_staging_dir",
                        "status": "acquiring",
                        "resume_token": "acquire-001"
                    }),
                );
                Ok(())
            }
            (SimulationActor::SemanticAcquire, "verify_model_checksum") => {
                sim.phase("model_acquisition", "verifying downloaded semantic model");
                sim.trigger_failpoint(FailpointId::Acquisition(AcquisitionStage::VerifyChecksum))
            }
            (SimulationActor::BackgroundSemantic, "resume_backfill_after_acquire_failure") => {
                sim.phase(
                    "scheduler",
                    "background worker records acquisition failure and yields",
                );
                sim.snapshot_json(
                    "scheduler_decision",
                    &json!({
                        "decision": "yield",
                        "reason": "semantic_acquisition_failed",
                        "next_retry": "manual_or_policy_gated"
                    }),
                );
                Ok(())
            }
            (SimulationActor::LexicalRepair, "publish_generation") => {
                sim.phase("publish", "staging lexical generation for atomic promotion");
                sim.snapshot_json(
                    "generation_before_publish_crash",
                    &json!({
                        "generation_id": "lexical-gen-002",
                        "source_fingerprint": "db-fp-123",
                        "state": "staged"
                    }),
                );
                sim.trigger_failpoint(FailpointId::Publish(
                    PublishCrashWindow::SaveGenerationManifest,
                ))
            }
            (SimulationActor::ForegroundSearch, "attach_after_publish_crash") => {
                sim.phase(
                    "foreground_search",
                    "foreground actor observes old-good generation after crash",
                );
                sim.snapshot_json(
                    "foreground_status_after_publish_crash",
                    &json!({
                        "visible_generation": "old_good",
                        "staged_generation": "lexical-gen-002",
                        "recovery_state": "attach_to_previous_generation"
                    }),
                );
                Ok(())
            }
            _ => unreachable!("unexpected deterministic turn"),
        });

    let artifacts = harness
        .write_artifacts()
        .expect("write simulation artifacts");
    (harness.summary(), artifacts, results)
}

#[test]
fn load_script_is_deterministic_and_saturates_at_tail() {
    let mut script = LoadScript::new(vec![
        LoadSample::idle("cold_start"),
        LoadSample::busy("editor_active"),
        LoadSample::loaded("system_under_load"),
    ]);

    let labels = vec![
        script.step().label,
        script.step().label,
        script.step().label,
        script.step().label,
    ];

    assert_eq!(
        labels,
        vec![
            "cold_start".to_owned(),
            "editor_active".to_owned(),
            "system_under_load".to_owned(),
            "system_under_load".to_owned(),
        ]
    );
}

#[test]
fn failpoint_crashes_once_and_then_clears() {
    let mut harness = SearchAssetSimulationHarness::new(
        "failpoint_once",
        LoadScript::new(vec![LoadSample::idle("idle")]),
    );
    let failpoint = FailpointId::Publish(PublishCrashWindow::SwapPublishedGeneration);
    harness.install_failpoint_once(failpoint.clone(), FailpointEffect::CrashOnce);

    let first = harness.trigger_failpoint(failpoint.clone());
    let second = harness.trigger_failpoint(failpoint.clone());

    assert!(matches!(
        first,
        Err(SimulationFailure::Crash { failpoint: seen }) if seen == failpoint
    ));
    assert!(
        second.is_ok(),
        "one-shot failpoint should clear after first trigger"
    );

    let summary = harness.summary();
    assert_eq!(summary.failpoint_markers.len(), 1);
    assert_eq!(summary.failpoint_markers[0].failpoint, failpoint);
    assert_eq!(summary.failpoint_markers[0].effect, "crash_once");
}

#[test]
fn contention_plan_records_per_actor_traces_and_outcomes() {
    let mut harness = SearchAssetSimulationHarness::new(
        "contention_traces",
        LoadScript::new(vec![
            LoadSample::idle("idle"),
            LoadSample::busy("busy"),
            LoadSample::idle("recover"),
        ]),
    );
    harness.install_failpoint_once(
        FailpointId::Acquisition(AcquisitionStage::VerifyChecksum),
        FailpointEffect::ErrorOnce {
            reason: "bad checksum".to_owned(),
        },
    );

    let plan = ContentionPlan::new()
        .turn(SimulationActor::ForegroundSearch, "serve_query")
        .turn(SimulationActor::SemanticAcquire, "verify_checksum")
        .turn(SimulationActor::LexicalRepair, "resume_repair");

    let results = harness.run_contention_plan(&plan, |turn, sim| match turn.actor {
        SimulationActor::ForegroundSearch => {
            sim.phase("foreground_search", "served lexical query");
            Ok(())
        }
        SimulationActor::SemanticAcquire => {
            sim.phase("model_acquisition", "verifying checksum");
            sim.trigger_failpoint(FailpointId::Acquisition(AcquisitionStage::VerifyChecksum))
        }
        SimulationActor::LexicalRepair => {
            sim.phase("lexical_repair", "repair resumes after acquisition failure");
            Ok(())
        }
        SimulationActor::BackgroundSemantic => unreachable!("not used in this test"),
    });

    assert_eq!(results.len(), 3);
    assert!(results[0].is_ok());
    assert!(matches!(
        &results[1],
        Err(SimulationFailure::InjectedError { reason, .. }) if reason == "bad checksum"
    ));
    assert!(results[2].is_ok());

    let summary = harness.summary();
    assert_eq!(summary.actor_traces.len(), 3);
    assert!(matches!(
        summary.actor_traces[1].outcome,
        util::search_asset_simulation::ActorOutcome::Failed(ref reason) if reason == "bad checksum"
    ));
    assert_eq!(summary.actor_traces[2].load.label, "recover");
}

#[test]
fn rollout_gate_verdict_persists_thresholds_and_recovery_evidence() {
    let mut harness = SearchAssetSimulationHarness::new(
        "rollout_gate_thresholds_and_crash_resume",
        LoadScript::new(vec![
            LoadSample::idle("search_ready_build"),
            LoadSample::busy("foreground_query"),
            LoadSample::loaded("publish_pressure"),
            LoadSample::idle("restart_recovery"),
        ]),
    );
    harness.install_failpoint_once(
        FailpointId::Publish(PublishCrashWindow::SwapPublishedGeneration),
        FailpointEffect::CrashOnce,
    );

    let plan = ContentionPlan::new()
        .turn(SimulationActor::LexicalRepair, "build_to_search_ready")
        .turn(SimulationActor::ForegroundSearch, "query_while_repairing")
        .turn(SimulationActor::LexicalRepair, "swap_publish_crash")
        .turn(SimulationActor::LexicalRepair, "restart_verdict");

    let results =
        harness.run_contention_plan(&plan, |turn, sim| match (turn.actor, turn.label.as_str()) {
            (SimulationActor::LexicalRepair, "build_to_search_ready") => {
                sim.phase(
                    "rollout_gate",
                    "search-ready generation prepared within rollout threshold",
                );
                sim.snapshot_json(
                    "search_ready_gate",
                    &json!({
                        "gate": "search_ready_ms",
                        "observed_ms": 1_200,
                        "threshold_ms": 5_000,
                        "status": "pass",
                        "generation_state": "search_ready"
                    }),
                );
                Ok(())
            }
            (SimulationActor::ForegroundSearch, "query_while_repairing") => {
                sim.phase(
                    "foreground_search",
                    "foreground query fails open to old-good generation during repair",
                );
                sim.snapshot_json(
                    "fail_open_during_repair",
                    &json!({
                        "requested_search_mode": "hybrid",
                        "realized_search_mode": "lexical",
                        "visible_generation": "old_good",
                        "blocked_wait_ms": 0,
                        "max_blocked_wait_ms": 250,
                        "status": "pass"
                    }),
                );
                Ok(())
            }
            (SimulationActor::LexicalRepair, "swap_publish_crash") => {
                sim.phase(
                    "publish",
                    "simulating crash while swapping the published generation",
                );
                sim.snapshot_json(
                    "pre_swap_crash",
                    &json!({
                        "candidate_generation": "lexical-gen-003",
                        "published_before_crash": "old_good",
                        "crash_window": "swap_published_generation"
                    }),
                );
                sim.trigger_failpoint(FailpointId::Publish(
                    PublishCrashWindow::SwapPublishedGeneration,
                ))
            }
            (SimulationActor::LexicalRepair, "restart_verdict") => {
                sim.phase(
                    "rollout_gate",
                    "restart selects old-good generation and preserves crash evidence",
                );
                sim.snapshot_json(
                    "rollout_verdict",
                    &json!({
                        "verdict": "pass",
                        "selected_generation_after_restart": "old_good",
                        "crash_evidence_retained": true,
                        "gates": {
                            "search_ready_ms": "pass",
                            "fail_open_wait": "pass",
                            "old_good_after_crash": "pass"
                        }
                    }),
                );
                Ok(())
            }
            _ => unreachable!("unexpected deterministic rollout-gate turn"),
        });

    assert_eq!(results.len(), 4);
    assert!(results[0].is_ok());
    assert!(results[1].is_ok());
    assert!(matches!(
        &results[2],
        Err(SimulationFailure::Crash { failpoint })
            if *failpoint == FailpointId::Publish(PublishCrashWindow::SwapPublishedGeneration)
    ));
    assert!(results[3].is_ok());

    let artifacts = harness.write_artifacts().expect("write rollout artifacts");
    assert!(artifacts.phase_log_path.exists());
    assert!(artifacts.failpoints_path.exists());
    assert!(artifacts.summary_path.exists());

    let phase_log = fs::read_to_string(&artifacts.phase_log_path).expect("read phase log");
    assert!(
        phase_log.contains("rollout_gate"),
        "phase log should preserve rollout-gate phases"
    );

    let verdict_path = artifacts.snapshot_dir.join("004-rollout_verdict.json");
    let verdict: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&verdict_path).expect("read rollout verdict snapshot"),
    )
    .expect("rollout verdict JSON");
    assert_eq!(verdict["verdict"], "pass");
    assert_eq!(
        verdict["selected_generation_after_restart"], "old_good",
        "restart must preserve old-good searchability after a swap crash"
    );
    assert_eq!(verdict["crash_evidence_retained"], true);
    assert_eq!(verdict["gates"]["search_ready_ms"], "pass");
    assert_eq!(verdict["gates"]["fail_open_wait"], "pass");
    assert_eq!(verdict["gates"]["old_good_after_crash"], "pass");
}

#[test]
fn many_core_responsiveness_gate_persists_phase_utilization_evidence() {
    let mut harness = SearchAssetSimulationHarness::new(
        "many_core_phase_utilization_responsiveness_gate",
        LoadScript::new(vec![
            LoadSample::idle("legacy_serial_baseline"),
            LoadSample::idle("segment_farm_build"),
            LoadSample::busy("foreground_probe"),
            LoadSample::loaded("settle_pressure"),
            LoadSample::idle("fully_settled"),
        ]),
    );

    let plan = ContentionPlan::new()
        .turn(SimulationActor::LexicalRepair, "record_serial_baseline")
        .turn(SimulationActor::LexicalRepair, "record_segment_farm")
        .turn(SimulationActor::ForegroundSearch, "probe_responsiveness")
        .turn(
            SimulationActor::LexicalRepair,
            "pause_settle_under_pressure",
        )
        .turn(SimulationActor::LexicalRepair, "rollout_verdict");

    let results =
        harness.run_contention_plan(&plan, |turn, sim| match (turn.actor, turn.label.as_str()) {
            (SimulationActor::LexicalRepair, "record_serial_baseline") => {
                sim.phase(
                    "many_core_baseline",
                    "recording legacy serial replay utilization for comparison",
                );
                sim.snapshot_json(
                    "phase_utilization_baseline",
                    &json!({
                        "phase": "legacy_serial_replay",
                        "available_cores": 32,
                        "active_workers": 1,
                        "reserved_cores": 4,
                        "cpu_core_utilization_pct": 3.1,
                        "queue_depth": 0,
                        "search_ready_ms": 14_500,
                        "measurement": "deterministic_harness_fixture"
                    }),
                );
                Ok(())
            }
            (SimulationActor::LexicalRepair, "record_segment_farm") => {
                sim.phase(
                    "many_core_segment_farm",
                    "recording phase utilization for the shard-farm build",
                );
                sim.snapshot_json(
                    "phase_utilization_segment_farm",
                    &json!({
                        "phase": "segment_farm_build",
                        "available_cores": 32,
                        "active_workers": 24,
                        "reserved_cores": 4,
                        "cpu_core_utilization_pct": 81.0,
                        "queue_depth": 14,
                        "search_ready_ms": 3_800,
                        "search_ready_threshold_ms": 8_000,
                        "status": "pass"
                    }),
                );
                Ok(())
            }
            (SimulationActor::ForegroundSearch, "probe_responsiveness") => {
                sim.phase(
                    "foreground_responsiveness",
                    "foreground probe stays within the interactive latency gate",
                );
                sim.snapshot_json(
                    "foreground_responsiveness_gate",
                    &json!({
                        "p95_interactive_latency_ms": 48,
                        "latency_threshold_ms": 100,
                        "blocked_wait_ms": 0,
                        "max_blocked_wait_ms": 250,
                        "visible_generation": "old_good",
                        "status": "pass"
                    }),
                );
                Ok(())
            }
            (SimulationActor::LexicalRepair, "pause_settle_under_pressure") => {
                sim.phase(
                    "controller_limited_settle",
                    "controller pauses non-critical settling while the machine is loaded",
                );
                sim.snapshot_json(
                    "settle_pressure_gate",
                    &json!({
                        "controller_decision": "pause_deferred_compaction",
                        "reason": "machine_pressure",
                        "search_ready": true,
                        "fully_settled": false,
                        "merge_debt_state": "paused",
                        "status": "pass"
                    }),
                );
                Ok(())
            }
            (SimulationActor::LexicalRepair, "rollout_verdict") => {
                sim.phase(
                    "many_core_rollout_gate",
                    "rollout verdict records utilization and responsiveness gates",
                );
                sim.snapshot_json(
                    "many_core_rollout_verdict",
                    &json!({
                        "verdict": "pass",
                        "phase_gates": {
                            "segment_farm_uses_many_cores": "pass",
                            "search_ready_time_improved": "pass",
                            "interactive_latency_preserved": "pass",
                            "deferred_settle_is_controller_limited": "pass"
                        },
                        "search_ready_improvement_ratio": 3.81,
                        "fully_settled_after_resume": true
                    }),
                );
                Ok(())
            }
            _ => unreachable!("unexpected deterministic many-core rollout turn"),
        });

    assert!(
        results.iter().all(Result::is_ok),
        "many-core rollout gate should not inject failures: {results:?}"
    );

    let summary = harness.summary();
    assert_eq!(summary.actor_traces.len(), 5);
    assert_eq!(summary.actor_traces[0].load.label, "legacy_serial_baseline");
    assert_eq!(summary.actor_traces[1].load.label, "segment_farm_build");
    assert_eq!(summary.actor_traces[2].load.label, "foreground_probe");
    assert!(summary.actor_traces[2].load.user_active);
    assert_eq!(summary.actor_traces[3].load.label, "settle_pressure");
    assert_eq!(summary.actor_traces[4].load.label, "fully_settled");

    for expected in [
        "001-phase_utilization_baseline.json",
        "002-phase_utilization_segment_farm.json",
        "003-foreground_responsiveness_gate.json",
        "004-settle_pressure_gate.json",
        "005-many_core_rollout_verdict.json",
    ] {
        assert!(
            summary.snapshot_digests.contains_key(expected),
            "missing many-core rollout snapshot digest for {expected}"
        );
    }

    let artifacts = harness
        .write_artifacts()
        .expect("write many-core rollout artifacts");
    assert!(artifacts.phase_log_path.exists());
    assert!(artifacts.actor_traces_path.exists());
    assert!(artifacts.summary_path.exists());

    let phase_log = fs::read_to_string(&artifacts.phase_log_path).expect("read phase log");
    assert!(
        phase_log.contains("many_core_segment_farm"),
        "phase log should preserve the segment-farm utilization phase"
    );
    assert!(
        phase_log.contains("foreground_responsiveness"),
        "phase log should preserve the foreground responsiveness phase"
    );

    let farm_path = artifacts
        .snapshot_dir
        .join("002-phase_utilization_segment_farm.json");
    let farm_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&farm_path).expect("read farm snapshot"))
            .expect("farm snapshot JSON");
    assert_eq!(farm_json["status"], "pass");
    assert_eq!(farm_json["active_workers"], 24);
    assert_eq!(farm_json["reserved_cores"], 4);

    let responsiveness_path = artifacts
        .snapshot_dir
        .join("003-foreground_responsiveness_gate.json");
    let responsiveness_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&responsiveness_path).expect("read responsiveness snapshot"),
    )
    .expect("responsiveness snapshot JSON");
    assert_eq!(responsiveness_json["status"], "pass");
    assert_eq!(responsiveness_json["blocked_wait_ms"], 0);

    let verdict_path = artifacts
        .snapshot_dir
        .join("005-many_core_rollout_verdict.json");
    let verdict_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&verdict_path).expect("read rollout verdict"))
            .expect("rollout verdict JSON");
    assert_eq!(verdict_json["verdict"], "pass");
    assert_eq!(
        verdict_json["phase_gates"]["segment_farm_uses_many_cores"],
        "pass"
    );
    assert_eq!(
        verdict_json["phase_gates"]["interactive_latency_preserved"],
        "pass"
    );
}

#[test]
fn robot_style_demo_is_deterministic_and_persists_artifacts() {
    let (first_summary, first_artifacts, first_results) = run_robot_style_demo();
    let (second_summary, second_artifacts, second_results) = run_robot_style_demo();

    assert_eq!(first_results.len(), 6);
    assert_eq!(first_results, second_results);
    assert_eq!(first_summary, second_summary);

    assert!(matches!(
        &first_results[2],
        Err(SimulationFailure::InjectedError { reason, .. }) if reason == "checksum mismatch"
    ));
    assert!(matches!(
        &first_results[4],
        Err(SimulationFailure::Crash { .. })
    ));
    assert!(first_results[5].is_ok());

    for artifacts in [first_artifacts, second_artifacts] {
        assert!(artifacts.phase_log_path.exists());
        assert!(artifacts.failpoints_path.exists());
        assert!(artifacts.actor_traces_path.exists());
        assert!(artifacts.summary_path.exists());

        let summary_json =
            fs::read_to_string(&artifacts.summary_path).expect("read deterministic summary");
        assert!(
            summary_json.contains("robot_style_publish_and_acquisition_demo"),
            "summary should include scenario name"
        );

        let fail_open_snapshot_path = artifacts
            .snapshot_dir
            .join("001-foreground_status_initial.json");
        let fail_open_snapshot: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&fail_open_snapshot_path).expect("read initial fail-open snapshot"),
        )
        .expect("fail-open snapshot should be valid JSON");
        assert_eq!(
            fail_open_snapshot["requested_search_mode"], "hybrid",
            "artifact should preserve requested hybrid intent"
        );
        assert_eq!(
            fail_open_snapshot["realized_search_mode"], "lexical",
            "artifact should preserve realized lexical fail-open mode"
        );
        assert_eq!(
            fail_open_snapshot["semantic_refinement"], false,
            "artifact should prove fail-open did not claim semantic refinement"
        );
        assert_eq!(
            fail_open_snapshot["fallback_tier"], "lexical",
            "artifact should name the fallback tier"
        );
        assert!(
            fail_open_snapshot["fallback_reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("semantic assets not ready")),
            "artifact should retain a diagnosable fallback reason"
        );

        let snapshot_entries = fs::read_dir(&artifacts.snapshot_dir)
            .expect("list snapshot dir")
            .count();
        assert!(
            snapshot_entries >= 4,
            "expected retained manifest/generation/status snapshots"
        );
    }
}
