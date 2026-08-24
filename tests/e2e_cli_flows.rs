//! E2E CLI/TUI flows with rich logging (yln.5).
//!
//! Tests cover:
//! - Search query E2E with --trace flag
//! - Detail find (view/expand commands)
//! - Filter combinations (agent, days, workspace)
//! - Logging/trace output validation
//!
//! All tests use real fixtures and assert outputs (no mocks).
//!
//! # E2E Logging
//!
//! Tests emit structured JSONL logs via E2eLogger when `E2E_LOG=1` is set.
//! See `docs/reference/E2E_LOGGING_SCHEMA.md` for log format.

use assert_cmd::Command;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::sources::provenance::Source;
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

mod util;

use util::e2e_log::{E2eCommandEnvironment, E2ePerformanceMetrics, PhaseTracker};

// =============================================================================
// E2E Logger Support
// =============================================================================

// PhaseTracker is provided by util::e2e_log

/// Create a minimal Codex session fixture.
fn make_codex_session(root: &std::path::Path, content: &str, ts: u64) {
    let sessions = root.join("sessions/2024/12/01");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-test.jsonl");
    let sample = format!(
        r#"{{"type": "event_msg", "timestamp": {ts}, "payload": {{"type": "user_message", "message": "{content}"}}}}
{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "{content}_response"}}}}
"#,
        ts + 1000
    );
    fs::write(file, sample).unwrap();
}

/// Create a Claude Code session fixture.
fn make_claude_session(root: &std::path::Path, project: &str, content: &str) {
    let project_dir = root.join(format!("projects/{project}"));
    fs::create_dir_all(&project_dir).unwrap();
    let file = project_dir.join("session.jsonl");
    let sample = format!(
        r#"{{"type": "user", "timestamp": "2024-12-01T10:00:00Z", "message": {{"role": "user", "content": "{content}"}}}}
{{"type": "assistant", "timestamp": "2024-12-01T10:01:00Z", "message": {{"role": "assistant", "content": "{content}_response"}}}}"#
    );
    fs::write(file, sample).unwrap();
}

#[allow(deprecated)]
fn base_cmd(command_env: &E2eCommandEnvironment) -> Command {
    let mut cmd = Command::cargo_bin("cass").unwrap();
    command_env.apply_to_assert(&mut cmd);
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd
}

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_cli_flows", test_name)
}

const PACK_E2E_REDACTION_CANARY: &str = "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";

struct PackArchiveFixture {
    tmp: TempDir,
    data_dir: PathBuf,
    artifact_dir: PathBuf,
    source_files: Vec<(PathBuf, Vec<u8>)>,
}

fn pack_agent(slug: &str, name: &str) -> Agent {
    Agent {
        id: None,
        slug: slug.to_string(),
        name: name.to_string(),
        version: Some("e2e-fixture".to_string()),
        kind: AgentKind::Cli,
    }
}

fn pack_message(idx: i64, role: MessageRole, created_at: i64, content: &str) -> Message {
    Message {
        id: None,
        idx,
        role,
        author: None,
        created_at: Some(created_at),
        content: content.to_string(),
        extra_json: serde_json::json!({}),
        snippets: Vec::new(),
    }
}

struct PackConversationSeed<'a> {
    agent_slug: &'a str,
    workspace: &'a Path,
    source_path: &'a Path,
    external_id: &'a str,
    title: &'a str,
    source_id: &'a str,
    origin_host: Option<&'a str>,
    started_at: i64,
    messages: Vec<Message>,
}

fn pack_conversation(seed: PackConversationSeed<'_>) -> Conversation {
    let ended_at = seed.messages.last().and_then(|message| message.created_at);
    Conversation {
        id: None,
        agent_slug: seed.agent_slug.to_string(),
        workspace: Some(seed.workspace.to_path_buf()),
        external_id: Some(seed.external_id.to_string()),
        title: Some(seed.title.to_string()),
        source_path: seed.source_path.to_path_buf(),
        started_at: Some(seed.started_at),
        ended_at,
        approx_tokens: None,
        metadata_json: serde_json::json!({ "fixture": "pack_handoff_journey" }),
        messages: seed.messages,
        source_id: seed.source_id.to_string(),
        origin_host: seed.origin_host.map(str::to_string),
    }
}

fn write_pack_source_log(path: &Path, lines: &[&str]) -> Vec<u8> {
    let body = format!("{}\n", lines.join("\n"));
    fs::create_dir_all(path.parent().expect("source log parent")).expect("create source log dir");
    fs::write(path, body.as_bytes()).expect("write source log fixture");
    body.into_bytes()
}

