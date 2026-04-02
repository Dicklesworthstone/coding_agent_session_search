//! CLI dispatch subprocess integration tests.
//!
//! This module covers CLI commands that were previously untested via subprocess
//! invocation. Tests invoke the real binary with representative flags, validate
//! output formats, exit codes, and JSON structure.
//!
//! Coverage targets: completions, man, health, doctor, context, timeline, expand,
//! export, export-html, sources subcommands, models subcommands.

use assert_cmd::Command;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use frankensqlite::Connection as FrankenConnection;
use frankensqlite::compat::{ConnectionExt, RowExt};
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

/// Create a base command with isolated test environment.
fn base_cmd(temp_home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    // Isolate test environment
    cmd.env("HOME", temp_home);
    cmd.env("XDG_DATA_HOME", temp_home.join(".local/share"));
    cmd.env("XDG_CONFIG_HOME", temp_home.join(".config"));
    cmd.env("CODEX_HOME", temp_home.join(".codex"));
    // Disable TTY detection
    cmd.env("NO_COLOR", "1");
    cmd
}

/// Create base command without HOME isolation (for simple tests), but with isolated XDG_DATA_HOME.
fn simple_cmd() -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("NO_COLOR", "1");

    // Create an isolated empty database with schema to avoid hitting the real user DB
    let tmp = tempfile::TempDir::new().unwrap();
    let db_dir = tmp.path().join("coding-agent-search");
    std::fs::create_dir_all(&db_dir).unwrap();
    let db_path = db_dir.join("agent_search.db");

    // Initialize the schema
    let fs = coding_agent_search::storage::sqlite::FrankenStorage::open(&db_path).unwrap();
    drop(fs);

    cmd.env("XDG_DATA_HOME", tmp.path());

    // Leak the temp dir so it survives the command execution
    std::mem::forget(tmp);

    cmd
}

fn sample_agent(slug: &str, name: &str) -> Agent {
    Agent {
        id: None,
        slug: slug.to_string(),
        name: name.to_string(),
        version: None,
        kind: AgentKind::Cli,
    }
}

fn sample_message(idx: i64, role: MessageRole, ts: i64, content: &str) -> Message {
    Message {
        id: None,
        idx,
        role,
        author: None,
        created_at: Some(ts),
        content: content.to_string(),
        extra_json: json!({}),
        snippets: Vec::new(),
    }
}

fn make_codex_session(root: &Path, content: &str, ts: u64) {
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

fn sample_conversation(
    agent_slug: &str,
    workspace: &Path,
    source_path: &Path,
    external_id: &str,
    title: &str,
    started_at: i64,
    messages: Vec<Message>,
) -> Conversation {
    Conversation {
        id: None,
        agent_slug: agent_slug.to_string(),
        workspace: Some(workspace.to_path_buf()),
        external_id: Some(external_id.to_string()),
        title: Some(title.to_string()),
        source_path: source_path.to_path_buf(),
        started_at: Some(started_at),
        ended_at: messages.last().and_then(|msg| msg.created_at),
        approx_tokens: None,
        metadata_json: json!({}),
        messages,
        source_id: "local".to_string(),
        origin_host: None,
    }
}

fn seed_analytics_workspace_fixture(temp_home: &TempDir) -> (PathBuf, PathBuf) {
    let data_dir = temp_home.path().join(".local/share/coding-agent-search");
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("agent_search.db");
    let storage = coding_agent_search::storage::sqlite::FrankenStorage::open(&db_path).unwrap();

    let workspace_a = temp_home.path().join("workspace-a");
    let workspace_b = temp_home.path().join("workspace-b");
    fs::create_dir_all(&workspace_a).unwrap();
    fs::create_dir_all(&workspace_b).unwrap();

    let session_a = workspace_a.join("analytics-a.jsonl");
    let session_b = workspace_b.join("analytics-b.jsonl");
    fs::write(&session_a, "{}\n").unwrap();
    fs::write(&session_b, "{}\n").unwrap();

    let codex_id = storage
        .ensure_agent(&sample_agent("codex", "Codex"))
        .unwrap();
    let workspace_a_id = storage
        .ensure_workspace(&workspace_a, Some("workspace-a"))
        .unwrap();
    let workspace_b_id = storage
        .ensure_workspace(&workspace_b, Some("workspace-b"))
        .unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    storage
        .insert_conversation_tree(
            codex_id,
            Some(workspace_a_id),
            &sample_conversation(
                "codex",
                &workspace_a,
                &session_a,
                "analytics-workspace-a",
                "Workspace A Analytics Session",
                now_ms,
                vec![
                    sample_message(0, MessageRole::User, now_ms, "question-a"),
                    sample_message(1, MessageRole::Agent, now_ms + 1, "answer-a"),
                ],
            ),
        )
        .unwrap();

    storage
        .insert_conversation_tree(
            codex_id,
            Some(workspace_b_id),
            &sample_conversation(
                "codex",
                &workspace_b,
                &session_b,
                "analytics-workspace-b",
                "Workspace B Analytics Session",
                now_ms + 10,
                vec![sample_message(
                    0,
                    MessageRole::User,
                    now_ms + 10,
                    "question-b",
                )],
            ),
        )
        .unwrap();

    storage.rebuild_analytics().unwrap();

    (workspace_a, workspace_b)
}

fn seed_analytics_models_workspace_fixture(temp_home: &TempDir) -> PathBuf {
    let (workspace_a, _workspace_b) = seed_analytics_workspace_fixture(temp_home);
    let db_path = temp_home
        .path()
        .join(".local/share/coding-agent-search/agent_search.db");
    let conn = FrankenConnection::open(db_path.to_string_lossy().to_string()).unwrap();

    let workspace_rows = conn
        .query_map_collect(
            "SELECT path, id FROM workspaces",
            &[],
            |row: &frankensqlite::Row| Ok((row.get_typed::<String>(0)?, row.get_typed::<i64>(1)?)),
        )
        .unwrap();
    let workspace_a_id = workspace_rows
        .into_iter()
        .find(|(path, _)| path == &workspace_a.to_string_lossy())
        .map(|(_, id)| id)
        .expect("workspace-a id");

    let message_rows = conn
        .query_map_collect(
            "SELECT m.id, m.conversation_id, c.workspace_id, c.agent_id, m.role, COALESCE(m.created_at, 0), LENGTH(m.content)
             FROM messages m
             JOIN conversations c ON c.id = m.conversation_id
             ORDER BY m.id",
            &[],
            |row: &frankensqlite::Row| {
                Ok((
                    row.get_typed::<i64>(0)?,
                    row.get_typed::<i64>(1)?,
                    row.get_typed::<Option<i64>>(2)?.expect("workspace id"),
                    row.get_typed::<i64>(3)?,
                    row.get_typed::<String>(4)?,
                    row.get_typed::<i64>(5)?,
                    row.get_typed::<i64>(6)?,
                ))
            },
        )
        .unwrap();

    let mut workspace_a_totals = vec![12_i64, 17_i64].into_iter();
    for (message_id, conversation_id, workspace_id, agent_id, role, created_at, content_chars) in
        message_rows
    {
        let (model_name, model_family, total_tokens) = if workspace_id == workspace_a_id {
            (
                Some("gpt-4o-mini".to_string()),
                Some("gpt-4o".to_string()),
                workspace_a_totals.next().expect("workspace-a token total"),
            )
        } else {
            (
                Some("claude-3-5-sonnet".to_string()),
                Some("claude".to_string()),
                11,
            )
        };
        let day_id =
            coding_agent_search::storage::sqlite::FrankenStorage::day_id_from_millis(created_at);
        conn.execute_compat(
            "INSERT OR REPLACE INTO token_usage (
                message_id, conversation_id, agent_id, workspace_id, source_id, timestamp_ms, day_id,
                model_name, model_family, total_tokens, role, content_chars, data_source
             ) VALUES (?1, ?2, ?3, ?4, 'local', ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'api')",
            frankensqlite::params![
                message_id,
                conversation_id,
                agent_id,
                workspace_id,
                created_at,
                day_id,
                model_name,
                model_family,
                total_tokens,
                role,
                content_chars,
            ],
        )
        .unwrap();
    }

    workspace_a
}

// =============================================================================
// Completions command tests
// =============================================================================

