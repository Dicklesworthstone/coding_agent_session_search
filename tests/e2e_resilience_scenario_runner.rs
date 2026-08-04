//! Real-binary execution gate for the eight report-derived resilience scenarios.
//!
//! Bead `coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.12`.
//! This is a distinct integration surface: the scenario registry is consumed as
//! executable data, every command runs through `e2e_runner`, and the gate
//! persists stdout, stderr, the structured event, a classified proof, fixture
//! provenance, and a fail-closed suite manifest.
//!
//! TEMPORARILY DISABLED (`#![cfg(any())]`): this suite was authored against the
//! `e2e_runner` artifact APIs (`ArtifactRunSpec`, `run_with_artifacts`,
//! `ScenarioArtifactManifest`, public `search::e2e_scenarios`) from commit
//! f6cd6376, whose feature series still needs re-landing on the repaired main
//! line (see the merge-repair commit and its follow-up bead). Re-enable by
//! removing the `cfg` below once those APIs are restored.
#![cfg(any())]

mod util;

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use coding_agent_search::e2e_runner::{
    ArtifactRunSpec, RunExpectation, RunMode, RunOutcome, RunSpec, ScenarioArtifactManifest,
    run_capture, run_with_artifacts,
};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::e2e_scenarios::{E2eScenario, ScenarioCommand, ci_scenarios};
use coding_agent_search::search::tantivy::{SCHEMA_HASH, expected_index_dir};
use coding_agent_search::storage::sqlite::{CURRENT_SCHEMA_VERSION, FrankenStorage};
use fs2::FileExt;
use serde_json::{Value, json};

const PROOF_BEAD: &str = "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.12";
const INCIDENT_PRIVATE_MARKER: &str = "CASS_PRIVATE_MARKER_DO_NOT_EMIT";
const ARCHIVED_BODY: &str = "synthetic archived conversation body";

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn file_hash(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn directory_membership(path: &Path) -> Result<Vec<String>, String> {
    let mut members = fs::read_dir(path)
        .map_err(|error| format!("read directory {}: {error}", path.display()))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .map_err(|error| format!("read member of {}: {error}", path.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    members.sort();
    Ok(members)
}

fn replace_placeholders(
    template: &str,
    placeholders: &BTreeMap<String, String>,
) -> Result<String, String> {
    let mut resolved = template.to_string();
    for (name, value) in placeholders {
        resolved = resolved.replace(&format!("{{{name}}}"), value);
    }
    if resolved.contains('{') || resolved.contains('}') {
        return Err(format!(
            "unresolved placeholder in template {template:?}: {resolved:?}"
        ));
    }
    Ok(resolved)
}

fn isolated_env(fixture: &PreparedFixture) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("HOME".to_string(), fixture.root.display().to_string()),
        (
            "XDG_DATA_HOME".to_string(),
            fixture.root.join("xdg-data").display().to_string(),
        ),
        (
            "XDG_CONFIG_HOME".to_string(),
            fixture.config_dir.display().to_string(),
        ),
        (
            "XDG_CACHE_HOME".to_string(),
            fixture.root.join("xdg-cache").display().to_string(),
        ),
        (
            "CODEX_HOME".to_string(),
            fixture.root.join(".codex").display().to_string(),
        ),
        ("CASS_IGNORE_SOURCES_CONFIG".to_string(), "1".to_string()),
        (
            "CODING_AGENT_SEARCH_NO_UPDATE_PROMPT".to_string(),
            "1".to_string(),
        ),
        ("CASS_SEMANTIC_EMBEDDER".to_string(), "hash".to_string()),
        ("NO_COLOR".to_string(), "1".to_string()),
    ])
}

