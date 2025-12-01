use assert_cmd::Command;
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
        .stdout(contains("--watch"));
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
