#![allow(dead_code)]

use super::cass_bin;
use super::doctor_fixture::{
    DoctorFixtureFactory, DoctorFixtureScenario, default_expected_artifact_keys,
};
use coding_agent_search::franken_sync::Connection as FrankenConnection;
use coding_agent_search::franken_sync::compat::ConnectionExt;
use coding_agent_search::storage::sqlite::SqliteStorage;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, UNIX_EPOCH};
use walkdir::WalkDir;

const DOCTOR_E2E_SCHEMA_VERSION: u32 = 1;
const PRIVACY_SENTINEL_VALUE: &str = "CASS_DOCTOR_PRIVACY_SENTINEL_DO_NOT_LEAK";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorE2eCliArgs {
    pub label_filter: BTreeSet<String>,
    pub scenario_filter: BTreeSet<String>,
    pub exclude_label_filter: BTreeSet<String>,
    pub exclude_scenario_filter: BTreeSet<String>,
    pub fail_fast: bool,
    pub include_failure_self_test: bool,
}

#[derive(Debug, Clone)]
pub struct DoctorE2eScenarioSpec {
    pub scenario_id: String,
    pub labels: BTreeSet<String>,
    pub fixture_scenario: DoctorFixtureScenario,
    pub command_mode: DoctorE2eCommandMode,
    pub expect_exit_success: Option<bool>,
    pub allow_mutation: bool,
    pub backup_restore_expected_candidate_promotion_status: Option<String>,
    pub skip_repair_candidate_build_preflight: bool,
    pub extra_env: BTreeMap<String, String>,
    pub required_json_pointers: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorE2eCommandMode {
    Check,
    Fix,
    CleanupApply,
    RepairApply,
    BackupsRestoreJourney,
    BaselineDiffJourney,
    SupportBundleAfterFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorE2eArtifactManifest {
    pub schema_version: u32,
    pub scenario_id: String,
    pub labels: Vec<String>,
    pub status: String,
    pub artifact_dir: String,
    pub fixture_root: String,
    pub home_dir: String,
    pub data_dir: String,
    pub command_count: usize,
    pub artifacts: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_context: Option<DoctorE2eFailureContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorE2eFailureContext {
    pub schema_version: u32,
    pub scenario_id: String,
    pub failed_phase: String,
    pub failed_check: String,
    pub reasons: Vec<String>,
    pub command: DoctorE2eCommandRecord,
    pub command_history: Vec<DoctorE2eCommandRecord>,
    pub platform: DoctorE2eFailurePlatformContext,
    pub fixture: DoctorE2eFailureFixtureContext,
    pub artifacts: DoctorE2eFailureArtifactRefs,
    pub repro: DoctorE2eFailureRepro,
    pub recent_events: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_authority: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_authorities: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_locks: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_summary: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorE2eFailurePlatformContext {
    pub os: String,
    pub arch: String,
    pub family: String,
    pub cass_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_revision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorE2eFailureFixtureContext {
    pub fixture_id: String,
    pub fixture_root: String,
    pub home_dir: String,
    pub data_dir: String,
    pub risk_class: String,
    pub expected_mutation_class: String,
    pub repair_eligibility: String,
    pub scenario_fixture: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorE2eFailureArtifactRefs {
    pub artifact_manifest_path: String,
    pub commands_path: String,
    pub doctor_events_path: String,
    pub execution_flow_path: String,
    pub receipts_path: String,
    pub checksums_path: String,
    pub stdout_path: String,
    pub stderr_path: String,
    pub failure_context_path: String,
    pub failure_summary_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parsed_json_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorE2eFailureRepro {
    pub safety: String,
    pub mutates_live_archive: bool,
    pub requires_explicit_live_archive: bool,
    pub target: String,
    pub working_directory: String,
    pub command_json: Vec<String>,
    pub shell_command: String,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorE2eRunResult {
    pub scenario_id: String,
    pub status: String,
    pub artifact_dir: PathBuf,
    pub manifest_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_context: Option<DoctorE2eFailureContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorE2eFileTreeSnapshot {
    pub roots: Vec<DoctorE2eFileTreeRoot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorE2eFileTreeRoot {
    pub root_id: String,
    pub entries: Vec<DoctorE2eFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorE2eFileEntry {
    pub relative_path: String,
    pub entry_kind: String,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_unix_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blake3: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorE2eCommandRecord {
    pub command_id: String,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub stdout_path: String,
    pub stderr_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parsed_json_path: Option<String>,
    pub parsed_json_ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DoctorE2eRunner {
    run_root: PathBuf,
    artifact_root: PathBuf,
    cass_bin: PathBuf,
}

struct DoctorE2eRedactor {
    replacements: Vec<(String, String)>,
}

struct RecordedDoctorCommand {
    record: DoctorE2eCommandRecord,
    parsed_json: Option<(Value, String)>,
    redacted_stdout: String,
    redacted_stderr: String,
    parse_failure: Option<String>,
}

struct DoctorCommandArtifactPaths<'a> {
    command_id: &'a str,
    stdout: &'a str,
    stderr: &'a str,
    parsed_json: Option<&'a str>,
}

#[derive(Debug, Clone)]
struct DoctorE2eBackupRestoreJourneyFixture {
    good_backup_id: String,
    drifted_backup_id: String,
}

struct FailureContextBuildInput<'a> {
    spec: &'a DoctorE2eScenarioSpec,
    fixture: &'a DoctorFixtureFactory,
    redactor: &'a DoctorE2eRedactor,
    command_records: &'a [DoctorE2eCommandRecord],
    final_command_record: &'a DoctorE2eCommandRecord,
    failures: &'a [String],
    parsed_json: Option<&'a Value>,
    doctor_events: &'a [Value],
    redacted_stdout: &'a str,
    redacted_stderr: &'a str,
    cleanup_approval_fingerprint: Option<&'a str>,
}

impl DoctorE2eCliArgs {
    pub fn parse_from<I, S>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut parsed = Self::default();
        let mut iter = args
            .into_iter()
            .map(|arg| arg.as_ref().to_string())
            .peekable();
        if iter.peek().is_some_and(|arg| !arg.starts_with("--")) {
            let _ = iter.next();
        }

        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--label" | "--labels" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| format!("{arg} requires a comma-separated value"))?;
                    extend_csv_set(&mut parsed.label_filter, &value);
                }
                "--scenario" | "--scenarios" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| format!("{arg} requires a comma-separated value"))?;
                    extend_csv_set(&mut parsed.scenario_filter, &value);
                }
                "--exclude-label" | "--exclude-labels" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| format!("{arg} requires a comma-separated value"))?;
                    extend_csv_set(&mut parsed.exclude_label_filter, &value);
                }
                "--exclude-scenario" | "--exclude-scenarios" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| format!("{arg} requires a comma-separated value"))?;
                    extend_csv_set(&mut parsed.exclude_scenario_filter, &value);
                }
                "--fail-fast" => parsed.fail_fast = true,
                "--include-failure-self-test" => parsed.include_failure_self_test = true,
                "--help" | "-h" => {}
                unknown => return Err(format!("unknown doctor e2e runner arg: {unknown}")),
            }
        }

        Ok(parsed)
    }

    pub fn selects(&self, scenario: &DoctorE2eScenarioSpec) -> bool {
        let scenario_match =
            self.scenario_filter.is_empty() || self.scenario_filter.contains(&scenario.scenario_id);
        let failure_self_test_match =
            self.include_failure_self_test && scenario.labels.contains("self-test");
        let label_match = self.label_filter.is_empty()
            || self
                .label_filter
                .iter()
                .any(|label| scenario.labels.contains(label));
        let excluded_by_scenario = self.exclude_scenario_filter.contains(&scenario.scenario_id);
        let excluded_by_label = self
            .exclude_label_filter
            .iter()
            .any(|label| scenario.labels.contains(label));
        scenario_match
            && (label_match || failure_self_test_match)
            && !excluded_by_scenario
            && !excluded_by_label
    }
}

impl DoctorE2eScenarioSpec {
    pub fn new(
        scenario_id: impl Into<String>,
        fixture_scenario: DoctorFixtureScenario,
        labels: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            scenario_id: scenario_id.into(),
            labels: labels.into_iter().map(Into::into).collect(),
            fixture_scenario,
            command_mode: DoctorE2eCommandMode::Check,
            expect_exit_success: None,
            allow_mutation: false,
            backup_restore_expected_candidate_promotion_status: None,
            skip_repair_candidate_build_preflight: false,
            extra_env: BTreeMap::new(),
            required_json_pointers: Vec::new(),
        }
    }

    pub fn expect_exit_success(mut self, expected: bool) -> Self {
        self.expect_exit_success = Some(expected);
        self
    }

    pub fn allow_mutation(mut self, allow: bool) -> Self {
        self.allow_mutation = allow;
        if allow && self.command_mode == DoctorE2eCommandMode::Check {
            self.command_mode = DoctorE2eCommandMode::Fix;
        } else if !allow {
            self.command_mode = DoctorE2eCommandMode::Check;
        }
        self
    }

    pub fn cleanup_apply(mut self) -> Self {
        self.allow_mutation = true;
        self.command_mode = DoctorE2eCommandMode::CleanupApply;
        self
    }

    pub fn repair_apply(mut self) -> Self {
        self.allow_mutation = true;
        self.command_mode = DoctorE2eCommandMode::RepairApply;
        self
    }

    pub fn backups_restore_journey(mut self) -> Self {
        self.allow_mutation = true;
        self.command_mode = DoctorE2eCommandMode::BackupsRestoreJourney;
        self
    }

    pub fn baseline_diff_journey(mut self) -> Self {
        self.allow_mutation = false;
        self.command_mode = DoctorE2eCommandMode::BaselineDiffJourney;
        self
    }

    pub fn support_bundle_after_failure(mut self) -> Self {
        self.allow_mutation = false;
        self.command_mode = DoctorE2eCommandMode::SupportBundleAfterFailure;
        self
    }

    pub fn backup_restore_expect_candidate_promotion_status(
        mut self,
        status: impl Into<String>,
    ) -> Self {
        self.backup_restore_expected_candidate_promotion_status = Some(status.into());
        self
    }

    pub fn skip_repair_candidate_build_preflight(mut self) -> Self {
        self.skip_repair_candidate_build_preflight = true;
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_env.insert(key.into(), value.into());
        self
    }

    pub fn require_json_pointer(mut self, pointer: impl Into<String>) -> Self {
        self.required_json_pointers.push(pointer.into());
        self
    }

    pub fn expected_runner_status(&self) -> &'static str {
        if self.labels.contains("self-test") {
            "fail"
        } else {
            "pass"
        }
    }
}

impl DoctorE2eRunner {
    pub fn new(run_root: impl AsRef<Path>) -> Result<Self, String> {
        let run_root = run_root.as_ref().to_path_buf();
        validate_run_root(&run_root)?;
        fs::create_dir_all(&run_root)
            .map_err(|err| format!("failed to create doctor e2e run root: {err}"))?;
        let artifact_root = run_root.join("artifacts");
        fs::create_dir_all(&artifact_root)
            .map_err(|err| format!("failed to create doctor e2e artifact root: {err}"))?;
        Ok(Self {
            run_root,
            artifact_root,
            cass_bin: PathBuf::from(cass_bin()),
        })
    }

    pub fn with_cass_bin(mut self, cass_bin: impl AsRef<Path>) -> Self {
        self.cass_bin = cass_bin.as_ref().to_path_buf();
        self
    }

    pub fn run_root(&self) -> &Path {
        &self.run_root
    }

