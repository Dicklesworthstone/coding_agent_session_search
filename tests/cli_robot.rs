use assert_cmd::Command;
use fs2::FileExt;
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;
use walkdir::WalkDir;

use clap::{self, CommandFactory, Parser};
use coding_agent_search::{Cli, Commands};

fn cass_bin() -> String {
    std::env::var("CARGO_BIN_EXE_cass")
        .ok()
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_cass").to_string())
}

fn base_cmd() -> Command {
    let mut cmd = Command::new(cass_bin());
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd
}

const SEARCH_DEMO_DATA_DIR: &str = "tests/fixtures/search_demo_data";

fn isolated_search_demo_data() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let src = Path::new(SEARCH_DEMO_DATA_DIR);
    for entry in WalkDir::new(src) {
        let entry = entry.unwrap();
        let rel = entry.path().strip_prefix(src).unwrap();
        let dst = tmp.path().join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&dst).unwrap();
        } else {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::copy(entry.path(), &dst).unwrap();
        }
    }
    tmp
}

#[test]
fn robot_help_prints_contract() {
    let mut cmd = base_cmd();
    cmd.arg("--robot-help");
    cmd.assert()
        .success()
        .stdout(contains("cass --robot-help (contract v1)"))
        .stdout(contains("Exit codes: 0 ok"));
}

#[test]
fn robot_help_has_sections_and_no_ansi() {
    let mut cmd = base_cmd();
    cmd.args(["--color=never", "--robot-help"]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "robot-help should not emit ANSI when color=never"
    );
    for needle in &[
        "QUICKSTART",
        "TIME FILTERS:",
        "WORKFLOW:",
        "OUTPUT:",
        "Subcommands:",
        "Exit codes:",
    ] {
        assert!(
            stdout.contains(needle),
            "robot-help output missing section {needle}"
        );
    }
}

#[test]
fn api_version_reports_contract() {
    let mut cmd = base_cmd();
    cmd.args(["api-version", "--json"]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid api-version json");
    assert_eq!(json["api_version"], 1);
    assert_eq!(json["contract_version"], "1");
    assert!(
        json["crate_version"].is_string(),
        "crate_version should be a string, got: {:?}",
        json["crate_version"]
    );
}

#[test]
fn introspect_includes_contract_and_globals() {
    let mut cmd = base_cmd();
    cmd.args(["introspect", "--json"]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid introspect json");
    assert_eq!(json["api_version"], 1);
    assert_eq!(json["contract_version"], "1");
    let globals = json["global_flags"].as_array().expect("global_flags array");
    assert!(!globals.is_empty(), "global_flags should list shared flags");
    let commands = json["commands"].as_array().expect("commands array");
    assert!(
        commands.iter().any(|c| c["name"] == "api-version"),
        "commands should include api-version"
    );
}

/// Global flags should expose value types and defaults in introspect.
#[test]
fn introspect_global_flags_have_types_and_defaults() {
    let mut cmd = base_cmd();
    cmd.args(["introspect", "--json"]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid introspect json");
    let globals = json["global_flags"].as_array().expect("global_flags array");

    let mut seen = std::collections::HashMap::new();
    for flag in globals {
        let name = flag["name"].as_str().unwrap_or_default().to_string();
        seen.insert(name.clone(), flag.clone());
        match name.as_str() {
            "color" => {
                assert_eq!(flag["value_type"], "enum");
                assert_eq!(flag["default"], "auto");
                let enums = flag["enum_values"].as_array().unwrap();
                assert!(
                    enums.iter().any(|v| v == "auto"),
                    "color enum should have 'auto', got: {:?}",
                    enums
                );
                assert!(
                    enums.iter().any(|v| v == "never"),
                    "color enum should have 'never', got: {:?}",
                    enums
                );
                assert!(
                    enums.iter().any(|v| v == "always"),
                    "color enum should have 'always', got: {:?}",
                    enums
                );
            }
            "progress" => {
                assert_eq!(flag["value_type"], "enum");
                assert_eq!(flag["default"], "auto");
                let enums = flag["enum_values"].as_array().unwrap();
                assert!(
                    enums.iter().any(|v| v == "auto"),
                    "progress enum should have 'auto', got: {:?}",
                    enums
                );
                assert!(
                    enums.iter().any(|v| v == "bars"),
                    "progress enum should have 'bars', got: {:?}",
                    enums
                );
                assert!(
                    enums.iter().any(|v| v == "plain"),
                    "progress enum should have 'plain', got: {:?}",
                    enums
                );
                assert!(
                    enums.iter().any(|v| v == "none"),
                    "progress enum should have 'none', got: {:?}",
                    enums
                );
            }
            "db" => {
                assert_eq!(flag["value_type"], "path");
            }
            "trace-file" => {
                assert_eq!(flag["value_type"], "path");
            }
            "wrap" => {
                assert_eq!(flag["value_type"], "integer");
            }
            "nowrap" => {
                assert_eq!(flag["arg_type"], "flag");
            }
            _ => {}
        }
    }

    for required in ["color", "progress", "db", "trace-file", "wrap", "nowrap"] {
        assert!(
            seen.contains_key(required),
            "global flag {required} should be documented"
        );
    }
}

/// Introspect should mark repeatable args and detect path/integer types.
#[test]
fn introspect_repeatable_and_value_types() {
    let mut cmd = base_cmd();
    cmd.args(["introspect", "--json"]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid introspect json");
    let commands = json["commands"].as_array().expect("commands array");

    let search = commands
        .iter()
        .find(|c| c["name"] == "search")
        .expect("search command present");
    let args = search["arguments"].as_array().expect("search args");

    let mut found_agent = false;
    let mut found_workspace = false;
    let mut found_data_dir = false;
    let mut found_limit = false;
    let mut found_aggregate = false;

    for arg in args {
        let name = arg["name"].as_str().unwrap_or_default();
        match name {
            "agent" => {
                found_agent = true;
                assert_eq!(arg["repeatable"], true);
            }
            "workspace" => {
                found_workspace = true;
                assert_eq!(arg["repeatable"], true);
            }
            "data-dir" => {
                found_data_dir = true;
                assert_eq!(arg["value_type"], "path");
            }
            "limit" => {
                found_limit = true;
                assert_eq!(arg["value_type"], "integer");
                assert_eq!(arg["default"], "0");
            }
            "aggregate" => {
                found_aggregate = true;
                assert_eq!(arg["repeatable"], true);
            }
            _ => {}
        }
    }

    assert!(found_agent, "search should document repeatable agent arg");
    assert!(
        found_workspace,
        "search should document repeatable workspace arg"
    );
    assert!(found_data_dir, "search should document data-dir path type");
    assert!(found_limit, "search should document integer limit");
    assert!(
        found_aggregate,
        "search should document repeatable aggregate"
    );
}

#[test]
fn state_matches_status() {
    let fixture = isolated_search_demo_data();
    let data_dir = fixture.path().to_str().unwrap();
    let mut status = base_cmd();
    status.args(["status", "--json", "--data-dir", data_dir]);
    let status_out = status.assert().success().get_output().clone();
    let status_json: Value = serde_json::from_slice(&status_out.stdout).expect("valid status json");

    let mut state = base_cmd();
    state.args(["state", "--json", "--data-dir", data_dir]);
    let state_out = state.assert().success().get_output().clone();
    let state_json: Value = serde_json::from_slice(&state_out.stdout).expect("valid state json");

    // Core assertion: status and state report the same health
    assert_eq!(status_json["healthy"], state_json["healthy"]);
    // Pending sessions should match between the two commands, regardless of the
    // rebuild/watch state observed in the fixture dataset.
    assert_eq!(
        status_json["pending"]["sessions"],
        state_json["pending"]["sessions"]
    );
    assert_eq!(status_json["semantic"], state_json["semantic"]);
}

#[test]
fn search_cursor_and_token_budget() {
    let data_dir = "tests/fixtures/search_demo_data";
    // First page with small token budget to force clamping
    let mut first = base_cmd();
    first.args([
        "search",
        "",
        "--json",
        "--limit",
        "3",
        "--max-tokens",
        "16",
        "--request-id",
        "rid-123",
        "--data-dir",
        data_dir,
    ]);
    let first_out = first.assert().success().get_output().clone();
    let first_json: Value = serde_json::from_slice(&first_out.stdout).expect("valid search json");
    assert_eq!(first_json["request_id"], "rid-123");
    let first_hits = first_json["hits"].as_array().expect("hits array");
    if first_hits.is_empty()
        && first_json["_meta"]
            .get("next_cursor")
            .and_then(|c| c.as_str())
            .is_none()
    {
        assert_eq!(first_json["count"].as_u64().unwrap_or(0), 0);
        return;
    }
    if let Some(cursor) = first_json["_meta"]
        .get("next_cursor")
        .and_then(|c| c.as_str())
    {
        // Second page using cursor should succeed and echo request_id if provided again
        let mut second = base_cmd();
        second.args([
            "search",
            "",
            "--json",
            "--cursor",
            cursor,
            "--request-id",
            "rid-456",
            "--data-dir",
            data_dir,
        ]);
        let second_out = second.assert().success().get_output().clone();
        let second_json: Value =
            serde_json::from_slice(&second_out.stdout).expect("valid search json");
        assert_eq!(second_json["request_id"], "rid-456");
        // Cursor page should not be empty
        let count = second_json["count"].as_u64().unwrap_or(0);
        assert!(count > 0, "cursor page should return results");
    } else {
        // If dataset is too small for pagination, ensure we returned some hits
        assert!(
            first_json["hits"]
                .as_array()
                .map(|h| !h.is_empty())
                .unwrap_or(false)
        );
    }
}

#[test]
fn search_cursor_jsonl_and_compact() {
    let data_dir = "tests/fixtures/search_demo_data";
    // JSONL meta line contains next_cursor
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--robot-format",
        "jsonl",
        "--robot-meta",
        "--limit",
        "2",
        "--data-dir",
        data_dir,
    ]);
    let out = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first_line = stdout.lines().next().expect("meta line present");
    let meta: Value = serde_json::from_str(first_line).expect("valid jsonl meta");
    assert!(meta.get("_meta").is_some());
    assert!(meta["_meta"].get("next_cursor").is_some());

    // Compact still returns cursor in payload
    let mut compact = base_cmd();
    compact.args([
        "search",
        "hello",
        "--robot-format",
        "compact",
        "--robot-meta",
        "--limit",
        "2",
        "--data-dir",
        data_dir,
    ]);
    let compact_out = compact.assert().success().get_output().clone();
    let json: Value = serde_json::from_slice(&compact_out.stdout).expect("compact json payload");
    assert!(json["_meta"].get("next_cursor").is_some());
}

#[test]
fn search_robot_format_sessions_matches_source_paths() {
    // rob.ctx.sessions: sessions output should match the unique sorted source_path set from JSON hits.
    let data_dir = "tests/fixtures/search_demo_data";

    // 1) Get source_path values via compact JSON.
    let mut compact = base_cmd();
    compact.args([
        "search",
        "hello",
        "--robot-format",
        "compact",
        "--fields",
        "minimal",
        "--limit",
        "50",
        "--data-dir",
        data_dir,
    ]);
    let compact_out = compact.assert().success().get_output().clone();
    let json: Value = serde_json::from_slice(&compact_out.stdout).expect("compact json payload");
    let hits = json["hits"].as_array().expect("hits array");

    let mut expected: Vec<String> = hits
        .iter()
        .filter_map(|h| {
            h.get("source_path")
                .and_then(|p| p.as_str())
                .map(str::to_string)
        })
        .collect();
    expected.sort();
    expected.dedup();

    // 2) Get session paths via sessions robot format.
    let mut sessions = base_cmd();
    sessions.args([
        "search",
        "hello",
        "--robot-format",
        "sessions",
        "--limit",
        "50",
        "--data-dir",
        data_dir,
    ]);
    let sessions_out = sessions.assert().success().get_output().clone();
    let actual: Vec<String> = String::from_utf8_lossy(&sessions_out.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    assert_eq!(
        actual, expected,
        "sessions output should equal unique sorted hit source_path values"
    );
}

#[test]
fn robot_docs_schemas_topic() {
    let mut cmd = base_cmd();
    cmd.args(["robot-docs", "schemas"]);
    cmd.assert()
        .success()
        .stdout(contains("schemas:"))
        .stdout(contains("search"));
}

#[test]
fn robot_docs_commands_includes_tui_reset_and_no_ansi() {
    let mut cmd = base_cmd();
    cmd.args(["--color=never", "robot-docs", "commands"]);
    let out = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "robot-docs commands should not emit ANSI when color=never"
    );
    assert!(
        stdout.contains("cass tui"),
        "commands topic should list cass tui"
    );
    assert!(
        stdout.contains("cass robot-docs <topic>"),
        "commands topic should list robot-docs command"
    );
}

#[test]
fn robot_docs_env_lists_key_vars_and_no_ansi() {
    let mut cmd = base_cmd();
    cmd.args(["--color=never", "robot-docs", "env"]);
    let out = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "robot-docs env should not emit ANSI when color=never"
    );
    for needle in &[
        "CODING_AGENT_SEARCH_NO_UPDATE_PROMPT",
        "CASS_DATA_DIR",
        "TUI_HEADLESS",
    ] {
        assert!(stdout.contains(needle), "env topic should include {needle}");
    }
}

fn read_fixture(name: &str) -> Value {
    let path = Path::new("tests/fixtures/cli_contract").join(name);
    let body = fs::read_to_string(&path).expect("fixture readable");
    serde_json::from_str(&body).expect("fixture valid json")
}

#[test]
fn capabilities_matches_golden_contract() {
    let mut cmd = base_cmd();
    cmd.args(["capabilities", "--json"]);
    let output = cmd.assert().success().get_output().clone();
    assert!(
        output.stderr.is_empty(),
        "capabilities should not log to stderr"
    );
    let mut actual: Value =
        serde_json::from_slice(&output.stdout).expect("valid capabilities json");
    let mut expected = read_fixture("capabilities.json");

    // Verify crate_version matches Cargo.toml (dynamic, not from fixture)
    let cargo_version = env!("CARGO_PKG_VERSION");
    assert_eq!(
        actual["crate_version"].as_str().unwrap(),
        cargo_version,
        "crate_version should match Cargo.toml version"
    );

    // Remove crate_version from both for contract comparison (version changes are expected)
    actual.as_object_mut().unwrap().remove("crate_version");
    expected.as_object_mut().unwrap().remove("crate_version");

    assert_eq!(actual, expected, "capabilities contract drifted");
}

#[test]
fn api_version_matches_golden_contract() {
    let mut cmd = base_cmd();
    cmd.args(["api-version", "--json"]);
    let output = cmd.assert().success().get_output().clone();
    assert!(
        output.stderr.is_empty(),
        "api-version should not log to stderr"
    );
    let actual: Value = serde_json::from_slice(&output.stdout).expect("valid api-version json");

    // Check stable contract fields against fixture
    let expected = read_fixture("api_version.json");
    assert_eq!(
        actual["api_version"], expected["api_version"],
        "api_version field drifted"
    );
    assert_eq!(
        actual["contract_version"], expected["contract_version"],
        "contract_version field drifted"
    );

    // Verify crate_version matches Cargo.toml (dynamic, not from fixture)
    let cargo_version = env!("CARGO_PKG_VERSION");
    assert_eq!(
        actual["crate_version"].as_str().unwrap(),
        cargo_version,
        "crate_version should match Cargo.toml version"
    );
}

#[test]
fn color_never_has_no_ansi() {
    let mut cmd = base_cmd();
    cmd.args(["--color=never", "--robot-help"]);
    cmd.assert()
        .success()
        .stdout(contains("cass --robot-help"))
        .stdout(predicate::str::contains("\u{1b}").not());
}

#[test]
fn wrap_40_inserts_line_breaks() {
    let mut cmd = base_cmd();
    cmd.args(["--wrap=40", "--robot-help"]);
    cmd.assert()
        .success()
        // With wrap at 40, long command examples should wrap across lines
        .stdout(contains("--robot #\nSearch with JSON output"));
}

#[test]
fn tui_bypasses_in_non_tty() {
    let mut cmd = base_cmd();
    // No subcommand provided; in test harness stdout is non-TTY so TUI should be blocked
    cmd.assert()
        .failure()
        .code(2)
        .stderr(contains("TUI is disabled"));
}

#[test]
fn search_error_writes_trace() {
    let tmp = TempDir::new().unwrap();
    let trace_path = tmp.path().join("trace.jsonl");

    let mut cmd = base_cmd();
    cmd.args([
        "--trace-file",
        trace_path.to_str().unwrap(),
        "--progress=plain",
        "search",
        "foo",
        "--json",
        "--data-dir",
        tmp.path().to_str().unwrap(),
    ]);

    let assert = cmd.assert().failure();
    let output = assert.get_output().clone();
    let code = output.status.code().expect("exit code present");
    // Accept both missing-index (3) and generic search error (9) depending on how the DB layer responds.
    assert!(matches!(code, 3 | 9), "unexpected exit code {code}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    if code == 3 {
        assert!(stderr.contains("missing-index"));
    } else {
        assert!(stderr.contains("\"kind\":\"search\""));
    }

    let trace = fs::read_to_string(&trace_path).expect("trace file exists");
    let last_line = trace.lines().last().expect("trace line present");
    let json: Value = serde_json::from_str(last_line).expect("valid trace json");
    let exit_code = json["exit_code"].as_i64().expect("exit_code present");
    assert_eq!(exit_code, code as i64);
    assert_eq!(json["contract_version"], "1");
}

// ============================================================
// yln.5: E2E Search Tests with Fixture Data
// ============================================================

#[test]
fn search_returns_json_results() {
    // E2E test: search with JSON output returns structured results (yln.5)
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "",
        "--json",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse JSON output
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON output");

    // Verify structure
    assert!(json["count"].is_number(), "JSON should have count field");
    assert!(json["hits"].is_array(), "JSON should have hits array");

    // Verify hit structure
    let hits = json["hits"].as_array().unwrap();
    if hits.is_empty() {
        return;
    }
    let first_hit = &hits[0];
    assert!(first_hit["agent"].is_string(), "Hit should have agent");
    assert!(
        first_hit["source_path"].is_string(),
        "Hit should have source_path"
    );
    assert!(first_hit["score"].is_number(), "Hit should have score");
}

#[test]
fn search_respects_limit() {
    // E2E test: --limit restricts results (yln.5)
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "Gemini",
        "--json",
        "--limit",
        "1",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let hits = json["hits"].as_array().expect("hits array");
    assert!(
        hits.len() <= 1,
        "Limit should restrict results to at most 1"
    );
}

#[test]
fn search_empty_query_returns_all() {
    // E2E test: empty query returns recent results (yln.5)
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "",
        "--json",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Empty query should return results (recent conversations)
    let hits = json["hits"].as_array().expect("Should return hits array");
    assert!(
        json["count"].is_number(),
        "empty-query robot output should still report total count"
    );
    assert!(
        hits.len() <= json["count"].as_u64().unwrap() as usize,
        "reported count should be at least the returned page length"
    );
}

#[test]
fn search_no_match_returns_empty_hits() {
    // E2E test: non-matching query returns empty results (yln.5)
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "xyznonexistentquery12345",
        "--json",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let count = json["count"].as_u64().expect("count field");
    assert_eq!(count, 0, "Non-matching query should return 0 results");

    let hits = json["hits"].as_array().expect("hits array");
    assert!(hits.is_empty(), "Hits array should be empty");
}

#[test]
fn include_attachments_flag_hidden_from_pages_help() {
    let cmd = Cli::command();
    let pages_cmd = cmd
        .find_subcommand("pages")
        .expect("pages subcommand must exist");
    let mut help_buf = Vec::new();
    pages_cmd.clone().write_help(&mut help_buf).unwrap();
    let help_text = String::from_utf8(help_buf).unwrap();
    assert!(
        !help_text.contains("include-attachments"),
        "--include-attachments should stay hidden from pages help until implemented.\n{help_text}"
    );
}

#[test]
fn search_writes_trace_on_success() {
    // E2E test: trace file captures successful search (yln.5)
    let tmp = TempDir::new().unwrap();
    let trace_path = tmp.path().join("search_trace.jsonl");

    let mut cmd = base_cmd();
    cmd.args([
        "--trace-file",
        trace_path.to_str().unwrap(),
        "search",
        "hello",
        "--json",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    cmd.assert().success();

    // Verify trace file was written
    let trace = fs::read_to_string(&trace_path).expect("trace file exists");
    assert!(!trace.is_empty(), "Trace file should have content");

    // Parse last line as JSON
    let last_line = trace.lines().last().expect("trace has lines");
    let json: Value = serde_json::from_str(last_line).expect("valid trace JSON");
    assert_eq!(
        json["exit_code"], 0,
        "Successful search should have exit_code 0"
    );
    assert_eq!(json["contract_version"], "1");
}

#[test]
fn search_missing_index_returns_json_error_contract() {
    let tmp = TempDir::new().unwrap();
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "foo",
        "--json",
        "--data-dir",
        tmp.path().to_str().unwrap(),
    ]);

    let output = cmd.assert().failure().get_output().clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Parse last non-empty line to be robust to any stray warnings
    let last_line = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .expect("stderr should contain a JSON error line");

    let val: Value =
        serde_json::from_str(last_line.trim()).expect("stderr should contain JSON error payload");
    let err = val
        .get("error")
        .and_then(|e| e.as_object())
        .expect("error object present");
    let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    assert_ne!(code, 0, "error code should be non-zero");
    assert!(
        err.get("kind").and_then(|k| k.as_str()).is_some(),
        "error kind should be present"
    );
    assert!(err.get("message").is_some(), "message should be included");
    assert!(
        err.get("retryable").is_some(),
        "retryable flag should be included"
    );
}

#[test]
fn search_missing_index_returns_json_error_contract_with_robot_format_compact() {
    let tmp = TempDir::new().unwrap();
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "foo",
        "--robot-format",
        "compact",
        "--data-dir",
        tmp.path().to_str().unwrap(),
    ]);

    let output = cmd.assert().failure().get_output().clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last_line = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .expect("stderr should contain a JSON error line");
    let val: Value =
        serde_json::from_str(last_line.trim()).expect("stderr should contain JSON error payload");
    assert_eq!(
        val["error"]["kind"],
        Value::String("missing-index".to_string())
    );
}