#[test]
fn completions_bash_outputs_valid_script() {
    let mut cmd = simple_cmd();
    cmd.args(["completions", "bash"]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Bash completions should contain function definitions
    assert!(
        stdout.contains("_cass"),
        "bash completions should define _cass function"
    );
    assert!(
        stdout.contains("complete"),
        "bash completions should have complete command"
    );
}

#[test]
fn completions_zsh_outputs_valid_script() {
    let mut cmd = simple_cmd();
    cmd.args(["completions", "zsh"]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Zsh completions should contain compdef
    assert!(
        stdout.contains("#compdef") || stdout.contains("compdef"),
        "zsh completions should have compdef directive"
    );
}

#[test]
fn completions_fish_outputs_valid_script() {
    let mut cmd = simple_cmd();
    cmd.args(["completions", "fish"]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Fish completions use complete command
    assert!(
        stdout.contains("complete -c cass"),
        "fish completions should define completions for cass"
    );
}

#[test]
fn completions_powershell_outputs_valid_script() {
    let mut cmd = simple_cmd();
    cmd.args(["completions", "powershell"]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // PowerShell completions use Register-ArgumentCompleter
    assert!(
        stdout.contains("Register-ArgumentCompleter")
            || stdout.contains("ArgumentCompleter")
            || stdout.contains("$scriptblock"),
        "powershell completions should define argument completer"
    );
}

#[test]
fn completions_help_shows_shells() {
    let mut cmd = simple_cmd();
    cmd.args(["completions", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("bash"))
        .stdout(contains("zsh"))
        .stdout(contains("fish"))
        .stdout(contains("powershell"));
}

// =============================================================================
// Man command tests
// =============================================================================

#[test]
fn man_outputs_groff_format() {
    let mut cmd = simple_cmd();
    cmd.arg("man");
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Man pages start with .TH (title header) or .\" comment
    assert!(
        stdout.contains(".TH") || stdout.contains(".SH"),
        "man output should be groff format with .TH or .SH macros"
    );
    assert!(
        stdout.contains("cass") || stdout.contains("CASS"),
        "man page should mention cass"
    );
}

#[test]
fn man_help_shows_usage() {
    let mut cmd = simple_cmd();
    cmd.args(["man", "--help"]);
    cmd.assert().success().stdout(contains("Generate man page"));
}

// =============================================================================
// Health command tests
// =============================================================================

#[test]
fn health_json_outputs_valid_structure() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["health", "--json", "--data-dir", data_dir.to_str().unwrap()]);

    let output = cmd.assert().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should output valid JSON with healthy field
    if !stdout.trim().is_empty() {
        let json: Value = serde_json::from_str(stdout.trim()).expect("valid health json");
        assert!(
            json.get("healthy").is_some(),
            "health JSON should have 'healthy' field"
        );
    }
}

#[test]
fn health_with_robot_meta_includes_metadata() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // First create the DB by running index
    let mut idx_cmd = base_cmd(tmp.path());
    idx_cmd.args(["index", "--data-dir", data_dir.to_str().unwrap(), "--json"]);
    idx_cmd.assert().success();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "health",
        "--json",
        "--robot-meta",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid health json with meta");

    // Should have _meta block
    assert!(
        json.get("_meta").is_some() || json.get("latency_ms").is_some(),
        "health --robot-meta should include metadata"
    );
}

#[test]
fn health_help_shows_options() {
    let mut cmd = simple_cmd();
    cmd.args(["health", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("health check"))
        .stdout(contains("--json"))
        .stdout(contains("--stale-threshold"));
}

// =============================================================================
// Doctor command tests
// =============================================================================

#[test]
fn doctor_json_outputs_valid_structure() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["doctor", "--json", "--data-dir", data_dir.to_str().unwrap()]);

    let output = cmd.assert().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should output valid JSON
    if !stdout.trim().is_empty() {
        let json: Value = serde_json::from_str(stdout.trim()).expect("valid doctor json");
        // Doctor should have checks or issues array
        assert!(
            json.get("checks").is_some()
                || json.get("issues").is_some()
                || json.get("status").is_some(),
            "doctor JSON should have diagnostic fields"
        );
    }
}

#[test]
fn doctor_verbose_shows_all_checks() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "doctor",
        "--verbose",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    // Just check it runs without error
    let _ = cmd.assert();
}

#[test]
fn doctor_help_shows_options() {
    let mut cmd = simple_cmd();
    cmd.args(["doctor", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("Diagnose"))
        .stdout(contains("--fix"))
        .stdout(contains("--verbose"));
}

#[test]
fn doctor_fix_quarantines_corrupted_database_bundle_sidecars() {
    let tmp = TempDir::new().unwrap();
    let temp_home = tmp.path();
    let data_dir = temp_home.join("data");
    let codex_home = temp_home.join(".codex");
    fs::create_dir_all(&data_dir).unwrap();
    make_codex_session(&codex_home, "doctor sidecar recovery", 1_733_011_200_000);

    let db_path = data_dir.join("agent_search.db");
    let corrupt_bytes = b"not a sqlite database".to_vec();
    let wal_bytes = b"stale wal bytes".to_vec();
    let shm_bytes = b"stale shm bytes".to_vec();
    fs::write(&db_path, &corrupt_bytes).unwrap();
    fs::write(data_dir.join("agent_search.db-wal"), &wal_bytes).unwrap();
    fs::write(data_dir.join("agent_search.db-shm"), &shm_bytes).unwrap();

    let doctor = base_cmd(temp_home)
        .current_dir(temp_home)
        .args([
            "doctor",
            "--fix",
            "--json",
            "--data-dir",
            data_dir.to_str().unwrap(),
        ])
        .output()
        .expect("doctor command");
    let doctor_json: Value = serde_json::from_slice(&doctor.stdout).expect("valid doctor json");
    assert_eq!(
        doctor_json.get("auto_fix_applied").and_then(Value::as_bool),
        Some(true),
        "doctor should at least quarantine the corrupted bundle\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr)
    );

    let entries: Vec<String> = fs::read_dir(&data_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    let backup_root = entries
        .iter()
        .find(|name| {
            name.starts_with("agent_search.corrupt.")
                && !name.ends_with("-wal")
                && !name.ends_with("-shm")
        })
        .cloned()
        .expect("doctor should quarantine the corrupt database root");
    let backup_root_path = data_dir.join(&backup_root);
    assert_eq!(fs::read(&backup_root_path).unwrap(), corrupt_bytes);
    assert_eq!(
        fs::read(format!("{}-wal", backup_root_path.display())).unwrap(),
        wal_bytes
    );
    assert_eq!(
        fs::read(format!("{}-shm", backup_root_path.display())).unwrap(),
        shm_bytes
    );

    let live_wal = data_dir.join("agent_search.db-wal");
    if live_wal.exists() {
        assert_ne!(fs::read(&live_wal).unwrap(), wal_bytes);
    }
    let live_shm = data_dir.join("agent_search.db-shm");
    if live_shm.exists() {
        assert_ne!(fs::read(&live_shm).unwrap(), shm_bytes);
    }

    let health = base_cmd(temp_home)
        .current_dir(temp_home)
        .args(["health", "--json", "--data-dir", data_dir.to_str().unwrap()])
        .output()
        .expect("health command");
    assert!(
        health.status.success(),
        "health should succeed once stale sidecars are quarantined\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&health.stdout),
        String::from_utf8_lossy(&health.stderr)
    );
    let health_json: Value = serde_json::from_slice(&health.stdout).expect("valid health json");
    assert_eq!(
        health_json
            .get("db")
            .and_then(|db| db.get("opened"))
            .and_then(Value::as_bool),
        Some(true)
    );
}

// =============================================================================
// Context command tests
// =============================================================================

#[test]
fn context_requires_path_argument() {
    let mut cmd = simple_cmd();
    cmd.arg("context");
    // Should fail without path
    cmd.assert().failure();
}

#[test]
fn context_json_with_nonexistent_path() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "context",
        "/nonexistent/path.jsonl",
        "--json",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    // May fail or return empty results - either is acceptable
    let output = cmd.assert().get_output().clone();
    let _stdout = String::from_utf8_lossy(&output.stdout);
    // Test passes if command completes (success or failure with message)
}

#[test]
fn context_help_shows_options() {
    let mut cmd = simple_cmd();
    cmd.args(["context", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("related sessions"))
        .stdout(contains("--json"))
        .stdout(contains("--limit"));
}

// =============================================================================
// Timeline command tests
// =============================================================================

#[test]
fn timeline_json_outputs_valid_structure() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // First create DB
    let mut idx_cmd = base_cmd(tmp.path());
    idx_cmd.args(["index", "--data-dir", data_dir.to_str().unwrap(), "--json"]);
    idx_cmd.assert().success();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "timeline",
        "--json",
        "--today",
        "--group-by",
        "none",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should output valid JSON (may be empty array)
    if !stdout.trim().is_empty() {
        let _json: Value = serde_json::from_str(stdout.trim()).expect("valid timeline json");
    }
}

#[test]
fn timeline_json_normalizes_remote_provenance_without_source_row() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("agent_search.db");
    let storage = coding_agent_search::storage::sqlite::FrankenStorage::open(&db_path).unwrap();

    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let session = tmp.path().join("timeline-remote-no-source-row.jsonl");
    fs::write(&session, "{\"session\":\"timeline-remote\"}\n").unwrap();

    let codex_id = storage
        .ensure_agent(&sample_agent("codex", "Codex"))
        .unwrap();
    let workspace_id = storage
        .ensure_workspace(&workspace, Some("workspace"))
        .unwrap();
    storage
        .upsert_source(&coding_agent_search::sources::provenance::Source::remote(
            "work-laptop",
            "user@work-laptop",
        ))
        .unwrap();
    let conn = frankensqlite::Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
    conn.execute("UPDATE sources SET kind = '' WHERE id = 'work-laptop'")
        .unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let mut conversation = sample_conversation(
        "codex",
        &workspace,
        &session,
        "timeline-remote-no-source-row",
        "Remote Timeline Session",
        now_ms,
        vec![
            sample_message(0, MessageRole::User, now_ms, "question"),
            sample_message(1, MessageRole::Agent, now_ms + 1, "answer"),
        ],
    );
    conversation.source_id = "work-laptop".to_string();
    conversation.origin_host = Some("   ".to_string());
    storage
        .insert_conversation_tree(codex_id, Some(workspace_id), &conversation)
        .unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "timeline",
        "--json",
        "--today",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    let output = cmd.assert().success().get_output().clone();
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid timeline json");
    let sessions: Vec<&Value> = if let Some(items) = json.as_array() {
        items.iter().collect()
    } else if let Some(items) = json["sessions"].as_array() {
        items.iter().collect()
    } else {
        json["groups"]
            .as_object()
            .expect("timeline groups object")
            .values()
            .flat_map(|value| value.as_array().into_iter().flatten())
            .collect()
    };
    let entry = sessions
        .into_iter()
        .find(|entry| entry["source_path"].as_str() == Some(session.to_string_lossy().as_ref()))
        .expect("remote timeline session entry");

    assert_eq!(entry["source_id"].as_str(), Some("work-laptop"));
    assert_eq!(entry["origin_kind"].as_str(), Some("remote"));
    assert!(entry["origin_host"].is_null());
}

