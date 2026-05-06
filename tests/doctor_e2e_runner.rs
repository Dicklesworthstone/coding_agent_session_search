mod util;

use std::collections::{BTreeMap, BTreeSet};
use util::doctor_e2e_runner::{
    DoctorE2eArtifactManifest, DoctorE2eCliArgs, DoctorE2eRunner, DoctorE2eScenarioSpec,
    default_doctor_e2e_run_root, default_doctor_e2e_scenarios, doctor_e2e_run_error_summary,
    doctor_e2e_run_result_summary, doctor_e2e_run_summary_manifest,
    doctor_e2e_scenario_registry_manifest, doctor_e2e_scenarios_for_args,
    doctor_e2e_shell_quote_arg, parse_doctor_json_stdout, select_scenarios,
    validate_artifact_manifest, validate_artifact_manifest_value,
    validate_doctor_e2e_run_summary_manifest_value,
    validate_doctor_e2e_scenario_registry_manifest_value,
};
use util::doctor_fixture::{
    DoctorFixtureFactory, DoctorFixtureScenario, default_expected_artifact_keys,
};
use walkdir::WalkDir;

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
    let selected_ids = selected
        .iter()
        .map(|scenario| scenario.scenario_id.as_str())
        .collect::<BTreeSet<_>>();

    assert_eq!(
        selected_ids,
        BTreeSet::from([
            "backups-restore-rollback-failpoint",
            "candidate-promote-corrupt-db-rollback-before-parent-sync",
            "candidate-promote-corrupt-db-rollback-failpoint",
            "quick-mirror-missing",
        ])
    );
    assert!(
        selected
            .iter()
            .all(|scenario| scenario.labels.contains("fault")),
        "fault label filter should only select fault-labelled scenarios: {selected_ids:?}"
    );
}

#[test]
fn doctor_e2e_exclude_filters_remove_matching_scenarios() {
    let scenarios = default_doctor_e2e_scenarios();
    let parsed = DoctorE2eCliArgs::parse_from([
        "doctor_v2",
        "--label",
        "quick",
        "--exclude-label",
        "low-disk",
        "--exclude-scenario",
        "quick-source-truncated",
    ])
    .expect("parse exclude filters");
    let selected = select_scenarios(&parsed, &scenarios);
    let selected_ids = selected
        .iter()
        .map(|scenario| scenario.scenario_id.as_str())
        .collect::<Vec<_>>();

    assert!(
        selected_ids.contains(&"quick-source-pruned"),
        "ordinary quick scenario should remain selected: {selected_ids:?}"
    );
    assert!(
        !selected_ids.contains(&"quick-source-truncated"),
        "explicit scenario exclusion should win: {selected_ids:?}"
    );
    assert!(
        !selected_ids.contains(&"cleanup-low-disk-derived-only"),
        "label exclusion should remove low-disk scenario: {selected_ids:?}"
    );
}