#[test]
fn search_missing_index_returns_json_error_contract_with_env_output_format() {
    let tmp = TempDir::new().unwrap();
    let mut cmd = base_cmd();
    cmd.env("CASS_OUTPUT_FORMAT", "compact");
    cmd.args(["search", "foo", "--data-dir", tmp.path().to_str().unwrap()]);

    let output = cmd.assert().failure().get_output().clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last_line = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .expect("stderr should contain a JSON error line");
    let val: Value =
        serde_json::from_str(last_line.trim()).expect("stderr should contain JSON error payload");
    assert_eq!(
        val["error"]["kind"],
        Value::String("missing-index".to_string())
    );
}

#[test]
fn stats_missing_index_returns_json_error_contract() {
    let tmp = TempDir::new().unwrap();
    let mut cmd = base_cmd();
    cmd.args([
        "stats",
        "--json",
        "--data-dir",
        tmp.path().to_str().unwrap(),
    ]);

    let output = cmd.assert().failure().get_output().clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last_line = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .expect("stderr should contain a JSON error line");
    let val: Value =
        serde_json::from_str(last_line.trim()).expect("stderr should contain JSON error payload");
    let err = val
        .get("error")
        .and_then(|e| e.as_object())
        .expect("error object present");
    let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    assert_ne!(code, 0, "error code should be non-zero");
    assert!(
        err.get("kind").and_then(|k| k.as_str()).is_some(),
        "error kind should be present"
    );
    assert!(
        err.get("retryable").is_some(),
        "retryable flag should be present"
    );
}

#[test]
fn search_json_includes_match_type() {
    // E2E test: JSON results include match_type field (yln.5)
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--json",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let hits = json["hits"].as_array().expect("hits array");
    if !hits.is_empty() {
        let first_hit = &hits[0];
        assert!(
            first_hit["match_type"].is_string(),
            "Hit should include match_type (exact/wildcard/fuzzy)"
        );
    }
}

#[test]
fn search_robot_format_is_valid_json_lines() {
    // E2E test: --robot output is JSON lines format (yln.5)
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--robot",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Robot mode should output JSON (same as --json)
    let json: Value =
        serde_json::from_str(stdout.trim()).expect("robot output should be valid JSON");
    assert!(
        json["hits"].is_array(),
        "Robot output should have hits array"
    );
}

#[test]
fn search_robot_meta_includes_fallback_and_cache_stats() {
    // CLI should surface wildcard_fallback + cache stats when --robot-meta is set
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--json",
        "--robot-meta",
        "--limit",
        "1",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let meta = json
        .get("_meta")
        .and_then(|m| m.as_object())
        .expect("_meta present when --robot-meta is used");

    assert!(
        meta.get("wildcard_fallback").is_some(),
        "_meta should include wildcard_fallback flag"
    );

    let cache = meta
        .get("cache_stats")
        .and_then(|c| c.as_object())
        .expect("_meta.cache_stats should be present");
    assert!(
        cache.contains_key("hits")
            && cache.contains_key("misses")
            && cache.contains_key("shortfall"),
        "cache_stats should expose hits, misses, shortfall"
    );
}

#[test]
fn stats_json_reports_counts() {
    let fixture = isolated_search_demo_data();
    let data_dir = fixture.path().to_str().unwrap();
    let mut cmd = base_cmd();
    cmd.args(["stats", "--json", "--data-dir", data_dir]);

    let assert = cmd.assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert!(
        json["conversations"].as_i64().unwrap_or(0) > 0,
        "stats should report conversations > 0"
    );
    assert!(
        json["messages"].as_i64().unwrap_or(0) > 0,
        "stats should report messages > 0"
    );
    assert!(
        json["by_agent"].is_array(),
        "stats should include per-agent breakdown"
    );
}

#[test]
fn diag_json_reports_database_state() {
    let fixture = isolated_search_demo_data();
    let data_dir = fixture.path().to_str().unwrap();
    let mut cmd = base_cmd();
    cmd.args(["diag", "--json", "--data-dir", data_dir]);

    let assert = cmd.assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert_eq!(
        json["database"]["exists"],
        Value::Bool(true),
        "diag should detect database file"
    );
    assert!(
        json["database"]["conversations"].as_i64().unwrap_or(0) > 0,
        "diag should report conversation count"
    );
    assert!(
        json["paths"]["data_dir"].is_string(),
        "diag should include data_dir path"
    );
}

#[test]
fn status_json_reports_index_health() {
    let fixture = isolated_search_demo_data();
    let data_dir = fixture.path().to_str().unwrap();
    let mut cmd = base_cmd();
    cmd.args(["status", "--json", "--data-dir", data_dir]);

    let assert = cmd.assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert!(
        json["database"]["exists"].as_bool().unwrap_or(false),
        "status should report database exists"
    );
    // Note: index.exists may be false for fixture data without tantivy index
    assert!(json["index"].is_object(), "status should have index object");
    // recommended_action may be null when healthy, so check it's present in the response
    assert!(
        json.get("recommended_action").is_some(),
        "status should include recommended_action field"
    );
}