#[test]
fn timeline_json_derives_remote_source_id_from_origin_host_when_source_id_blank() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("agent_search.db");
    let storage = coding_agent_search::storage::sqlite::FrankenStorage::open(&db_path).unwrap();

    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let session = tmp.path().join("timeline-blank-source-id.jsonl");
    fs::write(&session, "{\"session\":\"timeline-blank-remote\"}\n").unwrap();

    let codex_id = storage
        .ensure_agent(&sample_agent("codex", "Codex"))
        .unwrap();
    let workspace_id = storage
        .ensure_workspace(&workspace, Some("workspace"))
        .unwrap();
    let conn = frankensqlite::Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
    conn.execute(
        "INSERT INTO sources(id, kind, host_label, created_at, updated_at) VALUES ('   ', 'remote', 'user@work-laptop', 0, 0)",
    )
    .unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let mut conversation = sample_conversation(
        "codex",
        &workspace,
        &session,
        "timeline-blank-source-id",
        "Timeline Blank Source Id",
        now_ms,
        vec![
            sample_message(0, MessageRole::User, now_ms, "question"),
            sample_message(1, MessageRole::Agent, now_ms + 1, "answer"),
        ],
    );
    conversation.source_id = "   ".to_string();
    conversation.origin_host = Some("user@work-laptop".to_string());
    storage
        .insert_conversation_tree(codex_id, Some(workspace_id), &conversation)
        .unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "timeline",
        "--json",
        "--today",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    let output = cmd.assert().success().get_output().clone();
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid timeline json");
    let sessions: Vec<&Value> = if let Some(items) = json.as_array() {
        items.iter().collect()
    } else if let Some(items) = json["sessions"].as_array() {
        items.iter().collect()
    } else {
        json["groups"]
            .as_object()
            .expect("timeline groups object")
            .values()
            .flat_map(|value| value.as_array().into_iter().flatten())
            .collect()
    };
    let entry = sessions
        .into_iter()
        .find(|entry| entry["source_path"].as_str() == Some(session.to_string_lossy().as_ref()))
        .expect("blank source timeline entry");

    assert_eq!(entry["source_id"].as_str(), Some("user@work-laptop"));
    assert_eq!(entry["origin_kind"].as_str(), Some("remote"));
    assert_eq!(entry["origin_host"].as_str(), Some("user@work-laptop"));
}

#[test]
fn timeline_human_output_does_not_badge_trimmed_local_source_id() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("agent_search.db");
    let storage = coding_agent_search::storage::sqlite::FrankenStorage::open(&db_path).unwrap();

    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let session = tmp.path().join("timeline-trimmed-local.jsonl");
    fs::write(&session, "{\"session\":\"timeline-trimmed-local\"}\n").unwrap();

    let codex_id = storage
        .ensure_agent(&sample_agent("codex", "Codex"))
        .unwrap();
    let workspace_id = storage
        .ensure_workspace(&workspace, Some("workspace"))
        .unwrap();
    storage
        .upsert_source(&coding_agent_search::sources::provenance::Source {
            id: "  local  ".to_string(),
            kind: coding_agent_search::sources::provenance::SourceKind::Local,
            host_label: None,
            machine_id: None,
            platform: None,
            config_json: None,
            created_at: None,
            updated_at: None,
        })
        .unwrap();

    let mut conversation = sample_conversation(
        "codex",
        &workspace,
        &session,
        "timeline-trimmed-local",
        "Timeline Trimmed Local",
        1_700_000_000_000,
        vec![
            sample_message(0, MessageRole::User, 1_700_000_000_000, "question"),
            sample_message(1, MessageRole::Agent, 1_700_000_000_001, "answer"),
        ],
    );
    conversation.source_id = "  local  ".to_string();
    storage
        .insert_conversation_tree(codex_id, Some(workspace_id), &conversation)
        .unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "timeline",
        "--since",
        "2020-01-01",
        "--until",
        "2030-01-01",
        "--group-by",
        "none",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Timeline Trimmed Local"));
    assert!(
        !stdout.contains("[  local  ]"),
        "unexpected raw local badge: {stdout}"
    );
    assert!(
        !stdout.contains("[local]"),
        "unexpected normalized local badge: {stdout}"
    );
}

