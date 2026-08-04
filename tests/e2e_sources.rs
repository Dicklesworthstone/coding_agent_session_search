//! E2E tests for `cass sources` CLI commands.
//!
//! Tests the sources subcommands end-to-end:
//! - sources add (with --no-test to skip SSH)
//! - sources list
//! - sources remove
//! - sources doctor (limited without actual SSH)
//! - sources sync (dry-run only)
//!
//! Note: Tests that require actual SSH connectivity are marked #[ignore].

use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;

mod util;
use util::e2e_log::{E2eCommandEnvironment, PhaseTracker};

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_sources", test_name)
}

/// Helper: Create a sources.toml config file with given content.
fn create_sources_config(config_dir: &Path, toml_content: &str) {
    let config_file = config_dir.join("cass").join("sources.toml");
    fs::create_dir_all(config_file.parent().unwrap()).unwrap();
    fs::write(&config_file, toml_content).unwrap();
}

/// Helper: Read the sources.toml config file.
fn read_sources_config(config_dir: &Path) -> String {
    let config_file = config_dir.join("cass").join("sources.toml");
    fs::read_to_string(&config_file).unwrap_or_default()
}

fn read_optional_bytes(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn directory_member_label(kind: &str, name: &std::ffi::OsStr) -> String {
    format!("{kind}:{}", name.to_string_lossy())
}

fn directory_membership(path: &Path) -> std::io::Result<Vec<String>> {
    let mut members = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let kind = if file_type.is_dir() {
            "dir"
        } else if file_type.is_file() {
            "file"
        } else {
            "other"
        };
        members.push(directory_member_label(kind, &entry.file_name()));
    }
    members.sort();
    Ok(members)
}

fn cass_data_dir(data_root: &Path) -> std::path::PathBuf {
    data_root.join("coding-agent-search")
}

fn seed_archive_conversation(db_path: &Path, agent_slug: &str, marker: &str) {
    let storage = FrankenStorage::open(db_path).unwrap();
    let agent = Agent {
        id: None,
        slug: agent_slug.into(),
        name: agent_slug.into(),
        version: None,
        kind: AgentKind::Cli,
    };
    let agent_id = storage.ensure_agent(&agent).unwrap();
    let conversation = Conversation {
        id: None,
        agent_slug: agent_slug.into(),
        workspace: Some("/tmp/workspace".into()),
        external_id: Some(format!("{agent_slug}-{marker}")),
        title: Some(format!("{agent_slug} {marker}")),
        source_path: format!("/tmp/{agent_slug}-{marker}.jsonl").into(),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_000_100),
        approx_tokens: None,
        metadata_json: serde_json::Value::Null,
        messages: vec![
            Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: Some("user".into()),
                created_at: Some(1_700_000_000_010),
                content: format!("{agent_slug} {marker} user"),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            },
            Message {
                id: None,
                idx: 1,
                role: MessageRole::Agent,
                author: Some("assistant".into()),
                created_at: Some(1_700_000_000_020),
                content: format!("{agent_slug} {marker} assistant"),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            },
        ],
        source_id: "local".into(),
        origin_host: None,
    };
    storage
        .insert_conversation_tree(agent_id, None, &conversation)
        .unwrap();
}

// =============================================================================
// sources list tests
// =============================================================================