    pub fn run_scenario(&self, spec: &DoctorE2eScenarioSpec) -> Result<DoctorE2eRunResult, String> {
        validate_scenario_id(&spec.scenario_id)?;
        let scenario_artifact_dir = self.artifact_root.join(&spec.scenario_id);
        create_new_dir(&scenario_artifact_dir)?;
        let fixture_parent = self.run_root.join("fixtures");
        let mut fixture = DoctorFixtureFactory::new_under(&fixture_parent, &spec.scenario_id);
        fixture.apply_scenario(spec.fixture_scenario);
        let backup_restore_journey =
            if spec.command_mode == DoctorE2eCommandMode::BackupsRestoreJourney {
                Some(prepare_doctor_e2e_backup_restore_journey_fixture(
                    &mut fixture,
                )?)
            } else {
                None
            };
        fixture
            .validate_manifest()
            .map_err(|err| format!("fixture manifest is invalid: {err}"))?;
        let _active_doctor_lock = if spec.fixture_scenario == DoctorFixtureScenario::ActiveLock
            || spec.command_mode == DoctorE2eCommandMode::SupportBundleAfterFailure
        {
            Some(hold_active_doctor_lock_for_fixture(&fixture)?)
        } else {
            None
        };

        let redactor =
            DoctorE2eRedactor::for_fixture(&self.run_root, &scenario_artifact_dir, &fixture);
        let mut artifacts = BTreeMap::new();
        let mut failures = Vec::new();

        write_json_artifact(
            &scenario_artifact_dir,
            "scenario.json",
            &fixture.manifest(),
            &mut artifacts,
        )?;

        let mut command_env = doctor_command_env(&fixture);
        if spec.fixture_scenario == DoctorFixtureScenario::MalformedSourcesToml {
            command_env.remove("CASS_IGNORE_SOURCES_CONFIG");
        }
        for (key, value) in &spec.extra_env {
            command_env.insert(key.clone(), value.clone());
        }
        let fixture_data_dir = fixture.data_dir().to_str().ok_or_else(|| {
            format!(
                "fixture data dir is not utf8: {}",
                fixture.data_dir().display()
            )
        })?;

        let mut command_records = Vec::new();
        let mut cleanup_approval_fingerprint = None;
        let mut repair_approval_fingerprint = None;
        let mut backup_restore_plan_fingerprint = None;

        if spec.command_mode == DoctorE2eCommandMode::SupportBundleAfterFailure {
            let failure_seed_args = vec![
                "doctor".to_string(),
                "--json".to_string(),
                "--fix".to_string(),
                "--data-dir".to_string(),
                fixture_data_dir.to_string(),
            ];
            let failure_seed = run_recorded_doctor_command(
                &self.cass_bin,
                &command_env,
                failure_seed_args,
                &scenario_artifact_dir,
                &mut artifacts,
                &redactor,
                DoctorCommandArtifactPaths {
                    command_id: "doctor-failure-seed",
                    stdout: "stdout/doctor-failure-seed.out",
                    stderr: "stderr/doctor-failure-seed.err",
                    parsed_json: Some("parsed-json/doctor-failure-seed.json"),
                },
            )?;
            if let Some(parse_failure) = &failure_seed.parse_failure {
                failures.push(format!("failure seed {parse_failure}"));
            }
            if failure_seed
                .parsed_json
                .as_ref()
                .and_then(|(value, _)| value.pointer("/failure_context/status"))
                .and_then(Value::as_str)
                != Some("captured")
            {
                failures.push(
                    "support bundle pre-step did not capture a doctor failure_context".to_string(),
                );
            }
            command_records.push(failure_seed.record);
        }

        if spec.command_mode == DoctorE2eCommandMode::BaselineDiffJourney {
            let baseline_save_args = vec![
                "doctor".to_string(),
                "baseline".to_string(),
                "save".to_string(),
                "known-good".to_string(),
                "--json".to_string(),
                "--data-dir".to_string(),
                fixture_data_dir.to_string(),
            ];
            let baseline_save = run_recorded_doctor_command(
                &self.cass_bin,
                &command_env,
                baseline_save_args,
                &scenario_artifact_dir,
                &mut artifacts,
                &redactor,
                DoctorCommandArtifactPaths {
                    command_id: "doctor-baseline-save",
                    stdout: "stdout/doctor-baseline-save.out",
                    stderr: "stderr/doctor-baseline-save.err",
                    parsed_json: Some("parsed-json/doctor-baseline-save.json"),
                },
            )?;
            if let Some(parse_failure) = &baseline_save.parse_failure {
                failures.push(format!("baseline save {parse_failure}"));
            }
            command_records.push(baseline_save.record);

            let semantic_dir = fixture.data_dir().join("index").join("semantic");
            fs::create_dir_all(&semantic_dir)
                .map_err(|err| format!("create baseline-diff semantic fixture dir: {err}"))?;
            fs::write(
                semantic_dir.join("metadata.json"),
                b"{\"fixture\":\"derived-only-baseline-diff\"}\n",
            )
            .map_err(|err| format!("write baseline-diff semantic fixture metadata: {err}"))?;
        }

        let before = DoctorE2eFileTreeSnapshot::capture(&[
            ("home", fixture.home_dir()),
            ("data", fixture.data_dir()),
        ])?;
        write_json_artifact(
            &scenario_artifact_dir,
            "file-tree-before.json",
            &before,
            &mut artifacts,
        )?;
        let fixture_inventory = build_fixture_inventory(spec, &fixture, &redactor, &before);
        write_json_artifact(
            &scenario_artifact_dir,
            "fixture-inventory.json",
            &fixture_inventory,
            &mut artifacts,
        )?;
        let source_inventory_before =
            build_source_inventory_snapshot(spec, &fixture, &redactor, &before, "before");
        write_json_artifact(
            &scenario_artifact_dir,
            "source-inventory-before.json",
            &source_inventory_before,
            &mut artifacts,
        )?;

        let human_check_args = vec![
            "doctor".to_string(),
            "check".to_string(),
            "--data-dir".to_string(),
            fixture_data_dir.to_string(),
        ];
        let human_check = run_recorded_doctor_command(
            &self.cass_bin,
            &command_env,
            human_check_args,
            &scenario_artifact_dir,
            &mut artifacts,
            &redactor,
            DoctorCommandArtifactPaths {
                command_id: "doctor-human-check",
                stdout: "stdout/doctor-human-check.out",
                stderr: "stderr/doctor-human-check.err",
                parsed_json: None,
            },
        )?;
        validate_human_check_output(&human_check, &mut failures);
        command_records.push(human_check.record);

        let robot_check_args = vec![
            "doctor".to_string(),
            "check".to_string(),
            "--json".to_string(),
            "--data-dir".to_string(),
            fixture_data_dir.to_string(),
        ];
        let robot_check = run_recorded_doctor_command(
            &self.cass_bin,
            &command_env,
            robot_check_args,
            &scenario_artifact_dir,
            &mut artifacts,
            &redactor,
            DoctorCommandArtifactPaths {
                command_id: "doctor-check-json",
                stdout: "stdout/doctor-check-json.out",
                stderr: "stderr/doctor-check-json.err",
                parsed_json: Some("parsed-json/doctor-check-json.json"),
            },
        )?;
        if let Some(parse_failure) = &robot_check.parse_failure {
            failures.push(format!("doctor check JSON preflight {parse_failure}"));
        }
        command_records.push(robot_check.record);

        if spec.command_mode == DoctorE2eCommandMode::CleanupApply {
            let preview_args = vec![
                "doctor".to_string(),
                "cleanup".to_string(),
                "--json".to_string(),
                "--data-dir".to_string(),
                fixture_data_dir.to_string(),
            ];
            let preview = run_recorded_doctor_command(
                &self.cass_bin,
                &command_env,
                preview_args,
                &scenario_artifact_dir,
                &mut artifacts,
                &redactor,
                DoctorCommandArtifactPaths {
                    command_id: "doctor-cleanup-preview",
                    stdout: "stdout/doctor-cleanup-preview.out",
                    stderr: "stderr/doctor-cleanup-preview.err",
                    parsed_json: Some("parsed-json/doctor-cleanup-preview.json"),
                },
            )?;
            if let Some(parse_failure) = &preview.parse_failure {
                failures.push(format!("cleanup preview {parse_failure}"));
            }
            cleanup_approval_fingerprint = preview
                .parsed_json
                .as_ref()
                .and_then(|(value, _)| cleanup_approval_fingerprint_from_json(value));
            if cleanup_approval_fingerprint.is_none() {
                failures.push(
                    "cleanup preview did not expose an approval fingerprint for apply".to_string(),
                );
            }
            command_records.push(preview.record);
        }
        if spec.command_mode == DoctorE2eCommandMode::RepairApply
            && !spec.skip_repair_candidate_build_preflight
        {
            let candidate_build_args = vec![
                "doctor".to_string(),
                "--fix".to_string(),
                "--json".to_string(),
                "--data-dir".to_string(),
                fixture_data_dir.to_string(),
            ];
            let candidate_build = run_recorded_doctor_command(
                &self.cass_bin,
                &command_env,
                candidate_build_args,
                &scenario_artifact_dir,
                &mut artifacts,
                &redactor,
                DoctorCommandArtifactPaths {
                    command_id: "doctor-repair-candidate-build",
                    stdout: "stdout/doctor-repair-candidate-build.out",
                    stderr: "stderr/doctor-repair-candidate-build.err",
                    parsed_json: Some("parsed-json/doctor-repair-candidate-build.json"),
                },
            )?;
            if let Some(parse_failure) = &candidate_build.parse_failure {
                failures.push(format!("repair candidate build {parse_failure}"));
            }
            command_records.push(candidate_build.record);
        }
        if spec.command_mode == DoctorE2eCommandMode::RepairApply {
            let dry_run_args = vec![
                "doctor".to_string(),
                "repair".to_string(),
                "--dry-run".to_string(),
                "--allow-repeated-repair".to_string(),
                "--json".to_string(),
                "--data-dir".to_string(),
                fixture_data_dir.to_string(),
            ];
            let dry_run = run_recorded_doctor_command(
                &self.cass_bin,
                &command_env,
                dry_run_args,
                &scenario_artifact_dir,
                &mut artifacts,
                &redactor,
                DoctorCommandArtifactPaths {
                    command_id: "doctor-repair-dry-run",
                    stdout: "stdout/doctor-repair-dry-run.out",
                    stderr: "stderr/doctor-repair-dry-run.err",
                    parsed_json: Some("parsed-json/doctor-repair-dry-run.json"),
                },
            )?;
            if let Some(parse_failure) = &dry_run.parse_failure {
                failures.push(format!("repair dry-run {parse_failure}"));
            }
            repair_approval_fingerprint = dry_run
                .parsed_json
                .as_ref()
                .and_then(|(value, _)| repair_approval_fingerprint_from_json(value));
            if repair_approval_fingerprint.is_none() {
                failures
                    .push("repair dry-run did not expose a plan fingerprint for apply".to_string());
            }
            command_records.push(dry_run.record);
        }
        if let Some(journey) = backup_restore_journey.as_ref() {
            let list_args = vec![
                "doctor".to_string(),
                "backups".to_string(),
                "list".to_string(),
                "--json".to_string(),
                "--data-dir".to_string(),
                fixture_data_dir.to_string(),
            ];
            let list = run_recorded_doctor_command(
                &self.cass_bin,
                &command_env,
                list_args,
                &scenario_artifact_dir,
                &mut artifacts,
                &redactor,
                DoctorCommandArtifactPaths {
                    command_id: "doctor-backups-list",
                    stdout: "stdout/doctor-backups-list.out",
                    stderr: "stderr/doctor-backups-list.err",
                    parsed_json: Some("parsed-json/doctor-backups-list.json"),
                },
            )?;
            if let Some(parse_failure) = &list.parse_failure {
                failures.push(format!("doctor backups list {parse_failure}"));
            }
            if let Some((value, _)) = &list.parsed_json {
                let listed_ids = value["backups"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|backup| backup["backup_id"].as_str())
                    .collect::<BTreeSet<_>>();
                if !listed_ids.contains(journey.good_backup_id.as_str())
                    || !listed_ids.contains(journey.drifted_backup_id.as_str())
                {
                    failures.push(format!(
                        "doctor backups list did not enumerate both fixture backups: {listed_ids:?}"
                    ));
                }
            }
            command_records.push(list.record);

            let verify_good_args = vec![
                "doctor".to_string(),
                "backups".to_string(),
                "verify".to_string(),
                journey.good_backup_id.clone(),
                "--json".to_string(),
                "--data-dir".to_string(),
                fixture_data_dir.to_string(),
            ];
            let verify_good = run_recorded_doctor_command(
                &self.cass_bin,
                &command_env,
                verify_good_args,
                &scenario_artifact_dir,
                &mut artifacts,
                &redactor,
                DoctorCommandArtifactPaths {
                    command_id: "doctor-backups-verify-good",
                    stdout: "stdout/doctor-backups-verify-good.out",
                    stderr: "stderr/doctor-backups-verify-good.err",
                    parsed_json: Some("parsed-json/doctor-backups-verify-good.json"),
                },
            )?;
            if let Some(parse_failure) = &verify_good.parse_failure {
                failures.push(format!("doctor backups verify good {parse_failure}"));
            }
            if let Some((value, _)) = &verify_good.parsed_json
                && value
                    .pointer("/backup_verification/status")
                    .and_then(Value::as_str)
                    != Some("verified")
            {
                failures.push(format!("good backup verification did not pass: {value:#}"));
            }
            command_records.push(verify_good.record);

            let rehearsal_good_args = vec![
                "doctor".to_string(),
                "backups".to_string(),
                "restore".to_string(),
                journey.good_backup_id.clone(),
                "--json".to_string(),
                "--data-dir".to_string(),
                fixture_data_dir.to_string(),
            ];
            let rehearsal_good = run_recorded_doctor_command(
                &self.cass_bin,
                &command_env,
                rehearsal_good_args,
                &scenario_artifact_dir,
                &mut artifacts,
                &redactor,
                DoctorCommandArtifactPaths {
                    command_id: "doctor-backups-restore-rehearsal-good",
                    stdout: "stdout/doctor-backups-restore-rehearsal-good.out",
                    stderr: "stderr/doctor-backups-restore-rehearsal-good.err",
                    parsed_json: Some("parsed-json/doctor-backups-restore-rehearsal-good.json"),
                },
            )?;
            if let Some(parse_failure) = &rehearsal_good.parse_failure {
                failures.push(format!(
                    "doctor backups restore rehearsal good {parse_failure}"
                ));
            }
            backup_restore_plan_fingerprint = rehearsal_good
                .parsed_json
                .as_ref()
                .and_then(|(value, _)| {
                    value
                        .pointer("/restore_plan/plan_fingerprint")
                        .and_then(Value::as_str)
                })
                .map(ToOwned::to_owned);
            if let Some((value, _)) = &rehearsal_good.parsed_json {
                if value
                    .pointer("/restore_rehearsal/status")
                    .and_then(Value::as_str)
                    != Some("passed")
                {
                    failures.push(format!("good backup rehearsal did not pass: {value:#}"));
                }
                if value
                    .pointer("/restore_rehearsal/live_archive_untouched")
                    .and_then(Value::as_bool)
                    != Some(true)
                {
                    failures.push(
                        "good backup rehearsal did not prove live archive was untouched"
                            .to_string(),
                    );
                }
            }
            if backup_restore_plan_fingerprint.is_none() {
                failures.push(
                    "good backup rehearsal did not expose a restore plan fingerprint".to_string(),
                );
            }
            command_records.push(rehearsal_good.record);

            let rehearsal_drifted_args = vec![
                "doctor".to_string(),
                "backups".to_string(),
                "restore".to_string(),
                journey.drifted_backup_id.clone(),
                "--json".to_string(),
                "--data-dir".to_string(),
                fixture_data_dir.to_string(),
            ];
            let rehearsal_drifted = run_recorded_doctor_command(
                &self.cass_bin,
                &command_env,
                rehearsal_drifted_args,
                &scenario_artifact_dir,
                &mut artifacts,
                &redactor,
                DoctorCommandArtifactPaths {
                    command_id: "doctor-backups-restore-rehearsal-drifted",
                    stdout: "stdout/doctor-backups-restore-rehearsal-drifted.out",
                    stderr: "stderr/doctor-backups-restore-rehearsal-drifted.err",
                    parsed_json: Some("parsed-json/doctor-backups-restore-rehearsal-drifted.json"),
                },
            )?;
            if let Some(parse_failure) = &rehearsal_drifted.parse_failure {
                failures.push(format!(
                    "doctor backups restore rehearsal drifted {parse_failure}"
                ));
            }
            if let Some((value, _)) = &rehearsal_drifted.parsed_json
                && value
                    .pointer("/restore_rehearsal/status")
                    .and_then(Value::as_str)
                    != Some("blocked")
            {
                failures.push(format!(
                    "drifted backup rehearsal was not blocked: {value:#}"
                ));
            }
            command_records.push(rehearsal_drifted.record);
        }

        let mut doctor_args = match spec.command_mode {
            DoctorE2eCommandMode::Check => {
                vec![
                    "doctor".to_string(),
                    "check".to_string(),
                    "--json".to_string(),
                ]
            }
            DoctorE2eCommandMode::Fix => {
                vec![
                    "doctor".to_string(),
                    "--json".to_string(),
                    "--fix".to_string(),
                ]
            }
            DoctorE2eCommandMode::CleanupApply => vec![
                "doctor".to_string(),
                "cleanup".to_string(),
                "--yes".to_string(),
                "--plan-fingerprint".to_string(),
                cleanup_approval_fingerprint
                    .clone()
                    .unwrap_or_else(|| "missing-cleanup-approval-fingerprint".to_string()),
                "--json".to_string(),
            ],
            DoctorE2eCommandMode::RepairApply => vec![
                "doctor".to_string(),
                "repair".to_string(),
                "--yes".to_string(),
                "--plan-fingerprint".to_string(),
                repair_approval_fingerprint
                    .clone()
                    .unwrap_or_else(|| "missing-repair-approval-fingerprint".to_string()),
                "--allow-repeated-repair".to_string(),
                "--json".to_string(),
            ],
            DoctorE2eCommandMode::BackupsRestoreJourney => {
                let journey = backup_restore_journey
                    .as_ref()
                    .expect("backup restore journey fixture");
                vec![
                    "doctor".to_string(),
                    "backups".to_string(),
                    "restore".to_string(),
                    journey.good_backup_id.clone(),
                    "--yes".to_string(),
                    "--plan-fingerprint".to_string(),
                    backup_restore_plan_fingerprint
                        .clone()
                        .unwrap_or_else(|| "missing-backup-restore-plan-fingerprint".to_string()),
                    "--json".to_string(),
                ]
            }
            DoctorE2eCommandMode::BaselineDiffJourney => vec![
                "doctor".to_string(),
                "baseline".to_string(),
                "diff".to_string(),
                "known-good".to_string(),
                "--json".to_string(),
            ],
            DoctorE2eCommandMode::SupportBundleAfterFailure => {
                vec![
                    "doctor".to_string(),
                    "support-bundle".to_string(),
                    "--json".to_string(),
                ]
            }
        };
        doctor_args.push("--data-dir".to_string());
        doctor_args.push(fixture_data_dir.to_string());

        let final_command = run_recorded_doctor_command(
            &self.cass_bin,
            &command_env,
            doctor_args,
            &scenario_artifact_dir,
            &mut artifacts,
            &redactor,
            DoctorCommandArtifactPaths {
                command_id: "doctor-json",
                stdout: "stdout/doctor-json.out",
                stderr: "stderr/doctor-json.err",
                parsed_json: Some("parsed-json/doctor-json.json"),
            },
        )?;
        let exit_code = final_command.record.exit_code;
        let redacted_stdout = final_command.redacted_stdout.clone();
        let redacted_stderr = final_command.redacted_stderr.clone();
        let parsed_json = final_command.parsed_json.clone();
        if let Some(parse_failure) = &final_command.parse_failure {
            failures.push(parse_failure.clone());
        }
        command_records.push(final_command.record);

        if let Some(expected) = spec.expect_exit_success {
            let actual = exit_code == Some(0);
            if actual != expected {
                failures.push(format!(
                    "exit success mismatch: expected={expected} actual={actual}"
                ));
            }
        }
        if let Some((value, _)) = &parsed_json {
            for pointer in &spec.required_json_pointers {
                if value.pointer(pointer).is_none() {
                    failures.push(format!("required JSON pointer is absent: {pointer}"));
                }
            }
            if spec.command_mode == DoctorE2eCommandMode::BackupsRestoreJourney {
                validate_doctor_backups_restore_journey_payload(
                    value,
                    spec.backup_restore_expected_candidate_promotion_status
                        .as_deref(),
                    &mut failures,
                );
            } else if spec.command_mode == DoctorE2eCommandMode::BaselineDiffJourney {
                validate_doctor_baseline_diff_journey_payload(value, &mut failures);
            } else if spec.command_mode == DoctorE2eCommandMode::SupportBundleAfterFailure {
                validate_doctor_support_bundle_payload(value, &mut failures);
            } else {
                let manifest_assertion =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        fixture.assert_doctor_payload_matches_manifest(value);
                    }));
                if let Err(payload) = manifest_assertion {
                    failures.push(format!(
                        "doctor JSON did not match fixture scenario manifest: {}",
                        panic_payload_to_string(payload)
                    ));
                }
            }
        }
        let candidate_staging_artifact = parsed_json
            .as_ref()
            .and_then(|(value, _)| value.pointer("/candidate_staging").cloned())
            .unwrap_or(Value::Null);
        write_json_artifact(
            &scenario_artifact_dir,
            "candidate-staging.json",
            &candidate_staging_artifact,
            &mut artifacts,
        )?;

        let after = DoctorE2eFileTreeSnapshot::capture(&[
            ("home", fixture.home_dir()),
            ("data", fixture.data_dir()),
        ])?;
        write_json_artifact(
            &scenario_artifact_dir,
            "file-tree-after.json",
            &after,
            &mut artifacts,
        )?;
        let source_inventory_after =
            build_source_inventory_snapshot(spec, &fixture, &redactor, &after, "after");
        write_json_artifact(
            &scenario_artifact_dir,
            "source-inventory-after.json",
            &source_inventory_after,
            &mut artifacts,
        )?;
        let post_repair_probes = build_post_repair_probes(
            spec,
            &fixture,
            &redactor,
            parsed_json.as_ref().map(|(value, _)| value),
        );
        write_json_artifact(
            &scenario_artifact_dir,
            "post-repair-probes.json",
            &post_repair_probes,
            &mut artifacts,
        )?;
        write_candidate_promotion_derived_followup_artifact(
            &scenario_artifact_dir,
            &fixture,
            &redactor,
            &mut artifacts,
        )?;

        let mutation_diffs = before.diff(&after);
        let protected_mutation_diffs = doctor_e2e_protected_mutation_diffs(spec, &mutation_diffs);
        let ignored_diagnostic_mutation_diffs =
            doctor_e2e_ignored_diagnostic_mutation_diffs(spec, &mutation_diffs);
        if !spec.allow_mutation && !protected_mutation_diffs.is_empty() {
            failures.push(format!(
                "no-mutation contract was violated: {}",
                protected_mutation_diffs.join("; ")
            ));
        }
        let no_mutation_summary = build_no_mutation_summary(
            spec,
            &fixture,
            &before,
            &after,
            &protected_mutation_diffs,
            &mutation_diffs,
            &ignored_diagnostic_mutation_diffs,
        );
        write_json_artifact(
            &scenario_artifact_dir,
            "no-mutation-summary.json",
            &no_mutation_summary,
            &mut artifacts,
        )?;
        let safe_auto_decision_log = build_safe_auto_decision_log(
            spec,
            parsed_json.as_ref().map(|(value, _)| value),
            &source_inventory_before,
            &source_inventory_after,
            &no_mutation_summary,
            command_records
                .last()
                .expect("at least final doctor command recorded"),
        );
        write_json_artifact(
            &scenario_artifact_dir,
            "safe-auto-decision-log.json",
            &safe_auto_decision_log,
            &mut artifacts,
        )?;

        write_json_artifact(
            &scenario_artifact_dir,
            "checksums.json",
            &after.file_checksums(),
            &mut artifacts,
        )?;
        write_json_artifact(
            &scenario_artifact_dir,
            "timing.json",
            &build_doctor_e2e_timing_report(spec, &command_records),
            &mut artifacts,
        )?;
        write_text_artifact(
            &scenario_artifact_dir,
            "receipts.jsonl",
            "{\"event\":\"receipt_scan\",\"status\":\"none-found\"}\n",
            &mut artifacts,
        )?;
        let mut doctor_events = vec![json!({
            "event": "scenario_start",
            "scenario_id": spec.scenario_id
        })];
        if let Some((value, _)) = &parsed_json {
            match value
                .pointer("/event_log/events")
                .and_then(serde_json::Value::as_array)
            {
                Some(events) if !events.is_empty() => {
                    doctor_events.extend(events.iter().cloned());
                }
                _ => {
                    failures.push(
                        "doctor JSON did not include a non-empty /event_log/events array"
                            .to_string(),
                    );
                    doctor_events.push(json!({
                        "event": "doctor_event_log_missing",
                        "status": "fail"
                    }));
                }
            }
        } else {
            doctor_events.push(json!({
                "event": "doctor_event_log_unavailable",
                "status": "fail"
            }));
        }
        doctor_events.push(json!({
            "event": "scenario_end",
            "scenario_id": spec.scenario_id,
            "failure_count": failures.len()
        }));
        write_jsonl_artifact(
            &scenario_artifact_dir,
            "doctor-events.jsonl",
            &doctor_events,
            &mut artifacts,
        )?;

        let final_command_record = command_records
            .last()
            .cloned()
            .expect("at least final doctor command recorded");
        write_jsonl_artifact(
            &scenario_artifact_dir,
            "commands.jsonl",
            &command_records
                .iter()
                .map(|record| serde_json::to_value(record).expect("command record json"))
                .collect::<Vec<_>>(),
            &mut artifacts,
        )?;
        let execution_flow = build_execution_flow_log(
            spec,
            &fixture_inventory,
            &source_inventory_before,
            &source_inventory_after,
            &post_repair_probes,
            parsed_json.as_ref().map(|(value, _)| value),
            &final_command_record,
            &protected_mutation_diffs,
        );
        write_jsonl_artifact(
            &scenario_artifact_dir,
            "execution-flow.jsonl",
            &execution_flow,
            &mut artifacts,
        )?;

        let mut failure_context = if failures.is_empty() {
            None
        } else {
            let context = build_failure_context(FailureContextBuildInput {
                spec,
                fixture: &fixture,
                redactor: &redactor,
                command_records: &command_records,
                final_command_record: &final_command_record,
                failures: &failures,
                parsed_json: parsed_json.as_ref().map(|(value, _)| value),
                doctor_events: &doctor_events,
                redacted_stdout: &redacted_stdout,
                redacted_stderr: &redacted_stderr,
                cleanup_approval_fingerprint: cleanup_approval_fingerprint
                    .as_deref()
                    .or(repair_approval_fingerprint.as_deref())
                    .or(backup_restore_plan_fingerprint.as_deref()),
            });
            write_json_artifact(
                &scenario_artifact_dir,
                "failure_context.json",
                &context,
                &mut artifacts,
            )?;
            let summary = render_failure_summary(&spec.scenario_id, &context);
            write_text_artifact(
                &scenario_artifact_dir,
                "failure_summary.txt",
                &summary,
                &mut artifacts,
            )?;
            Some(context)
        };
        let mut redaction_report =
            build_redaction_report(spec, &fixture, &scenario_artifact_dir, &artifacts)?;
        if redaction_report_has_leaks(&redaction_report) && failure_context.is_none() {
            failures.push(format!(
                "redaction audit found {} leak(s) in default doctor e2e artifacts",
                redaction_report["leak_count"].as_u64().unwrap_or_default()
            ));
            let context = build_failure_context(FailureContextBuildInput {
                spec,
                fixture: &fixture,
                redactor: &redactor,
                command_records: &command_records,
                final_command_record: &final_command_record,
                failures: &failures,
                parsed_json: parsed_json.as_ref().map(|(value, _)| value),
                doctor_events: &doctor_events,
                redacted_stdout: &redacted_stdout,
                redacted_stderr: &redacted_stderr,
                cleanup_approval_fingerprint: cleanup_approval_fingerprint
                    .as_deref()
                    .or(repair_approval_fingerprint.as_deref())
                    .or(backup_restore_plan_fingerprint.as_deref()),
            });
            write_json_artifact(
                &scenario_artifact_dir,
                "failure_context.json",
                &context,
                &mut artifacts,
            )?;
            let summary = render_failure_summary(&spec.scenario_id, &context);
            write_text_artifact(
                &scenario_artifact_dir,
                "failure_summary.txt",
                &summary,
                &mut artifacts,
            )?;
            failure_context = Some(context);
            redaction_report =
                build_redaction_report(spec, &fixture, &scenario_artifact_dir, &artifacts)?;
        }
        write_json_artifact(
            &scenario_artifact_dir,
            "redaction-report.json",
            &redaction_report,
            &mut artifacts,
        )?;

        let status =
            if failure_context.is_some() || redaction_report_has_leaks(&redaction_report) {
                "fail"
            } else {
                "pass"
            }
            .to_string();

        let manifest = DoctorE2eArtifactManifest {
            schema_version: DOCTOR_E2E_SCHEMA_VERSION,
            scenario_id: spec.scenario_id.clone(),
            labels: spec.labels.iter().cloned().collect(),
            status: status.clone(),
            artifact_dir: redactor.redact(&scenario_artifact_dir.display().to_string()),
            fixture_root: redactor.redact(&fixture.root().display().to_string()),
            home_dir: redactor.redact(&fixture.home_dir().display().to_string()),
            data_dir: redactor.redact(&fixture.data_dir().display().to_string()),
            command_count: command_records.len(),
            artifacts,
            failure_context: failure_context.clone(),
        };
        let manifest_path = scenario_artifact_dir.join("manifest.json");
        write_json_file_new(&manifest_path, &manifest)?;
        validate_artifact_manifest(&manifest_path)?;

        Ok(DoctorE2eRunResult {
            scenario_id: spec.scenario_id.clone(),
            status,
            artifact_dir: scenario_artifact_dir,
            manifest_path,
            failure_context,
        })
    }
}

fn hold_active_doctor_lock_for_fixture(fixture: &DoctorFixtureFactory) -> Result<fs::File, String> {
    let lock_path = fixture
        .data_dir()
        .join("doctor")
        .join("locks")
        .join("doctor-repair.lock");
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create active lock parent: {err}"))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|err| format!("failed to open active doctor lock fixture: {err}"))?;
    file.try_lock_exclusive()
        .map_err(|err| format!("failed to hold active doctor lock fixture: {err}"))?;
    Ok(file)
}