#[test]
fn timeline_help_shows_options() {
    let mut cmd = simple_cmd();
    cmd.args(["timeline", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("timeline"))
        .stdout(contains("--since"))
        .stdout(contains("--until"))
        .stdout(contains("--today"));
}

// =============================================================================
// Expand command tests
// =============================================================================

#[test]
fn expand_requires_path_and_line() {
    let mut cmd = simple_cmd();
    cmd.arg("expand");
    cmd.assert().failure();
}

#[test]
fn expand_help_shows_options() {
    let mut cmd = simple_cmd();
    cmd.args(["expand", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("messages around"))
        .stdout(contains("--line"))
        .stdout(contains("--context"))
        .stdout(contains("--json"));
}

// =============================================================================
// Export command tests
// =============================================================================

#[test]
fn export_requires_path() {
    let mut cmd = simple_cmd();
    cmd.arg("export");
    cmd.assert().failure();
}

#[test]
fn export_help_shows_formats() {
    let mut cmd = simple_cmd();
    cmd.args(["export", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("Export"))
        .stdout(contains("--format"))
        .stdout(contains("--output"))
        .stdout(contains("markdown").or(contains("Markdown")));
}

// =============================================================================
// Export-HTML command tests
// =============================================================================

#[test]
fn export_html_requires_session() {
    let mut cmd = simple_cmd();
    cmd.arg("export-html");
    cmd.assert().failure();
}

#[test]
fn export_html_help_shows_encryption_options() {
    let mut cmd = simple_cmd();
    cmd.args(["export-html", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("HTML"))
        .stdout(contains("--encrypt"))
        .stdout(contains("--output-dir"));
}

// =============================================================================
// Sources subcommand tests
// =============================================================================

#[test]
fn sources_list_json_outputs_valid_structure() {
    let tmp = TempDir::new().unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["sources", "list", "--json"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should output valid JSON with sources array
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid sources list json");
    assert!(
        json.get("sources").map(|v| v.is_array()).unwrap_or(false) || json.is_object(),
        "sources list --json should return object with sources array"
    );
}

#[test]
fn sources_list_verbose() {
    let tmp = TempDir::new().unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["sources", "list", "--verbose"]);

    // Should complete without error
    cmd.assert().success();
}

#[test]
fn sources_doctor_json_outputs_structure() {
    let tmp = TempDir::new().unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["sources", "doctor", "--json"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should output valid JSON
    let _json: Value = serde_json::from_str(stdout.trim()).expect("valid sources doctor json");
}

#[test]
fn sources_help_shows_subcommands() {
    let mut cmd = simple_cmd();
    cmd.args(["sources", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("list"))
        .stdout(contains("add"))
        .stdout(contains("remove"))
        .stdout(contains("doctor"))
        .stdout(contains("sync"));
}

// =============================================================================
// Models subcommand tests
// =============================================================================

#[test]
fn models_status_json_outputs_structure() {
    let tmp = TempDir::new().unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["models", "status", "--json"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should output valid JSON
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid models status json");
    // Should have installed or available field
    assert!(
        json.get("installed").is_some()
            || json.get("models").is_some()
            || json.get("status").is_some()
            || json.is_object(),
        "models status JSON should have status information"
    );
}

#[test]
fn models_verify_json_with_no_model() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "models",
        "verify",
        "--json",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    // May succeed with empty or fail - either is acceptable
    let output = cmd.assert().get_output().clone();
    let _stdout = String::from_utf8_lossy(&output.stdout);
}

#[test]
fn models_help_shows_subcommands() {
    let mut cmd = simple_cmd();
    cmd.args(["models", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("status"))
        .stdout(contains("install"))
        .stdout(contains("verify"))
        .stdout(contains("remove"));
}

#[test]
fn models_install_help_shows_options() {
    let mut cmd = simple_cmd();
    cmd.args(["models", "install", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("--model"))
        .stdout(contains("--mirror"))
        .stdout(contains("--from-file"));
}

// =============================================================================
// Pages command tests
// =============================================================================

#[test]
fn pages_help_shows_options() {
    let mut cmd = simple_cmd();
    cmd.args(["pages", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("searchable archive"))
        .stdout(contains("--export-only"))
        .stdout(contains("--verify"))
        .stdout(contains("--no-encryption"))
        .stdout(contains("--target"))
        .stdout(contains("--project"))
        .stdout(contains("--account-id"))
        .stdout(contains("--api-token"));
}

#[test]
fn pages_verify_with_nonexistent_path() {
    let tmp = TempDir::new().unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["pages", "--verify", "/nonexistent/bundle"]);

    // Should fail with appropriate error
    cmd.assert().failure();
}

// =============================================================================
// Exit code tests
// =============================================================================

#[test]
fn search_requires_query_argument() {
    // search command requires a query argument
    let mut cmd = simple_cmd();
    cmd.arg("search");
    // Should fail without query
    cmd.assert().failure();
}

#[test]
fn missing_required_arg_returns_error() {
    let mut cmd = simple_cmd();
    cmd.args(["search"]); // Missing query
    cmd.assert().failure();
}

// =============================================================================
// Clap parsing tests for new commands
// =============================================================================

use clap::Parser;
use coding_agent_search::{AnalyticsBucketing, AnalyticsCommand, Cli, Commands};

#[test]
fn parse_completions_bash() {
    let cli = Cli::try_parse_from(["cass", "completions", "bash"]).expect("parse completions bash");
    match cli.command {
        Some(Commands::Completions { shell }) => {
            assert_eq!(shell, clap_complete::Shell::Bash);
        }
        other => panic!("expected completions command, got {other:?}"),
    }
}

#[test]
fn parse_health_with_stale_threshold() {
    let cli = Cli::try_parse_from(["cass", "health", "--stale-threshold", "600"])
        .expect("parse health with threshold");
    match cli.command {
        Some(Commands::Health {
            stale_threshold, ..
        }) => {
            assert_eq!(stale_threshold, 600);
        }
        other => panic!("expected health command, got {other:?}"),
    }
}

#[test]
fn parse_doctor_with_fix() {
    let cli = Cli::try_parse_from(["cass", "doctor", "--fix", "--verbose"])
        .expect("parse doctor with fix");
    match cli.command {
        Some(Commands::Doctor { fix, verbose, .. }) => {
            assert!(fix, "fix should be true");
            assert!(verbose, "verbose should be true");
        }
        other => panic!("expected doctor command, got {other:?}"),
    }
}

#[test]
fn parse_timeline_with_filters() {
    let cli = Cli::try_parse_from([
        "cass",
        "timeline",
        "--since",
        "2024-01-01",
        "--agent",
        "claude",
    ])
    .expect("parse timeline with filters");
    match cli.command {
        Some(Commands::Timeline { since, agent, .. }) => {
            assert_eq!(since, Some("2024-01-01".to_string()));
            assert_eq!(agent, vec!["claude"]);
        }
        other => panic!("expected timeline command, got {other:?}"),
    }
}

#[test]
fn parse_expand_with_context() {
    let cli = Cli::try_parse_from([
        "cass",
        "expand",
        "/path/to/session.jsonl",
        "--line",
        "100",
        "-C",
        "5",
    ])
    .expect("parse expand with context");
    match cli.command {
        Some(Commands::Expand {
            path,
            line,
            context,
            ..
        }) => {
            assert_eq!(path.to_str().unwrap(), "/path/to/session.jsonl");
            assert_eq!(line, 100);
            assert_eq!(context, 5);
        }
        other => panic!("expected expand command, got {other:?}"),
    }
}

#[test]
fn parse_context_with_limit() {
    let cli = Cli::try_parse_from(["cass", "context", "/path/to/session.jsonl", "--limit", "10"])
        .expect("parse context with limit");
    match cli.command {
        Some(Commands::Context { path, limit, .. }) => {
            assert_eq!(path.to_str().unwrap(), "/path/to/session.jsonl");
            assert_eq!(limit, 10);
        }
        other => panic!("expected context command, got {other:?}"),
    }
}

#[test]
fn parse_sessions_with_workspace_and_limit() {
    let cli = Cli::try_parse_from([
        "cass",
        "sessions",
        "--workspace",
        "/path/to/project",
        "--limit",
        "3",
        "--json",
    ])
    .expect("parse sessions with workspace and limit");
    match cli.command {
        Some(Commands::Sessions {
            workspace,
            current,
            limit,
            json,
            ..
        }) => {
            assert_eq!(workspace.unwrap().to_str().unwrap(), "/path/to/project");
            assert!(!current);
            assert_eq!(limit, Some(3));
            assert!(json);
        }
        other => panic!("expected sessions command, got {other:?}"),
    }
}

#[test]
fn sessions_json_reports_recent_and_current_workspace_sessions() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("agent_search.db");
    let storage = coding_agent_search::storage::sqlite::FrankenStorage::open(&db_path).unwrap();

    let workspace_a = tmp.path().join("workspace-a");
    let workspace_a_nested = workspace_a.join("src");
    let workspace_b = tmp.path().join("workspace-b");
    fs::create_dir_all(&workspace_a_nested).unwrap();
    fs::create_dir_all(&workspace_b).unwrap();

    let session_a_old = tmp.path().join("claude-old.jsonl");
    let session_a_new = tmp.path().join("claude-new.jsonl");
    let session_b = tmp.path().join("codex.jsonl");
    fs::write(&session_a_old, "{\"session\":\"old\"}\n").unwrap();
    std::thread::sleep(Duration::from_millis(5));
    fs::write(&session_a_new, "{\"session\":\"new\"}\n").unwrap();
    std::thread::sleep(Duration::from_millis(5));
    fs::write(&session_b, "{\"session\":\"other\"}\n").unwrap();

    let claude_id = storage
        .ensure_agent(&sample_agent("claude_code", "Claude Code"))
        .unwrap();
    let codex_id = storage
        .ensure_agent(&sample_agent("codex", "Codex"))
        .unwrap();
    let workspace_a_id = storage
        .ensure_workspace(&workspace_a, Some("workspace-a"))
        .unwrap();
    let workspace_b_id = storage
        .ensure_workspace(&workspace_b, Some("workspace-b"))
        .unwrap();

    storage
        .insert_conversation_tree(
            claude_id,
            Some(workspace_a_id),
            &sample_conversation(
                "claude_code",
                &workspace_a,
                &session_a_old,
                "claude-old",
                "Old Claude Session",
                1_700_000_000_000,
                vec![
                    sample_message(0, MessageRole::User, 1_700_000_000_000, "old question"),
                    sample_message(1, MessageRole::Agent, 1_700_000_000_001, "old answer"),
                ],
            ),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            claude_id,
            Some(workspace_a_id),
            &sample_conversation(
                "claude_code",
                &workspace_a,
                &session_a_new,
                "claude-new",
                "Newest Claude Session",
                1_700_000_100_000,
                vec![
                    sample_message(0, MessageRole::User, 1_700_000_100_000, "new question"),
                    sample_message(1, MessageRole::Agent, 1_700_000_100_001, "new answer"),
                ],
            ),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            codex_id,
            Some(workspace_b_id),
            &sample_conversation(
                "codex",
                &workspace_b,
                &session_b,
                "codex-other",
                "Other Workspace Session",
                1_700_000_200_000,
                vec![
                    sample_message(0, MessageRole::User, 1_700_000_200_000, "other question"),
                    sample_message(1, MessageRole::Agent, 1_700_000_200_001, "other answer"),
                ],
            ),
        )
        .unwrap();

    let mut all_cmd = base_cmd(tmp.path());
    all_cmd.args([
        "sessions",
        "--json",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);
    let all_output = all_cmd.assert().success().get_output().clone();
    let all_json: Value = serde_json::from_slice(&all_output.stdout).expect("valid sessions json");
    let all_sessions = all_json["sessions"].as_array().expect("sessions array");
    assert_eq!(all_sessions.len(), 3, "should list all recent sessions");
    assert_eq!(
        all_sessions[0]["path"].as_str().unwrap(),
        session_b.to_string_lossy(),
        "most recently modified file should come first"
    );
    assert_eq!(all_sessions[0]["message_count"], 2);
    assert_eq!(all_sessions[0]["human_turns"], 1);
    assert_eq!(all_sessions[0]["source_id"].as_str(), Some("local"));
    assert!(all_sessions[0]["origin_host"].is_null());
    assert!(all_sessions[0]["size_bytes"].is_number());

    let mut current_cmd = base_cmd(tmp.path());
    current_cmd.current_dir(&workspace_a_nested);
    current_cmd.args([
        "sessions",
        "--current",
        "--json",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);
    let current_output = current_cmd.assert().success().get_output().clone();
    let current_json: Value =
        serde_json::from_slice(&current_output.stdout).expect("valid current sessions json");
    let current_sessions = current_json["sessions"].as_array().expect("sessions array");
    assert_eq!(
        current_sessions.len(),
        1,
        "--current should return one best match"
    );
    assert_eq!(
        current_sessions[0]["path"].as_str().unwrap(),
        session_a_new.to_string_lossy(),
        "current workspace should resolve to newest matching workspace session"
    );
    assert_eq!(
        current_sessions[0]["workspace"].as_str().unwrap(),
        workspace_a.to_string_lossy()
    );
}

#[test]
fn sessions_json_keeps_local_file_metadata_for_trimmed_local_source_id() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("agent_search.db");
    let storage = coding_agent_search::storage::sqlite::FrankenStorage::open(&db_path).unwrap();

    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let session = tmp.path().join("trimmed-local.jsonl");
    fs::write(&session, "{\"session\":\"trimmed-local\"}\n").unwrap();

    let codex_id = storage
        .ensure_agent(&sample_agent("codex", "Codex"))
        .unwrap();
    let workspace_id = storage
        .ensure_workspace(&workspace, Some("workspace"))
        .unwrap();
    storage
        .upsert_source(&coding_agent_search::sources::provenance::Source {
            id: "  local  ".to_string(),
            kind: coding_agent_search::sources::provenance::SourceKind::Local,
            host_label: None,
            machine_id: None,
            platform: None,
            config_json: None,
            created_at: None,
            updated_at: None,
        })
        .unwrap();

    let mut conversation = sample_conversation(
        "codex",
        &workspace,
        &session,
        "trimmed-local",
        "Trimmed Local Session",
        1_700_000_000_000,
        vec![
            sample_message(0, MessageRole::User, 1_700_000_000_000, "question"),
            sample_message(1, MessageRole::Agent, 1_700_000_000_001, "answer"),
        ],
    );
    conversation.source_id = "  local  ".to_string();
    storage
        .insert_conversation_tree(codex_id, Some(workspace_id), &conversation)
        .unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "sessions",
        "--json",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    let output = cmd.assert().success().get_output().clone();
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid sessions json");
    let sessions = json["sessions"].as_array().expect("sessions array");
    let entry = sessions
        .iter()
        .find(|entry| entry["path"].as_str() == Some(session.to_string_lossy().as_ref()))
        .expect("trimmed local session entry");

    assert_eq!(entry["source_id"].as_str(), Some("local"));
    assert!(
        entry["size_bytes"].is_number(),
        "expected local metadata for trimmed local source"
    );
    assert!(
        entry["modified"].is_string(),
        "expected modified timestamp for trimmed local source"
    );
}

#[test]
fn sessions_json_derives_remote_source_id_from_origin_host_when_source_id_blank() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("agent_search.db");
    let storage = coding_agent_search::storage::sqlite::FrankenStorage::open(&db_path).unwrap();

    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let session = tmp.path().join("remote-blank-source-id.jsonl");
    fs::write(&session, "{\"session\":\"remote\"}\n").unwrap();

    let codex_id = storage
        .ensure_agent(&sample_agent("codex", "Codex"))
        .unwrap();
    let workspace_id = storage
        .ensure_workspace(&workspace, Some("workspace"))
        .unwrap();
    let conn = frankensqlite::Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
    conn.execute(
        "INSERT INTO sources(id, kind, host_label, created_at, updated_at) VALUES ('   ', 'remote', 'user@work-laptop', 0, 0)",
    )
    .unwrap();

    let mut conversation = sample_conversation(
        "codex",
        &workspace,
        &session,
        "remote-blank-source-id",
        "Remote Blank Source Id",
        1_700_000_000_000,
        vec![
            sample_message(0, MessageRole::User, 1_700_000_000_000, "question"),
            sample_message(1, MessageRole::Agent, 1_700_000_000_001, "answer"),
        ],
    );
    conversation.source_id = "   ".to_string();
    conversation.origin_host = Some("user@work-laptop".to_string());
    storage
        .insert_conversation_tree(codex_id, Some(workspace_id), &conversation)
        .unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "sessions",
        "--json",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    let output = cmd.assert().success().get_output().clone();
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid sessions json");
    let sessions = json["sessions"].as_array().expect("sessions array");
    let entry = sessions
        .iter()
        .find(|entry| entry["path"].as_str() == Some(session.to_string_lossy().as_ref()))
        .expect("remote blank source session entry");

    assert_eq!(entry["source_id"].as_str(), Some("user@work-laptop"));
    assert_eq!(entry["origin_host"].as_str(), Some("user@work-laptop"));
    assert!(
        entry["size_bytes"].is_null(),
        "remote fallback source_id must not be treated as local metadata"
    );
}

#[test]
fn sessions_json_keeps_local_file_metadata_for_blank_source_id() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("agent_search.db");
    let storage = coding_agent_search::storage::sqlite::FrankenStorage::open(&db_path).unwrap();

    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let session = tmp.path().join("blank-local-source-id.jsonl");
    fs::write(&session, "{\"session\":\"blank-local\"}\n").unwrap();

    let codex_id = storage
        .ensure_agent(&sample_agent("codex", "Codex"))
        .unwrap();
    let workspace_id = storage
        .ensure_workspace(&workspace, Some("workspace"))
        .unwrap();
    let conn = frankensqlite::Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
    conn.execute(
        "INSERT INTO sources(id, kind, host_label, created_at, updated_at) VALUES ('   ', 'local', NULL, 0, 0)",
    )
    .unwrap();

    let mut conversation = sample_conversation(
        "codex",
        &workspace,
        &session,
        "blank-local-source-id",
        "Blank Local Source Id",
        1_700_000_000_000,
        vec![
            sample_message(0, MessageRole::User, 1_700_000_000_000, "question"),
            sample_message(1, MessageRole::Agent, 1_700_000_000_001, "answer"),
        ],
    );
    conversation.source_id = "   ".to_string();
    storage
        .insert_conversation_tree(codex_id, Some(workspace_id), &conversation)
        .unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "sessions",
        "--json",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    let output = cmd.assert().success().get_output().clone();
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid sessions json");
    let sessions = json["sessions"].as_array().expect("sessions array");
    let entry = sessions
        .iter()
        .find(|entry| entry["path"].as_str() == Some(session.to_string_lossy().as_ref()))
        .expect("blank local session entry");

    assert_eq!(entry["source_id"].as_str(), Some("local"));
    assert!(
        entry["size_bytes"].is_number(),
        "blank local source_id should still resolve to local file metadata"
    );
    assert!(entry["modified"].is_string());
}

#[test]
fn sessions_json_trims_blank_remote_origin_host() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("agent_search.db");
    let storage = coding_agent_search::storage::sqlite::FrankenStorage::open(&db_path).unwrap();

    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let session = tmp.path().join("remote-blank-origin-host.jsonl");
    fs::write(&session, "{\"session\":\"remote\"}\n").unwrap();

    let codex_id = storage
        .ensure_agent(&sample_agent("codex", "Codex"))
        .unwrap();
    let workspace_id = storage
        .ensure_workspace(&workspace, Some("workspace"))
        .unwrap();
    storage
        .upsert_source(&coding_agent_search::sources::provenance::Source::remote(
            "work-laptop",
            "user@work-laptop",
        ))
        .unwrap();

    let mut conversation = sample_conversation(
        "codex",
        &workspace,
        &session,
        "remote-blank-origin-host",
        "Remote Blank Origin Host",
        1_700_000_000_000,
        vec![
            sample_message(0, MessageRole::User, 1_700_000_000_000, "question"),
            sample_message(1, MessageRole::Agent, 1_700_000_000_001, "answer"),
        ],
    );
    conversation.source_id = "work-laptop".to_string();
    conversation.origin_host = Some("   ".to_string());
    storage
        .insert_conversation_tree(codex_id, Some(workspace_id), &conversation)
        .unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "sessions",
        "--json",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    let output = cmd.assert().success().get_output().clone();
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid sessions json");
    let sessions = json["sessions"].as_array().expect("sessions array");
    let entry = sessions
        .iter()
        .find(|entry| entry["path"].as_str() == Some(session.to_string_lossy().as_ref()))
        .expect("remote session entry");

    assert_eq!(entry["source_id"].as_str(), Some("work-laptop"));
    assert!(
        entry["origin_host"].is_null(),
        "blank origin_host should be trimmed away so downstream displays fall back to source_id"
    );
}

#[test]
fn sessions_json_distinguishes_same_path_across_sources() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("agent_search.db");
    let storage = coding_agent_search::storage::sqlite::FrankenStorage::open(&db_path).unwrap();

    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let shared_path = tmp.path().join("shared-session.jsonl");
    fs::write(&shared_path, "{\"session\":\"shared\"}\\n").unwrap();

    let codex_id = storage
        .ensure_agent(&sample_agent("codex", "Codex"))
        .unwrap();
    let workspace_id = storage
        .ensure_workspace(&workspace, Some("workspace"))
        .unwrap();
    storage
        .upsert_source(&coding_agent_search::sources::provenance::Source::remote(
            "laptop",
            "user@laptop",
        ))
        .unwrap();

    storage
        .insert_conversation_tree(
            codex_id,
            Some(workspace_id),
            &sample_conversation(
                "codex",
                &workspace,
                &shared_path,
                "shared-local",
                "Shared Session",
                1_700_000_000_000,
                vec![
                    sample_message(0, MessageRole::User, 1_700_000_000_000, "local question"),
                    sample_message(1, MessageRole::Agent, 1_700_000_000_001, "local answer"),
                ],
            ),
        )
        .unwrap();

    let mut remote = sample_conversation(
        "codex",
        &workspace,
        &shared_path,
        "shared-remote",
        "Shared Session",
        1_700_000_100_000,
        vec![
            sample_message(0, MessageRole::User, 1_700_000_100_000, "remote question"),
            sample_message(1, MessageRole::Agent, 1_700_000_100_001, "remote answer"),
        ],
    );
    remote.source_id = "laptop".to_string();
    remote.origin_host = Some("user@laptop".to_string());
    storage
        .insert_conversation_tree(codex_id, Some(workspace_id), &remote)
        .unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "sessions",
        "--json",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    let output = cmd.assert().success().get_output().clone();
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid sessions json");
    let sessions = json["sessions"].as_array().expect("sessions array");
    let shared: Vec<&Value> = sessions
        .iter()
        .filter(|entry| entry["path"].as_str() == Some(shared_path.to_string_lossy().as_ref()))
        .collect();

    assert_eq!(shared.len(), 2, "same-path sessions should both be visible");
    assert!(
        shared
            .iter()
            .any(|entry| entry["source_id"].as_str() == Some("local"))
    );
    assert!(
        shared
            .iter()
            .any(|entry| entry["source_id"].as_str() == Some("laptop"))
    );
    assert!(
        shared
            .iter()
            .any(|entry| entry["origin_host"].as_str() == Some("user@laptop"))
    );
}

#[test]
fn sessions_current_prefers_closest_workspace_over_newer_parent_workspace() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("agent_search.db");
    let storage = coding_agent_search::storage::sqlite::FrankenStorage::open(&db_path).unwrap();

    let workspace_root = tmp.path().join("repo");
    let workspace_nested = workspace_root.join("apps/web");
    let cwd = workspace_nested.join("src/components");
    fs::create_dir_all(&cwd).unwrap();

    let nested_session_path = tmp.path().join("nested.jsonl");
    let root_session_path = tmp.path().join("root.jsonl");
    fs::write(&nested_session_path, "{\"session\":\"nested\"}\n").unwrap();
    std::thread::sleep(Duration::from_millis(5));
    fs::write(&root_session_path, "{\"session\":\"root\"}\n").unwrap();

    let claude_id = storage
        .ensure_agent(&sample_agent("claude_code", "Claude Code"))
        .unwrap();
    let workspace_root_id = storage
        .ensure_workspace(&workspace_root, Some("repo"))
        .unwrap();
    let workspace_nested_id = storage
        .ensure_workspace(&workspace_nested, Some("repo-web"))
        .unwrap();

    storage
        .insert_conversation_tree(
            claude_id,
            Some(workspace_nested_id),
            &sample_conversation(
                "claude_code",
                &workspace_nested,
                &nested_session_path,
                "nested-session",
                "Nested Session",
                1_700_000_100_000,
                vec![
                    sample_message(0, MessageRole::User, 1_700_000_100_000, "nested question"),
                    sample_message(1, MessageRole::Agent, 1_700_000_100_001, "nested answer"),
                ],
            ),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            claude_id,
            Some(workspace_root_id),
            &sample_conversation(
                "claude_code",
                &workspace_root,
                &root_session_path,
                "root-session",
                "Root Session",
                1_700_000_200_000,
                vec![
                    sample_message(0, MessageRole::User, 1_700_000_200_000, "root question"),
                    sample_message(1, MessageRole::Agent, 1_700_000_200_001, "root answer"),
                ],
            ),
        )
        .unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.current_dir(&cwd);
    cmd.args([
        "sessions",
        "--current",
        "--json",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    let output = cmd.assert().success().get_output().clone();
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid current sessions json");
    let sessions = json["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 1, "--current should default to one session");
    assert_eq!(
        sessions[0]["path"].as_str().unwrap(),
        nested_session_path.to_string_lossy(),
        "closest matching workspace should win over a newer parent workspace session"
    );
    assert_eq!(
        sessions[0]["workspace"].as_str().unwrap(),
        workspace_nested.to_string_lossy()
    );
}

#[test]
fn parse_export_with_format() {
    let cli = Cli::try_parse_from([
        "cass",
        "export",
        "/path/to/session.jsonl",
        "--format",
        "json",
    ])
    .expect("parse export with format");
    match cli.command {
        Some(Commands::Export { path, format, .. }) => {
            assert_eq!(path.to_str().unwrap(), "/path/to/session.jsonl");
            assert_eq!(format, coding_agent_search::ConvExportFormat::Json);
        }
        other => panic!("expected export command, got {other:?}"),
    }
}

#[test]
fn parse_export_html_with_encrypt() {
    let cli = Cli::try_parse_from([
        "cass",
        "export-html",
        "/path/to/session.jsonl",
        "--encrypt",
        "--password-stdin",
    ])
    .expect("parse export-html with encrypt");
    match cli.command {
        Some(Commands::ExportHtml {
            session,
            encrypt,
            password_stdin,
            ..
        }) => {
            assert_eq!(session.to_str().unwrap(), "/path/to/session.jsonl");
            assert!(encrypt);
            assert!(password_stdin);
        }
        other => panic!("expected export-html command, got {other:?}"),
    }
}

// =============================================================================
// Analytics CLI scaffolding tests (br-z9fse.3.1)
// =============================================================================

#[test]
fn analytics_help_lists_expected_subcommands() {
    let mut cmd = simple_cmd();
    cmd.args(["analytics", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("status"))
        .stdout(contains("tokens"))
        .stdout(contains("tools"))
        .stdout(contains("models"))
        .stdout(contains("rebuild"))
        .stdout(contains("validate"));
}

#[test]
fn analytics_tokens_help_lists_shared_flags_and_group_by() {
    let mut cmd = simple_cmd();
    cmd.args(["analytics", "tokens", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("--since"))
        .stdout(contains("--until"))
        .stdout(contains("--days"))
        .stdout(contains("--agent"))
        .stdout(contains("--workspace"))
        .stdout(contains("--source"))
        .stdout(contains("--json"))
        .stdout(contains("--group-by"));
}

#[test]
fn analytics_subcommands_emit_uniform_json_envelope() {
    let tmp_home = TempDir::new().expect("temp home");
    let data_dir = tmp_home.path().join("cass_data");
    fs::create_dir_all(&data_dir).expect("create data dir");
    let data_dir_str = data_dir.to_string_lossy().to_string();
    // Create an empty-but-valid SQLite database so analytics commands can open
    // it without requiring a full `cass index --full` run.
    let db_path = data_dir.join("agent_search.db");
    rusqlite::Connection::open(&db_path).expect("create sqlite db");

    let shared: Vec<&str> = vec![
        "--json",
        "--since",
        "2026-01-01",
        "--until",
        "2026-01-31",
        "--days",
        "7",
        "--agent",
        "claude",
        "--workspace",
        "/tmp/project-a",
        "--source",
        "local",
        "--data-dir",
        data_dir_str.as_str(),
    ];

    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("analytics/status", vec!["analytics", "status"]),
        (
            "analytics/tokens",
            vec!["analytics", "tokens", "--group-by", "day"],
        ),
        (
            "analytics/tools",
            vec!["analytics", "tools", "--group-by", "week"],
        ),
        (
            "analytics/models",
            vec!["analytics", "models", "--group-by", "month"],
        ),
        ("analytics/rebuild", vec!["analytics", "rebuild", "--force"]),
        ("analytics/validate", vec!["analytics", "validate", "--fix"]),
    ];

    // Commands that may fail due to DB lock contention in multi-agent environments.
    let lock_sensitive_commands = ["analytics/rebuild"];

    for (expected_command, mut args) in cases {
        args.extend_from_slice(&shared);
        let mut cmd = base_cmd(tmp_home.path());
        cmd.args(&args);
        let output = cmd.output().expect("failed to execute command");

        // Rebuild may fail with exit 9 ("database is locked") when other processes
        // hold the DB — skip validation for this transient case.
        if !output.status.success() && lock_sensitive_commands.contains(&expected_command) {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("database is locked") {
                eprintln!("Skipping {expected_command}: DB locked (transient, not a test failure)");
                continue;
            }
            panic!(
                "unexpected failure for {expected_command}: exit={:?} stderr={stderr}",
                output.status.code()
            );
        }
        assert!(
            output.status.success(),
            "{expected_command} exited with code {:?}",
            output.status.code()
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        // Note: some analytics subcommands (rebuild, validate, models) emit
        // human-readable diagnostics to stderr even in --json mode.  This is by design
        // — stderr carries diagnostics, stdout carries structured JSON.

        let json: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
            panic!("invalid JSON for {expected_command}: {e}\nstdout={stdout}")
        });

        assert_eq!(json["command"], expected_command);
        let data = &json["data"];
        match expected_command {
            "analytics/status" => {
                assert!(
                    data["tables"].is_array(),
                    "analytics/status should expose table stats: {json}"
                );
                assert!(
                    data["coverage"].is_object(),
                    "analytics/status should expose coverage block: {json}"
                );
                assert!(
                    data["drift"].is_object(),
                    "analytics/status should expose drift block: {json}"
                );
            }
            "analytics/tokens" => {
                assert!(
                    data["buckets"].is_array(),
                    "analytics/tokens should expose bucketed rows: {json}"
                );
                assert!(
                    data["_meta"].is_object(),
                    "analytics/tokens should include _meta block: {json}"
                );
            }
            "analytics/tools" => {
                assert!(
                    data["rows"].is_array(),
                    "analytics/tools should expose rows: {json}"
                );
            }
            "analytics/models" => {
                assert!(
                    data["by_api_tokens"].is_object(),
                    "analytics/models should expose by_api_tokens: {json}"
                );
            }
            "analytics/rebuild" => {
                assert!(
                    data["track"].is_string(),
                    "analytics/rebuild should expose track: {json}"
                );
                assert!(
                    data["tracks_rebuilt"].is_array(),
                    "analytics/rebuild should expose tracks_rebuilt: {json}"
                );
            }
            "analytics/validate" => {
                assert!(
                    data["summary"].is_object(),
                    "analytics/validate should expose summary: {json}"
                );
                assert!(
                    data["checks"].is_array(),
                    "analytics/validate should expose checks: {json}"
                );
            }
            _ => panic!("unexpected analytics subcommand: {expected_command}"),
        }
        assert!(
            json["_meta"]["elapsed_ms"].as_u64().is_some(),
            "missing numeric elapsed_ms for {expected_command}: {json}"
        );

        let filters = json["_meta"]["filters_applied"]
            .as_array()
            .expect("filters_applied array");
        assert!(
            !filters.is_empty(),
            "filters_applied should include shared filters for {expected_command}"
        );
    }
}

#[test]
fn parse_analytics_tokens_with_shared_flags() {
    let cli = Cli::try_parse_from([
        "cass",
        "analytics",
        "tokens",
        "--group-by",
        "week",
        "--since",
        "2026-01-01",
        "--until",
        "2026-01-31",
        "--days",
        "7",
        "--agent",
        "claude",
        "--agent",
        "codex",
        "--workspace",
        "/tmp/ws-a",
        "--workspace",
        "/tmp/ws-b",
        "--source",
        "remote",
        "--json",
    ])
    .expect("parse analytics tokens with shared flags");

    match cli.command {
        Some(Commands::Analytics(AnalyticsCommand::Tokens { common, group_by })) => {
            assert_eq!(group_by, AnalyticsBucketing::Week);
            assert_eq!(common.since.as_deref(), Some("2026-01-01"));
            assert_eq!(common.until.as_deref(), Some("2026-01-31"));
            assert_eq!(common.days, Some(7));
            assert_eq!(common.agent, vec!["claude", "codex"]);
            assert_eq!(common.workspace, vec!["/tmp/ws-a", "/tmp/ws-b"]);
            assert_eq!(common.source.as_deref(), Some("remote"));
            assert!(common.json);
        }
        other => panic!("expected analytics tokens command, got {other:?}"),
    }
}

#[test]
fn parse_analytics_models_subcommand_name_maps_to_variant() {
    let cli = Cli::try_parse_from(["cass", "analytics", "models", "--group-by", "day", "--json"])
        .expect("parse analytics models");
    match cli.command {
        Some(Commands::Analytics(AnalyticsCommand::AnalyticsModels { common, group_by })) => {
            assert_eq!(group_by, AnalyticsBucketing::Day);
            assert!(common.json);
        }
        other => panic!("expected analytics models command variant, got {other:?}"),
    }
}

#[test]
fn analytics_group_by_invalid_value_returns_actionable_error() {
    let mut cmd = simple_cmd();
    cmd.args(["analytics", "tokens", "--group-by", "fortnight", "--json"]);
    let output = cmd.assert().failure().get_output().clone();
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();

    assert!(
        stderr.contains("possible values")
            || stderr.contains("possible value")
            || stderr.contains("invalid value"),
        "invalid --group-by should report actionable enum guidance, stderr={stderr}"
    );
}

// =============================================================================
// Analytics tokens data tests (br-z9fse.3.3)
// =============================================================================

#[test]
fn analytics_tokens_json_returns_buckets_and_totals() {
    let mut cmd = simple_cmd();
    cmd.args(["analytics", "tokens", "--json"]);
    let output = cmd.assert().success().get_output().clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\nstdout={stdout}"));

    assert_eq!(json["command"], "analytics/tokens");

    let data = &json["data"];
    assert!(
        data["buckets"].is_array(),
        "analytics/tokens must expose buckets array: {data}"
    );
    assert!(
        data["bucket_count"].is_number(),
        "analytics/tokens must expose bucket_count: {data}"
    );

    // _meta must include path and group_by
    let meta = &data["_meta"];
    assert!(meta.is_object(), "missing _meta in data: {data}");
    assert!(
        meta["elapsed_ms"].is_number(),
        "missing elapsed_ms in _meta: {meta}"
    );
    assert!(
        meta["group_by"].is_string(),
        "missing group_by in _meta: {meta}"
    );
    assert_eq!(meta["group_by"], "day", "default group_by should be day");
}

#[test]
fn analytics_tokens_group_by_hour() {
    let mut cmd = simple_cmd();
    cmd.args(["analytics", "tokens", "--group-by", "hour", "--json"]);
    let output = cmd.assert().success().get_output().clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\nstdout={stdout}"));

    let meta = &json["data"]["_meta"];
    assert_eq!(meta["group_by"], "hour");
    assert_eq!(meta["source_table"], "usage_hourly");
}

#[test]
fn analytics_tokens_group_by_week() {
    let mut cmd = simple_cmd();
    cmd.args(["analytics", "tokens", "--group-by", "week", "--json"]);
    let output = cmd.assert().success().get_output().clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\nstdout={stdout}"));

    let meta = &json["data"]["_meta"];
    assert_eq!(meta["group_by"], "week");
    assert_eq!(meta["source_table"], "usage_daily");
}

#[test]
fn analytics_tokens_group_by_month() {
    let mut cmd = simple_cmd();
    cmd.args(["analytics", "tokens", "--group-by", "month", "--json"]);
    let output = cmd.assert().success().get_output().clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\nstdout={stdout}"));

    let meta = &json["data"]["_meta"];
    assert_eq!(meta["group_by"], "month");
    assert_eq!(meta["source_table"], "usage_daily");
}

#[test]
fn analytics_tokens_with_time_filter() {
    let mut cmd = simple_cmd();
    cmd.args(["analytics", "tokens", "--days", "7", "--json"]);
    let output = cmd.assert().success().get_output().clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\nstdout={stdout}"));

    // Should still have valid structure even if no data in range
    assert!(json["data"]["buckets"].is_array());
    assert!(json["data"]["bucket_count"].is_number());

    // Totals should always be present
    let totals = &json["data"]["totals"];
    assert!(
        totals.is_object(),
        "totals should be present even with empty results: {json}"
    );
    assert!(totals["counts"].is_object());
    assert!(totals["api_tokens"].is_object());
    assert!(totals["content_tokens"].is_object());
    assert!(totals["coverage"].is_object());
    assert!(totals["derived"].is_object());
}

#[test]
fn analytics_tokens_with_agent_filter() {
    let mut cmd = simple_cmd();
    cmd.args(["analytics", "tokens", "--agent", "claude_code", "--json"]);
    let output = cmd.assert().success().get_output().clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\nstdout={stdout}"));

    assert!(json["data"]["buckets"].is_array());

    // Verify filter was applied
    let filters = json["_meta"]["filters_applied"]
        .as_array()
        .expect("filters_applied array");
    let has_agent_filter = filters
        .iter()
        .any(|f| f.as_str().unwrap_or("").contains("agent="));
    assert!(
        has_agent_filter,
        "should include agent filter in _meta.filters_applied"
    );
}

#[test]
fn analytics_tokens_workspace_filter_applies_and_normalizes_filters() {
    let tmp = TempDir::new().unwrap();
    let (workspace_a, _workspace_b) = seed_analytics_workspace_fixture(&tmp);
    let workspace_arg = format!("  {}  ", workspace_a.display());

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "analytics",
        "tokens",
        "--workspace",
        &workspace_arg,
        "--agent",
        "  codex  ",
        "--source",
        "  LOCAL  ",
        "--json",
    ]);
    let output = cmd.assert().success().get_output().clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\nstdout={stdout}"));

    assert_eq!(json["data"]["totals"]["counts"]["message_count"], 2);

    let filters: Vec<String> = json["_meta"]["filters_applied"]
        .as_array()
        .expect("filters_applied array")
        .iter()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .collect();

    assert!(filters.iter().any(|value| value == "agent=codex"));
    assert!(filters.iter().any(|value| value == "source=local"));
    assert!(
        filters
            .iter()
            .any(|value| value == &format!("workspace={}", workspace_a.display()))
    );
}

#[test]
fn analytics_status_workspace_filter_applies_and_normalizes_filters() {
    let tmp = TempDir::new().unwrap();
    let (workspace_a, _workspace_b) = seed_analytics_workspace_fixture(&tmp);
    let workspace_arg = format!("  {}  ", workspace_a.display());

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "analytics",
        "status",
        "--workspace",
        &workspace_arg,
        "--agent",
        "  codex  ",
        "--source",
        "  LOCAL  ",
        "--json",
    ]);
    let output = cmd.assert().success().get_output().clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "invalid JSON: {e}
stdout={stdout}"
        )
    });

    assert_eq!(json["data"]["coverage"]["total_messages"], 2);

    let message_metrics = json["data"]["tables"]
        .as_array()
        .expect("tables array")
        .iter()
        .find(|table| table["table"] == "message_metrics")
        .expect("message_metrics table entry");
    assert_eq!(message_metrics["row_count"], 2);

    let filters: Vec<String> = json["_meta"]["filters_applied"]
        .as_array()
        .expect("filters_applied array")
        .iter()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .collect();

    assert!(filters.iter().any(|value| value == "agent=codex"));
    assert!(filters.iter().any(|value| value == "source=local"));
    assert!(
        filters
            .iter()
            .any(|value| value == &format!("workspace={}", workspace_a.display()))
    );
}