#[test]
fn doctor_e2e_backups_restore_journey_is_registered_as_fixture_mutation() {
    let scenarios = default_doctor_e2e_scenarios();
    let parsed = DoctorE2eCliArgs::parse_from(["doctor_v2", "--label", "backups"])
        .expect("parse backups label filter");
    let selected = select_scenarios(&parsed, &scenarios);
    let selected_ids = selected
        .iter()
        .map(|scenario| scenario.scenario_id.as_str())
        .collect::<BTreeSet<_>>();

    assert_eq!(
        selected_ids,
        BTreeSet::from([
            "backups-restore-fixture-journey",
            "backups-restore-rollback-failpoint",
        ])
    );

    let manifest = doctor_e2e_scenario_registry_manifest(&parsed, &scenarios, &selected);
    validate_doctor_e2e_scenario_registry_manifest_value(&manifest)
        .expect("backups scenario manifest should validate");
    let scenario = manifest["scenarios"]
        .as_array()
        .expect("scenario registry array")
        .iter()
        .find(|scenario| {
            scenario["scenario_id"].as_str() == Some("backups-restore-fixture-journey")
        })
        .expect("backups scenario entry");
    assert_eq!(
        scenario["command_mode"].as_str(),
        Some("backups-restore-journey")
    );
    assert_eq!(
        scenario["expected_mutation_class"].as_str(),
        Some("fixture-only-mutation")
    );
    assert!(
        scenario["required_json_pointers"]
            .as_array()
            .expect("required pointers")
            .iter()
            .any(|pointer| pointer.as_str() == Some("/restore_apply/receipt_path")),
        "backups e2e scenario should require restore apply receipt evidence: {scenario:#}"
    );

    let rollback_scenario = manifest["scenarios"]
        .as_array()
        .expect("scenario registry array")
        .iter()
        .find(|scenario| {
            scenario["scenario_id"].as_str() == Some("backups-restore-rollback-failpoint")
        })
        .expect("backup rollback scenario entry");
    assert!(
        rollback_scenario["required_json_pointers"]
            .as_array()
            .expect("required pointers")
            .iter()
            .any(|pointer| {
                pointer.as_str() == Some("/restore_apply/candidate_promotion/rollback_reference")
            }),
        "backup rollback scenario should require rollback reference evidence: {rollback_scenario:#}"
    );
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
fn doctor_e2e_scenario_registry_manifest_is_self_describing() {
    let parsed = DoctorE2eCliArgs::parse_from([
        "doctor_v2",
        "--label",
        "quick",
        "--scenario",
        "quick-source-pruned",
    ])
    .expect("parse scenario manifest args");
    let scenarios = doctor_e2e_scenarios_for_args(&parsed);
    let selected = select_scenarios(&parsed, &scenarios);
    let manifest = doctor_e2e_scenario_registry_manifest(&parsed, &scenarios, &selected);
    validate_doctor_e2e_scenario_registry_manifest_value(&manifest)
        .expect("scenario registry manifest should validate");

    assert_eq!(
        manifest["manifest_kind"].as_str(),
        Some("cass_doctor_e2e_scenario_registry_v1")
    );
    assert_eq!(manifest["selected_scenario_count"].as_u64(), Some(1));
    assert_eq!(
        manifest["safety_contract"]["uses_fixture_data_only"].as_bool(),
        Some(true)
    );
    assert_eq!(
        manifest["safety_contract"]["launches_bare_cass_tui"].as_bool(),
        Some(false)
    );
    let selected_scenario = manifest["scenarios"]
        .as_array()
        .expect("scenario list")
        .iter()
        .find(|scenario| scenario["selected"].as_bool() == Some(true))
        .expect("selected scenario manifest");
    assert_eq!(
        selected_scenario["scenario_id"].as_str(),
        Some("quick-source-pruned")
    );
    assert!(
        selected_scenario["safe_rerun_command"]
            .as_str()
            .is_some_and(|command| command
                .contains("scripts/e2e/doctor_v2.sh run --scenario quick-source-pruned")
                && command.contains("--artifact-dir <absolute-base-dir>")),
        "scenario manifest should include a safe rerun command: {selected_scenario:#}"
    );
    assert_eq!(
        selected_scenario["expected_mutation_class"].as_str(),
        Some("read-only")
    );
    assert_eq!(
        selected_scenario["local_execution_class"].as_str(),
        Some("local-quick-read-only")
    );
}

#[test]
fn doctor_e2e_run_summary_manifest_is_self_describing() {
    let parsed = DoctorE2eCliArgs::parse_from([
        "doctor_v2",
        "--scenario",
        "quick-source-pruned",
        "--exclude-label",
        "mutation",
    ])
    .expect("parse doctor e2e args");
    let scenarios = doctor_e2e_scenarios_for_args(&parsed);
    let selected = select_scenarios(&parsed, &scenarios);
    let scenario = selected
        .first()
        .expect("selected scenario for run summary test");
    let result = util::doctor_e2e_runner::DoctorE2eRunResult {
        scenario_id: scenario.scenario_id.clone(),
        status: "pass".to_string(),
        artifact_dir: std::path::PathBuf::from("/tmp/cass-doctor-v2/artifacts/quick-source-pruned"),
        manifest_path: std::path::PathBuf::from(
            "/tmp/cass-doctor-v2/artifacts/quick-source-pruned/manifest.json",
        ),
        failure_context: None,
    };
    let mut summary = doctor_e2e_run_result_summary(scenario, &result);
    assert_eq!(
        summary["next_suggested_command"].as_str(),
        Some(
            "scripts/e2e/doctor_v2.sh run --scenario quick-source-pruned --artifact-dir <absolute-base-dir>"
        )
    );
    assert_eq!(
        summary["expected_mutation_class"].as_str(),
        Some("read-only")
    );
    assert!(summary["log_paths"]["commands_jsonl"].as_str().is_some());

    summary["runner_status_matches_expected"] = serde_json::Value::Bool(true);
    let manifest = doctor_e2e_run_summary_manifest(
        &parsed,
        std::path::Path::new("/tmp/cass-doctor-v2"),
        vec![summary],
    );
    validate_doctor_e2e_run_summary_manifest_value(&manifest)
        .expect("run summary manifest should validate");
    assert_eq!(manifest["status"].as_str(), Some("pass"));

    let error_summary = doctor_e2e_run_error_summary(scenario, "fixture setup failed");
    let failed_manifest = doctor_e2e_run_summary_manifest(
        &parsed,
        std::path::Path::new("/tmp/cass-doctor-v2"),
        vec![error_summary],
    );
    validate_doctor_e2e_run_summary_manifest_value(&failed_manifest)
        .expect("failed run summary manifest should validate");
    assert_eq!(failed_manifest["status"].as_str(), Some("fail"));
}

#[test]
fn doctor_fixture_source_truncation_keeps_mirror_and_present_source_distinct() {
    let mut fixture = DoctorFixtureFactory::new("source-truncated-helper");
    fixture.apply_scenario(DoctorFixtureScenario::SourceTruncated);
    fixture
        .validate_manifest()
        .expect("truncated-source fixture manifest should remain internally consistent");

    let manifest = fixture.manifest();
    assert_eq!(
        manifest.expected_coverage_state,
        "source-truncated-mirror-verified"
    );
    assert_eq!(
        manifest
            .expected_source_inventory
            .missing_current_source_count,
        0,
        "fixture should model source truncation without pretending the source file is gone"
    );
    assert_eq!(
        manifest.expected_source_inventory.mirrored_source_count, 1,
        "fixture should keep the pre-truncation raw mirror as recovery evidence"
    );
    assert!(
        manifest
            .expected_anomalies
            .iter()
            .any(|anomaly| anomaly == "upstream-source-truncated")
    );
    assert!(
        manifest.artifacts.iter().any(|artifact| {
            artifact.artifact_kind == "provider_source_truncated"
                && artifact.relative_path.contains(".codex/")
        }),
        "fixture should record the truncated provider source artifact"
    );
    assert!(
        manifest.structured_log.iter().any(|entry| {
            entry.step == "overwrite_file_for_fixture_drift"
                && entry.detail.contains("provider_source_truncated")
        }),
        "fixture should log that upstream bytes drifted after mirror capture"
    );
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
fn doctor_e2e_failure_context_shell_quotes_repro_args() {
    assert_eq!(doctor_e2e_shell_quote_arg("plain/path"), "plain/path");
    assert_eq!(doctor_e2e_shell_quote_arg(""), "''");
    assert_eq!(
        doctor_e2e_shell_quote_arg("path with spaces"),
        "'path with spaces'"
    );
    assert_eq!(
        doctor_e2e_shell_quote_arg("can't-delete"),
        "'can'\"'\"'t-delete'"
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
    .require_json_pointer("/doctor_command/surface")
    .require_json_pointer("/check_scope/skipped_expensive_collectors")
    .require_json_pointer("/active_repair")
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
        "candidate-staging.json",
        "post-repair-probes.json",
        "file-tree-before.json",
        "file-tree-after.json",
        "checksums.json",
        "timing.json",
        "receipts.jsonl",
        "doctor-events.jsonl",
        "redaction-report.json",
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
    let redaction_report: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("redaction-report.json")).unwrap(),
    )
    .expect("redaction report json");
    assert_eq!(redaction_report["status"].as_str(), Some("pass"));
    assert_eq!(redaction_report["leak_count"].as_u64(), Some(0));
    assert_eq!(
        redaction_report["raw_needles_included"].as_bool(),
        Some(false),
        "redaction report must identify leak checks by id/hash, not by embedding raw secrets"
    );
    assert!(
        redaction_report["checks"]
            .as_array()
            .is_some_and(|checks| checks
                .iter()
                .any(|check| check["needle_id"].as_str() == Some("privacy_sentinel_value"))),
        "redaction report should explicitly check seeded privacy sentinels: {redaction_report:#}"
    );
    assert_default_artifacts_do_not_leak_sensitive_values(
        &result.artifact_dir,
        temp.path().to_string_lossy().as_ref(),
        "CASS_DOCTOR_PRIVACY_SENTINEL",
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
    assert!(
        execution_flow.contains("\"doctor_command\"")
            && execution_flow.contains("\"surface\":\"check\""),
        "execution flow should record that read-only scenarios exercise doctor check: {execution_flow}"
    );
}

#[test]
fn doctor_e2e_runner_redaction_report_covers_seeded_support_bundle_sentinel() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let runner = DoctorE2eRunner::new(temp.path().join("run")).expect("runner");
    let spec = DoctorE2eScenarioSpec::new(
        "privacy-support-bundle-sentinel-test",
        DoctorFixtureScenario::SupportBundle,
        ["privacy", "support-bundle"],
    )
    .require_json_pointer("/raw_mirror/policy/support_bundle_policy")
    .require_json_pointer("/operation_outcome/kind");

    let result = runner
        .run_scenario(&spec)
        .expect("run privacy support-bundle sentinel scenario");
    assert_eq!(result.status, "pass");
    validate_artifact_manifest(&result.manifest_path).expect("artifact manifest valid");

    let redaction_report: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("redaction-report.json")).unwrap(),
    )
    .expect("redaction report json");
    assert_eq!(redaction_report["status"].as_str(), Some("pass"));
    assert_eq!(redaction_report["leak_count"].as_u64(), Some(0));
    assert_eq!(
        redaction_report["privacy_sentinel_count"].as_u64(),
        Some(1),
        "support-bundle fixture should seed a real sensitive attachment sentinel"
    );
    assert_eq!(
        redaction_report["redaction_policy"]["sensitive_attachments_require_opt_in"].as_bool(),
        Some(true)
    );
    assert!(
        redaction_report["checks"]
            .as_array()
            .expect("redaction checks")
            .iter()
            .any(
                |check| check["needle_id"].as_str() == Some("privacy_sentinel_value")
                    && check["status"].as_str() == Some("pass")
            ),
        "redaction report should prove the seeded secret was scanned and absent: {redaction_report:#}"
    );
    assert_default_artifacts_do_not_leak_sensitive_values(
        &result.artifact_dir,
        temp.path().to_string_lossy().as_ref(),
        "CASS_DOCTOR_PRIVACY_SENTINEL",
    );
}

