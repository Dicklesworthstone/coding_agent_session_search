mod util;

use serde_json::Value;
use util::doctor_fixture::{
    DoctorFixtureArtifact, DoctorFixtureFactory, DoctorFixtureScenario, DoctorProviderSpec,
    default_expected_artifact_keys,
};

#[test]
fn doctor_fixture_factory_is_deterministic_and_root_confined() {
    let mut first = DoctorFixtureFactory::new("deterministic");
    first.apply_scenario(DoctorFixtureScenario::SourcePruned);
    first.validate_manifest().expect("first manifest valid");

    let mut second = DoctorFixtureFactory::new("deterministic");
    second.apply_scenario(DoctorFixtureScenario::SourcePruned);
    second.validate_manifest().expect("second manifest valid");

    assert_eq!(
        first.manifest(),
        second.manifest(),
        "scenario manifests should be deterministic and avoid temp-root-specific absolute paths"
    );
    for artifact in &first.manifest().artifacts {
        let absolute = first.root().join(&artifact.relative_path);
        assert!(
            absolute.starts_with(first.root()),
            "artifact must stay under fixture root: {}",
            artifact.relative_path
        );
    }
}

#[test]
fn doctor_fixture_factory_rejects_hostile_paths() {
    let factory = DoctorFixtureFactory::new("hostile-paths");
    assert!(factory.confined_home_path("../escape").is_err());
    assert!(factory.confined_home_path("/tmp/escape").is_err());
    assert!(factory.confined_data_path("raw-mirror/v1").is_ok());
}

#[test]
fn doctor_fixture_factory_provider_matrix_never_targets_real_agent_homes() {
    let mut factory = DoctorFixtureFactory::new("provider-matrix");
    factory.add_all_provider_source_trees();
    factory.validate_manifest().expect("manifest valid");
    let real_home = std::env::var_os("HOME").map(std::path::PathBuf::from);

    assert_eq!(
        factory.manifest().provider_set.len(),
        DoctorProviderSpec::all().len(),
        "provider matrix should include every doctor-relevant provider fixture"
    );
    for artifact in &factory.manifest().artifacts {
        let absolute = factory.root().join(&artifact.relative_path);
        assert!(
            absolute.starts_with(factory.root()),
            "provider fixture wrote outside temp root: {}",
            artifact.relative_path
        );
        if let Some(real_home) = &real_home {
            assert!(
                !absolute.starts_with(real_home.join(".codex"))
                    && !absolute.starts_with(real_home.join(".claude")),
                "provider fixture must not write to real agent harness homes: {}",
                absolute.display()
            );
        }
    }
}

#[test]
fn doctor_fixture_factory_places_privacy_sentinel_without_manifest_leak() {
    let mut factory = DoctorFixtureFactory::new("privacy");
    factory.apply_scenario(DoctorFixtureScenario::SupportBundle);
    factory.validate_manifest().expect("manifest valid");

    let manifest_json = serde_json::to_string(factory.manifest()).expect("serialize manifest");
    assert!(
        !manifest_json.contains("CASS_DOCTOR_PRIVACY_SENTINEL"),
        "fixture manifest must hash/redact privacy sentinels instead of embedding raw secrets"
    );
    assert!(
        factory
            .manifest()
            .privacy_sentinels
            .iter()
            .any(|sentinel| sentinel.must_be_absent_from_default_output),
        "privacy sentinel should declare default-output absence requirement"
    );
}

#[test]
fn doctor_fixture_factory_can_materialize_all_named_scenarios() {
    for scenario in [
        DoctorFixtureScenario::Healthy,
        DoctorFixtureScenario::PartiallyIndexed,
        DoctorFixtureScenario::SourcePruned,
        DoctorFixtureScenario::MirrorMissing,
        DoctorFixtureScenario::DbCorrupt,
        DoctorFixtureScenario::IndexCorrupt,
        DoctorFixtureScenario::StaleLock,
        DoctorFixtureScenario::ActiveLock,
        DoctorFixtureScenario::InterruptedRepair,
        DoctorFixtureScenario::BackupAvailable,
        DoctorFixtureScenario::LowDisk,
        DoctorFixtureScenario::BackupExclusion,
        DoctorFixtureScenario::SupportBundle,
        DoctorFixtureScenario::MultiSource,
        DoctorFixtureScenario::PathEdgeCases,
    ] {
        let mut factory = DoctorFixtureFactory::new(format!("scenario-{scenario:?}"));
        factory.apply_scenario(scenario);
        factory
            .validate_manifest()
            .unwrap_or_else(|err| panic!("scenario {scenario:?} manifest invalid: {err}"));
        assert!(
            !factory.manifest().risk_class.trim().is_empty(),
            "scenario {scenario:?} should declare a risk class"
        );
        assert!(
            !factory.manifest().expected_mutation_class.trim().is_empty(),
            "scenario {scenario:?} should declare a mutation class"
        );
        assert!(
            !factory.manifest().repair_eligibility.trim().is_empty(),
            "scenario {scenario:?} should declare repair eligibility"
        );
        assert!(
            !factory.manifest().structured_log.is_empty(),
            "scenario {scenario:?} should emit structured setup log entries"
        );
    }
}