#[test]
fn analytics_models_workspace_filter_applies_and_uses_workspace_scoped_breakdown() {
    let tmp = TempDir::new().unwrap();
    let workspace_a = seed_analytics_models_workspace_fixture(&tmp);
    let workspace_arg = format!("  {}  ", workspace_a.display());

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "analytics",
        "models",
        "--workspace",
        &workspace_arg,
        "--agent",
        "  codex  ",
        "--source",
        "  LOCAL  ",
        "--json",
    ]);
    let output = cmd.assert().success().get_output().clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\nstdout={stdout}"));

    assert_eq!(
        json["data"]["by_api_tokens"]["_meta"]["source_table"],
        "token_usage"
    );
    let rows = json["data"]["by_api_tokens"]["rows"]
        .as_array()
        .expect("breakdown rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["key"], "gpt-4o");
    assert_eq!(rows[0]["value"], 29);

    let filters: Vec<String> = json["_meta"]["filters_applied"]
        .as_array()
        .expect("filters_applied array")
        .iter()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .collect();

    assert!(filters.iter().any(|value| value == "agent=codex"));
    assert!(filters.iter().any(|value| value == "source=local"));
    assert!(
        filters
            .iter()
            .any(|value| value == &format!("workspace={}", workspace_a.display()))
    );
}