#[test]
fn doctor_e2e_runner_records_truncated_source_with_verified_mirror() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let runner = DoctorE2eRunner::new(temp.path().join("run")).expect("runner");
    let spec = DoctorE2eScenarioSpec::new(
        "artifact-source-truncated",
        DoctorFixtureScenario::SourceTruncated,
        ["quick", "source-mirror", "truncated"],
    )
    .require_json_pointer("/source_inventory")
    .require_json_pointer("/raw_mirror")
    .require_json_pointer("/coverage_summary")
    .require_json_pointer("/source_authority/selected_authority");

    let result = runner
        .run_scenario(&spec)
        .expect("run truncated-source doctor e2e scenario");
    assert_eq!(result.status, "pass");
    validate_artifact_manifest(&result.manifest_path).expect("artifact manifest valid");

    let source_before: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("source-inventory-before.json")).unwrap(),
    )
    .expect("source inventory before json");
    let source_after: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("source-inventory-after.json")).unwrap(),
    )
    .expect("source inventory after json");
    assert!(
        source_before["upstream_source_files"]["tree_entry_count"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "truncated-source fixture should keep the upstream path present"
    );
    assert_eq!(
        source_before["source_discovery"]["expected_missing_current_source_count"].as_u64(),
        Some(0),
        "truncated source is degraded evidence, not a missing-source fixture"
    );
    assert_eq!(
        source_before["raw_mirror_files"]["tree_entry_count"],
        source_after["raw_mirror_files"]["tree_entry_count"],
        "read-only truncated-source check must not rewrite raw mirror evidence"
    );
    let structured_log = source_before["source_discovery"]["structured_fixture_log"]
        .as_array()
        .expect("structured fixture log");
    assert!(
        structured_log.iter().any(|entry| {
            entry["step"].as_str() == Some("overwrite_file_for_fixture_drift")
                && entry["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.contains("provider_source_truncated"))
        }),
        "fixture log should prove the upstream source was truncated after mirror capture: {structured_log:#?}"
    );

    let payload: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("parsed-json/doctor-json.json")).unwrap(),
    )
    .expect("doctor json artifact");
    assert_eq!(
        payload["source_inventory"]["missing_current_source_count"].as_u64(),
        Some(0),
        "doctor should distinguish present-but-truncated sources from removed sources"
    );
    assert_eq!(payload["raw_mirror"]["status"].as_str(), Some("verified"));
    assert_eq!(
        payload["raw_mirror"]["manifests"][0]["upstream_path_exists"].as_bool(),
        Some(true),
        "raw mirror report should record that the upstream path still exists"
    );
    assert_eq!(
        payload["coverage_summary"]["raw_mirror_db_link_count"].as_u64(),
        Some(1),
        "coverage summary should keep the verified mirror link after source truncation"
    );
    let stdout =
        std::fs::read_to_string(result.artifact_dir.join("stdout/doctor-json.out")).unwrap();
    assert!(
        !stdout.contains("truncated after mirror"),
        "doctor JSON must not leak truncated source bytes"
    );

    let execution_flow =
        std::fs::read_to_string(result.artifact_dir.join("execution-flow.jsonl")).unwrap();
    for field in [
        "source_discovery",
        "raw_mirror_hash",
        "source_inventory_before",
        "source_inventory_after",
        "mutation_audit",
    ] {
        assert!(
            execution_flow.contains(field),
            "truncated-source execution flow should include {field}: {execution_flow}"
        );
    }
}

#[test]
fn doctor_e2e_runner_reports_no_safe_rebuild_authority_without_mirror() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let runner = DoctorE2eRunner::new(temp.path().join("run")).expect("runner");
    let spec = DoctorE2eScenarioSpec::new(
        "artifact-mirror-missing",
        DoctorFixtureScenario::MirrorMissing,
        ["quick", "source-mirror", "fault"],
    )
    .require_json_pointer("/source_inventory")
    .require_json_pointer("/raw_mirror")
    .require_json_pointer("/coverage_summary")
    .require_json_pointer("/coverage_risk")
    .require_json_pointer("/source_authority")
    .require_json_pointer("/candidate_staging");

    let result = runner
        .run_scenario(&spec)
        .expect("run mirror-missing doctor e2e scenario");
    assert_eq!(result.status, "pass");
    validate_artifact_manifest(&result.manifest_path).expect("artifact manifest valid");

    let payload: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("parsed-json/doctor-json.json")).unwrap(),
    )
    .expect("doctor json artifact");
    assert_eq!(
        payload["source_inventory"]["missing_current_source_count"].as_u64(),
        Some(1),
        "mirror-missing fixture should report the pruned upstream source"
    );
    assert_eq!(
        payload["raw_mirror"]["summary"]["manifest_count"].as_u64(),
        Some(0),
        "mirror-missing fixture should not invent raw mirror manifests"
    );
    assert_eq!(
        payload["coverage_summary"]["db_without_raw_mirror_count"].as_u64(),
        Some(1),
        "coverage summary should flag archive rows without mirror evidence"
    );
    assert_eq!(
        payload["coverage_summary"]["coverage_reducing_live_source_rebuild_refused"].as_bool(),
        Some(true),
        "doctor must refuse source-session-only rebuild when it would shrink coverage"
    );
    let selected_authorities = payload["source_authority"]["selected_authorities"]
        .as_array()
        .expect("selected authorities");
    assert!(
        selected_authorities
            .iter()
            .all(|candidate| candidate["authority"].as_str() != Some("verified_raw_mirror")),
        "verified raw mirror must not be selected when no mirror exists: {:#}",
        payload["source_authority"]
    );
    assert!(
        payload["source_authority"]["rejected_authorities"]
            .as_array()
            .expect("rejected authorities")
            .iter()
            .any(|candidate| {
                candidate["authority"].as_str() == Some("live_upstream_source")
                    && candidate["evidence"].as_array().is_some_and(|evidence| {
                        evidence.iter().any(|entry| {
                            entry.as_str() == Some("coverage-shrinks-relative-to-archive")
                        })
                    })
            }),
        "live upstream source should be rejected with coverage-shrink evidence: {:#}",
        payload["source_authority"]
    );
    assert!(
        payload["candidate_staging"]["latest_build"].is_null(),
        "read-only mirror-missing check should not stage a candidate"
    );

    let source_before: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("source-inventory-before.json")).unwrap(),
    )
    .expect("source inventory before json");
    assert_eq!(
        source_before["raw_mirror_files"]["tree_entry_count"].as_u64(),
        Some(0),
        "source inventory should prove there were no raw mirror files"
    );
    let execution_flow =
        std::fs::read_to_string(result.artifact_dir.join("execution-flow.jsonl")).unwrap();
    assert!(
        execution_flow.contains("\"status\":\"unchanged\""),
        "mirror-missing read-only run should preserve no-mutation evidence: {execution_flow}"
    );
}

