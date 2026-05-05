#![allow(dead_code)]

use super::cass_bin;
use super::doctor_fixture::{
    DoctorFixtureFactory, DoctorFixtureScenario, default_expected_artifact_keys,
};
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use walkdir::WalkDir;

const DOCTOR_E2E_SCHEMA_VERSION: u32 = 1;
const PRIVACY_SENTINEL_VALUE: &str = "CASS_DOCTOR_PRIVACY_SENTINEL_DO_NOT_LEAK";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorE2eCliArgs {
    pub label_filter: BTreeSet<String>,
    pub scenario_filter: BTreeSet<String>,
    pub fail_fast: bool,
    pub include_failure_self_test: bool,
}

#[derive(Debug, Clone)]
pub struct DoctorE2eScenarioSpec {
    pub scenario_id: String,
    pub labels: BTreeSet<String>,
    pub fixture_scenario: DoctorFixtureScenario,
    pub expect_exit_success: Option<bool>,
    pub allow_mutation: bool,
    pub required_json_pointers: Vec<String>,
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
    pub reasons: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
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
    pub blake3: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorE2eCommandRecord {
    pub command_id: String,
    pub argv: Vec<String>,
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
        scenario_match && (label_match || failure_self_test_match)
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
            expect_exit_success: None,
            allow_mutation: false,
            required_json_pointers: Vec::new(),
        }
    }

    pub fn expect_exit_success(mut self, expected: bool) -> Self {
        self.expect_exit_success = Some(expected);
        self
    }

    pub fn allow_mutation(mut self, allow: bool) -> Self {
        self.allow_mutation = allow;
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
        fixture
            .validate_manifest()
            .map_err(|err| format!("fixture manifest is invalid: {err}"))?;

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

        let command_start = Instant::now();
        let output = Command::new(&self.cass_bin)
            .args([
                "doctor",
                "--json",
                "--data-dir",
                fixture.data_dir().to_str().ok_or_else(|| {
                    format!(
                        "fixture data dir is not utf8: {}",
                        fixture.data_dir().display()
                    )
                })?,
            ])
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("NO_COLOR", "1")
            .env("CASS_NO_COLOR", "1")
            .env("XDG_DATA_HOME", fixture.home_dir())
            .env("XDG_CONFIG_HOME", fixture.home_dir())
            .env("HOME", fixture.home_dir())
            .output()
            .map_err(|err| format!("failed to run cass doctor --json: {err}"))?;
        let duration_ms = elapsed_ms(command_start);
        let exit_code = output.status.code();
        let stdout_text = String::from_utf8_lossy(&output.stdout);
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        let redacted_stdout = redactor.redact(&stdout_text);
        let redacted_stderr = redactor.redact(&stderr_text);

        let stdout_path = write_text_artifact(
            &scenario_artifact_dir,
            "stdout/doctor-json.out",
            &redacted_stdout,
            &mut artifacts,
        )?;
        let stderr_path = write_text_artifact(
            &scenario_artifact_dir,
            "stderr/doctor-json.err",
            &redacted_stderr,
            &mut artifacts,
        )?;

        let parsed_json = match serde_json::from_slice::<Value>(&output.stdout) {
            Ok(value) => {
                let redacted_value = redact_json_value(value, &redactor);
                let parsed_path = write_json_artifact(
                    &scenario_artifact_dir,
                    "parsed-json/doctor-json.json",
                    &redacted_value,
                    &mut artifacts,
                )?;
                Some((redacted_value, parsed_path))
            }
            Err(err) => {
                failures.push(format!("doctor stdout was not valid JSON: {err}"));
                None
            }
        };

        if let Some(expected) = spec.expect_exit_success {
            let actual = output.status.success();
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
            let manifest_assertion = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fixture.assert_doctor_payload_matches_manifest(value);
            }));
            if let Err(payload) = manifest_assertion {
                failures.push(format!(
                    "doctor JSON did not match fixture scenario manifest: {}",
                    panic_payload_to_string(payload)
                ));
            }
        }

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

        let mutation_diffs = before.diff(&after);
        if !spec.allow_mutation && !mutation_diffs.is_empty() {
            failures.push(format!(
                "no-mutation contract was violated: {}",
                mutation_diffs.join("; ")
            ));
        }

        write_json_artifact(
            &scenario_artifact_dir,
            "checksums.json",
            &after.file_checksums(),
            &mut artifacts,
        )?;
        write_json_artifact(
            &scenario_artifact_dir,
            "timing.json",
            &json!({
                "scenario_id": spec.scenario_id,
                "commands": [{
                    "command_id": "doctor-json",
                    "duration_ms": duration_ms
                }],
                "total_duration_ms": duration_ms
            }),
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

        let command_record = DoctorE2eCommandRecord {
            command_id: "doctor-json".to_string(),
            argv: vec![
                redactor.redact(&self.cass_bin.display().to_string()),
                "doctor".to_string(),
                "--json".to_string(),
                "--data-dir".to_string(),
                redactor.redact(&fixture.data_dir().display().to_string()),
            ],
            exit_code,
            duration_ms,
            stdout_path,
            stderr_path,
            parsed_json_path: parsed_json.as_ref().map(|(_, path)| path.clone()),
            parsed_json_ok: parsed_json.is_some(),
            failure_reason: failures.first().cloned(),
        };
        write_jsonl_artifact(
            &scenario_artifact_dir,
            "commands.jsonl",
            &[serde_json::to_value(&command_record).expect("command record json")],
            &mut artifacts,
        )?;

        let failure_context = if failures.is_empty() {
            None
        } else {
            let context = DoctorE2eFailureContext {
                reasons: failures.clone(),
                command_id: Some("doctor-json".to_string()),
                exit_code,
                stdout_tail: Some(tail_chars(&redacted_stdout, 4096)),
                stderr_tail: Some(tail_chars(&redacted_stderr, 4096)),
            };
            let summary = render_failure_summary(&spec.scenario_id, &context);
            write_text_artifact(
                &scenario_artifact_dir,
                "failure_summary.txt",
                &summary,
                &mut artifacts,
            )?;
            Some(context)
        };

        let status = if failure_context.is_some() {
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
            command_count: 1,
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
                Some(_) => diffs.push(format!("changed:{key}")),
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
            "quick-source-pruned",
            DoctorFixtureScenario::SourcePruned,
            ["quick", "source-mirror", "privacy"],
        )
        .require_json_pointer("/source_inventory")
        .require_json_pointer("/raw_mirror")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/operation_state/mutating_doctor_allowed")
        .require_json_pointer("/source_authority/selected_authority"),
        DoctorE2eScenarioSpec::new(
            "quick-mirror-missing",
            DoctorFixtureScenario::MirrorMissing,
            ["quick", "source-mirror", "fault"],
        )
        .require_json_pointer("/source_inventory")
        .require_json_pointer("/operation_outcome/kind")
        .require_json_pointer("/operation_state/mutating_doctor_allowed")
        .require_json_pointer("/source_authority/selected_authority"),
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
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("artifact path has unsafe component: {relative}"));
            }
        }
    }
    Ok(())
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
        "commands.jsonl" => "commands_jsonl",
        "stdout/doctor-json.out" => "stdout_doctor_json",
        "stderr/doctor-json.err" => "stderr_doctor_json",
        "parsed-json/doctor-json.json" => "parsed_json_doctor_json",
        "file-tree-before.json" => "file_tree_before",
        "file-tree-after.json" => "file_tree_after",
        "checksums.json" => "checksums",
        "timing.json" => "timing",
        "receipts.jsonl" => "receipts",
        "doctor-events.jsonl" => "doctor_logs",
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
    summary
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

fn epoch_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}