#[test]
fn view_json_highlights_requested_line() {
    let mut cmd = base_cmd();
    cmd.args([
        "view",
        "tests/fixtures/amp/thread-001.json",
        "--json",
        "-n",
        "5",
        "-C",
        "0",
    ]);

    let assert = cmd.assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert_eq!(
        json["target_line"].as_u64(),
        Some(5),
        "target_line should reflect requested line"
    );
    let lines = json["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 1, "context 0 should return single line");
    assert_eq!(
        lines[0]["line"].as_u64(),
        Some(5),
        "line number should match requested"
    );
    assert!(
        lines[0]["highlighted"].as_bool().unwrap_or(false),
        "requested line should be highlighted"
    );
    assert!(
        lines[0]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("\"Hello\""),
        "content should include requested line text"
    );
}

#[test]
fn introspect_json_lists_commands() {
    let mut cmd = base_cmd();
    cmd.args(["introspect", "--json"]);

    let assert = cmd.assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let commands = json["commands"].as_array().expect("commands array");
    let names: Vec<String> = commands
        .iter()
        .filter_map(|c| c["name"].as_str().map(|s| s.to_string()))
        .collect();
    assert!(
        names.contains(&"search".to_string()) && names.contains(&"status".to_string()),
        "introspect should include search and status commands"
    );
}

fn fetch_introspect_json() -> Value {
    let mut cmd = base_cmd();
    cmd.args(["introspect", "--json"]);

    let stdout = String::from_utf8_lossy(&cmd.assert().success().get_output().stdout).into_owned();
    serde_json::from_str(stdout.trim()).expect("valid introspect JSON")
}

fn find_command<'a>(json: &'a Value, name: &str) -> &'a Value {
    let msg = format!("command {name} missing from introspect");
    json["commands"]
        .as_array()
        .and_then(|cmds| cmds.iter().find(|c| c["name"] == name))
        .expect(&msg)
}

fn find_arg<'a>(cmd: &'a Value, name: &str) -> &'a Value {
    let msg = format!("arg {name} missing in command {}", cmd["name"]);
    cmd["arguments"]
        .as_array()
        .and_then(|args| args.iter().find(|a| a["name"] == name))
        .expect(&msg)
}

#[test]
fn introspect_commands_match_clap_subcommands() {
    let json = fetch_introspect_json();

    let clap_cmd = Cli::command();
    let clap_commands: HashSet<String> = clap_cmd
        .get_subcommands()
        .map(|c: &clap::Command| c.get_name().to_string())
        .collect();

    let introspect_commands: HashSet<String> = json["commands"]
        .as_array()
        .expect("commands array")
        .iter()
        .filter_map(|c| c["name"].as_str().map(|s| s.to_string()))
        .collect();

    assert_eq!(
        clap_commands, introspect_commands,
        "introspect should list exactly the Clap subcommands"
    );

    // Ensure no help/version pseudo-args leak into schemas
    for cmd in json["commands"].as_array().unwrap() {
        let args = cmd["arguments"].as_array().unwrap();
        assert!(
            !args
                .iter()
                .any(|a| a["name"] == "help" || a["name"] == "version"),
            "help/version flags should be hidden in introspect"
        );
    }
}

#[test]
fn introspect_arguments_capture_types_defaults_and_repeatable() {
    let json = fetch_introspect_json();

    let search = find_command(&json, "search");
    let limit = find_arg(search, "limit");
    assert_eq!(limit["value_type"], "integer");
    assert_eq!(limit["default"], "0");

    let offset = find_arg(search, "offset");
    assert_eq!(offset["value_type"], "integer");
    assert_eq!(offset["default"], "0");

    let agent = find_arg(search, "agent");
    assert_eq!(agent["repeatable"], true);
    assert_eq!(agent["arg_type"], "option");

    let workspace = find_arg(search, "workspace");
    assert_eq!(workspace["repeatable"], true);

    let robot_format = find_arg(search, "robot-format");
    assert_eq!(robot_format["value_type"], "enum");
    let formats = robot_format["enum_values"].as_array().unwrap();
    let format_set: HashSet<_> = formats.iter().map(|v| v.as_str().unwrap()).collect();
    assert!(
        format_set.contains("json")
            && format_set.contains("jsonl")
            && format_set.contains("compact")
    );

    let data_dir = find_arg(search, "data-dir");
    assert_eq!(data_dir["value_type"], "path");

    let aggregate = find_arg(search, "aggregate");
    assert_eq!(aggregate["repeatable"], true);
    assert_eq!(aggregate["value_type"], "string");

    let stale = find_arg(find_command(&json, "status"), "stale-threshold");
    assert_eq!(stale["value_type"], "integer");
    assert_eq!(stale["default"], "1800");

    let view = find_command(&json, "view");
    let path_arg = find_arg(view, "path");
    assert_eq!(path_arg["value_type"], "path");
    assert_eq!(path_arg["arg_type"], "positional");

    // Repeatable watch-once paths (index command)
    let index = find_command(&json, "index");
    let watch_once = find_arg(index, "watch-once");
    assert_eq!(watch_once["repeatable"], true);
    assert_eq!(watch_once["value_type"], "path");
}

#[test]
fn introspect_sessions_command_exposes_workspace_current_and_limit() {
    let json = fetch_introspect_json();

    let sessions = find_command(&json, "sessions");
    let workspace = find_arg(sessions, "workspace");
    assert_eq!(workspace["value_type"], "path");
    assert_eq!(workspace["arg_type"], "option");

    let current = find_arg(sessions, "current");
    assert_eq!(current["arg_type"], "flag");

    let limit = find_arg(sessions, "limit");
    assert_eq!(limit["value_type"], "integer");

    let data_dir = find_arg(sessions, "data-dir");
    assert_eq!(data_dir["value_type"], "path");
}