#[test]
fn doctor_e2e_runner_builds_candidate_with_fix_and_logs_lifecycle() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let runner = DoctorE2eRunner::new(temp.path().join("run")).expect("runner");
    let spec = DoctorE2eScenarioSpec::new(
        "artifact-candidate-build",
        DoctorFixtureScenario::SourcePruned,
        ["candidate", "source-mirror"],
    )
    .allow_mutation(true)
    .require_json_pointer("/candidate_staging")
    .require_json_pointer("/candidate_staging/latest_build")
    .require_json_pointer("/candidate_staging/latest_build/candidate_id")
    .require_json_pointer("/candidate_staging/latest_build/live_inventory_before")
    .require_json_pointer("/candidate_staging/latest_build/live_inventory_after")
    .require_json_pointer("/candidate_staging/latest_build/manifest_path");

    let result = runner
        .run_scenario(&spec)
        .expect("run candidate-build doctor e2e scenario");
    assert_eq!(result.status, "pass");
    validate_artifact_manifest(&result.manifest_path).expect("artifact manifest valid");

    let candidate_staging: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("candidate-staging.json")).unwrap(),
    )
    .expect("candidate staging artifact json");
    let latest_build = &candidate_staging["latest_build"];
    assert_eq!(
        latest_build["status"].as_str(),
        Some("completed"),
        "mutating doctor e2e should build a terminal candidate: {candidate_staging:#}"
    );
    assert!(
        latest_build["candidate_id"]
            .as_str()
            .is_some_and(|id| !id.trim().is_empty()),
        "candidate build should record a stable candidate_id: {candidate_staging:#}"
    );
    assert_eq!(
        latest_build["candidate_conversation_count"].as_u64(),
        Some(1),
        "candidate DB should preserve the fixture conversation row"
    );
    assert_eq!(
        latest_build["candidate_message_count"].as_u64(),
        Some(2),
        "candidate DB should preserve fixture messages"
    );
    assert_eq!(
        latest_build["live_inventory_unchanged"].as_bool(),
        Some(true),
        "candidate build must prove live DB/index inventory is unchanged before any promotion"
    );
    assert!(
        latest_build["checksum_count"]
            .as_u64()
            .is_some_and(|count| count >= 6),
        "candidate should checksum DB, logs, receipts, and derived metadata: {candidate_staging:#}"
    );
    assert!(
        latest_build["selected_authority_evidence"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item
                .as_str()
                .is_some_and(|text| text.starts_with("verified-blob-count=")))),
        "candidate e2e should prove raw mirror evidence contributed to the authority decision"
    );
    assert_eq!(
        candidate_staging["completed_candidate_count"].as_u64(),
        Some(1),
        "candidate staging inventory should report the completed candidate"
    );

    let after_tree: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("file-tree-after.json")).unwrap(),
    )
    .expect("after file tree json");
    let data_entries = after_tree["roots"]
        .as_array()
        .and_then(|roots| {
            roots
                .iter()
                .find(|root| root["root_id"].as_str() == Some("data"))
        })
        .and_then(|root| root["entries"].as_array())
        .expect("data tree entries");
    for expected_suffix in [
        "manifest.json",
        "database/candidate.db",
        "logs/skipped-records.jsonl",
        "logs/parse-errors.jsonl",
        "receipts/fs-mutations.jsonl",
        "index/lexical/candidate-generation.json",
        "index/semantic/metadata.json",
    ] {
        assert!(
            data_entries.iter().any(|entry| {
                entry["relative_path"].as_str().is_some_and(|path| {
                    path.starts_with("doctor/candidates/") && path.ends_with(expected_suffix)
                })
            }),
            "candidate file tree should include {expected_suffix}: {after_tree:#}"
        );
    }

    let execution_flow =
        std::fs::read_to_string(result.artifact_dir.join("execution-flow.jsonl")).unwrap();
    assert!(
        execution_flow.contains("\"phase\":\"candidate_staging\""),
        "execution flow should include a candidate_staging phase: {execution_flow}"
    );
    for field in [
        "candidate_id",
        "lifecycle_status",
        "manifest_path",
        "checksum_count",
        "skipped_record_count",
        "parse_error_count",
        "selected_authority",
        "evidence_sources",
        "coverage_before",
        "coverage_after",
        "confidence",
        "live_inventory_before",
        "live_inventory_after",
        "live_inventory_unchanged",
    ] {
        assert!(
            execution_flow.contains(field),
            "candidate e2e log should include {field}: {execution_flow}"
        );
    }
}

#[test]
fn doctor_e2e_runner_cleanup_low_disk_prunes_only_derived_and_logs() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let runner = DoctorE2eRunner::new(temp.path().join("run")).expect("runner");
    let spec = DoctorE2eScenarioSpec::new(
        "artifact-cleanup-low-disk",
        DoctorFixtureScenario::LowDisk,
        ["quick", "cleanup", "low-disk"],
    )
    .cleanup_apply()
    .env("CASS_TEST_DOCTOR_STORAGE_AVAILABLE_BYTES", "1024")
    .require_json_pointer("/storage_pressure")
    .require_json_pointer("/quarantine/lexical_cleanup_dry_run")
    .require_json_pointer("/cleanup_apply")
    .require_json_pointer("/cleanup_apply/actions")
    .require_json_pointer("/candidate_staging");

    let result = runner
        .run_scenario(&spec)
        .expect("run low-disk cleanup doctor e2e scenario");
    assert_eq!(result.status, "pass");
    validate_artifact_manifest(&result.manifest_path).expect("artifact manifest valid");

    let payload: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("parsed-json/doctor-json.json")).unwrap(),
    )
    .expect("doctor cleanup apply json");
    assert_eq!(payload["storage_pressure"]["status"].as_str(), Some("warn"));
    assert_eq!(
        payload["storage_pressure"]["available_bytes"].as_u64(),
        Some(1024),
        "low-disk E2E must use the deterministic storage-pressure override"
    );
    assert_eq!(
        payload["storage_pressure"]["low_disk_risk"].as_str(),
        Some("low_free_space"),
        "storage pressure should classify the deterministic override as low free space"
    );
    assert!(
        payload["storage_pressure"]["reclaimable_bytes_by_class"]
            .get("reclaimable_derived_cache")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|bytes| bytes > 0),
        "storage pressure should expose derived reclaimable bytes: {payload:#}"
    );
    for precious_class in [
        "backup_bundle",
        "bookmark_store",
        "canonical_archive_db",
        "operation_receipt",
        "raw_mirror_blob",
        "support_bundle",
        "user_config",
    ] {
        assert!(
            payload["storage_pressure"]["precious_bytes_by_class"]
                .get(precious_class)
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|bytes| bytes > 0),
            "storage pressure should account for precious class {precious_class}: {payload:#}"
        );
        assert!(
            payload["storage_pressure"]["reclaimable_bytes_by_class"]
                .get(precious_class)
                .is_none(),
            "precious class {precious_class} must not be reported as reclaimable: {payload:#}"
        );
    }
    assert!(
        payload["storage_pressure"]["recommended_action"]
            .as_str()
            .is_some_and(|action| action.contains("fingerprint-approved derived cleanup")),
        "low-disk guidance should point to explicit derived cleanup approval: {payload:#}"
    );
    let cleanup = &payload["cleanup_apply"];
    assert_eq!(cleanup["requested"].as_bool(), Some(true));
    assert_eq!(cleanup["applied"].as_bool(), Some(true));
    assert_eq!(cleanup["pruned_asset_count"].as_u64(), Some(1));
    assert!(
        cleanup["actions"]
            .as_array()
            .expect("cleanup actions")
            .iter()
            .all(|action| {
                action["artifact_kind"].as_str() == Some("lexical_generation")
                    && action["asset_class"].as_str() == Some("reclaimable_derived_cache")
                    && action["safety_classification"].as_str() == Some("derived_reclaimable")
                    && action["disposition"].as_str() == Some("failed_reclaimable")
                    && action["applied"].as_bool() == Some(true)
            }),
        "low-disk cleanup may only apply derived generation cleanup actions: {cleanup:#}"
    );

    let before_tree: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("file-tree-before.json")).unwrap(),
    )
    .expect("before file tree json");
    let after_tree: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("file-tree-after.json")).unwrap(),
    )
    .expect("after file tree json");
    let before_data = data_file_hashes(&before_tree);
    let after_data = data_file_hashes(&after_tree);

    assert!(
        before_data
            .keys()
            .any(|path| path.starts_with("index/generation-failed-reclaimable/")),
        "fixture should seed a failed derived generation before cleanup: {before_tree:#}"
    );
    assert!(
        !after_data
            .keys()
            .any(|path| path.starts_with("index/generation-failed-reclaimable/")),
        "cleanup apply should remove the failed derived generation only: {after_tree:#}"
    );

    for protected_path in [
        "agent_search.db",
        "backups/low-disk-agent_search.db.bak",
        "doctor/receipts/prior-cleanup-receipt.json",
        "doctor/support-bundles/prior-support-bundle.json",
        "sources.toml",
        "bookmarks.json",
    ] {
        assert_eq!(
            before_data.get(protected_path),
            after_data.get(protected_path),
            "cleanup must preserve protected file {protected_path}"
        );
    }

    let raw_mirror_before = filtered_hashes(&before_data, "raw-mirror/v1/");
    let raw_mirror_after = filtered_hashes(&after_data, "raw-mirror/v1/");
    assert!(
        !raw_mirror_before.is_empty(),
        "low-disk fixture should include raw mirror evidence"
    );
    assert_eq!(
        raw_mirror_before, raw_mirror_after,
        "cleanup must not rewrite or prune raw mirror evidence"
    );

    let commands = std::fs::read_to_string(result.artifact_dir.join("commands.jsonl")).unwrap();
    assert!(
        commands.contains("\"command_id\":\"doctor-cleanup-preview\"")
            && commands.contains("\"command_id\":\"doctor-json\"")
            && commands.contains("CASS_TEST_DOCTOR_STORAGE_AVAILABLE_BYTES"),
        "commands log should include preview, apply, and low-disk override evidence: {commands}"
    );
    let execution_flow =
        std::fs::read_to_string(result.artifact_dir.join("execution-flow.jsonl")).unwrap();
    for phase in [
        "storage_pressure",
        "cleanup_apply",
        "mutation_audit",
        "source_inventory_before",
        "source_inventory_after",
    ] {
        assert!(
            execution_flow.contains(&format!("\"phase\":\"{phase}\"")),
            "low-disk cleanup execution log should include phase {phase}: {execution_flow}"
        );
    }
}

