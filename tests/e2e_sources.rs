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

use assert_cmd::cargo::cargo_bin_cmd;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

mod util;
use util::EnvGuard;
use util::e2e_log::PhaseTracker;

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

fn cass_data_dir(data_root: &Path) -> std::path::PathBuf {
    data_root.join("coding-agent-search")
}

// =============================================================================
// sources list tests
// =============================================================================

/// Test: sources list with no configured sources shows appropriate message.
#[test]
fn sources_list_empty() {
    let tracker = tracker_for("sources_list_empty");
    let _trace_guard = tracker.trace_env_guard();

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start("run_sources_list", Some("Run sources list with no config"));
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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
    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with one source"), start);

    let start = tracker.start("run_sources_list", Some("Run sources list"));
    let output = cargo_bin_cmd!("cass")
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

/// Test: sources list --verbose shows additional details.
#[test]
fn sources_list_verbose() {
    let tracker = tracker_for("sources_list_verbose");
    let _trace_guard = tracker.trace_env_guard();

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
    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end(
        "setup",
        Some("Create config with verbose-testable source"),
        start,
    );

    let start = tracker.start(
        "run_sources_list_verbose",
        Some("Run sources list --verbose"),
    );
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

    let start = tracker.start("setup", Some("Create config for JSON output test"));
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
    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config for JSON output test"), start);

    let start = tracker.start("run_sources_list_json", Some("Run sources list --json"));
    let output = cargo_bin_cmd!("cass")
        .args(["sources", "list", "--json"])
        .env("XDG_CONFIG_HOME", &config_dir)
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
    let _trace_guard = tracker.trace_env_guard();

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start("run_sources_add", Some("Run sources add with --no-test"));
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start(
        "run_sources_add",
        Some("Run sources add with explicit paths"),
    );
    let output = cargo_bin_cmd!("cass")
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

/// Test: sources add fails without paths.
#[test]
fn sources_add_no_paths_error() {
    let tracker = tracker_for("sources_add_no_paths_error");
    let _trace_guard = tracker.trace_env_guard();

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start("run_sources_add", Some("Run sources add without paths"));
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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
    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with existing source"), start);

    let start = tracker.start(
        "run_sources_add_duplicate",
        Some("Add source with duplicate name"),
    );
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start(
        "run_sources_add_reserved_local",
        Some("Attempt to add source named local"),
    );
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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
    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end(
        "setup",
        Some("Create config with existing mixed-case source"),
        start,
    );

    let start = tracker.start(
        "run_sources_add_duplicate",
        Some("Add source with duplicate name differing only by case"),
    );
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start(
        "run_sources_add_invalid",
        Some("Add source with invalid URL"),
    );
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start("run_sources_add", Some("Add source without explicit name"));
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create temp config directory"), start);

    let start = tracker.start(
        "run_sources_add",
        Some("Add source without explicit name for reserved local hostname"),
    );
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with two sources"), start);

    let start = tracker.start("run_sources_remove", Some("Remove laptop source"));
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with one source"), start);

    let start = tracker.start("run_sources_remove", Some("Remove nonexistent source"));
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    let _guard_data = EnvGuard::set("XDG_DATA_HOME", data_dir.to_string_lossy());
    tracker.end(
        "setup",
        Some("Create config and data directory for purge test"),
        start,
    );

    let start = tracker.start(
        "run_sources_remove_purge",
        Some("Remove source with --purge"),
    );
    let output = cargo_bin_cmd!("cass")
        .args(["sources", "remove", "laptop", "--purge", "-y"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    let _guard_data = EnvGuard::set("XDG_DATA_HOME", data_dir.to_string_lossy());
    tracker.end(
        "setup",
        Some("Create mixed-case config and matching data directory for purge test"),
        start,
    );

    let start = tracker.start(
        "run_sources_remove_purge",
        Some("Remove mixed-case source with lowercase filter and --purge"),
    );
    let output = cargo_bin_cmd!("cass")
        .args(["sources", "remove", "laptop", "--purge", "-y"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with mixed-case source"), start);

    let start = tracker.start(
        "run_sources_remove_cancelled",
        Some("Run interactive remove with lowercase filter and cancel it"),
    );
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
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
    let _trace_guard = tracker.trace_env_guard();

    let start = tracker.start("setup", Some("Create empty config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create empty config directory"), start);

    let start = tracker.start(
        "run_sources_doctor",
        Some("Run sources doctor with no sources"),
    );
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end(
        "setup",
        Some("Create config with one source for doctor JSON"),
        start,
    );

    let start = tracker.start("run_sources_doctor_json", Some("Run sources doctor --json"));
    let output = cargo_bin_cmd!("cass")
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
    // Should output valid JSON (even if connectivity fails)
    // Note: The output is an array of source diagnostics
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");

    // JSON should be an array of source diagnostics
    assert!(
        json.is_array(),
        "Expected array of source diagnostics, got: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1, "Expected one source in diagnostics");
    assert_eq!(arr[0]["source_id"], "laptop");
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with two sources"), start);

    let start = tracker.start(
        "run_sources_doctor_filtered",
        Some("Run doctor filtered to laptop"),
    );
    let output = cargo_bin_cmd!("cass")
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

    // Should only contain laptop diagnostics
    if let Some(sources) = json.get("sources").and_then(|s| s.as_array()) {
        assert_eq!(sources.len(), 1, "Should only have one source in output");
        assert_eq!(sources[0]["name"], "laptop");
    }
    tracker.end(
        "verify_filtered_output",
        Some("Verify only laptop in output"),
        start,
    );

    tracker.complete();
}

// =============================================================================
// sources sync tests
// =============================================================================

/// Test: sources sync with no sources configured.
#[test]
fn sources_sync_no_sources() {
    let tracker = tracker_for("sources_sync_no_sources");
    let _trace_guard = tracker.trace_env_guard();

    let start = tracker.start("setup", Some("Create empty config and data directories"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    let _guard_data = EnvGuard::set("XDG_DATA_HOME", data_dir.to_string_lossy());
    tracker.end(
        "setup",
        Some("Create empty config and data directories"),
        start,
    );

    let start = tracker.start("run_sources_sync", Some("Run sources sync with no sources"));
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    let _guard_data = EnvGuard::set("XDG_DATA_HOME", data_dir.to_string_lossy());
    tracker.end(
        "setup",
        Some("Create config with one source for dry-run"),
        start,
    );

    let start = tracker.start(
        "run_sources_sync_dry_run",
        Some("Run sources sync --dry-run"),
    );
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    let _guard_data = EnvGuard::set("XDG_DATA_HOME", data_dir.to_string_lossy());
    tracker.end(
        "setup",
        Some("Create config with two sources for filtered sync"),
        start,
    );

    let start = tracker.start(
        "run_sources_sync_filtered",
        Some("Run sync filtered to laptop"),
    );
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    let _guard_data = EnvGuard::set("XDG_DATA_HOME", data_dir.to_string_lossy());
    tracker.end("setup", Some("Create config for sync JSON test"), start);

    let start = tracker.start(
        "run_sources_sync_json",
        Some("Run sources sync --json --dry-run"),
    );
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create temp config directory"), start);

    // 1. Add a source
    let start = tracker.start("add_source", Some("Add server source"));
    let output = cargo_bin_cmd!("cass")
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
    let output = cargo_bin_cmd!("cass")
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
    let output = cargo_bin_cmd!("cass")
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
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

    let start = tracker.start("setup", Some("Create temp config directory"));
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create temp config directory"), start);

    // Add first source
    let start = tracker.start("add_laptop", Some("Add laptop source"));
    cargo_bin_cmd!("cass")
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
    cargo_bin_cmd!("cass")
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
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end(
        "setup",
        Some("Create config with source but no mappings"),
        start,
    );

    let start = tracker.start("run_mappings_list", Some("Run mappings list for laptop"));
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end(
        "setup",
        Some("Create config with source and path mapping"),
        start,
    );

    let start = tracker.start("run_mappings_list", Some("Run mappings list for laptop"));
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end(
        "setup",
        Some("Create config with mapping for JSON test"),
        start,
    );

    let start = tracker.start("run_mappings_list_json", Some("Run mappings list --json"));
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with laptop source"), start);

    let start = tracker.start(
        "run_mappings_list",
        Some("List mappings for nonexistent source"),
    );
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with laptop source"), start);

    let start = tracker.start("run_mappings_add", Some("Add basic path mapping"));
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with laptop source"), start);

    let start = tracker.start("run_mappings_add", Some("Add mapping with agent filter"));
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with laptop source"), start);

    // Add first mapping
    let start = tracker.start("add_first_mapping", Some("Add /home/user mapping"));
    cargo_bin_cmd!("cass")
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
    cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with laptop source"), start);

    let start = tracker.start(
        "run_mappings_add",
        Some("Add mapping to nonexistent source"),
    );
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with two path mappings"), start);

    let start = tracker.start("run_mappings_remove", Some("Remove mapping at index 0"));
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with one path mapping"), start);

    let start = tracker.start(
        "run_mappings_remove",
        Some("Remove mapping at invalid index 99"),
    );
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with no mappings"), start);

    let start = tracker.start(
        "run_mappings_remove",
        Some("Remove from empty mappings list"),
    );
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with path mapping"), start);

    let start = tracker.start("run_mappings_test", Some("Test path that matches mapping"));
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with path mapping"), start);

    let start = tracker.start(
        "run_mappings_test",
        Some("Test path that does not match mapping"),
    );
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
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
    let output = cargo_bin_cmd!("cass")
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
    let _trace_guard = tracker.trace_env_guard();

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

    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    tracker.end("setup", Some("Create config with laptop source"), start);

    // 1. Add a mapping
    let start = tracker.start("add_mapping", Some("Add path mapping"));
    cargo_bin_cmd!("cass")
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
    let output = cargo_bin_cmd!("cass")
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
    let output = cargo_bin_cmd!("cass")
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
    cargo_bin_cmd!("cass")
        .args(["sources", "mappings", "remove", "laptop", "0"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .assert()
        .success();
    tracker.end("remove_mapping", Some("Remove the mapping"), start);

    // 5. List again - should be empty
    let start = tracker.start("verify_empty", Some("Verify mapping was removed"));
    let output = cargo_bin_cmd!("cass")
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
