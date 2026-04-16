use assert_cmd::Command;
use clap::Parser;
use coding_agent_search::storage::sqlite::SqliteStorage;
use coding_agent_search::{Cli, Commands};
use predicates::str::contains;
use serial_test::serial;
use std::fs;
use tempfile::TempDir;

mod util;
use util::EnvGuard;

fn base_cmd(temp_home: &std::path::Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    // Isolate connectors by pointing HOME and XDG vars to temp dir
    cmd.env("HOME", temp_home);
    cmd.env("XDG_DATA_HOME", temp_home.join(".local/share"));
    cmd.env("XDG_CONFIG_HOME", temp_home.join(".config"));
    // Specific overrides if needed (some might fallback to other paths, but HOME usually covers it)
    cmd.env("CODEX_HOME", temp_home.join(".codex"));
    cmd
}

#[test]
fn index_help_prints_usage() {
    let tmp = TempDir::new().unwrap();
    let mut cmd = base_cmd(tmp.path());
    cmd.args(["index", "--help"]);
    cmd.assert()
        .success()
        .stdout(contains("Run indexer"))
        .stdout(contains("--full"))
        .stdout(contains("--watch"))
        .stdout(contains("--semantic"))
        .stdout(contains("--embedder"));
}

#[test]
fn index_parses_semantic_flags() {
    let cli = Cli::try_parse_from(["cass", "index", "--semantic", "--embedder", "fastembed"])
        .expect("parse index flags");

    match cli.command {
        Some(Commands::Index {
            semantic, embedder, ..
        }) => {
            assert!(semantic, "semantic flag should be true");
            assert_eq!(embedder, "fastembed");
        }
        other => panic!("expected index command, got {other:?}"),
    }
}

#[test]
fn index_default_embedder_is_fastembed() {
    let cli = Cli::try_parse_from(["cass", "index", "--semantic"]).expect("parse index flags");

    match cli.command {
        Some(Commands::Index { embedder, .. }) => {
            assert_eq!(embedder, "fastembed");
        }
        other => panic!("expected index command, got {other:?}"),
    }
}

#[test]
fn index_creates_db_and_index() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["index", "--data-dir", data_dir.to_str().unwrap(), "--json"]);

    cmd.assert().success();

    assert!(data_dir.join("agent_search.db").exists(), "DB created");
    // Index dir should exist
    let index_path = data_dir.join("index");
    assert!(index_path.exists(), "index dir created");
}

#[test]
fn index_full_rebuilds() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // First run
    let mut cmd1 = base_cmd(tmp.path());
    cmd1.args(["index", "--data-dir", data_dir.to_str().unwrap(), "--json"]);
    cmd1.assert().success();

    // Second run with --full
    let mut cmd2 = base_cmd(tmp.path());
    cmd2.args([
        "index",
        "--full",
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--json",
    ]);

    cmd2.assert().success();
}

#[test]
fn index_watch_once_triggers() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let dummy_path = data_dir.join("dummy.txt");
    fs::write(&dummy_path, "dummy content").unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "index",
        "--watch-once",
        dummy_path.to_str().unwrap(),
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--json",
    ]);

    cmd.assert().success();
}

#[test]
fn index_force_rebuild_flag() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let mut cmd = base_cmd(tmp.path());
    cmd.args([
        "index",
        "--force-rebuild",
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--json",
    ]);

    cmd.assert().success();
    assert!(data_dir.join("agent_search.db").exists());
}

#[test]
fn index_handles_existing_schema_13_db() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("agent_search.db");

    // Seed an existing DB and force schema_version=13 to guard against
    // regressions where v13 is treated as unsupported.
    let storage = SqliteStorage::open(&db_path).expect("seed sqlite db");
    storage
        .raw()
        .execute("UPDATE meta SET value = '13' WHERE key = 'schema_version'")
        .expect("set schema_version to 13");
    drop(storage);

    let mut cmd = base_cmd(tmp.path());
    cmd.args(["index", "--data-dir", data_dir.to_str().unwrap(), "--json"]);

    let output = cmd.output().expect("run index");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "index should succeed for existing schema v13 db. stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        !stderr.contains("unsupported schema version 13"),
        "stderr should not contain schema-v13 rejection. stderr: {stderr}"
    );

    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("index --json should emit valid JSON");
    assert_eq!(payload.get("success").and_then(|v| v.as_bool()), Some(true));
}