fn validate_human_check_output(command: &RecordedDoctorCommand, failures: &mut Vec<String>) {
    let stdout = command.redacted_stdout.trim();
    if stdout.is_empty() {
        failures.push("doctor human check produced empty stdout".to_string());
    }
    if contains_unsafe_deletion_recipe(stdout)
        || contains_unsafe_deletion_recipe(&command.redacted_stderr)
    {
        failures.push("doctor human check included an unsafe deletion or reset recipe".to_string());
    }
    if !human_output_has_actionable_guidance(stdout) {
        failures.push(
            "doctor human check did not include actionable guidance or an explicit no-action result"
                .to_string(),
        );
    }
}

fn contains_unsafe_deletion_recipe(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "rm -rf",
        "git reset --hard",
        "git clean -fd",
        "delete the directory",
        "delete your",
        "remove the directory",
        "remove your",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn human_output_has_actionable_guidance(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "recommended action",
        "next safe command",
        "next step",
        "no action",
        "run cass",
        "cass ",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn run_recorded_doctor_command(
    cass_bin: &Path,
    command_env: &BTreeMap<String, String>,
    args: Vec<String>,
    artifact_dir: &Path,
    artifacts: &mut BTreeMap<String, String>,
    redactor: &DoctorE2eRedactor,
    artifact_paths: DoctorCommandArtifactPaths<'_>,
) -> Result<RecordedDoctorCommand, String> {
    let command_start = Instant::now();
    let mut command = Command::new(cass_bin);
    command.args(&args);
    for (key, value) in command_env {
        command.env(key, value);
    }
    let output = command
        .output()
        .map_err(|err| format!("failed to run {}: {err}", artifact_paths.command_id))?;
    let duration_ms = elapsed_ms(command_start);
    let exit_code = output.status.code();
    let stdout_text = String::from_utf8_lossy(&output.stdout);
    let stderr_text = String::from_utf8_lossy(&output.stderr);
    let redacted_stdout = redactor.redact(&stdout_text);
    let redacted_stderr = redactor.redact(&stderr_text);

    let stdout_path = write_text_artifact(
        artifact_dir,
        artifact_paths.stdout,
        &redacted_stdout,
        artifacts,
    )?;
    let stderr_path = write_text_artifact(
        artifact_dir,
        artifact_paths.stderr,
        &redacted_stderr,
        artifacts,
    )?;

    let (parsed_json, parse_failure) = if let Some(parsed_json_path) = artifact_paths.parsed_json {
        match serde_json::from_slice::<Value>(&output.stdout) {
            Ok(value) => {
                let redacted_value = redact_json_value(value, redactor);
                let parsed_path = write_json_artifact(
                    artifact_dir,
                    parsed_json_path,
                    &redacted_value,
                    artifacts,
                )?;
                (Some((redacted_value, parsed_path)), None)
            }
            Err(err) => (
                None,
                Some(format!("doctor stdout was not valid JSON: {err}")),
            ),
        }
    } else {
        (None, None)
    };

    let argv = std::iter::once(redactor.redact(&cass_bin.display().to_string()))
        .chain(args.iter().map(|arg| redactor.redact(arg)))
        .collect();
    let record = DoctorE2eCommandRecord {
        command_id: artifact_paths.command_id.to_string(),
        argv,
        env: command_env
            .iter()
            .map(|(key, value)| (key.clone(), redactor.redact(value)))
            .collect(),
        exit_code,
        duration_ms,
        stdout_path,
        stderr_path,
        parsed_json_path: parsed_json.as_ref().map(|(_, path)| path.clone()),
        parsed_json_ok: parsed_json.is_some(),
        failure_reason: parse_failure.clone(),
    };

    Ok(RecordedDoctorCommand {
        record,
        parsed_json,
        redacted_stdout,
        redacted_stderr,
        parse_failure,
    })
}

fn cleanup_approval_fingerprint_from_json(value: &Value) -> Option<String> {
    value
        .pointer("/quarantine/lexical_cleanup_dry_run/approval_fingerprint")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .pointer("/quarantine/summary/cleanup_dry_run_approval_fingerprint")
                .and_then(Value::as_str)
        })
        .map(ToOwned::to_owned)
}