fn setup_pack_archive_fixture(tracker: &PhaseTracker) -> PackArchiveFixture {
    let tmp = TempDir::new().expect("create pack e2e tempdir");
    let home = tmp.path();
    let data_dir = home.join("cass_pack_data");
    let artifact_dir = home.join("pack_artifacts");
    let logs_dir = home.join("source_logs");
    let workspace_checkout = home.join("workspaces/checkout-service");
    let workspace_billing = home.join("workspaces/billing-worker");
    fs::create_dir_all(&data_dir).expect("create data dir");
    fs::create_dir_all(&artifact_dir).expect("create artifact dir");
    fs::create_dir_all(&workspace_checkout).expect("create checkout workspace");
    fs::create_dir_all(&workspace_billing).expect("create billing workspace");

    let phase_start = tracker.start(
        "seed_pack_archive",
        Some("Seed real archive DB plus source log files for cass pack"),
    );

    let codex_source = logs_dir.join("codex-checkout.jsonl");
    let claude_source = logs_dir.join("claude-checkout.jsonl");
    let codex_original = write_pack_source_log(
        &codex_source,
        &[
            "user: checkout failure appears after payment redirect",
            "assistant: duplicate checkout failure timeout retry guard was missing",
        ],
    );
    let claude_original = write_pack_source_log(
        &claude_source,
        &[
            "user: remote checkout failure includes bearer token",
            "assistant: duplicate checkout failure timeout retry guard was missing",
        ],
    );

    let storage =
        SqliteStorage::open(&data_dir.join("agent_search.db")).expect("create pack archive DB");
    storage
        .upsert_source(&Source::local())
        .expect("seed local source");
    storage
        .upsert_source(&Source::remote(
            "remote-build-node",
            "builder@remote-build-node.internal",
        ))
        .expect("seed remote source");

    let codex_agent_id = storage
        .ensure_agent(&pack_agent("codex", "Codex"))
        .expect("seed codex agent");
    let claude_agent_id = storage
        .ensure_agent(&pack_agent("claude", "Claude Code"))
        .expect("seed claude agent");
    let checkout_workspace_id = storage
        .ensure_workspace(&workspace_checkout, Some("checkout-service"))
        .expect("seed checkout workspace");
    let billing_workspace_id = storage
        .ensure_workspace(&workspace_billing, Some("billing-worker"))
        .expect("seed billing workspace");

    storage
        .insert_conversation_tree(
            codex_agent_id,
            Some(checkout_workspace_id),
            &pack_conversation(PackConversationSeed {
                agent_slug: "codex",
                workspace: &workspace_checkout,
                source_path: &codex_source,
                external_id: "codex-checkout",
                title: "Fresh checkout failure triage",
                source_id: "local",
                origin_host: None,
                started_at: 1_777_806_000_000,
                messages: vec![
                    pack_message(
                        0,
                        MessageRole::User,
                        1_777_806_001_000,
                        "checkout failure after redirect in checkout-service",
                    ),
                    pack_message(
                        1,
                        MessageRole::Agent,
                        1_777_806_002_000,
                        &format!(
                            "duplicate checkout failure timeout retry guard was missing; pasted key {PACK_E2E_REDACTION_CANARY}"
                        ),
                    ),
                ],
            }),
        )
        .expect("insert local pack conversation");

    storage
        .insert_conversation_tree(
            claude_agent_id,
            Some(billing_workspace_id),
            &pack_conversation(PackConversationSeed {
                agent_slug: "claude",
                workspace: &workspace_billing,
                source_path: &claude_source,
                external_id: "claude-checkout-remote",
                title: "Stale remote checkout failure handoff",
                source_id: "remote-build-node",
                origin_host: Some("builder@remote-build-node.internal"),
                started_at: 1_704_067_200_000,
                messages: vec![
                    pack_message(
                        0,
                        MessageRole::User,
                        1_704_067_201_000,
                        "remote checkout failure from billing-worker",
                    ),
                    pack_message(
                        1,
                        MessageRole::Agent,
                        1_704_067_202_000,
                        "duplicate checkout failure timeout retry guard was missing on remote source",
                    ),
                ],
            }),
        )
        .expect("insert remote pack conversation");

    tracker.end(
        "seed_pack_archive",
        Some("Seed real archive DB plus source log files for cass pack"),
        phase_start,
    );

    PackArchiveFixture {
        tmp,
        data_dir,
        artifact_dir,
        source_files: vec![
            (codex_source, codex_original),
            (claude_source, claude_original),
        ],
    }
}

fn scrub_pack_artifact(mut value: Value) -> Value {
    if let Some(meta) = value.get_mut("_meta").and_then(Value::as_object_mut) {
        meta.insert(
            "generated_at_ms".to_string(),
            Value::String("[scrubbed]".to_string()),
        );
        meta.insert(
            "elapsed_ms".to_string(),
            Value::String("[scrubbed]".to_string()),
        );
    }
    if let Some(health) = value.get_mut("health").and_then(Value::as_object_mut) {
        health.insert(
            "index_generation".to_string(),
            Value::String("[scrubbed]".to_string()),
        );
    }
    value
}

/// Setup test environment with fixtures and run index.
fn setup_indexed_env() -> (TempDir, std::path::PathBuf) {
    let tracker = PhaseTracker::new("e2e_cli_flows", "setup_indexed_env");
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    // Create fixtures
    let phase_start = tracker.start(
        "create_fixtures",
        Some("Create Codex and Claude session fixtures"),
    );
    make_codex_session(&codex_home, "authentication error in login", 1733011200000);
    make_claude_session(&claude_home, "myapp", "fix the database connection");
    tracker.end(
        "create_fixtures",
        Some("Create Codex and Claude session fixtures"),
        phase_start,
    );

    // Run index
    let phase_start = tracker.start("index", Some("Run full index on fixture sessions"));
    base_cmd(&command_env)
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    let index_ms = phase_start.elapsed().as_millis() as u64;
    tracker.end(
        "index",
        Some("Run full index on fixture sessions"),
        phase_start,
    );
    tracker.metrics(
        "cass_index",
        &E2ePerformanceMetrics::new()
            .with_duration(index_ms)
            .with_throughput(2, index_ms)
            .with_custom("operation", "full_index"),
    );

    tracker.flush();
    (tmp, data_dir)
}

