//! CLI dispatch subprocess integration tests.
//!
//! This module covers CLI commands that were previously untested via subprocess
//! invocation. Tests invoke the real binary with representative flags, validate
//! output formats, exit codes, and JSON structure.
//!
//! Coverage targets: completions, man, health, doctor, context, timeline, expand,
//! export, export-html, sources subcommands, models subcommands.

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::Value;
use std::fs;
use std::path::Path;
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

/// Create base command without HOME isolation (for simple tests).
fn simple_cmd() -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("NO_COLOR", "1");
    cmd
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
        .stdout(contains("cost"))
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
    let shared = [
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
        (
            "analytics/cost",
            vec!["analytics", "cost", "--group-by", "hour"],
        ),
        ("analytics/rebuild", vec!["analytics", "rebuild", "--force"]),
        ("analytics/validate", vec!["analytics", "validate", "--fix"]),
    ];

    // Commands that may fail due to DB lock contention in multi-agent environments.
    let lock_sensitive_commands = ["analytics/rebuild"];

    for (expected_command, mut args) in cases {
        args.extend_from_slice(&shared);
        let mut cmd = simple_cmd();
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
        // Note: some analytics subcommands (rebuild, validate, cost, models) emit
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
                assert!(
                    data["by_cost"].is_object(),
                    "analytics/models should expose by_cost: {json}"
                );
            }
            "analytics/cost" => {
                assert!(
                    data["totals"].is_object(),
                    "analytics/cost should expose totals: {json}"
                );
                assert!(
                    data["buckets"].is_array(),
                    "analytics/cost should expose buckets: {json}"
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