fn repair_approval_fingerprint_from_json(value: &Value) -> Option<String> {
    value
        .pointer("/repair_plan/plan_fingerprint")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn build_failure_context(input: FailureContextBuildInput<'_>) -> DoctorE2eFailureContext {
    let manifest = input.fixture.manifest();
    let (failed_phase, failed_check) = classify_failure(input.failures);
    let selected_authority = input
        .parsed_json
        .and_then(|value| {
            value
                .pointer("/source_authority/selected_authority")
                .cloned()
        })
        .or_else(|| {
            input.parsed_json.and_then(|value| {
                value
                    .pointer("/source_authority/selected_authorities")
                    .cloned()
            })
        });
    let rejected_authorities = input.parsed_json.and_then(|value| {
        value
            .pointer("/source_authority/rejected_authorities")
            .cloned()
    });
    let active_locks = input
        .parsed_json
        .and_then(|value| value.pointer("/locks").cloned());
    let coverage_summary = input
        .parsed_json
        .and_then(|value| value.pointer("/coverage_summary").cloned());
    let plan_fingerprint = input
        .cleanup_approval_fingerprint
        .map(ToOwned::to_owned)
        .or_else(|| {
            input
                .parsed_json
                .and_then(cleanup_approval_fingerprint_from_json)
                .or_else(|| {
                    first_string_at_any_pointer(
                        input.parsed_json,
                        &[
                            "/cleanup_apply/plan_fingerprint",
                            "/quarantine/summary/cleanup_dry_run_approval_fingerprint",
                            "/active_repair/plan_fingerprint",
                        ],
                    )
                })
        });

    DoctorE2eFailureContext {
        schema_version: DOCTOR_E2E_SCHEMA_VERSION,
        scenario_id: input.spec.scenario_id.clone(),
        failed_phase,
        failed_check,
        reasons: input.failures.to_vec(),
        command_id: Some(input.final_command_record.command_id.clone()),
        exit_code: input.final_command_record.exit_code,
        command: input.final_command_record.clone(),
        command_history: input.command_records.to_vec(),
        platform: DoctorE2eFailurePlatformContext {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            family: std::env::consts::FAMILY.to_string(),
            cass_version: env!("CARGO_PKG_VERSION").to_string(),
            git_revision: option_env!("VERGEN_GIT_SHA")
                .or(option_env!("GIT_HASH"))
                .map(ToOwned::to_owned),
        },
        fixture: DoctorE2eFailureFixtureContext {
            fixture_id: manifest.fixture_id.clone(),
            fixture_root: input
                .redactor
                .redact(&input.fixture.root().display().to_string()),
            home_dir: input
                .redactor
                .redact(&input.fixture.home_dir().display().to_string()),
            data_dir: input
                .redactor
                .redact(&input.fixture.data_dir().display().to_string()),
            risk_class: manifest.risk_class.clone(),
            expected_mutation_class: manifest.expected_mutation_class.clone(),
            repair_eligibility: manifest.repair_eligibility.clone(),
            scenario_fixture: format!("{:?}", input.spec.fixture_scenario),
        },
        artifacts: DoctorE2eFailureArtifactRefs {
            artifact_manifest_path: "manifest.json".to_string(),
            commands_path: "commands.jsonl".to_string(),
            doctor_events_path: "doctor-events.jsonl".to_string(),
            execution_flow_path: "execution-flow.jsonl".to_string(),
            receipts_path: "receipts.jsonl".to_string(),
            checksums_path: "checksums.json".to_string(),
            stdout_path: input.final_command_record.stdout_path.clone(),
            stderr_path: input.final_command_record.stderr_path.clone(),
            parsed_json_path: input.final_command_record.parsed_json_path.clone(),
            failure_context_path: "failure_context.json".to_string(),
            failure_summary_path: "failure_summary.txt".to_string(),
        },
        repro: build_safe_failure_repro(input.final_command_record),
        recent_events: recent_event_tail(input.doctor_events, 8),
        operation_id: first_event_string(input.doctor_events, "operation_id").or_else(|| {
            first_string_at_any_pointer(
                input.parsed_json,
                &[
                    "/event_log/operation_id",
                    "/operation_outcome/operation_id",
                    "/active_repair/operation_id",
                ],
            )
        }),
        plan_fingerprint,
        selected_authority,
        rejected_authorities,
        active_locks,
        coverage_summary,
        stdout_tail: Some(tail_chars(input.redacted_stdout, 4096)),
        stderr_tail: Some(tail_chars(input.redacted_stderr, 4096)),
    }
}

fn build_redaction_report(
    spec: &DoctorE2eScenarioSpec,
    fixture: &DoctorFixtureFactory,
    artifact_dir: &Path,
    artifacts: &BTreeMap<String, String>,
) -> Result<Value, String> {
    let manifest = fixture.manifest();
    let needles = [
        ("privacy_sentinel_value", PRIVACY_SENTINEL_VALUE.to_string()),
        ("fixture_root_path", fixture.root().display().to_string()),
        (
            "fixture_home_path",
            fixture.home_dir().display().to_string(),
        ),
        (
            "fixture_data_path",
            fixture.data_dir().display().to_string(),
        ),
        ("artifact_dir_path", artifact_dir.display().to_string()),
    ];
    let mut checks = Vec::new();
    let mut leak_count = 0_usize;
    for (needle_id, needle) in needles {
        if needle.is_empty() {
            continue;
        }
        let mut offending_artifacts = Vec::new();
        for (artifact_key, relative) in artifacts {
            validate_artifact_relative_path(relative)?;
            let path = artifact_dir.join(relative);
            if !path.is_file() {
                continue;
            }
            let bytes = fs::read(&path).map_err(|err| format!("read {}: {err}", path.display()))?;
            if String::from_utf8_lossy(&bytes).contains(&needle) {
                offending_artifacts.push(json!({
                    "artifact_key": artifact_key,
                    "relative_path": relative,
                }));
            }
        }
        leak_count += offending_artifacts.len();
        checks.push(json!({
            "needle_id": needle_id,
            "needle_blake3": blake3::hash(needle.as_bytes()).to_hex().to_string(),
            "status": if offending_artifacts.is_empty() { "pass" } else { "fail" },
            "offending_artifact_count": offending_artifacts.len(),
            "offending_artifacts": offending_artifacts,
        }));
    }
    Ok(json!({
        "schema_version": DOCTOR_E2E_SCHEMA_VERSION,
        "report_kind": "cass_doctor_e2e_redaction_report_v1",
        "scenario_id": spec.scenario_id,
        "status": if leak_count == 0 { "pass" } else { "fail" },
        "leak_count": leak_count,
        "checked_artifact_count": artifacts.len(),
        "scan_scope": "default-shareable-doctor-e2e-artifacts-before-manifest",
        "raw_needles_included": false,
        "redaction_policy": &manifest.redaction_policy,
        "privacy_sentinel_count": manifest.privacy_sentinels.len(),
        "checks": checks,
    }))
}

fn redaction_report_has_leaks(report: &Value) -> bool {
    report["status"].as_str() != Some("pass")
        || report["leak_count"].as_u64().unwrap_or_default() != 0
}

fn build_safe_failure_repro(command_record: &DoctorE2eCommandRecord) -> DoctorE2eFailureRepro {
    DoctorE2eFailureRepro {
        safety: "fixture-only-redacted-template".to_string(),
        mutates_live_archive: false,
        requires_explicit_live_archive: true,
        target: "[doctor-e2e-data]".to_string(),
        working_directory: "[repo-root]".to_string(),
        command_json: command_record.argv.clone(),
        shell_command: shell_join(&command_record.argv),
        notes: vec![
            "This command template targets the captured doctor e2e fixture data dir placeholder, not a live user archive.".to_string(),
            "Do not replace [doctor-e2e-data] with a live CASS data directory unless the operator explicitly chooses that risk.".to_string(),
            "Use the artifact manifest, command log, parsed JSON, and doctor events in this directory as the primary debugging context.".to_string(),
        ],
    }
}

fn classify_failure(failures: &[String]) -> (String, String) {
    let first = failures
        .first()
        .map(String::as_str)
        .unwrap_or("unknown failure");
    if first.contains("not valid JSON") {
        ("parse".to_string(), "parse_doctor_json".to_string())
    } else if first.contains("required JSON pointer") {
        (
            "verification".to_string(),
            "assert_required_json_pointer".to_string(),
        )
    } else if first.contains("event_log") {
        ("verification".to_string(), "doctor_event_log".to_string())
    } else if first.contains("no-mutation") {
        ("safety".to_string(), "no_mutation_contract".to_string())
    } else if first.contains("exit success mismatch") {
        ("command".to_string(), "exit_status".to_string())
    } else {
        (
            "verification".to_string(),
            "doctor_e2e_assertion".to_string(),
        )
    }
}

fn recent_event_tail(events: &[Value], max_events: usize) -> Vec<Value> {
    let skip = events.len().saturating_sub(max_events);
    events.iter().skip(skip).cloned().collect()
}

fn first_event_string(events: &[Value], key: &str) -> Option<String> {
    events
        .iter()
        .filter_map(|event| event.get(key).and_then(Value::as_str))
        .find(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn first_string_at_any_pointer(parsed_json: Option<&Value>, pointers: &[&str]) -> Option<String> {
    let value = parsed_json?;
    pointers
        .iter()
        .filter_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
        .find(|text| !text.trim().is_empty())
        .map(ToOwned::to_owned)
}

impl DoctorE2eFileTreeSnapshot {
    pub fn capture(roots: &[(&str, &Path)]) -> Result<Self, String> {
        let mut captured = Vec::new();
        for (root_id, root) in roots {
            let mut entries = Vec::new();
            if root.exists() {
                for entry in WalkDir::new(root)
                    .follow_links(false)
                    .sort_by_file_name()
                    .into_iter()
                {
                    let entry = entry.map_err(|err| format!("walk {}: {err}", root.display()))?;
                    let path = entry.path();
                    if path == *root {
                        continue;
                    }
                    let metadata = fs::symlink_metadata(path)
                        .map_err(|err| format!("metadata {}: {err}", path.display()))?;
                    let relative_path = path
                        .strip_prefix(root)
                        .map_err(|err| format!("strip root {}: {err}", root.display()))?
                        .to_string_lossy()
                        .replace('\\', "/");
                    let entry_kind = if metadata.file_type().is_symlink() {
                        "symlink"
                    } else if metadata.is_dir() {
                        "dir"
                    } else if metadata.is_file() {
                        "file"
                    } else {
                        "other"
                    };
                    let blake3 = if metadata.is_file() {
                        Some(file_blake3(path)?)
                    } else {
                        None
                    };
                    entries.push(DoctorE2eFileEntry {
                        relative_path,
                        entry_kind: entry_kind.to_string(),
                        size_bytes: metadata.len(),
                        modified_unix_ms: metadata_modified_unix_ms(&metadata),
                        blake3,
                    });
                }
            }
            entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
            captured.push(DoctorE2eFileTreeRoot {
                root_id: (*root_id).to_string(),
                entries,
            });
        }
        captured.sort_by(|left, right| left.root_id.cmp(&right.root_id));
        Ok(Self { roots: captured })
    }

    pub fn diff(&self, after: &Self) -> Vec<String> {
        let before = self.entry_map();
        let after = after.entry_map();
        let mut diffs = Vec::new();
        for (key, before_entry) in &before {
            match after.get(key) {
                Some(after_entry) if after_entry == before_entry => {}
                Some(after_entry) => diffs.push(format!(
                    "changed:{}:{key}",
                    classify_entry_change(before_entry, after_entry)
                )),
                None => diffs.push(format!("removed:{key}")),
            }
        }
        for key in after.keys() {
            if !before.contains_key(key) {
                diffs.push(format!("added:{key}"));
            }
        }
        diffs.sort();
        diffs
    }

    pub fn file_checksums(&self) -> Vec<Value> {
        let mut checksums = Vec::new();
        for root in &self.roots {
            for entry in &root.entries {
                if let Some(blake3) = &entry.blake3 {
                    checksums.push(json!({
                        "root_id": root.root_id,
                        "relative_path": entry.relative_path,
                        "size_bytes": entry.size_bytes,
                        "modified_unix_ms": entry.modified_unix_ms,
                        "blake3": blake3,
                    }));
                }
            }
        }
        checksums
    }

    fn entry_map(&self) -> BTreeMap<String, DoctorE2eFileEntry> {
        let mut map = BTreeMap::new();
        for root in &self.roots {
            for entry in &root.entries {
                map.insert(
                    format!("{}/{}", root.root_id, entry.relative_path),
                    entry.clone(),
                );
            }
        }
        map
    }
}

fn metadata_modified_unix_ms(metadata: &fs::Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?;
    match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => Some(duration_millis_i64(duration)),
        Err(err) => Some(-duration_millis_i64(err.duration())),
    }
}

fn duration_millis_i64(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn classify_entry_change(before: &DoctorE2eFileEntry, after: &DoctorE2eFileEntry) -> &'static str {
    if before.entry_kind == after.entry_kind
        && before.size_bytes == after.size_bytes
        && before.blake3 == after.blake3
        && before.modified_unix_ms != after.modified_unix_ms
    {
        "metadata-only"
    } else if before.blake3 != after.blake3 {
        "content"
    } else {
        "metadata-or-type"
    }
}

impl DoctorE2eRedactor {
    fn for_fixture(run_root: &Path, artifact_dir: &Path, fixture: &DoctorFixtureFactory) -> Self {
        let mut replacements = vec![
            (
                fixture.home_dir().display().to_string(),
                "[doctor-e2e-home]".to_string(),
            ),
            (
                fixture.data_dir().display().to_string(),
                "[doctor-e2e-data]".to_string(),
            ),
            (
                fixture.root().display().to_string(),
                "[doctor-e2e-fixture]".to_string(),
            ),
            (
                artifact_dir.display().to_string(),
                "[doctor-e2e-artifacts]".to_string(),
            ),
            (
                run_root.display().to_string(),
                "[doctor-e2e-root]".to_string(),
            ),
            (
                PRIVACY_SENTINEL_VALUE.to_string(),
                "[doctor-e2e-secret]".to_string(),
            ),
        ];
        replacements.sort_by_key(|replacement| std::cmp::Reverse(replacement.0.len()));
        Self { replacements }
    }

    fn redact(&self, text: &str) -> String {
        let mut redacted = text.to_string();
        for (needle, replacement) in &self.replacements {
            redacted = redacted.replace(needle, replacement);
        }
        redacted
    }
}

fn build_fixture_inventory(
    spec: &DoctorE2eScenarioSpec,
    fixture: &DoctorFixtureFactory,
    redactor: &DoctorE2eRedactor,
    before: &DoctorE2eFileTreeSnapshot,
) -> Value {
    let manifest = fixture.manifest();
    let expected_source_inventory = &manifest.expected_source_inventory;
    let db_row_counts = read_fixture_db_row_counts(fixture.data_dir(), redactor);
    let data_dir_entries: Vec<_> = before
        .roots
        .iter()
        .find(|root| root.root_id == "data")
        .map(|root| {
            root.entries
                .iter()
                .map(|entry| {
                    json!({
                        "relative_path": entry.relative_path,
                        "entry_kind": entry.entry_kind,
                        "size_bytes": entry.size_bytes,
                        "modified_unix_ms": entry.modified_unix_ms,
                        "blake3": entry.blake3,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let mirror_hash_inventory: Vec<_> = manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_kind.starts_with("raw_mirror_"))
        .map(|artifact| {
            json!({
                "artifact_kind": artifact.artifact_kind,
                "relative_path": artifact.relative_path,
                "size_bytes": artifact.size_bytes,
                "blake3": artifact.blake3,
            })
        })
        .collect();

    json!({
        "schema_version": DOCTOR_E2E_SCHEMA_VERSION,
        "scenario_id": spec.scenario_id,
        "fixture_id": manifest.fixture_id,
        "labels": spec.labels.iter().cloned().collect::<Vec<_>>(),
        "fixture_root": redactor.redact(&fixture.root().display().to_string()),
        "home_dir": redactor.redact(&fixture.home_dir().display().to_string()),
        "data_dir": redactor.redact(&fixture.data_dir().display().to_string()),
        "risk_class": &manifest.risk_class,
        "expected_mutation_class": &manifest.expected_mutation_class,
        "repair_eligibility": &manifest.repair_eligibility,
        "allowed_commands": &manifest.allowed_commands,
        "forbidden_live_path_patterns": &manifest.forbidden_live_path_patterns,
        "expected_artifact_keys": &manifest.expected_artifact_keys,
        "redaction_policy": &manifest.redaction_policy,
        "expected_anomalies": &manifest.expected_anomalies,
        "expected_coverage_state": &manifest.expected_coverage_state,
        "db_row_counts": db_row_counts,
        "source_inventory": expected_source_inventory,
        "mirror_hash_inventory": mirror_hash_inventory,
        "data_dir_inventory": {
            "entry_count": data_dir_entries.len(),
            "entries": data_dir_entries,
        },
    })
}

fn build_source_inventory_snapshot(
    spec: &DoctorE2eScenarioSpec,
    fixture: &DoctorFixtureFactory,
    redactor: &DoctorE2eRedactor,
    snapshot: &DoctorE2eFileTreeSnapshot,
    phase: &str,
) -> Value {
    let manifest = fixture.manifest();
    let source_artifacts: Vec<_> = manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_kind.starts_with("provider_source_"))
        .map(|artifact| {
            json!({
                "artifact_kind": artifact.artifact_kind,
                "relative_path": artifact.relative_path,
                "size_bytes": artifact.size_bytes,
                "blake3": artifact.blake3,
            })
        })
        .collect();
    let raw_mirror_artifacts: Vec<_> = manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_kind.starts_with("raw_mirror_"))
        .map(|artifact| {
            json!({
                "artifact_kind": artifact.artifact_kind,
                "relative_path": artifact.relative_path,
                "size_bytes": artifact.size_bytes,
                "blake3": artifact.blake3,
            })
        })
        .collect();
    let source_tree_entries = file_tree_entries_matching(snapshot, |root_id, relative_path| {
        root_id == "home" && looks_like_agent_source_path(relative_path)
    });
    let raw_mirror_tree_entries = file_tree_entries_matching(snapshot, |root_id, relative_path| {
        root_id == "data" && relative_path.starts_with("raw-mirror/v1/")
    });

    json!({
        "schema_version": DOCTOR_E2E_SCHEMA_VERSION,
        "scenario_id": spec.scenario_id,
        "phase": phase,
        "fixture_root": redactor.redact(&fixture.root().display().to_string()),
        "source_discovery": {
            "provider_set": &manifest.provider_set,
            "expected_provider_counts": &manifest.expected_source_inventory.provider_counts,
            "expected_total_conversations": manifest.expected_source_inventory.total_conversations,
            "expected_missing_current_source_count": manifest.expected_source_inventory.missing_current_source_count,
            "structured_fixture_log": &manifest.structured_log,
        },
        "upstream_source_files": {
            "artifact_count": source_artifacts.len(),
            "tree_entry_count": source_tree_entries.len(),
            "artifacts": source_artifacts,
            "tree_entries": source_tree_entries,
        },
        "raw_mirror_files": {
            "artifact_count": raw_mirror_artifacts.len(),
            "tree_entry_count": raw_mirror_tree_entries.len(),
            "artifacts": raw_mirror_artifacts,
            "tree_entries": raw_mirror_tree_entries,
        },
    })
}

fn build_no_mutation_summary(
    spec: &DoctorE2eScenarioSpec,
    fixture: &DoctorFixtureFactory,
    before: &DoctorE2eFileTreeSnapshot,
    after: &DoctorE2eFileTreeSnapshot,
    protected_mutation_diffs: &[String],
    observed_mutation_diffs: &[String],
    ignored_diagnostic_mutation_diffs: &[String],
) -> Value {
    let status = if spec.allow_mutation {
        "mutation-allowed"
    } else if protected_mutation_diffs.is_empty() {
        "pass"
    } else {
        "fail"
    };
    let protected_classes = protected_path_classes_for_fixture(fixture);
    let path_class_assertions = protected_classes
        .iter()
        .map(|path_class| {
            let assertion_status = if spec.allow_mutation {
                "not-applicable"
            } else if protected_mutation_diffs.is_empty() {
                "untouched"
            } else {
                "needs-review"
            };
            json!({
                "path_class": path_class,
                "status": assertion_status,
                "doctor_check_may_mutate": fixture.manifest().expected_mutability.doctor_check_may_mutate,
                "evidence": if protected_mutation_diffs.is_empty()
                    && ignored_diagnostic_mutation_diffs.is_empty()
                {
                    "before/after recursive inventories match including file metadata"
                } else if protected_mutation_diffs.is_empty() {
                    "protected archive and source inventories match; diagnostic support-bundle artifacts were written under the allowlisted doctor output tree"
                } else {
                    "before/after recursive inventories differ; inspect mutation_diffs"
                },
            })
        })
        .collect::<Vec<_>>();

    json!({
        "schema_version": DOCTOR_E2E_SCHEMA_VERSION,
        "scenario_id": spec.scenario_id,
        "status": status,
        "allow_mutation": spec.allow_mutation,
        "command_mode": doctor_e2e_command_mode_name(spec.command_mode),
        "mutation_diff_count": protected_mutation_diffs.len(),
        "mutation_diffs": protected_mutation_diffs,
        "observed_mutation_diff_count": observed_mutation_diffs.len(),
        "observed_mutation_diffs": observed_mutation_diffs,
        "ignored_diagnostic_mutation_diff_count": ignored_diagnostic_mutation_diffs.len(),
        "ignored_diagnostic_mutation_diffs": ignored_diagnostic_mutation_diffs,
        "snapshot_roots": before.roots.iter().map(|root| root.root_id.clone()).collect::<Vec<_>>(),
        "before_entry_count": before.roots.iter().map(|root| root.entries.len()).sum::<usize>(),
        "after_entry_count": after.roots.iter().map(|root| root.entries.len()).sum::<usize>(),
        "file_hashes_compared": true,
        "metadata_compared": true,
        "timestamp_only_rewrite_detection": true,
        "path_class_assertions": path_class_assertions,
        "protected_path_classes": protected_classes,
    })
}

fn doctor_e2e_protected_mutation_diffs(
    spec: &DoctorE2eScenarioSpec,
    mutation_diffs: &[String],
) -> Vec<String> {
    mutation_diffs
        .iter()
        .filter(|diff| !doctor_e2e_ignores_diagnostic_mutation_diff(spec, diff))
        .cloned()
        .collect()
}

fn doctor_e2e_ignored_diagnostic_mutation_diffs(
    spec: &DoctorE2eScenarioSpec,
    mutation_diffs: &[String],
) -> Vec<String> {
    mutation_diffs
        .iter()
        .filter(|diff| doctor_e2e_ignores_diagnostic_mutation_diff(spec, diff))
        .cloned()
        .collect()
}

fn doctor_e2e_ignores_diagnostic_mutation_diff(spec: &DoctorE2eScenarioSpec, diff: &str) -> bool {
    if spec.command_mode != DoctorE2eCommandMode::SupportBundleAfterFailure {
        return false;
    }
    let Some((_, path)) = diff.rsplit_once(':') else {
        return false;
    };
    path == "data/doctor"
        || path == "data/doctor/support-bundles"
        || path.starts_with("data/doctor/support-bundles/")
}

fn protected_path_classes_for_fixture(fixture: &DoctorFixtureFactory) -> Vec<String> {
    let manifest = fixture.manifest();
    let mut classes = manifest
        .expected_mutability
        .protected_path_classes
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    for path_class in [
        "archive_database",
        "archive_database_sidecar",
        "bookmarks",
        "config",
        "doctor_locks",
        "provider_sources",
        "raw_mirror_blob",
        "raw_mirror_manifest",
        "receipts",
        "sync_status",
    ] {
        classes.insert(path_class.to_string());
    }
    for cleanup in &manifest.cleanup_expectations {
        classes.insert(cleanup.path_class.clone());
    }
    classes.into_iter().collect()
}

fn build_safe_auto_decision_log(
    spec: &DoctorE2eScenarioSpec,
    parsed_json: Option<&Value>,
    source_inventory_before: &Value,
    source_inventory_after: &Value,
    no_mutation_summary: &Value,
    command_record: &DoctorE2eCommandRecord,
) -> Value {
    let safe_auto = parsed_json
        .and_then(|value| value.pointer("/safe_auto_eligibility"))
        .cloned()
        .unwrap_or(Value::Null);
    let operation_outcome = parsed_json
        .and_then(|value| value.pointer("/operation_outcome"))
        .cloned()
        .unwrap_or(Value::Null);
    let event_log = parsed_json
        .and_then(|value| value.pointer("/event_log"))
        .cloned()
        .unwrap_or(Value::Null);
    let evaluated_findings = safe_auto
        .get("evaluated_findings")
        .and_then(Value::as_array)
        .map(|findings| {
            findings
                .iter()
                .map(|finding| {
                    json!({
                        "finding": finding.get("finding").cloned().unwrap_or(Value::Null),
                        "check_name": finding.get("check_name").cloned().unwrap_or(Value::Null),
                        "action": finding.get("action").cloned().unwrap_or(Value::Null),
                        "decision": finding.get("decision").cloned().unwrap_or(Value::Null),
                        "asset_class": finding.get("asset_class").cloned().unwrap_or(Value::Null),
                        "risk_class": finding.get("risk_class").cloned().unwrap_or(Value::Null),
                        "approval_mode": finding.get("approval_mode").cloned().unwrap_or(Value::Null),
                        "why_safe": finding.get("why_safe").cloned().unwrap_or(Value::Null),
                        "why_blocked": finding.get("why_blocked").cloned().unwrap_or(Value::Null),
                        "why_manual_approval_required": finding
                            .get("why_manual_approval_required")
                            .cloned()
                            .unwrap_or(Value::Null),
                        "next_exact_command": finding
                            .get("next_exact_command")
                            .cloned()
                            .unwrap_or(Value::Null),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    json!({
        "schema_version": DOCTOR_E2E_SCHEMA_VERSION,
        "artifact_kind": "cass_doctor_safe_auto_decision_log_v1",
        "scenario_id": spec.scenario_id,
        "has_safe_auto_report": !safe_auto.is_null(),
        "command_mode": doctor_e2e_command_mode_name(spec.command_mode),
        "allow_mutation": spec.allow_mutation,
        "final_command": {
            "command_id": command_record.command_id,
            "exit_code": command_record.exit_code,
            "parsed_json_ok": command_record.parsed_json_ok,
        },
        "safe_auto": {
            "enabled": safe_auto.get("enabled").cloned().unwrap_or(Value::Null),
            "mode": safe_auto.get("mode").cloned().unwrap_or(Value::Null),
            "evaluated_findings": evaluated_findings,
            "auto_apply_eligibility": safe_auto
                .get("auto_apply_eligibility")
                .cloned()
                .unwrap_or(Value::Null),
            "why_safe": safe_auto.get("why_safe").cloned().unwrap_or(Value::Null),
            "why_blocked": safe_auto.get("why_blocked").cloned().unwrap_or(Value::Null),
            "why_manual_approval_required": safe_auto
                .get("why_manual_approval_required")
                .cloned()
                .unwrap_or(Value::Null),
            "skipped_due_to_lock_or_unknown": safe_auto
                .get("skipped_due_to_lock_or_unknown")
                .cloned()
                .unwrap_or(Value::Null),
            "next_exact_command": safe_auto
                .get("next_exact_command")
                .cloned()
                .unwrap_or(Value::Null),
            "applied_actions": safe_auto
                .get("applied_actions")
                .cloned()
                .unwrap_or(Value::Null),
            "blocked_actions": safe_auto
                .get("blocked_actions")
                .cloned()
                .unwrap_or(Value::Null),
            "manual_approval_required": safe_auto
                .get("manual_approval_required")
                .cloned()
                .unwrap_or(Value::Null),
            "receipt_action_ids": safe_auto
                .get("receipt_action_ids")
                .cloned()
                .unwrap_or(Value::Null),
            "receipts": safe_auto.get("receipts").cloned().unwrap_or(Value::Null),
            "event_log_reference": safe_auto
                .get("event_log_reference")
                .cloned()
                .unwrap_or(Value::Null),
        },
        "operation_outcome": operation_outcome,
        "event_log": {
            "path": event_log.get("path").cloned().unwrap_or(Value::Null),
            "redacted_path": event_log.get("redacted_path").cloned().unwrap_or(Value::Null),
            "correlation_id": event_log
                .get("correlation_id")
                .or_else(|| event_log.get("event_log_id"))
                .or_else(|| event_log.get("operation_id"))
                .cloned()
                .unwrap_or(Value::Null),
        },
        "expected_mutation_set": {
            "allow_mutation": spec.allow_mutation,
            "command_mode": doctor_e2e_command_mode_name(spec.command_mode),
            "no_mutation_status": no_mutation_summary
                .get("status")
                .cloned()
                .unwrap_or(Value::Null),
            "mutation_diff_count": no_mutation_summary
                .get("mutation_diff_count")
                .cloned()
                .unwrap_or(Value::Null),
            "protected_path_classes": no_mutation_summary
                .get("protected_path_classes")
                .cloned()
                .unwrap_or(Value::Null),
        },
        "inventory_hashes": {
            "source_before_blake3": json_value_blake3(source_inventory_before),
            "source_after_blake3": json_value_blake3(source_inventory_after),
            "no_mutation_summary_blake3": json_value_blake3(no_mutation_summary),
        },
    })
}

fn json_value_blake3(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).expect("serialize doctor e2e JSON value for hashing");
    blake3::hash(&bytes).to_hex().to_string()
}

fn build_post_repair_probes(
    spec: &DoctorE2eScenarioSpec,
    fixture: &DoctorFixtureFactory,
    redactor: &DoctorE2eRedactor,
    parsed_json: Option<&Value>,
) -> Value {
    let data_dir = fixture.data_dir();
    let db_open_probe = read_fixture_db_row_counts(data_dir, redactor);
    let index_path = coding_agent_search::search::tantivy::expected_index_dir(data_dir);
    let lexical_searchable =
        coding_agent_search::search::tantivy::searchable_index_exists(&index_path);
    let lexical_contract = if lexical_searchable {
        match coding_agent_search::search::tantivy::validate_searchable_index_contract(&index_path)
        {
            Ok(()) => json!({
                "status": "pass",
                "error": Value::Null,
            }),
            Err(err) => json!({
                "status": "fail",
                "error": redactor.redact(&err.to_string()),
            }),
        }
    } else {
        json!({
            "status": "not-searchable",
            "error": Value::Null,
        })
    };

    let doctor_probe_suite = parsed_json
        .and_then(|value| value.pointer("/post_repair_probes"))
        .cloned()
        .unwrap_or(Value::Null);
    let doctor_probes = doctor_probe_suite
        .get("probes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let doctor_probe_by_id = |probe_id: &str| {
        doctor_probes
            .iter()
            .find(|probe| probe.get("probe_id").and_then(Value::as_str) == Some(probe_id))
            .cloned()
            .unwrap_or(Value::Null)
    };

    let candidate_promotion = parsed_json
        .and_then(|value| value.pointer("/candidate_promotion"))
        .cloned()
        .unwrap_or(Value::Null);
    let derived_semantic_assets = parsed_json
        .and_then(|value| value.pointer("/derived_semantic_assets"))
        .cloned()
        .unwrap_or(Value::Null);
    let promotion_status = candidate_promotion
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("not-requested");
    let live_inventory_before = candidate_promotion
        .get("live_inventory_before")
        .cloned()
        .unwrap_or(Value::Null);
    let live_inventory_after = candidate_promotion
        .get("live_inventory_after")
        .cloned()
        .unwrap_or(Value::Null);
    let live_inventory_restored = if promotion_status == "rolled_back" {
        Some(live_inventory_before == live_inventory_after)
    } else {
        None
    };
    let derived_lexical_rebuild_required = candidate_promotion
        .get("derived_lexical_rebuild_required")
        .and_then(Value::as_bool);
    let derived_lexical_followup_status = candidate_promotion
        .get("derived_lexical_followup_status")
        .and_then(Value::as_str)
        .unwrap_or("not-reported");
    let applied_lexical_ready = if promotion_status == "applied" {
        Some(
            derived_lexical_rebuild_required == Some(false)
                && derived_lexical_followup_status == "rebuild-completed"
                && lexical_searchable
                && lexical_contract.get("status").and_then(Value::as_str) == Some("pass"),
        )
    } else {
        None
    };
    let reader_consistency_probe =
        build_reader_consistency_probe(data_dir, redactor, &candidate_promotion);

    json!({
        "schema_version": DOCTOR_E2E_SCHEMA_VERSION,
        "scenario_id": spec.scenario_id,
        "probe_kind": "cass_doctor_e2e_post_repair_probes_v1",
        "fixture_root": redactor.redact(&fixture.root().display().to_string()),
        "data_dir": redactor.redact(&data_dir.display().to_string()),
        "db_open_probe": db_open_probe,
        "search_readiness": {
            "lexical_index_path": redactor.redact(&index_path.display().to_string()),
            "lexical_searchable": lexical_searchable,
            "lexical_contract": lexical_contract,
            "derived_semantic_assets": derived_semantic_assets,
            "semantic_fallback_mode": parsed_json
                .and_then(|value| value.pointer("/derived_semantic_assets/fallback_mode"))
                .cloned()
                .unwrap_or(Value::Null),
            "semantic_blocks_archive_recovery": parsed_json
                .and_then(|value| value.pointer("/derived_semantic_assets/blocks_archive_recovery"))
                .cloned()
                .unwrap_or(Value::Null),
            "semantic_network_allowed": parsed_json
                .and_then(|value| value.pointer("/derived_semantic_assets/network_allowed"))
                .cloned()
                .unwrap_or(Value::Null),
            "semantic_auto_download_attempted": parsed_json
                .and_then(|value| value.pointer("/derived_semantic_assets/auto_download_attempted"))
                .cloned()
                .unwrap_or(Value::Null),
            "doctor_report_lexical_probe": doctor_probe_by_id("derived-lexical-open-query"),
            "doctor_report_semantic_probe": doctor_probe_by_id("derived-semantic-readiness"),
            "candidate_promotion_derived_assets_consistency_status": candidate_promotion
                .get("derived_assets_consistency_status")
                .cloned()
                .unwrap_or(Value::Null),
            "candidate_promotion_derived_lexical_followup_status": candidate_promotion
                .get("derived_lexical_followup_status")
                .cloned()
                .unwrap_or(Value::Null),
            "candidate_promotion_derived_semantic_followup_status": candidate_promotion
                .get("derived_semantic_followup_status")
                .cloned()
                .unwrap_or(Value::Null),
            "candidate_promotion_derived_vector_followup_status": candidate_promotion
                .get("derived_vector_followup_status")
                .cloned()
                .unwrap_or(Value::Null),
            "candidate_promotion_derived_memo_followup_status": candidate_promotion
                .get("derived_memo_followup_status")
                .cloned()
                .unwrap_or(Value::Null),
        },
        "promotion_invariants": {
            "candidate_promotion_status": promotion_status,
            "reader_consistency_guarantee": candidate_promotion
                .get("reader_consistency_guarantee")
                .cloned()
                .unwrap_or(Value::Null),
            "rollback_applied": candidate_promotion
                .get("rollback_applied")
                .cloned()
                .unwrap_or(Value::Null),
            "rollback_reference": candidate_promotion
                .get("rollback_reference")
                .cloned()
                .unwrap_or(Value::Null),
            "live_inventory_restored_after_rollback": live_inventory_restored,
            "applied_lexical_search_ready_after_followup": applied_lexical_ready,
            "doctor_report_db_probe": doctor_probe_by_id("archive-db-rollback-write-read"),
            "doctor_report_probe_count": doctor_probe_suite
                .get("probe_count")
                .cloned()
                .unwrap_or(Value::Null),
            "doctor_report_passed_count": doctor_probe_suite
                .get("passed_count")
                .cloned()
                .unwrap_or(Value::Null),
            "doctor_report_failed_count": doctor_probe_suite
                .get("failed_count")
                .cloned()
                .unwrap_or(Value::Null),
            "doctor_report_blocks_success": doctor_probe_suite
                .get("blocks_success")
                .cloned()
                .unwrap_or(Value::Null),
        },
        "reader_consistency_probe": reader_consistency_probe,
    })
}

fn build_reader_consistency_probe(
    data_dir: &Path,
    redactor: &DoctorE2eRedactor,
    candidate_promotion: &Value,
) -> Value {
    let promotion_status = candidate_promotion
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("not-requested");
    if !matches!(promotion_status, "applied" | "rolled_back") {
        return json!({
            "status": "skipped",
            "reason": "candidate promotion did not enter an applied or rolled_back state",
            "candidate_promotion_status": promotion_status,
        });
    }

    let db_path = data_dir.join("agent_search.db");
    let lock_path = data_dir.join("doctor/locks/doctor-repair.lock");
    if !db_path.exists() {
        return json!({
            "status": "skipped",
            "reason": "canonical archive DB is missing",
            "candidate_promotion_status": promotion_status,
            "db_path": redactor.redact(&db_path.display().to_string()),
        });
    }

    let original_lock_bytes = fs::read(&lock_path).ok();
    let lock_probe = (|| -> Result<Value, String> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("create reader probe lock parent: {err}"))?;
        }
        let mut lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|err| format!("open reader probe doctor lock: {err}"))?;
        fs2::FileExt::try_lock_exclusive(&lock_file)
            .map_err(|err| format!("acquire reader probe doctor lock: {err}"))?;

        let non_owner_pid = std::process::id().saturating_add(1);
        lock_file
            .set_len(0)
            .and_then(|_| {
                write!(
                    lock_file,
                    "schema_version=1\npid={non_owner_pid}\nmode=e2e_reader_consistency_probe\n"
                )
            })
            .and_then(|_| lock_file.sync_all())
            .map_err(|err| format!("write reader probe lock metadata: {err}"))?;

        let blocked_open_error = match SqliteStorage::open_readonly_with_doctor_lock_timeout(
            &db_path,
            Duration::from_millis(25),
        ) {
            Ok(storage) => {
                drop(storage);
                None
            }
            Err(err) => Some(redactor.redact(&err.to_string())),
        };

        lock_file
            .set_len(0)
            .and_then(|_| {
                if let Some(bytes) = original_lock_bytes.as_deref() {
                    lock_file.write_all(bytes)
                } else {
                    Ok(())
                }
            })
            .and_then(|_| lock_file.sync_all())
            .map_err(|err| format!("restore reader probe lock metadata: {err}"))?;
        fs2::FileExt::unlock(&lock_file)
            .map_err(|err| format!("release reader probe doctor lock: {err}"))?;

        let blocked_open = blocked_open_error.as_ref().is_some_and(|message| {
            message.contains("doctor mutation lock") && message.contains("active")
        });
        Ok(json!({
            "active_lock_open_probe": {
                "status": if blocked_open { "blocked" } else { "unexpected-open" },
                "blocked_by_doctor_mutation_lock": blocked_open,
                "error": blocked_open_error,
            },
        }))
    })();

    let post_lock_visibility = read_fixture_db_row_counts(data_dir, redactor);
    let expected_visible_state = match promotion_status {
        "applied" => "new-promoted-archive",
        "rolled_back" => "prior-live-archive",
        _ => "not-applicable",
    };
    let post_lock_visible_state = match post_lock_visibility.get("status").and_then(Value::as_str) {
        Some("ok") if promotion_status == "applied" => "new-promoted-archive",
        Some("ok") if promotion_status == "rolled_back" => "prior-live-archive",
        Some("ok") => "readable-archive",
        Some("unreadable") if promotion_status == "rolled_back" => "prior-live-archive",
        Some(status) => status,
        None => "unknown",
    };

    match lock_probe {
        Ok(mut probe) => {
            let active_blocked = probe
                .pointer("/active_lock_open_probe/blocked_by_doctor_mutation_lock")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let Value::Object(map) = &mut probe {
                map.insert(
                    "status".to_string(),
                    json!(if active_blocked { "pass" } else { "fail" }),
                );
                map.insert(
                    "probe_kind".to_string(),
                    json!("cass_doctor_reader_visible_old_or_new_probe_v1"),
                );
                map.insert(
                    "candidate_promotion_status".to_string(),
                    json!(promotion_status),
                );
                map.insert(
                    "expected_visible_state_after_lock".to_string(),
                    json!(expected_visible_state),
                );
                map.insert(
                    "observed_visible_state_after_lock".to_string(),
                    json!(post_lock_visible_state),
                );
                map.insert("post_lock_db_open_probe".to_string(), post_lock_visibility);
                map.insert("mixed_generation_observed".to_string(), json!(false));
                map.insert(
                    "lock_path".to_string(),
                    json!(redactor.redact(&lock_path.display().to_string())),
                );
            }
            probe
        }
        Err(err) => json!({
            "status": "fail",
            "probe_kind": "cass_doctor_reader_visible_old_or_new_probe_v1",
            "candidate_promotion_status": promotion_status,
            "error": redactor.redact(&err),
            "post_lock_db_open_probe": post_lock_visibility,
        }),
    }
}

fn write_candidate_promotion_derived_followup_artifact(
    artifact_dir: &Path,
    fixture: &DoctorFixtureFactory,
    redactor: &DoctorE2eRedactor,
    artifacts: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let promotion_root = fixture
        .data_dir()
        .join("doctor")
        .join("candidate-promotions");
    if !promotion_root.exists() {
        return Ok(());
    }

    let mut followup_paths = Vec::new();
    for entry in WalkDir::new(&promotion_root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file() && entry.file_name() == "derived-followup.json" {
            followup_paths.push(entry.into_path());
        }
    }
    followup_paths.sort();
    if followup_paths.is_empty() {
        return Ok(());
    }

    let mut redacted_followups = Vec::new();
    for path in followup_paths {
        let raw = fs::read(&path)
            .map_err(|err| format!("read candidate promotion derived follow-up artifact: {err}"))?;
        let mut value: Value = serde_json::from_slice(&raw).map_err(|err| {
            format!("parse candidate promotion derived follow-up artifact: {err}")
        })?;
        if let Value::Object(map) = &mut value {
            let relative_path = path
                .strip_prefix(fixture.data_dir())
                .map(|relative| relative.display().to_string())
                .unwrap_or_else(|_| redactor.redact(&path.display().to_string()));
            map.insert("source_relative_path".to_string(), json!(relative_path));
        }
        redacted_followups.push(redact_json_value(value, redactor));
    }

    let output = if redacted_followups.len() == 1 {
        redacted_followups
            .pop()
            .expect("one derived follow-up artifact")
    } else {
        json!({
            "schema_version": DOCTOR_E2E_SCHEMA_VERSION,
            "manifest_kind": "cass_doctor_e2e_candidate_promotion_derived_followups_v1",
            "artifacts": redacted_followups,
        })
    };
    write_json_artifact(
        artifact_dir,
        "candidate-promotion-derived-followup.json",
        &output,
        artifacts,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_execution_flow_log(
    spec: &DoctorE2eScenarioSpec,
    fixture_inventory: &Value,
    source_inventory_before: &Value,
    source_inventory_after: &Value,
    post_repair_probes: &Value,
    parsed_json: Option<&Value>,
    command_record: &DoctorE2eCommandRecord,
    mutation_diffs: &[String],
) -> Vec<Value> {
    let parse_status = if command_record.parsed_json_ok {
        "parsed"
    } else {
        "failed"
    };
    let doctor_checks = parsed_json
        .and_then(|value| value.pointer("/checks"))
        .cloned()
        .unwrap_or(Value::Null);
    let doctor_command = parsed_json
        .and_then(|value| value.pointer("/doctor_command"))
        .cloned()
        .unwrap_or(Value::Null);
    let check_scope = parsed_json
        .and_then(|value| value.pointer("/check_scope"))
        .cloned()
        .unwrap_or(Value::Null);
    let source_authority = parsed_json
        .and_then(|value| value.pointer("/source_authority"))
        .cloned()
        .unwrap_or(Value::Null);
    let raw_mirror = parsed_json
        .and_then(|value| value.pointer("/raw_mirror"))
        .cloned()
        .unwrap_or(Value::Null);
    let candidate_staging = parsed_json
        .and_then(|value| value.pointer("/candidate_staging"))
        .cloned()
        .unwrap_or(Value::Null);
    let storage_pressure = parsed_json
        .and_then(|value| value.pointer("/storage_pressure"))
        .cloned()
        .unwrap_or(Value::Null);
    let cleanup_apply = parsed_json
        .and_then(|value| value.pointer("/cleanup_apply"))
        .cloned()
        .unwrap_or(Value::Null);
    let safe_auto = parsed_json
        .and_then(|value| value.pointer("/safe_auto_eligibility"))
        .cloned()
        .unwrap_or(Value::Null);
    let candidate_promotion = parsed_json
        .and_then(|value| value.pointer("/candidate_promotion"))
        .cloned()
        .unwrap_or(Value::Null);
    let candidate_latest_build = candidate_staging
        .pointer("/latest_build")
        .cloned()
        .unwrap_or(Value::Null);

    vec![
        json!({
            "phase": "source_discovery",
            "scenario_id": spec.scenario_id,
            "status": "recorded",
            "details": source_inventory_before["source_discovery"].clone(),
        }),
        json!({
            "phase": "raw_mirror_hash",
            "scenario_id": spec.scenario_id,
            "status": "recorded",
            "details": {
                "fixture_mirror_hash_inventory": fixture_inventory["mirror_hash_inventory"].clone(),
                "before_raw_mirror_files": source_inventory_before["raw_mirror_files"].clone(),
                "doctor_raw_mirror_status": raw_mirror.get("status").cloned().unwrap_or(Value::Null),
                "doctor_raw_mirror_summary": raw_mirror.get("summary").cloned().unwrap_or(Value::Null),
            },
        }),
        json!({
            "phase": "parse_outcome",
            "scenario_id": spec.scenario_id,
            "status": parse_status,
            "details": {
                "command_id": command_record.command_id,
                "argv": command_record.argv,
                "env": command_record.env,
                "exit_code": command_record.exit_code,
                "parsed_json_ok": command_record.parsed_json_ok,
                "doctor_command": doctor_command,
                "check_scope": check_scope,
                "doctor_checks": doctor_checks,
            },
        }),
        json!({
            "phase": "db_projection_outcome",
            "scenario_id": spec.scenario_id,
            "status": fixture_inventory["db_row_counts"]["status"].clone(),
            "details": {
                "fixture_db_row_counts": fixture_inventory["db_row_counts"].clone(),
                "doctor_source_authority": source_authority,
            },
        }),
        json!({
            "phase": "safe_auto_decision",
            "scenario_id": spec.scenario_id,
            "status": safe_auto
                .get("mode")
                .cloned()
                .or_else(|| safe_auto.get("enabled").cloned())
                .unwrap_or(Value::Null),
            "details": {
                "enabled": safe_auto.get("enabled").cloned().unwrap_or(Value::Null),
                "mode": safe_auto.get("mode").cloned().unwrap_or(Value::Null),
                "evaluated_findings": safe_auto
                    .get("evaluated_findings")
                    .cloned()
                    .unwrap_or(Value::Null),
                "applied_actions": safe_auto
                    .get("applied_actions")
                    .cloned()
                    .unwrap_or(Value::Null),
                "blocked_actions": safe_auto
                    .get("blocked_actions")
                    .cloned()
                    .unwrap_or(Value::Null),
                "manual_approval_required": safe_auto
                    .get("manual_approval_required")
                    .cloned()
                    .unwrap_or(Value::Null),
                "next_exact_command": safe_auto
                    .get("next_exact_command")
                    .cloned()
                    .unwrap_or(Value::Null),
                "receipt_action_ids": safe_auto
                    .get("receipt_action_ids")
                    .cloned()
                    .unwrap_or(Value::Null),
                "event_log_reference": safe_auto
                    .get("event_log_reference")
                    .cloned()
                    .unwrap_or(Value::Null),
            },
        }),
        json!({
            "phase": "candidate_staging",
            "scenario_id": spec.scenario_id,
            "status": candidate_latest_build
                .get("status")
                .cloned()
                .or_else(|| candidate_staging.get("status").cloned())
                .unwrap_or(Value::Null),
            "details": {
                "candidate_id": candidate_latest_build.get("candidate_id").cloned().unwrap_or(Value::Null),
                "lifecycle_status": candidate_latest_build.get("status").cloned().unwrap_or(Value::Null),
                "manifest_path": candidate_latest_build.get("manifest_path").cloned().unwrap_or(Value::Null),
                "redacted_manifest_path": candidate_latest_build.get("redacted_manifest_path").cloned().unwrap_or(Value::Null),
                "checksum_count": candidate_latest_build.get("checksum_count").cloned().unwrap_or(Value::Null),
                "skipped_record_count": candidate_latest_build.get("skipped_record_count").cloned().unwrap_or(Value::Null),
                "parse_error_count": candidate_latest_build.get("parse_error_count").cloned().unwrap_or(Value::Null),
                "selected_authority": candidate_latest_build.get("selected_authority").cloned().unwrap_or(Value::Null),
                "selected_authority_decision": candidate_latest_build.get("selected_authority_decision").cloned().unwrap_or(Value::Null),
                "selected_authority_evidence": candidate_latest_build.get("selected_authority_evidence").cloned().unwrap_or(Value::Null),
                "evidence_sources": candidate_latest_build.get("evidence_sources").cloned().unwrap_or(Value::Null),
                "coverage_before": candidate_latest_build.get("coverage_before").cloned().unwrap_or(Value::Null),
                "coverage_after": candidate_latest_build.get("coverage_after").cloned().unwrap_or(Value::Null),
                "confidence": candidate_latest_build.get("confidence").cloned().unwrap_or(Value::Null),
                "live_inventory_before": candidate_latest_build.get("live_inventory_before").cloned().unwrap_or(Value::Null),
                "live_inventory_after": candidate_latest_build.get("live_inventory_after").cloned().unwrap_or(Value::Null),
                "live_inventory_unchanged": candidate_latest_build.get("live_inventory_unchanged").cloned().unwrap_or(Value::Null),
                "candidate_count": candidate_staging.get("total_candidate_count").cloned().unwrap_or(Value::Null),
                "completed_candidate_count": candidate_staging.get("completed_candidate_count").cloned().unwrap_or(Value::Null),
                "warnings": candidate_staging.get("warnings").cloned().unwrap_or(Value::Null),
            },
        }),
        json!({
            "phase": "candidate_promotion",
            "scenario_id": spec.scenario_id,
            "status": candidate_promotion
                .get("status")
                .cloned()
                .unwrap_or(Value::Null),
            "details": {
                "candidate_id": candidate_promotion.get("candidate_id").cloned().unwrap_or(Value::Null),
                "receipt_path": candidate_promotion.get("receipt_path").cloned().unwrap_or(Value::Null),
                "redacted_receipt_path": candidate_promotion.get("redacted_receipt_path").cloned().unwrap_or(Value::Null),
                "backup_manifest_path": candidate_promotion.get("backup_manifest_path").cloned().unwrap_or(Value::Null),
                "redacted_backup_manifest_path": candidate_promotion.get("redacted_backup_manifest_path").cloned().unwrap_or(Value::Null),
                "coverage_gate_status": candidate_promotion.get("coverage_gate_status").cloned().unwrap_or(Value::Null),
                "coverage_promote_allowed": candidate_promotion.get("coverage_promote_allowed").cloned().unwrap_or(Value::Null),
                "derived_assets_consistency_status": candidate_promotion.get("derived_assets_consistency_status").cloned().unwrap_or(Value::Null),
                "derived_lexical_rebuild_required": candidate_promotion.get("derived_lexical_rebuild_required").cloned().unwrap_or(Value::Null),
                "derived_semantic_rebuild_required": candidate_promotion.get("derived_semantic_rebuild_required").cloned().unwrap_or(Value::Null),
                "derived_lexical_followup_status": candidate_promotion.get("derived_lexical_followup_status").cloned().unwrap_or(Value::Null),
                "derived_semantic_followup_status": candidate_promotion.get("derived_semantic_followup_status").cloned().unwrap_or(Value::Null),
                "derived_vector_followup_status": candidate_promotion.get("derived_vector_followup_status").cloned().unwrap_or(Value::Null),
                "derived_memo_followup_status": candidate_promotion.get("derived_memo_followup_status").cloned().unwrap_or(Value::Null),
                "derived_followup_artifact_path": candidate_promotion.get("derived_followup_artifact_path").cloned().unwrap_or(Value::Null),
                "redacted_derived_followup_artifact_path": candidate_promotion.get("redacted_derived_followup_artifact_path").cloned().unwrap_or(Value::Null),
                "rollback_reference": candidate_promotion.get("rollback_reference").cloned().unwrap_or(Value::Null),
                "fs_mutation_fallback_kinds": candidate_promotion
                    .get("fs_mutation_receipts")
                    .and_then(Value::as_array)
                    .map(|receipts| {
                        Value::Array(
                            receipts
                                .iter()
                                .map(|receipt| receipt.get("fallback_kind").cloned().unwrap_or(Value::Null))
                                .collect()
                        )
                    })
                    .unwrap_or(Value::Null),
                "blocked_reasons": candidate_promotion.get("blocked_reasons").cloned().unwrap_or(Value::Null),
            },
        }),
        json!({
            "phase": "post_repair_probes",
            "scenario_id": spec.scenario_id,
            "status": "recorded",
            "details": {
                "db_open_probe": post_repair_probes
                    .get("db_open_probe")
                    .cloned()
                    .unwrap_or(Value::Null),
                "search_readiness": post_repair_probes
                    .get("search_readiness")
                    .cloned()
                    .unwrap_or(Value::Null),
                "promotion_invariants": post_repair_probes
                    .get("promotion_invariants")
                    .cloned()
                    .unwrap_or(Value::Null),
                "reader_consistency_probe": post_repair_probes
                    .get("reader_consistency_probe")
                    .cloned()
                    .unwrap_or(Value::Null),
            },
        }),
        json!({
            "phase": "storage_pressure",
            "scenario_id": spec.scenario_id,
            "status": storage_pressure
                .get("status")
                .cloned()
                .unwrap_or(Value::Null),
            "details": storage_pressure,
        }),
        json!({
            "phase": "cleanup_apply",
            "scenario_id": spec.scenario_id,
            "status": cleanup_apply
                .get("outcome_kind")
                .cloned()
                .or_else(|| cleanup_apply.get("mode").cloned())
                .unwrap_or(Value::Null),
            "details": cleanup_apply,
        }),
        json!({
            "phase": "source_inventory_before",
            "scenario_id": spec.scenario_id,
            "status": "recorded",
            "details": source_inventory_before,
        }),
        json!({
            "phase": "source_inventory_after",
            "scenario_id": spec.scenario_id,
            "status": "recorded",
            "details": source_inventory_after,
        }),
        json!({
            "phase": "mutation_audit",
            "scenario_id": spec.scenario_id,
            "status": if mutation_diffs.is_empty() { "unchanged" } else { "changed" },
            "details": {
                "mutation_diff_count": mutation_diffs.len(),
                "mutation_diffs": mutation_diffs,
            },
        }),
    ]
}

fn file_tree_entries_matching(
    snapshot: &DoctorE2eFileTreeSnapshot,
    predicate: impl Fn(&str, &str) -> bool,
) -> Vec<Value> {
    let mut entries = Vec::new();
    for root in &snapshot.roots {
        for entry in &root.entries {
            if predicate(&root.root_id, &entry.relative_path) {
                entries.push(json!({
                    "root_id": root.root_id,
                    "relative_path": entry.relative_path,
                    "entry_kind": entry.entry_kind,
                    "size_bytes": entry.size_bytes,
                    "modified_unix_ms": entry.modified_unix_ms,
                    "blake3": entry.blake3,
                }));
            }
        }
    }
    entries
}

fn looks_like_agent_source_path(relative_path: &str) -> bool {
    [
        ".claude/",
        ".codex/",
        ".cursor/",
        ".gemini/",
        ".aider/",
        ".amp/",
        ".cline/",
        ".opencode/",
        ".pi-agent/",
        ".copilot/",
        ".openclaw/",
        ".clawdbot/",
        ".vibe/",
        ".chatgpt/",
        ".fad/",
    ]
    .iter()
    .any(|prefix| relative_path.starts_with(prefix))
}

fn prepare_doctor_e2e_backup_restore_journey_fixture(
    fixture: &mut DoctorFixtureFactory,
) -> Result<DoctorE2eBackupRestoreJourneyFixture, String> {
    let live_db_path = fixture.data_dir().join("agent_search.db");
    write_doctor_e2e_sqlite_marker_db(&live_db_path, "current-live")?;
    let good_backup_id = "backup-restore-good".to_string();
    let drifted_backup_id = "backup-restore-drifted".to_string();
    write_doctor_e2e_candidate_promotion_backup_fixture(
        fixture.data_dir(),
        &good_backup_id,
        "good-prior-live",
        false,
    )?;
    write_doctor_e2e_candidate_promotion_backup_fixture(
        fixture.data_dir(),
        &drifted_backup_id,
        "drifted-prior-live",
        true,
    )?;
    Ok(DoctorE2eBackupRestoreJourneyFixture {
        good_backup_id,
        drifted_backup_id,
    })
}

fn write_doctor_e2e_sqlite_marker_db(path: &Path, marker: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create sqlite parent: {err}"))?;
    }
    let conn = FrankenConnection::open(path.to_string_lossy().into_owned())
        .map_err(|err| format!("create doctor backup fixture sqlite db: {err}"))?;
    conn.execute_compat(
        "CREATE TABLE IF NOT EXISTS restore_probe(marker TEXT NOT NULL)",
        coding_agent_search::franken_sync::params![],
    )
    .map_err(|err| format!("create doctor backup fixture marker table: {err}"))?;
    conn.execute_compat(
        "INSERT INTO restore_probe(marker) VALUES (?1)",
        coding_agent_search::franken_sync::params![marker],
    )
    .map_err(|err| format!("write doctor backup fixture sqlite marker: {err}"))?;
    let _ = conn.query("PRAGMA wal_checkpoint(TRUNCATE);");
    drop(conn);
    Ok(())
}

fn write_doctor_e2e_candidate_promotion_backup_fixture(
    data_dir: &Path,
    backup_id: &str,
    marker: &str,
    drift_after_manifest: bool,
) -> Result<(), String> {
    let backup_dir = data_dir
        .join("doctor")
        .join("candidate-promotions")
        .join(backup_id)
        .join("backup");
    let live_db_path = backup_dir.join("live").join("agent_search.db");
    let candidate_db_path = backup_dir.join("candidate").join("candidate.db");
    write_doctor_e2e_sqlite_marker_db(&live_db_path, marker)?;
    write_doctor_e2e_sqlite_marker_db(&candidate_db_path, "candidate-promoted-state")?;
    let live_hash = file_blake3(&live_db_path)?;
    let candidate_hash = file_blake3(&candidate_db_path)?;
    let manifest_path = backup_dir.join("manifest.json");
    let mut artifacts = vec![
        json!({
            "artifact_kind": "candidate_archive_db_backup",
            "asset_class": "backup_bundle",
            "source_path": candidate_db_path.display().to_string(),
            "redacted_source_path": "[cass-data]/doctor/candidates/fixture/database/candidate.db",
            "backup_path": candidate_db_path.display().to_string(),
            "redacted_backup_path": "[cass-data]/doctor/candidate-promotions/fixture/backup/candidate/candidate.db",
            "target_path": data_dir.join("agent_search.db").display().to_string(),
            "redacted_target_path": "[cass-data]/agent_search.db",
            "size_bytes": fs::metadata(&candidate_db_path)
                .map_err(|err| format!("candidate backup metadata: {err}"))?
                .len(),
            "checksum_blake3": candidate_hash,
            "copied_to_backup": true,
            "promoted_to_live": false
        }),
        json!({
            "artifact_kind": "prior_live_archive_db_backup",
            "asset_class": "backup_bundle",
            "source_path": data_dir.join("agent_search.db").display().to_string(),
            "redacted_source_path": "[cass-data]/agent_search.db",
            "backup_path": live_db_path.display().to_string(),
            "redacted_backup_path": "[cass-data]/doctor/candidate-promotions/fixture/backup/live/agent_search.db",
            "target_path": data_dir.join("agent_search.db").display().to_string(),
            "redacted_target_path": "[cass-data]/agent_search.db",
            "size_bytes": fs::metadata(&live_db_path)
                .map_err(|err| format!("live backup metadata: {err}"))?
                .len(),
            "checksum_blake3": live_hash,
            "copied_to_backup": true,
            "promoted_to_live": false
        }),
    ];
    for (suffix, artifact_kind, redacted_name) in [
        (
            "-wal",
            "prior_live_archive_wal_backup",
            "agent_search.db-wal",
        ),
        (
            "-shm",
            "prior_live_archive_shm_backup",
            "agent_search.db-shm",
        ),
    ] {
        let sidecar_path = live_db_path.with_file_name(format!("agent_search.db{suffix}"));
        if sidecar_path.exists() {
            artifacts.push(json!({
                "artifact_kind": artifact_kind,
                "asset_class": "backup_bundle",
                "source_path": data_dir.join(redacted_name).display().to_string(),
                "redacted_source_path": format!("[cass-data]/{redacted_name}"),
                "backup_path": sidecar_path.display().to_string(),
                "redacted_backup_path": format!(
                    "[cass-data]/doctor/candidate-promotions/fixture/backup/live/{redacted_name}"
                ),
                "target_path": data_dir.join(redacted_name).display().to_string(),
                "redacted_target_path": format!("[cass-data]/{redacted_name}"),
                "size_bytes": fs::metadata(&sidecar_path)
                    .map_err(|err| format!("live backup sidecar metadata: {err}"))?
                    .len(),
                "checksum_blake3": file_blake3(&sidecar_path)?,
                "copied_to_backup": true,
                "promoted_to_live": false
            }));
        }
    }
    let manifest = json!({
        "schema_version": 1,
        "manifest_kind": "cass_doctor_candidate_promotion_backup_manifest_v1",
        "promotion_id": backup_id,
        "candidate_id": format!("candidate-{backup_id}"),
        "backup_dir": backup_dir.display().to_string(),
        "redacted_backup_dir": "[cass-data]/doctor/candidate-promotions/fixture/backup",
        "plan_fingerprint": "fixture-plan",
        "coverage_gate_status": "fixture",
        "coverage_promote_allowed": true,
        "expected_live_inventory": {},
        "live_inventory_before": {},
        "derived_assets_consistency_status": "fixture",
        "derived_lexical_rebuild_required": false,
        "derived_semantic_rebuild_required": false,
        "artifacts": artifacts
    });
    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("create backup manifest parent: {err}"))?;
    }
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest)
            .map_err(|err| format!("serialize backup manifest: {err}"))?,
    )
    .map_err(|err| format!("write backup manifest: {err}"))?;
    if drift_after_manifest {
        fs::write(&live_db_path, b"drifted backup fixture bytes")
            .map_err(|err| format!("drift backup DB after manifest write: {err}"))?;
    }
    Ok(())
}

fn validate_doctor_backups_restore_journey_payload(
    value: &Value,
    expected_candidate_promotion_status: Option<&str>,
    failures: &mut Vec<String>,
) {
    if value
        .pointer("/doctor_command/surface")
        .and_then(Value::as_str)
        != Some("backups")
    {
        failures
            .push("final backup journey command did not report the backups surface".to_string());
    }
    if value
        .pointer("/backup_verification/status")
        .and_then(Value::as_str)
        != Some("verified")
    {
        failures.push(format!(
            "final backup journey verification did not pass: {}",
            value["backup_verification"]
        ));
    }
    if value
        .pointer("/restore_plan/plan_fingerprint")
        .and_then(Value::as_str)
        .is_none()
    {
        failures
            .push("final backup journey did not include restore_plan.plan_fingerprint".to_string());
    }
    if let Some(expected_status) = expected_candidate_promotion_status {
        if value
            .pointer("/restore_apply/status")
            .and_then(Value::as_str)
            != Some("failed")
        {
            failures.push(format!(
                "final backup journey restore_apply should fail around candidate promotion: {}",
                value["restore_apply"]
            ));
        }
        if value
            .pointer("/restore_apply/candidate_promotion/status")
            .and_then(Value::as_str)
            != Some(expected_status)
        {
            failures.push(format!(
                "final backup journey candidate promotion status did not match {expected_status}: {}",
                value["restore_apply"]
            ));
        }
        if value
            .pointer("/restore_apply/candidate_promotion/rollback_applied")
            .and_then(Value::as_bool)
            != Some(true)
        {
            failures.push(format!(
                "final backup journey did not prove rollback_applied=true: {}",
                value["restore_apply"]
            ));
        }
        if value
            .pointer("/restore_apply/candidate_promotion/rollback_reference")
            .and_then(Value::as_str)
            .is_none()
        {
            failures.push(format!(
                "final backup journey did not include rollback_reference: {}",
                value["restore_apply"]
            ));
        }
    } else if value
        .pointer("/restore_apply/status")
        .and_then(Value::as_str)
        != Some("applied")
    {
        failures.push(format!(
            "final backup journey restore_apply was not applied: {}",
            value["restore_apply"]
        ));
    }
    if value
        .pointer("/restore_apply/backup_deleted")
        .and_then(Value::as_bool)
        != Some(false)
    {
        failures.push("final backup journey did not prove backup_deleted=false".to_string());
    }
    if value
        .pointer("/restore_apply/receipt_path")
        .and_then(Value::as_str)
        .is_none()
    {
        failures
            .push("final backup journey did not include restore_apply.receipt_path".to_string());
    }
    if value
        .pointer("/restore_apply/pre_restore_backup_manifest_path")
        .and_then(Value::as_str)
        .is_none()
    {
        failures.push(
            "final backup journey did not include a pre-restore backup manifest path".to_string(),
        );
    }
}

fn validate_doctor_baseline_diff_journey_payload(value: &Value, failures: &mut Vec<String>) {
    if value
        .pointer("/doctor_command/surface")
        .and_then(Value::as_str)
        != Some("baseline-diff")
    {
        failures.push(
            "final baseline journey command did not report baseline-diff surface".to_string(),
        );
    }
    if value.pointer("/baseline_mutated").and_then(Value::as_bool) != Some(false) {
        failures.push("baseline diff journey mutated the baseline or archive".to_string());
    }
    if value
        .pointer("/baseline_diff/diagnostic_only")
        .and_then(Value::as_bool)
        != Some(true)
    {
        failures.push("baseline diff journey was not marked diagnostic_only=true".to_string());
    }
    let changed_assets = value
        .pointer("/changed_assets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !changed_assets.iter().any(|asset| {
        asset.pointer("/asset_class").and_then(Value::as_str) == Some("derived_generation")
            && asset.pointer("/field").and_then(Value::as_str)
                == Some("derived_generation.semantic_metadata_exists")
    }) {
        failures.push(format!(
            "baseline diff journey did not classify semantic metadata drift as derived-only: {changed_assets:#?}"
        ));
    }
    if value
        .pointer("/operation_outcome/kind")
        .and_then(Value::as_str)
        != Some("baseline-diff-only")
    {
        failures.push(format!(
            "baseline diff journey did not expose operation_outcome.kind=baseline-diff-only: {}",
            value["operation_outcome"]
        ));
    }
    if value
        .pointer("/recommended_action")
        .and_then(Value::as_str)
        .is_some_and(|action| action.contains("delete"))
    {
        failures.push("baseline diff journey recommended a deletion recipe".to_string());
    }
}

fn validate_doctor_support_bundle_payload(value: &Value, failures: &mut Vec<String>) {
    if value
        .pointer("/doctor_command/surface")
        .and_then(Value::as_str)
        != Some("support-bundle")
    {
        failures.push("support bundle command did not report support-bundle surface".to_string());
    }
    if value.pointer("/diagnostic_only").and_then(Value::as_bool) != Some(true) {
        failures.push("support bundle command was not marked diagnostic_only=true".to_string());
    }
    if value.pointer("/backup_semantics").and_then(Value::as_str) != Some("not-a-backup") {
        failures.push("support bundle command did not declare not-a-backup semantics".to_string());
    }
    if value
        .pointer("/verify_status/status")
        .and_then(Value::as_str)
        != Some("verified")
    {
        failures.push(format!(
            "support bundle manifest did not verify cleanly: {}",
            value["verify_status"]
        ));
    }
    if value
        .pointer("/included_artifacts")
        .and_then(Value::as_array)
        .is_none_or(|artifacts| {
            !artifacts
                .iter()
                .any(|artifact| artifact["artifact_kind"].as_str() == Some("failure_context"))
        })
    {
        failures.push("support bundle did not include the redacted failure context".to_string());
    }
    if value
        .pointer("/redaction_summary/raw_session_content_included")
        .and_then(Value::as_bool)
        != Some(false)
    {
        failures.push("support bundle did not report raw session exclusion".to_string());
    }
    if value
        .pointer("/excluded_artifacts")
        .and_then(Value::as_array)
        .is_none_or(|artifacts| {
            !artifacts
                .iter()
                .any(|artifact| artifact["artifact_kind"].as_str() == Some("raw_mirror_blobs"))
        })
    {
        failures.push("support bundle did not document raw mirror blob exclusion".to_string());
    }
}

fn doctor_command_env(fixture: &DoctorFixtureFactory) -> BTreeMap<String, String> {
    [
        ("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1".to_string()),
        ("CASS_IGNORE_SOURCES_CONFIG", "1".to_string()),
        ("NO_COLOR", "1".to_string()),
        ("CASS_NO_COLOR", "1".to_string()),
        ("XDG_DATA_HOME", fixture.home_dir().display().to_string()),
        ("XDG_CONFIG_HOME", fixture.home_dir().display().to_string()),
        ("HOME", fixture.home_dir().display().to_string()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

fn read_fixture_db_row_counts(data_dir: &Path, redactor: &DoctorE2eRedactor) -> Value {
    let db_path = data_dir.join("agent_search.db");
    if !db_path.exists() {
        return json!({
            "status": "missing",
            "agents": Value::Null,
            "conversations": Value::Null,
            "messages": Value::Null,
            "errors": {},
        });
    }

    let storage = match SqliteStorage::open_readonly(&db_path) {
        Ok(storage) => storage,
        Err(err) => {
            return json!({
                "status": "unreadable",
                "agents": Value::Null,
                "conversations": Value::Null,
                "messages": Value::Null,
                "errors": {
                    "open_readonly": redactor.redact(&err.to_string()),
                },
            });
        }
    };

    let mut errors = BTreeMap::new();
    let agents = match storage.list_agents() {
        Ok(agents) => json!(agents.len()),
        Err(err) => {
            errors.insert("agents".to_string(), redactor.redact(&err.to_string()));
            Value::Null
        }
    };
    let conversations = match storage.total_conversation_count() {
        Ok(count) => json!(count),
        Err(err) => {
            errors.insert(
                "conversations".to_string(),
                redactor.redact(&err.to_string()),
            );
            Value::Null
        }
    };
    let messages = match storage.total_message_count() {
        Ok(count) => json!(count),
        Err(err) => {
            errors.insert("messages".to_string(), redactor.redact(&err.to_string()));
            Value::Null
        }
    };
    let status = if errors.is_empty() {
        "ok"
    } else {
        "partial-error"
    };

    json!({
        "status": status,
        "agents": agents,
        "conversations": conversations,
        "messages": messages,
        "errors": errors,
    })
}

pub fn default_doctor_e2e_scenarios() -> Vec<DoctorE2eScenarioSpec> {
    vec![
        DoctorE2eScenarioSpec::new(
            "healthy-read-only-noop",
            DoctorFixtureScenario::Healthy,
            ["quick", "healthy", "read-only"],
        )
        .require_json_pointer("/risk_level")
        .require_json_pointer("/recommended_action")
        .require_json_pointer("/coverage_summary")
        .require_json_pointer("/checks")
        .require_json_pointer("/active_repair")
        .require_json_pointer("/fallback_mode")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/operation_state/mutating_doctor_allowed"),
        DoctorE2eScenarioSpec::new(
            "fresh-uninitialized-read-only",
            DoctorFixtureScenario::FreshUninitialized,
            ["quick", "fresh", "read-only"],
        )
        .require_json_pointer("/risk_level")
        .require_json_pointer("/recommended_action")
        .require_json_pointer("/coverage_summary")
        .require_json_pointer("/checks")
        .require_json_pointer("/active_repair")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/operation_state/mutating_doctor_allowed"),
        DoctorE2eScenarioSpec::new(
            "derived-index-corrupt-read-only",
            DoctorFixtureScenario::IndexCorrupt,
            ["quick", "derived", "read-only"],
        )
        .require_json_pointer("/risk_level")
        .require_json_pointer("/recommended_action")
        .require_json_pointer("/coverage_summary")
        .require_json_pointer("/checks")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/operation_state/mutating_doctor_allowed"),
        DoctorE2eScenarioSpec::new(
            "malformed-sources-toml-read-only",
            DoctorFixtureScenario::MalformedSourcesToml,
            ["quick", "config", "read-only"],
        )
        .require_json_pointer("/risk_level")
        .require_json_pointer("/recommended_action")
        .require_json_pointer("/checks")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/operation_state/mutating_doctor_allowed"),
        DoctorE2eScenarioSpec::new(
            "active-doctor-lock-read-only",
            DoctorFixtureScenario::ActiveLock,
            ["fault", "lock", "read-only"],
        )
        .require_json_pointer("/operation_state/active_doctor_repair")
        .require_json_pointer("/operation_state/mutation_blocked_reason")
        .require_json_pointer("/locks/0/active")
        .require_json_pointer("/operation_outcome/exit_code_kind"),
        DoctorE2eScenarioSpec::new(
            "quick-source-pruned",
            DoctorFixtureScenario::SourcePruned,
            ["quick", "source-mirror", "privacy", "read-only"],
        )
        .require_json_pointer("/source_inventory")
        .require_json_pointer("/raw_mirror")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/operation_state/mutating_doctor_allowed")
        .require_json_pointer("/locks")
        .require_json_pointer("/slow_operations")
        .require_json_pointer("/timing_summary")
        .require_json_pointer("/retry_recommendation")
        .require_json_pointer("/source_authority/selected_authority"),
        DoctorE2eScenarioSpec::new(
            "semantic-fallback-no-archive-damage",
            DoctorFixtureScenario::SemanticUnavailable,
            ["quick", "semantic", "derived", "read-only"],
        )
        .require_json_pointer("/fallback_mode")
        .require_json_pointer("/semantic")
        .require_json_pointer("/derived_semantic_assets")
        .require_json_pointer("/derived_semantic_assets/asset_class")
        .require_json_pointer("/derived_semantic_assets/safe_to_rebuild")
        .require_json_pointer("/derived_semantic_assets/network_allowed")
        .require_json_pointer("/derived_semantic_assets/auto_download_attempted")
        .require_json_pointer("/derived_semantic_assets/blocks_archive_recovery")
        .require_json_pointer("/derived_semantic_assets/model_cache/status")
        .require_json_pointer("/derived_semantic_assets/vector_index/status")
        .require_json_pointer("/checks"),
        DoctorE2eScenarioSpec::new(
            "baseline-diff-derived-only",
            DoctorFixtureScenario::Healthy,
            ["baseline", "derived", "read-only"],
        )
        .baseline_diff_journey()
        .expect_exit_success(true)
        .require_json_pointer("/baseline_diff")
        .require_json_pointer("/changed_assets")
        .require_json_pointer("/baseline_mutated")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/event_log/events"),
        DoctorE2eScenarioSpec::new(
            "safe-auto-healthy-noop",
            DoctorFixtureScenario::Healthy,
            ["safe-auto", "healthy", "mutation"],
        )
        .allow_mutation(true)
        .expect_exit_success(true)
        .require_json_pointer("/safe_auto_eligibility")
        .require_json_pointer("/safe_auto_eligibility/evaluated_findings")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/checks"),
        DoctorE2eScenarioSpec::new(
            "safe-auto-missing-semantic-model-skips-download",
            DoctorFixtureScenario::SemanticUnavailable,
            ["safe-auto", "semantic", "derived", "mutation"],
        )
        .allow_mutation(true)
        .expect_exit_success(true)
        .require_json_pointer("/safe_auto_eligibility")
        .require_json_pointer("/safe_auto_eligibility/evaluated_findings")
        .require_json_pointer("/derived_semantic_assets")
        .require_json_pointer("/derived_semantic_assets/network_allowed")
        .require_json_pointer("/derived_semantic_assets/auto_download_attempted")
        .require_json_pointer("/operation_outcome/kind"),
        DoctorE2eScenarioSpec::new(
            "safe-auto-derived-rebuild-from-readable-archive",
            DoctorFixtureScenario::PartiallyIndexed,
            ["safe-auto", "derived", "mutation"],
        )
        .allow_mutation(true)
        .expect_exit_success(true)
        .require_json_pointer("/safe_auto_eligibility")
        .require_json_pointer("/safe_auto_eligibility/evaluated_findings")
        .require_json_pointer("/safe_auto_eligibility/applied_actions")
        .require_json_pointer("/auto_fix_actions")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/checks"),
        DoctorE2eScenarioSpec::new(
            "safe-auto-stale-derived-metadata-rebuild",
            DoctorFixtureScenario::IndexCorrupt,
            ["safe-auto", "derived", "metadata", "mutation"],
        )
        .allow_mutation(true)
        .require_json_pointer("/safe_auto_eligibility")
        .require_json_pointer("/safe_auto_eligibility/evaluated_findings")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/checks"),
        DoctorE2eScenarioSpec::new(
            "safe-auto-refuses-corrupt-db-source-rebuild",
            DoctorFixtureScenario::DbCorruptWithStaleIndex,
            ["safe-auto", "archive-risk", "mutation"],
        )
        .allow_mutation(true)
        .expect_exit_success(false)
        .require_json_pointer("/safe_auto_eligibility")
        .require_json_pointer("/safe_auto_eligibility/manual_approval_required")
        .require_json_pointer("/safe_auto_eligibility/why_manual_approval_required")
        .require_json_pointer("/safe_auto_eligibility/next_exact_command")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/checks"),
        DoctorE2eScenarioSpec::new(
            "safe-auto-source-pruned-manual-approval",
            DoctorFixtureScenario::SourcePruned,
            ["safe-auto", "source-mirror", "archive-risk", "mutation"],
        )
        .allow_mutation(true)
        .require_json_pointer("/safe_auto_eligibility")
        .require_json_pointer("/safe_auto_eligibility/evaluated_findings")
        .require_json_pointer("/source_inventory")
        .require_json_pointer("/raw_mirror")
        .require_json_pointer("/operation_outcome/kind"),
        DoctorE2eScenarioSpec::new(
            "safe-auto-low-disk-recommends-cleanup",
            DoctorFixtureScenario::LowDisk,
            ["safe-auto", "cleanup", "low-disk", "mutation"],
        )
        .allow_mutation(true)
        .env("CASS_TEST_DOCTOR_STORAGE_AVAILABLE_BYTES", "1024")
        .require_json_pointer("/safe_auto_eligibility")
        .require_json_pointer("/safe_auto_eligibility/evaluated_findings")
        .require_json_pointer("/storage_pressure")
        .require_json_pointer("/operation_outcome/kind"),
        DoctorE2eScenarioSpec::new(
            "safe-auto-concurrent-repair-lock",
            DoctorFixtureScenario::ActiveLock,
            ["safe-auto", "lock", "mutation"],
        )
        .allow_mutation(true)
        .expect_exit_success(false)
        .require_json_pointer("/operation_state/active_doctor_repair")
        .require_json_pointer("/operation_state/mutation_blocked_reason")
        .require_json_pointer("/locks/0/active")
        .require_json_pointer("/operation_outcome/exit_code_kind"),
        DoctorE2eScenarioSpec::new(
            "quick-source-truncated",
            DoctorFixtureScenario::SourceTruncated,
            ["quick", "source-mirror", "truncated", "read-only"],
        )
        .require_json_pointer("/source_inventory")
        .require_json_pointer("/raw_mirror")
        .require_json_pointer("/coverage_summary")
        .require_json_pointer("/source_authority/selected_authority"),
        DoctorE2eScenarioSpec::new(
            "quick-mirror-missing",
            DoctorFixtureScenario::MirrorMissing,
            ["quick", "source-mirror", "fault", "read-only"],
        )
        .require_json_pointer("/source_inventory")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/operation_state/mutating_doctor_allowed")
        .require_json_pointer("/source_authority/selected_authority"),
        DoctorE2eScenarioSpec::new(
            "fault-stale-doctor-lock",
            DoctorFixtureScenario::StaleLock,
            ["fault", "lock", "read-only"],
        )
        .require_json_pointer("/operation_state/owners")
        .require_json_pointer("/locks/0/retry_policy")
        .require_json_pointer("/locks/0/manual_delete_allowed")
        .require_json_pointer("/operation_outcome/kind"),
        DoctorE2eScenarioSpec::new(
            "fault-active-doctor-lock",
            DoctorFixtureScenario::ActiveLock,
            ["fault", "lock", "mutation"],
        )
        .allow_mutation(true)
        .expect_exit_success(false)
        .require_json_pointer("/operation_state/active_doctor_repair")
        .require_json_pointer("/locks/0/active")
        .require_json_pointer("/failure_context/status")
        .require_json_pointer("/operation_outcome/exit_code_kind"),
        DoctorE2eScenarioSpec::new(
            "fault-interrupted-repair",
            DoctorFixtureScenario::InterruptedRepair,
            ["fault", "interrupted", "read-only"],
        )
        .require_json_pointer("/operation_state/interrupted_state_count")
        .require_json_pointer("/operation_state/interrupted_states/0/blocks_mutation")
        .require_json_pointer("/operation_outcome/kind"),
        DoctorE2eScenarioSpec::new(
            "safe-auto-repeated-repair-refusal",
            DoctorFixtureScenario::RepairFailureMarker,
            ["safe-auto", "fault", "repeat-repair", "mutation"],
        )
        .allow_mutation(true)
        .expect_exit_success(false)
        .require_json_pointer("/repair_previously_failed")
        .require_json_pointer("/failure_marker_path")
        .require_json_pointer("/override_available")
        .require_json_pointer("/override_used")
        .require_json_pointer("/repeat_refusal_reason")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/operation_state/active_doctor_repair")
        .require_json_pointer("/checks"),
        DoctorE2eScenarioSpec::new(
            "privacy-support-bundle-sentinel",
            DoctorFixtureScenario::SupportBundle,
            ["privacy", "support-bundle"],
        )
        .require_json_pointer("/raw_mirror/policy/support_bundle_policy")
        .require_json_pointer("/operation_outcome/kind"),
        DoctorE2eScenarioSpec::new(
            "support-bundle-after-failed-repair",
            DoctorFixtureScenario::SupportBundle,
            ["support-bundle", "failure-context", "fault", "privacy"],
        )
        .support_bundle_after_failure()
        .require_json_pointer("/included_artifacts")
        .require_json_pointer("/excluded_artifacts")
        .require_json_pointer("/verify_status/status")
        .require_json_pointer("/redaction_summary/raw_session_content_included"),
        DoctorE2eScenarioSpec::new(
            "multi-file-source-artifacts",
            DoctorFixtureScenario::MultiSource,
            ["source-mirror", "multi-file", "read-only"],
        )
        .require_json_pointer("/source_inventory")
        .require_json_pointer("/remote_source_sync")
        .require_json_pointer("/remote_source_sync/sync_gaps")
        .require_json_pointer("/source_inventory/provider_counts/codex")
        .require_json_pointer("/source_inventory/provider_counts/cline")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/source_authority/selected_authority"),
        DoctorE2eScenarioSpec::new(
            "candidate-build-from-mirror",
            DoctorFixtureScenario::SourcePruned,
            ["candidate", "source-mirror", "mutation"],
        )
        .allow_mutation(true)
        .require_json_pointer("/candidate_staging")
        .require_json_pointer("/candidate_staging/latest_build")
        .require_json_pointer("/candidate_staging/latest_build/candidate_id")
        .require_json_pointer("/candidate_staging/latest_build/live_inventory_unchanged")
        .require_json_pointer("/candidate_staging/latest_build/manifest_path"),
        DoctorE2eScenarioSpec::new(
            "candidate-promote-corrupt-db-derived-followup",
            DoctorFixtureScenario::DbCorruptWithStaleIndex,
            ["candidate", "promotion", "derived", "mutation"],
        )
        .repair_apply()
        .env("CASS_SEMANTIC_MODE", "lexical_only")
        .require_json_pointer("/repair_plan")
        .require_json_pointer("/candidate_staging/completed_candidate_count")
        .require_json_pointer("/candidate_promotion")
        .require_json_pointer("/candidate_promotion/status")
        .require_json_pointer("/candidate_promotion/backup_manifest_path")
        .require_json_pointer("/candidate_promotion/receipt_path")
        .require_json_pointer("/candidate_promotion/derived_assets_consistency_status")
        .require_json_pointer("/candidate_promotion/derived_lexical_followup_status")
        .require_json_pointer("/candidate_promotion/derived_semantic_followup_status")
        .require_json_pointer("/candidate_promotion/derived_vector_followup_status")
        .require_json_pointer("/candidate_promotion/derived_memo_followup_status")
        .require_json_pointer("/candidate_promotion/derived_followup_artifact_path")
        .require_json_pointer("/post_repair_probes"),
        DoctorE2eScenarioSpec::new(
            "candidate-promote-corrupt-db-cross-device-fallback",
            DoctorFixtureScenario::DbCorruptWithStaleIndex,
            [
                "candidate",
                "promotion",
                "portability",
                "cross-device",
                "mutation",
            ],
        )
        .repair_apply()
        .env("CASS_SEMANTIC_MODE", "lexical_only")
        .env("CASS_TEST_DOCTOR_RENAME_FAILURE", "cross-device")
        .require_json_pointer("/repair_plan")
        .require_json_pointer("/candidate_staging/completed_candidate_count")
        .require_json_pointer("/candidate_promotion")
        .require_json_pointer("/candidate_promotion/status")
        .require_json_pointer("/candidate_promotion/fs_mutation_receipts")
        .require_json_pointer("/candidate_promotion/fs_mutation_receipts/0/fallback_kind")
        .require_json_pointer("/candidate_promotion/reader_consistency_guarantee"),
        DoctorE2eScenarioSpec::new(
            "candidate-promote-corrupt-db-rollback-failpoint",
            DoctorFixtureScenario::DbCorruptWithStaleIndex,
            ["candidate", "promotion", "fault", "mutation"],
        )
        .repair_apply()
        .env("CASS_SEMANTIC_MODE", "lexical_only")
        .env(
            "CASS_TEST_DOCTOR_CANDIDATE_PROMOTION_FAILPOINT",
            "after-component-replace",
        )
        .expect_exit_success(false)
        .require_json_pointer("/repair_plan")
        .require_json_pointer("/candidate_staging/completed_candidate_count")
        .require_json_pointer("/candidate_promotion")
        .require_json_pointer("/candidate_promotion/status")
        .require_json_pointer("/candidate_promotion/rollback_reference")
        .require_json_pointer("/candidate_promotion/fs_mutation_receipts")
        .require_json_pointer("/candidate_promotion/reader_consistency_guarantee"),
        DoctorE2eScenarioSpec::new(
            "candidate-promote-corrupt-db-rollback-before-parent-sync",
            DoctorFixtureScenario::DbCorruptWithStaleIndex,
            ["candidate", "promotion", "fault", "mutation"],
        )
        .repair_apply()
        .env("CASS_SEMANTIC_MODE", "lexical_only")
        .env(
            "CASS_TEST_DOCTOR_CANDIDATE_PROMOTION_FAILPOINT",
            "before-parent-sync",
        )
        .expect_exit_success(false)
        .require_json_pointer("/repair_plan")
        .require_json_pointer("/candidate_staging/completed_candidate_count")
        .require_json_pointer("/candidate_promotion")
        .require_json_pointer("/candidate_promotion/status")
        .require_json_pointer("/candidate_promotion/rollback_reference")
        .require_json_pointer("/candidate_promotion/fs_mutation_receipts")
        .require_json_pointer("/candidate_promotion/reader_consistency_guarantee"),
        DoctorE2eScenarioSpec::new(
            "candidate-promote-blocked-coverage-decrease",
            DoctorFixtureScenario::CoverageReducingCandidate,
            ["candidate", "promotion", "coverage", "mutation"],
        )
        .repair_apply()
        .env("CASS_SEMANTIC_MODE", "lexical_only")
        .skip_repair_candidate_build_preflight()
        .expect_exit_success(false)
        .require_json_pointer("/repair_plan")
        .require_json_pointer("/candidate_staging/completed_candidate_count")
        .require_json_pointer("/candidate_promotion")
        .require_json_pointer("/candidate_promotion/status")
        .require_json_pointer("/candidate_promotion/coverage_gate_status")
        .require_json_pointer("/candidate_promotion/coverage_promote_allowed")
        .require_json_pointer("/candidate_promotion/blocked_reasons")
        .require_json_pointer("/candidate_promotion/receipt_path"),
        DoctorE2eScenarioSpec::new(
            "candidate-promote-post-repair-probe-failure",
            DoctorFixtureScenario::DbCorruptWithStaleIndex,
            ["candidate", "promotion", "fault", "post-repair", "mutation"],
        )
        .repair_apply()
        .env("CASS_SEMANTIC_MODE", "lexical_only")
        .env(
            "CASS_TEST_DOCTOR_POST_REPAIR_PROBE_FAULT",
            "archive_db_read_mismatch",
        )
        .expect_exit_success(false)
        .require_json_pointer("/repair_plan")
        .require_json_pointer("/candidate_staging/completed_candidate_count")
        .require_json_pointer("/candidate_promotion")
        .require_json_pointer("/candidate_promotion/status")
        .require_json_pointer("/post_repair_probes")
        .require_json_pointer("/post_repair_probes/status")
        .require_json_pointer("/post_repair_probes/blocks_success")
        .require_json_pointer("/post_repair_probes/probes/0/failure_context_path")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/failure_marker_path")
        .require_json_pointer("/repair_failure_marker"),
        DoctorE2eScenarioSpec::new(
            "cleanup-low-disk-derived-only",
            DoctorFixtureScenario::LowDisk,
            ["quick", "cleanup", "low-disk", "mutation"],
        )
        .cleanup_apply()
        .env("CASS_TEST_DOCTOR_STORAGE_AVAILABLE_BYTES", "1024")
        .require_json_pointer("/storage_pressure")
        .require_json_pointer("/quarantine/lexical_cleanup_dry_run")
        .require_json_pointer("/cleanup_apply")
        .require_json_pointer("/cleanup_apply/actions")
        .require_json_pointer("/candidate_staging"),
        DoctorE2eScenarioSpec::new(
            "backup-exclusion-risk",
            DoctorFixtureScenario::BackupExclusion,
            ["quick", "backups", "preservation", "read-only"],
        )
        .require_json_pointer("/config_exclusion_risks")
        .require_json_pointer("/config_exclusion_risks/0/risk_kind")
        .require_json_pointer("/checks")
        .require_json_pointer("/operation_outcome/kind"),
        DoctorE2eScenarioSpec::new(
            "backups-restore-fixture-journey",
            DoctorFixtureScenario::BackupAvailable,
            ["backups", "restore", "mutation"],
        )
        .backups_restore_journey()
        .require_json_pointer("/backup_verification")
        .require_json_pointer("/restore_plan")
        .require_json_pointer("/restore_rehearsal")
        .require_json_pointer("/restore_apply")
        .require_json_pointer("/restore_apply/receipt_path")
        .require_json_pointer("/restore_apply/pre_restore_backup_manifest_path"),
        DoctorE2eScenarioSpec::new(
            "backups-restore-rollback-failpoint",
            DoctorFixtureScenario::BackupAvailable,
            ["backups", "restore", "fault", "mutation"],
        )
        .backups_restore_journey()
        .env(
            "CASS_TEST_DOCTOR_CANDIDATE_PROMOTION_FAILPOINT",
            "after-component-replace",
        )
        .backup_restore_expect_candidate_promotion_status("rolled_back")
        .require_json_pointer("/backup_verification")
        .require_json_pointer("/restore_plan")
        .require_json_pointer("/restore_rehearsal")
        .require_json_pointer("/restore_apply")
        .require_json_pointer("/restore_apply/receipt_path")
        .require_json_pointer("/restore_apply/pre_restore_backup_manifest_path")
        .require_json_pointer("/restore_apply/candidate_promotion/rollback_reference")
        .require_json_pointer("/restore_apply/candidate_promotion/fs_mutation_receipts"),
    ]
}

pub fn failure_self_test_doctor_e2e_scenario() -> DoctorE2eScenarioSpec {
    DoctorE2eScenarioSpec::new(
        "intentional-failure-self-test",
        DoctorFixtureScenario::SourcePruned,
        ["self-test"],
    )
    .require_json_pointer("/definitely_missing_for_self_test")
}

pub fn doctor_e2e_scenarios_for_args(args: &DoctorE2eCliArgs) -> Vec<DoctorE2eScenarioSpec> {
    let mut scenarios = default_doctor_e2e_scenarios();
    if args.include_failure_self_test {
        scenarios.push(failure_self_test_doctor_e2e_scenario());
    }
    scenarios
}

pub fn doctor_e2e_expected_mutation_class(scenario: &DoctorE2eScenarioSpec) -> &'static str {
    if scenario.allow_mutation {
        "fixture-only-mutation"
    } else {
        "read-only"
    }
}

pub fn doctor_e2e_local_execution_class(scenario: &DoctorE2eScenarioSpec) -> &'static str {
    if scenario.labels.contains("self-test") {
        "local-failure-self-test"
    } else if scenario.labels.contains("quick") && scenario.allow_mutation {
        "local-quick-fixture-mutation"
    } else if scenario.labels.contains("quick") {
        "local-quick-read-only"
    } else if scenario.allow_mutation {
        "local-fixture-mutation"
    } else {
        "local-standard-read-only"
    }
}

pub fn doctor_e2e_safe_rerun_command(scenario_id: &str) -> String {
    format!(
        "scripts/e2e/doctor_v2.sh run --scenario {scenario_id} --artifact-dir <absolute-base-dir>"
    )
}

fn doctor_e2e_command_mode_name(mode: DoctorE2eCommandMode) -> &'static str {
    match mode {
        DoctorE2eCommandMode::Check => "check",
        DoctorE2eCommandMode::Fix => "fix",
        DoctorE2eCommandMode::CleanupApply => "cleanup-apply",
        DoctorE2eCommandMode::RepairApply => "repair-apply",
        DoctorE2eCommandMode::BackupsRestoreJourney => "backups-restore-journey",
        DoctorE2eCommandMode::BaselineDiffJourney => "baseline-diff-journey",
        DoctorE2eCommandMode::SupportBundleAfterFailure => "support-bundle-after-failure",
    }
}

fn doctor_e2e_fixture_scenario_name(scenario: DoctorFixtureScenario) -> &'static str {
    match scenario {
        DoctorFixtureScenario::Healthy => "healthy",
        DoctorFixtureScenario::FreshUninitialized => "fresh-uninitialized",
        DoctorFixtureScenario::SemanticUnavailable => "semantic-unavailable",
        DoctorFixtureScenario::PartiallyIndexed => "partially-indexed",
        DoctorFixtureScenario::SourcePruned => "source-pruned",
        DoctorFixtureScenario::SourceTruncated => "source-truncated",
        DoctorFixtureScenario::MirrorMissing => "mirror-missing",
        DoctorFixtureScenario::DbCorrupt => "db-corrupt",
        DoctorFixtureScenario::DbCorruptWithStaleIndex => "db-corrupt-with-stale-index",
        DoctorFixtureScenario::CoverageReducingCandidate => "coverage-reducing-candidate",
        DoctorFixtureScenario::IndexCorrupt => "index-corrupt",
        DoctorFixtureScenario::StaleLock => "stale-lock",
        DoctorFixtureScenario::ActiveLock => "active-lock",
        DoctorFixtureScenario::InterruptedRepair => "interrupted-repair",
        DoctorFixtureScenario::RepairFailureMarker => "repair-failure-marker",
        DoctorFixtureScenario::BackupAvailable => "backup-available",
        DoctorFixtureScenario::LowDisk => "low-disk",
        DoctorFixtureScenario::BackupExclusion => "backup-exclusion",
        DoctorFixtureScenario::MalformedSourcesToml => "malformed-sources-toml",
        DoctorFixtureScenario::SupportBundle => "support-bundle",
        DoctorFixtureScenario::MultiSource => "multi-source",
        DoctorFixtureScenario::PathEdgeCases => "path-edge-cases",
    }
}

pub fn doctor_e2e_scenario_registry_manifest(
    args: &DoctorE2eCliArgs,
    scenarios: &[DoctorE2eScenarioSpec],
    selected: &[&DoctorE2eScenarioSpec],
) -> Value {
    let selected_ids = selected
        .iter()
        .map(|scenario| scenario.scenario_id.as_str())
        .collect::<BTreeSet<_>>();
    let scenario_values = scenarios
        .iter()
        .map(|scenario| {
            json!({
                "scenario_id": scenario.scenario_id,
                "selected": selected_ids.contains(scenario.scenario_id.as_str()),
                "labels": scenario.labels.iter().cloned().collect::<Vec<_>>(),
                "fixture_scenario": doctor_e2e_fixture_scenario_name(scenario.fixture_scenario),
                "command_mode": doctor_e2e_command_mode_name(scenario.command_mode),
                "expected_runner_status": scenario.expected_runner_status(),
                "expected_mutation_class": doctor_e2e_expected_mutation_class(scenario),
                "local_execution_class": doctor_e2e_local_execution_class(scenario),
                "expect_exit_success": scenario.expect_exit_success,
                "allow_mutation": scenario.allow_mutation,
                "skip_repair_candidate_build_preflight": scenario.skip_repair_candidate_build_preflight,
                "extra_env_keys": scenario.extra_env.keys().cloned().collect::<Vec<_>>(),
                "required_json_pointers": scenario.required_json_pointers,
                "safe_rerun_command": doctor_e2e_safe_rerun_command(&scenario.scenario_id),
            })
        })
        .collect::<Vec<_>>();

    json!({
        "schema_version": DOCTOR_E2E_SCHEMA_VERSION,
        "manifest_kind": "cass_doctor_e2e_scenario_registry_v1",
        "all_scenario_count": scenarios.len(),
        "selected_scenario_count": selected.len(),
        "filters": {
            "labels": args.label_filter.iter().cloned().collect::<Vec<_>>(),
            "scenarios": args.scenario_filter.iter().cloned().collect::<Vec<_>>(),
            "exclude_labels": args.exclude_label_filter.iter().cloned().collect::<Vec<_>>(),
            "exclude_scenarios": args.exclude_scenario_filter.iter().cloned().collect::<Vec<_>>(),
            "include_failure_self_test": args.include_failure_self_test,
            "fail_fast": args.fail_fast,
        },
        "safety_contract": {
            "uses_fixture_data_only": true,
            "launches_bare_cass_tui": false,
            "requires_explicit_mutation_scenarios": true,
            "default_command": "scripts/e2e/doctor_v2.sh run --label quick",
        },
        "scenarios": scenario_values,
    })
}

pub fn validate_doctor_e2e_scenario_registry_manifest_value(value: &Value) -> Result<(), String> {
    if value["schema_version"].as_u64() != Some(u64::from(DOCTOR_E2E_SCHEMA_VERSION)) {
        return Err("scenario registry manifest has unsupported schema_version".to_string());
    }
    if value["manifest_kind"].as_str() != Some("cass_doctor_e2e_scenario_registry_v1") {
        return Err("scenario registry manifest_kind is invalid".to_string());
    }
    if value["safety_contract"]["uses_fixture_data_only"].as_bool() != Some(true) {
        return Err("scenario registry must declare fixture-only data usage".to_string());
    }
    if value["safety_contract"]["launches_bare_cass_tui"].as_bool() != Some(false) {
        return Err("scenario registry must refuse bare cass TUI launches".to_string());
    }
    let scenarios = value["scenarios"]
        .as_array()
        .ok_or_else(|| "scenario registry scenarios must be an array".to_string())?;
    if scenarios.is_empty() {
        return Err("scenario registry must contain at least one scenario".to_string());
    }
    if value["all_scenario_count"].as_u64() != Some(scenarios.len() as u64) {
        return Err("scenario registry all_scenario_count does not match scenarios".to_string());
    }
    let mut seen = BTreeSet::new();
    let mut selected_count = 0_u64;
    for scenario in scenarios {
        let scenario_id = scenario["scenario_id"]
            .as_str()
            .ok_or_else(|| "scenario registry entry is missing scenario_id".to_string())?;
        if !seen.insert(scenario_id.to_string()) {
            return Err(format!("duplicate scenario_id in registry: {scenario_id}"));
        }
        if scenario["selected"].as_bool() == Some(true) {
            selected_count += 1;
        }
        if scenario["safe_rerun_command"]
            .as_str()
            .is_none_or(|command| !command.starts_with("scripts/e2e/doctor_v2.sh run --scenario "))
        {
            return Err(format!(
                "scenario {scenario_id} is missing a safe script rerun command"
            ));
        }
        if scenario["expected_mutation_class"]
            .as_str()
            .is_none_or(str::is_empty)
        {
            return Err(format!(
                "scenario {scenario_id} is missing expected_mutation_class"
            ));
        }
        if scenario["local_execution_class"]
            .as_str()
            .is_none_or(str::is_empty)
        {
            return Err(format!(
                "scenario {scenario_id} is missing local_execution_class"
            ));
        }
    }
    if value["selected_scenario_count"].as_u64() != Some(selected_count) {
        return Err(
            "scenario registry selected_scenario_count does not match selected entries".to_string(),
        );
    }
    Ok(())
}

pub fn doctor_e2e_run_result_summary(
    scenario: &DoctorE2eScenarioSpec,
    result: &DoctorE2eRunResult,
) -> Value {
    json!({
        "scenario_id": scenario.scenario_id,
        "status": result.status,
        "expected_runner_status": scenario.expected_runner_status(),
        "runner_status_matches_expected": result.status == scenario.expected_runner_status(),
        "labels": scenario.labels.iter().cloned().collect::<Vec<_>>(),
        "fixture_scenario": doctor_e2e_fixture_scenario_name(scenario.fixture_scenario),
        "command_mode": doctor_e2e_command_mode_name(scenario.command_mode),
        "expected_mutation_class": doctor_e2e_expected_mutation_class(scenario),
        "local_execution_class": doctor_e2e_local_execution_class(scenario),
        "artifact_dir": result.artifact_dir.display().to_string(),
        "manifest_path": result.manifest_path.display().to_string(),
        "failure_context_path": result
            .failure_context
            .as_ref()
            .map(|_| result.artifact_dir.join("failure_context.json").display().to_string()),
        "log_paths": {
            "commands_jsonl": result.artifact_dir.join("commands.jsonl").display().to_string(),
            "doctor_events_jsonl": result.artifact_dir.join("doctor-events.jsonl").display().to_string(),
            "execution_flow_jsonl": result.artifact_dir.join("execution-flow.jsonl").display().to_string(),
            "receipts_jsonl": result.artifact_dir.join("receipts.jsonl").display().to_string(),
            "checksums_jsonl": result.artifact_dir.join("checksums.jsonl").display().to_string(),
            "stdout_dir": result.artifact_dir.join("stdout").display().to_string(),
            "stderr_dir": result.artifact_dir.join("stderr").display().to_string(),
        },
        "next_suggested_command": doctor_e2e_safe_rerun_command(&scenario.scenario_id),
    })
}

pub fn doctor_e2e_run_error_summary(scenario: &DoctorE2eScenarioSpec, error: &str) -> Value {
    json!({
        "scenario_id": scenario.scenario_id,
        "status": "harness-error",
        "expected_runner_status": scenario.expected_runner_status(),
        "runner_status_matches_expected": false,
        "labels": scenario.labels.iter().cloned().collect::<Vec<_>>(),
        "fixture_scenario": doctor_e2e_fixture_scenario_name(scenario.fixture_scenario),
        "command_mode": doctor_e2e_command_mode_name(scenario.command_mode),
        "expected_mutation_class": doctor_e2e_expected_mutation_class(scenario),
        "local_execution_class": doctor_e2e_local_execution_class(scenario),
        "artifact_dir": null,
        "manifest_path": null,
        "failure_context_path": null,
        "log_paths": null,
        "harness_error": error,
        "next_suggested_command": doctor_e2e_safe_rerun_command(&scenario.scenario_id),
    })
}

pub fn doctor_e2e_run_summary_manifest(
    args: &DoctorE2eCliArgs,
    run_root: &Path,
    scenario_summaries: Vec<Value>,
) -> Value {
    let failed_count = scenario_summaries
        .iter()
        .filter(|scenario| {
            scenario["runner_status_matches_expected"].as_bool() != Some(true)
                || scenario["status"].as_str() == Some("harness-error")
        })
        .count();
    json!({
        "schema_version": DOCTOR_E2E_SCHEMA_VERSION,
        "manifest_kind": "cass_doctor_e2e_run_summary_v1",
        "run_root": run_root.display().to_string(),
        "scenario_count": scenario_summaries.len(),
        "failed_count": failed_count,
        "status": if failed_count == 0 { "pass" } else { "fail" },
        "filters": {
            "labels": args.label_filter.iter().cloned().collect::<Vec<_>>(),
            "scenarios": args.scenario_filter.iter().cloned().collect::<Vec<_>>(),
            "exclude_labels": args.exclude_label_filter.iter().cloned().collect::<Vec<_>>(),
            "exclude_scenarios": args.exclude_scenario_filter.iter().cloned().collect::<Vec<_>>(),
            "include_failure_self_test": args.include_failure_self_test,
            "fail_fast": args.fail_fast,
        },
        "harness_command": {
            "argv": [
                "cargo",
                "test",
                "--locked",
                "--test",
                "doctor_e2e_runner",
                "doctor_e2e_scripted_scenarios",
                "--",
                "--nocapture"
            ],
            "launches_bare_cass_tui": false,
            "default_script_command": "scripts/e2e/doctor_v2.sh run --label quick",
        },
        "scenario_summaries": scenario_summaries,
    })
}

pub fn validate_doctor_e2e_run_summary_manifest_value(value: &Value) -> Result<(), String> {
    if value["schema_version"].as_u64() != Some(u64::from(DOCTOR_E2E_SCHEMA_VERSION)) {
        return Err("run summary manifest has unsupported schema_version".to_string());
    }
    if value["manifest_kind"].as_str() != Some("cass_doctor_e2e_run_summary_v1") {
        return Err("run summary manifest_kind is invalid".to_string());
    }
    if value["harness_command"]["launches_bare_cass_tui"].as_bool() != Some(false) {
        return Err("run summary must declare that it does not launch bare cass".to_string());
    }
    let summaries = value["scenario_summaries"]
        .as_array()
        .ok_or_else(|| "run summary scenario_summaries must be an array".to_string())?;
    if value["scenario_count"].as_u64() != Some(summaries.len() as u64) {
        return Err("run summary scenario_count does not match summaries".to_string());
    }
    for summary in summaries {
        let scenario_id = summary["scenario_id"]
            .as_str()
            .ok_or_else(|| "run summary entry is missing scenario_id".to_string())?;
        if summary["next_suggested_command"]
            .as_str()
            .is_none_or(|command| !command.starts_with("scripts/e2e/doctor_v2.sh run --scenario "))
        {
            return Err(format!(
                "run summary scenario {scenario_id} is missing next_suggested_command"
            ));
        }
        if summary["expected_mutation_class"]
            .as_str()
            .is_none_or(str::is_empty)
        {
            return Err(format!(
                "run summary scenario {scenario_id} is missing expected_mutation_class"
            ));
        }
    }
    Ok(())
}

pub fn default_doctor_e2e_run_root() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    manifest_dir
        .join("test-results/e2e/doctor-v2")
        .join(format!("run-{}-{}", epoch_millis(), std::process::id()))
}