fn setup_index(fixture: &PreparedFixture, setup_dir: &Path) -> Result<Vec<PathBuf>, String> {
    util::seed_codex_session(
        &fixture.root.join(".codex"),
        "rollout-resilience-matrix.jsonl",
        "resilienceprobe",
        true,
    );
    let spec = RunSpec {
        binary_path: util::cass_bin(),
        args: vec![
            "index".to_string(),
            "--full".to_string(),
            "--json".to_string(),
            "--no-progress-events".to_string(),
            "--data-dir".to_string(),
            fixture.data_dir.display().to_string(),
        ],
        timeout: Duration::from_secs(60),
        env_overrides: isolated_env(fixture),
        cwd: fixture.root.clone(),
        fixture_id: Some("synthetic-codex-session".to_string()),
        require_path: None,
        phase: "fixture-setup".to_string(),
        mode: RunMode::Quick,
    };
    let expectation = RunExpectation {
        expect_json: false,
        assertions: vec![(
            "index-produced-output".to_string(),
            Box::new(|stdout: &str, stderr: &str| {
                !stdout.trim().is_empty() || !stderr.trim().is_empty()
            }),
        )],
    };
    let capture = run_capture(&spec, &expectation, epoch_ms());
    fs::create_dir_all(setup_dir)
        .map_err(|error| format!("create {}: {error}", setup_dir.display()))?;
    let stdout_path = setup_dir.join("index.stdout.log");
    let stderr_path = setup_dir.join("index.stderr.log");
    let event_path = setup_dir.join("index.event.json");
    fs::write(&stdout_path, capture.stdout)
        .map_err(|error| format!("write {}: {error}", stdout_path.display()))?;
    fs::write(&stderr_path, capture.stderr)
        .map_err(|error| format!("write {}: {error}", stderr_path.display()))?;
    fs::write(
        &event_path,
        serde_json::to_vec_pretty(&capture.event)
            .map_err(|error| format!("serialize setup event: {error}"))?,
    )
    .map_err(|error| format!("write {}: {error}", event_path.display()))?;
    if capture.event.outcome != RunOutcome::Success {
        return Err(format!(
            "fixture index failed: outcome={} exit={:?}; setup artifacts retained under {}",
            capture.event.outcome.kind(),
            capture.event.exit_code,
            setup_dir.display()
        ));
    }
    Ok(vec![stdout_path, stderr_path, event_path])
}

fn write_quarantine_record(data_dir: &Path) -> Result<PathBuf, String> {
    let quarantine_dir = data_dir.join("quarantine");
    fs::create_dir_all(&quarantine_dir)
        .map_err(|error| format!("create {}: {error}", quarantine_dir.display()))?;
    let path = quarantine_dir.join("index_ingest_poison.jsonl");
    let record = json!({
        "schema_version": 1,
        "conversation_id": "codex|/synthetic/missing.jsonl|/synthetic/workspace|poison|1|2|1",
        "schema_version_at_quarantine": CURRENT_SCHEMA_VERSION,
        "first_quarantined_at_ms": 1_733_000_000_000_i64,
        "last_attempt_at_ms": 1_733_000_000_000_i64,
        "attempt_count": 1,
        "reason": "index-ingest-out-of-memory",
        "error_kind": "out-of-memory",
        "last_error": "synthetic out of memory",
        "agent_slug": "codex",
        "external_id": "poison",
        "source_path": "/synthetic/missing.jsonl",
        "workspace": "/synthetic/workspace",
        "started_at": 1,
        "ended_at": 2,
        "message_count": 1
    });
    fs::write(&path, format!("{record}\n"))
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(path)
}