/// Test: sources list with no configured sources shows appropriate message.
#[test]
fn sources_list_empty() {
    let tracker = tracker_for("sources_list_empty");

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start("run_sources_list", Some("Run sources list with no config"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "list"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources list command");
    tracker.end(
        "run_sources_list",
        Some("Run sources list with no config"),
        start,
    );

    let start = tracker.start("verify_output", Some("Verify empty sources message"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No sources configured") || stdout.contains("0 sources"),
        "Expected empty sources message, got: {stdout}"
    );
    tracker.end("verify_output", Some("Verify empty sources message"), start);

    tracker.complete();
}

/// Test: sources list with configured sources shows them.
#[test]
fn sources_list_with_sources() {
    let tracker = tracker_for("sources_list_with_sources");

    let start = tracker.start("setup", Some("Create config with one source"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
sync_schedule = "manual"
"#,
    );
    tracker.end("setup", Some("Create config with one source"), start);

    let start = tracker.start("run_sources_list", Some("Run sources list"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "list"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources list command");
    tracker.end("run_sources_list", Some("Run sources list"), start);

    let start = tracker.start("verify_output", Some("Verify source appears in output"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("laptop"),
        "Expected source name in output, got: {stdout}"
    );
    tracker.end(
        "verify_output",
        Some("Verify source appears in output"),
        start,
    );

    tracker.complete();
}

#[test]
fn sources_agents_exclude_and_list_json() -> Result<(), Box<dyn std::error::Error>> {
    let tracker = tracker_for("sources_agents_exclude_and_list_json");

    let tmp = tempfile::TempDir::new()?;
    let config_dir = tmp.path().join("config");
    let data_dir = cass_data_dir(&tmp.path().join("data"));
    fs::create_dir_all(&config_dir)?;

    let start = tracker.start(
        "exclude_agent",
        Some("Exclude openclaw from future indexing runs"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "agents",
            "exclude",
            "openclaw",
            "--keep-indexed-data",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .output()?;
    tracker.end(
        "exclude_agent",
        Some("Exclude openclaw from future indexing runs"),
        start,
    );
    assert!(
        output.status.success(),
        "exclude failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let config = read_sources_config(&config_dir);
    assert!(
        config.contains("disabled_agents") && config.contains("openclaw"),
        "expected disabled_agents entry in config, got: {config}"
    );

    let start = tracker.start("list_agents_json", Some("List excluded agents in JSON"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "agents", "list", "--json"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .output()?;
    tracker.end(
        "list_agents_json",
        Some("List excluded agents in JSON"),
        start,
    );
    assert!(
        output.status.success(),
        "list failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["disabled_agents"], serde_json::json!(["openclaw"]));

    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "list", "--json"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .output()?;
    assert!(output.status.success(), "sources list --json failed");
    let json: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["disabled_agents"], serde_json::json!(["openclaw"]));

    tracker.complete();
    Ok(())
}

#[test]
fn sources_agents_include_removes_existing_exclusion() {
    let tracker = tracker_for("sources_agents_include_removes_existing_exclusion");

    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    create_sources_config(&config_dir, "disabled_agents = [\"openclaw\"]\n");

    let start = tracker.start(
        "include_agent",
        Some("Re-enable openclaw for future indexing runs"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "agents", "include", "openclaw"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources agents include command");
    tracker.end(
        "include_agent",
        Some("Re-enable openclaw for future indexing runs"),
        start,
    );
    assert!(
        output.status.success(),
        "include failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "agents", "list", "--json"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources agents list command");
    assert!(output.status.success(), "sources agents list failed");
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(json["disabled_agents"], serde_json::json!([]));

    tracker.complete();
}

#[test]
fn sources_agents_exclude_purges_local_archive_data_by_default() {
    let tracker = tracker_for("sources_agents_exclude_purges_local_archive_data_by_default");

    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_root = tmp.path().join("data");
    let data_dir = cass_data_dir(&data_root);
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("agent_search.db");

    seed_archive_conversation(&db_path, "openclaw", "purge-me");
    seed_archive_conversation(&db_path, "codex", "keep-me");

    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "agents", "exclude", "openclaw"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .output()
        .expect("sources agents exclude command");
    assert!(
        output.status.success(),
        "exclude failed: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let storage = FrankenStorage::open(&db_path).unwrap();
    let conversations = storage.list_conversations(10, 0).unwrap();
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].agent_slug, "codex");

    let mut command = tracker.cass_assert_command();
    let search_output = command
        .args(["search", "purge-me", "--robot", "--limit", "5"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .output()
        .expect("search removed agent data");
    assert!(
        search_output.status.success(),
        "search after purge failed: {}\nstdout: {}\nstderr: {}",
        search_output.status,
        String::from_utf8_lossy(&search_output.stdout),
        String::from_utf8_lossy(&search_output.stderr)
    );
    let removed_hits: Value = serde_json::from_slice(&search_output.stdout).expect("valid json");
    let removed_total = removed_hits
        .get("total")
        .or_else(|| removed_hits.get("count"))
        .and_then(|value| value.as_i64())
        .or_else(|| {
            removed_hits
                .get("results")
                .and_then(|value| value.as_array())
                .map(|values| values.len() as i64)
        })
        .or_else(|| {
            removed_hits
                .get("hits")
                .and_then(|value| value.as_array())
                .map(|values| values.len() as i64)
        })
        .unwrap_or(-1);
    assert_eq!(
        removed_total,
        0,
        "expected removed agent search to have no hits: {}",
        String::from_utf8_lossy(&search_output.stdout)
    );

    let mut command = tracker.cass_assert_command();
    let search_output = command
        .args(["search", "keep-me", "--robot", "--limit", "5"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .output()
        .expect("search retained agent data");
    assert!(search_output.status.success(), "search kept agent failed");
    let kept_hits: Value = serde_json::from_slice(&search_output.stdout).expect("valid json");
    let kept_total = kept_hits
        .get("total")
        .or_else(|| kept_hits.get("count"))
        .and_then(|value| value.as_i64())
        .or_else(|| {
            kept_hits
                .get("results")
                .and_then(|value| value.as_array())
                .map(|values| values.len() as i64)
        })
        .or_else(|| {
            kept_hits
                .get("hits")
                .and_then(|value| value.as_array())
                .map(|values| values.len() as i64)
        })
        .unwrap_or_default();
    assert!(
        kept_total >= 1,
        "expected retained codex data to remain searchable: {}",
        String::from_utf8_lossy(&search_output.stdout)
    );
}

/// Test: sources list --verbose shows additional details.
#[test]
fn sources_list_verbose() {
    let tracker = tracker_for("sources_list_verbose");

    let start = tracker.start("setup", Some("Create config with verbose-testable source"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "workstation"
type = "ssh"
host = "dev@work.example.com"
paths = ["~/.claude/projects", "~/.codex/sessions"]
sync_schedule = "daily"
"#,
    );
    tracker.end(
        "setup",
        Some("Create config with verbose-testable source"),
        start,
    );

    let start = tracker.start(
        "run_sources_list_verbose",
        Some("Run sources list --verbose"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "list", "--verbose"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources list --verbose command");
    tracker.end(
        "run_sources_list_verbose",
        Some("Run sources list --verbose"),
        start,
    );

    let start = tracker.start(
        "verify_output",
        Some("Verify verbose output contains details"),
    );
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("workstation"), "Missing source name");
    assert!(
        stdout.contains("work.example.com") || stdout.contains("dev@work"),
        "Missing host info in verbose output"
    );
    tracker.end(
        "verify_output",
        Some("Verify verbose output contains details"),
        start,
    );

    tracker.complete();
}

/// Test: sources list --json outputs valid JSON.
#[test]
fn sources_list_json() {
    let tracker = tracker_for("sources_list_json");

    let start = tracker.start("setup", Some("Create config for JSON output test"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );
    tracker.end("setup", Some("Create config for JSON output test"), start);

    let start = tracker.start("run_sources_list_json", Some("Run sources list --json"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "list", "--json"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .output()
        .expect("sources list --json command");
    tracker.end(
        "run_sources_list_json",
        Some("Run sources list --json"),
        start,
    );

    let start = tracker.start("verify_json", Some("Verify JSON structure and content"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    assert!(
        json.get("sources").is_some(),
        "Expected 'sources' field in JSON"
    );
    let sources = json["sources"].as_array().expect("sources should be array");
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["name"], "laptop");
    assert_eq!(sources[0]["sync_health"]["health"], "never_synced");
    assert_eq!(sources[0]["sync_health"]["action"], "skip");
    assert_eq!(sources[0]["sync_health"]["stale_value_score"], 100);
    assert_eq!(
        sources[0]["sync_health"]["staleness_ms"],
        serde_json::Value::Null
    );
    assert_eq!(sources[0]["sync_health"]["manual_override"], false);
    assert!(
        sources[0]["sync_health"]["reasons"]
            .as_array()
            .is_some_and(|reasons| !reasons.is_empty())
    );
    tracker.end(
        "verify_json",
        Some("Verify JSON structure and content"),
        start,
    );

    tracker.complete();
}

// =============================================================================
// sources add tests
// =============================================================================

/// Test: sources add with --no-test creates config without SSH connectivity.
#[test]
fn sources_add_no_test() {
    let tracker = tracker_for("sources_add_no_test");

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start("run_sources_add", Some("Run sources add with --no-test"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "add",
            "user@myserver.local",
            "--name",
            "myserver",
            "--preset",
            "linux-defaults",
            "--no-test",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources add command");
    tracker.end(
        "run_sources_add",
        Some("Run sources add with --no-test"),
        start,
    );

    let start = tracker.start(
        "verify_output",
        Some("Verify add success and config written"),
    );
    assert!(
        output.status.success(),
        "sources add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Added source 'myserver'"),
        "Expected success message, got: {stdout}"
    );
    let config_content = read_sources_config(&config_dir);
    assert!(
        config_content.contains("myserver"),
        "Source not in config file"
    );
    assert!(
        config_content.contains("user@myserver.local"),
        "Host not in config file"
    );
    tracker.end(
        "verify_output",
        Some("Verify add success and config written"),
        start,
    );

    tracker.complete();
}

/// Test: sources add with explicit paths.
#[test]
fn sources_add_explicit_paths() {
    let tracker = tracker_for("sources_add_explicit_paths");

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start(
        "run_sources_add",
        Some("Run sources add with explicit paths"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "add",
            "admin@devbox",
            "--name",
            "devbox",
            "--path",
            "~/.claude/projects",
            "--path",
            "~/.codex/sessions",
            "--no-test",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources add command");
    tracker.end(
        "run_sources_add",
        Some("Run sources add with explicit paths"),
        start,
    );

    let start = tracker.start("verify_config", Some("Verify paths in config file"));
    assert!(
        output.status.success(),
        "sources add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config_content = read_sources_config(&config_dir);
    assert!(
        config_content.contains("devbox"),
        "Source name not in config"
    );
    assert!(
        config_content.contains(".claude/projects"),
        "Path 1 not in config"
    );
    assert!(
        config_content.contains(".codex/sessions"),
        "Path 2 not in config"
    );
    tracker.end("verify_config", Some("Verify paths in config file"), start);

    tracker.complete();
}

/// Test: sources add merges --preset paths with explicit --path entries (#358).
#[test]
fn sources_add_preset_merges_explicit_paths() {
    let tracker = tracker_for("sources_add_preset_merges_explicit_paths");

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start(
        "run_sources_add",
        Some("Run sources add with --preset plus --path"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "add",
            "user@studio.local",
            "--name",
            "studio",
            "--preset",
            "linux-defaults",
            "--path",
            "~/.local/share/opencode",
            "--no-test",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources add command");
    tracker.end(
        "run_sources_add",
        Some("Run sources add with --preset plus --path"),
        start,
    );

    let start = tracker.start(
        "verify_config",
        Some("Verify preset and custom paths both present"),
    );
    assert!(
        output.status.success(),
        "sources add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config_content = read_sources_config(&config_dir);
    assert!(
        config_content.contains(".claude/projects"),
        "Preset path missing from config: {config_content}"
    );
    assert!(
        config_content.contains(".local/share/opencode"),
        "Explicit --path entry silently dropped when combined with --preset (#358): {config_content}"
    );
    tracker.end(
        "verify_config",
        Some("Verify preset and custom paths both present"),
        start,
    );

    tracker.complete();
}

/// Test: sources add fails without paths.
#[test]
fn sources_add_no_paths_error() {
    let tracker = tracker_for("sources_add_no_paths_error");

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start("run_sources_add", Some("Run sources add without paths"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "add",
            "user@server.local",
            "--name",
            "server",
            "--no-test",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources add command");
    tracker.end(
        "run_sources_add",
        Some("Run sources add without paths"),
        start,
    );

    let start = tracker.start("verify_error", Some("Verify paths error reported"));
    assert!(
        !output.status.success(),
        "command should have failed but succeeded with: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No paths") || stderr.contains("path"),
        "Expected paths error, got: {stderr}"
    );
    tracker.end("verify_error", Some("Verify paths error reported"), start);

    tracker.complete();
}

/// Test: sources add rejects duplicate source names.
#[test]
fn sources_add_duplicate_error() {
    let tracker = tracker_for("sources_add_duplicate_error");

    let start = tracker.start("setup", Some("Create config with existing source"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );
    tracker.end("setup", Some("Create config with existing source"), start);

    let start = tracker.start(
        "run_sources_add_duplicate",
        Some("Add source with duplicate name"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "add",
            "other@other.local",
            "--name",
            "laptop",
            "--preset",
            "linux-defaults",
            "--no-test",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources add command");
    tracker.end(
        "run_sources_add_duplicate",
        Some("Add source with duplicate name"),
        start,
    );

    let start = tracker.start("verify_error", Some("Verify duplicate error"));
    assert!(
        !output.status.success(),
        "command should have failed but succeeded with: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already exists") || stderr.contains("duplicate"),
        "Expected duplicate error, got: {stderr}"
    );
    tracker.end("verify_error", Some("Verify duplicate error"), start);

    tracker.complete();
}

/// Test: sources add rejects the reserved local source name.
#[test]
fn sources_add_reserved_local_name_error() {
    let tracker = tracker_for("sources_add_reserved_local_name_error");

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start(
        "run_sources_add_reserved_local",
        Some("Attempt to add source named local"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "add",
            "user@other.local",
            "--name",
            "local",
            "--preset",
            "linux-defaults",
            "--no-test",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources add command");
    tracker.end(
        "run_sources_add_reserved_local",
        Some("Attempt to add source named local"),
        start,
    );

    let start = tracker.start("verify_error", Some("Verify reserved local error"));
    assert!(
        !output.status.success(),
        "command should have failed but succeeded with: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("reserved") || stderr.contains("built-in local source"),
        "Expected reserved-name error, got: {stderr}"
    );
    tracker.end("verify_error", Some("Verify reserved local error"), start);

    tracker.complete();
}

/// Test: sources add rejects duplicate names that differ only by case.
#[test]
fn sources_add_duplicate_error_case_insensitive() {
    let tracker = tracker_for("sources_add_duplicate_error_case_insensitive");

    let start = tracker.start(
        "setup",
        Some("Create config with existing mixed-case source"),
    );
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "Laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );
    tracker.end(
        "setup",
        Some("Create config with existing mixed-case source"),
        start,
    );

    let start = tracker.start(
        "run_sources_add_duplicate",
        Some("Add source with duplicate name differing only by case"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "add",
            "other@other.local",
            "--name",
            "laptop",
            "--preset",
            "linux-defaults",
            "--no-test",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources add command");
    tracker.end(
        "run_sources_add_duplicate",
        Some("Add source with duplicate name differing only by case"),
        start,
    );

    let start = tracker.start(
        "verify_error",
        Some("Verify case-insensitive duplicate error"),
    );
    assert!(
        !output.status.success(),
        "command should have failed but succeeded with: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already exists") || stderr.contains("duplicate"),
        "Expected duplicate error, got: {stderr}"
    );
    tracker.end(
        "verify_error",
        Some("Verify case-insensitive duplicate error"),
        start,
    );

    tracker.complete();
}

/// Test: sources add with invalid URL format.
#[test]
fn sources_add_invalid_url() {
    let tracker = tracker_for("sources_add_invalid_url");

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start(
        "run_sources_add_invalid",
        Some("Add source with invalid URL"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "add",
            "laptop.local",
            "--preset",
            "linux-defaults",
            "--no-test",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources add command");
    tracker.end(
        "run_sources_add_invalid",
        Some("Add source with invalid URL"),
        start,
    );

    let start = tracker.start("verify_error", Some("Verify invalid URL error"));
    assert!(
        !output.status.success(),
        "command should have failed but succeeded with: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("username") || stderr.contains("Invalid"),
        "Expected invalid URL error, got: {stderr}"
    );
    tracker.end("verify_error", Some("Verify invalid URL error"), start);

    tracker.complete();
}

/// Test: sources add auto-generates name from hostname.
#[test]
fn sources_add_auto_name() {
    let tracker = tracker_for("sources_add_auto_name");

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start("run_sources_add", Some("Add source without explicit name"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "add",
            "user@devlaptop.home.lan",
            "--preset",
            "linux-defaults",
            "--no-test",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources add command");
    tracker.end(
        "run_sources_add",
        Some("Add source without explicit name"),
        start,
    );

    let start = tracker.start("verify_auto_name", Some("Verify auto-generated name"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let config_content = read_sources_config(&config_dir);
    assert!(
        config_content.contains("name = \"devlaptop\""),
        "Auto-generated name not found in config: {config_content}"
    );
    tracker.end(
        "verify_auto_name",
        Some("Verify auto-generated name"),
        start,
    );

    tracker.complete();
}

/// Test: sources add auto-generated names do not collide with the built-in local source.
#[test]
fn sources_add_auto_name_disambiguates_reserved_local() {
    let tracker = tracker_for("sources_add_auto_name_disambiguates_reserved_local");

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start(
        "run_sources_add",
        Some("Add source without explicit name for reserved local hostname"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "add",
            "user@local",
            "--preset",
            "linux-defaults",
            "--no-test",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources add command");
    tracker.end(
        "run_sources_add",
        Some("Add source without explicit name for reserved local hostname"),
        start,
    );

    let start = tracker.start(
        "verify_auto_name",
        Some("Verify reserved local auto-name was disambiguated"),
    );
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let config_content = read_sources_config(&config_dir);
    assert!(
        config_content.contains("name = \"local-ssh\""),
        "Reserved auto-generated name not rewritten in config: {config_content}"
    );
    assert!(
        config_content.contains("host = \"user@local\""),
        "Host not preserved in config: {config_content}"
    );
    tracker.end(
        "verify_auto_name",
        Some("Verify reserved local auto-name was disambiguated"),
        start,
    );

    tracker.complete();
}

// =============================================================================
// sources remove tests
// =============================================================================

/// Test: sources remove removes a configured source.
#[test]
fn sources_remove_basic() {
    let tracker = tracker_for("sources_remove_basic");

    let start = tracker.start("setup", Some("Create config with two sources"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]

[[sources]]
name = "workstation"
type = "ssh"
host = "dev@work.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end("setup", Some("Create config with two sources"), start);

    let start = tracker.start("run_sources_remove", Some("Remove laptop source"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "remove", "laptop", "-y"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources remove command");
    tracker.end("run_sources_remove", Some("Remove laptop source"), start);

    let start = tracker.start(
        "verify_removal",
        Some("Verify laptop removed and workstation kept"),
    );
    assert!(
        output.status.success(),
        "sources remove failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify config was updated
    let config_content = read_sources_config(&config_dir);
    assert!(
        !config_content.contains("name = \"laptop\""),
        "Removed source still in config"
    );
    assert!(
        config_content.contains("workstation"),
        "Other source incorrectly removed"
    );
    tracker.end(
        "verify_removal",
        Some("Verify laptop removed and workstation kept"),
        start,
    );

    tracker.complete();
}

/// Test: sources remove with nonexistent source.
#[test]
fn sources_remove_nonexistent() {
    let tracker = tracker_for("sources_remove_nonexistent");

    let start = tracker.start("setup", Some("Create config with one source"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end("setup", Some("Create config with one source"), start);

    let start = tracker.start("run_sources_remove", Some("Remove nonexistent source"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "remove", "nonexistent", "-y"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources remove command");
    tracker.end(
        "run_sources_remove",
        Some("Remove nonexistent source"),
        start,
    );

    let start = tracker.start("verify_error", Some("Verify not found error"));
    // Should fail gracefully
    assert!(
        !output.status.success(),
        "command should have failed but succeeded with: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found") || stderr.contains("does not exist"),
        "Expected not found error, got: {stderr}"
    );
    tracker.end("verify_error", Some("Verify not found error"), start);

    tracker.complete();
}

/// Test: sources remove with --purge flag.
#[test]
fn sources_remove_with_purge() {
    let tracker = tracker_for("sources_remove_with_purge");

    let start = tracker.start(
        "setup",
        Some("Create config and data directory for purge test"),
    );
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();

    // Create source data directory
    let source_data = cass_data_dir(&data_dir).join("remotes").join("laptop");
    fs::create_dir_all(&source_data).unwrap();
    fs::write(source_data.join("session.jsonl"), "test data").unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end(
        "setup",
        Some("Create config and data directory for purge test"),
        start,
    );

    let start = tracker.start(
        "run_sources_remove_purge",
        Some("Remove source with --purge"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "remove", "laptop", "--purge", "-y"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env_remove("CASS_DATA_DIR")
        .output()
        .expect("sources remove --purge command");
    tracker.end(
        "run_sources_remove_purge",
        Some("Remove source with --purge"),
        start,
    );

    let start = tracker.start("verify_removal", Some("Verify source removed from config"));
    assert!(
        output.status.success(),
        "sources remove --purge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify config was updated
    let config_content = read_sources_config(&config_dir);
    assert!(
        !config_content.contains("laptop"),
        "Removed source still in config"
    );
    assert!(
        !source_data.exists(),
        "Synced source data should have been purged"
    );
    tracker.end(
        "verify_removal",
        Some("Verify source removed from config"),
        start,
    );

    tracker.complete();
}

/// Test: sources remove with --purge resolves the stored source name for data cleanup.
#[test]
fn sources_remove_with_purge_case_insensitive_uses_stored_name() {
    let tracker = tracker_for("sources_remove_with_purge_case_insensitive_uses_stored_name");

    let start = tracker.start(
        "setup",
        Some("Create mixed-case config and matching data directory for purge test"),
    );
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();

    let source_data = cass_data_dir(&data_dir).join("remotes").join("Laptop");
    fs::create_dir_all(&source_data).unwrap();
    fs::write(source_data.join("session.jsonl"), "test data").unwrap();

    let sync_status_path = cass_data_dir(&data_dir).join("sync_status.json");
    fs::create_dir_all(sync_status_path.parent().unwrap()).unwrap();
    fs::write(
        &sync_status_path,
        r#"{
  "sources": {
    "Laptop": {
      "last_sync": 1234567890,
      "last_result": "success",
      "files_synced": 1,
      "bytes_transferred": 9,
      "duration_ms": 12
    }
  }
}"#,
    )
    .unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "Laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end(
        "setup",
        Some("Create mixed-case config and matching data directory for purge test"),
        start,
    );

    let start = tracker.start(
        "run_sources_remove_purge",
        Some("Remove mixed-case source with lowercase filter and --purge"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "remove", "laptop", "--purge", "-y"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env_remove("CASS_DATA_DIR")
        .output()
        .expect("sources remove --purge command");
    tracker.end(
        "run_sources_remove_purge",
        Some("Remove mixed-case source with lowercase filter and --purge"),
        start,
    );

    let start = tracker.start(
        "verify_removal",
        Some("Verify mixed-case source config and mirror data were removed"),
    );
    assert!(
        output.status.success(),
        "sources remove --purge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config_content = read_sources_config(&config_dir);
    assert!(
        !config_content.contains("name = \"Laptop\""),
        "Removed source still in config"
    );
    assert!(
        !source_data.exists(),
        "Stored-name mirror directory should have been purged"
    );
    let sync_status_content = fs::read_to_string(&sync_status_path).expect("read sync status");
    assert!(
        !sync_status_content.contains("\"Laptop\""),
        "Removed source should have been pruned from sync status"
    );
    tracker.end(
        "verify_removal",
        Some("Verify mixed-case source config and mirror data were removed"),
        start,
    );

    tracker.complete();
}

/// Test: interactive remove prompt shows the canonical stored source name.
#[test]
fn sources_remove_prompt_uses_stored_name_case_insensitive() {
    let tracker = tracker_for("sources_remove_prompt_uses_stored_name_case_insensitive");

    let start = tracker.start("setup", Some("Create config with mixed-case source"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "Laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end("setup", Some("Create config with mixed-case source"), start);

    let start = tracker.start(
        "run_sources_remove_cancelled",
        Some("Run interactive remove with lowercase filter and cancel it"),
    );
    let mut cmd = tracker.cass_std_command();
    cmd.args(["sources", "remove", "laptop"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn sources remove command");
    child
        .stdin
        .as_mut()
        .expect("child stdin")
        .write_all(b"n\n")
        .expect("write cancel confirmation");
    let output = child.wait_with_output().expect("wait for sources remove");
    tracker.end(
        "run_sources_remove_cancelled",
        Some("Run interactive remove with lowercase filter and cancel it"),
        start,
    );

    let start = tracker.start(
        "verify_prompt",
        Some("Verify prompt used the stored canonical source name"),
    );
    assert!(
        output.status.success(),
        "cancelled remove should exit successfully: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Remove source 'Laptop' from configuration?"),
        "Expected canonical stored name in prompt, got: {stdout}"
    );
    assert!(
        stdout.contains("Cancelled."),
        "Expected cancellation output: {stdout}"
    );
    tracker.end(
        "verify_prompt",
        Some("Verify prompt used the stored canonical source name"),
        start,
    );

    tracker.complete();
}

// =============================================================================
// sources doctor tests
// =============================================================================

/// Test: sources doctor with no sources configured.
#[test]
fn sources_doctor_no_sources() {
    let tracker = tracker_for("sources_doctor_no_sources");

    let start = tracker.start("setup", Some("Create empty config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    tracker.end("setup", Some("Create empty config directory"), start);

    let start = tracker.start(
        "run_sources_doctor",
        Some("Run sources doctor with no sources"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "doctor"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources doctor command");
    tracker.end(
        "run_sources_doctor",
        Some("Run sources doctor with no sources"),
        start,
    );

    let start = tracker.start("verify_output", Some("Verify no sources message"));
    // Should succeed but indicate no sources
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No") && stdout.contains("sources"),
        "Expected no sources message, got: {stdout}"
    );
    tracker.end("verify_output", Some("Verify no sources message"), start);

    tracker.complete();
}

/// Test: sources doctor --json outputs valid JSON.
#[test]
fn sources_doctor_json() {
    let tracker = tracker_for("sources_doctor_json");

    let start = tracker.start(
        "setup",
        Some("Create config with one source for doctor JSON"),
    );
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end(
        "setup",
        Some("Create config with one source for doctor JSON"),
        start,
    );

    let start = tracker.start("run_sources_doctor_json", Some("Run sources doctor --json"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "doctor", "--json"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources doctor --json command");
    tracker.end(
        "run_sources_doctor_json",
        Some("Run sources doctor --json"),
        start,
    );

    let start = tracker.start(
        "verify_json",
        Some("Verify JSON array with laptop diagnostics"),
    );
    // Should output valid JSON (even if connectivity fails). Bead uojcg.8.6:
    // the envelope is now the source-doctor health report (flattened: explicit
    // per-source state + safe next command + mutation_free) plus the detailed
    // raw checks under `diagnostics`.
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");

    assert!(
        json.is_object(),
        "Expected source-doctor health object, got: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(json["schema_version"], 1, "stable schema version");
    assert_eq!(json["mutation_free"], true, "diagnosis is read-only");

    let sources = json["sources"]
        .as_array()
        .expect("sources array in health report");
    assert_eq!(sources.len(), 1, "Expected one source in health report");
    assert_eq!(sources[0]["source_id"], "laptop");
    assert!(
        sources[0]["state"].is_string(),
        "each source carries an explicit doctor state"
    );
    assert_eq!(json["summary"]["total"], 1, "summary counts the source");

    let diagnostics = json["diagnostics"]
        .as_array()
        .expect("diagnostics array of raw checks");
    assert_eq!(
        diagnostics.len(),
        1,
        "raw checks retained under diagnostics"
    );
    assert_eq!(diagnostics[0]["source_id"], "laptop");
    tracker.end(
        "verify_json",
        Some("Verify JSON array with laptop diagnostics"),
        start,
    );

    tracker.complete();
}

/// Test: sources doctor --source filters to specific source.
#[test]
fn sources_doctor_single_source() {
    let tracker = tracker_for("sources_doctor_single_source");

    let start = tracker.start("setup", Some("Create config with two sources"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]

[[sources]]
name = "workstation"
type = "ssh"
host = "dev@work.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end("setup", Some("Create config with two sources"), start);

    let start = tracker.start(
        "run_sources_doctor_filtered",
        Some("Run doctor filtered to laptop"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "doctor", "--source", "laptop", "--json"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources doctor --source command");
    tracker.end(
        "run_sources_doctor_filtered",
        Some("Run doctor filtered to laptop"),
        start,
    );

    let start = tracker.start(
        "verify_filtered_output",
        Some("Verify only laptop in output"),
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");

    // Bead uojcg.8.6: filtered output contains only the laptop source, in the
    // health report's `sources` array (each entry keyed by `source_id`).
    let sources = json["sources"]
        .as_array()
        .expect("sources array in health report");
    assert_eq!(sources.len(), 1, "Should only have one source in output");
    assert_eq!(sources[0]["source_id"], "laptop");
    assert_eq!(json["summary"]["total"], 1);
    tracker.end(
        "verify_filtered_output",
        Some("Verify only laptop in output"),
        start,
    );

    tracker.complete();
}

/// Bead uojcg.8.6: `sources doctor --json` classifies an unreachable remote into
/// an explicit *unreached* state (never folded into healthy), offers a
/// preservation-safe (non-destructive) next command, reports the diagnosis as
/// mutation-free, and in fact does not rewrite the sources config.
#[test]
fn sources_doctor_health_unreachable_is_mutation_free_and_safe() {
    let tracker = tracker_for("sources_doctor_health_unreachable_is_mutation_free_and_safe");

    let start = tracker.start("setup", Some("Config with an unreachable remote"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();

    // `.invalid` is a reserved TLD (RFC 6761): DNS resolution always fails, so
    // the host is deterministically unreachable in any environment.
    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "retired-laptop"
type = "ssh"
host = "user@retired-laptop.invalid"
paths = ["~/.claude/projects"]
"#,
    );

    let before = read_sources_config(&config_dir);
    tracker.end("setup", Some("Config with an unreachable remote"), start);

    let start = tracker.start("run", Some("Run sources doctor --json"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "doctor", "--json"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .output()
        .expect("sources doctor --json command");
    tracker.end("run", Some("Run sources doctor --json"), start);

    let start = tracker.start("verify", Some("Verify unreached classification + safety"));
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");

    assert_eq!(json["mutation_free"], true, "diagnosis must be read-only");

    let sources = json["sources"].as_array().expect("sources array");
    assert_eq!(sources.len(), 1, "one source diagnosed");
    let entry = &sources[0];
    assert_eq!(entry["source_id"], "retired-laptop");
    assert_eq!(
        entry["host_reached"], false,
        "an unreachable host must not be reported as reached"
    );
    let state = entry["state"].as_str().expect("explicit state string");
    assert!(
        matches!(state, "unreachable" | "timeout" | "auth_denied"),
        "unreachable host must classify as an unreached state, got {state}"
    );

    // Unreached is counted in its own bucket, never as healthy.
    assert_eq!(json["summary"]["unreached"], 1, "counted as unreached");
    assert_eq!(json["summary"]["healthy"], 0, "never folded into healthy");

    // The offered next command is preservation-safe (never destructive).
    let next = entry["safe_next_command"]
        .as_str()
        .expect("safe next command for an unreached host");
    let lower = next.to_ascii_lowercase();
    for needle in [
        "--delete",
        "rm -rf",
        "rm -r ",
        "prune",
        "shred",
        "--remove-source-files",
    ] {
        assert!(
            !lower.contains(needle),
            "safe next command must not be destructive: {next:?}"
        );
    }
    tracker.end(
        "verify",
        Some("Verify unreached classification + safety"),
        start,
    );

    // Mutation-free in fact: the config file is byte-identical after the run.
    let after = read_sources_config(&config_dir);
    assert_eq!(
        before, after,
        "sources doctor must not rewrite sources.toml"
    );

    tracker.complete();
}

/// Bead uojcg.8.7: `sources doctor --json` only runs the bounded remote
/// binary/platform probe against a host it actually reached. For an unreachable
/// host it must NOT claim a `cass_version` (the binary was never probed) and must
/// NOT classify the host as `cass_missing`/`old_cass` — the transport failure
/// dominates. The host's platform identity stays present regardless, and the
/// diagnosis remains mutation-free.
#[test]
fn sources_doctor_unreachable_omits_unprobed_remote_binary_8_7() {
    let tracker = tracker_for("sources_doctor_unreachable_omits_unprobed_remote_binary_8_7");

    let start = tracker.start("setup", Some("Config with an unreachable remote"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();

    // `.invalid` (RFC 6761) never resolves: the host is deterministically
    // unreachable, so the bounded cass/platform probe must be skipped entirely.
    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "retired-laptop"
type = "ssh"
host = "user@retired-laptop.invalid"
paths = ["~/.claude/projects"]
"#,
    );

    let before = read_sources_config(&config_dir);
    tracker.end("setup", Some("Config with an unreachable remote"), start);

    let start = tracker.start("run", Some("Run sources doctor --json"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "doctor", "--json"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .output()
        .expect("sources doctor --json command");
    tracker.end("run", Some("Run sources doctor --json"), start);

    let start = tracker.start("verify", Some("Verify no remote-binary overclaim"));
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");

    assert_eq!(json["mutation_free"], true, "diagnosis must be read-only");

    // The transport failure dominates: never a binary-derived state.
    let entry = &json["sources"].as_array().expect("sources array")[0];
    let state = entry["state"].as_str().expect("explicit state string");
    assert!(
        matches!(state, "unreachable" | "timeout" | "auth_denied"),
        "an unreachable host must not be classified by an unprobed binary, got {state}"
    );

    // The diagnostics host_report must NOT carry a cass_version: an unreachable
    // host is never binary-probed, so claiming one would be an overclaim.
    let host_report = &json["diagnostics"].as_array().expect("diagnostics array")[0]["host_report"];
    assert!(
        host_report.get("cass_version").is_none(),
        "unreachable host must not report a probed cass_version: {host_report}"
    );
    // Host identity (platform) is always present, even for an unreachable host.
    assert!(
        host_report["platform"].is_object(),
        "platform identity must be present regardless of reachability"
    );
    tracker.end("verify", Some("Verify no remote-binary overclaim"), start);

    // Mutation-free in fact: the config is byte-identical after the run.
    assert_eq!(
        before,
        read_sources_config(&config_dir),
        "sources doctor must not rewrite sources.toml"
    );

    tracker.complete();
}

/// A deliberately slow external-probe fixture drives the real binary past a
/// one-millisecond fleet budget. Both robot and human surfaces must stop
/// honestly, identify every skipped source, preserve all evidence byte-for-byte,
/// and return a bounded partial diagnosis instead of hanging.
#[test]
fn sources_doctor_budget_exhaustion_is_bounded_explicit_and_mutation_free() {
    let tracker =
        tracker_for("sources_doctor_budget_exhaustion_is_bounded_explicit_and_mutation_free");

    let setup_start = tracker.start(
        "setup",
        Some("Create two-source fixture with a probe slower than the total budget"),
    );
    let tmp = tempfile::TempDir::new().expect("create source-doctor budget fixture");
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&config_dir).expect("create config directory");
    fs::create_dir_all(&data_dir).expect("create data directory");
    let archive_path = data_dir.join("agent_search.db");
    let sync_status_path = data_dir.join("sync_status.json");
    let first_mirror_dir = data_dir.join("remotes").join("budget-first").join("mirror");
    let second_mirror_dir = data_dir
        .join("remotes")
        .join("budget-second")
        .join("mirror");
    fs::create_dir_all(&first_mirror_dir).expect("create first source mirror");
    fs::create_dir_all(&second_mirror_dir).expect("create second source mirror");
    fs::write(&archive_path, b"pre-existing archive evidence\n")
        .expect("write archive evidence sentinel");
    fs::write(
        &sync_status_path,
        serde_json::to_vec_pretty(&serde_json::json!({ "sources": {} }))
            .expect("serialize sync-status evidence"),
    )
    .expect("write sync-status evidence sentinel");
    let first_mirror_marker = first_mirror_dir.join("retained-first.jsonl");
    let second_mirror_marker = second_mirror_dir.join("retained-second.jsonl");
    fs::write(&first_mirror_marker, b"retained first mirror evidence\n")
        .expect("write first mirror evidence sentinel");
    fs::write(&second_mirror_marker, b"retained second mirror evidence\n")
        .expect("write second mirror evidence sentinel");
    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "budget-first"
type = "ssh"
host = "fixture@budget.test"
paths = ["~/.claude/projects"]

[[sources]]
name = "budget-second"
type = "ssh"
host = "fixture@budget.test"
paths = ["~/.codex/sessions"]
"#,
    );
    let probe_path = tmp.path().join("source-doctor-budget-probe.json");
    fs::write(
        &probe_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "host": "fixture@budget.test",
            "os": "Linux",
            "cass_version": env!("CARGO_PKG_VERSION"),
            "remote_path": "nonempty",
            "delay_ms": 5,
        }))
        .expect("serialize source-doctor budget probe"),
    )
    .expect("write source-doctor budget probe");
    let config_path = config_dir.join("cass/sources.toml");
    let config_before = fs::read(&config_path).expect("read source config before diagnosis");
    let probe_before = fs::read(&probe_path).expect("read probe before diagnosis");
    let archive_before = fs::read(&archive_path).expect("read archive evidence before diagnosis");
    let sync_status_before =
        fs::read(&sync_status_path).expect("read sync-status evidence before diagnosis");
    let first_mirror_before =
        fs::read(&first_mirror_marker).expect("read first mirror evidence before diagnosis");
    let second_mirror_before =
        fs::read(&second_mirror_marker).expect("read second mirror evidence before diagnosis");
    let data_members_before =
        directory_membership(&data_dir).expect("list data directory before diagnosis");
    let first_mirror_members_before =
        directory_membership(&first_mirror_dir).expect("list first mirror before diagnosis");
    let second_mirror_members_before =
        directory_membership(&second_mirror_dir).expect("list second mirror before diagnosis");
    tracker.end(
        "setup",
        Some("Create two-source fixture with a probe slower than the total budget"),
        setup_start,
    );

    let robot_start = tracker.start(
        "robot_budget",
        Some("Run real sources doctor JSON surface with a one-millisecond budget"),
    );
    let wall_start = std::time::Instant::now();
    let mut robot_cmd = tracker.cass_std_command();
    robot_cmd
        .args(["sources", "doctor", "--json"])
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .env("CASS_TEST_SOURCES_DOCTOR_PROBE", &probe_path)
        .env("CASS_FLEET_PER_HOST_BUDGET_MS", "20")
        .env("CASS_FLEET_BUDGET_MS", "1")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("NO_COLOR", "1");
    let robot_output = util::timeout::spawn_with_timeout_or_diag(
        robot_cmd,
        "sources_doctor_budget_robot",
        Some(&data_dir),
        std::time::Duration::from_secs(5),
    );
    let robot_wall = wall_start.elapsed();
    tracker.end(
        "robot_budget",
        Some("Run real sources doctor JSON surface with a one-millisecond budget"),
        robot_start,
    );
    assert_eq!(
        robot_output.status.code(),
        Some(1),
        "budget-exhausted diagnosis must be non-healthy; stdout={}; stderr={}",
        String::from_utf8_lossy(&robot_output.stdout),
        String::from_utf8_lossy(&robot_output.stderr)
    );
    assert!(
        robot_wall < std::time::Duration::from_secs(5),
        "source doctor exceeded the E2E hard bound: {robot_wall:?}"
    );
    assert!(
        robot_output.stdout.len() < 256 * 1024 && robot_output.stderr.len() < 256 * 1024,
        "bounded diagnosis emitted unbounded output: stdout={} stderr={}",
        robot_output.stdout.len(),
        robot_output.stderr.len()
    );

    let verify_robot_start = tracker.start(
        "verify_robot",
        Some("Verify partial JSON truthfully reports timeout and skipped work"),
    );
    let payload: Value =
        serde_json::from_slice(&robot_output.stdout).expect("budget output must be valid JSON");
    assert_eq!(payload["mutation_free"], true);
    assert_eq!(payload["budget"]["budget_ms"], 1);
    assert_eq!(payload["budget"]["timed_out"], true);
    assert!(
        payload["budget"]["elapsed_ms"]
            .as_u64()
            .is_some_and(|elapsed| elapsed < 1_000),
        "budget accounting must remain bounded: {}",
        payload["budget"]
    );
    assert_eq!(
        payload["budget"]["recommended_next_probe"],
        "cass sources list --json"
    );
    let skipped = payload["budget"]["skipped_sections"]
        .as_array()
        .expect("skipped source list");
    for source in ["budget-first", "budget-second"] {
        assert!(
            skipped
                .iter()
                .any(|entry| entry == &Value::String(format!("source:{source}"))),
            "budget output must name skipped source {source}: {payload}"
        );
    }
    let sources = payload["sources"].as_array().expect("health sources array");
    assert_eq!(sources.len(), 2);
    assert!(
        sources.iter().all(|source| {
            source["state"] == "timeout"
                && source["host_reached"] == false
                && source["connection_error"]
                    .as_str()
                    .is_some_and(|error| error.contains("timed out"))
        }),
        "every uncompleted source must be an explicit timeout: {payload}"
    );
    let diagnostics = payload["diagnostics"]
        .as_array()
        .expect("raw diagnostics array");
    assert_eq!(diagnostics.len(), 2);
    assert!(diagnostics.iter().all(|diagnostic| {
        diagnostic["host_report"]["status"] == "timed-out"
            && diagnostic["host_report"]["timed_out"] == true
    }));
    tracker.end(
        "verify_robot",
        Some("Verify partial JSON truthfully reports timeout and skipped work"),
        verify_robot_start,
    );

    let human_start = tracker.start(
        "human_budget",
        Some("Run human surface and verify parity with the robot timeout contract"),
    );
    let mut human_cmd = tracker.cass_std_command();
    human_cmd
        .args(["sources", "doctor"])
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .env("CASS_TEST_SOURCES_DOCTOR_PROBE", &probe_path)
        .env("CASS_FLEET_PER_HOST_BUDGET_MS", "20")
        .env("CASS_FLEET_BUDGET_MS", "1")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("NO_COLOR", "1");
    let human_output = util::timeout::spawn_with_timeout_or_diag(
        human_cmd,
        "sources_doctor_budget_human",
        Some(&data_dir),
        std::time::Duration::from_secs(5),
    );
    tracker.end(
        "human_budget",
        Some("Run human surface and verify parity with the robot timeout contract"),
        human_start,
    );
    assert_eq!(human_output.status.code(), Some(1));
    let human = String::from_utf8(human_output.stdout).expect("human output is UTF-8");
    for expected in [
        "Checking source: budget-first",
        "Checking source: budget-second",
        "State codes: source=timeout host=timed-out",
        "Budget:",
        "skipped: source:budget-first, source:budget-second",
        "Next bounded probe: cass sources list --json",
    ] {
        assert!(
            human.contains(expected),
            "human timeout output missing {expected:?}: {human}"
        );
    }

    let mutation_start = tracker.start(
        "verify_mutation_free",
        Some("Prove config, probe, and data membership remained byte-identical"),
    );
    assert_eq!(
        fs::read(&config_path).expect("read source config after diagnosis"),
        config_before
    );
    assert_eq!(
        fs::read(&probe_path).expect("read probe after diagnosis"),
        probe_before
    );
    assert_eq!(
        fs::read(&archive_path).expect("read archive evidence after diagnosis"),
        archive_before
    );
    assert_eq!(
        fs::read(&sync_status_path).expect("read sync-status evidence after diagnosis"),
        sync_status_before
    );
    assert_eq!(
        fs::read(&first_mirror_marker).expect("read first mirror evidence after diagnosis"),
        first_mirror_before
    );
    assert_eq!(
        fs::read(&second_mirror_marker).expect("read second mirror evidence after diagnosis"),
        second_mirror_before
    );
    assert_eq!(
        directory_membership(&data_dir).expect("list data directory after diagnosis"),
        data_members_before
    );
    assert_eq!(
        directory_membership(&first_mirror_dir).expect("list first mirror after diagnosis"),
        first_mirror_members_before
    );
    assert_eq!(
        directory_membership(&second_mirror_dir).expect("list second mirror after diagnosis"),
        second_mirror_members_before
    );
    tracker.end(
        "verify_mutation_free",
        Some("Prove config, probe, and data membership remained byte-identical"),
        mutation_start,
    );
    tracker.complete();
}

/// The slow-probe seam is intentionally tiny and test-only. Reject an
/// excessive delay before any network operation, and preserve both the source
/// configuration and the fixture that caused the error.
#[test]
fn sources_doctor_rejects_unbounded_fixture_delay_before_network_or_mutation() {
    let tracker =
        tracker_for("sources_doctor_rejects_unbounded_fixture_delay_before_network_or_mutation");

    let tmp = tempfile::TempDir::new().expect("create invalid-delay fixture root");
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&config_dir).expect("create config directory");
    fs::create_dir_all(&data_dir).expect("create data directory");
    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "bounded-fixture"
type = "ssh"
host = "fixture@bounded.test"
paths = ["~/.claude/projects"]
"#,
    );
    let probe_path = tmp.path().join("invalid-delay.json");
    fs::write(
        &probe_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "host": "fixture@bounded.test",
            "os": "Linux",
            "cass_version": env!("CARGO_PKG_VERSION"),
            "remote_path": "nonempty",
            "delay_ms": 101,
        }))
        .expect("serialize invalid-delay probe"),
    )
    .expect("write invalid-delay probe");
    let config_path = config_dir.join("cass/sources.toml");
    let config_before = fs::read(&config_path).expect("read config before invalid probe");
    let probe_before = fs::read(&probe_path).expect("read invalid probe before command");
    let data_members_before =
        directory_membership(&data_dir).expect("list data before invalid probe");

    let start = tracker.start(
        "reject",
        Some("Run real binary and reject delay above the fixed fixture ceiling"),
    );
    let mut cmd = tracker.cass_std_command();
    cmd.args(["sources", "doctor", "--json"])
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .env("CASS_TEST_SOURCES_DOCTOR_PROBE", &probe_path)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("NO_COLOR", "1");
    let output = util::timeout::spawn_with_timeout_or_diag(
        cmd,
        "sources_doctor_invalid_delay",
        Some(&data_dir),
        std::time::Duration::from_secs(5),
    );
    tracker.end(
        "reject",
        Some("Run real binary and reject delay above the fixed fixture ceiling"),
        start,
    );
    assert!(
        !output.status.success(),
        "invalid fixture delay must fail closed"
    );
    let rendered = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        rendered.contains("delay_ms") && rendered.contains("100"),
        "error must identify the bounded field and ceiling: {rendered}"
    );
    assert_eq!(
        fs::read(&config_path).expect("read config after invalid probe"),
        config_before
    );
    assert_eq!(
        fs::read(&probe_path).expect("read invalid probe after command"),
        probe_before
    );
    assert_eq!(
        directory_membership(&data_dir).expect("list data after invalid probe"),
        data_members_before
    );
    tracker.complete();
}

#[derive(Clone, Copy)]
enum ReachableDoctorSyncFixture {
    None,
    Failed,
    FutureSuccessful,
}

#[derive(Clone, Copy)]
struct ReachableDoctorScenario {
    expected_state: &'static str,
    cass_version: Option<&'static str>,
    remote_path: &'static str,
    local_mirror_nonempty: bool,
    sync: ReachableDoctorSyncFixture,
}

fn prove_reachable_source_doctor_scenario(
    command_env: &E2eCommandEnvironment,
    fixture_root: &Path,
    scenario: ReachableDoctorScenario,
) -> Result<(), String> {
    let case_root = fixture_root.join(scenario.expected_state);
    let config_dir = case_root.join("config");
    let data_dir = case_root.join("data");
    let mirror_dir = data_dir
        .join("remotes")
        .join("fixture-source")
        .join("mirror");
    fs::create_dir_all(&config_dir)
        .map_err(|err| format!("{}: create config dir: {err}", scenario.expected_state))?;
    fs::create_dir_all(&mirror_dir)
        .map_err(|err| format!("{}: create mirror dir: {err}", scenario.expected_state))?;
    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "fixture-source"
type = "ssh"
host = "fixture@reachable.test"
paths = ["~/.claude/projects"]
"#,
    );

    let mirror_marker = mirror_dir.join("retained-session.jsonl");
    if scenario.local_mirror_nonempty {
        fs::write(&mirror_marker, b"retained source evidence\n")
            .map_err(|err| format!("{}: write mirror marker: {err}", scenario.expected_state))?;
    }

    let db_path = data_dir.join("agent_search.db");
    if matches!(scenario.sync, ReachableDoctorSyncFixture::FutureSuccessful) {
        seed_archive_conversation(&db_path, "codex", "source-doctor-stale-index");
    }
    let db_before = read_optional_bytes(&db_path)
        .map_err(|err| format!("{}: read fixture DB: {err}", scenario.expected_state))?;

    let sync_status_path = data_dir.join("sync_status.json");
    match scenario.sync {
        ReachableDoctorSyncFixture::None => {}
        ReachableDoctorSyncFixture::Failed => {
            let payload = serde_json::json!({
                "sources": {
                    "fixture-source": {
                        "last_sync": 1,
                        "last_result": { "failed": "bounded fixture failure" },
                        "files_synced": 0,
                        "bytes_transferred": 0,
                        "duration_ms": 1
                    }
                }
            });
            fs::write(
                &sync_status_path,
                serde_json::to_vec_pretty(&payload)
                    .map_err(|err| format!("serialize failed-sync fixture: {err}"))?,
            )
            .map_err(|err| format!("{}: write sync status: {err}", scenario.expected_state))?;
        }
        ReachableDoctorSyncFixture::FutureSuccessful => {
            let payload = serde_json::json!({
                "sources": {
                    "fixture-source": {
                        "last_sync": i64::MAX,
                        "last_result": "success",
                        "files_synced": 1,
                        "bytes_transferred": 1,
                        "duration_ms": 1
                    }
                }
            });
            fs::write(
                &sync_status_path,
                serde_json::to_vec_pretty(&payload)
                    .map_err(|err| format!("serialize stale-index fixture: {err}"))?,
            )
            .map_err(|err| format!("{}: write sync status: {err}", scenario.expected_state))?;
        }
    }
    let sync_before = read_optional_bytes(&sync_status_path)
        .map_err(|err| format!("{}: read sync status: {err}", scenario.expected_state))?;

    let probe_path = case_root.join("source-doctor-probe.json");
    let probe_payload = serde_json::json!({
        "host": "fixture@reachable.test",
        "os": "Linux",
        "cass_version": scenario.cass_version,
        "remote_path": scenario.remote_path,
    });
    fs::write(
        &probe_path,
        serde_json::to_vec_pretty(&probe_payload)
            .map_err(|err| format!("serialize source-doctor probe: {err}"))?,
    )
    .map_err(|err| format!("{}: write probe: {err}", scenario.expected_state))?;

    let config_before = fs::read(config_dir.join("cass/sources.toml"))
        .map_err(|err| format!("{}: read config: {err}", scenario.expected_state))?;
    let probe_before = fs::read(&probe_path)
        .map_err(|err| format!("{}: read probe: {err}", scenario.expected_state))?;
    let mirror_before = read_optional_bytes(&mirror_marker)
        .map_err(|err| format!("{}: read mirror marker: {err}", scenario.expected_state))?;
    let data_members_before = directory_membership(&data_dir)
        .map_err(|err| format!("{}: list data dir: {err}", scenario.expected_state))?;
    let remotes_members_before = directory_membership(&data_dir.join("remotes"))
        .map_err(|err| format!("{}: list remotes dir: {err}", scenario.expected_state))?;
    let mirror_members_before = directory_membership(&mirror_dir)
        .map_err(|err| format!("{}: list mirror dir: {err}", scenario.expected_state))?;

    let mut cmd = command_env.cass_std_command();
    cmd.args(["sources", "doctor", "--json"])
        .current_dir(&case_root)
        .env("HOME", &case_root)
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .env("CASS_TEST_SOURCES_DOCTOR_PROBE", &probe_path)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("NO_COLOR", "1");
    let output = util::timeout::spawn_with_timeout_or_diag(
        cmd,
        scenario.expected_state,
        Some(&data_dir),
        std::time::Duration::from_secs(10),
    );
    let exit = output
        .status
        .code()
        .ok_or_else(|| format!("{}: cass terminated by signal", scenario.expected_state))?;
    if exit != 1 {
        return Err(format!(
            "{}: unhealthy native source state must exit 1, got exit={exit}, stdout={}, stderr={}",
            scenario.expected_state,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let payload: Value = serde_json::from_slice(&output.stdout).map_err(|err| {
        format!(
            "{}: stdout was not JSON: {err}; stdout={}",
            scenario.expected_state,
            String::from_utf8_lossy(&output.stdout)
        )
    })?;
    let entry = payload
        .get("sources")
        .and_then(Value::as_array)
        .and_then(|sources| sources.first())
        .ok_or_else(|| format!("{}: missing sources[0]", scenario.expected_state))?;
    let state = entry.get("state").and_then(Value::as_str);
    if state != Some(scenario.expected_state) {
        return Err(format!(
            "{}: observed state {state:?}; payload={payload}",
            scenario.expected_state
        ));
    }
    if entry.get("host_reached").and_then(Value::as_bool) != Some(true)
        || payload.get("mutation_free").and_then(Value::as_bool) != Some(true)
        || payload
            .pointer("/summary/unhealthy")
            .and_then(Value::as_u64)
            != Some(1)
        || payload
            .pointer("/summary/unreached")
            .and_then(Value::as_u64)
            != Some(0)
    {
        return Err(format!(
            "{}: reachable/mutation-free summary contract failed: {payload}",
            scenario.expected_state
        ));
    }
    let local_storage = payload
        .pointer("/diagnostics/0/checks")
        .and_then(Value::as_array)
        .and_then(|checks| {
            checks
                .iter()
                .find(|check| check.get("name").and_then(Value::as_str) == Some("Local Storage"))
        })
        .ok_or_else(|| {
            format!(
                "{}: missing Local Storage diagnostic",
                scenario.expected_state
            )
        })?;
    if local_storage.get("status").and_then(Value::as_str) != Some("pass")
        || !local_storage
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| {
                message.contains("appears writable") && message.contains("no write attempted")
            })
    {
        return Err(format!(
            "{}: local storage diagnostic overclaimed its read-only probe: {local_storage}",
            scenario.expected_state
        ));
    }
    let safe_next = entry
        .get("safe_next_command")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{}: missing safe_next_command", scenario.expected_state))?;
    let safe_lower = safe_next.to_ascii_lowercase();
    if [
        "--delete",
        "rm -rf",
        "rm -r ",
        "shred",
        "--remove-source-files",
    ]
    .iter()
    .any(|needle| safe_lower.contains(needle))
    {
        return Err(format!(
            "{}: destructive safe_next_command {safe_next:?}",
            scenario.expected_state
        ));
    }

    let host_status = payload
        .pointer("/diagnostics/0/host_report/status")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{}: missing host status", scenario.expected_state))?;
    if matches!(scenario.expected_state, "cass_missing" | "old_cass") {
        let expected_host_status = if scenario.expected_state == "cass_missing" {
            "command-not-found"
        } else {
            "old-binary-skew"
        };
        let expected_root_cause = if scenario.expected_state == "cass_missing" {
            "unknown"
        } else {
            "old-binary-skew"
        };
        if host_status != expected_host_status
            || payload
                .pointer("/diagnostics/0/host_report/cass_version")
                .and_then(Value::as_str)
                != scenario.cass_version
            || payload
                .pointer("/diagnostics/0/host_report/likely_root_cause")
                .and_then(Value::as_str)
                != Some(expected_root_cause)
            || payload
                .pointer("/diagnostics/0/host_report/root_cause/family")
                .and_then(Value::as_str)
                != Some(expected_root_cause)
            || !payload
                .pointer("/diagnostics/0/host_report/recommended_action")
                .and_then(Value::as_str)
                .is_some_and(|action| !action.trim().is_empty())
        {
            return Err(format!(
                "{}: binary host report contradicts native source state: {payload}",
                scenario.expected_state
            ));
        }
    }

    let mut human_cmd = command_env.cass_std_command();
    human_cmd
        .args(["sources", "doctor"])
        .current_dir(&case_root)
        .env("HOME", &case_root)
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .env("CASS_TEST_SOURCES_DOCTOR_PROBE", &probe_path)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("NO_COLOR", "1");
    let human_output = util::timeout::spawn_with_timeout_or_diag(
        human_cmd,
        &format!("{}_human", scenario.expected_state),
        Some(&data_dir),
        std::time::Duration::from_secs(10),
    );
    let human_exit = human_output.status.code().ok_or_else(|| {
        format!(
            "{} human: cass terminated by signal",
            scenario.expected_state
        )
    })?;
    if human_exit != 1 {
        return Err(format!(
            "{} human: unhealthy native source state must exit 1, got exit={human_exit}, stdout={}, stderr={}",
            scenario.expected_state,
            String::from_utf8_lossy(&human_output.stdout),
            String::from_utf8_lossy(&human_output.stderr)
        ));
    }
    let human = String::from_utf8(human_output.stdout)
        .map_err(|err| format!("{} human stdout not UTF-8: {err}", scenario.expected_state))?;
    let expected_codes = format!(
        "State codes: source={} host={host_status}",
        scenario.expected_state
    );
    if !human.contains(&expected_codes)
        || !human.contains(&format!("Next safe command: {safe_next}"))
        || !human.contains("Readiness: attention-required")
        || !human.contains("Host reached: yes")
        || !human.contains("Why: ")
    {
        return Err(format!(
            "{}: human/robot native-state parity failed; expected codes={expected_codes:?}, safe_next={safe_next:?}, stdout={human}",
            scenario.expected_state
        ));
    }

    let sync_after = read_optional_bytes(&sync_status_path)
        .map_err(|err| format!("{}: re-read sync status: {err}", scenario.expected_state))?;
    let db_after = read_optional_bytes(&db_path)
        .map_err(|err| format!("{}: re-read fixture DB: {err}", scenario.expected_state))?;
    let config_after = fs::read(config_dir.join("cass/sources.toml"))
        .map_err(|err| format!("{}: re-read config: {err}", scenario.expected_state))?;
    let probe_after = fs::read(&probe_path)
        .map_err(|err| format!("{}: re-read probe: {err}", scenario.expected_state))?;
    let mirror_after = read_optional_bytes(&mirror_marker)
        .map_err(|err| format!("{}: re-read mirror marker: {err}", scenario.expected_state))?;
    let data_members_after = directory_membership(&data_dir)
        .map_err(|err| format!("{}: re-list data dir: {err}", scenario.expected_state))?;
    let remotes_members_after = directory_membership(&data_dir.join("remotes"))
        .map_err(|err| format!("{}: re-list remotes dir: {err}", scenario.expected_state))?;
    let mirror_members_after = directory_membership(&mirror_dir)
        .map_err(|err| format!("{}: re-list mirror dir: {err}", scenario.expected_state))?;
    if !config_after.cmp(&config_before).is_eq()
        || !probe_after.cmp(&probe_before).is_eq()
        || !sync_after.cmp(&sync_before).is_eq()
        || !db_after.cmp(&db_before).is_eq()
        || !mirror_after.cmp(&mirror_before).is_eq()
        || !data_members_after.cmp(&data_members_before).is_eq()
        || !remotes_members_after.cmp(&remotes_members_before).is_eq()
        || !mirror_members_after.cmp(&mirror_members_before).is_eq()
    {
        return Err(format!(
            "{}: sources doctor mutated fixture evidence",
            scenario.expected_state
        ));
    }

    Ok(())
}

/// Bead hv29t: drive every reachable source-doctor failure state through the
/// real `cass` binary while injecting only the external SSH probe facts. The
/// live reducers still assess version skew, gather cass-owned local sync/index
/// evidence, apply preservation precedence, build the health report, and
/// serialize the robot envelope.
#[test]
fn sources_doctor_reachable_states_use_live_real_binary_reduction() -> Result<(), String> {
    let tracker = tracker_for("sources_doctor_reachable_states_use_live_real_binary_reduction");
    let command_env = tracker.command_environment();
    let scenarios = [
        ReachableDoctorScenario {
            expected_state: "cass_missing",
            cass_version: None,
            remote_path: "nonempty",
            local_mirror_nonempty: false,
            sync: ReachableDoctorSyncFixture::None,
        },
        ReachableDoctorScenario {
            expected_state: "old_cass",
            cass_version: Some("0.0.1"),
            remote_path: "nonempty",
            local_mirror_nonempty: false,
            sync: ReachableDoctorSyncFixture::None,
        },
        ReachableDoctorScenario {
            expected_state: "mirror_ahead",
            cass_version: Some(env!("CARGO_PKG_VERSION")),
            remote_path: "empty",
            local_mirror_nonempty: true,
            sync: ReachableDoctorSyncFixture::None,
        },
        ReachableDoctorScenario {
            expected_state: "mirror_behind",
            cass_version: Some(env!("CARGO_PKG_VERSION")),
            remote_path: "nonempty",
            local_mirror_nonempty: false,
            sync: ReachableDoctorSyncFixture::Failed,
        },
        ReachableDoctorScenario {
            expected_state: "stale_index",
            cass_version: Some(env!("CARGO_PKG_VERSION")),
            remote_path: "nonempty",
            local_mirror_nonempty: false,
            sync: ReachableDoctorSyncFixture::FutureSuccessful,
        },
        ReachableDoctorScenario {
            expected_state: "remote_pruned",
            cass_version: Some(env!("CARGO_PKG_VERSION")),
            remote_path: "missing",
            local_mirror_nonempty: true,
            sync: ReachableDoctorSyncFixture::Failed,
        },
        ReachableDoctorScenario {
            expected_state: "source_root_unreadable",
            cass_version: Some(env!("CARGO_PKG_VERSION")),
            remote_path: "missing",
            local_mirror_nonempty: false,
            sync: ReachableDoctorSyncFixture::None,
        },
    ];
    let tmp = tempfile::tempdir().map_err(|err| format!("create fixture root: {err}"))?;

    for scenario in scenarios {
        prove_reachable_source_doctor_scenario(&command_env, tmp.path(), scenario)?;
    }

    tracker.complete();
    Ok(())
}

// =============================================================================
// sources sync tests
// =============================================================================

/// Test: sources sync with no sources configured.
#[test]
fn sources_sync_no_sources() {
    let tracker = tracker_for("sources_sync_no_sources");

    let start = tracker.start("setup", Some("Create empty config and data directories"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();

    tracker.end(
        "setup",
        Some("Create empty config and data directories"),
        start,
    );

    let start = tracker.start("run_sources_sync", Some("Run sources sync with no sources"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "sync"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .output()
        .expect("sources sync command");
    tracker.end(
        "run_sources_sync",
        Some("Run sources sync with no sources"),
        start,
    );

    let start = tracker.start("verify_output", Some("Verify no sources message"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No") && stdout.contains("sources"),
        "Expected no sources message, got: {stdout}"
    );
    tracker.end("verify_output", Some("Verify no sources message"), start);

    tracker.complete();
}

/// Test: sources sync --dry-run shows what would be synced.
#[test]
fn sources_sync_dry_run() {
    let tracker = tracker_for("sources_sync_dry_run");

    let start = tracker.start("setup", Some("Create config with one source for dry-run"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end(
        "setup",
        Some("Create config with one source for dry-run"),
        start,
    );

    let start = tracker.start(
        "run_sources_sync_dry_run",
        Some("Run sources sync --dry-run"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "sync", "--dry-run"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .output()
        .expect("sources sync --dry-run command");
    tracker.end(
        "run_sources_sync_dry_run",
        Some("Run sources sync --dry-run"),
        start,
    );

    let start = tracker.start("verify_output", Some("Verify dry run mentions source"));
    // Dry run should indicate the source would be synced
    // Note: Will likely fail SSH connectivity, but should still report the source
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    assert!(
        combined.contains("laptop") || combined.contains("dry"),
        "Expected source name or dry run message, got: {combined}"
    );
    tracker.end(
        "verify_output",
        Some("Verify dry run mentions source"),
        start,
    );

    tracker.complete();
}

/// Test: sources sync --source filters to specific source.
#[test]
fn sources_sync_single_source() {
    let tracker = tracker_for("sources_sync_single_source");

    let start = tracker.start(
        "setup",
        Some("Create config with two sources for filtered sync"),
    );
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]

[[sources]]
name = "workstation"
type = "ssh"
host = "dev@work.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end(
        "setup",
        Some("Create config with two sources for filtered sync"),
        start,
    );

    let start = tracker.start(
        "run_sources_sync_filtered",
        Some("Run sync filtered to laptop"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "sync", "--source", "laptop", "--dry-run"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .output()
        .expect("sources sync --source command");
    tracker.end(
        "run_sources_sync_filtered",
        Some("Run sync filtered to laptop"),
        start,
    );

    let start = tracker.start(
        "verify_filtered_output",
        Some("Verify only laptop in output"),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    // Should only mention laptop, not workstation
    assert!(
        combined.contains("laptop"),
        "Expected laptop in output, got: {combined}"
    );
    // The source filter should work even if sync fails due to SSH
    tracker.end(
        "verify_filtered_output",
        Some("Verify only laptop in output"),
        start,
    );

    tracker.complete();
}

/// Test: sources sync --json outputs valid JSON.
#[test]
fn sources_sync_json() {
    let tracker = tracker_for("sources_sync_json");

    let start = tracker.start("setup", Some("Create config for sync JSON test"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end("setup", Some("Create config for sync JSON test"), start);

    let start = tracker.start(
        "run_sources_sync_json",
        Some("Run sources sync --json --dry-run"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "sync", "--json", "--dry-run"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .output()
        .expect("sources sync --json command");
    tracker.end(
        "run_sources_sync_json",
        Some("Run sources sync --json --dry-run"),
        start,
    );

    let start = tracker.start("verify_json", Some("Verify valid JSON output"));
    // Should output valid JSON even if sync fails
    if !output.stdout.is_empty() {
        let json: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("valid JSON output");

        // Should have a sources or results field
        assert!(
            json.get("sources").is_some() || json.get("results").is_some(),
            "Expected sources or results field in JSON output: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let sources = json["sources"].as_array().expect("sources should be array");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0]["source"], "laptop");
        assert_eq!(sources[0]["status"], "dry_run");
        assert_eq!(sources[0]["sync_decision"]["action"], "sync");
        assert_eq!(sources[0]["sync_decision"]["stale_value_score"], 100);
        assert_eq!(sources[0]["sync_decision"]["manual_override"], true);
        assert!(
            sources[0]["sync_decision"]["reasons"]
                .as_array()
                .is_some_and(|reasons| !reasons.is_empty())
        );
        let transport_decision = &sources[0]["transport_decision"];
        assert!(
            transport_decision["chosen_transport"].as_str().is_some(),
            "transport decision should name the chosen transport: {transport_decision}"
        );
        assert!(
            transport_decision["auth_source"].as_str().is_some(),
            "transport decision should name auth source: {transport_decision}"
        );
        assert!(
            transport_decision["fallback_rationale"].as_str().is_some(),
            "transport decision should explain fallback rationale: {transport_decision}"
        );
        let attempts = transport_decision["attempted_transports"]
            .as_array()
            .expect("transport attempts should be an array");
        let transports = attempts
            .iter()
            .filter_map(|attempt| attempt["transport"].as_str())
            .collect::<Vec<_>>();
        assert!(transports.contains(&"rsync"));
        assert!(transports.contains(&"wsl-rsync"));
        assert!(transports.contains(&"scp"));
        assert!(transports.contains(&"sftp"));
        assert!(
            attempts.iter().any(|attempt| attempt["status"] == "chosen"),
            "one transport attempt should be chosen: {attempts:?}"
        );
        assert!(
            attempts
                .iter()
                .all(|attempt| attempt["auth_source"].as_str().is_some()),
            "each transport attempt should record auth source: {attempts:?}"
        );
        let paths = sources[0]["paths"]
            .as_array()
            .expect("paths should be an array");
        assert!(
            paths
                .iter()
                .all(|path| path.get("failure_reason").is_some()),
            "each path should include failure_reason: {paths:?}"
        );

        let mut command = tracker.cass_assert_command();
        let env_output = command
            .args(["sources", "sync", "--dry-run"])
            .env("XDG_CONFIG_HOME", &config_dir)
            .env("XDG_DATA_HOME", &data_dir)
            .env("CASS_OUTPUT_FORMAT", "json")
            .output()
            .expect("sources sync env-json command");
        assert!(
            env_output.status.success(),
            "env-json sync failed: {}\nstderr: {}",
            env_output.status,
            String::from_utf8_lossy(&env_output.stderr)
        );
        let env_json: serde_json::Value =
            serde_json::from_slice(&env_output.stdout).expect("valid env JSON output");
        let env_sources = env_json["sources"]
            .as_array()
            .expect("env sources should be array");
        assert_eq!(env_sources.len(), 1);
        assert_eq!(env_sources[0]["sync_decision"]["manual_override"], true);
        assert!(
            env_sources[0]["transport_decision"]["chosen_transport"]
                .as_str()
                .is_some()
        );
    }
    tracker.end("verify_json", Some("Verify valid JSON output"), start);

    tracker.complete();
}

// =============================================================================
// Integration workflow tests
// =============================================================================

/// Test: Complete workflow - add, list, remove.
#[test]
fn sources_workflow_add_list_remove() {
    let tracker = tracker_for("sources_workflow_add_list_remove");

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    tracker.end("setup", Some("Create temp config directory"), start);

    // 1. Add a source
    let start = tracker.start("add_source", Some("Add server source"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "add",
            "user@server.example",
            "--name",
            "server",
            "--preset",
            "linux-defaults",
            "--no-test",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources add command");
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    tracker.end("add_source", Some("Add server source"), start);

    // 2. List sources - should show the added source
    let start = tracker.start(
        "list_sources",
        Some("List sources and verify server present"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "list"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources list command");
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("server"));
    tracker.end(
        "list_sources",
        Some("List sources and verify server present"),
        start,
    );

    // 3. Remove the source
    let start = tracker.start("remove_source", Some("Remove server source"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "remove", "server", "-y"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources remove command");
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    tracker.end("remove_source", Some("Remove server source"), start);

    // 4. List again - should be empty
    let start = tracker.start("verify_empty", Some("Verify source was removed"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "list"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources list command");
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("server"),
        "Source should be removed, got: {stdout}"
    );
    tracker.end("verify_empty", Some("Verify source was removed"), start);

    tracker.complete();
}

/// Test: Add multiple sources and list them.
#[test]
fn sources_multiple_add_list() {
    let tracker = tracker_for("sources_multiple_add_list");

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    tracker.end("setup", Some("Create temp config directory"), start);

    // Add first source
    let start = tracker.start("add_laptop", Some("Add laptop source"));
    tracker
        .cass_assert_command()
        .args([
            "sources",
            "add",
            "user@laptop.local",
            "--name",
            "laptop",
            "--preset",
            "macos-defaults",
            "--no-test",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .assert()
        .success();
    tracker.end("add_laptop", Some("Add laptop source"), start);

    // Add second source
    let start = tracker.start("add_workstation", Some("Add workstation source"));
    tracker
        .cass_assert_command()
        .args([
            "sources",
            "add",
            "dev@workstation.office",
            "--name",
            "workstation",
            "--preset",
            "linux-defaults",
            "--no-test",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .assert()
        .success();
    tracker.end("add_workstation", Some("Add workstation source"), start);

    // List all sources
    let start = tracker.start("verify_list", Some("List sources and verify both present"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "list", "--json"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources list command");

    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let sources = json["sources"].as_array().expect("sources array");

    assert_eq!(sources.len(), 2);
    let names: Vec<&str> = sources.iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(names.contains(&"laptop"));
    assert!(names.contains(&"workstation"));
    tracker.end(
        "verify_list",
        Some("List sources and verify both present"),
        start,
    );

    tracker.complete();
}

// =============================================================================
// sources mappings list tests
// =============================================================================

/// Test: sources mappings list with no mappings configured.
#[test]
fn mappings_list_empty() {
    let tracker = tracker_for("mappings_list_empty");

    let start = tracker.start("setup", Some("Create config with source but no mappings"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end(
        "setup",
        Some("Create config with source but no mappings"),
        start,
    );

    let start = tracker.start("run_mappings_list", Some("Run mappings list for laptop"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "mappings", "list", "laptop"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources mappings list command");
    tracker.end(
        "run_mappings_list",
        Some("Run mappings list for laptop"),
        start,
    );

    let start = tracker.start("verify_output", Some("Verify empty mappings message"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No") || stdout.contains("0 mapping"),
        "Expected no mappings message, got: {stdout}"
    );
    tracker.end(
        "verify_output",
        Some("Verify empty mappings message"),
        start,
    );

    tracker.complete();
}

/// Test: sources mappings list with mappings configured.
#[test]
fn mappings_list_with_mappings() {
    let tracker = tracker_for("mappings_list_with_mappings");

    let start = tracker.start("setup", Some("Create config with source and path mapping"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]

[[sources.path_mappings]]
from = "/home/user/projects"
to = "/Users/me/projects"
"#,
    );

    tracker.end(
        "setup",
        Some("Create config with source and path mapping"),
        start,
    );

    let start = tracker.start("run_mappings_list", Some("Run mappings list for laptop"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "mappings", "list", "laptop"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources mappings list command");
    tracker.end(
        "run_mappings_list",
        Some("Run mappings list for laptop"),
        start,
    );

    let start = tracker.start("verify_output", Some("Verify mapping paths in output"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("/home/user/projects") && stdout.contains("/Users/me/projects"),
        "Expected mapping paths in output, got: {stdout}"
    );
    tracker.end(
        "verify_output",
        Some("Verify mapping paths in output"),
        start,
    );

    tracker.complete();
}

/// Test: sources mappings list --json outputs valid JSON.
#[test]
fn mappings_list_json() {
    let tracker = tracker_for("mappings_list_json");

    let start = tracker.start("setup", Some("Create config with mapping for JSON test"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]

[[sources.path_mappings]]
from = "/home/user/projects"
to = "/Users/me/projects"
"#,
    );

    tracker.end(
        "setup",
        Some("Create config with mapping for JSON test"),
        start,
    );

    let start = tracker.start("run_mappings_list_json", Some("Run mappings list --json"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "mappings", "list", "laptop", "--json"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources mappings list --json command");
    tracker.end(
        "run_mappings_list_json",
        Some("Run mappings list --json"),
        start,
    );

    let start = tracker.start("verify_json", Some("Verify JSON contains mappings field"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");

    assert!(
        json.get("mappings").is_some(),
        "Expected 'mappings' field in JSON"
    );
    tracker.end(
        "verify_json",
        Some("Verify JSON contains mappings field"),
        start,
    );

    tracker.complete();
}

/// Test: sources mappings list for nonexistent source.
#[test]
fn mappings_list_nonexistent_source() {
    let tracker = tracker_for("mappings_list_nonexistent_source");

    let start = tracker.start("setup", Some("Create config with laptop source"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end("setup", Some("Create config with laptop source"), start);

    let start = tracker.start(
        "run_mappings_list",
        Some("List mappings for nonexistent source"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "mappings", "list", "nonexistent"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources mappings list command");
    tracker.end(
        "run_mappings_list",
        Some("List mappings for nonexistent source"),
        start,
    );

    let start = tracker.start("verify_error", Some("Verify not found error"));
    assert!(
        !output.status.success(),
        "command should have failed but succeeded with: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found") || stderr.contains("does not exist"),
        "Expected not found error, got: {stderr}"
    );
    tracker.end("verify_error", Some("Verify not found error"), start);

    tracker.complete();
}

// =============================================================================
// sources mappings add tests
// =============================================================================

/// Test: sources mappings add basic mapping.
#[test]
fn mappings_add_basic() {
    let tracker = tracker_for("mappings_add_basic");

    let start = tracker.start("setup", Some("Create config with laptop source"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end("setup", Some("Create config with laptop source"), start);

    let start = tracker.start("run_mappings_add", Some("Add basic path mapping"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "mappings",
            "add",
            "laptop",
            "--from",
            "/remote/path",
            "--to",
            "/local/path",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources mappings add command");
    tracker.end("run_mappings_add", Some("Add basic path mapping"), start);

    let start = tracker.start("verify_config", Some("Verify mapping in config file"));
    assert!(
        output.status.success(),
        "mappings add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify config was updated
    let config_content = read_sources_config(&config_dir);
    assert!(
        config_content.contains("/remote/path") && config_content.contains("/local/path"),
        "Mapping not in config: {config_content}"
    );
    tracker.end(
        "verify_config",
        Some("Verify mapping in config file"),
        start,
    );

    tracker.complete();
}

/// Test: sources mappings add with agent filter.
#[test]
fn mappings_add_with_agents() {
    let tracker = tracker_for("mappings_add_with_agents");

    let start = tracker.start("setup", Some("Create config with laptop source"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end("setup", Some("Create config with laptop source"), start);

    let start = tracker.start("run_mappings_add", Some("Add mapping with agent filter"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "mappings",
            "add",
            "laptop",
            "--from",
            "/opt/work",
            "--to",
            "/Volumes/Work",
            "--agents",
            "claude_code,codex",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources mappings add command");
    tracker.end(
        "run_mappings_add",
        Some("Add mapping with agent filter"),
        start,
    );

    let start = tracker.start("verify_config", Some("Verify agent filter in config"));
    assert!(
        output.status.success(),
        "mappings add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config_content = read_sources_config(&config_dir);
    assert!(
        config_content.contains("claude_code") || config_content.contains("agents"),
        "Agent filter not in config: {config_content}"
    );
    tracker.end(
        "verify_config",
        Some("Verify agent filter in config"),
        start,
    );

    tracker.complete();
}

/// Test: sources mappings add multiple mappings.
#[test]
fn mappings_add_multiple() {
    let tracker = tracker_for("mappings_add_multiple");

    let start = tracker.start("setup", Some("Create config with laptop source"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end("setup", Some("Create config with laptop source"), start);

    // Add first mapping
    let start = tracker.start("add_first_mapping", Some("Add /home/user mapping"));
    tracker
        .cass_assert_command()
        .args([
            "sources",
            "mappings",
            "add",
            "laptop",
            "--from",
            "/home/user",
            "--to",
            "/Users/me",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .assert()
        .success();
    tracker.end("add_first_mapping", Some("Add /home/user mapping"), start);

    // Add second mapping
    let start = tracker.start("add_second_mapping", Some("Add /opt/projects mapping"));
    tracker
        .cass_assert_command()
        .args([
            "sources",
            "mappings",
            "add",
            "laptop",
            "--from",
            "/opt/projects",
            "--to",
            "/Projects",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .assert()
        .success();
    tracker.end(
        "add_second_mapping",
        Some("Add /opt/projects mapping"),
        start,
    );

    // Verify both mappings are in config
    let start = tracker.start("verify_config", Some("Verify both mappings in config"));
    let config_content = read_sources_config(&config_dir);
    assert!(
        config_content.contains("/home/user") && config_content.contains("/opt/projects"),
        "Both mappings not in config: {config_content}"
    );
    tracker.end(
        "verify_config",
        Some("Verify both mappings in config"),
        start,
    );

    tracker.complete();
}

/// Test: sources mappings add to nonexistent source.
#[test]
fn mappings_add_nonexistent_source() {
    let tracker = tracker_for("mappings_add_nonexistent_source");

    let start = tracker.start("setup", Some("Create config with laptop source"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end("setup", Some("Create config with laptop source"), start);

    let start = tracker.start(
        "run_mappings_add",
        Some("Add mapping to nonexistent source"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "mappings",
            "add",
            "nonexistent",
            "--from",
            "/from",
            "--to",
            "/to",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources mappings add command");
    tracker.end(
        "run_mappings_add",
        Some("Add mapping to nonexistent source"),
        start,
    );

    let start = tracker.start("verify_error", Some("Verify not found error"));
    assert!(
        !output.status.success(),
        "command should have failed but succeeded with: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found") || stderr.contains("does not exist"),
        "Expected not found error, got: {stderr}"
    );
    tracker.end("verify_error", Some("Verify not found error"), start);

    tracker.complete();
}

// =============================================================================
// sources mappings remove tests
// =============================================================================

/// Test: sources mappings remove by index.
#[test]
fn mappings_remove_by_index() {
    let tracker = tracker_for("mappings_remove_by_index");

    let start = tracker.start("setup", Some("Create config with two path mappings"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]

[[sources.path_mappings]]
from = "/home/user"
to = "/Users/me"

[[sources.path_mappings]]
from = "/opt/work"
to = "/Work"
"#,
    );

    tracker.end("setup", Some("Create config with two path mappings"), start);

    let start = tracker.start("run_mappings_remove", Some("Remove mapping at index 0"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "mappings", "remove", "laptop", "0"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources mappings remove command");
    tracker.end(
        "run_mappings_remove",
        Some("Remove mapping at index 0"),
        start,
    );

    let start = tracker.start(
        "verify_removal",
        Some("Verify first mapping removed, second kept"),
    );
    assert!(
        output.status.success(),
        "mappings remove failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // First mapping should be gone, second should remain
    let config_content = read_sources_config(&config_dir);
    assert!(
        !config_content.contains("/home/user"),
        "Removed mapping still in config"
    );
    assert!(
        config_content.contains("/opt/work"),
        "Other mapping incorrectly removed"
    );
    tracker.end(
        "verify_removal",
        Some("Verify first mapping removed, second kept"),
        start,
    );

    tracker.complete();
}

/// Test: sources mappings remove with invalid index.
#[test]
fn mappings_remove_invalid_index() {
    let tracker = tracker_for("mappings_remove_invalid_index");

    let start = tracker.start("setup", Some("Create config with one path mapping"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]

[[sources.path_mappings]]
from = "/home/user"
to = "/Users/me"
"#,
    );

    tracker.end("setup", Some("Create config with one path mapping"), start);

    let start = tracker.start(
        "run_mappings_remove",
        Some("Remove mapping at invalid index 99"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "mappings", "remove", "laptop", "99"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources mappings remove command");
    tracker.end(
        "run_mappings_remove",
        Some("Remove mapping at invalid index 99"),
        start,
    );

    let start = tracker.start("verify_error", Some("Verify index out of range error"));
    assert!(
        !output.status.success(),
        "command should have failed but succeeded with: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("index") || stderr.contains("out of") || stderr.contains("range"),
        "Expected index error, got: {stderr}"
    );
    tracker.end(
        "verify_error",
        Some("Verify index out of range error"),
        start,
    );

    tracker.complete();
}

/// Test: sources mappings remove from empty mappings list.
#[test]
fn mappings_remove_from_empty() {
    let tracker = tracker_for("mappings_remove_from_empty");

    let start = tracker.start("setup", Some("Create config with no mappings"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end("setup", Some("Create config with no mappings"), start);

    let start = tracker.start(
        "run_mappings_remove",
        Some("Remove from empty mappings list"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "mappings", "remove", "laptop", "0"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources mappings remove command");
    tracker.end(
        "run_mappings_remove",
        Some("Remove from empty mappings list"),
        start,
    );

    let start = tracker.start("verify_error", Some("Verify empty mappings error"));
    assert!(
        !output.status.success(),
        "command should have failed but succeeded with: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no mapping") || stderr.contains("empty") || stderr.contains("index"),
        "Expected no mappings error, got: {stderr}"
    );
    tracker.end("verify_error", Some("Verify empty mappings error"), start);

    tracker.complete();
}

// =============================================================================
// sources mappings test tests
// =============================================================================

/// Test: sources mappings test with matching path.
#[test]
fn mappings_test_match() {
    let tracker = tracker_for("mappings_test_match");

    let start = tracker.start("setup", Some("Create config with path mapping"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]

[[sources.path_mappings]]
from = "/home/user/projects"
to = "/Users/me/projects"
"#,
    );

    tracker.end("setup", Some("Create config with path mapping"), start);

    let start = tracker.start("run_mappings_test", Some("Test path that matches mapping"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "mappings",
            "test",
            "laptop",
            "/home/user/projects/myapp/src/main.rs",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources mappings test command");
    tracker.end(
        "run_mappings_test",
        Some("Test path that matches mapping"),
        start,
    );

    let start = tracker.start("verify_rewritten_path", Some("Verify path was rewritten"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("/Users/me/projects/myapp/src/main.rs"),
        "Expected rewritten path, got: {stdout}"
    );
    tracker.end(
        "verify_rewritten_path",
        Some("Verify path was rewritten"),
        start,
    );

    tracker.complete();
}

/// Test: sources mappings test with non-matching path.
#[test]
fn mappings_test_no_match() {
    let tracker = tracker_for("mappings_test_no_match");

    let start = tracker.start("setup", Some("Create config with path mapping"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]

[[sources.path_mappings]]
from = "/home/user/projects"
to = "/Users/me/projects"
"#,
    );

    tracker.end("setup", Some("Create config with path mapping"), start);

    let start = tracker.start(
        "run_mappings_test",
        Some("Test path that does not match mapping"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "mappings",
            "test",
            "laptop",
            "/opt/other/path/file.rs",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources mappings test command");
    tracker.end(
        "run_mappings_test",
        Some("Test path that does not match mapping"),
        start,
    );

    let start = tracker.start(
        "verify_unchanged_path",
        Some("Verify path unchanged or no match"),
    );
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Path should be unchanged or indicate no match
    assert!(
        stdout.contains("/opt/other/path/file.rs") || stdout.contains("no match"),
        "Expected unchanged path or no match, got: {stdout}"
    );
    tracker.end(
        "verify_unchanged_path",
        Some("Verify path unchanged or no match"),
        start,
    );

    tracker.complete();
}

/// Test: sources mappings test with agent filter.
#[test]
fn mappings_test_with_agent() {
    let tracker = tracker_for("mappings_test_with_agent");

    let start = tracker.start("setup", Some("Create config with agent-filtered mapping"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]

[[sources.path_mappings]]
from = "/home/user"
to = "/Users/me"
agents = ["claude_code"]
"#,
    );

    tracker.end(
        "setup",
        Some("Create config with agent-filtered mapping"),
        start,
    );

    // Test with matching agent
    let start = tracker.start(
        "run_mappings_test",
        Some("Test mapping with matching agent"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "mappings",
            "test",
            "laptop",
            "/home/user/file.rs",
            "--agent",
            "claude_code",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("sources mappings test command");
    tracker.end(
        "run_mappings_test",
        Some("Test mapping with matching agent"),
        start,
    );

    let start = tracker.start(
        "verify_rewritten_path",
        Some("Verify path rewritten for matching agent"),
    );
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("/Users/me/file.rs"),
        "Expected rewritten path for matching agent, got: {stdout}"
    );
    tracker.end(
        "verify_rewritten_path",
        Some("Verify path rewritten for matching agent"),
        start,
    );

    tracker.complete();
}

// =============================================================================
// mappings workflow integration test
// =============================================================================

/// Test: Complete mappings workflow - add, list, test, remove.
#[test]
fn mappings_workflow_complete() {
    let tracker = tracker_for("mappings_workflow_complete");

    let start = tracker.start("setup", Some("Create config with laptop source"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    create_sources_config(
        &config_dir,
        r#"
[[sources]]
name = "laptop"
type = "ssh"
host = "user@laptop.local"
paths = ["~/.claude/projects"]
"#,
    );

    tracker.end("setup", Some("Create config with laptop source"), start);

    // 1. Add a mapping
    let start = tracker.start("add_mapping", Some("Add path mapping"));
    tracker
        .cass_assert_command()
        .args([
            "sources",
            "mappings",
            "add",
            "laptop",
            "--from",
            "/remote/path",
            "--to",
            "/local/path",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .assert()
        .success();
    tracker.end("add_mapping", Some("Add path mapping"), start);

    // 2. List mappings - should show the added mapping
    let start = tracker.start("list_mappings", Some("List mappings and verify added"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "mappings", "list", "laptop"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("list command");
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("/remote/path"));
    tracker.end(
        "list_mappings",
        Some("List mappings and verify added"),
        start,
    );

    // 3. Test the mapping
    let start = tracker.start("test_mapping", Some("Test path rewriting"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "mappings",
            "test",
            "laptop",
            "/remote/path/subdir/file.rs",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("test command");
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("/local/path/subdir/file.rs"));
    tracker.end("test_mapping", Some("Test path rewriting"), start);

    // 4. Remove the mapping
    let start = tracker.start("remove_mapping", Some("Remove the mapping"));
    tracker
        .cass_assert_command()
        .args(["sources", "mappings", "remove", "laptop", "0"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .assert()
        .success();
    tracker.end("remove_mapping", Some("Remove the mapping"), start);

    // 5. List again - should be empty
    let start = tracker.start("verify_empty", Some("Verify mapping was removed"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "mappings", "list", "laptop"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .output()
        .expect("list command");
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // After removal, should show "No path mappings" message
    assert!(
        stdout.contains("No") || !stdout.contains("/remote/path"),
        "Mapping should be removed, got: {stdout}"
    );
    tracker.end("verify_empty", Some("Verify mapping was removed"), start);

    tracker.complete();
}