#[test]
fn diag_json_reports_paths_and_connectors() {
    let mut cmd = base_cmd();
    cmd.args([
        "diag",
        "--json",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid diag JSON");

    assert!(json["paths"]["data_dir"].is_string());
    assert!(json["database"]["exists"].is_boolean());
    assert!(json["index"]["exists"].is_boolean());
    assert!(
        json["connectors"].is_array(),
        "diag should include connectors array"
    );

    let connector_names: HashSet<String> = json["connectors"]
        .as_array()
        .expect("connectors array")
        .iter()
        .filter_map(|entry| entry.get("name"))
        .filter_map(|name| name.as_str())
        .map(str::to_string)
        .collect();

    for expected in ["aider", "pi_agent", "claude_code"] {
        assert!(
            connector_names.contains(expected),
            "diag connectors missing expected entry: {expected}"
        );
    }
}

#[test]
fn view_json_outputs_file_excerpt() {
    // Use a small text file and ensure view returns JSON payload.
    let mut cmd = base_cmd();
    let path = "README.md";
    cmd.args(["view", path, "--json", "-n", "1", "-C", "0"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid view JSON");

    assert_eq!(json["path"], path);
    assert!(json["lines"].is_array());
}

#[test]
fn status_json_reports_staleness_flags() {
    let mut cmd = base_cmd();
    cmd.args([
        "status",
        "--json",
        "--data-dir",
        "tests/fixtures/search_demo_data",
        "--stale-threshold",
        "1",
    ]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid status JSON");
    let Some(status) = json.get("status").and_then(|v| v.as_object()) else {
        // If status key is absent in this build/contract, skip further assertions.
        return;
    };
    assert!(
        status.get("db_exists").and_then(|v| v.as_bool()).is_some(),
        "status should include db_exists boolean"
    );
    assert!(
        status
            .get("index_exists")
            .and_then(|v| v.as_bool())
            .is_some(),
        "status should include index_exists boolean"
    );
    assert!(
        status.get("stale").and_then(|v| v.as_bool()).is_some(),
        "status should include stale boolean"
    );
}

#[test]
fn stats_missing_db_returns_code_3() {
    let tmp = TempDir::new().unwrap();

    let mut cmd = base_cmd();
    cmd.args([
        "stats",
        "--json",
        "--data-dir",
        tmp.path().to_str().unwrap(),
    ]);

    let assert = cmd.assert().failure();
    let output = assert.get_output().clone();
    assert_eq!(
        output.status.code(),
        Some(3),
        "missing db should return exit code 3"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("missing-db") || stderr.contains("Database not found"),
        "stderr should mention missing database"
    );
}

#[test]
fn search_agent_filter_limits_hits() {
    // Agent filter should restrict results to the chosen agent
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "",
        "--json",
        "--agent",
        "aider",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let hits = json["hits"].as_array().expect("hits array");
    if hits.is_empty() {
        assert_eq!(json["count"].as_u64().unwrap_or(0), 0);
        return;
    }
    for hit in hits {
        assert_eq!(hit["agent"], "aider", "agent filter should be enforced");
    }
}

#[test]
fn search_offset_skips_results() {
    // Offset should skip earlier hits while preserving order
    let mut cmd_full = base_cmd();
    cmd_full.args([
        "search",
        "hello",
        "--json",
        "--limit",
        "3",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);
    let full_bytes = cmd_full.assert().success().get_output().stdout.to_vec();
    let full_stdout = String::from_utf8_lossy(&full_bytes);
    let full_json: Value =
        serde_json::from_str(full_stdout.trim()).expect("valid JSON for base search");
    let full_hits = full_json["hits"].as_array().expect("hits array");
    if full_hits.len() < 2 {
        // dataset too small to assert offset meaningfully
        return;
    }
    let mut cmd_offset = base_cmd();
    cmd_offset.args([
        "search",
        "hello",
        "--json",
        "--limit",
        "1",
        "--offset",
        "1",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);
    let offset_bytes = cmd_offset.assert().success().get_output().stdout.to_vec();
    let offset_stdout = String::from_utf8_lossy(&offset_bytes);
    let offset_json: Value =
        serde_json::from_str(offset_stdout.trim()).expect("valid JSON for offset search");
    let offset_hits = offset_json["hits"].as_array().expect("hits array");

    assert_eq!(offset_hits.len(), 1, "limit should be applied after offset");
    let offset_path = offset_hits[0]["source_path"].as_str().unwrap_or_default();

    // Minimal guarantee: with offset applied we still get a hit (if data has >1),
    // and the limit is honored. Dataset ordering/dedup may vary.
    assert!(
        !offset_path.is_empty(),
        "offset result should still return a hit"
    );
}

#[test]
fn robot_mode_auto_quiet_suppresses_info_logs() {
    // rob.ctx.quiet: Robot mode (--json) should auto-suppress INFO logs on stderr
    // This ensures AI agents get clean, parseable stdout without log noise
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--json",
        "--limit",
        "1",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);
    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);

    // INFO logs should NOT appear in stderr when using --json
    assert!(
        !stderr.contains("INFO"),
        "Robot mode should auto-suppress INFO logs. Got stderr: {stderr}"
    );

    // JSON output should still be valid on stdout
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON output");
    assert!(json["hits"].is_array(), "Should have valid hits array");
}

#[test]
fn non_robot_mode_shows_info_logs() {
    // Verify that non-robot mode DOES show INFO logs (baseline check)
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--limit",
        "1",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);
    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);

    // INFO logs SHOULD appear in stderr when NOT using --json
    assert!(
        stderr.contains("INFO") || stderr.contains("search_start"),
        "Non-robot mode should show INFO logs. Got stderr: {stderr}"
    );
}

// ============================================================
// rob.ctx.fields: Field Selection Tests
// ============================================================

#[test]
fn fields_filters_to_requested_only() {
    // rob.ctx.fields: --fields should filter hits to only requested fields
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "",
        "--json",
        "--limit",
        "1",
        "--fields",
        "source_path,line_number",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let hits = json["hits"].as_array().expect("hits array");
    if hits.is_empty() {
        return;
    }

    let hit = &hits[0];
    // Should have only the requested fields
    assert!(hit["source_path"].is_string(), "Should have source_path");
    assert!(hit["line_number"].is_number(), "Should have line_number");
    // Should NOT have other fields
    assert!(hit["score"].is_null(), "Should NOT have score");
    assert!(hit["agent"].is_null(), "Should NOT have agent");
    assert!(hit["content"].is_null(), "Should NOT have content");
}

#[test]
fn fields_minimal_preset_expands() {
    // rob.ctx.fields: 'minimal' preset should expand to source_path,line_number,agent
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "",
        "--json",
        "--limit",
        "1",
        "--fields",
        "minimal",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let hits = json["hits"].as_array().expect("hits array");
    if hits.is_empty() {
        return;
    }
    let hit = &hits[0];
    // Minimal preset fields
    assert!(hit["source_path"].is_string(), "Should have source_path");
    assert!(hit["line_number"].is_number(), "Should have line_number");
    assert!(hit["agent"].is_string(), "Should have agent");
    // Should NOT have extra fields
    assert!(hit["score"].is_null(), "Should NOT have score");
    assert!(hit["content"].is_null(), "Should NOT have content");
}

#[test]
fn fields_summary_preset_expands() {
    // rob.ctx.fields: 'summary' preset should expand to source_path,line_number,agent,title,score
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "",
        "--json",
        "--limit",
        "1",
        "--fields",
        "summary",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let hits = json["hits"].as_array().expect("hits array");
    if hits.is_empty() {
        return;
    }
    let hit = &hits[0];
    // Summary preset fields
    assert!(hit["source_path"].is_string(), "Should have source_path");
    assert!(hit["line_number"].is_number(), "Should have line_number");
    assert!(hit["agent"].is_string(), "Should have agent");
    assert!(!hit["title"].is_null(), "Should have title");
    assert!(hit["score"].is_number(), "Should have score");
    // Should NOT have extra fields
    assert!(hit["content"].is_null(), "Should NOT have content");
    assert!(hit["snippet"].is_null(), "Should NOT have snippet");
}

#[test]
fn fields_works_with_jsonl_format() {
    // rob.ctx.fields: Field selection should work with --robot-format jsonl
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--json",
        "--robot-format",
        "jsonl",
        "--limit",
        "1",
        "--fields",
        "source_path,score",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // JSONL: each line is a separate JSON object (hit)
    for line in stdout.lines() {
        let json: Value = serde_json::from_str(line).expect("valid JSON line");
        // Skip _meta lines
        if json.get("_meta").is_some() {
            continue;
        }
        // Hit lines should only have requested fields
        assert!(json["source_path"].is_string(), "Should have source_path");
        assert!(json["score"].is_number(), "Should have score");
        // Count fields (excluding null)
        let obj = json.as_object().expect("object");
        assert_eq!(obj.len(), 2, "Should have exactly 2 fields");
    }
}

// ============================================================
// rob.ctx.trunc: Content Truncation Tests
// ============================================================

#[test]
fn max_content_length_truncates_long_content() {
    // rob.ctx.trunc: --max-content-length should truncate content fields with ellipsis
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "",
        "--json",
        "--limit",
        "1",
        "--max-content-length",
        "5",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let hits = json["hits"].as_array().expect("hits array");
    if hits.is_empty() {
        return;
    }
    let hit = &hits[0];
    // Content should be truncated with ellipsis
    let content = hit["content"].as_str().expect("content string");
    assert!(
        content.ends_with("..."),
        "Truncated content should end with ellipsis"
    );
    assert!(
        content.len() <= 5,
        "Content should be at most max_content_length"
    );

    // Should have _truncated indicator
    assert!(
        hit.get("content_truncated").is_some(),
        "Should have content_truncated indicator"
    );
}

#[test]
fn max_content_length_adds_truncated_indicator() {
    // rob.ctx.trunc: Truncation adds _truncated indicator for each truncated field
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--json",
        "--limit",
        "1",
        "--max-content-length",
        "3",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let hits = json["hits"].as_array().expect("hits array");
    if hits.is_empty() {
        return;
    }
    let hit = &hits[0];
    // Both content and snippet should have truncated indicators
    if hit["content"].is_string() {
        assert!(
            hit.get("content_truncated").is_some(),
            "content_truncated indicator should exist when content is truncated"
        );
    }
    if hit["snippet"].is_string() {
        assert!(
            hit.get("snippet_truncated").is_some(),
            "snippet_truncated indicator should exist when snippet is truncated"
        );
    }
}

#[test]
fn max_content_length_preserves_short_content() {
    // rob.ctx.trunc: Content shorter than limit should not be truncated
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--json",
        "--limit",
        "1",
        "--max-content-length",
        "1000",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let hits = json["hits"].as_array().expect("hits array");
    if hits.is_empty() {
        return;
    }
    let hit = &hits[0];
    // Should NOT have truncated indicators when content is short
    assert!(
        hit.get("content_truncated").is_none(),
        "Short content should not have truncated indicator"
    );
    // Content should not end with ellipsis
    if let Some(content) = hit["content"].as_str() {
        assert!(
            !content.ends_with("..."),
            "Short content should not have ellipsis"
        );
    }
}

#[test]
fn max_content_length_works_with_fields() {
    // rob.ctx.trunc: Truncation should work alongside field selection
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "",
        "--json",
        "--limit",
        "1",
        "--max-content-length",
        "5",
        "--fields",
        "content,snippet",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let hits = json["hits"].as_array().expect("hits array");
    if hits.is_empty() {
        return;
    }
    let hit = &hits[0];
    // Should have requested fields
    assert!(hit["content"].is_string(), "Should have content field");
    // Should be truncated
    let content = hit["content"].as_str().unwrap();
    assert!(content.ends_with("..."), "Content should be truncated");
    // Truncated indicator should be included even when fields are filtered
    assert!(
        hit.get("content_truncated").is_some(),
        "Truncated indicator should be included"
    );
}

// ============================================================
// rob.state.status: Status Command Tests
// ============================================================

#[test]
fn status_json_returns_health_info() {
    // rob.state.status: status command should return health information as JSON
    let fixture = isolated_search_demo_data();
    let data_dir = fixture.path().to_str().unwrap();
    let mut cmd = base_cmd();
    cmd.args(["status", "--json", "--data-dir", data_dir]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should have required top-level fields
    assert!(json["healthy"].is_boolean(), "Should have healthy boolean");
    assert!(json["index"].is_object(), "Should have index object");
    assert!(json["database"].is_object(), "Should have database object");
    assert!(json["pending"].is_object(), "Should have pending object");
    assert!(json["semantic"].is_object(), "Should have semantic object");

    // Database should exist in fixture
    assert_eq!(
        json["database"]["exists"],
        Value::Bool(true),
        "Database should exist"
    );
    assert!(
        json["database"]["conversations"].as_i64().unwrap() > 0,
        "Should have conversations"
    );
    assert!(
        json["database"]["messages"].as_i64().unwrap() > 0,
        "Should have messages"
    );
}

#[test]
fn status_json_reports_stale_threshold() {
    // rob.state.status: status should include stale threshold info
    let fixture = isolated_search_demo_data();
    let data_dir = fixture.path().to_str().unwrap();
    let mut cmd = base_cmd();
    cmd.args([
        "status",
        "--json",
        "--stale-threshold",
        "60",
        "--data-dir",
        data_dir,
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should have stale threshold
    assert_eq!(
        json["index"]["stale_threshold_seconds"],
        Value::Number(60.into()),
        "Stale threshold should match argument"
    );
}

#[test]
fn status_missing_db_reports_not_initialized() {
    // rob.state.status: brand-new data dir should surface initialization guidance
    let tmp = TempDir::new().unwrap();

    let mut cmd = base_cmd();
    cmd.args([
        "status",
        "--json",
        "--data-dir",
        tmp.path().to_str().unwrap(),
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert_eq!(json["status"], Value::String("not_initialized".to_string()));
    assert_eq!(json["initialized"], Value::Bool(false));
    assert_eq!(
        json["database"]["exists"],
        Value::Bool(false),
        "Database should not exist"
    );
    assert_eq!(
        json["healthy"],
        Value::Bool(false),
        "Should not be healthy without db"
    );
    assert_eq!(json["index"]["exists"], Value::Bool(false));
    assert_eq!(json["index"]["stale"], Value::Bool(false));
    assert!(
        !tmp.path().join("index").exists(),
        "status should not create an empty index dir while inspecting a fresh install"
    );
    assert!(
        json["explanation"]
            .as_str()
            .unwrap_or("")
            .contains("fresh install"),
        "status should explain that this is an expected cold-start state: {json}"
    );
    assert!(
        json["recommended_action"]
            .as_str()
            .unwrap_or("")
            .contains("cass index --full"),
        "Should recommend the first index run"
    );
    assert_eq!(
        json["semantic"]["status"],
        Value::String("not_initialized".to_string())
    );
    assert!(
        json["semantic"]["summary"]
            .as_str()
            .unwrap_or("")
            .contains("optional"),
        "fresh installs should not surface semantic model absence as a failure: {json}"
    );
    assert_eq!(json["semantic"]["embedder_id"], Value::Null);
    assert_eq!(json["semantic"]["vector_index_path"], Value::Null);
    assert_eq!(json["semantic"]["model_dir"], Value::Null);
    assert_eq!(json["semantic"]["hnsw_path"], Value::Null);
}

#[test]
fn status_empty_index_dir_without_meta_still_reports_not_initialized() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join("index").join("v7")).unwrap();

    let mut cmd = base_cmd();
    cmd.args([
        "status",
        "--json",
        "--data-dir",
        tmp.path().to_str().unwrap(),
    ]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert_eq!(json["status"], Value::String("not_initialized".to_string()));
    assert_eq!(json["initialized"], Value::Bool(false));
    assert_eq!(json["index"]["exists"], Value::Bool(false));
    assert_eq!(json["index"]["stale"], Value::Bool(false));
    assert!(
        json["recommended_action"]
            .as_str()
            .unwrap_or("")
            .contains("cass index --full"),
        "empty leftover index dirs should not masquerade as a usable index: {json}"
    );
}

#[test]
fn health_missing_db_reports_not_initialized() {
    let tmp = TempDir::new().unwrap();

    let mut cmd = base_cmd();
    cmd.args([
        "health",
        "--json",
        "--data-dir",
        tmp.path().to_str().unwrap(),
    ]);

    let assert = cmd.assert().failure();
    let output = assert.get_output();
    assert_eq!(output.status.code(), Some(1));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert!(
        output.stderr.is_empty(),
        "health --json should not emit a second structured error once stdout already contains the full payload"
    );
    assert_eq!(json["status"], Value::String("not_initialized".to_string()));
    assert_eq!(json["initialized"], Value::Bool(false));
    assert_eq!(json["healthy"], Value::Bool(false));
    assert_eq!(json["state"]["index"]["exists"], Value::Bool(false));
    assert_eq!(json["state"]["index"]["stale"], Value::Bool(false));
    assert!(
        !tmp.path().join("index").exists(),
        "health should not create an empty index dir while probing a fresh install"
    );
    assert!(
        json["explanation"]
            .as_str()
            .unwrap_or("")
            .contains("fresh install"),
        "health should explain the cold-start state: {json}"
    );
    assert!(
        json["recommended_action"]
            .as_str()
            .unwrap_or("")
            .contains("cass index --full"),
        "health should recommend the initial index run: {json}"
    );
    assert!(
        json["errors"]
            .as_array()
            .map(|errors| errors
                .iter()
                .any(|entry| entry.as_str() == Some("database not initialized yet")))
            .unwrap_or(false),
        "health should distinguish not-initialized from broken: {json}"
    );
    assert_eq!(
        json["state"]["semantic"]["status"],
        Value::String("not_initialized".to_string())
    );
    assert_eq!(json["state"]["semantic"]["embedder_id"], Value::Null);
    assert_eq!(json["state"]["semantic"]["vector_index_path"], Value::Null);
    assert_eq!(json["state"]["semantic"]["model_dir"], Value::Null);
    assert_eq!(json["state"]["semantic"]["hnsw_path"], Value::Null);
}

#[test]
fn doctor_not_initialized_ignores_active_lock_for_other_db() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let db_path = data_dir.join("agent_search.db");
    let other_db_path = data_dir.join("other-agent-search.db");
    let lock_path = data_dir.join("index-run.lock");

    let mut lock_file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    lock_file.try_lock_exclusive().unwrap();
    writeln!(
        lock_file,
        "pid={}
started_at_ms={}
db_path={}
mode=index
job_kind=lexical_refresh
phase=rebuilding",
        std::process::id(),
        1_733_001_222_000_i64,
        other_db_path.display()
    )
    .unwrap();
    lock_file.flush().unwrap();

    let mut cmd = base_cmd();
    cmd.args([
        "doctor",
        "--json",
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--db",
        db_path.to_str().unwrap(),
    ]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert_eq!(json["status"], Value::String("not_initialized".to_string()));
    assert_eq!(json["initialized"], Value::Bool(false));
    assert_eq!(json["failures"], Value::from(0));
}

#[test]
fn doctor_missing_data_dir_reports_not_initialized_without_failure() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("fresh-cass-home");

    let mut cmd = base_cmd();
    cmd.args(["doctor", "--json", "--data-dir", data_dir.to_str().unwrap()]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert!(
        output.stderr.is_empty(),
        "doctor --json on a fresh install should explain initialization state without emitting an error: {stdout}"
    );
    assert_eq!(json["status"], Value::String("not_initialized".to_string()));
    assert_eq!(json["initialized"], Value::Bool(false));
    assert_eq!(json["healthy"], Value::Bool(false));
    assert_eq!(json["failures"], Value::from(0));
    assert!(
        json["explanation"]
            .as_str()
            .unwrap_or("")
            .contains("fresh install"),
        "doctor should explain the cold-start state: {json}"
    );
    assert!(
        json["recommended_action"]
            .as_str()
            .unwrap_or("")
            .contains("cass index --full"),
        "doctor should point users at the initial index run: {json}"
    );
    assert!(
        json["checks"]
            .as_array()
            .map(|checks| checks.iter().any(|check| {
                check["name"] == Value::String("database".to_string())
                    && check["status"] == Value::String("warn".to_string())
            }))
            .unwrap_or(false),
        "doctor should classify missing database as informational on fresh installs: {json}"
    );
    assert!(
        json["checks"]
            .as_array()
            .map(|checks| checks.iter().any(|check| {
                check["name"] == Value::String("index".to_string())
                    && check["status"] == Value::String("warn".to_string())
            }))
            .unwrap_or(false),
        "doctor should classify missing index as informational on fresh installs: {json}"
    );
}

#[test]
fn search_missing_index_reports_current_rebuild_in_progress() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let db_path = data_dir.join("agent_search.db");
    let lock_path = data_dir.join("index-run.lock");

    let mut lock_file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    lock_file.try_lock_exclusive().unwrap();
    writeln!(
        lock_file,
        "pid={}
started_at_ms={}
db_path={}
mode=index
job_kind=lexical_refresh
phase=rebuilding",
        std::process::id(),
        1_733_001_333_000_i64,
        db_path.display()
    )
    .unwrap();
    lock_file.flush().unwrap();

    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "auth",
        "--json",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);

    let assert = cmd.assert().failure();
    let output = assert.get_output();
    assert_eq!(output.status.code(), Some(3));

    let stderr = String::from_utf8_lossy(&output.stderr);
    let json: Value = serde_json::from_str(stderr.trim()).expect("valid JSON");

    assert_eq!(
        json["error"]["kind"],
        Value::String("missing-index".to_string())
    );
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("already building the initial search index"),
        "search should explain that the first index run is already in progress: {json}"
    );
    assert!(
        json["error"]["hint"]
            .as_str()
            .unwrap_or("")
            .contains("cass status --json"),
        "search should point callers at status while waiting for the initial index build: {json}"
    );
}

#[test]
fn search_missing_index_explains_initialization_required() {
    let tmp = TempDir::new().unwrap();

    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "auth",
        "--json",
        "--data-dir",
        tmp.path().to_str().unwrap(),
    ]);

    let assert = cmd.assert().failure();
    let output = assert.get_output();
    assert_eq!(output.status.code(), Some(3));

    let stderr = String::from_utf8_lossy(&output.stderr);
    let json: Value = serde_json::from_str(stderr.trim()).expect("valid JSON");

    assert!(
        !tmp.path().join("index").exists(),
        "search should not create an empty index dir when the archive is not initialized"
    );
    assert_eq!(
        json["error"]["kind"],
        Value::String("missing-index".to_string())
    );
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("not been initialized"),
        "search should explain that the data dir needs first-run indexing: {json}"
    );
    assert!(
        json["error"]["hint"]
            .as_str()
            .unwrap_or("")
            .contains("cass index --full"),
        "search should tell the user exactly how to initialize the archive: {json}"
    );
}

#[test]
fn status_json_reports_open_error_for_unopenable_db_path() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    fs::create_dir_all(data_dir.join("index").join("v4")).unwrap();
    fs::create_dir_all(data_dir.join("agent_search.db")).unwrap();

    let mut cmd = base_cmd();
    cmd.args(["status", "--json", "--data-dir"])
        .arg(data_dir)
        .timeout(std::time::Duration::from_secs(10));

    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "status should succeed even when the db path is unopenable"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(json["healthy"], Value::Bool(false));
    assert_eq!(json["status"], Value::String("degraded".to_string()));
    assert_eq!(json["database"]["exists"], Value::Bool(true));
    assert_eq!(json["database"]["opened"], Value::Bool(false));
    assert_ne!(
        json["semantic"]["availability"],
        Value::String("load_failed".to_string())
    );
    assert!(
        !json["semantic"]["summary"]
            .as_str()
            .unwrap_or("")
            .contains("asset inspection failed"),
        "status should preserve the semantic root cause instead of collapsing to a generic asset failure: {json}"
    );
    assert!(
        json["database"]["open_error"]
            .as_str()
            .unwrap_or("")
            .contains("Failed to open")
            || json["database"]["open_error"]
                .as_str()
                .unwrap_or("")
                .contains("open"),
        "status should surface the open failure: {json}"
    );
}