fn data_file_hashes(tree: &serde_json::Value) -> BTreeMap<String, String> {
    tree["roots"]
        .as_array()
        .and_then(|roots| {
            roots
                .iter()
                .find(|root| root["root_id"].as_str() == Some("data"))
        })
        .and_then(|root| root["entries"].as_array())
        .expect("data tree entries")
        .iter()
        .filter(|entry| entry["entry_kind"].as_str() == Some("file"))
        .filter_map(|entry| {
            Some((
                entry["relative_path"].as_str()?.to_string(),
                entry["blake3"].as_str()?.to_string(),
            ))
        })
        .collect()
}

fn filtered_hashes(entries: &BTreeMap<String, String>, prefix: &str) -> BTreeMap<String, String> {
    entries
        .iter()
        .filter(|(path, _)| path.starts_with(prefix))
        .map(|(path, hash)| (path.clone(), hash.clone()))
        .collect()
}

fn assert_default_artifacts_do_not_leak_sensitive_values(
    artifact_dir: &std::path::Path,
    sensitive_path_prefix: &str,
    privacy_sentinel_prefix: &str,
) {
    for entry in WalkDir::new(artifact_dir)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry.expect("walk doctor e2e artifact");
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let bytes = std::fs::read(path).expect("read doctor e2e artifact");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(sensitive_path_prefix),
            "default artifact leaked sensitive fixture path prefix in {}",
            path.display()
        );
        assert!(
            !text.contains(privacy_sentinel_prefix),
            "default artifact leaked privacy sentinel prefix in {}",
            path.display()
        );
    }
}

#[test]
fn doctor_e2e_runner_reconstructs_candidate_from_mirror_when_db_is_corrupt() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let runner = DoctorE2eRunner::new(temp.path().join("run")).expect("runner");
    let spec = DoctorE2eScenarioSpec::new(
        "artifact-corrupt-db-mirror-reconstruct",
        DoctorFixtureScenario::DbCorrupt,
        ["candidate", "archive-corrupt", "source-mirror"],
    )
    .allow_mutation(true)
    .require_json_pointer("/raw_mirror")
    .require_json_pointer("/candidate_staging/latest_build")
    .require_json_pointer("/candidate_staging/latest_build/evidence_sources")
    .require_json_pointer("/candidate_staging/latest_build/coverage_before")
    .require_json_pointer("/candidate_staging/latest_build/coverage_after")
    .require_json_pointer("/candidate_staging/latest_build/confidence");

    let result = runner
        .run_scenario(&spec)
        .expect("run corrupt-db mirror reconstruction scenario");
    assert_eq!(result.status, "pass");
    validate_artifact_manifest(&result.manifest_path).expect("artifact manifest valid");

    let candidate_staging: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("candidate-staging.json")).unwrap(),
    )
    .expect("candidate staging artifact json");
    let latest_build = &candidate_staging["latest_build"];
    assert_eq!(latest_build["status"].as_str(), Some("completed"));
    assert_eq!(
        latest_build["confidence"].as_str(),
        Some("verified_raw_mirror_reconstruction")
    );
    assert_eq!(
        latest_build["candidate_conversation_count"].as_u64(),
        Some(1)
    );
    assert_eq!(latest_build["candidate_message_count"].as_u64(), Some(1));
    assert!(
        latest_build["evidence_sources"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item
                .as_str()
                .is_some_and(|text| text.starts_with("verified_raw_mirror:manifest_id=")))),
        "candidate build should identify verified raw mirror evidence: {latest_build:#}"
    );
    assert_eq!(
        latest_build["coverage_after"]["coverage_source"].as_str(),
        Some("verified_raw_mirror_candidate_archive")
    );
    assert_eq!(
        latest_build["live_inventory_unchanged"].as_bool(),
        Some(true),
        "candidate build must not overwrite the corrupt live archive"
    );

    let after_tree: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("file-tree-after.json")).unwrap(),
    )
    .expect("after file tree json");
    let data_entries = after_tree["roots"]
        .as_array()
        .and_then(|roots| {
            roots
                .iter()
                .find(|root| root["root_id"].as_str() == Some("data"))
        })
        .and_then(|root| root["entries"].as_array())
        .expect("data tree entries");
    assert!(
        data_entries.iter().any(|entry| {
            entry["relative_path"].as_str().is_some_and(|path| {
                path.starts_with("doctor/candidates/")
                    && path.contains("/evidence/raw-mirror/blobs/")
            })
        }),
        "candidate should stage raw mirror evidence copies for audit: {after_tree:#}"
    );
    let corrupt_db_after = data_entries
        .iter()
        .find(|entry| entry["relative_path"].as_str() == Some("agent_search.db"))
        .expect("live corrupt DB entry");
    assert_eq!(
        corrupt_db_after["size_bytes"].as_u64(),
        Some("not a sqlite database".len() as u64),
        "live corrupt DB should remain in place for later explicit promotion/restore handling"
    );
}