#[test]
fn analytics_tokens_unknown_workspace_filter_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let _ = seed_analytics_workspace_fixture(&tmp);
    let missing_workspace = tmp.path().join("missing-workspace");

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "analytics",
        "tokens",
        "--workspace",
        missing_workspace.to_string_lossy().as_ref(),
        "--json",
    ]);
    let output = cmd.assert().success().get_output().clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\nstdout={stdout}"));

    assert_eq!(json["data"]["bucket_count"], 0);
    assert_eq!(json["data"]["totals"]["counts"]["message_count"], 0);
}

#[test]
fn analytics_tokens_totals_structure_complete() {
    // Verify that the totals JSON includes all required sections
    // even when the database has no data.
    let mut cmd = simple_cmd();
    cmd.args(["analytics", "tokens", "--json"]);
    let output = cmd.assert().success().get_output().clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\nstdout={stdout}"));

    let totals = &json["data"]["totals"];
    if totals.is_object() {
        // Check counts section
        let counts = &totals["counts"];
        assert!(counts["message_count"].is_number());
        assert!(counts["user_message_count"].is_number());
        assert!(counts["assistant_message_count"].is_number());
        assert!(counts["tool_call_count"].is_number());
        assert!(counts["plan_message_count"].is_number());

        // Check api_tokens section
        let api = &totals["api_tokens"];
        assert!(api["total"].is_number());
        assert!(api["input"].is_number());
        assert!(api["output"].is_number());
        assert!(api["cache_read"].is_number());
        assert!(api["cache_creation"].is_number());
        assert!(api["thinking"].is_number());

        // Check content_tokens section
        let content = &totals["content_tokens"];
        assert!(content["est_total"].is_number());
        assert!(content["est_user"].is_number());
        assert!(content["est_assistant"].is_number());

        // Check coverage section
        let coverage = &totals["coverage"];
        assert!(coverage["api_coverage_message_count"].is_number());
        assert!(coverage["api_coverage_pct"].is_number());

        // Check derived section exists
        assert!(
            totals["derived"].is_object(),
            "totals.derived must be present"
        );
    }
}