fn seed_rebuild_state(data_dir: &Path) -> Result<std::fs::File, String> {
    let db_path = data_dir.join("agent_search.db");
    let index_path = expected_index_dir(data_dir);
    fs::create_dir_all(&index_path)
        .map_err(|error| format!("create {}: {error}", index_path.display()))?;
    let checkpoint_path = index_path.join(".lexical-rebuild-state.json");
    fs::write(
        &checkpoint_path,
        serde_json::to_vec_pretty(&json!({
            "version": 2,
            "schema_hash": SCHEMA_HASH,
            "db": {
                "db_path": db_path.display().to_string(),
                "total_conversations": 10,
                "total_messages": 20,
                "storage_fingerprint": "synthetic:10"
            },
            "page_size": 1024,
            "committed_offset": 4,
            "committed_conversation_id": 4,
            "processed_conversations": 4,
            "indexed_docs": 20,
            "committed_meta_fingerprint": null,
            "pending": null,
            "completed": false,
            "updated_at_ms": 1_733_000_123_000_i64,
            "runtime": {
                "queue_depth": 3,
                "inflight_message_bytes": 65536,
                "max_message_bytes_in_flight": 131072,
                "pending_batch_conversations": 9,
                "pending_batch_message_bytes": 131072,
                "page_prep_workers": 6,
                "active_page_prep_jobs": 2,
                "ordered_buffered_pages": 4,
                "budget_generation": 1,
                "producer_budget_wait_count": 2,
                "producer_budget_wait_ms": 17,
                "producer_handoff_wait_count": 1,
                "producer_handoff_wait_ms": 9,
                "host_loadavg_1m_milli": 7250,
                "controller_mode": "pressure_limited",
                "controller_reason": "queue_depth_3_reached_pipeline_capacity_3",
                "staged_merge_workers_max": 3,
                "staged_merge_allowed_jobs": 1,
                "staged_merge_active_jobs": 1,
                "staged_merge_ready_artifacts": 5,
                "staged_merge_ready_groups": 1,
                "staged_merge_controller_reason": "page_prep_workers_saturated_6_of_6",
                "staged_shard_build_workers_max": 6,
                "staged_shard_build_allowed_jobs": 5,
                "staged_shard_build_active_jobs": 4,
                "staged_shard_build_pending_jobs": 2,
                "staged_shard_build_controller_reason": "reserving_1_slots_for_staged_merge_active_jobs_1_ready_groups_1",
                "updated_at_ms": 1_733_000_124_000_i64
            }
        }))
        .map_err(|error| format!("serialize rebuild state: {error}"))?,
    )
    .map_err(|error| format!("write {}: {error}", checkpoint_path.display()))?;

    let lock_path = data_dir.join("index-run.lock");
    let mut lock = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("open {}: {error}", lock_path.display()))?;
    lock.lock_exclusive()
        .map_err(|error| format!("lock {}: {error}", lock_path.display()))?;
    writeln!(
        lock,
        "pid={}\nstarted_at_ms={}\ndb_path={}\nmode=index",
        std::process::id(),
        1_733_000_111_000_i64,
        db_path.display()
    )
    .map_err(|error| format!("write lock metadata: {error}"))?;
    lock.flush()
        .map_err(|error| format!("flush lock metadata: {error}"))?;
    Ok(lock)
}

fn fixture_message(content: &str, created_at: i64) -> Message {
    Message {
        id: None,
        idx: 0,
        role: MessageRole::User,
        author: Some("synthetic-fixture".to_string()),
        created_at: Some(created_at),
        content: content.to_string(),
        extra_json: json!({"synthetic": true}),
        snippets: Vec::new(),
    }
}

fn fixture_conversation(
    external_id: &str,
    source_path: &str,
    source_id: &str,
    origin_host: Option<&str>,
    created_at: i64,
    content: &str,
) -> Conversation {
    Conversation {
        id: None,
        agent_slug: "codex".to_string(),
        workspace: Some(PathBuf::from("/synthetic/workspace")),
        external_id: Some(external_id.to_string()),
        title: Some(format!("synthetic {external_id}")),
        source_path: PathBuf::from(source_path),
        started_at: Some(created_at),
        ended_at: Some(created_at + 1),
        approx_tokens: None,
        metadata_json: json!({"synthetic": true}),
        messages: vec![fixture_message(content, created_at)],
        source_id: source_id.to_string(),
        origin_host: origin_host.map(str::to_string),
    }
}