/// Creates a Codex session file with the modern envelope format.
fn codex_iso_timestamp(ts_ms: u64) -> String {
    let ts_ms_i64 = i64::try_from(ts_ms).unwrap_or(i64::MAX);
    chrono::DateTime::from_timestamp_millis(ts_ms_i64)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339()
}

fn make_codex_session(root: &std::path::Path, date_path: &str, filename: &str, content: &str) {
    let sessions = root.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let workspace = root.to_string_lossy();
    let lines = [
        serde_json::json!({
            "timestamp": codex_iso_timestamp(ts),
            "type": "session_meta",
            "payload": {
                "id": filename,
                "cwd": workspace,
                "cli_version": "0.42.0"
            }
        }),
        serde_json::json!({
            "timestamp": codex_iso_timestamp(ts + 1_000),
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": content }]
            }
        }),
        serde_json::json!({
            "timestamp": codex_iso_timestamp(ts + 2_000),
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "text", "text": format!("{content}_response") }]
            }
        }),
    ];
    let mut sample = String::new();
    for line in lines {
        sample.push_str(&serde_json::to_string(&line).unwrap());
        sample.push('\n');
    }
    fs::write(file, sample).unwrap();
}

#[test]
#[serial]
fn index_json_reports_full_refresh_lexical_strategy() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "strategy-full.jsonl",
        "full_strategy_content",
    );

    let mut cmd = base_cmd(home);
    cmd.args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    let output = cmd.output().expect("run full index");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "full index should succeed. stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        !stdout.trim().is_empty(),
        "full index --json should emit stdout. stderr: {stderr}"
    );

    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    let stats = payload
        .get("indexing_stats")
        .and_then(|value| value.as_object())
        .expect("indexing_stats object");

    assert_eq!(
        stats
            .get("lexical_strategy")
            .and_then(|value| value.as_str()),
        Some("deferred_authoritative_db_rebuild")
    );
    assert_eq!(
        stats
            .get("lexical_strategy_reason")
            .and_then(|value| value.as_str()),
        Some("full_refresh_defers_inline_lexical_writes_to_authoritative_db_rebuild")
    );
}

#[test]
#[serial]
fn index_json_reports_repeat_full_refresh_strategy_on_populated_canonical_db() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "strategy-canonical.jsonl",
        "canonical_only_strategy_content",
    );

    let mut initial_index = base_cmd(home);
    initial_index
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir);
    initial_index.assert().success();

    let mut cmd = base_cmd(home);
    cmd.args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    let output = cmd.output().expect("run canonical-only full rebuild");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "canonical-only full rebuild should succeed. stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        !stdout.trim().is_empty(),
        "canonical-only full rebuild --json should emit stdout. stderr: {stderr}"
    );

    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    let stats = payload
        .get("indexing_stats")
        .and_then(|value| value.as_object())
        .expect("indexing_stats object");

    assert_eq!(
        stats
            .get("lexical_strategy")
            .and_then(|value| value.as_str()),
        Some("deferred_authoritative_db_rebuild")
    );
    assert_eq!(
        stats
            .get("lexical_strategy_reason")
            .and_then(|value| value.as_str()),
        Some("full_refresh_defers_inline_lexical_writes_to_authoritative_db_rebuild")
    );
}

