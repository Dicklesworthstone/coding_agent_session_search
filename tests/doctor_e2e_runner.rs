mod util;

use std::collections::{BTreeMap, BTreeSet};
use util::doctor_e2e_runner::{
    DoctorE2eArtifactManifest, DoctorE2eCliArgs, DoctorE2eRunner, DoctorE2eScenarioSpec,
    default_doctor_e2e_run_root, default_doctor_e2e_scenarios, doctor_e2e_scenarios_for_args,
    parse_doctor_json_stdout, select_scenarios, validate_artifact_manifest,
    validate_artifact_manifest_value,
};
use util::doctor_fixture::{DoctorFixtureScenario, default_expected_artifact_keys};

#[test]
fn doctor_e2e_cli_args_parse_labels_scenarios_and_flags() {
    let parsed = DoctorE2eCliArgs::parse_from([
        "doctor_v2",
        "--label",
        "quick,privacy",
        "--scenario",
        "quick-source-pruned",
        "--fail-fast",
        "--include-failure-self-test",
    ])
    .expect("parse doctor e2e args");

    assert_eq!(
        parsed.label_filter,
        BTreeSet::from(["privacy".to_string(), "quick".to_string()])
    );
    assert_eq!(
        parsed.scenario_filter,
        BTreeSet::from(["quick-source-pruned".to_string()])
    );
    assert!(parsed.fail_fast);
    assert!(parsed.include_failure_self_test);
}

#[test]
fn doctor_e2e_label_filter_selects_matching_scenarios() {
    let scenarios = default_doctor_e2e_scenarios();
    let parsed = DoctorE2eCliArgs::parse_from(["doctor_v2", "--label", "fault"])
        .expect("parse label filter");
    let selected = select_scenarios(&parsed, &scenarios);

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].scenario_id, "quick-mirror-missing");
}

#[test]
fn doctor_e2e_include_failure_self_test_selects_intentional_failure() {
    let parsed = DoctorE2eCliArgs::parse_from([
        "doctor_v2",
        "--label",
        "quick",
        "--include-failure-self-test",
    ])
    .expect("parse self-test flag");
    let scenarios = doctor_e2e_scenarios_for_args(&parsed);
    let selected = select_scenarios(&parsed, &scenarios);

    assert!(
        selected
            .iter()
            .any(|scenario| scenario.scenario_id == "intentional-failure-self-test"),
        "include flag should add and select the failure self-test scenario"
    );
    let self_test = selected
        .iter()
        .find(|scenario| scenario.scenario_id == "intentional-failure-self-test")
        .expect("selected self-test scenario");
    assert_eq!(self_test.expected_runner_status(), "fail");
}

#[test]
fn doctor_e2e_runner_refuses_unsafe_run_roots() {
    let err = DoctorE2eRunner::new("relative/run-root").expect_err("relative root rejected");
    assert!(
        err.contains("must be absolute"),
        "error should explain unsafe root, got: {err}"
    );
}

#[test]
fn doctor_e2e_json_parse_failures_are_diagnostic() {
    let err = parse_doctor_json_stdout(b"not json").expect_err("invalid json rejected");
    assert!(
        err.contains("not valid JSON"),
        "parse failure should be actionable, got: {err}"
    );
}

#[test]
fn doctor_e2e_manifest_validation_rejects_missing_artifacts() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut artifacts = BTreeMap::new();
    for key in default_expected_artifact_keys() {
        artifacts.insert(key.to_string(), format!("{key}.missing"));
    }
    let manifest = DoctorE2eArtifactManifest {
        schema_version: 1,
        scenario_id: "missing-artifact".to_string(),
        labels: vec!["quick".to_string()],
        status: "pass".to_string(),
        artifact_dir: "[doctor-e2e-artifacts]".to_string(),
        fixture_root: "[doctor-e2e-fixture]".to_string(),
        home_dir: "[doctor-e2e-home]".to_string(),
        data_dir: "[doctor-e2e-data]".to_string(),
        command_count: 1,
        artifacts,
        failure_context: None,
    };

    let err = validate_artifact_manifest_value(temp.path(), &manifest)
        .expect_err("missing artifact paths rejected");
    assert!(
        err.contains("is missing"),
        "manifest validator should identify absent artifact files, got: {err}"
    );
}

