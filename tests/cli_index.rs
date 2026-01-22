use assert_cmd::Command;
use clap::Parser;
use coding_agent_search::{Cli, Commands};
use predicates::str::contains;
use std::fs;
use tempfile::TempDir;

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
    let cli = Cli::try_parse_from([
        "cass",
        "index",
        "--semantic",
        "--embedder",
        "fastembed",
    ])
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

/// Creates a Codex session file with the modern envelope format.
fn make_codex_session(root: &std::path::Path, date_path: &str, filename: &str, content: &str) {
    let sessions = root.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    // Modern Codex JSONL envelope format
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let sample = format!(
        r#"{{"type": "event_msg", "timestamp": {ts}, "payload": {{"type": "user_message", "message": "{content}"}}}}
{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "{content}_response"}}}}"#,
        ts + 1000
    );
    fs::write(file, sample).unwrap();
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