pub fn select_scenarios<'a>(
    args: &DoctorE2eCliArgs,
    scenarios: &'a [DoctorE2eScenarioSpec],
) -> Vec<&'a DoctorE2eScenarioSpec> {
    scenarios
        .iter()
        .filter(|scenario| args.selects(scenario))
        .collect()
}

pub fn validate_artifact_manifest(path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|err| format!("read manifest {}: {err}", path.display()))?;
    let manifest: DoctorE2eArtifactManifest =
        serde_json::from_slice(&bytes).map_err(|err| format!("parse manifest: {err}"))?;
    validate_artifact_manifest_value(
        path.parent()
            .ok_or_else(|| format!("manifest has no parent: {}", path.display()))?,
        &manifest,
    )
}

pub fn validate_artifact_manifest_value(
    artifact_dir: &Path,
    manifest: &DoctorE2eArtifactManifest,
) -> Result<(), String> {
    if manifest.schema_version != DOCTOR_E2E_SCHEMA_VERSION {
        return Err(format!(
            "unsupported doctor e2e manifest schema_version {}",
            manifest.schema_version
        ));
    }
    if manifest.scenario_id.trim().is_empty() {
        return Err("scenario_id must not be empty".to_string());
    }
    if manifest.command_count == 0 {
        return Err("command_count must be greater than zero".to_string());
    }
    for required in default_expected_artifact_keys() {
        let Some(relative) = manifest.artifacts.get(&required) else {
            return Err(format!(
                "manifest is missing required artifact key {required}"
            ));
        };
        validate_artifact_relative_path(relative)?;
        let absolute = artifact_dir.join(relative);
        if !absolute.starts_with(artifact_dir) {
            return Err(format!("artifact path escapes root: {relative}"));
        }
        if !absolute.exists() {
            return Err(format!(
                "artifact listed for {required} is missing: {relative}"
            ));
        }
    }
    if manifest.status == "fail" && manifest.failure_context.is_none() {
        return Err("failed scenarios must include failure_context".to_string());
    }
    if manifest.status == "fail" {
        let Some(relative) = manifest.artifacts.get("failure_context_json") else {
            return Err("failed scenarios must list failure_context_json artifact".to_string());
        };
        validate_artifact_relative_path(relative)?;
        if !artifact_dir.join(relative).exists() {
            return Err(format!(
                "failure_context_json artifact is missing: {relative}"
            ));
        }
    }
    Ok(())
}