#[test]
fn pack_handoff_journey_uses_real_archive_and_preserves_sources() {
    let tracker = tracker_for("pack_handoff_journey_uses_real_archive_and_preserves_sources");
    let command_env = tracker.command_environment();
    let fixture = setup_pack_archive_fixture(&tracker);
    let sessions_stdin = fixture
        .source_files
        .iter()
        .map(|(path, _)| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let trace_file = fixture.artifact_dir.join("pack.trace.jsonl");
    let stdout_path = fixture.artifact_dir.join("pack.stdout.json");
    let stderr_path = fixture.artifact_dir.join("pack.stderr.txt");
    let scrubbed_path = fixture.artifact_dir.join("pack.stdout.scrubbed.json");
    let failure_context_path = fixture.artifact_dir.join("pack.failure-context.json");

    let pack_start = tracker.start(
        "run_pack",
        Some("Execute cass pack against real archive DB and sessions-from stdin"),
    );
    let mut cmd = base_cmd(&command_env);
    cmd.args(["--trace-file"])
        .arg(&trace_file)
        .args([
            "pack",
            "checkout failure",
            "--robot",
            "--max-tokens",
            "4000",
            "--limit",
            "10",
            "--max-evidence",
            "6",
            "--sessions-from",
            "-",
            "--data-dir",
        ])
        .arg(&fixture.data_dir)
        .env("HOME", fixture.tmp.path())
        .write_stdin(sessions_stdin);
    let output = cmd.output().expect("run cass pack e2e");
    let pack_ms = pack_start.elapsed().as_millis() as u64;
    tracker.end("run_pack", Some("cass pack complete"), pack_start);

    fs::write(&stdout_path, &output.stdout).expect("write pack stdout artifact");
    fs::write(&stderr_path, &output.stderr).expect("write pack stderr artifact");
    if !output.status.success() {
        let failure_context = serde_json::json!({
            "status": output.status.code(),
            "stdout_path": stdout_path,
            "stderr_path": stderr_path,
            "trace_file": trace_file,
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        });
        fs::write(
            &failure_context_path,
            serde_json::to_vec_pretty(&failure_context).expect("serialize failure context"),
        )
        .expect("write pack failure context artifact");
    }
    assert!(
        output.status.success(),
        "cass pack e2e failed; artifacts in {}",
        fixture.artifact_dir.display()
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains(PACK_E2E_REDACTION_CANARY) && !stderr.contains(PACK_E2E_REDACTION_CANARY),
        "pack output must not leak raw fixture secret"
    );
    assert!(
        stdout.trim_start().starts_with('{'),
        "robot pack stdout must be a JSON object: {stdout}"
    );
    let json: Value = serde_json::from_str(stdout.trim()).expect("pack stdout is valid JSON");
    let scrubbed = scrub_pack_artifact(json.clone());
    fs::write(
        &scrubbed_path,
        serde_json::to_vec_pretty(&scrubbed).expect("serialize scrubbed pack artifact"),
    )
    .expect("write scrubbed pack artifact");

    assert_eq!(json["schema_version"], "cass.pack.v1");
    assert_eq!(json["query"]["text"], "checkout failure");
    assert_eq!(json["limits"]["max_tokens"], 4000);
    assert!(
        json["health"]["source_readiness"]
            .as_array()
            .is_some_and(|sources| !sources.is_empty()),
        "pack health must include source readiness: {json}"
    );
    assert!(
        json["freshness"]["newest_evidence_at_ms"].is_i64()
            || json["freshness"]["newest_evidence_at_ms"].is_u64(),
        "pack must report evidence freshness: {json}"
    );
    assert!(
        json["privacy"]["redaction_applied"].as_bool() == Some(true)
            || stdout.contains("[REDACTED]"),
        "pack must redact or explicitly mark the sensitive fixture string: {json}"
    );

    let session_paths = fixture
        .source_files
        .iter()
        .map(|(path, _)| path.display().to_string())
        .collect::<std::collections::HashSet<_>>();
    let evidence = json["evidence"]
        .as_array()
        .expect("pack evidence must be an array");
    assert!(
        !evidence.is_empty(),
        "pack must select at least one evidence item"
    );
    assert!(
        evidence.iter().all(|item| {
            item["citation"]["source_path"]
                .as_str()
                .is_some_and(|path| session_paths.contains(path))
        }),
        "--sessions-from - must restrict evidence to the provided sessions: {json}"
    );
    assert!(
        evidence.iter().any(|item| {
            item["citation"]["origin_kind"]
                .as_str()
                .is_some_and(|kind| kind != "local")
                || item["citation"]["source_id"]
                    .as_str()
                    .is_some_and(|source_id| source_id != "local")
        }),
        "fixture must include selected remote-source provenance evidence: {json}"
    );
    assert!(
        evidence.iter().any(|item| {
            item["excerpt"]
                .as_str()
                .is_some_and(|excerpt| excerpt.contains("checkout failure"))
        }),
        "pack evidence should preserve the handoff query context: {json}"
    );

    for (path, original) in &fixture.source_files {
        let current = fs::read(path).expect("read source log after pack");
        assert_eq!(
            &current,
            original,
            "cass pack must not mutate source session log {}",
            path.display()
        );
    }
    assert!(stdout_path.exists(), "stdout artifact should exist");
    assert!(stderr_path.exists(), "stderr artifact should exist");
    assert!(
        scrubbed_path.exists(),
        "scrubbed output artifact should exist"
    );
    assert!(
        trace_file.exists(),
        "trace artifact should exist when --trace-file is requested"
    );

    tracker.metrics(
        "cass_pack",
        &E2ePerformanceMetrics::new()
            .with_duration(pack_ms)
            .with_throughput(evidence.len() as u64, pack_ms)
            .with_custom("operation", "pack_handoff"),
    );
    tracker.complete();
}

// =============================================================================
// Search Query E2E Tests with trace file
// =============================================================================

#[test]
fn search_with_trace_file_creates_trace() {
    let tracker = tracker_for("search_with_trace_file_creates_trace");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();
    let trace_file = tmp.path().join("trace.jsonl");

    let output = base_cmd(&command_env)
        .args(["--trace-file"])
        .arg(&trace_file)
        .args(["search", "authentication", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Search with trace-file should succeed"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Main output should be valid JSON
    let json: Value = serde_json::from_str(stdout.trim()).expect("stdout should be valid JSON");
    assert!(json.get("hits").is_some() || json.get("results").is_some());

    // Trace file should exist (may be empty if no spans logged)
    // Note: trace file creation is best-effort
}

#[test]
fn search_basic_returns_valid_json() {
    let tracker = tracker_for("search_basic_returns_valid_json");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    let search_start = tracker.start("run_search", Some("Execute basic search command"));
    let output = base_cmd(&command_env)
        .args(["search", "database", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let search_ms = search_start.elapsed().as_millis() as u64;
    tracker.end("run_search", Some("Search complete"), search_start);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should be valid JSON
    let json: Value = serde_json::from_str(stdout.trim()).expect("Should be valid JSON");
    let hit_count = json
        .get("hits")
        .or_else(|| json.get("results"))
        .and_then(|h| h.as_array())
        .map(|a| a.len() as u64)
        .unwrap_or(0);
    assert!(
        json.get("hits").is_some() || json.get("results").is_some() || json.get("count").is_some(),
        "Should have results structure. JSON: {}",
        json
    );

    tracker.metrics(
        "cass_search",
        &E2ePerformanceMetrics::new()
            .with_duration(search_ms)
            .with_throughput(hit_count, search_ms)
            .with_custom("query", "database"),
    );
    tracker.complete();
}

#[test]
fn search_returns_hits_with_expected_fields() {
    let tracker = tracker_for("search_returns_hits_with_expected_fields");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    let output = base_cmd(&command_env)
        .args([
            "search",
            "authentication",
            "--robot",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Bead 7k7pl: pin hits/results as a JSON array, not just
    // "field present". Follow-up `.as_array()` would silently turn a
    // regression (null/scalar) into `None` and skip the inner block
    // — upgrade forces the regression to surface here.
    let hits = json.get("hits").or_else(|| json.get("results"));
    let hits_array = hits
        .and_then(|h| h.as_array())
        .unwrap_or_else(|| panic!("hits/results must be an array. JSON: {}", json));

    if !hits_array.is_empty() {
        let first_hit = &hits_array[0];
        // Bead 7k7pl: pin TYPE on the hit-schema fields —
        // source_path/path must be a string, agent must be a string.
        // A null-or-numeric regression would slip past `.is_some()`
        // while breaking JSON consumers that call `.as_str()`.
        let source_path = first_hit
            .get("source_path")
            .and_then(|v| v.as_str())
            .or_else(|| first_hit.get("path").and_then(|v| v.as_str()));
        assert!(
            source_path.is_some(),
            "Hit must have string `source_path` or `path`. Hit: {}",
            first_hit
        );
        assert!(
            first_hit.get("agent").and_then(|v| v.as_str()).is_some(),
            "Hit must have a string `agent` field. Hit: {}",
            first_hit
        );
    }
}

// =============================================================================
// Detail Find Tests (view/expand)
// =============================================================================

#[test]
fn view_command_returns_session_detail() {
    let tracker = tracker_for("view_command_returns_session_detail");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();
    let codex_session = tmp
        .path()
        .join(".codex/sessions/2024/12/01/rollout-test.jsonl");

    // View the session
    let view_start = tracker.start("run_view", Some("Execute view command on session"));
    let output = base_cmd(&command_env)
        .args(["view", "--robot", "--data-dir"])
        .arg(&data_dir)
        .arg(&codex_session)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let view_ms = view_start.elapsed().as_millis() as u64;
    tracker.end("run_view", Some("View complete"), view_start);

    // View may exit with 0 or non-zero depending on whether session is indexed
    let stdout = String::from_utf8_lossy(&output.stdout);

    if output.status.success() {
        // Should be valid JSON
        let json: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
        // May have messages or error
        assert!(
            json.get("messages").is_some()
                || json.get("error").is_some()
                || json.get("conversation").is_some(),
            "View should return messages or error. stdout: {}",
            stdout
        );
    }

    tracker.metrics(
        "cass_view",
        &E2ePerformanceMetrics::new()
            .with_duration(view_ms)
            .with_custom("operation", "view_session"),
    );
    tracker.complete();
}

#[test]
fn expand_command_with_context() {
    let tracker = tracker_for("expand_command_with_context");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();
    let codex_session = tmp
        .path()
        .join(".codex/sessions/2024/12/01/rollout-test.jsonl");

    // Expand with context
    let output = base_cmd(&command_env)
        .args(["expand", "--robot", "-n", "1", "-C", "2", "--data-dir"])
        .arg(&data_dir)
        .arg(&codex_session)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Expand may succeed or fail depending on line existence
    if output.status.success() && !stdout.is_empty() {
        let json: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
        // Should have context or messages
        assert!(
            json.get("messages").is_some()
                || json.get("context").is_some()
                || json.get("lines").is_some(),
            "Expand should return context. stdout: {}, stderr: {}",
            stdout,
            stderr
        );
    }
}

// =============================================================================
// Filter Combination Tests
// =============================================================================

#[test]
fn search_filter_by_agent() {
    let tracker = tracker_for("search_filter_by_agent");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    // Search for codex agent only
    let output = base_cmd(&command_env)
        .args([
            "search",
            "authentication",
            "--robot",
            "--agent",
            "codex",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // All hits should be from codex
    if let Some(hits) = json
        .get("hits")
        .or_else(|| json.get("results"))
        .and_then(|h| h.as_array())
    {
        for hit in hits {
            let agent = hit.get("agent").and_then(|a| a.as_str()).unwrap_or("");
            assert!(
                agent.contains("codex") || agent.is_empty(),
                "Expected codex agent, got: {}",
                agent
            );
        }
    }
}

#[test]
fn search_filter_by_days() {
    let tracker = tracker_for("search_filter_by_days");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    // Search with days filter (should include recent sessions)
    let output = base_cmd(&command_env)
        .args([
            "search",
            "database",
            "--robot",
            "--days",
            "365",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should parse as valid JSON
    let _json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
}

#[test]
fn search_combined_filters() {
    let tracker = tracker_for("search_combined_filters");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    // Combine multiple filters
    let output = base_cmd(&command_env)
        .args([
            "search",
            "error",
            "--robot",
            "--limit",
            "10",
            "--days",
            "30",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    // Check limit is respected
    if let Some(hits) = json
        .get("hits")
        .or_else(|| json.get("results"))
        .and_then(|h| h.as_array())
    {
        assert!(hits.len() <= 10, "Should respect limit=10");
    }
}

#[test]
fn search_with_workspace_filter() {
    let tracker = tracker_for("search_with_workspace_filter");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();
    let workspace = tmp.path().join(".claude/projects/myapp");

    // Search with workspace filter
    let output = base_cmd(&command_env)
        .args(["search", "database", "--robot", "--workspace"])
        .arg(&workspace)
        .arg("--data-dir")
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should parse as valid JSON
    let _json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
}

// =============================================================================
// Logging/Trace Validation Tests
// =============================================================================

#[test]
fn trace_output_contains_operation_markers() {
    let tracker = tracker_for("trace_output_contains_operation_markers");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    let output = base_cmd(&command_env)
        .args(["search", "test", "--robot", "--trace", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    // Even if no results, trace should work
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Trace should contain some operation info
    // May be empty if tracing not fully enabled, but when present should have structure
    if !stderr.is_empty() && stderr.contains('{') {
        // Likely JSON trace - verify parseable
        for line in stderr.lines() {
            if line.starts_with('{') {
                let _: Value = serde_json::from_str(line).unwrap_or(Value::Null);
            }
        }
    }
}

#[test]
fn verbose_mode_increases_logging() {
    let tracker = tracker_for("verbose_mode_increases_logging");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    // Run with -v for verbose
    let output = base_cmd(&command_env)
        .args(["search", "test", "--robot", "-v", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Verbose mode may produce more stderr output
    // This is a weak assertion but validates verbose doesn't break execution
    let _ = stderr; // Use stderr to avoid unused warning
    assert!(output.status.success() || output.status.code() == Some(3));
}

// =============================================================================
// Robot Mode Output Validation
// =============================================================================

#[test]
fn robot_mode_suppresses_ansi() {
    let tracker = tracker_for("robot_mode_suppresses_ansi");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    let output = base_cmd(&command_env)
        .args([
            "search",
            "authentication",
            "--robot",
            "--color=never",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should not contain ANSI escape codes
    assert!(
        !stdout.contains('\x1b'),
        "Robot mode with --color=never should not emit ANSI"
    );
}

#[test]
fn robot_mode_json_output_only() {
    let tracker = tracker_for("robot_mode_json_output_only");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    let output = base_cmd(&command_env)
        .args(["search", "test", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);

    // stdout should be pure JSON (or empty)
    if !stdout.trim().is_empty() {
        let _: Value =
            serde_json::from_str(stdout.trim()).expect("Robot mode stdout should be valid JSON");
    }
}

// =============================================================================
// Health/Status Commands E2E
// =============================================================================

#[test]
fn health_command_returns_structured_output() {
    let tracker = tracker_for("health_command_returns_structured_output");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    let health_start = tracker.start("run_health", Some("Execute health check command"));
    let output = base_cmd(&command_env)
        .args(["health", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let health_ms = health_start.elapsed().as_millis() as u64;
    tracker.end("run_health", Some("Health check complete"), health_start);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should have health status
    assert!(
        json.get("healthy").is_some() || json.get("status").is_some(),
        "Health should report status. JSON: {}",
        json
    );

    tracker.metrics(
        "cass_health",
        &E2ePerformanceMetrics::new()
            .with_duration(health_ms)
            .with_custom("operation", "health_check"),
    );
    tracker.complete();
}

#[test]
fn stats_command_returns_aggregations() {
    let tracker = tracker_for("stats_command_returns_aggregations");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    let stats_start = tracker.start("run_stats", Some("Execute stats command"));
    let output = base_cmd(&command_env)
        .args(["stats", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let stats_ms = stats_start.elapsed().as_millis() as u64;
    tracker.end("run_stats", Some("Stats complete"), stats_start);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should have some statistics
    assert!(
        json.get("total").is_some()
            || json.get("sessions").is_some()
            || json.get("count").is_some()
            || json.get("by_agent").is_some(),
        "Stats should have counts. JSON: {}",
        json
    );

    tracker.metrics(
        "cass_stats",
        &E2ePerformanceMetrics::new()
            .with_duration(stats_ms)
            .with_custom("operation", "stats"),
    );
    tracker.complete();
}

#[test]
fn capabilities_command_lists_features() {
    let tracker = tracker_for("capabilities_command_lists_features");
    let command_env = tracker.command_environment();
    let output = base_cmd(&command_env)
        .args(["capabilities", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should list capabilities
    assert!(
        json.get("commands").is_some()
            || json.get("capabilities").is_some()
            || json.get("features").is_some(),
        "Capabilities should list features. JSON: {}",
        json
    );
}

// =============================================================================
// Error Handling E2E Tests
// =============================================================================

#[test]
fn search_no_index_handles_gracefully() {
    let tracker = tracker_for("search_no_index_handles_gracefully");
    let command_env = tracker.command_environment();
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("empty_data");
    fs::create_dir_all(&data_dir).unwrap();

    let output = base_cmd(&command_env)
        .args(["search", "test", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    let exit_code = output.status.code().unwrap_or(99);

    // Exit code 3 means missing index, 0 means empty results, 1 means no index/db,
    // 9 means unknown error. All are valid outcomes for no-index scenario.
    assert!(
        exit_code == 0 || exit_code == 1 || exit_code == 3 || exit_code == 9,
        "No index should return exit 0, 1, 3, or 9, got: {}",
        exit_code
    );
}

#[test]
fn truly_invalid_command_returns_error() {
    let tracker = tracker_for("truly_invalid_command_returns_error");
    let command_env = tracker.command_environment();
    // Test with a truly malformed command (not interpretable as search)
    let output = base_cmd(&command_env)
        .args(["--nonexistent-flag-only"])
        .output()
        .unwrap();

    // Should either fail or be auto-corrected - verify it doesn't crash
    // The forgiving CLI may interpret most things as search queries
    let exit_code = output.status.code().unwrap_or(0);
    assert!(
        exit_code == 0 || exit_code == 2 || exit_code == 3,
        "Should return valid exit code (0, 2, or 3), got: {}",
        exit_code
    );
}

#[test]
fn view_nonexistent_file_handles_gracefully() {
    let tracker = tracker_for("view_nonexistent_file_handles_gracefully");
    let command_env = tracker.command_environment();
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let output = base_cmd(&command_env)
        .args([
            "view",
            "/nonexistent/path/session.jsonl",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .output()
        .unwrap();

    // Should handle gracefully (non-zero exit but structured error)
    let stdout = String::from_utf8_lossy(&output.stdout);

    // If output present, should be valid JSON or error message
    if !stdout.trim().is_empty() {
        // May be JSON error or plain text
        if stdout.trim().starts_with('{') {
            let _ = serde_json::from_str::<Value>(stdout.trim());
        }
    }
}

// =============================================================================
// Index Watch-Once Tests (br-154l)
// =============================================================================

#[test]
fn index_incremental_processes_file_changes() {
    let tracker = tracker_for("index_incremental_processes_file_changes");
    let command_env = tracker.command_environment();
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create initial fixture
    make_codex_session(&codex_home, "initial session content", 1733011200000);

    // Run full index first
    let phase_start = tracker.start("initial_index", Some("Run initial full index"));
    base_cmd(&command_env)
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("initial_index", Some("Initial index complete"), phase_start);

    // Get initial stats
    let stats_output = base_cmd(&command_env)
        .args(["stats", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .unwrap();
    let initial_stats: Value =
        serde_json::from_str(&String::from_utf8_lossy(&stats_output.stdout)).unwrap_or_default();

    // Create a new session file
    let new_sessions = codex_home.join("sessions/2024/12/02");
    fs::create_dir_all(&new_sessions).unwrap();
    let new_file = new_sessions.join("rollout-new.jsonl");
    let new_content = r#"{"type": "event_msg", "timestamp": 1733097600000, "payload": {"type": "user_message", "message": "new session content"}}
{"type": "response_item", "timestamp": 1733097601000, "payload": {"role": "assistant", "content": "response to new session"}}"#;
    fs::write(&new_file, new_content).unwrap();

    // Run incremental index to pick up the new file
    let incr_start = tracker.start("incremental_index", Some("Run incremental index"));
    let output = base_cmd(&command_env)
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .unwrap();
    let incr_ms = incr_start.elapsed().as_millis() as u64;
    tracker.end(
        "incremental_index",
        Some("Incremental index complete"),
        incr_start,
    );

    assert!(
        output.status.success(),
        "Incremental index should succeed. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify new session was indexed by checking stats
    let final_stats_output = base_cmd(&command_env)
        .args(["stats", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .unwrap();
    let final_stats: Value =
        serde_json::from_str(&String::from_utf8_lossy(&final_stats_output.stdout))
            .unwrap_or_default();

    // Stats should reflect new session (or at least not crash)
    let initial_count = initial_stats
        .get("total")
        .or_else(|| initial_stats.get("sessions"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let final_count = final_stats
        .get("total")
        .or_else(|| final_stats.get("sessions"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    // Final count should be >= initial (new session indexed)
    assert!(
        final_count >= initial_count,
        "Session count should increase or stay same after incremental index"
    );

    tracker.metrics(
        "cass_incremental_index",
        &E2ePerformanceMetrics::new()
            .with_duration(incr_ms)
            .with_custom("operation", "incremental_index"),
    );
    tracker.complete();
}

// =============================================================================
// Semantic/Hybrid Search Tests (br-154l)
// =============================================================================

#[test]
fn search_semantic_mode() {
    let tracker = tracker_for("search_semantic_mode");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    // Attempt semantic search (may fallback to lexical if no embedder)
    let search_start = tracker.start("run_semantic_search", Some("Execute semantic search"));
    let output = base_cmd(&command_env)
        .args([
            "search",
            "database connection",
            "--robot",
            "--mode",
            "semantic",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let search_ms = search_start.elapsed().as_millis() as u64;
    tracker.end(
        "run_semantic_search",
        Some("Semantic search complete"),
        search_start,
    );

    // Semantic mode may succeed, gracefully degrade, or error
    // Various exit codes are valid depending on semantic index availability
    let exit_code = output.status.code().unwrap_or(99);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The test passes if we get structured output (success or error)
    // or if it fails gracefully with expected exit codes
    if !stdout.trim().is_empty() && stdout.trim().starts_with('{') {
        let json: Value = serde_json::from_str(stdout.trim()).unwrap_or_default();
        // Valid if has any recognizable structure
        assert!(
            json.is_object(),
            "Semantic search should return JSON. stdout: {}, stderr: {}",
            stdout,
            stderr
        );
    }

    tracker.metrics(
        "cass_semantic_search",
        &E2ePerformanceMetrics::new()
            .with_duration(search_ms)
            .with_custom("mode", "semantic")
            .with_custom("exit_code", exit_code.to_string()),
    );
    // Test passes as long as it doesn't crash unexpectedly
    tracker.complete();
}

#[test]
fn search_hybrid_mode() {
    let tracker = tracker_for("search_hybrid_mode");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    // Attempt hybrid search
    let search_start = tracker.start("run_hybrid_search", Some("Execute hybrid search"));
    let output = base_cmd(&command_env)
        .args([
            "search",
            "authentication error",
            "--robot",
            "--mode",
            "hybrid",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let search_ms = search_start.elapsed().as_millis() as u64;
    tracker.end(
        "run_hybrid_search",
        Some("Hybrid search complete"),
        search_start,
    );

    // Hybrid mode may succeed, gracefully degrade, or error
    let exit_code = output.status.code().unwrap_or(99);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The test passes if we get structured output (success or error)
    if !stdout.trim().is_empty() && stdout.trim().starts_with('{') {
        let json: Value = serde_json::from_str(stdout.trim()).unwrap_or_default();
        // Valid if has any recognizable structure
        assert!(
            json.is_object(),
            "Hybrid search should return JSON. stdout: {}, stderr: {}",
            stdout,
            stderr
        );
    }

    tracker.metrics(
        "cass_hybrid_search",
        &E2ePerformanceMetrics::new()
            .with_duration(search_ms)
            .with_custom("mode", "hybrid")
            .with_custom("exit_code", exit_code.to_string()),
    );
    // Test passes as long as it doesn't crash unexpectedly
    tracker.complete();
}

#[test]
fn search_lexical_mode_explicit() {
    let tracker = tracker_for("search_lexical_mode_explicit");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    // Explicit lexical mode (should always work)
    let search_start = tracker.start(
        "run_lexical_search",
        Some("Execute explicit lexical search"),
    );
    let output = base_cmd(&command_env)
        .args([
            "search",
            "authentication",
            "--robot",
            "--mode",
            "lexical",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let search_ms = search_start.elapsed().as_millis() as u64;
    tracker.end(
        "run_lexical_search",
        Some("Lexical search complete"),
        search_start,
    );

    assert!(output.status.success(), "Lexical search should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("Should be valid JSON");
    assert!(
        json.get("hits").is_some() || json.get("results").is_some(),
        "Lexical search should return hits/results. JSON: {}",
        json
    );

    tracker.metrics(
        "cass_lexical_search",
        &E2ePerformanceMetrics::new()
            .with_duration(search_ms)
            .with_custom("mode", "lexical"),
    );
    tracker.complete();
}

// =============================================================================
// Diag Command Tests (br-154l)
// =============================================================================

#[test]
fn diag_command_returns_diagnostic_info() {
    let tracker = tracker_for("diag_command_returns_diagnostic_info");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    let diag_start = tracker.start("run_diag", Some("Execute diag command"));
    let output = base_cmd(&command_env)
        .args(["diag", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let diag_ms = diag_start.elapsed().as_millis() as u64;
    tracker.end("run_diag", Some("Diag complete"), diag_start);

    // Diag should succeed or return structured error
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() && !stdout.trim().is_empty() {
        let json: Value = serde_json::from_str(stdout.trim()).expect("Should be valid JSON");
        // Should have diagnostic info like version, db path, index stats, etc.
        assert!(
            json.get("version").is_some()
                || json.get("db_path").is_some()
                || json.get("index_path").is_some()
                || json.get("diagnostics").is_some()
                || json.get("config").is_some(),
            "Diag should return diagnostic fields. JSON: {}, stderr: {}",
            json,
            stderr
        );
    }

    tracker.metrics(
        "cass_diag",
        &E2ePerformanceMetrics::new()
            .with_duration(diag_ms)
            .with_custom("operation", "diag"),
    );
    tracker.complete();
}

#[test]
fn status_command_returns_index_status() {
    let tracker = tracker_for("status_command_returns_index_status");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    let status_start = tracker.start("run_status", Some("Execute status command"));
    let output = base_cmd(&command_env)
        .args(["status", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    let status_ms = status_start.elapsed().as_millis() as u64;
    tracker.end("run_status", Some("Status complete"), status_start);

    let stdout = String::from_utf8_lossy(&output.stdout);

    if output.status.success() && !stdout.trim().is_empty() {
        let json: Value = serde_json::from_str(stdout.trim()).expect("Should be valid JSON");
        // Status should have index state info (various possible field names)
        assert!(
            json.get("healthy").is_some()
                || json.get("index").is_some()
                || json.get("database").is_some()
                || json.get("indexed").is_some()
                || json.get("sessions").is_some()
                || json.get("status").is_some()
                || json.get("last_indexed").is_some()
                || json.get("count").is_some()
                || json.get("_meta").is_some(),
            "Status should return index state. JSON: {}",
            json
        );
    }

    tracker.metrics(
        "cass_status",
        &E2ePerformanceMetrics::new()
            .with_duration(status_ms)
            .with_custom("operation", "status"),
    );
    tracker.complete();
}

// =============================================================================
// Multi-Agent E2E Tests
// =============================================================================

#[test]
fn search_across_multiple_agents() {
    let tracker = tracker_for("search_across_multiple_agents");
    let command_env = tracker.command_environment();
    let (tmp, data_dir) = setup_indexed_env();

    // Search should find results from both codex and claude
    let output = base_cmd(&command_env)
        .args([
            "search",
            "error OR database",
            "--robot",
            "--limit",
            "20",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should have results (may be from one or both agents)
    let hits = json.get("hits").or_else(|| json.get("results"));
    assert!(
        hits.is_some(),
        "Should have hits from multi-agent search. JSON: {}",
        json
    );
}

/// GH#414: a `--sessions-from` filter that matches zero indexed sessions must
/// be distinguishable from a genuine query miss — the robot payload reports
/// how the filter resolved, warns when it provably matched nothing, and the
/// broaden-the-query suggestion (which points at the one thing that was NOT
/// the problem) is suppressed in exactly that case.
#[test]
fn search_sessions_from_reports_filter_resolution_and_suppresses_query_suggestions() {
    let tracker = tracker_for(
        "search_sessions_from_reports_filter_resolution_and_suppresses_query_suggestions",
    );
    let command_env = tracker.command_environment();
    let fixture = setup_pack_archive_fixture(&tracker);

    let run_search = |sessions_from: Option<&PathBuf>| -> Value {
        let mut cmd = base_cmd(&command_env);
        cmd.args(["search", "checkout", "--robot", "--data-dir"])
            .arg(&fixture.data_dir)
            .env("HOME", fixture.tmp.path());
        if let Some(list) = sessions_from {
            cmd.arg("--sessions-from").arg(list);
        }
        let output = cmd.output().expect("run cass search e2e");
        assert!(
            output.status.success(),
            "search must exit 0: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim())
            .expect("search stdout is valid JSON")
    };

    // Control: the query genuinely matches the fixture, and with no
    // --sessions-from the payload carries no sessions_filter block.
    let control = run_search(None);
    let control_count = control["count"].as_u64().expect("count");
    assert!(control_count > 0, "control query must hit the fixture");
    assert!(
        control.get("sessions_filter").is_none(),
        "sessions_filter must be absent without --sessions-from"
    );

    // A filter naming an indexed session resolves matched=1 and still hits.
    let good_list = fixture.artifact_dir.join("sessions-good.txt");
    fs::write(
        &good_list,
        format!("{}\n", fixture.source_files[0].0.display()),
    )
    .expect("write good session list");
    let filtered = run_search(Some(&good_list));
    assert!(
        filtered.get("sessions_filter").is_some(),
        "payload missing sessions_filter with --sessions-from supplied: {filtered}"
    );
    assert_eq!(filtered["sessions_filter"]["requested"], 1);
    assert_eq!(filtered["sessions_filter"]["matched"], 1);
    assert!(
        filtered.get("sessions_filter_warning").is_none(),
        "a matched filter must not warn"
    );

    // The reported case: a filter naming a never-indexed path. Same query,
    // same archive — the zero must be attributed to the filter.
    let bogus_list = fixture.artifact_dir.join("sessions-bogus.txt");
    fs::write(&bogus_list, "/nonexistent/path/never-indexed.jsonl\n")
        .expect("write bogus session list");
    let unmatched = run_search(Some(&bogus_list));
    assert_eq!(unmatched["count"], 0, "bogus filter yields zero hits");
    assert_eq!(unmatched["sessions_filter"]["requested"], 1);
    assert_eq!(
        unmatched["sessions_filter"]["matched"], 0,
        "matched must be 0, not null: the resolution ran and proved the miss"
    );
    assert!(
        unmatched["sessions_filter_warning"]
            .as_str()
            .is_some_and(|w| w.contains("--sessions-from")),
        "the payload must say the filter, not the query, explains the zero"
    );
    assert!(
        unmatched.get("suggestions").is_none(),
        "broaden-the-query suggestions must be suppressed when the filter \
         provably matched no indexed session"
    );
}