#[test]
fn doctor_e2e_runner_records_artifacts_and_no_mutation_for_pruned_source() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let runner = DoctorE2eRunner::new(temp.path().join("run")).expect("runner");
    let spec = DoctorE2eScenarioSpec::new(
        "artifact-pruned-source",
        DoctorFixtureScenario::SourcePruned,
        ["quick", "source-mirror"],
    )
    .require_json_pointer("/source_inventory")
    .require_json_pointer("/raw_mirror")
    .require_json_pointer("/operation_outcome/kind")
    .require_json_pointer("/operation_state/mutating_doctor_allowed")
    .require_json_pointer("/source_authority/selected_authority");

    let result = runner.run_scenario(&spec).expect("run doctor e2e scenario");
    assert_eq!(result.status, "pass");
    validate_artifact_manifest(&result.manifest_path).expect("artifact manifest valid");

    for relative in [
        "manifest.json",
        "scenario.json",
        "fixture-inventory.json",
        "source-inventory-before.json",
        "source-inventory-after.json",
        "execution-flow.jsonl",
        "commands.jsonl",
        "stdout/doctor-json.out",
        "stderr/doctor-json.err",
        "parsed-json/doctor-json.json",
        "file-tree-before.json",
        "file-tree-after.json",
        "checksums.json",
        "timing.json",
        "receipts.jsonl",
        "doctor-events.jsonl",
    ] {
        assert!(
            result.artifact_dir.join(relative).exists(),
            "missing expected artifact {relative}"
        );
    }

    let stdout =
        std::fs::read_to_string(result.artifact_dir.join("stdout/doctor-json.out")).unwrap();
    assert!(
        !stdout.contains(temp.path().to_string_lossy().as_ref()),
        "stdout artifact should redact temp paths"
    );
    assert!(
        !stdout.contains("CASS_DOCTOR_PRIVACY_SENTINEL"),
        "stdout artifact should not leak privacy sentinels"
    );

    let doctor_events =
        std::fs::read_to_string(result.artifact_dir.join("doctor-events.jsonl")).unwrap();
    assert!(
        doctor_events.contains("\"phase\":\"operation_started\""),
        "doctor event artifact should preserve the real doctor operation event stream"
    );
    assert!(
        doctor_events.contains("\"hash_chain_tip\"")
            || doctor_events.contains("\"previous_event_hash\""),
        "doctor event artifact should include hash-chain evidence for debugging"
    );

    let fixture_inventory: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("fixture-inventory.json")).unwrap(),
    )
    .expect("fixture inventory json");
    assert_eq!(
        fixture_inventory["scenario_id"].as_str(),
        Some("artifact-pruned-source")
    );
    assert_eq!(
        fixture_inventory["db_row_counts"]["status"].as_str(),
        Some("ok")
    );
    assert_eq!(
        fixture_inventory["db_row_counts"]["agents"].as_u64(),
        Some(1)
    );
    assert_eq!(
        fixture_inventory["db_row_counts"]["conversations"].as_u64(),
        Some(1)
    );
    assert_eq!(
        fixture_inventory["db_row_counts"]["messages"].as_u64(),
        Some(2)
    );
    assert!(
        fixture_inventory["mirror_hash_inventory"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "fixture inventory should include raw mirror hash evidence"
    );
    let inventory_text =
        serde_json::to_string(&fixture_inventory).expect("serialize fixture inventory");
    assert!(
        !inventory_text.contains(temp.path().to_string_lossy().as_ref()),
        "fixture inventory should redact temp paths"
    );
    assert!(
        !inventory_text.contains("CASS_DOCTOR_PRIVACY_SENTINEL"),
        "fixture inventory should not leak privacy sentinels"
    );

    let source_before: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("source-inventory-before.json")).unwrap(),
    )
    .expect("source inventory before json");
    let source_after: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("source-inventory-after.json")).unwrap(),
    )
    .expect("source inventory after json");
    assert_eq!(source_before["phase"].as_str(), Some("before"));
    assert_eq!(source_after["phase"].as_str(), Some("after"));
    assert!(
        source_before["raw_mirror_files"]["tree_entry_count"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "before source inventory should include raw mirror file evidence"
    );
    assert_eq!(
        source_before["raw_mirror_files"]["tree_entry_count"],
        source_after["raw_mirror_files"]["tree_entry_count"],
        "read-only doctor run should not change raw mirror inventory"
    );

    let execution_flow =
        std::fs::read_to_string(result.artifact_dir.join("execution-flow.jsonl")).unwrap();
    for phase in [
        "source_discovery",
        "raw_mirror_hash",
        "parse_outcome",
        "db_projection_outcome",
        "source_inventory_before",
        "source_inventory_after",
        "mutation_audit",
    ] {
        assert!(
            execution_flow.contains(&format!("\"phase\":\"{phase}\"")),
            "execution flow should include phase {phase}: {execution_flow}"
        );
    }
}