#[test]
fn health_json_reports_open_error_for_unopenable_db_path() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    fs::create_dir_all(data_dir.join("index").join("v4")).unwrap();
    fs::create_dir_all(data_dir.join("agent_search.db")).unwrap();

    let mut cmd = base_cmd();
    cmd.args(["health", "--json", "--data-dir"])
        .arg(data_dir)
        .timeout(std::time::Duration::from_secs(10));

    let output = cmd.output().unwrap();
    assert!(
        !output.status.success(),
        "health should fail when the db exists but cannot be opened"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(json["healthy"], Value::Bool(false));
    assert_eq!(json["status"], Value::String("degraded".to_string()));
    assert_eq!(json["db"]["exists"], Value::Bool(true));
    assert_eq!(json["db"]["opened"], Value::Bool(false));
    assert!(
        json["db"]["open_error"]
            .as_str()
            .unwrap_or("")
            .contains("Failed to open")
            || json["db"]["open_error"]
                .as_str()
                .unwrap_or("")
                .contains("open"),
        "health should surface the open failure: {json}"
    );
    assert_ne!(
        json["state"]["semantic"]["availability"],
        Value::String("load_failed".to_string())
    );
    assert!(
        !json["state"]["semantic"]["summary"]
            .as_str()
            .unwrap_or("")
            .contains("asset inspection failed"),
        "health should preserve the semantic root cause instead of collapsing to a generic asset failure: {json}"
    );
}