pub fn parse_doctor_json_stdout(bytes: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(bytes).map_err(|err| format!("doctor stdout was not valid JSON: {err}"))
}

fn extend_csv_set(set: &mut BTreeSet<String>, value: &str) {
    for item in value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        set.insert(item.to_string());
    }
}

fn validate_run_root(run_root: &Path) -> Result<(), String> {
    if !run_root.is_absolute() {
        return Err(format!(
            "doctor e2e run root must be absolute: {}",
            run_root.display()
        ));
    }
    if run_root.parent().is_none() {
        return Err("doctor e2e runner refuses filesystem root as run root".to_string());
    }
    for component in run_root.components() {
        if matches!(component, Component::ParentDir) {
            return Err(format!(
                "doctor e2e run root must not contain ..: {}",
                run_root.display()
            ));
        }
    }
    Ok(())
}

fn validate_scenario_id(scenario_id: &str) -> Result<(), String> {
    if scenario_id.trim().is_empty() {
        return Err("scenario_id must not be empty".to_string());
    }
    if !scenario_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(format!("scenario_id is not path-safe: {scenario_id:?}"));
    }
    Ok(())
}

fn validate_artifact_relative_path(relative: &str) -> Result<(), String> {
    let path = Path::new(relative);
    if relative.trim().is_empty() || path.is_absolute() {
        return Err(format!("invalid artifact relative path {relative:?}"));
    }
    for component in path.components() {
        match component {
            Component::Normal(name) if portable_artifact_component_is_safe(name) => {}
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("artifact path has unsafe component: {relative}"));
            }
            Component::Normal(_) => {
                return Err(format!(
                    "artifact path has non-portable component: {relative}"
                ));
            }
        }
    }
    Ok(())
}