fn seed_archive(db_path: &Path, conversations: &[Conversation]) -> Result<(), String> {
    let storage = FrankenStorage::open(db_path)
        .map_err(|error| format!("open FrankenStorage {}: {error:#}", db_path.display()))?;
    let agent_id = storage
        .ensure_agent(&Agent {
            id: None,
            slug: "codex".to_string(),
            name: "Codex".to_string(),
            version: Some("synthetic-fixture".to_string()),
            kind: AgentKind::Cli,
        })
        .map_err(|error| format!("seed agent: {error:#}"))?;
    for conversation in conversations {
        storage
            .insert_conversation_tree(agent_id, None, conversation)
            .map_err(|error| format!("seed conversation: {error:#}"))?;
    }
    Ok(())
}

struct PreparedFixture {
    root: PathBuf,
    data_dir: PathBuf,
    config_dir: PathBuf,
    placeholders: BTreeMap<String, String>,
    fixture_manifest_path: PathBuf,
    setup_artifacts: Vec<PathBuf>,
    protected_hashes: Vec<(PathBuf, String)>,
    protected_memberships: Vec<(PathBuf, Vec<String>)>,
    _rebuild_lock: Option<std::fs::File>,
}

impl PreparedFixture {
    fn resolve(&self, template: &str) -> Result<String, String> {
        replace_placeholders(template, &self.placeholders)
    }

    fn assert_protected_unchanged(&self) -> Result<(), String> {
        for (path, expected) in &self.protected_hashes {
            let actual = file_hash(path)?;
            if &actual != expected {
                return Err(format!(
                    "read-only scenario mutated protected fixture {}: before={expected} after={actual}",
                    path.display()
                ));
            }
        }
        for (path, expected) in &self.protected_memberships {
            let actual = directory_membership(path)?;
            let missing = expected
                .iter()
                .filter(|member| !actual.contains(member))
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                return Err(format!(
                    "read-only scenario removed protected directory members from {}: missing={missing:?} before={expected:?} after={actual:?}",
                    path.display()
                ));
            }
        }
        Ok(())
    }
}