#[test]
fn doctor_e2e_intentional_failure_preserves_failure_context_and_artifacts() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let runner = DoctorE2eRunner::new(temp.path().join("run")).expect("runner");
    let spec = DoctorE2eScenarioSpec::new(
        "intentional-failure",
        DoctorFixtureScenario::SourcePruned,
        ["quick", "self-test"],
    )
    .require_json_pointer("/definitely_missing_for_self_test");

    let result = runner
        .run_scenario(&spec)
        .expect("runner should return a failed result with artifacts");
    assert_eq!(result.status, "fail");
    let context = result.failure_context.expect("failure context");
    assert!(
        context
            .reasons
            .iter()
            .any(|reason| reason.contains("required JSON pointer")),
        "failure context should explain the assertion failure: {:?}",
        context.reasons
    );
    assert!(result.artifact_dir.join("failure_summary.txt").exists());
    validate_artifact_manifest(&result.manifest_path).expect("failed artifact manifest valid");
}

#[test]
fn doctor_e2e_scripted_scenarios() {
    let labels = std::env::var("CASS_DOCTOR_E2E_LABELS").unwrap_or_else(|_| "quick".to_string());
    let scenarios_arg = std::env::var("CASS_DOCTOR_E2E_SCENARIOS").unwrap_or_default();
    let mut args = vec!["doctor_v2".to_string(), "--label".to_string(), labels];
    if !scenarios_arg.trim().is_empty() {
        args.push("--scenario".to_string());
        args.push(scenarios_arg);
    }
    if std::env::var("CASS_DOCTOR_E2E_INCLUDE_FAILURE_SELF_TEST").is_ok() {
        args.push("--include-failure-self-test".to_string());
    }
    let parsed = DoctorE2eCliArgs::parse_from(args).expect("parse scripted args");
    let scenarios = doctor_e2e_scenarios_for_args(&parsed);
    let selected = select_scenarios(&parsed, &scenarios);
    assert!(
        !selected.is_empty(),
        "doctor e2e script selection should choose at least one scenario"
    );

    let run_root = std::env::var("CASS_DOCTOR_E2E_RUN_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| default_doctor_e2e_run_root());
    let runner = DoctorE2eRunner::new(&run_root).expect("runner");
    for scenario in selected {
        let result = runner
            .run_scenario(scenario)
            .expect("run scripted scenario");
        assert_eq!(
            result.status,
            scenario.expected_runner_status(),
            "scripted doctor scenario should produce the expected status with artifacts at {}",
            result.artifact_dir.display()
        );
        if parsed.fail_fast && result.status == "fail" {
            break;
        }
    }
}