#[test]
fn doctor_fixture_manifest_declares_safety_and_artifact_contracts() {
    let mut factory = DoctorFixtureFactory::new("contract-fields");
    factory.apply_scenario(DoctorFixtureScenario::SourcePruned);
    factory.validate_manifest().expect("manifest valid");
    let manifest = factory.manifest();

    assert_eq!(manifest.risk_class, "archive-sole-copy-risk");
    assert_eq!(manifest.expected_mutation_class, "read-only");
    assert_eq!(manifest.repair_eligibility, "reconstruct-plan-required");
    assert!(
        manifest
            .allowed_commands
            .iter()
            .all(|command| command.contains("--json") || command.contains("--robot")),
        "allowed commands must be machine-readable: {:?}",
        manifest.allowed_commands
    );
    assert!(
        manifest
            .forbidden_live_path_patterns
            .iter()
            .any(|pattern| pattern == "real-home/.codex"),
        "manifest should document live agent harness paths that fixtures must not target"
    );
    for required in default_expected_artifact_keys() {
        assert!(
            manifest.expected_artifact_keys.contains(&required),
            "manifest should require e2e artifact key {required}"
        );
    }
    assert!(!manifest.redaction_policy.raw_session_text_in_default_output);
    assert!(
        !manifest
            .redaction_policy
            .full_source_paths_in_default_output
    );
    assert!(!manifest.redaction_policy.privacy_sentinel_in_default_output);
}

#[test]
fn doctor_fixture_manifest_validation_rejects_sensitive_default_output_policy() {
    let factory = DoctorFixtureFactory::new("bad-redaction-policy");
    let mut manifest = factory.manifest().clone();
    manifest.redaction_policy.raw_session_text_in_default_output = true;

    let err = manifest
        .validate_against_root(factory.root())
        .expect_err("sensitive default output policy must be rejected");
    assert!(
        err.contains("default redaction policy"),
        "validation error should explain redaction policy failure, got: {err}"
    );
}

#[test]
fn doctor_fixture_manifest_validation_catches_missing_artifacts() {
    let factory = DoctorFixtureFactory::new("invalid-manifest");
    let mut manifest = factory.manifest().clone();
    manifest.artifacts.push(DoctorFixtureArtifact {
        artifact_kind: "missing".to_string(),
        relative_path: "missing/file".to_string(),
        size_bytes: 0,
        blake3: blake3::hash(b"").to_hex().to_string(),
    });

    let err = manifest
        .validate_against_root(factory.root())
        .expect_err("missing artifact must invalidate manifest");
    assert!(
        err.contains("listed but missing"),
        "validation error should explain missing artifact, got: {err}"
    );
}

#[test]
fn doctor_fixture_manifest_drives_doctor_json_assertions_for_pruned_mirror() {
    let mut factory = DoctorFixtureFactory::new("doctor-json-pruned-mirror");
    let source =
        factory.add_provider_source(DoctorProviderSpec::codex(), "local", true, true, true);
    factory.validate_manifest().expect("manifest valid");

    assert!(
        !source.source_path.exists(),
        "fixture should model an already-pruned upstream source without deleting a temp file"
    );
    let out = factory
        .cass_cmd()
        .args([
            "doctor",
            "--json",
            "--data-dir",
            factory.data_dir().to_str().expect("utf8 data dir"),
        ])
        .output()
        .expect("run cass doctor --json");
    assert!(
        !out.stdout.is_empty(),
        "cass doctor --json should emit robot JSON even when this fixture lacks a derived index; status={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !source.source_path.exists(),
        "doctor check must not recreate the pruned upstream source"
    );

    let payload: Value = serde_json::from_slice(&out.stdout).expect("doctor JSON");
    factory.assert_doctor_payload_matches_manifest(&payload);
    assert_eq!(payload["raw_mirror"]["status"].as_str(), Some("verified"));
    assert_eq!(
        payload["source_inventory"]["missing_current_source_count"].as_u64(),
        Some(1),
        "doctor should report the fixture's pruned upstream source"
    );
}

#[test]
fn doctor_fixture_raw_mirror_keeps_source_id_distinct_from_origin_kind() {
    let mut factory = DoctorFixtureFactory::new("remote-raw-mirror");
    let source = factory.add_provider_source(
        DoctorProviderSpec::codex(),
        "work-laptop",
        false,
        true,
        false,
    );
    factory.validate_manifest().expect("manifest valid");

    let manifest_path = factory
        .data_dir()
        .join("raw-mirror/v1/manifests")
        .join(format!(
            "{}.json",
            source
                .manifest_id
                .as_deref()
                .expect("raw mirror manifest id")
        ));
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).expect("read raw mirror manifest"))
            .expect("parse raw mirror manifest");

    assert_eq!(manifest["db_links"][0]["conversation_id"].as_i64(), Some(1));
    assert_eq!(manifest["db_links"][0]["message_count"].as_u64(), Some(2));
    assert_eq!(manifest["source_id"].as_str(), Some("work-laptop"));
    assert_eq!(manifest["origin_kind"].as_str(), Some("ssh"));
    assert_eq!(manifest["origin_host"].as_str(), Some("work-laptop"));
}