fn prepare_fixture(
    scenario: &E2eScenario,
    fixtures_root: &Path,
) -> Result<PreparedFixture, String> {
    let root = fixtures_root.join(scenario.id);
    let data_dir = root.join("data");
    let config_dir = root.join("config");
    fs::create_dir_all(&data_dir)
        .map_err(|error| format!("create {}: {error}", data_dir.display()))?;
    fs::create_dir_all(&config_dir)
        .map_err(|error| format!("create {}: {error}", config_dir.display()))?;

    let db_path = data_dir.join("agent_search.db");
    let source_path =
        "/Users/synthetic/Library/Application Support/cass/moved-session.jsonl".to_string();
    let trace_path = root.join("trace.jsonl");
    let probe_path = root.join("source-doctor-probe.json");
    let fixture_manifest_path = root.join("fixture-manifest.json");
    let mut placeholders = BTreeMap::from([
        ("data_dir".to_string(), data_dir.display().to_string()),
        ("db_path".to_string(), db_path.display().to_string()),
        ("source_path".to_string(), source_path.clone()),
        ("config_dir".to_string(), config_dir.display().to_string()),
        ("trace_path".to_string(), trace_path.display().to_string()),
        ("probe_path".to_string(), probe_path.display().to_string()),
        (
            "fixture_manifest_path".to_string(),
            fixture_manifest_path.display().to_string(),
        ),
    ]);
    let mut fixture = PreparedFixture {
        root,
        data_dir,
        config_dir: config_dir.clone(),
        placeholders: BTreeMap::new(),
        fixture_manifest_path,
        setup_artifacts: Vec::new(),
        protected_hashes: Vec::new(),
        protected_memberships: Vec::new(),
        _rebuild_lock: None,
    };

    match scenario.id {
        "local_healthy_lexical_semantic_unavailable" => {
            fixture.setup_artifacts = setup_index(&fixture, &fixture.root.join("setup"))?;
        }
        "local_stale_quarantine" => {
            fixture.setup_artifacts = setup_index(&fixture, &fixture.root.join("setup"))?;
            fixture
                .setup_artifacts
                .push(write_quarantine_record(&fixture.data_dir)?);
        }
        "archive_risk_backup_first" => {
            fs::write(&db_path, b"synthetic unreadable sqlite archive evidence")
                .map_err(|error| format!("write {}: {error}", db_path.display()))?;
            fixture
                .protected_hashes
                .push((db_path.clone(), file_hash(&db_path)?));
            fixture.protected_memberships.push((
                fixture.data_dir.clone(),
                directory_membership(&fixture.data_dir)?,
            ));
        }
        "long_rebuild_watch_stall" => {
            fixture._rebuild_lock = Some(seed_rebuild_state(&fixture.data_dir)?);
            fixture.setup_artifacts.extend([
                expected_index_dir(&fixture.data_dir).join(".lexical-rebuild-state.json"),
                fixture.data_dir.join("index-run.lock"),
            ]);
        }
        "dependency_noise" => {}
        "moved_workspace_archive_drilldown" => {
            seed_archive(
                &db_path,
                &[fixture_conversation(
                    "archive-only",
                    &source_path,
                    "remote-proof",
                    Some("synthetic-host"),
                    1_733_000_000_000,
                    ARCHIVED_BODY,
                )],
            )?;
            fixture.setup_artifacts.push(db_path.clone());
        }
        "remote_unreachable_old_host" => {
            let config_path = config_dir.join("cass/sources.toml");
            fs::create_dir_all(
                config_path
                    .parent()
                    .ok_or_else(|| "sources config missing parent".to_string())?,
            )
            .map_err(|error| format!("create sources config dir: {error}"))?;
            fs::write(
                &config_path,
                r#"[[sources]]
name = "fixture-source"
type = "ssh"
host = "fixture@reachable.test"
paths = ["~/.claude/projects"]
platform = "linux"
"#,
            )
            .map_err(|error| format!("write sources config: {error}"))?;
            fs::write(
                &probe_path,
                br#"{"host":"fixture@reachable.test","os":"Linux","cass_version":"0.0.1","remote_path":"nonempty"}"#,
            )
            .map_err(|error| format!("write source doctor probe: {error}"))?;
            fixture
                .setup_artifacts
                .extend([config_path, probe_path.clone()]);
        }
        "incident_mining_large_corpus" => {
            seed_archive(
                &db_path,
                &[
                    fixture_conversation(
                        "incident-local-older",
                        "/synthetic/older.jsonl",
                        "local",
                        None,
                        1_700_000_000_000,
                        "cass index-ingest-out-of-memory quarantined_conversations=1",
                    ),
                    fixture_conversation(
                        "incident-remote-newest",
                        "/synthetic/remote.jsonl",
                        "remote-proof",
                        Some("synthetic-host"),
                        1_800_000_000_000,
                        &format!(
                            "{INCIDENT_PRIVATE_MARKER} cass health_class degraded database is locked"
                        ),
                    ),
                ],
            )?;
            fixture.setup_artifacts.push(db_path.clone());
        }
        other => return Err(format!("no deterministic fixture builder for {other}")),
    }

    let manifest = json!({
        "schema_version": 1,
        "scenario_id": scenario.id,
        "fixture_setup": scenario.fixture_setup,
        "synthetic": true,
        "privacy_note": scenario.privacy_note,
        "created_at_ms": epoch_ms(),
        "setup_artifacts": fixture
            .setup_artifacts
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>(),
        "protected_files": fixture
            .protected_hashes
            .iter()
            .map(|(path, hash)| json!({"path": path, "blake3": hash}))
            .collect::<Vec<_>>(),
        "protected_directories": fixture
            .protected_memberships
            .iter()
            .map(|(path, members)| json!({"path": path, "members": members}))
            .collect::<Vec<_>>(),
    });
    fs::write(
        &fixture.fixture_manifest_path,
        serde_json::to_vec_pretty(&manifest)
            .map_err(|error| format!("serialize fixture manifest: {error}"))?,
    )
    .map_err(|error| format!("write {}: {error}", fixture.fixture_manifest_path.display()))?;
    fixture
        .setup_artifacts
        .push(fixture.fixture_manifest_path.clone());
    placeholders.insert(
        "fixture_manifest_path".to_string(),
        fixture.fixture_manifest_path.display().to_string(),
    );
    fixture.placeholders = placeholders;
    Ok(fixture)
}