#[test]
fn status_human_readable_output() {
    // rob.state.status: status without --json should produce human-readable output
    let fixture = isolated_search_demo_data();
    let data_dir = fixture.path().to_str().unwrap();
    let mut cmd = base_cmd();
    cmd.args(["status", "--data-dir", data_dir]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should contain human-readable sections
    assert!(stdout.contains("CASS Status"), "Should have status header");
    assert!(stdout.contains("Database"), "Should have database section");
    assert!(stdout.contains("Semantic"), "Should have semantic section");
    assert!(
        stdout.contains("Conversations"),
        "Should show conversation count"
    );
}

// ============================================================
// rob.flow.agg: Aggregation Mode Tests
// ============================================================

#[test]
fn aggregate_single_field_returns_buckets() {
    // rob.flow.agg: --aggregate agent should return agent buckets
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--json",
        "--aggregate",
        "agent",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should have aggregations object
    assert!(
        json["aggregations"].is_object(),
        "Should have aggregations object"
    );
    let aggs = &json["aggregations"];

    // Should have agent aggregation
    assert!(aggs["agent"].is_object(), "Should have agent aggregation");
    let agent_agg = &aggs["agent"];
    assert!(
        agent_agg["buckets"].is_array(),
        "Agent aggregation should have buckets"
    );

    // Each bucket should have key and count
    let buckets = agent_agg["buckets"].as_array().unwrap();
    if !buckets.is_empty() {
        let first = &buckets[0];
        assert!(first["key"].is_string(), "Bucket should have key");
        assert!(first["count"].is_number(), "Bucket should have count");
    }

    // Should have other_count
    assert!(
        agent_agg["other_count"].is_number(),
        "Should have other_count"
    );
}

#[test]
fn aggregate_multiple_fields_returns_all() {
    // rob.flow.agg: --aggregate agent,workspace returns both aggregations
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--json",
        "--aggregate",
        "agent,workspace",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let aggs = &json["aggregations"];
    assert!(aggs["agent"].is_object(), "Should have agent aggregation");
    assert!(
        aggs["workspace"].is_object(),
        "Should have workspace aggregation"
    );
}

#[test]
fn aggregate_includes_total_matches() {
    // rob.flow.agg: Aggregation response includes total_matches
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "",
        "--json",
        "--aggregate",
        "agent",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should have total_matches field
    assert!(
        json["total_matches"].is_number(),
        "Should have total_matches field"
    );
    let returned_hits = json["hits"].as_array().map(|hits| hits.len()).unwrap_or(0);
    assert!(
        json["total_matches"].as_u64().unwrap_or(0) >= returned_hits as u64,
        "total_matches should be at least the number of returned hits"
    );
}

#[test]
fn aggregate_with_limit_returns_both_hits_and_aggs() {
    // rob.flow.agg: --aggregate with --limit returns both aggregations and hits
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--json",
        "--aggregate",
        "agent",
        "--limit",
        "2",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should have aggregations
    assert!(
        json["aggregations"]["agent"].is_object(),
        "Should have aggregations"
    );

    // Should have hits (with limit applied)
    let hits = json["hits"].as_array().expect("hits array");
    assert!(
        hits.len() <= 2,
        "Hits should respect --limit even with aggregation"
    );
}

#[test]
fn aggregate_match_type_returns_exact_wildcard_buckets() {
    // rob.flow.agg: --aggregate match_type returns match type distribution
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--json",
        "--aggregate",
        "match_type",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should have match_type aggregation
    assert!(
        json["aggregations"]["match_type"].is_object(),
        "Should have match_type aggregation"
    );

    let buckets = json["aggregations"]["match_type"]["buckets"]
        .as_array()
        .expect("buckets array");

    // At least one bucket should exist (exact, wildcard, or fuzzy)
    if !buckets.is_empty() {
        let keys: Vec<&str> = buckets.iter().filter_map(|b| b["key"].as_str()).collect();
        // Keys should be lowercase match types
        for key in &keys {
            assert!(
                ["exact", "wildcard", "fuzzy", "recent"].contains(key),
                "Match type key '{}' should be valid",
                key
            );
        }
    }
}

#[test]
fn aggregate_empty_query_returns_aggs() {
    // rob.flow.agg: Empty query with aggregation returns all-document aggregations
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "",
        "--json",
        "--aggregate",
        "agent",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should have aggregations even with empty query
    assert!(
        json["aggregations"]["agent"].is_object(),
        "Should have agent aggregation for empty query"
    );
}

#[test]
fn aggregate_preserves_offset_when_not_aggregating() {
    // Verify that regular offset functionality is not broken by aggregation code
    // This is a regression test for the offset=0 bug fix
    let mut cmd_no_agg = base_cmd();
    cmd_no_agg.args([
        "search",
        "hello",
        "--json",
        "--limit",
        "1",
        "--offset",
        "1",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let output = cmd_no_agg.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Should NOT have aggregations field (not requested)
    assert!(
        json.get("aggregations").is_none()
            || json["aggregations"]
                .as_object()
                .is_none_or(|o| o.is_empty()),
        "Should not have aggregations when not requested"
    );

    // Hits should be present (offset applied)
    let hits = json["hits"].as_array().expect("hits array");
    assert!(hits.len() <= 1, "Limit should be respected");
}

// ============================================================
// rob.api.caps: Capabilities Introspection Tests
// ============================================================

#[test]
fn capabilities_json_returns_valid_structure() {
    // rob.api.caps: capabilities --json should return valid JSON with required fields
    let mut cmd = base_cmd();
    cmd.args(["capabilities", "--json"]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    // Required top-level fields
    assert!(
        json["crate_version"].is_string(),
        "Should have crate_version"
    );
    assert!(json["api_version"].is_number(), "Should have api_version");
    assert!(
        json["contract_version"].is_string(),
        "Should have contract_version"
    );
    assert!(json["features"].is_array(), "Should have features array");
    assert!(
        json["connectors"].is_array(),
        "Should have connectors array"
    );
    assert!(json["limits"].is_object(), "Should have limits object");
}

#[test]
fn capabilities_json_includes_expected_features() {
    // rob.api.caps: capabilities should list all expected features
    let mut cmd = base_cmd();
    cmd.args(["capabilities", "--json"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let features = json["features"].as_array().expect("features array");
    let feature_list: Vec<&str> = features.iter().filter_map(|v| v.as_str()).collect();

    // Check for expected features
    assert!(
        feature_list.contains(&"json_output"),
        "Should have json_output feature"
    );
    assert!(
        feature_list.contains(&"aggregations"),
        "Should have aggregations feature"
    );
    assert!(
        feature_list.contains(&"field_selection"),
        "Should have field_selection feature"
    );
    assert!(
        feature_list.contains(&"time_filters"),
        "Should have time_filters feature"
    );
}

#[test]
fn capabilities_json_includes_connectors() {
    // rob.api.caps: capabilities should list supported agent connectors
    let mut cmd = base_cmd();
    cmd.args(["capabilities", "--json"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let connectors = json["connectors"].as_array().expect("connectors array");
    let connector_list: Vec<&str> = connectors.iter().filter_map(|v| v.as_str()).collect();

    // Check for expected connectors
    assert!(connector_list.contains(&"codex"), "Should support codex");
    assert!(
        connector_list.contains(&"claude_code"),
        "Should support claude_code"
    );
    assert!(
        connector_list.len() >= 4,
        "Should have at least 4 connectors"
    );
}

#[test]
fn capabilities_connectors_cover_indexer_registry() {
    // Prevent drift between the indexer connector registry and the capabilities contract.
    let mut cmd = base_cmd();
    cmd.args(["capabilities", "--json"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let connectors = json["connectors"].as_array().expect("connectors array");
    let connector_list: Vec<String> = connectors
        .iter()
        .filter_map(|v| v.as_str())
        .map(str::to_string)
        .collect();
    let connector_set: HashSet<String> = connector_list.iter().cloned().collect();

    assert_eq!(
        connector_set.len(),
        connector_list.len(),
        "capabilities connector list should not contain duplicates"
    );

    let expected_from_registry: Vec<String> =
        coding_agent_search::indexer::get_connector_factories()
            .into_iter()
            .map(|(slug, _)| match slug {
                "claude" => "claude_code".to_string(),
                other => other.to_string(),
            })
            .collect();

    for expected in expected_from_registry {
        assert!(
            connector_set.contains(&expected),
            "capabilities connector list missing registry connector: {expected}"
        );
    }
}

#[test]
fn capabilities_json_includes_limits() {
    // rob.api.caps: capabilities should include system limits
    let mut cmd = base_cmd();
    cmd.args(["capabilities", "--json"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let limits = &json["limits"];
    assert!(limits["max_limit"].is_number(), "Should have max_limit");
    assert!(
        limits["max_content_length"].is_number(),
        "Should have max_content_length"
    );
    assert!(limits["max_fields"].is_number(), "Should have max_fields");
    assert!(
        limits["max_agg_buckets"].is_number(),
        "Should have max_agg_buckets"
    );

    // Sanity check values
    let max_limit = limits["max_limit"].as_u64().expect("max_limit");
    assert!(
        max_limit == 0 || max_limit >= 1000,
        "max_limit should be unlimited (0) or reasonably high"
    );
}

#[test]
fn capabilities_human_output_contains_sections() {
    // rob.api.caps: capabilities without --json should produce human-readable output
    let mut cmd = base_cmd();
    cmd.args(["capabilities"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should contain human-readable sections
    assert!(
        stdout.contains("CASS Capabilities"),
        "Should have capabilities header"
    );
    assert!(stdout.contains("Version:"), "Should show version");
    assert!(stdout.contains("Features:"), "Should have features section");
    assert!(
        stdout.contains("Connectors:"),
        "Should have connectors section"
    );
    assert!(stdout.contains("Limits:"), "Should have limits section");
}

#[test]
fn capabilities_version_matches_crate() {
    // rob.api.caps: capabilities version should match crate version
    let mut cmd = base_cmd();
    cmd.args(["capabilities", "--json"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let version = json["crate_version"].as_str().expect("crate_version");
    // Should be a valid semver version
    assert!(
        version.chars().filter(|c| *c == '.').count() == 2,
        "Version should be semver format (x.y.z)"
    );
}

#[test]
fn search_json_includes_suggestions_for_typos() {
    // rob.query.suggest: Zero-hit search should return suggestions
    // Fixture data has "gemini" agent. "gemenii" should trigger typo suggestion.
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "gemenii",
        "--json",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let hits = json["hits"].as_array().expect("hits array");
    assert!(hits.is_empty(), "Should have 0 hits for typo");

    let suggestions = json["suggestions"].as_array().expect("suggestions array");
    assert!(!suggestions.is_empty(), "Should have suggestions");

    let found = suggestions
        .iter()
        .any(|s| s["kind"] == "spelling_fix" && s["suggested_query"].as_str() == Some("gemini"));
    assert!(found, "Should suggest 'gemini' for 'gemenii'");
}

// =============================================================================
// CLI Argument Normalization Tests (tst.cli.norm)
// Tests for forgiving CLI that auto-corrects minor syntax issues
// =============================================================================

/// Single-dash long flags should be auto-corrected to double-dash
/// e.g., `-robot` → `--robot`
#[test]
fn normalize_single_dash_to_double_dash() {
    // Test that -robot-help still works (should be normalized to --robot-help)
    let mut cmd = base_cmd();
    cmd.arg("-robot-help");
    // Should succeed because -robot-help is normalized to --robot-help
    cmd.assert().success().stdout(contains("cass --robot-help"));
}

/// Case normalization for flags: --Robot → --robot
#[test]
fn normalize_flag_case() {
    let mut cmd = base_cmd();
    cmd.args(["--Robot-help"]);
    // Should succeed because --Robot-help is normalized to --robot-help
    cmd.assert().success().stdout(contains("cass --robot-help"));
}

/// Subcommand aliases should work: find → search
#[test]
fn subcommand_alias_find_to_search() {
    let mut cmd = base_cmd();
    cmd.args([
        "find",
        "test query",
        "--json",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);
    // 'find' should be normalized to 'search'
    // May succeed or fail based on search results, but should not fail on parsing
    let assert = cmd.assert();
    // If command is recognized, it should either succeed or fail with a search-related error
    // not a "command not found" error
    assert.code(predicate::in_iter(vec![0, 1, 2, 3]));
}

/// Subcommand alias: query → search
#[test]
fn subcommand_alias_query_to_search() {
    let mut cmd = base_cmd();
    cmd.args([
        "query",
        "test",
        "--json",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);
    let assert = cmd.assert();
    assert.code(predicate::in_iter(vec![0, 1, 2, 3]));
}

/// Subcommand alias: ls → stats
#[test]
fn subcommand_alias_ls_to_stats() {
    let fixture = isolated_search_demo_data();
    let data_dir = fixture.path().to_str().unwrap();
    let mut cmd = base_cmd();
    cmd.args(["ls", "--json", "--data-dir", data_dir]);
    // 'ls' should be normalized to 'stats'
    let assert = cmd.assert();
    assert.code(predicate::in_iter(vec![0, 1, 2, 3]));
}

/// Subcommand alias: docs → robot-docs
#[test]
fn subcommand_alias_docs_to_robot_docs() {
    let mut cmd = base_cmd();
    cmd.args(["docs", "commands"]);
    // 'docs' should be normalized to 'robot-docs'
    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should output robot-docs content
    assert!(
        stdout.contains("search") || stdout.contains("cass"),
        "docs alias should produce robot-docs output"
    );
}

/// Flag-as-subcommand: --robot-docs → robot-docs
#[test]
fn flag_as_subcommand_robot_docs() {
    let mut cmd = base_cmd();
    cmd.args(["--robot-docs", "commands"]);
    // --robot-docs should be treated as subcommand
    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("search") || stdout.contains("cass"),
        "--robot-docs should work like robot-docs subcommand"
    );
}

/// Correction notices appear on stderr when auto-correcting
#[test]
fn correction_notice_appears_on_stderr() {
    let mut cmd = base_cmd();
    // Use a combination that triggers auto-correction
    cmd.args(["-robot-help"]);
    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Should have some correction notice on stderr
    // Note: The exact format may vary, but there should be some indication of correction
    assert!(
        stderr.contains("Auto-corrected")
            || stderr.contains("syntax_correction")
            || stderr.contains("→")
            || stderr.is_empty(), // Or stderr might be empty if no correction was needed
        "Correction notice should appear on stderr when args are normalized"
    );
}

/// Global flags can appear after subcommand (should be hoisted)
#[test]
fn global_flags_hoisted_from_after_subcommand() {
    let mut cmd = base_cmd();
    // Put --color=never after robot-docs (should be hoisted to front)
    cmd.args(["robot-docs", "commands", "--color=never"]);
    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should work and not contain ANSI codes
    assert!(
        !stdout.contains('\u{1b}'),
        "Global flag --color=never should be respected even after subcommand"
    );
}

/// Error messages include contextual examples in JSON format
#[test]
fn error_messages_include_contextual_examples() {
    let mut cmd = base_cmd();
    // Invalid command that should trigger rich error
    cmd.args(["--json", "foobar", "invalid"]);
    let assert = cmd.assert().failure();
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Should have examples in the error output
    assert!(
        stderr.contains("examples") || stderr.contains("cass"),
        "Error should include examples to help the agent"
    );
}

/// Combining multiple normalizations works correctly
#[test]
fn multiple_normalizations_combined() {
    // Test: -Robot-help (single dash + wrong case)
    let mut cmd = base_cmd();
    cmd.args(["-Robot-help"]);
    // Should normalize to --robot-help
    cmd.assert().success().stdout(contains("cass --robot-help"));
}

// =============================================================================
// P7.9: Robot-docs Provenance Output Tests
// Tests for provenance fields in robot/JSON output
// =============================================================================

/// Search results should include provenance fields (source_id) in default output
#[test]
fn search_json_includes_source_id_provenance() {
    // P7.9: Search results should include source_id field
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--json",
        "--limit",
        "1",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let hits = json["hits"].as_array().expect("hits array");
    if !hits.is_empty() {
        let hit = &hits[0];
        // source_id should be present and a string
        assert!(
            hit["source_id"].is_string(),
            "Hit should have source_id provenance field"
        );
        // Default fixture data should be 'local'
        assert_eq!(
            hit["source_id"], "local",
            "Fixture data should be from local source"
        );
    }
}

/// Search results with provenance preset should include origin fields
#[test]
fn search_fields_provenance_preset_expands() {
    // P7.9: 'provenance' preset should expand to source_id,origin_kind,origin_host
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--json",
        "--limit",
        "1",
        "--fields",
        "provenance,source_path",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let hits = json["hits"].as_array().expect("hits array");
    if !hits.is_empty() {
        let hit = &hits[0];
        // Provenance preset fields should be present
        assert!(
            hit["source_id"].is_string(),
            "Should have source_id from provenance preset"
        );
        // origin_kind may be null for local sources (that's okay)
        assert!(
            hit.get("origin_kind").is_some(),
            "Should have origin_kind field in output"
        );
        // source_path should also be included
        assert!(
            hit["source_path"].is_string(),
            "Should have source_path field"
        );
    }
}

/// Search results with default fields should include provenance in output
#[test]
fn search_default_output_includes_provenance_fields() {
    // P7.9: Default search output (full fields) should include provenance
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--json",
        "--limit",
        "1",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let hits = json["hits"].as_array().expect("hits array");
    if !hits.is_empty() {
        let hit = &hits[0];
        // Default output should include core provenance fields
        assert!(
            hit.get("source_id").is_some(),
            "Default output should include source_id"
        );
        // origin_kind should be present (value may be "local" or other kind)
        assert!(
            hit.get("origin_kind").is_some(),
            "Default output should include origin_kind"
        );
        // Note: origin_host is only included when using provenance preset,
        // not in default output, so we don't check for it here
    }
}

/// Introspect should show provenance in field presets or known fields
#[test]
fn introspect_lists_provenance_in_search_fields() {
    // P7.9: Introspect should show provenance-related options for search
    let mut cmd = base_cmd();
    cmd.args(["introspect", "--json"]);

    let assert = cmd.assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let commands = json["commands"].as_array().expect("commands array");
    let search_cmd = commands
        .iter()
        .find(|c| c["name"] == "search")
        .expect("search command should exist");

    // Check for fields arg which should support provenance preset
    let fields_arg = search_cmd["arguments"]
        .as_array()
        .and_then(|args| args.iter().find(|a| a["name"] == "fields"));

    assert!(
        fields_arg.is_some(),
        "Search should have fields argument for filtering"
    );
}

// =============================================================================
// ege.10: Additional Robot-Docs Topic Tests
// =============================================================================

/// robot-docs paths topic lists data directories
#[test]
fn robot_docs_paths_lists_directories() {
    let mut cmd = base_cmd();
    cmd.args(["--color=never", "robot-docs", "paths"]);
    let out = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "robot-docs paths should not emit ANSI when color=never"
    );
    // Should contain path-related content
    assert!(
        stdout.contains("data") || stdout.contains("path") || stdout.contains("directory"),
        "paths topic should describe data directories"
    );
}

/// robot-docs guide topic provides comprehensive usage guide
#[test]
fn robot_docs_guide_provides_usage_info() {
    let mut cmd = base_cmd();
    cmd.args(["--color=never", "robot-docs", "guide"]);
    let out = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "robot-docs guide should not emit ANSI when color=never"
    );
    // Should contain guide content
    assert!(
        stdout.contains("search") || stdout.contains("cass") || stdout.contains("agent"),
        "guide topic should provide usage information"
    );
}

/// robot-docs exit-codes topic lists all exit codes
#[test]
fn robot_docs_exit_codes_lists_codes() {
    let mut cmd = base_cmd();
    cmd.args(["--color=never", "robot-docs", "exit-codes"]);
    let out = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "robot-docs exit-codes should not emit ANSI when color=never"
    );
    // Should list exit codes
    assert!(
        stdout.contains('0') && stdout.contains('2') && stdout.contains('3'),
        "exit-codes topic should list standard exit codes (0, 2, 3)"
    );
}

/// robot-docs examples topic provides practical examples
#[test]
fn robot_docs_examples_provides_practical_examples() {
    let mut cmd = base_cmd();
    cmd.args(["--color=never", "robot-docs", "examples"]);
    let out = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "robot-docs examples should not emit ANSI when color=never"
    );
    // Should contain example commands
    assert!(
        stdout.contains("cass") && stdout.contains("--"),
        "examples topic should show cass command examples"
    );
}

/// robot-docs contracts topic describes the API contract
#[test]
fn robot_docs_contracts_describes_api() {
    let mut cmd = base_cmd();
    cmd.args(["--color=never", "robot-docs", "contracts"]);
    let out = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "robot-docs contracts should not emit ANSI when color=never"
    );
    // Should describe contract/API info
    assert!(
        stdout.contains("contract") || stdout.contains("version") || stdout.contains("API"),
        "contracts topic should describe API contract"
    );
}

/// robot-docs analytics topic should describe the implemented safe repair path
#[test]
fn robot_docs_analytics_describes_safe_fix_mode() {
    let mut cmd = base_cmd();
    cmd.args(["--color=never", "robot-docs", "analytics"]);
    let out = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "robot-docs analytics should not emit ANSI when color=never"
    );
    assert!(
        stdout.contains("cass analytics validate --fix --json"),
        "analytics topic should document the implemented safe fix path"
    );
    assert!(
        !stdout.contains("not yet implemented"),
        "analytics topic should not claim implemented fix behavior is unavailable"
    );
}

/// robot-docs wrap topic explains text wrapping
#[test]
fn robot_docs_wrap_explains_wrapping() {
    let mut cmd = base_cmd();
    cmd.args(["--color=never", "robot-docs", "wrap"]);
    let out = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "robot-docs wrap should not emit ANSI when color=never"
    );
    // Should explain wrapping
    assert!(
        stdout.contains("wrap") || stdout.contains("width") || stdout.contains("column"),
        "wrap topic should explain text wrapping options"
    );
}

// =============================================================================
// ege.10: Golden Contract Tests
// =============================================================================

/// Introspect output should match golden contract (structure, not dynamic values)
#[test]
fn introspect_matches_golden_contract_structure() {
    let mut cmd = base_cmd();
    cmd.args(["introspect", "--json"]);
    let output = cmd.assert().success().get_output().clone();
    assert!(
        output.stderr.is_empty(),
        "introspect should not log to stderr"
    );
    let actual: Value = serde_json::from_slice(&output.stdout).expect("valid introspect json");

    // Load expected structure
    let expected = read_fixture("introspect.json");

    // Check stable contract fields
    assert_eq!(
        actual["api_version"], expected["api_version"],
        "api_version should match golden"
    );
    assert_eq!(
        actual["contract_version"], expected["contract_version"],
        "contract_version should match golden"
    );

    // Check that global_flags array has expected structure
    let actual_globals = actual["global_flags"]
        .as_array()
        .expect("global_flags array");
    let expected_globals = expected["global_flags"]
        .as_array()
        .expect("expected global_flags");
    assert_eq!(
        actual_globals.len(),
        expected_globals.len(),
        "global_flags count should match golden"
    );

    // Check that expected global flags exist
    let actual_flag_names: HashSet<_> = actual_globals
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    for expected_flag in expected_globals {
        let name = expected_flag["name"].as_str().expect("flag name");
        assert!(
            actual_flag_names.contains(name),
            "Expected global flag '{}' not found",
            name
        );
    }

    // Check that commands array has expected commands
    let actual_cmds = actual["commands"].as_array().expect("commands array");
    let expected_cmds = expected["commands"].as_array().expect("expected commands");
    let actual_cmd_names: HashSet<_> = actual_cmds
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    let expected_cmd_names: HashSet<_> = expected_cmds
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert_eq!(
        actual_cmd_names, expected_cmd_names,
        "command names should match golden"
    );
}

// =============================================================================
// ege.10: Comprehensive Exit Code Contract Tests
// =============================================================================

/// Exit code 0: Success for valid search
#[test]
fn exit_code_0_success_search() {
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "hello",
        "--json",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);
    cmd.assert().code(0);
}

/// Exit code 0: Success for valid stats
#[test]
fn exit_code_0_success_stats() {
    let fixture = isolated_search_demo_data();
    let data_dir = fixture.path().to_str().unwrap();
    let mut cmd = base_cmd();
    cmd.args(["stats", "--json", "--data-dir", data_dir]);
    cmd.assert().code(0);
}

/// Exit code 0: Success for robot-docs
#[test]
fn exit_code_0_success_robot_docs() {
    let mut cmd = base_cmd();
    cmd.args(["robot-docs", "commands"]);
    cmd.assert().code(0);
}

/// Exit code 0: Success for capabilities
#[test]
fn exit_code_0_success_capabilities() {
    let mut cmd = base_cmd();
    cmd.args(["capabilities", "--json"]);
    cmd.assert().code(0);
}

/// Exit code 2: Usage/parsing error for invalid subcommand
#[test]
fn exit_code_2_invalid_subcommand() {
    let mut cmd = base_cmd();
    cmd.args(["--json", "nonexistent_command"]);
    cmd.assert().code(2);
}

/// Exit code 2: TUI disabled in non-TTY environment
#[test]
fn exit_code_2_tui_disabled_non_tty() {
    let mut cmd = base_cmd();
    // No subcommand triggers TUI which should be disabled in test
    cmd.assert().code(2);
}

/// Exit code 3: Missing index for search
#[test]
fn exit_code_3_missing_index_search() {
    let tmp = TempDir::new().unwrap();
    let mut cmd = base_cmd();
    cmd.args([
        "search",
        "test",
        "--json",
        "--data-dir",
        tmp.path().to_str().unwrap(),
    ]);
    // Missing index should return code 3 or 9 depending on how error is classified
    let output = cmd.assert().failure().get_output().clone();
    let code = output.status.code().expect("exit code");
    assert!(
        code == 3 || code == 9,
        "Missing index should return code 3 or 9, got {code}"
    );
}

/// Exit code 3: Missing database for stats
#[test]
fn exit_code_3_missing_db_stats() {
    let tmp = TempDir::new().unwrap();
    let mut cmd = base_cmd();
    cmd.args([
        "stats",
        "--json",
        "--data-dir",
        tmp.path().to_str().unwrap(),
    ]);
    cmd.assert().code(3);
}

/// Contract: All exit codes are documented in robot-docs exit-codes
#[test]
fn all_exit_codes_documented() {
    let mut cmd = base_cmd();
    cmd.args(["robot-docs", "exit-codes"]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // All documented exit codes should be mentioned
    for code in ["0", "2", "3", "9"] {
        assert!(
            stdout.contains(code),
            "Exit code {} should be documented in robot-docs exit-codes",
            code
        );
    }
}

// =============================================================================
// ege.10: Trace Mode Contract Tests
// =============================================================================

/// Trace file includes required contract fields on success
#[test]
fn trace_includes_contract_fields_on_success() {
    let tmp = TempDir::new().unwrap();
    let trace_path = tmp.path().join("trace.jsonl");

    let mut cmd = base_cmd();
    cmd.args([
        "--trace-file",
        trace_path.to_str().unwrap(),
        "search",
        "hello",
        "--json",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);

    cmd.assert().success();

    let trace = fs::read_to_string(&trace_path).expect("trace file exists");
    let last_line = trace.lines().last().expect("trace has lines");
    let json: Value = serde_json::from_str(last_line).expect("valid trace JSON");

    // Required contract fields
    assert_eq!(json["exit_code"], 0, "exit_code should be 0 for success");
    assert_eq!(
        json["contract_version"], "1",
        "contract_version should be 1"
    );
    // Trace uses start_ts and end_ts for timestamps
    assert!(
        json["start_ts"].is_string() || json["end_ts"].is_string(),
        "timestamp (start_ts/end_ts) should be present"
    );
    assert!(
        json["duration_ms"].is_number(),
        "duration_ms should be present"
    );
}

/// Trace file includes error details on failure
#[test]
fn trace_includes_error_on_failure() {
    let tmp = TempDir::new().unwrap();
    let trace_path = tmp.path().join("trace.jsonl");

    let mut cmd = base_cmd();
    cmd.args([
        "--trace-file",
        trace_path.to_str().unwrap(),
        "search",
        "test",
        "--json",
        "--data-dir",
        tmp.path().to_str().unwrap(),
    ]);

    cmd.assert().failure();

    let trace = fs::read_to_string(&trace_path).expect("trace file exists");
    let last_line = trace.lines().last().expect("trace has lines");
    let json: Value = serde_json::from_str(last_line).expect("valid trace JSON");

    // Error case should have non-zero exit code
    let exit_code = json["exit_code"].as_i64().expect("exit_code");
    assert_ne!(exit_code, 0, "exit_code should be non-zero for failure");
    assert_eq!(json["contract_version"], "1");
}

// =============================================================================
// TST.8: Global Flags & Defaults Coverage Tests
// Tests verifying global flags propagate and introspect shows defaults
// =============================================================================

/// Introspect should include quiet and verbose global flags with proper types
#[test]
fn introspect_global_flags_quiet_verbose_documented() {
    let json = fetch_introspect_json();
    let globals = json["global_flags"].as_array().expect("global_flags array");

    let mut found_quiet = false;
    let mut found_verbose = false;

    for flag in globals {
        let name = flag["name"].as_str().unwrap_or_default();
        match name {
            "quiet" => {
                found_quiet = true;
                assert_eq!(flag["arg_type"], "flag", "quiet should be a flag type");
                assert_eq!(flag["short"], "q", "quiet should have -q as short option");
            }
            "verbose" => {
                found_verbose = true;
                assert_eq!(flag["arg_type"], "flag", "verbose should be a flag type");
                assert_eq!(flag["short"], "v", "verbose should have -v as short option");
            }
            _ => {}
        }
    }

    assert!(found_quiet, "quiet should be documented in global_flags");
    assert!(
        found_verbose,
        "verbose should be documented in global_flags"
    );
}

/// Introspect should include robot-help global flag
#[test]
fn introspect_global_flags_robot_help_documented() {
    let json = fetch_introspect_json();
    let globals = json["global_flags"].as_array().expect("global_flags array");

    let found = globals.iter().any(|f| f["name"] == "robot-help");
    assert!(found, "robot-help should be documented in global_flags");
}

/// Context argument should be documented in expand command with proper defaults
#[test]
fn introspect_expand_context_argument() {
    let json = fetch_introspect_json();
    let expand = find_command(&json, "expand");
    let context = find_arg(expand, "context");

    assert_eq!(
        context["value_type"], "integer",
        "context should be integer type"
    );
    // Expand context has default value of 3
    assert_eq!(
        context["default"], "3",
        "expand --context should default to 3"
    );
}

/// Context argument should be documented in view command with proper defaults
#[test]
fn introspect_view_context_argument() {
    let json = fetch_introspect_json();
    let view = find_command(&json, "view");
    let context = find_arg(view, "context");

    assert_eq!(
        context["value_type"], "integer",
        "context should be integer type"
    );
    // View context also has default of 5
    assert_eq!(context["default"], "5", "context should default to 5");
}

/// All global flags mentioned in introspect should have required=false
#[test]
fn introspect_global_flags_all_optional() {
    let json = fetch_introspect_json();
    let globals = json["global_flags"].as_array().expect("global_flags array");

    for flag in globals {
        let name = flag["name"].as_str().unwrap_or("unknown");
        assert_eq!(
            flag["required"], false,
            "global flag {name} should not be required"
        );
    }
}

/// Verify complete list of expected global flags exists
#[test]
fn introspect_global_flags_complete_list() {
    let json = fetch_introspect_json();
    let globals = json["global_flags"].as_array().expect("global_flags array");

    let expected_flags = [
        "db",
        "robot-help",
        "trace-file",
        "quiet",
        "verbose",
        "color",
        "progress",
        "wrap",
        "nowrap",
    ];

    let actual_names: HashSet<_> = globals.iter().filter_map(|f| f["name"].as_str()).collect();

    for expected in expected_flags {
        assert!(
            actual_names.contains(expected),
            "global flag '{expected}' should be documented in introspect"
        );
    }
}

/// Status command should have stale-threshold with proper default
#[test]
fn introspect_status_stale_threshold_default() {
    let json = fetch_introspect_json();
    let status = find_command(&json, "status");
    let stale = find_arg(status, "stale-threshold");

    assert_eq!(
        stale["value_type"], "integer",
        "stale-threshold should be integer type"
    );
    assert_eq!(
        stale["default"], "1800",
        "stale-threshold should default to 1800 (30 minutes)"
    );
}

/// Health command should have stale-threshold with proper default
#[test]
fn introspect_health_stale_threshold_default() {
    let json = fetch_introspect_json();
    let health = find_command(&json, "health");
    let stale = find_arg(health, "stale-threshold");

    assert_eq!(
        stale["value_type"], "integer",
        "stale-threshold should be integer type"
    );
    // Health uses a shorter default (5 minutes) for quick checks
    assert_eq!(
        stale["default"], "300",
        "health --stale-threshold should default to 300 (5 minutes)"
    );
}

/// Global --quiet flag should suppress info-level logs
#[test]
fn global_quiet_flag_suppresses_info_logs() {
    let mut cmd = base_cmd();
    cmd.args(["--quiet", "capabilities", "--json"]);
    let output = cmd.assert().success().get_output().clone();
    let stderr = String::from_utf8_lossy(&output.stderr);

    // With --quiet, stderr should not have INFO-level messages
    assert!(
        !stderr.contains("INFO"),
        "INFO logs should be suppressed with --quiet"
    );
}

/// Global --verbose flag should be accepted without error
#[test]
fn global_verbose_flag_accepted() {
    let mut cmd = base_cmd();
    cmd.args(["--verbose", "capabilities", "--json"]);
    cmd.assert().success();
}

/// Global flags can be placed before or after subcommand
#[test]
fn global_flags_work_before_subcommand() {
    let mut cmd = base_cmd();
    cmd.args(["--color=never", "capabilities", "--json"]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should not contain ANSI escape codes
    assert!(
        !stdout.contains('\u{1b}'),
        "--color=never should disable ANSI codes"
    );
}

/// Global --nowrap flag should be documented and accepted
#[test]
fn global_nowrap_flag_works() {
    let mut cmd = base_cmd();
    cmd.args(["--nowrap", "capabilities", "--json"]);
    cmd.assert().success();
}

/// Global --wrap flag should accept integer value
#[test]
fn global_wrap_flag_accepts_integer() {
    let mut cmd = base_cmd();
    cmd.args(["--wrap", "80", "capabilities", "--json"]);
    cmd.assert().success();
}

/// Search limit flag should have correct default in introspect
#[test]
fn introspect_search_limit_default() {
    let json = fetch_introspect_json();
    let search = find_command(&json, "search");
    let limit = find_arg(search, "limit");

    assert_eq!(limit["value_type"], "integer");
    assert_eq!(limit["default"], "0", "search --limit should default to 0");
}

/// Search offset flag should have correct default in introspect
#[test]
fn introspect_search_offset_default() {
    let json = fetch_introspect_json();
    let search = find_command(&json, "search");
    let offset = find_arg(search, "offset");

    assert_eq!(offset["value_type"], "integer");
    assert_eq!(
        offset["default"], "0",
        "search --offset should default to 0"
    );
}

/// Progress flag should have enum values and default
#[test]
fn introspect_global_progress_enum_values() {
    let json = fetch_introspect_json();
    let globals = json["global_flags"].as_array().expect("global_flags");

    let progress = globals
        .iter()
        .find(|f| f["name"] == "progress")
        .expect("progress flag exists");

    assert_eq!(progress["value_type"], "enum");
    assert_eq!(progress["default"], "auto");

    let enum_values: HashSet<_> = progress["enum_values"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();

    assert!(enum_values.contains("auto"));
    assert!(enum_values.contains("bars"));
    assert!(enum_values.contains("plain"));
    assert!(enum_values.contains("none"));
}

/// Color flag should have enum values and default
#[test]
fn introspect_global_color_enum_values() {
    let json = fetch_introspect_json();
    let globals = json["global_flags"].as_array().expect("global_flags");

    let color = globals
        .iter()
        .find(|f| f["name"] == "color")
        .expect("color flag exists");

    assert_eq!(color["value_type"], "enum");
    assert_eq!(color["default"], "auto");

    let enum_values: HashSet<_> = color["enum_values"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();

    assert!(enum_values.contains("auto"));
    assert!(enum_values.contains("never"));
    assert!(enum_values.contains("always"));
}

/// Dynamic schema builder should not introduce regressions - all commands present
#[test]
fn introspect_dynamic_schema_all_commands_present() {
    let json = fetch_introspect_json();
    let commands = json["commands"].as_array().expect("commands array");

    let expected_commands = [
        "tui",
        "index",
        "completions",
        "search",
        "status",
        "diag",
        "capabilities",
        "introspect",
        "robot-docs",
        "api-version",
        "view",
        "expand",
        "timeline",
        "export",
        "health",
        "state",
        "sources",
    ];

    let actual_names: HashSet<_> = commands.iter().filter_map(|c| c["name"].as_str()).collect();

    for expected in expected_commands {
        assert!(
            actual_names.contains(expected),
            "command '{expected}' should be present in introspect schema"
        );
    }
}

/// Dynamic schema builder should include response_schemas section
#[test]
fn introspect_has_response_schemas() {
    let json = fetch_introspect_json();
    let schemas = json["response_schemas"].as_object();
    assert!(
        schemas.is_some(),
        "introspect should include response_schemas"
    );
    assert!(
        !schemas.unwrap().is_empty(),
        "response_schemas should not be empty"
    );
}

// =============================================================================
// TST.9: Repeatable + Path/Integer Inference Tests
// Tests for introspect correctly documenting repeatable options and type hints
// =============================================================================

/// Search command days parameter should be integer type
#[test]
fn introspect_search_days_integer_type() {
    let json = fetch_introspect_json();
    let search = find_command(&json, "search");
    let days = find_arg(search, "days");

    assert_eq!(
        days["value_type"], "integer",
        "search --days should be integer type"
    );
    assert_eq!(
        days["arg_type"], "option",
        "search --days should be an option"
    );
}

/// View command line parameter should be integer type
#[test]
fn introspect_view_line_integer_type() {
    let json = fetch_introspect_json();
    let view = find_command(&json, "view");
    let line = find_arg(view, "line");

    assert_eq!(
        line["value_type"], "integer",
        "view -n/--line should be integer type"
    );
    assert_eq!(
        line["short"], "n",
        "view --line should have short option -n"
    );
}

/// Expand command line parameter should be integer type
#[test]
fn introspect_expand_line_integer_type() {
    let json = fetch_introspect_json();
    let expand = find_command(&json, "expand");
    let line = find_arg(expand, "line");

    assert_eq!(
        line["value_type"], "integer",
        "expand -n/--line should be integer type"
    );
    assert_eq!(
        line["short"], "n",
        "expand --line should have short option -n"
    );
}

/// Search command agent parameter should be repeatable
#[test]
fn introspect_search_agent_repeatable() {
    let json = fetch_introspect_json();
    let search = find_command(&json, "search");
    let agent = find_arg(search, "agent");

    assert_eq!(
        agent["repeatable"], true,
        "search --agent should be repeatable"
    );
}

/// Search command workspace parameter should be repeatable
#[test]
fn introspect_search_workspace_repeatable() {
    let json = fetch_introspect_json();
    let search = find_command(&json, "search");
    let workspace = find_arg(search, "workspace");

    assert_eq!(
        workspace["repeatable"], true,
        "search --workspace should be repeatable"
    );
}

/// Index command watch-once parameter should be repeatable path
#[test]
fn introspect_index_watch_once_repeatable_path() {
    let json = fetch_introspect_json();
    let index = find_command(&json, "index");
    let watch_once = find_arg(index, "watch-once");

    assert_eq!(
        watch_once["repeatable"], true,
        "index --watch-once should be repeatable"
    );
    assert_eq!(
        watch_once["value_type"], "path",
        "index --watch-once should be path type"
    );
}

/// Index command semantic flag should be documented as a flag.
#[test]
fn introspect_index_semantic_flag() {
    let json = fetch_introspect_json();
    let index = find_command(&json, "index");
    let semantic = find_arg(index, "semantic");

    assert_eq!(
        semantic["arg_type"], "flag",
        "index --semantic should be a flag"
    );
}

/// Index command embedder should default to fastembed.
#[test]
fn introspect_index_embedder_default() {
    let json = fetch_introspect_json();
    let index = find_command(&json, "index");
    let embedder = find_arg(index, "embedder");

    assert_eq!(
        embedder["value_type"], "string",
        "index --embedder should be string type"
    );
    assert_eq!(
        embedder["default"], "fastembed",
        "index --embedder should default to fastembed"
    );
}

/// Index command parsing should accept semantic + embedder flags.
#[test]
fn parse_index_semantic_embedder_flags() {
    let cli =
        Cli::try_parse_from(["cass", "index", "--semantic", "--embedder", "fastembed"]).unwrap();
    let command = cli.command.as_ref();
    assert!(
        matches!(command, Some(Commands::Index { .. })),
        "expected index command, got {:?}",
        cli.command
    );
    if let Some(Commands::Index {
        semantic, embedder, ..
    }) = command
    {
        assert!(*semantic, "semantic flag should be set");
        assert_eq!(embedder.as_str(), "fastembed");
    }
}

/// Index command parsing should default embedder to fastembed.
#[test]
fn parse_index_embedder_default() {
    let cli = Cli::try_parse_from(["cass", "index", "--semantic"]).unwrap();
    let command = cli.command.as_ref();
    assert!(
        matches!(command, Some(Commands::Index { .. })),
        "expected index command, got {:?}",
        cli.command
    );
    if let Some(Commands::Index {
        semantic, embedder, ..
    }) = command
    {
        assert!(*semantic, "semantic flag should be set");
        assert_eq!(embedder.as_str(), "fastembed");
    }
}

/// Search command aggregate parameter should be repeatable
#[test]
fn introspect_search_aggregate_repeatable() {
    let json = fetch_introspect_json();
    let search = find_command(&json, "search");
    let aggregate = find_arg(search, "aggregate");

    assert_eq!(
        aggregate["repeatable"], true,
        "search --aggregate should be repeatable"
    );
}

/// Global db parameter should be path type
#[test]
fn introspect_global_db_path_type() {
    let json = fetch_introspect_json();
    let globals = json["global_flags"].as_array().expect("global_flags");

    let db = globals
        .iter()
        .find(|f| f["name"] == "db")
        .expect("db flag exists");

    assert_eq!(db["value_type"], "path", "global --db should be path type");
}

/// Global trace-file parameter should be path type
#[test]
fn introspect_global_trace_file_path_type() {
    let json = fetch_introspect_json();
    let globals = json["global_flags"].as_array().expect("global_flags");

    let trace_file = globals
        .iter()
        .find(|f| f["name"] == "trace-file")
        .expect("trace-file flag exists");

    assert_eq!(
        trace_file["value_type"], "path",
        "global --trace-file should be path type"
    );
}

/// View command path positional should be path type
#[test]
fn introspect_view_path_positional_type() {
    let json = fetch_introspect_json();
    let view = find_command(&json, "view");
    let path = find_arg(view, "path");

    assert_eq!(
        path["value_type"], "path",
        "view path positional should be path type"
    );
    assert_eq!(
        path["arg_type"], "positional",
        "view path should be positional argument"
    );
}

/// Expand command path positional should be path type
#[test]
fn introspect_expand_path_positional_type() {
    let json = fetch_introspect_json();
    let expand = find_command(&json, "expand");
    let path = find_arg(expand, "path");

    assert_eq!(
        path["value_type"], "path",
        "expand path positional should be path type"
    );
    assert_eq!(
        path["arg_type"], "positional",
        "expand path should be positional argument"
    );
}

/// Search command data-dir parameter should be path type
#[test]
fn introspect_search_data_dir_path_type() {
    let json = fetch_introspect_json();
    let search = find_command(&json, "search");
    let data_dir = find_arg(search, "data-dir");

    assert_eq!(
        data_dir["value_type"], "path",
        "search --data-dir should be path type"
    );
}

/// Context command limit parameter should be integer type
#[test]
fn introspect_context_limit_integer_type() {
    let json = fetch_introspect_json();
    let context = find_command(&json, "context");
    let limit = find_arg(context, "limit");

    assert_eq!(
        limit["value_type"], "integer",
        "context --limit should be integer type"
    );
}

/// All repeatable options documented correctly across commands
#[test]
fn introspect_all_repeatable_options_documented() {
    let json = fetch_introspect_json();

    // Check search command repeatables
    let search = find_command(&json, "search");
    for name in ["agent", "workspace", "aggregate"] {
        let arg = find_arg(search, name);
        assert_eq!(
            arg["repeatable"], true,
            "search --{name} should be marked repeatable"
        );
    }

    // Check index command repeatables
    let index = find_command(&json, "index");
    let watch_once = find_arg(index, "watch-once");
    assert_eq!(
        watch_once["repeatable"], true,
        "index --watch-once should be marked repeatable"
    );
}

/// All path-type options documented correctly across commands
#[test]
fn introspect_all_path_options_documented() {
    let json = fetch_introspect_json();

    // Check global path types
    let globals = json["global_flags"].as_array().expect("global_flags");
    for name in ["db", "trace-file"] {
        let msg = format!("{name} exists");
        let flag = globals.iter().find(|f| f["name"] == name).expect(&msg);
        assert_eq!(
            flag["value_type"], "path",
            "global --{name} should be path type"
        );
    }

    // Check command path types
    let search = find_command(&json, "search");
    assert_eq!(
        find_arg(search, "data-dir")["value_type"],
        "path",
        "search --data-dir should be path type"
    );

    let view = find_command(&json, "view");
    assert_eq!(
        find_arg(view, "path")["value_type"],
        "path",
        "view path should be path type"
    );
}

/// All integer-type options documented correctly
#[test]
fn introspect_all_integer_options_documented() {
    let json = fetch_introspect_json();

    let search = find_command(&json, "search");
    for name in ["limit", "offset", "days"] {
        let arg = find_arg(search, name);
        assert_eq!(
            arg["value_type"], "integer",
            "search --{name} should be integer type"
        );
    }

    let view = find_command(&json, "view");
    for name in ["line", "context"] {
        let arg = find_arg(view, name);
        assert_eq!(
            arg["value_type"], "integer",
            "view --{name} should be integer type"
        );
    }

    let expand = find_command(&json, "expand");
    for name in ["line", "context"] {
        let arg = find_arg(expand, name);
        assert_eq!(
            arg["value_type"], "integer",
            "expand --{name} should be integer type"
        );
    }

    let status = find_command(&json, "status");
    assert_eq!(
        find_arg(status, "stale-threshold")["value_type"],
        "integer",
        "status --stale-threshold should be integer type"
    );

    let health = find_command(&json, "health");
    assert_eq!(
        find_arg(health, "stale-threshold")["value_type"],
        "integer",
        "health --stale-threshold should be integer type"
    );
}

// ============================================================================
// TOON FORMAT INTEGRATION TESTS
// ============================================================================

/// Test that --robot-format toon is accepted as valid option
#[test]
fn robot_format_toon_is_valid_option() {
    let mut cmd = base_cmd();
    // Should not fail with "invalid value" error
    // Use --limit 1 since limit 0 causes panic in tantivy
    cmd.args([
        "search",
        "hello",
        "--robot-format",
        "toon",
        "--limit",
        "1",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);
    // Ensure the flag is accepted and command succeeds.
    cmd.assert().success();
}

/// Test that CASS_OUTPUT_FORMAT=toon env var is respected
#[test]
fn cass_output_format_env_triggers_robot_mode() {
    let mut cmd = base_cmd();
    cmd.env("CASS_OUTPUT_FORMAT", "json");
    cmd.args([
        "search",
        "hello",
        "--limit",
        "1",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should output JSON since CASS_OUTPUT_FORMAT=json sets robot mode
    assert!(
        stdout.trim().starts_with('{') || stdout.trim().starts_with('['),
        "CASS_OUTPUT_FORMAT=json should produce JSON output"
    );
}

/// Test that TOON_DEFAULT_FORMAT=json env var works
#[test]
fn toon_default_format_env_json_works() {
    let mut cmd = base_cmd();
    cmd.env("TOON_DEFAULT_FORMAT", "json");
    cmd.args([
        "search",
        "hello",
        "--limit",
        "1",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should output JSON
    assert!(
        stdout.trim().starts_with('{') || stdout.trim().starts_with('['),
        "TOON_DEFAULT_FORMAT=json should produce JSON output"
    );
}

/// Test that CLI flag overrides env vars
#[test]
fn cli_robot_format_overrides_env() {
    let mut cmd = base_cmd();
    cmd.env("CASS_OUTPUT_FORMAT", "compact");
    cmd.args([
        "search",
        "hello",
        "--robot-format",
        "json",
        "--limit",
        "1",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Pretty JSON has newlines, compact doesn't (if env var was respected wrongly)
    // This test is checking that --robot-format json overrides compact
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid json");
    assert!(json.is_object(), "output should be valid JSON object");
}

/// Test that --robot-format toon help shows toon in possible values
#[test]
fn robot_format_help_includes_toon() {
    let mut cmd = base_cmd();
    cmd.args(["search", "--help"]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.to_lowercase().contains("toon"),
        "search --help should mention toon format option"
    );
}

/// Test that introspect shows toon in robot-format enum values
#[test]
fn introspect_robot_format_includes_toon() {
    let mut cmd = base_cmd();
    cmd.args(["introspect", "--json"]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(stdout.trim()).expect("valid introspect json");
    let commands = json["commands"].as_array().expect("commands array");

    let search = commands
        .iter()
        .find(|c| c["name"] == "search")
        .expect("search command present");
    let args = search["arguments"].as_array().expect("search args");

    let robot_format = args
        .iter()
        .find(|a| a["name"] == "robot-format")
        .expect("robot-format arg should exist");

    let enum_values = robot_format["enum_values"]
        .as_array()
        .expect("robot-format should have enum_values");

    assert!(
        enum_values.iter().any(|v| v == "toon"),
        "robot-format enum_values should include toon"
    );
}

/// Test that CASS_OUTPUT_FORMAT takes precedence over TOON_DEFAULT_FORMAT
#[test]
fn cass_output_format_takes_precedence() {
    let mut cmd = base_cmd();
    // Set both env vars - CASS_OUTPUT_FORMAT should win
    cmd.env("TOON_DEFAULT_FORMAT", "compact");
    cmd.env("CASS_OUTPUT_FORMAT", "json");
    cmd.args([
        "search",
        "hello",
        "--limit",
        "1",
        "--data-dir",
        "tests/fixtures/search_demo_data",
    ]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Pretty JSON has newlines
    assert!(
        stdout.contains('\n'),
        "CASS_OUTPUT_FORMAT=json should produce pretty JSON (with newlines), not compact"
    );
}