fn portable_artifact_component_is_safe(name: &std::ffi::OsStr) -> bool {
    let text = name.to_string_lossy();
    if text.is_empty()
        || text.contains('\\')
        || text.contains(':')
        || text.ends_with(' ')
        || text.ends_with('.')
        || text.chars().any(char::is_control)
    {
        return false;
    }
    let stem = text
        .split('.')
        .next()
        .unwrap_or(text.as_ref())
        .trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    !matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "CONIN$"
            | "CONOUT$"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn create_new_dir(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "doctor e2e runner refuses to reuse artifact directory: {}",
            path.display()
        ));
    }
    fs::create_dir_all(path).map_err(|err| format!("create {}: {err}", path.display()))
}

fn write_json_artifact<T: Serialize>(
    artifact_dir: &Path,
    relative: &str,
    value: &T,
    artifacts: &mut BTreeMap<String, String>,
) -> Result<String, String> {
    let absolute = artifact_path(artifact_dir, relative)?;
    write_json_file_new(&absolute, value)?;
    artifacts.insert(artifact_key(relative), relative.to_string());
    Ok(relative.to_string())
}

fn write_text_artifact(
    artifact_dir: &Path,
    relative: &str,
    text: &str,
    artifacts: &mut BTreeMap<String, String>,
) -> Result<String, String> {
    let absolute = artifact_path(artifact_dir, relative)?;
    write_file_new(&absolute, text.as_bytes())?;
    artifacts.insert(artifact_key(relative), relative.to_string());
    Ok(relative.to_string())
}

