//! Explicit `--trace-file` trace-surface regression suite.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.2.5
//! ("Gate dependency tracing behind explicit trace surfaces").
//!
//! Dependency-level logs (frankensqlite / frankensearch / asupersync) must NOT
//! appear during normal robot commands; they are available only through an
//! explicit surface. These tests pin that contract for `--trace-file`: even under
//! a deliberately noisy `RUST_LOG`, robot stdout stays pure JSON and stderr stays
//! free of dependency tracing. The explicit trace keeps CASS DEBUG plus global
//! WARN/ERROR events as bounded, redacted JSONL; no trace file is created when
//! the operator does not ask for one.

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

mod util;
use util::cass_bin;

type CliTraceTestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

macro_rules! trace_test_require {
    ($condition:expr, $($message:tt)+) => {
        if !$condition {
            return Err(format_args!($($message)+).to_string().into());
        }
    };
}

/// A `RUST_LOG` loud enough that any unfiltered dependency span would otherwise
/// flood stderr/stdout.
const NOISY_RUST_LOG: &str = "trace,fsqlite=trace,fsqlite_core=trace";

fn noisy_robot_cmd() -> Command {
    let mut cmd = Command::new(cass_bin());
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("RUST_LOG", NOISY_RUST_LOG);
    cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd
}

fn parse_stdout_json(stdout: &str) -> CliTraceTestResult<Value> {
    let trimmed = stdout.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Ok(value);
    }
    let last_line = trimmed
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or("robot stdout had no non-empty lines")?;
    serde_json::from_str::<Value>(last_line.trim()).map_err(|error| {
        format!("robot stdout was not valid JSON ({error}); stdout:\n{stdout}").into()
    })
}

fn check_stderr_has_no_dependency_tracing(label: &str, stderr: &str) -> CliTraceTestResult {
    let lower = stderr.to_lowercase();
    trace_test_require!(
        !lower.contains("fsqlite"),
        "{label}: stderr leaked frankensqlite tracing despite --trace-file routing; stderr:\n{stderr}"
    );
    for level in [" INFO ", " DEBUG ", " TRACE "] {
        trace_test_require!(
            !stderr.contains(level),
            "{label}: stderr emitted a {} line; deep logs should go to the trace file, not stderr; stderr:\n{stderr}",
            level.trim()
        );
    }
    Ok(())
}

fn read_trace_events(path: &std::path::Path) -> CliTraceTestResult<Vec<Value>> {
    let trace = std::fs::read_to_string(path)?;
    trace_test_require!(!trace.trim().is_empty(), "trace file should not be empty");
    let mut events = Vec::new();
    for (index, line) in trace
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
    {
        let event = serde_json::from_str::<Value>(line).map_err(|error| {
            format!(
                "trace line {} was not valid JSON ({error}): {line}",
                index + 1
            )
        })?;
        events.push(event);
    }
    trace_test_require!(
        events
            .iter()
            .all(|event| event["schema_version"] == "cass-trace-v1"),
        "every trace line must carry the versioned schema: {events:#?}"
    );
    trace_test_require!(
        events.iter().all(|event| {
            event.get("trace_id").is_some()
                && event.get("test_id").is_some()
                && (event["fields"].get("event").is_some()
                    || event["fields"].get("message").is_some())
        }),
        "every trace line must carry correlation and event/message fields: {events:#?}"
    );
    Ok(events)
}

fn command_summary(events: &[Value]) -> CliTraceTestResult<&Value> {
    events
        .iter()
        .rev()
        .find(|event| event["fields"]["event"] == "command_summary")
        .ok_or_else(|| "trace must include a command_summary event".into())
}

#[test]
fn trace_file_keeps_stdout_clean_and_captures_to_file() -> CliTraceTestResult {
    let tmp = TempDir::new()?;
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir)?;
    let trace_path = tmp.path().join("trace.jsonl");

    // `status` touches the storage layer (a dependency that emits tracing). Under
    // noisy RUST_LOG + --trace-file, deep logs must route to the file, not stderr.
    let output = noisy_robot_cmd()
        .args([
            "status",
            "--json",
            "--data-dir",
            data_dir.to_str().ok_or("data directory is not UTF-8")?,
            "--trace-file",
            trace_path.to_str().ok_or("trace path is not UTF-8")?,
        ])
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let json = parse_stdout_json(&stdout)?;
    trace_test_require!(
        json.is_object(),
        "status --json should be a JSON object: {json}"
    );
    check_stderr_has_no_dependency_tracing("status+trace-file", &stderr)?;

    // The trace file is the explicit surface: it must exist and carry content
    // (at minimum the per-invocation summary line; plus any captured diagnostics).
    trace_test_require!(
        trace_path.exists(),
        "trace file should be created when --trace-file is set"
    );
    let events = read_trace_events(&trace_path)?;
    let summary = command_summary(&events)?;
    trace_test_require!(
        summary["fields"]["cmd"] == "status",
        "trace summary should record the command"
    );
    trace_test_require!(
        !events.iter().any(|event| {
            event["level"] == "DEBUG"
                && event["target"]
                    .as_str()
                    .is_some_and(|target| target.starts_with("fsqlite"))
        }),
        "dependency DEBUG events must not enter the bounded trace artifact"
    );
    Ok(())
}