#[test]
fn doctor_e2e_runner_promotes_corrupt_db_candidate_and_records_derived_followup() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let runner = DoctorE2eRunner::new(temp.path().join("run")).expect("runner");
    let spec = DoctorE2eScenarioSpec::new(
        "artifact-corrupt-db-promotion-derived-followup",
        DoctorFixtureScenario::DbCorruptWithStaleIndex,
        ["candidate", "promotion", "derived"],
    )
    .repair_apply()
    .env("CASS_SEMANTIC_MODE", "lexical_only")
    .require_json_pointer("/repair_plan")
    .require_json_pointer("/candidate_staging/completed_candidate_count")
    .require_json_pointer("/candidate_promotion")
    .require_json_pointer("/candidate_promotion/status")
    .require_json_pointer("/candidate_promotion/derived_assets_consistency_status")
    .require_json_pointer("/candidate_promotion/derived_lexical_followup_status")
    .require_json_pointer("/candidate_promotion/derived_semantic_followup_status")
    .require_json_pointer("/candidate_promotion/derived_vector_followup_status")
    .require_json_pointer("/candidate_promotion/derived_memo_followup_status")
    .require_json_pointer("/candidate_promotion/derived_followup_artifact_path");

    let result = runner
        .run_scenario(&spec)
        .expect("run corrupt-db candidate promotion e2e scenario");
    assert_eq!(result.status, "pass");
    validate_artifact_manifest(&result.manifest_path).expect("artifact manifest valid");

    let payload: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("parsed-json/doctor-json.json")).unwrap(),
    )
    .expect("doctor repair apply json");
    let promotion = &payload["candidate_promotion"];
    assert_eq!(
        promotion["status"].as_str(),
        Some("applied"),
        "repair apply should promote the verified candidate after fingerprint approval: {payload:#}"
    );
    assert_eq!(
        promotion["derived_lexical_rebuild_required"].as_bool(),
        Some(false),
        "successful post-promotion lexical rebuild should clear the stale requirement: {promotion:#}"
    );
    assert_eq!(
        promotion["derived_lexical_followup_status"].as_str(),
        Some("rebuild-completed")
    );
    assert_eq!(
        promotion["derived_semantic_rebuild_required"].as_bool(),
        Some(false),
        "lexical-only policy should make semantic follow-up an explicit skipped fallback, not an unresolved repair requirement: {promotion:#}"
    );
    assert_eq!(
        promotion["derived_semantic_followup_status"].as_str(),
        Some("skipped-lexical-fallback-active-no-auto-download")
    );
    assert_eq!(
        promotion["derived_vector_followup_status"].as_str(),
        Some("skipped-lexical-fallback-active-no-auto-download")
    );
    assert_eq!(
        promotion["derived_memo_followup_status"].as_str(),
        Some("not-mutated-rebuildable-cache-does-not-block-archive-recovery")
    );
    assert_eq!(
        promotion["derived_assets_consistency_status"].as_str(),
        Some("promoted-archive-derived-followup-completed")
    );
    assert!(
        promotion["redacted_derived_followup_artifact_path"]
            .as_str()
            .is_some_and(|path| path.contains("derived-followup.json")),
        "promotion report should point to the durable derived follow-up artifact: {promotion:#}"
    );
    let post_repair_probes: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("post-repair-probes.json")).unwrap(),
    )
    .expect("post repair probes json");
    assert_eq!(
        post_repair_probes["db_open_probe"]["status"].as_str(),
        Some("ok"),
        "successful promotion should leave an openable archive DB: {post_repair_probes:#}"
    );
    assert_eq!(
        post_repair_probes["search_readiness"]["lexical_searchable"].as_bool(),
        Some(true),
        "successful derived follow-up should leave a searchable lexical index: {post_repair_probes:#}"
    );
    assert_eq!(
        post_repair_probes["search_readiness"]["lexical_contract"]["status"].as_str(),
        Some("pass"),
        "post-repair probe should validate the lexical search contract: {post_repair_probes:#}"
    );
    assert_eq!(
        post_repair_probes["promotion_invariants"]["applied_lexical_search_ready_after_followup"]
            .as_bool(),
        Some(true),
        "promotion invariants should connect applied promotion to lexical readiness: {post_repair_probes:#}"
    );
    let reader_probe = &post_repair_probes["reader_consistency_probe"];
    assert_eq!(
        reader_probe["status"].as_str(),
        Some("pass"),
        "reader consistency probe should pass for applied promotion: {post_repair_probes:#}"
    );
    assert_eq!(
        reader_probe["active_lock_open_probe"]["blocked_by_doctor_mutation_lock"].as_bool(),
        Some(true),
        "reader opens should be blocked while the synthetic doctor mutation lock is active: {reader_probe:#}"
    );
    assert_eq!(
        reader_probe["expected_visible_state_after_lock"].as_str(),
        Some("new-promoted-archive")
    );
    assert_eq!(
        reader_probe["observed_visible_state_after_lock"].as_str(),
        Some("new-promoted-archive")
    );
    assert_eq!(
        reader_probe["mixed_generation_observed"].as_bool(),
        Some(false)
    );

    let after_tree: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("file-tree-after.json")).unwrap(),
    )
    .expect("after file tree json");
    let data_entries = after_tree["roots"]
        .as_array()
        .and_then(|roots| {
            roots
                .iter()
                .find(|root| root["root_id"].as_str() == Some("data"))
        })
        .and_then(|root| root["entries"].as_array())
        .expect("data tree entries");
    let promoted_db = data_entries
        .iter()
        .find(|entry| entry["relative_path"].as_str() == Some("agent_search.db"))
        .expect("promoted live DB entry");
    assert!(
        promoted_db["size_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > "not a sqlite database".len() as u64),
        "live DB should no longer be the corrupt fixture bytes after repair apply: {after_tree:#}"
    );
    assert!(
        data_entries.iter().any(|entry| {
            entry["relative_path"].as_str().is_some_and(|path| {
                path.starts_with("doctor/candidate-promotions/")
                    && path.ends_with("/derived-followup.json")
            })
        }),
        "file tree should include the append-only derived follow-up artifact: {after_tree:#}"
    );
    let redacted_followup = promotion["redacted_derived_followup_artifact_path"]
        .as_str()
        .expect("redacted followup path");
    assert!(
        data_entries
            .iter()
            .filter_map(|entry| entry["relative_path"].as_str())
            .any(|path| {
                path.starts_with("doctor/candidate-promotions/")
                    && path.ends_with("/derived-followup.json")
            }),
        "missing derived follow-up artifact tree entry for {redacted_followup}"
    );
    let followup_artifact: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            result
                .artifact_dir
                .join("candidate-promotion-derived-followup.json"),
        )
        .unwrap(),
    )
    .expect("parse copied derived follow-up artifact");
    assert_eq!(
        followup_artifact["derived_asset_actions"]["semantic_vector_index"]["status"].as_str(),
        Some("skipped-lexical-fallback-active-no-auto-download")
    );
    assert_eq!(
        followup_artifact["derived_asset_actions"]["semantic_vector_index"]
            ["blocks_archive_recovery"]
            .as_bool(),
        Some(false)
    );
    assert_eq!(
        followup_artifact["derived_asset_actions"]["memoization_cache"]["status"].as_str(),
        Some("not-mutated-rebuildable-cache-does-not-block-archive-recovery")
    );
    assert_eq!(
        followup_artifact["derived_asset_actions"]["memoization_cache"]["blocks_archive_recovery"]
            .as_bool(),
        Some(false)
    );

    let commands = std::fs::read_to_string(result.artifact_dir.join("commands.jsonl")).unwrap();
    for command_id in [
        "doctor-repair-candidate-build",
        "doctor-repair-dry-run",
        "doctor-json",
    ] {
        assert!(
            commands.contains(command_id),
            "promotion e2e should log command phase {command_id}: {commands}"
        );
    }
    let execution_flow =
        std::fs::read_to_string(result.artifact_dir.join("execution-flow.jsonl")).unwrap();
    for field in [
        "\"phase\":\"candidate_promotion\"",
        "\"phase\":\"post_repair_probes\"",
        "derived_lexical_followup_status",
        "derived_semantic_followup_status",
        "derived_vector_followup_status",
        "derived_memo_followup_status",
        "redacted_derived_followup_artifact_path",
        "reader_consistency_probe",
        "blocked_by_doctor_mutation_lock",
    ] {
        assert!(
            execution_flow.contains(field),
            "promotion e2e execution flow should include {field}: {execution_flow}"
        );
    }
}

