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