#[test]
fn no_trace_file_created_without_the_flag() -> CliTraceTestResult {
    let tmp = TempDir::new()?;
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir)?;
    let trace_path = tmp.path().join("should_not_exist.jsonl");

    let output = noisy_robot_cmd()
        .args([
            "status",
            "--json",
            "--data-dir",
            data_dir.to_str().ok_or("data directory is not UTF-8")?,
        ])
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = parse_stdout_json(&stdout)?;
    trace_test_require!(
        json.is_object(),
        "status --json should be a JSON object: {json}"
    );
    trace_test_require!(
        !trace_path.exists(),
        "no trace file should be created when --trace-file is not passed"
    );
    Ok(())
}

#[test]
fn trace_file_does_not_corrupt_stdout_for_api_version() -> CliTraceTestResult {
    let tmp = TempDir::new()?;
    let trace_path = tmp.path().join("trace.jsonl");
    let raw_secret = format!("sk-{}", "q".repeat(24));

    let output = noisy_robot_cmd()
        .env("CASS_TRACE_ID", &raw_secret)
        .args([
            "api-version",
            "--json",
            "--trace-file",
            trace_path.to_str().ok_or("trace path is not UTF-8")?,
        ])
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = parse_stdout_json(&stdout)?;
    trace_test_require!(
        json.get("api_version").is_some() || json.get("version").is_some(),
        "api-version stdout should be clean JSON even with --trace-file: {json}"
    );
    trace_test_require!(trace_path.exists(), "trace file should be created");
    let events = read_trace_events(&trace_path)?;
    let summary = command_summary(&events)?;
    trace_test_require!(
        summary["fields"]["cmd"] == "api-version",
        "command summary recorded the wrong command: {summary}"
    );
    let trace = std::fs::read_to_string(&trace_path)?;
    trace_test_require!(!trace.contains(&raw_secret), "trace leaked a secret token");
    trace_test_require!(
        matches!(
            summary.pointer("/fields/trace_id").and_then(Value::as_str),
            Some("[REDACTED]")
        ),
        "command summary leaked the trace correlation secret"
    );
    Ok(())
}

#[test]
fn failed_command_keeps_redacted_failure_diagnostics_in_trace() -> CliTraceTestResult {
    let tmp = TempDir::new()?;
    let trace_path = tmp.path().join("trace.jsonl");
    let missing_path = tmp.path().join("missing-session.jsonl");

    let output = noisy_robot_cmd()
        .args([
            "view",
            missing_path
                .to_str()
                .ok_or("missing-session path is not UTF-8")?,
            "--json",
            "--trace-file",
            trace_path.to_str().ok_or("trace path is not UTF-8")?,
        ])
        .output()?;
    trace_test_require!(
        !output.status.success(),
        "viewing a missing source must exercise the failure trace path"
    );

    let events = read_trace_events(&trace_path)?;
    let summary = command_summary(&events)?;
    trace_test_require!(
        summary["fields"]["cmd"] == "view",
        "failure summary recorded the wrong command: {summary}"
    );
    trace_test_require!(
        summary["fields"]["exit_code"]
            .as_i64()
            .is_some_and(|code| code != 0),
        "failure summary omitted its nonzero exit code: {summary}"
    );
    trace_test_require!(
        summary["fields"]["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "failure summary must retain an actionable, redacted diagnostic: {summary}"
    );
    trace_test_require!(
        summary["fields"]["error"]["kind"]
            .as_str()
            .is_some_and(|kind| !kind.is_empty()),
        "failure summary must retain the machine-readable reason code: {summary}"
    );
    trace_test_require!(
        summary["fields"]["duration_ms"].as_u64().is_some()
            && summary["fields"]["start_ts"].as_str().is_some()
            && summary["fields"]["end_ts"].as_str().is_some(),
        "failure summary must retain phase timing evidence: {summary}"
    );
    trace_test_require!(
        summary["fields"]["error"]["retryable"].as_bool().is_some(),
        "failure summary must retain the retry policy: {summary}"
    );
    let trace = std::fs::read_to_string(&trace_path)?;
    trace_test_require!(
        !trace.contains(missing_path.to_string_lossy().as_ref()),
        "failure trace leaked a private absolute path: {trace}"
    );
    Ok(())
}

#[test]
fn trace_file_obeys_explicit_hard_byte_budget() -> CliTraceTestResult {
    let tmp = TempDir::new()?;
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir)?;
    let trace_path = tmp.path().join("trace.jsonl");
    let max_bytes = 4096_u64;

    let output = noisy_robot_cmd()
        .env("CASS_TRACE_MAX_BYTES", max_bytes.to_string())
        .args([
            "status",
            "--json",
            "--data-dir",
            data_dir.to_str().ok_or("data directory is not UTF-8")?,
            "--trace-file",
            trace_path.to_str().ok_or("trace path is not UTF-8")?,
        ])
        .output()?;
    trace_test_require!(output.status.success(), "bounded status command failed");

    let metadata = std::fs::metadata(&trace_path)?;
    trace_test_require!(
        metadata.len() <= max_bytes,
        "trace exceeded explicit hard budget: {} > {max_bytes}",
        metadata.len()
    );
    let events = read_trace_events(&trace_path)?;
    trace_test_require!(
        command_summary(&events)?["fields"]["cmd"] == "status",
        "bounded trace summary recorded the wrong command"
    );
    Ok(())
}