fn assert_candidate_promotion_rollback_failpoint(failpoint_phase: &str, scenario_id: &str) {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let runner = DoctorE2eRunner::new(temp.path().join("run")).expect("runner");
    let spec = DoctorE2eScenarioSpec::new(
        scenario_id,
        DoctorFixtureScenario::DbCorruptWithStaleIndex,
        ["candidate", "promotion", "fault"],
    )
    .repair_apply()
    .env("CASS_SEMANTIC_MODE", "lexical_only")
    .env(
        "CASS_TEST_DOCTOR_CANDIDATE_PROMOTION_FAILPOINT",
        failpoint_phase,
    )
    .expect_exit_success(false)
    .require_json_pointer("/repair_plan")
    .require_json_pointer("/candidate_staging/completed_candidate_count")
    .require_json_pointer("/candidate_promotion")
    .require_json_pointer("/candidate_promotion/status")
    .require_json_pointer("/candidate_promotion/rollback_reference")
    .require_json_pointer("/candidate_promotion/fs_mutation_receipts");

    let result = runner
        .run_scenario(&spec)
        .expect("run corrupt-db candidate promotion rollback e2e scenario");
    assert_eq!(result.status, "pass");
    validate_artifact_manifest(&result.manifest_path).expect("artifact manifest valid");

    let payload: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("parsed-json/doctor-json.json")).unwrap(),
    )
    .expect("doctor repair rollback json");
    let promotion = &payload["candidate_promotion"];
    assert_eq!(
        promotion["status"].as_str(),
        Some("rolled_back"),
        "promotion failpoint should leave an explicit rolled_back report: {payload:#}"
    );
    assert_eq!(promotion["rollback_applied"].as_bool(), Some(true));
    assert_eq!(
        promotion["reader_consistency_guarantee"].as_str(),
        Some("failed-promotion-rolled-back-to-prior-live-bundle-backup")
    );
    assert_eq!(
        promotion["live_inventory_after"], promotion["live_inventory_before"],
        "rollback should restore the full prior live DB/WAL/index inventory: {promotion:#}"
    );
    let post_repair_probes: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("post-repair-probes.json")).unwrap(),
    )
    .expect("post repair rollback probes json");
    assert_eq!(
        post_repair_probes["promotion_invariants"]["candidate_promotion_status"].as_str(),
        Some("rolled_back"),
        "post-repair probes should carry branchable rollback status: {post_repair_probes:#}"
    );
    assert_eq!(
        post_repair_probes["promotion_invariants"]["live_inventory_restored_after_rollback"]
            .as_bool(),
        Some(true),
        "post-repair probes should prove the full live inventory was restored: {post_repair_probes:#}"
    );
    assert_eq!(
        post_repair_probes["db_open_probe"]["status"].as_str(),
        Some("unreadable"),
        "this corrupt-archive rollback fixture should explicitly report the old DB is still unreadable instead of implying successful repair: {post_repair_probes:#}"
    );
    let reader_probe = &post_repair_probes["reader_consistency_probe"];
    assert_eq!(
        reader_probe["status"].as_str(),
        Some("pass"),
        "reader consistency probe should pass for rollback promotion: {post_repair_probes:#}"
    );
    assert_eq!(
        reader_probe["active_lock_open_probe"]["blocked_by_doctor_mutation_lock"].as_bool(),
        Some(true),
        "reader opens should be blocked while the synthetic doctor mutation lock is active: {reader_probe:#}"
    );
    assert_eq!(
        reader_probe["expected_visible_state_after_lock"].as_str(),
        Some("prior-live-archive")
    );
    assert_eq!(
        reader_probe["observed_visible_state_after_lock"].as_str(),
        Some("prior-live-archive")
    );
    assert_eq!(
        reader_probe["mixed_generation_observed"].as_bool(),
        Some(false)
    );
    assert!(
        promotion["rollback_reference"]
            .as_str()
            .is_some_and(|reference| reference.contains("restored-prior-live:")),
        "rollback reference should point at restored prior-live evidence: {promotion:#}"
    );
    assert!(
        promotion["fs_mutation_receipts"]
            .as_array()
            .is_some_and(|receipts| receipts.iter().any(|receipt| {
                receipt["status"].as_str() == Some("failed")
                    && receipt["blocked_reasons"]
                        .as_array()
                        .is_some_and(|reasons| {
                            reasons.iter().any(|reason| {
                                reason.as_str().is_some_and(|text| {
                                    text.contains("injected test candidate promotion failpoint")
                                        && text.contains(failpoint_phase)
                                })
                            })
                        })
                    && receipt["precondition_checks"]
                        .as_array()
                        .is_some_and(|checks| {
                            checks.iter().any(|check| {
                                check.as_str() == Some("rollback_restored_prior_live_sqlite_bundle")
                            })
                        })
            })),
        "failed receipt should preserve failpoint root cause and rollback proof: {promotion:#}"
    );

    let after_tree: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("file-tree-after.json")).unwrap(),
    )
    .expect("after file tree json");
    let data_entries = after_tree["roots"]
        .as_array()
        .and_then(|roots| {
            roots
                .iter()
                .find(|root| root["root_id"].as_str() == Some("data"))
        })
        .and_then(|root| root["entries"].as_array())
        .expect("data tree entries");
    let rolled_back_db = data_entries
        .iter()
        .find(|entry| entry["relative_path"].as_str() == Some("agent_search.db"))
        .expect("rolled back live DB entry");
    assert_eq!(
        rolled_back_db["size_bytes"].as_u64(),
        Some("not a sqlite database".len() as u64),
        "rollback should restore the prior corrupt live DB bytes instead of leaving the promoted candidate visible: {after_tree:#}"
    );
    assert!(
        data_entries.iter().any(|entry| {
            entry["relative_path"].as_str().is_some_and(|path| {
                path.starts_with("doctor/candidate-promotions/")
                    && path.ends_with("/event-log.json")
            })
        }),
        "rollback scenario should leave durable candidate-promotion event logs: {after_tree:#}"
    );

    let execution_flow =
        std::fs::read_to_string(result.artifact_dir.join("execution-flow.jsonl")).unwrap();
    for field in [
        "\"phase\":\"candidate_promotion\"",
        "\"phase\":\"post_repair_probes\"",
        "\"status\":\"rolled_back\"",
        "rollback_reference",
        "reader_consistency_probe",
        "blocked_by_doctor_mutation_lock",
    ] {
        assert!(
            execution_flow.contains(field),
            "rollback e2e execution flow should include {field}: {execution_flow}"
        );
    }
}

#[test]
fn doctor_e2e_runner_rolls_back_candidate_promotion_after_component_replace_failpoint() {
    assert_candidate_promotion_rollback_failpoint(
        "after-component-replace",
        "artifact-corrupt-db-promotion-rollback-after-component-replace",
    );
}

#[test]
fn doctor_e2e_runner_rolls_back_candidate_promotion_before_parent_sync_failpoint() {
    assert_candidate_promotion_rollback_failpoint(
        "before-parent-sync",
        "artifact-corrupt-db-promotion-rollback-before-parent-sync",
    );
}