fn command_expectation(
    scenario: &'static E2eScenario,
    command: &'static ScenarioCommand,
    fixture: &PreparedFixture,
) -> Result<RunExpectation<'static>, String> {
    let mut assertions = Vec::new();
    for expectation in command.json_assertions {
        let expectation = *expectation;
        assertions.push((
            expectation.label(),
            Box::new(move |stdout: &str, _stderr: &str| {
                serde_json::from_str::<Value>(stdout.trim())
                    .is_ok_and(|value| expectation.evaluate(&value))
            }) as Box<dyn Fn(&str, &str) -> bool>,
        ));
    }
    assertions.push((
        "stdout-is-ansi-free".to_string(),
        Box::new(|stdout: &str, _stderr: &str| !stdout.as_bytes().contains(&0x1b)),
    ));
    for (index, marker) in command.forbidden_stream_markers.iter().enumerate() {
        let marker = marker.to_ascii_lowercase();
        assertions.push((
            format!("streams-omit-forbidden-marker-{index}"),
            Box::new(move |stdout: &str, stderr: &str| {
                !stdout.to_ascii_lowercase().contains(&marker)
                    && !stderr.to_ascii_lowercase().contains(&marker)
            }),
        ));
    }

    let fixture_manifest = fixture.fixture_manifest_path.clone();
    let scenario_id = scenario.id.to_string();
    assertions.push((
        "fixture-manifest-identifies-scenario".to_string(),
        Box::new(move |_stdout: &str, _stderr: &str| {
            fs::read(&fixture_manifest)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .and_then(|value| {
                    value
                        .get("scenario_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                == Some(scenario_id.clone())
        }),
    ));
    for (index, path) in fixture.setup_artifacts.iter().cloned().enumerate() {
        assertions.push((
            format!("setup-artifact-{index}-exists"),
            Box::new(move |_stdout: &str, _stderr: &str| path.is_file()),
        ));
    }

    if command.id == "status-under-dependency-noise" {
        let trace_path = PathBuf::from(fixture.resolve("{trace_path}")?);
        assertions.push((
            "explicit-trace-is-nonempty-jsonl".to_string(),
            Box::new(move |_stdout: &str, _stderr: &str| {
                fs::read_to_string(&trace_path).ok().is_some_and(|trace| {
                    trace
                        .lines()
                        .rev()
                        .find(|line| !line.trim().is_empty())
                        .and_then(|line| serde_json::from_str::<Value>(line.trim()).ok())
                        .is_some_and(|summary| summary["cmd"] == "status")
                })
            }),
        ));
    }
    if command.id.ends_with("source-doctor") {
        let probe_path = PathBuf::from(fixture.resolve("{probe_path}")?);
        assertions.push((
            "source-probe-identifies-synthetic-host".to_string(),
            Box::new(move |_stdout: &str, _stderr: &str| {
                fs::read(&probe_path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .is_some_and(|probe| probe["host"] == "fixture@reachable.test")
            }),
        ));
    }

    Ok(RunExpectation {
        expect_json: true,
        assertions,
    })
}

fn command_extra_artifacts(
    fixture: &PreparedFixture,
    command: &ScenarioCommand,
) -> Result<Vec<PathBuf>, String> {
    let mut paths = fixture.setup_artifacts.clone();
    for template in command.expected_extra_artifacts {
        let path = PathBuf::from(fixture.resolve(template)?);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(paths)
}

#[test]
fn deterministic_resilience_matrix_emits_complete_current_redacted_proof() -> Result<(), String> {
    let artifact_root = match dotenvy::var("CASS_RESILIENCE_PROOF_DIR") {
        Ok(path) => PathBuf::from(path),
        Err(_) => std::env::temp_dir().join("cass-resilience-proof-artifacts"),
    };
    fs::create_dir_all(&artifact_root)
        .map_err(|error| format!("create artifact root {}: {error}", artifact_root.display()))?;
    let artifact_root = fs::canonicalize(&artifact_root).map_err(|error| {
        format!(
            "canonicalize artifact root {}: {error}",
            artifact_root.display()
        )
    })?;

    let suite_started_ms = epoch_ms();
    let run_id = format!(
        "resilience-matrix-{}-{}",
        std::process::id(),
        suite_started_ms
    );
    let run_root = artifact_root.join(&run_id);
    let fixtures_root = run_root.join("fixtures");
    fs::create_dir_all(&fixtures_root)
        .map_err(|error| format!("create fixture root {}: {error}", fixtures_root.display()))?;

    let scenarios = ci_scenarios();
    let expected_scenario_ids = scenarios
        .iter()
        .map(|scenario| scenario.id.to_string())
        .collect::<Vec<_>>();
    let expected_command_keys = scenarios
        .iter()
        .flat_map(|scenario| {
            scenario
                .commands
                .iter()
                .map(|command| format!("{}/{}", scenario.id, command.id))
        })
        .collect::<Vec<_>>();
    let expected_command_count = scenarios
        .iter()
        .map(|scenario| scenario.commands.len())
        .sum();
    let mut manifest = ScenarioArtifactManifest::new(
        &run_id,
        RunMode::Quick,
        suite_started_ms,
        expected_scenario_ids,
        expected_command_keys,
        expected_command_count,
    );

    for scenario in scenarios {
        let fixture = prepare_fixture(scenario, &fixtures_root)
            .map_err(|error| format!("{} fixture: {error}", scenario.id))?;
        for command in scenario.commands {
            let args = command
                .args
                .iter()
                .map(|argument| fixture.resolve(argument))
                .collect::<Result<Vec<_>, _>>()?;
            let mut env = isolated_env(&fixture);
            if scenario.id == "remote_unreachable_old_host" {
                env.remove("CASS_IGNORE_SOURCES_CONFIG");
            }
            for override_value in command.env {
                env.insert(
                    override_value.key.to_string(),
                    fixture.resolve(override_value.value)?,
                );
            }
            let spec = RunSpec {
                binary_path: util::cass_bin(),
                args,
                timeout: Duration::from_millis(command.timeout_ms),
                env_overrides: env,
                cwd: fixture.root.clone(),
                fixture_id: Some(scenario.id.to_string()),
                require_path: Some(fixture.fixture_manifest_path.clone()),
                phase: "verify".to_string(),
                mode: RunMode::Quick,
            };
            let expectation = command_expectation(scenario, command, &fixture)?;
            let extra_artifacts = command_extra_artifacts(&fixture, command)?;
            let issue_ids = [PROOF_BEAD, scenario.owning_bead];
            let request = ArtifactRunSpec {
                artifact_root: &artifact_root,
                run_id: &run_id,
                scenario_id: scenario.id,
                command_id: command.id,
                issue_ids: &issue_ids,
                privacy_note: scenario.privacy_note,
                suite_started_ms,
                accepted_exit_codes: command.accepted_exit_codes,
                forbidden_stream_markers: command.forbidden_stream_markers,
                extra_artifact_paths: &extra_artifacts,
            };
            let entry = run_with_artifacts(&spec, &expectation, &request, epoch_ms())
                .map_err(|error| format!("{}/{} runner: {error}", scenario.id, command.id))?;
            manifest.record(entry);
        }
        fixture.assert_protected_unchanged()?;
    }

    manifest.suite_finished_ms = manifest.suite_finished_ms.max(epoch_ms());
    let errors = manifest.validation_errors();
    if !errors.is_empty() {
        return Err(format!(
            "resilience proof manifest is incomplete:\n  - {}",
            errors.join("\n  - ")
        ));
    }
    let manifest_path = run_root.join("scenario-manifest.json");
    let manifest_jsonl_path = run_root.join("scenario-manifest.jsonl");
    manifest
        .write_json(&manifest_path)
        .map_err(|error| format!("write manifest: {error}"))?;
    manifest
        .write_jsonl(&manifest_jsonl_path)
        .map_err(|error| format!("write manifest jsonl: {error}"))?;

    let persisted: ScenarioArtifactManifest = serde_json::from_slice(
        &fs::read(&manifest_path)
            .map_err(|error| format!("read {}: {error}", manifest_path.display()))?,
    )
    .map_err(|error| format!("parse {}: {error}", manifest_path.display()))?;
    if !persisted.is_clean_pass() {
        return Err(format!(
            "persisted manifest failed revalidation: {:?}",
            persisted.validation_errors()
        ));
    }
    let mut redaction_scan_paths = vec![manifest_path.clone(), manifest_jsonl_path.clone()];
    for entry in &persisted.entries {
        redaction_scan_paths.extend([
            PathBuf::from(&entry.stdout_path),
            PathBuf::from(&entry.stderr_path),
            PathBuf::from(&entry.event_path),
            PathBuf::from(&entry.proof_path),
        ]);
    }
    for path in redaction_scan_paths {
        if fs::read_to_string(&path)
            .map_err(|error| format!("read redaction artifact {}: {error}", path.display()))?
            .contains(INCIDENT_PRIVATE_MARKER)
        {
            return Err(format!(
                "retained proof artifact leaked the synthetic private marker: {}",
                path.display()
            ));
        }
    }

    // Prove the integrated gate fails closed without deleting or modifying any
    // real artifact: mutate only in-memory clones.
    let mut stale = persisted.clone();
    stale.entries[0].fresh_for_suite = false;
    if stale.is_clean_pass() {
        return Err("stale artifact clone passed completeness validation".to_string());
    }
    let mut redaction_failed = persisted.clone();
    redaction_failed.entries[0].redaction_safe = false;
    if redaction_failed.is_clean_pass() {
        return Err("redaction-failed clone passed completeness validation".to_string());
    }
    let mut generated_only = persisted.clone();
    generated_only.entries[0].generated_only = true;
    if generated_only.is_clean_pass() {
        return Err("generated-only clone passed completeness validation".to_string());
    }
    let mut missing_log = persisted.clone();
    missing_log.entries[0].stdout_path = run_root
        .join("intentionally-absent.stdout")
        .display()
        .to_string();
    if missing_log.is_clean_pass() {
        return Err("missing-log clone passed completeness validation".to_string());
    }
    let mut unexpected_command = persisted.clone();
    unexpected_command.entries[0].command_id = "unexpected-command".to_string();
    if unexpected_command.is_clean_pass() {
        return Err("unexpected command clone passed coverage validation".to_string());
    }
    let mut ambiguous_duplicate = persisted.clone();
    ambiguous_duplicate
        .entries
        .push(ambiguous_duplicate.entries[0].clone());
    if ambiguous_duplicate.is_clean_pass() {
        return Err("ambiguous duplicate command clone passed validation".to_string());
    }
    let mut invalid_event = persisted.clone();
    invalid_event.entries[0].event_path = manifest_path.display().to_string();
    if invalid_event.is_clean_pass() {
        return Err("invalid event artifact clone passed validation".to_string());
    }
    let mut invalid_proof = persisted.clone();
    invalid_proof.entries[0].proof_path = manifest_path.display().to_string();
    if invalid_proof.is_clean_pass() {
        return Err("invalid proof artifact clone passed validation".to_string());
    }
    let empty = ScenarioArtifactManifest::new(
        "empty",
        RunMode::Quick,
        suite_started_ms,
        vec!["required".to_string()],
        vec!["required/command".to_string()],
        1,
    );
    if empty.is_clean_pass() {
        return Err("empty manifest passed by doing nothing".to_string());
    }

    println!(
        "resilience_scenario_manifest={} entries={} scenarios={}",
        manifest_path.display(),
        persisted.entries.len(),
        persisted.expected_scenario_ids.len()
    );
    Ok(())
}