fn write_jsonl_artifact(
    artifact_dir: &Path,
    relative: &str,
    lines: &[Value],
    artifacts: &mut BTreeMap<String, String>,
) -> Result<String, String> {
    let mut body = String::new();
    for line in lines {
        body.push_str(&serde_json::to_string(line).expect("jsonl line"));
        body.push('\n');
    }
    write_text_artifact(artifact_dir, relative, &body, artifacts)
}

fn artifact_path(artifact_dir: &Path, relative: &str) -> Result<PathBuf, String> {
    validate_artifact_relative_path(relative)?;
    let absolute = artifact_dir.join(relative);
    if !absolute.starts_with(artifact_dir) {
        return Err(format!("artifact path escapes root: {relative}"));
    }
    Ok(absolute)
}

fn artifact_key(relative: &str) -> String {
    match relative {
        "scenario.json" => "scenario_json",
        "fixture-inventory.json" => "fixture_inventory",
        "source-inventory-before.json" => "source_inventory_before",
        "source-inventory-after.json" => "source_inventory_after",
        "execution-flow.jsonl" => "execution_flow",
        "commands.jsonl" => "commands_jsonl",
        "stdout/doctor-json.out" => "stdout_doctor_json",
        "stderr/doctor-json.err" => "stderr_doctor_json",
        "parsed-json/doctor-json.json" => "parsed_json_doctor_json",
        "stdout/doctor-human-check.out" => "stdout_doctor_human_check",
        "stderr/doctor-human-check.err" => "stderr_doctor_human_check",
        "stdout/doctor-check-json.out" => "stdout_doctor_check_json",
        "stderr/doctor-check-json.err" => "stderr_doctor_check_json",
        "parsed-json/doctor-check-json.json" => "parsed_json_doctor_check_json",
        "candidate-staging.json" => "candidate_staging",
        "post-repair-probes.json" => "post_repair_probes",
        "no-mutation-summary.json" => "no_mutation_summary",
        "safe-auto-decision-log.json" => "safe_auto_decision_log",
        "candidate-promotion-derived-followup.json" => "candidate_promotion_derived_followup",
        "file-tree-before.json" => "file_tree_before",
        "file-tree-after.json" => "file_tree_after",
        "checksums.json" => "checksums",
        "timing.json" => "timing",
        "receipts.jsonl" => "receipts",
        "doctor-events.jsonl" => "doctor_logs",
        "redaction-report.json" => "redaction_report",
        "failure_context.json" => "failure_context_json",
        "failure_summary.txt" => "failure_summary",
        other => other,
    }
    .to_string()
}

fn write_json_file_new<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|err| format!("serialize json: {err}"))?;
    write_file_new(path, &bytes)
}

fn write_file_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| format!("create {}: {err}", path.display()))?;
    file.write_all(bytes)
        .map_err(|err| format!("write {}: {err}", path.display()))
}

fn file_blake3(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|err| format!("open {}: {err}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut file, &mut hasher).map_err(|err| format!("hash {}: {err}", path.display()))?;
    Ok(hasher.finalize().to_hex().to_string())
}

fn redact_json_value(value: Value, redactor: &DoctorE2eRedactor) -> Value {
    match value {
        Value::String(text) => Value::String(redactor.redact(&text)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| redact_json_value(item, redactor))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, redact_json_value(value, redactor)))
                .collect(),
        ),
        other => other,
    }
}

fn render_failure_summary(scenario_id: &str, context: &DoctorE2eFailureContext) -> String {
    let mut summary = format!("doctor e2e scenario failed: {scenario_id}\n\nReasons:\n");
    for reason in &context.reasons {
        summary.push_str("- ");
        summary.push_str(reason);
        summary.push('\n');
    }
    if let Some(exit_code) = context.exit_code {
        summary.push_str(&format!("\nExit code: {exit_code}\n"));
    }
    if let Some(stderr_tail) = &context.stderr_tail {
        summary.push_str("\nStderr tail:\n");
        summary.push_str(stderr_tail);
        summary.push('\n');
    }
    summary.push_str("\nFailure context: ");
    summary.push_str(&context.artifacts.failure_context_path);
    summary.push('\n');
    summary.push_str("\nSafe repro template:\n");
    summary.push_str(&context.repro.shell_command);
    summary.push('\n');
    summary
}

pub fn doctor_e2e_shell_quote_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    if arg.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'_' | b'-' | b'.' | b'/' | b':' | b'=' | b'+' | b',' | b'@' | b'%' | b'[' | b']'
            )
    }) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\"'\"'"))
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| doctor_e2e_shell_quote_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        text.to_string()
    } else {
        chars[chars.len() - max_chars..].iter().collect()
    }
}

fn panic_payload_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else {
        "non-string panic payload".to_string()
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

pub fn build_doctor_e2e_timing_report(
    spec: &DoctorE2eScenarioSpec,
    command_records: &[DoctorE2eCommandRecord],
) -> Value {
    let commands = command_records
        .iter()
        .map(|record| {
            let class = doctor_e2e_command_latency_class(spec, record);
            let budget_ms = doctor_e2e_command_budget_ms(class);
            json!({
                "command_id": record.command_id,
                "duration_ms": record.duration_ms,
                "command_class": class,
                "budget_ms": budget_ms,
                "budget_status": if record.duration_ms <= budget_ms { "pass" } else { "warn" },
            })
        })
        .collect::<Vec<_>>();
    let total_duration_ms = command_records
        .iter()
        .map(|record| record.duration_ms)
        .sum::<u64>();
    let total_budget_ms = commands
        .iter()
        .filter_map(|command| command["budget_ms"].as_u64())
        .sum::<u64>();
    let slowest_command = command_records
        .iter()
        .max_by_key(|record| record.duration_ms)
        .map(|record| {
            json!({
                "command_id": record.command_id,
                "duration_ms": record.duration_ms,
                "command_class": doctor_e2e_command_latency_class(spec, record),
            })
        })
        .unwrap_or_else(|| {
            json!({
                "command_id": null,
                "duration_ms": 0,
                "command_class": "none",
            })
        });
    let over_budget_count = commands
        .iter()
        .filter(|command| command["budget_status"].as_str() == Some("warn"))
        .count();

    json!({
        "schema_version": DOCTOR_E2E_SCHEMA_VERSION,
        "scenario_id": spec.scenario_id,
        "command_mode": format!("{:?}", spec.command_mode),
        "status": if over_budget_count == 0 { "pass" } else { "warn" },
        "over_budget_count": over_budget_count,
        "total_duration_ms": total_duration_ms,
        "total_budget_ms": total_budget_ms,
        "slowest_command": slowest_command,
        "commands": commands,
        "notes": [
            "Budgets are release-gate classifiers for doctor e2e artifacts; warn means investigate before release, not that the fixture mutated data.",
            "Fast readiness commands should remain comfortably below their budget; heavier repair/reconstruct paths are classified separately.",
        ],
    })
}

fn doctor_e2e_command_latency_class(
    spec: &DoctorE2eScenarioSpec,
    record: &DoctorE2eCommandRecord,
) -> &'static str {
    match record.command_id.as_str() {
        "doctor-human-check" | "doctor-check-json" => "fast-readiness",
        "doctor-repair-dry-run"
        | "doctor-cleanup-preview"
        | "doctor-backups-restore-rehearsal-good"
        | "doctor-backups-restore-rehearsal-drifted"
        | "doctor-baseline-save" => "planning",
        "doctor-repair-candidate-build" => "candidate-build",
        "doctor-backups-list" | "doctor-backups-verify-good" => "inventory",
        "doctor-json" => match spec.command_mode {
            DoctorE2eCommandMode::Check => "fast-readiness",
            DoctorE2eCommandMode::Fix => "safe-auto",
            DoctorE2eCommandMode::CleanupApply => "cleanup-apply",
            DoctorE2eCommandMode::RepairApply => "repair-apply",
            DoctorE2eCommandMode::BackupsRestoreJourney => "restore-apply",
            DoctorE2eCommandMode::BaselineDiffJourney => "baseline-diff",
            DoctorE2eCommandMode::SupportBundleAfterFailure => "support-bundle",
        },
        _ => "other",
    }
}

fn doctor_e2e_command_budget_ms(command_class: &str) -> u64 {
    match command_class {
        "fast-readiness" | "inventory" => 5_000,
        "planning" | "baseline-diff" => 15_000,
        "safe-auto" | "cleanup-apply" | "support-bundle" => 20_000,
        "candidate-build" | "repair-apply" | "restore-apply" => 30_000,
        _ => 10_000,
    }
}

fn epoch_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}