#[test]
#[serial]
fn index_json_reports_incremental_lexical_strategy() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "strategy-incremental-1.jsonl",
        "incremental_strategy_content_alpha",
    );

    let mut initial_index = base_cmd(home);
    initial_index
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir);
    initial_index.assert().success();

    std::thread::sleep(std::time::Duration::from_secs(2));
    make_codex_session(
        &codex_home,
        "2025/11/25",
        "strategy-incremental-2.jsonl",
        "incremental_strategy_content_beta",
    );

    let mut cmd = base_cmd(home);
    cmd.args(["index", "--json", "--data-dir"]).arg(&data_dir);
    let output = cmd.output().expect("run incremental index");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "incremental index should succeed. stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        !stdout.trim().is_empty(),
        "incremental index --json should emit stdout. stderr: {stderr}"
    );

    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    let stats = payload
        .get("indexing_stats")
        .and_then(|value| value.as_object())
        .expect("indexing_stats object");

    assert_eq!(
        stats
            .get("lexical_strategy")
            .and_then(|value| value.as_str()),
        Some("incremental_inline")
    );
    assert_eq!(
        stats
            .get("lexical_strategy_reason")
            .and_then(|value| value.as_str()),
        Some("incremental_scan_applies_inline_lexical_updates_only_for_new_messages")
    );
}

#[test]
#[serial]
fn index_json_reports_watch_once_incremental_lexical_strategy() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "strategy-watch-once-1.jsonl",
        "watch_once_strategy_seed",
    );

    let mut initial_index = base_cmd(home);
    initial_index
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir);
    initial_index.assert().success();

    std::thread::sleep(std::time::Duration::from_secs(2));
    let targeted_path = codex_home.join("sessions/2025/11/25/strategy-watch-once-2.jsonl");
    make_codex_session(
        &codex_home,
        "2025/11/25",
        "strategy-watch-once-2.jsonl",
        "watch_once_strategy_delta",
    );

    let mut cmd = base_cmd(home);
    cmd.args(["index", "--watch-once"])
        .arg(&targeted_path)
        .args(["--json", "--data-dir"])
        .arg(&data_dir);
    let output = cmd.output().expect("run targeted watch-once index");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "watch-once incremental index should succeed. stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        !stdout.trim().is_empty(),
        "watch-once incremental index --json should emit stdout. stderr: {stderr}"
    );

    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    let stats = payload
        .get("indexing_stats")
        .and_then(|value| value.as_object())
        .expect("indexing_stats object");

    assert_eq!(
        stats
            .get("lexical_strategy")
            .and_then(|value| value.as_str()),
        Some("incremental_inline")
    );
    assert_eq!(
        stats
            .get("lexical_strategy_reason")
            .and_then(|value| value.as_str()),
        Some("watch_once_targeted_reindex_applies_inline_lexical_updates_for_changed_paths")
    );
}

#[test]
#[serial]
fn plain_index_recreates_missing_lexical_checkpoint_from_live_assets() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    make_codex_session(
        &codex_home,
        "2025/11/24",
        "checkpoint-bootstrap.jsonl",
        "checkpoint_bootstrap_content",
    );

    let mut initial_index = base_cmd(home);
    initial_index
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    initial_index.assert().success();

    let index_path = coding_agent_search::search::tantivy::index_dir(&data_dir)
        .expect("resolve versioned tantivy index path");
    let state_path = index_path.join(".lexical-rebuild-state.json");
    let state_backup_path = index_path.join(".lexical-rebuild-state.backup.json");
    if state_path.exists() {
        fs::rename(&state_path, &state_backup_path).expect("hide lexical checkpoint");
    }
    assert!(
        !state_path.exists(),
        "test fixture should remove the visible lexical checkpoint"
    );

    let mut plain_index = base_cmd(home);
    plain_index
        .args(["index", "--json", "--data-dir"])
        .arg(&data_dir);
    let output = plain_index.output().expect("run plain incremental index");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "plain incremental index should repair the missing lexical checkpoint. stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        state_path.exists(),
        "plain incremental index should recreate the lexical checkpoint"
    );

    let checkpoint: serde_json::Value =
        serde_json::from_slice(&fs::read(&state_path).expect("read recreated checkpoint"))
            .expect("parse recreated checkpoint");
    assert_eq!(checkpoint["completed"], serde_json::Value::Bool(true));

    let mut health = base_cmd(home);
    health
        .args(["health", "--json", "--data-dir"])
        .arg(&data_dir);
    let health_output = health
        .output()
        .expect("run health after checkpoint bootstrap");
    assert!(
        health_output.status.success(),
        "health should stay green after checkpoint bootstrap\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&health_output.stdout),
        String::from_utf8_lossy(&health_output.stderr)
    );
}