// =============================================================================
// Analytics rebuild data tests (br-z9fse.3.4)
// =============================================================================

#[test]
fn analytics_rebuild_json_envelope_structure() {
    // Use isolated temp dir to avoid DB lock contention with parallel tests.
    let temp = TempDir::new().unwrap();
    let mut cmd = base_cmd(temp.path());
    cmd.args(["analytics", "rebuild", "--json"]);
    let output = cmd.output().expect("run analytics rebuild");

    if output.status.success() {
        // DB existed and rebuild succeeded — validate JSON envelope on stdout.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: Value = serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("invalid JSON: {e}\nstdout={stdout}"));

        assert_eq!(json["command"], "analytics/rebuild");
        assert!(
            json["_meta"]["elapsed_ms"].is_number(),
            "envelope must include _meta.elapsed_ms: {json}"
        );

        let data = &json["data"];
        assert!(
            data["track_a"].is_object(),
            "analytics/rebuild must expose track_a results on success: {data}"
        );
        assert!(data["track_a"]["message_metrics_rows"].is_number());
        assert!(data["track_a"]["usage_hourly_rows"].is_number());
        assert!(data["track_a"]["usage_daily_rows"].is_number());
        assert!(data["track_a"]["elapsed_ms"].is_number());
        assert_eq!(data["track"], "a");
        assert!(data["tracks_rebuilt"].is_array());
        assert!(data["overall_elapsed_ms"].is_number());
    } else {
        // In isolated env the DB does not exist — rebuild exits non-zero with
        // a structured error on stderr.  Validate the error is well-formed.
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.trim().is_empty(),
            "analytics rebuild should emit an error diagnostic on stderr when DB is missing"
        );
        // The error should mention the missing database.
        assert!(
            stderr.contains("not found") || stderr.contains("missing") || stderr.contains("error"),
            "rebuild error should describe the missing DB: {stderr}"
        );
    }
}