#[test]
fn doctor_e2e_runner_records_multi_file_source_artifacts() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let runner = DoctorE2eRunner::new(temp.path().join("run")).expect("runner");
    let spec = DoctorE2eScenarioSpec::new(
        "artifact-multi-file-source",
        DoctorFixtureScenario::MultiSource,
        ["source-mirror", "multi-file"],
    )
    .require_json_pointer("/source_inventory")
    .require_json_pointer("/source_inventory/provider_counts/codex")
    .require_json_pointer("/source_inventory/provider_counts/cline")
    .require_json_pointer("/operation_outcome/kind")
    .require_json_pointer("/source_authority/selected_authority");

    let result = runner
        .run_scenario(&spec)
        .expect("run multi-file doctor e2e scenario");
    assert_eq!(result.status, "pass");
    validate_artifact_manifest(&result.manifest_path).expect("artifact manifest valid");

    let source_before: serde_json::Value = serde_json::from_slice(
        &std::fs::read(result.artifact_dir.join("source-inventory-before.json")).unwrap(),
    )
    .expect("source inventory before json");
    assert_eq!(
        source_before["source_discovery"]["provider_set"]
            .as_array()
            .map(Vec::len),
        Some(2),
        "multi-source artifact should record both fixture providers"
    );
    assert_eq!(
        source_before["source_discovery"]["expected_provider_counts"]["codex"].as_u64(),
        Some(1)
    );
    assert_eq!(
        source_before["source_discovery"]["expected_provider_counts"]["cline"].as_u64(),
        Some(1)
    );
    assert!(
        source_before["upstream_source_files"]["tree_entry_count"]
            .as_u64()
            .is_some_and(|count| count >= 3),
        "multi-file source inventory should include Codex primary, Cline primary, and Cline sidecar"
    );
    let source_artifacts = source_before["upstream_source_files"]["artifacts"]
        .as_array()
        .expect("source artifacts array");
    assert!(
        source_artifacts.iter().any(|artifact| {
            artifact["artifact_kind"].as_str() == Some("provider_source_sidecar")
                && artifact["relative_path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("task_metadata.json"))
        }),
        "multi-file source artifact bundle should include the Cline metadata sidecar"
    );

    let execution_flow =
        std::fs::read_to_string(result.artifact_dir.join("execution-flow.jsonl")).unwrap();
    for phase in [
        "source_discovery",
        "parse_outcome",
        "db_projection_outcome",
        "source_inventory_before",
        "source_inventory_after",
    ] {
        assert!(
            execution_flow.contains(&format!("\"phase\":\"{phase}\"")),
            "multi-file execution flow should include phase {phase}: {execution_flow}"
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
    assert_eq!(context.schema_version, 1);
    assert_eq!(context.scenario_id, "intentional-failure");
    assert_eq!(context.failed_phase, "verification");
    assert_eq!(context.failed_check, "assert_required_json_pointer");
    assert_eq!(context.command_id.as_deref(), Some("doctor-json"));
    assert_eq!(context.command.command_id, "doctor-json");
    assert_eq!(context.command_history.len(), 1);
    assert_eq!(context.fixture.data_dir, "[doctor-e2e-data]");
    assert_eq!(
        context.artifacts.failure_context_path,
        "failure_context.json"
    );
    assert_eq!(context.artifacts.commands_path, "commands.jsonl");
    assert_eq!(context.repro.safety, "fixture-only-redacted-template");
    assert!(!context.repro.mutates_live_archive);
    assert!(context.repro.requires_explicit_live_archive);
    assert_eq!(context.repro.target, "[doctor-e2e-data]");
    assert!(
        context
            .repro
            .command_json
            .iter()
            .any(|arg| arg == "[doctor-e2e-data]"),
        "safe repro command should target the redacted fixture data placeholder: {:?}",
        context.repro.command_json
    );
    assert!(
        context.repro.shell_command.contains("[doctor-e2e-data]"),
        "safe repro command should include the fixture data placeholder: {}",
        context.repro.shell_command
    );
    assert!(
        context
            .recent_events
            .iter()
            .any(|event| event["event"].as_str() == Some("scenario_end")),
        "failure context should include recent event-log context: {:?}",
        context.recent_events
    );
    assert!(context.selected_authority.is_some());
    assert!(context.rejected_authorities.is_some());
    assert!(context.active_locks.is_some());
    assert!(context.coverage_summary.is_some());
    let failure_context_path = result.artifact_dir.join("failure_context.json");
    assert!(failure_context_path.exists());
    let failure_context_json =
        std::fs::read_to_string(&failure_context_path).expect("failure context json");
    assert!(
        !failure_context_json.contains(temp.path().to_string_lossy().as_ref()),
        "failure context should redact temp paths"
    );
    assert!(
        !failure_context_json.contains("CASS_DOCTOR_PRIVACY_SENTINEL"),
        "failure context should not leak privacy sentinels"
    );
    assert!(
        failure_context_json.contains("\"failure_context_path\": \"failure_context.json\""),
        "failure context should include self-describing artifact references: {failure_context_json}"
    );
    let manifest_json: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&result.manifest_path).expect("read failed manifest"),
    )
    .expect("failed manifest json");
    assert_eq!(
        manifest_json["artifacts"]["failure_context_json"].as_str(),
        Some("failure_context.json")
    );
    assert!(result.artifact_dir.join("failure_summary.txt").exists());
    validate_artifact_manifest(&result.manifest_path).expect("failed artifact manifest valid");
}

#[test]
fn doctor_e2e_scripted_scenarios() {
    let labels = std::env::var("CASS_DOCTOR_E2E_LABELS").unwrap_or_else(|_| "quick".to_string());
    let scenarios_arg = std::env::var("CASS_DOCTOR_E2E_SCENARIOS").unwrap_or_default();
    let exclude_labels_arg = std::env::var("CASS_DOCTOR_E2E_EXCLUDE_LABELS").unwrap_or_default();
    let exclude_scenarios_arg =
        std::env::var("CASS_DOCTOR_E2E_EXCLUDE_SCENARIOS").unwrap_or_default();
    let mut args = vec!["doctor_v2".to_string(), "--label".to_string(), labels];
    if !scenarios_arg.trim().is_empty() {
        args.push("--scenario".to_string());
        args.push(scenarios_arg);
    }
    if !exclude_labels_arg.trim().is_empty() {
        args.push("--exclude-label".to_string());
        args.push(exclude_labels_arg);
    }
    if !exclude_scenarios_arg.trim().is_empty() {
        args.push("--exclude-scenario".to_string());
        args.push(exclude_scenarios_arg);
    }
    if std::env::var("CASS_DOCTOR_E2E_INCLUDE_FAILURE_SELF_TEST").is_ok() {
        args.push("--include-failure-self-test".to_string());
    }
    if std::env::var("CASS_DOCTOR_E2E_FAIL_FAST").is_ok() {
        args.push("--fail-fast".to_string());
    }
    let parsed = DoctorE2eCliArgs::parse_from(args).expect("parse scripted args");
    let scenarios = doctor_e2e_scenarios_for_args(&parsed);
    let selected = select_scenarios(&parsed, &scenarios);
    let run_root = std::env::var("CASS_DOCTOR_E2E_RUN_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| default_doctor_e2e_run_root());
    std::fs::create_dir_all(&run_root).expect("create doctor e2e run root");
    let scenario_manifest = doctor_e2e_scenario_registry_manifest(&parsed, &scenarios, &selected);
    validate_doctor_e2e_scenario_registry_manifest_value(&scenario_manifest)
        .expect("scripted scenario registry manifest should validate");
    let scenario_manifest_path = run_root.join("scenario-manifest.json");
    std::fs::write(
        &scenario_manifest_path,
        serde_json::to_vec_pretty(&scenario_manifest).expect("scenario manifest json"),
    )
    .expect("write doctor e2e scenario manifest");
    if std::env::var("CASS_DOCTOR_E2E_LIST_ONLY").is_ok() {
        println!(
            "{}",
            serde_json::to_string_pretty(&scenario_manifest).expect("scenario manifest text")
        );
        return;
    }
    assert!(
        !selected.is_empty(),
        "doctor e2e script selection should choose at least one scenario"
    );

    let runner = DoctorE2eRunner::new(&run_root).expect("runner");
    let mut scenario_summaries = Vec::new();
    let mut problems = Vec::new();
    for scenario in &selected {
        let result = match runner.run_scenario(scenario) {
            Ok(result) => result,
            Err(err) => {
                scenario_summaries.push(doctor_e2e_run_error_summary(scenario, &err));
                problems.push(format!("{}: {err}", scenario.scenario_id));
                if parsed.fail_fast {
                    break;
                }
                continue;
            }
        };
        let status_matches = result.status == scenario.expected_runner_status();
        scenario_summaries.push(doctor_e2e_run_result_summary(scenario, &result));
        if !status_matches {
            problems.push(format!(
                "{}: expected runner status {}, got {} (artifacts at {})",
                scenario.scenario_id,
                scenario.expected_runner_status(),
                result.status,
                result.artifact_dir.display()
            ));
        }
        if parsed.fail_fast && (result.status == "fail" || !status_matches) {
            break;
        }
    }
    let run_summary = doctor_e2e_run_summary_manifest(&parsed, &run_root, scenario_summaries);
    validate_doctor_e2e_run_summary_manifest_value(&run_summary)
        .expect("scripted run summary manifest should validate");
    std::fs::write(
        run_root.join("run-summary.json"),
        serde_json::to_vec_pretty(&run_summary).expect("run summary json"),
    )
    .expect("write doctor e2e run summary");
    assert!(
        problems.is_empty(),
        "scripted doctor scenarios reported problems after writing run-summary.json: {}",
        problems.join("; ")
    );
}