/// Test incremental indexing: creates sessions, indexes, adds more, re-indexes,
/// and verifies only new sessions are processed while all remain searchable.
#[test]
fn incremental_index_only_processes_new_sessions() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Phase 1: Create initial 5 sessions
    make_codex_session(
        &codex_home,
        "2025/11/20",
        "rollout-1.jsonl",
        "alpha_content",
    );
    make_codex_session(&codex_home, "2025/11/20", "rollout-2.jsonl", "beta_content");
    make_codex_session(
        &codex_home,
        "2025/11/21",
        "rollout-1.jsonl",
        "gamma_content",
    );
    make_codex_session(
        &codex_home,
        "2025/11/21",
        "rollout-2.jsonl",
        "delta_content",
    );
    make_codex_session(
        &codex_home,
        "2025/11/22",
        "rollout-1.jsonl",
        "epsilon_content",
    );

    // Full index
    let mut cmd1 = base_cmd(home);
    cmd1.env("CODEX_HOME", &codex_home);
    cmd1.args([
        "index",
        "--full",
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--json",
    ]);
    cmd1.assert().success();

    // Verify all 5 sessions indexed - search for unique content
    for term in [
        "alpha_content",
        "beta_content",
        "gamma_content",
        "delta_content",
        "epsilon_content",
    ] {
        let mut search = base_cmd(home);
        search.env("CODEX_HOME", &codex_home);
        search.args([
            "search",
            term,
            "--robot",
            "--data-dir",
            data_dir.to_str().unwrap(),
        ]);
        let output = search.output().expect("search command");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "search should succeed for {term}. stdout: {stdout}, stderr: {stderr}"
        );
        let json: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("valid json output");
        let hits = json
            .get("hits")
            .and_then(|h| h.as_array())
            .expect("hits array");
        assert!(
            !hits.is_empty(),
            "Should find hit for {term} after initial index. Full response: {stdout}"
        );
    }

    // Phase 2: Wait to ensure mtime difference, then add 2 new sessions
    std::thread::sleep(std::time::Duration::from_secs(2));

    make_codex_session(&codex_home, "2025/11/23", "rollout-1.jsonl", "zeta_content");
    make_codex_session(&codex_home, "2025/11/23", "rollout-2.jsonl", "eta_content");

    // Incremental index (no --full flag)
    let mut cmd2 = base_cmd(home);
    cmd2.env("CODEX_HOME", &codex_home);
    cmd2.args(["index", "--data-dir", data_dir.to_str().unwrap(), "--json"]);
    cmd2.assert().success();

    // Verify all 7 sessions are now searchable
    for term in [
        "alpha_content",
        "beta_content",
        "gamma_content",
        "delta_content",
        "epsilon_content",
        "zeta_content",
        "eta_content",
    ] {
        let mut search = base_cmd(home);
        search.env("CODEX_HOME", &codex_home);
        search.args([
            "search",
            term,
            "--robot",
            "--data-dir",
            data_dir.to_str().unwrap(),
        ]);
        let output = search.output().expect("search command");
        assert!(output.status.success(), "search should succeed");
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
        let hits = json
            .get("hits")
            .and_then(|h| h.as_array())
            .expect("hits array");
        assert!(
            !hits.is_empty(),
            "Should find hit for {term} after incremental index"
        );
    }

    // Verify the new sessions specifically
    let mut search_zeta = base_cmd(home);
    search_zeta.env("CODEX_HOME", &codex_home);
    search_zeta.args([
        "search",
        "zeta_content",
        "--robot",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);
    let output = search_zeta.output().expect("search command");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");
    assert!(
        !hits.is_empty(),
        "Should find at least one hit for zeta_content"
    );
    assert_eq!(
        hits[0]["agent"], "codex",
        "Hit should be from codex connector"
    );
}