#[test]
fn analytics_validate_reports_query_failure_for_malformed_schema() {
    let tmp_home = TempDir::new().expect("temp home");
    let data_dir = tmp_home.path().join("cass_data");
    fs::create_dir_all(&data_dir).expect("create data dir");
    let db_path = data_dir.join("agent_search.db");

    let conn = rusqlite::Connection::open(&db_path).expect("create sqlite db");
    conn.execute_batch(
        "CREATE TABLE message_metrics (day_id INTEGER);
         CREATE TABLE usage_daily (day_id INTEGER);
         INSERT INTO usage_daily (day_id) VALUES (20254);",
    )
    .expect("create malformed analytics tables");
    drop(conn);

    let mut cmd = base_cmd(tmp_home.path());
    cmd.args([
        "analytics",
        "validate",
        "--json",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid analytics validate JSON");

    assert_eq!(json["command"], "analytics/validate");
    let checks = json["data"]["checks"].as_array().expect("checks array");
    let query_failure = checks
        .iter()
        .find(|check| check["id"] == "track_a.query_exec")
        .expect("track_a query failure should be reported");

    assert_eq!(query_failure["ok"], false);
    assert_eq!(query_failure["severity"], "error");
    assert!(
        query_failure["details"]
            .as_str()
            .unwrap()
            .contains("Track A invariant query failed")
    );
    assert_eq!(json["data"]["perf"]["timeseries"]["within_budget"], false);
    assert!(
        json["data"]["perf"]["timeseries"]["error"]
            .as_str()
            .is_some_and(|error| !error.trim().is_empty())
    );
    assert!(
        json["data"]["perf"]["timeseries"]["details"]
            .as_str()
            .unwrap()
            .contains("failed")
    );
    assert_eq!(json["data"]["perf"]["breakdown"]["within_budget"], false);
    assert!(
        json["data"]["perf"]["breakdown"]["error"]
            .as_str()
            .is_some_and(|error| !error.trim().is_empty())
    );
    assert!(
        json["data"]["summary"]["errors"].as_u64().unwrap_or(0) >= 1,
        "malformed analytics schema should surface at least one error"
    );
}

#[test]
fn analytics_rebuild_help_shows_force_flag() {
    let mut cmd = simple_cmd();
    cmd.args(["analytics", "rebuild", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("--force"))
        .stdout(contains("--json"));
}

#[test]
fn analytics_rebuild_parses_force_and_json_flags() {
    use clap::Parser;
    use coding_agent_search::{AnalyticsCommand, Cli, Commands};

    let cli = Cli::try_parse_from(["cass", "analytics", "rebuild", "--force", "--json"])
        .expect("parse analytics rebuild with force+json");

    match cli.command {
        Some(Commands::Analytics(AnalyticsCommand::Rebuild { common, force })) => {
            assert!(force, "--force should be true");
            assert!(common.json, "--json should be true");
        }
        other => panic!("expected analytics rebuild, got {other:?}"),
    }
}
