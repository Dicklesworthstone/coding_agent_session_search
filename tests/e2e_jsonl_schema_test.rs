//! Validates E2E JSONL output conforms to the expected schema.
//!
//! Complements the shell-based `scripts/validate-e2e-jsonl.sh` with deeper
//! Rust-side validation that runs as part of `cargo test`.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

mod util;
use util::e2e_log::{E2eError, E2ePerformanceMetrics, PhaseTracker};

type SchemaTestResult = Result<(), Box<dyn std::error::Error>>;

macro_rules! schema_test_require {
    ($condition:expr, $($message:tt)+) => {
        if !$condition {
            return Err(format_args!($($message)+).to_string().into());
        }
    };
}

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_jsonl_schema_test", test_name)
}

fn selected_e2e_root() -> Option<PathBuf> {
    std::env::var("CASS_E2E_RUN_ID")
        .ok()
        .map(|run_id| PathBuf::from("test-results/e2e/runs").join(run_id))
}

/// Required fields per event type.
/// Common fields (ts, event, run_id, runner) are checked separately.
const EVENT_SPECIFIC_FIELDS: &[(&str, &[&str])] = &[
    ("run_start", &["env"]),
    ("run_end", &["summary"]),
    ("test_start", &["test"]),
    ("test_end", &["test", "result"]),
    ("phase_start", &["phase"]),
    ("phase_end", &["phase", "duration_ms"]),
    ("metrics", &["name", "metrics"]),
    ("log", &["level", "msg"]),
];

const COMMON_FIELDS: &[&str] = &["ts", "event", "run_id", "runner"];
const TRACE_SCHEMA_VERSION: &str = "cass-trace-v1";
const E2E_TRACE_MAX_BYTES: u64 = 512 * 1024;
const SEMANTIC_TRACE_AGGREGATE_MAX_BYTES: u64 = 10 * 1024 * 1024;

fn is_log_file(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        // Trace files use the separate cass-trace-v1 schema below.
        if name == "trace.jsonl" || name == "combined.jsonl" {
            return false;
        }
        // Exclude cass.log files (application logs, not E2E schema)
        if name == "cass.log" {
            return false;
        }
        // Only validate E2E log files (shell_*.jsonl, rust_*.jsonl patterns)
        if name.ends_with(".jsonl") {
            return name.starts_with("shell_") || name.starts_with("rust_");
        }
    }

    false
}

fn collect_matching_files(root: &Path, predicate: fn(&Path) -> bool) -> Vec<PathBuf> {
    fn visit(dir: &Path, predicate: fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, predicate, out);
                } else if predicate(&path) {
                    out.push(path);
                }
            }
        }
    }

    let mut files = Vec::new();
    visit(root, predicate, &mut files);
    files.sort();
    files
}

fn collect_jsonl_logs(root: &Path) -> Vec<PathBuf> {
    collect_matching_files(root, is_log_file)
}

fn collect_trace_logs(root: &Path) -> Vec<PathBuf> {
    collect_matching_files(root, |path| {
        !path
            .components()
            .any(|component| component.as_os_str() == ".previous")
            && path.file_name().and_then(|name| name.to_str()) == Some("trace.jsonl")
    })
}

/// Validate a single JSONL event object.
fn validate_event(json: &Value) -> Result<(), String> {
    // Check common required fields
    for field in COMMON_FIELDS {
        if json.get(*field).is_none() {
            return Err(format!("Missing common field '{field}'"));
        }
    }

    let event = json["event"]
        .as_str()
        .ok_or("'event' field is not a string")?;

    // Validate event-specific fields
    if let Some((_, specific)) = EVENT_SPECIFIC_FIELDS.iter().find(|(e, _)| *e == event) {
        for field in *specific {
            if json.get(*field).is_none() {
                return Err(format!("Event '{event}' missing required field '{field}'"));
            }
        }
    }
    // Unknown event types are allowed (forward-compatible)

    // Validate timestamp is parseable
    if let Some(ts) = json["ts"].as_str() {
        chrono::DateTime::parse_from_rfc3339(ts)
            .map_err(|e| format!("Invalid timestamp '{ts}': {e}"))?;
    }

    // Validate nested object structure for known events
    match event {
        "test_start" if json["test"].get("name").and_then(|v| v.as_str()).is_none() => {
            return Err(format!("Event '{event}' missing 'test.name' string"));
        }
        "test_start" => {}
        "test_end" => {
            if json["test"].get("name").and_then(|v| v.as_str()).is_none() {
                return Err(format!("Event '{event}' missing 'test.name' string"));
            }
            if json["result"]
                .get("status")
                .and_then(|v| v.as_str())
                .is_none()
            {
                return Err("Event 'test_end' missing 'result.status' string".to_string());
            }
        }
        "phase_start" if json["phase"].get("name").and_then(|v| v.as_str()).is_none() => {
            return Err(format!("Event '{event}' missing 'phase.name' string"));
        }
        "phase_start" => {}
        "phase_end" => {
            if json["phase"].get("name").and_then(|v| v.as_str()).is_none() {
                return Err(format!("Event '{event}' missing 'phase.name' string"));
            }
            if !json["duration_ms"].is_number() {
                return Err("Event 'phase_end' 'duration_ms' is not a number".to_string());
            }
        }
        _ => {}
    }

    Ok(())
}

fn validate_trace_event(json: &Value) -> Result<(), String> {
    if !json.is_object() {
        return Err("Trace record is not a JSON object".to_string());
    }
    if json["schema_version"] != TRACE_SCHEMA_VERSION {
        return Err(format!(
            "Trace record schema_version must be '{TRACE_SCHEMA_VERSION}'"
        ));
    }
    let timestamp = json["timestamp"]
        .as_str()
        .ok_or("Trace record timestamp is not a string")?;
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map_err(|error| format!("Invalid trace timestamp '{timestamp}': {error}"))?;
    for field in ["level", "target"] {
        if json[field].as_str().is_none_or(|value| value.is_empty()) {
            return Err(format!("Trace record '{field}' must be a non-empty string"));
        }
    }
    for field in ["trace_id", "test_id"] {
        if json.get(field).is_none() {
            return Err(format!(
                "Trace record is missing correlation field '{field}'"
            ));
        }
    }
    let fields = json["fields"]
        .as_object()
        .ok_or("Trace record 'fields' is not an object")?;
    if fields
        .get("event")
        .or_else(|| fields.get("message"))
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err("Trace fields require a non-empty event or message".to_string());
    }
    Ok(())
}

/// Validate structural consistency within a single JSONL file.
fn validate_file_structure(events: &[Value]) -> Vec<String> {
    let mut warnings = Vec::new();

    let has_run_start = events.iter().any(|e| e["event"] == "run_start");
    let has_test_start = events.iter().any(|e| e["event"] == "test_start");

    if has_test_start && !has_run_start {
        warnings.push("Has test events but no run_start".to_string());
    }

    // Count matched test_start / test_end pairs
    let test_starts = events.iter().filter(|e| e["event"] == "test_start").count();
    let test_ends = events.iter().filter(|e| e["event"] == "test_end").count();
    if test_starts != test_ends {
        warnings.push(format!(
            "Mismatched test_start ({test_starts}) and test_end ({test_ends})"
        ));
    }

    warnings
}

#[test]
fn trace_schema_validator_rejects_unversioned_records() -> SchemaTestResult {
    let unversioned = serde_json::json!({
        "timestamp": "2026-01-01T00:00:00Z",
        "level": "INFO",
        "target": "cass::trace",
        "trace_id": "trace-1",
        "test_id": "suite/test",
        "fields": {"event": "command_summary"},
    });
    let error = match validate_trace_event(&unversioned) {
        Ok(()) => return Err("an unversioned trace record passed the schema gate".into()),
        Err(error) => error,
    };
    schema_test_require!(error.contains("schema_version"), "{error}");
    Ok(())
}

#[test]
fn trace_collection_excludes_archived_evidence() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::TempDir::new()?;
    let active = temp.path().join("suite/active/trace.jsonl");
    let archived = temp.path().join("suite/.previous/old-run/trace.jsonl");
    fs::create_dir_all(active.parent().ok_or("active trace has no parent")?)?;
    fs::create_dir_all(archived.parent().ok_or("archived trace has no parent")?)?;
    fs::write(&active, b"")?;
    fs::write(&archived, b"")?;

    let traces = collect_trace_logs(temp.path());
    if traces != vec![active] {
        return Err(format!("archive exclusion returned unexpected traces: {traces:?}").into());
    }
    Ok(())
}

#[test]
fn shell_validator_rejects_test_end_without_result_status() -> SchemaTestResult {
    let tracker = tracker_for("shell_validator_rejects_test_end_without_result_status");

    let temp = tempfile::TempDir::new()?;
    let log_path = temp.path().join("missing_status.jsonl");
    fs::write(
        &log_path,
        r#"{"ts":"2026-01-01T00:00:00Z","event":"run_start","run_id":"r1","runner":"rust","env":{}}
{"ts":"2026-01-01T00:00:01Z","event":"test_start","run_id":"r1","runner":"rust","test":{"name":"status_required"}}
{"ts":"2026-01-01T00:00:02Z","event":"test_end","run_id":"r1","runner":"rust","test":{"name":"status_required"}}
{"ts":"2026-01-01T00:00:03Z","event":"run_end","run_id":"r1","runner":"rust","summary":{}}
"#,
    )?;

    let output = Command::new("bash")
        .arg("scripts/validate-e2e-jsonl.sh")
        .arg(&log_path)
        .output()?;

    assert!(
        !output.status.success(),
        "shell validator accepted a test_end event without result.status\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("test_end missing 'result.status' field"),
        "shell validator failed without the expected result.status diagnostic:\n{combined}"
    );

    tracker.complete();
    Ok(())
}

#[test]
fn shell_validator_rejects_unversioned_trace_jsonl() -> SchemaTestResult {
    let tracker = tracker_for("shell_validator_rejects_unversioned_trace_jsonl");

    let temp = tempfile::TempDir::new()?;
    let trace_path = temp.path().join("trace.jsonl");
    fs::write(
        &trace_path,
        r#"{"timestamp":"2026-01-01T00:00:00Z","level":"INFO","target":"cass::trace","trace_id":"trace-1","test_id":"suite/test","fields":{"event":"command_summary","duration_ms":1,"exit_code":0}}
"#,
    )?;

    let output = Command::new("bash")
        .arg("scripts/validate-e2e-jsonl.sh")
        .arg(&trace_path)
        .output()?;
    schema_test_require!(
        !output.status.success(),
        "shell validator accepted an unversioned trace artifact"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    schema_test_require!(
        combined.contains("Invalid cass-trace-v1 JSONL envelope"),
        "shell validator failed without the versioned-trace diagnostic:\n{combined}"
    );

    tracker.complete();
    Ok(())
}

#[test]
fn shell_validator_rejects_trace_without_command_outcome() -> SchemaTestResult {
    let tracker = tracker_for("shell_validator_rejects_trace_without_command_outcome");

    let temp = tempfile::TempDir::new()?;
    let trace_path = temp.path().join("trace.jsonl");
    fs::write(
        &trace_path,
        r#"{"schema_version":"cass-trace-v1","timestamp":"2026-01-01T00:00:00Z","level":"DEBUG","target":"cass::semantic","trace_id":"trace-1","test_id":"suite/test","fields":{"event":"diagnostic","phase":"search"}}
"#,
    )?;

    let output = Command::new("bash")
        .arg("scripts/validate-e2e-jsonl.sh")
        .arg(&trace_path)
        .output()?;
    schema_test_require!(
        !output.status.success(),
        "shell validator accepted a trace without its command outcome"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    schema_test_require!(
        combined.contains("trace has no command_summary outcome record"),
        "shell validator failed without the missing-outcome diagnostic:\n{combined}"
    );

    tracker.complete();
    Ok(())
}

#[test]
fn shell_validator_enforces_the_semantic_trace_aggregate_budget() -> SchemaTestResult {
    let tracker = tracker_for("shell_validator_enforces_the_semantic_trace_aggregate_budget");

    let temp = tempfile::TempDir::new()?;
    let semantic_root = temp.path().join("test-results/e2e/e2e_semantic_search");
    fs::create_dir_all(&semantic_root)?;
    let payload = "x".repeat(480_000);
    let record = serde_json::json!({
        "schema_version": "cass-trace-v1",
        "timestamp": "2026-01-01T00:00:00Z",
        "level": "INFO",
        "target": "cass::trace",
        "trace_id": "trace-1",
        "test_id": "e2e_semantic_search/aggregate-budget",
        "fields": {
            "event": "command_summary",
            "duration_ms": 1,
            "exit_code": 0,
            "padding": payload,
        },
    })
    .to_string();
    let mut trace_paths = Vec::new();
    for index in 0..22 {
        let case_dir = semantic_root.join(format!("case-{index:02}"));
        fs::create_dir_all(&case_dir)?;
        let trace_path = case_dir.join("trace.jsonl");
        fs::write(&trace_path, format!("{record}\n"))?;
        trace_paths.push(trace_path);
    }

    let output = Command::new("bash")
        .arg("scripts/validate-e2e-jsonl.sh")
        .args(&trace_paths)
        .output()?;
    schema_test_require!(
        !output.status.success(),
        "shell validator accepted an over-budget semantic trace aggregate"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    schema_test_require!(
        combined.contains("semantic trace aggregate")
            && combined.contains("exceeding the 10485760-byte campaign gate"),
        "shell validator failed without the aggregate-budget diagnostic:\n{combined}"
    );

    tracker.complete();
    Ok(())
}

#[test]
fn shell_validator_default_discovery_is_scoped_to_one_explicit_run() -> SchemaTestResult {
    let tracker = tracker_for("shell_validator_default_discovery_is_scoped_to_one_explicit_run");

    let repo_root = std::env::current_dir()?;
    let validator = repo_root.join("scripts/validate-e2e-jsonl.sh");
    let temp = tempfile::TempDir::new()?;
    let run_id = "selected-run-20260728";
    let current_dir = temp.path().join("test-results/e2e/runs").join(run_id);
    let unrelated_dir = temp
        .path()
        .join("test-results/e2e/runs/unrelated-run-20260728");
    fs::create_dir_all(&current_dir)?;
    fs::create_dir_all(&unrelated_dir)?;

    fs::write(
        current_dir.join("current.jsonl"),
        r#"{"ts":"2026-01-01T00:00:00Z","event":"run_start","run_id":"current","runner":"rust","env":{}}
{"ts":"2026-01-01T00:00:01Z","event":"test_start","run_id":"current","runner":"rust","test":{"name":"current"}}
{"ts":"2026-01-01T00:00:02Z","event":"test_end","run_id":"current","runner":"rust","test":{"name":"current"},"result":{"status":"pass"}}
{"ts":"2026-01-01T00:00:03Z","event":"run_end","run_id":"current","runner":"rust","summary":{}}
"#,
    )?;
    fs::write(
        unrelated_dir.join("stale.jsonl"),
        r#"{"ts":"2026-01-01T00:00:00Z","event":"run_start","run_id":"stale","runner":"rust","env":{}}
{"ts":"2026-01-01T00:00:01Z","event":"test_start","run_id":"stale","runner":"rust","test":{"name":"stale"}}
{"ts":"2026-01-01T00:00:02Z","event":"test_end","run_id":"stale","runner":"rust","test":{"name":"stale"}}
{"ts":"2026-01-01T00:00:03Z","event":"run_end","run_id":"stale","runner":"rust","summary":{}}
"#,
    )?;

    let output = Command::new("bash")
        .arg(&validator)
        .env("CASS_E2E_RUN_ID", run_id)
        .current_dir(temp.path())
        .output()?;

    assert!(
        output.status.success(),
        "shell validator did not isolate the selected run\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("unrelated-run"),
        "shell validator consumed another run's evidence:\n{stdout}"
    );

    tracker.complete();
    Ok(())
}

#[test]
fn shell_validator_refuses_implicit_process_global_discovery() -> SchemaTestResult {
    let tracker = tracker_for("shell_validator_refuses_implicit_process_global_discovery");
    let repo_root = std::env::current_dir()?;
    let validator = repo_root.join("scripts/validate-e2e-jsonl.sh");
    let temp = tempfile::TempDir::new()?;
    fs::create_dir_all(temp.path().join("test-results/e2e"))?;
    let output = Command::new("bash")
        .arg(&validator)
        .env_remove("CASS_E2E_RUN_ID")
        .current_dir(temp.path())
        .output()?;
    schema_test_require!(
        !output.status.success(),
        "validator accepted process-global no-argument discovery"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    schema_test_require!(
        combined.contains("requires a valid CASS_E2E_RUN_ID"),
        "missing fail-closed diagnostic:\n{combined}"
    );
    tracker.complete();
    Ok(())
}

#[test]
fn jsonl_files_valid_schema() {
    let tracker = tracker_for("jsonl_files_valid_schema");

    let Some(e2e_dir) = selected_e2e_root() else {
        eprintln!("No CASS_E2E_RUN_ID — skipping run-artifact validation");
        tracker.complete();
        return;
    };
    if !e2e_dir.exists() {
        eprintln!("No test-results/e2e directory — skipping JSONL validation");
        tracker.complete();
        return;
    }

    let phase_start = tracker.start("discover_files", Some("Find JSONL files"));
    let active_log = tracker.artifacts().cass_log_path.clone();
    let jsonl_files = collect_jsonl_logs(&e2e_dir)
        .into_iter()
        .filter(|path| path != &active_log)
        .collect::<Vec<_>>();
    tracker.end("discover_files", Some("Find JSONL files"), phase_start);

    if jsonl_files.is_empty() {
        eprintln!("No JSONL files in test-results/e2e/ — skipping");
        tracker.complete();
        return;
    }

    let phase_start = tracker.start("validate_events", Some("Validate event schema"));
    let mut total_events = 0usize;
    let mut errors: Vec<String> = Vec::new();
    let mut event_type_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for path in &jsonl_files {
        let content = fs::read_to_string(path).unwrap();
        let mut file_events = Vec::new();

        for (line_num, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            total_events += 1;

            match serde_json::from_str::<Value>(line) {
                Ok(json) => {
                    if let Some(evt) = json["event"].as_str() {
                        *event_type_counts.entry(evt.to_string()).or_default() += 1;
                    }
                    if let Err(e) = validate_event(&json) {
                        errors.push(format!("{}:{}: {e}", path.display(), line_num + 1));
                    }
                    file_events.push(json);
                }
                Err(e) => {
                    errors.push(format!(
                        "{}:{}: Invalid JSON: {e}",
                        path.display(),
                        line_num + 1
                    ));
                }
            }
        }

        // Structural validation per file
        for warning in validate_file_structure(&file_events) {
            errors.push(format!("{}: {warning}", path.display()));
        }
    }
    tracker.end(
        "validate_events",
        Some("Validate event schema"),
        phase_start,
    );

    tracker.metrics(
        "jsonl_validation",
        &E2ePerformanceMetrics::new()
            .with_custom("files_checked", serde_json::json!(jsonl_files.len()))
            .with_custom("total_events", serde_json::json!(total_events))
            .with_custom("error_count", serde_json::json!(errors.len()))
            .with_custom("event_types", serde_json::json!(event_type_counts)),
    );

    if !errors.is_empty() {
        tracker.fail(E2eError::new(format!("{} schema errors", errors.len())));
        assert!(
            errors.is_empty(),
            "JSONL schema validation failed ({} errors in {} files, {} events):\n{}",
            errors.len(),
            jsonl_files.len(),
            total_events,
            errors.join("\n")
        );
        return;
    }

    eprintln!(
        "Validated {total_events} events across {} JSONL files",
        jsonl_files.len()
    );
    tracker.complete();
}

#[test]
fn trace_jsonl_files_are_valid_correlated_and_bounded() -> SchemaTestResult {
    let tracker = tracker_for("trace_jsonl_files_are_valid_correlated_and_bounded");

    let Some(e2e_dir) = selected_e2e_root() else {
        eprintln!("No CASS_E2E_RUN_ID — skipping run-trace validation");
        tracker.complete();
        return Ok(());
    };
    if !e2e_dir.exists() {
        tracker.complete();
        return Ok(());
    }

    let trace_files = collect_trace_logs(&e2e_dir);
    if trace_files.is_empty() {
        eprintln!("No per-test trace.jsonl artifacts — skipping trace validation");
        tracker.complete();
        return Ok(());
    }

    let semantic_root = e2e_dir.join("e2e_semantic_search");
    let mut semantic_bytes = 0_u64;
    let mut nonempty_files = 0_u64;
    let mut total_events = 0_u64;
    let mut truncation_receipts = 0_u64;
    let mut filter_receipts = 0_u64;
    let mut errors = Vec::new();

    for path in &trace_files {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                errors.push(format!("{}: metadata failed: {error}", path.display()));
                continue;
            }
        };
        if metadata.len() > E2E_TRACE_MAX_BYTES {
            errors.push(format!(
                "{}: {} bytes exceeds the declared per-test {}-byte budget",
                path.display(),
                metadata.len(),
                E2E_TRACE_MAX_BYTES
            ));
        }
        if path.starts_with(&semantic_root) {
            semantic_bytes = semantic_bytes.saturating_add(metadata.len());
        }
        if metadata.len() == 0 {
            continue;
        }
        nonempty_files = nonempty_files.saturating_add(1);

        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) => {
                errors.push(format!("{}: read failed: {error}", path.display()));
                continue;
            }
        };
        if content.contains("/home/") || content.contains("/data/projects/") {
            errors.push(format!(
                "{}: trace contains an unredacted private path",
                path.display()
            ));
        }

        let mut command_summaries = 0_u64;
        for (line_index, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            total_events = total_events.saturating_add(1);
            let event = match serde_json::from_str::<Value>(line) {
                Ok(event) => event,
                Err(error) => {
                    errors.push(format!(
                        "{}:{}: invalid JSON: {error}",
                        path.display(),
                        line_index + 1
                    ));
                    continue;
                }
            };
            if let Err(error) = validate_trace_event(&event) {
                errors.push(format!("{}:{}: {error}", path.display(), line_index + 1));
                continue;
            }
            if event["trace_id"]
                .as_str()
                .is_none_or(|value| value.is_empty())
                || event["test_id"]
                    .as_str()
                    .is_none_or(|value| value.is_empty())
            {
                errors.push(format!(
                    "{}:{}: E2E trace correlation IDs must be non-empty strings",
                    path.display(),
                    line_index + 1
                ));
            }

            match event["fields"]["event"].as_str() {
                Some("trace_truncated") => {
                    truncation_receipts = truncation_receipts.saturating_add(1);
                    let suppressed_events = event["fields"]["suppressed_events"].as_u64();
                    let suppression_reason_total =
                        ["byte_budget", "event_budget", "oversize_event"]
                            .into_iter()
                            .try_fold(0_u64, |total, reason| {
                                event["fields"]["suppression_reasons"][reason]
                                    .as_u64()
                                    .map(|count| total.saturating_add(count))
                            });
                    let failure_tail_fields_are_valid = [
                        "failure_tail_events",
                        "failure_tail_bytes",
                        "failure_tail_dropped_events",
                    ]
                    .into_iter()
                    .all(|field| event["fields"][field].as_u64().is_some());
                    if event["fields"]["artifact_complete"] != Value::Bool(false)
                        || event["fields"]["reason"].as_str().is_none_or(str::is_empty)
                        || suppressed_events.is_none_or(|count| count == 0)
                        || suppression_reason_total != suppressed_events
                        || !failure_tail_fields_are_valid
                    {
                        errors.push(format!(
                            "{}:{}: invalid truncation receipt",
                            path.display(),
                            line_index + 1
                        ));
                    }
                }
                Some("trace_filter_summary") => {
                    filter_receipts = filter_receipts.saturating_add(1);
                    if event["fields"]["artifact_complete"] != Value::Bool(true)
                        || event["fields"]["filtered_events"]
                            .as_u64()
                            .is_none_or(|count| count == 0)
                    {
                        errors.push(format!(
                            "{}:{}: invalid filter receipt",
                            path.display(),
                            line_index + 1
                        ));
                    }
                }
                Some("command_summary") => {
                    command_summaries = command_summaries.saturating_add(1);
                    if event["fields"]["duration_ms"].as_u64().is_none()
                        || event["fields"]["exit_code"].as_i64().is_none()
                    {
                        errors.push(format!(
                            "{}:{}: command summary lacks duration or outcome",
                            path.display(),
                            line_index + 1
                        ));
                    }
                }
                _ => {}
            }
        }

        if command_summaries == 0 {
            errors.push(format!(
                "{}: trace has no command_summary outcome record",
                path.display()
            ));
        }
    }

    if semantic_bytes > SEMANTIC_TRACE_AGGREGATE_MAX_BYTES {
        errors.push(format!(
            "semantic trace aggregate is {semantic_bytes} bytes, exceeding the {}-byte campaign gate",
            SEMANTIC_TRACE_AGGREGATE_MAX_BYTES
        ));
    }

    tracker.metrics(
        "trace_jsonl_validation",
        &E2ePerformanceMetrics::new()
            .with_custom("files_checked", serde_json::json!(trace_files.len()))
            .with_custom("nonempty_files", serde_json::json!(nonempty_files))
            .with_custom("events_checked", serde_json::json!(total_events))
            .with_custom("semantic_bytes", serde_json::json!(semantic_bytes))
            .with_custom(
                "truncation_receipts",
                serde_json::json!(truncation_receipts),
            )
            .with_custom("filter_receipts", serde_json::json!(filter_receipts))
            .with_custom("error_count", serde_json::json!(errors.len())),
    );

    schema_test_require!(
        errors.is_empty(),
        "Trace JSONL gate failed ({} errors across {} files):\n{}",
        errors.len(),
        trace_files.len(),
        errors.join("\n")
    );
    tracker.complete();
    Ok(())
}

#[test]
fn e2e_subprocess_sources_cannot_mutate_the_parent_environment() -> SchemaTestResult {
    let tracker = tracker_for("e2e_subprocess_sources_cannot_mutate_the_parent_environment");
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let sources = [
        "tests/e2e_cli_flows.rs",
        "tests/e2e_deploy.rs",
        "tests/e2e_filters.rs",
        "tests/e2e_index_tui.rs",
        "tests/e2e_install_easy.rs",
        "tests/e2e_jsonl_schema_test.rs",
        "tests/e2e_large_dataset.rs",
        "tests/e2e_multi_connector.rs",
        "tests/e2e_pages.rs",
        "tests/e2e_search_index.rs",
        "tests/e2e_semantic_search.rs",
        "tests/e2e_sources.rs",
        "tests/e2e_ssh_sources.rs",
        "tests/e2e_tui_smoke_flows.rs",
        "tests/pages_preview_integration.rs",
        "tests/util/e2e_log.rs",
    ];
    let forbidden = [
        ["trace_env", "_guard"].concat(),
        ["EnvGuard", "::set("].concat(),
        ["std::env::", "set_var"].concat(),
        ["std::env::", "remove_var"].concat(),
    ];
    let mut violations = Vec::new();

    for relative in sources {
        let path = manifest_dir.join(relative);
        let content = fs::read_to_string(&path)?;
        let policy_surface = if relative == "tests/util/e2e_log.rs" {
            content
                .split_once("#[cfg(test)]")
                .map_or(content.as_str(), |(production, _)| production)
        } else {
            content.as_str()
        };
        for needle in &forbidden {
            if policy_surface.contains(needle) {
                violations.push(format!(
                    "{relative}: forbidden process-global mutation `{needle}`"
                ));
            }
        }
    }

    schema_test_require!(
        violations.is_empty(),
        "E2E subprocess environment policy regressed:\n{}",
        violations.join("\n")
    );
    tracker.metrics(
        "process_environment_policy",
        &E2ePerformanceMetrics::new()
            .with_custom("files_checked", serde_json::json!(sources.len()))
            .with_custom("forbidden_patterns", serde_json::json!(forbidden.len())),
    );
    tracker.complete();
    Ok(())
}

#[test]
fn concurrent_children_keep_home_codex_home_and_trace_artifacts_disjoint() -> SchemaTestResult {
    #[derive(Debug)]
    struct Witness {
        trace_id: String,
        home: String,
        codex_home: String,
        trace_file: String,
        witness_path: PathBuf,
    }

    fn run_witness(root: PathBuf, name: &'static str) -> Result<Witness, String> {
        let tracker = tracker_for(name);
        let home = root.join(name).join("home");
        let codex_home = root.join(name).join("codex");
        fs::create_dir_all(&home).map_err(|error| format!("create HOME: {error}"))?;
        fs::create_dir_all(&codex_home).map_err(|error| format!("create CODEX_HOME: {error}"))?;
        let witness_path = tracker.artifacts().dir.join("child-environment.txt");
        let command_env = tracker
            .command_environment()
            .with_home(&home)
            .with_codex_home(&codex_home);

        let version = command_env
            .cass_assert_command()
            .arg("--version")
            .output()
            .map_err(|error| format!("run cass --version: {error}"))?;
        if !version.status.success() {
            return Err(format!(
                "cass --version failed: {}",
                String::from_utf8_lossy(&version.stderr)
            ));
        }

        let output = command_env
            .sh_command()
            .arg("-c")
            .arg(
                "printf '%s\\n%s\\n%s\\n%s\\n' \
                 \"$CASS_TRACE_ID\" \"$HOME\" \"$CODEX_HOME\" \"$CASS_TRACE_FILE\" \
                 > \"$CASS_ENV_WITNESS\"",
            )
            .env("CASS_ENV_WITNESS", &witness_path)
            .output()
            .map_err(|error| format!("run child environment witness: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "environment witness failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let fields = fs::read_to_string(&witness_path)
            .map_err(|error| format!("read environment witness: {error}"))?
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if fields.len() != 4 {
            return Err(format!("expected four witness fields, got {fields:?}"));
        }
        let witness = Witness {
            trace_id: fields[0].clone(),
            home: fields[1].clone(),
            codex_home: fields[2].clone(),
            trace_file: fields[3].clone(),
            witness_path,
        };
        tracker.complete();
        Ok(witness)
    }

    let tracker =
        tracker_for("concurrent_children_keep_home_codex_home_and_trace_artifacts_disjoint");
    let tracked_keys = ["HOME", "CODEX_HOME", "CASS_TRACE_FILE", "CASS_TRACE_ID"];
    let process_before = tracked_keys.map(std::env::var_os);
    let tmp = tempfile::TempDir::new()?;
    let left_root = tmp.path().to_path_buf();
    let right_root = tmp.path().to_path_buf();
    let left = std::thread::spawn(move || run_witness(left_root, "parallel_environment_left"));
    let right = std::thread::spawn(move || run_witness(right_root, "parallel_environment_right"));
    let left = left
        .join()
        .map_err(|_| "left environment witness panicked")??;
    let right = right
        .join()
        .map_err(|_| "right environment witness panicked")??;

    schema_test_require!(left.trace_id != right.trace_id, "trace IDs must be unique");
    schema_test_require!(left.home != right.home, "HOME values must be isolated");
    schema_test_require!(
        left.codex_home != right.codex_home,
        "CODEX_HOME values must be isolated"
    );
    schema_test_require!(
        left.trace_file != right.trace_file,
        "trace artifact paths must be isolated"
    );
    schema_test_require!(
        left.witness_path.exists() && right.witness_path.exists(),
        "both child-specific witness artifacts must exist"
    );
    schema_test_require!(
        tracked_keys.map(std::env::var_os) == process_before,
        "child environment application must not mutate the parent process"
    );

    tracker.metrics(
        "parallel_environment_isolation",
        &E2ePerformanceMetrics::new()
            .with_custom("child_count", 2)
            .with_custom("distinct_trace_ids", 2)
            .with_custom("distinct_trace_files", 2),
    );
    tracker.complete();
    Ok(())
}

#[test]
fn jsonl_timestamps_are_rfc3339() {
    let tracker = tracker_for("jsonl_timestamps_are_rfc3339");

    let Some(e2e_dir) = selected_e2e_root() else {
        tracker.complete();
        return;
    };
    if !e2e_dir.exists() {
        tracker.complete();
        return;
    }

    let phase_start = tracker.start("check_timestamps", Some("Validate all timestamps"));
    let mut checked = 0usize;
    let mut bad = Vec::new();

    for path in collect_jsonl_logs(&e2e_dir) {
        let content = fs::read_to_string(&path).unwrap();
        for (line_num, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(json) = serde_json::from_str::<Value>(line)
                && let Some(ts) = json["ts"].as_str()
            {
                checked += 1;
                if chrono::DateTime::parse_from_rfc3339(ts).is_err() {
                    bad.push(format!("{}:{}: {ts}", path.display(), line_num + 1));
                }
            }
        }
    }
    tracker.end(
        "check_timestamps",
        Some("Validate all timestamps"),
        phase_start,
    );

    tracker.metrics(
        "timestamp_validation",
        &E2ePerformanceMetrics::new()
            .with_custom("timestamps_checked", serde_json::json!(checked))
            .with_custom("invalid_count", serde_json::json!(bad.len())),
    );

    assert!(
        bad.is_empty(),
        "Found {} invalid RFC3339 timestamps:\n{}",
        bad.len(),
        bad.join("\n")
    );

    eprintln!("All {checked} timestamps are valid RFC3339");
    tracker.complete();
}

#[test]
fn jsonl_run_ids_consistent_within_file() -> SchemaTestResult {
    let tracker = tracker_for("jsonl_run_ids_consistent_within_file");

    let Some(e2e_dir) = selected_e2e_root() else {
        tracker.complete();
        return Ok(());
    };
    let expected_run_id = std::env::var("CASS_E2E_RUN_ID")?;
    if !e2e_dir.exists() {
        tracker.complete();
        return Ok(());
    }

    let phase_start = tracker.start("check_run_ids", Some("Check run_id consistency"));
    let mut errors = Vec::new();

    for path in collect_jsonl_logs(&e2e_dir) {
        let content = fs::read_to_string(&path).unwrap();
        let mut run_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(json) = serde_json::from_str::<Value>(line)
                && let Some(run_id) = json["run_id"].as_str()
            {
                run_ids.insert(run_id.to_string());
            }
        }

        if run_ids.len() != 1 || !run_ids.contains(&expected_run_id) {
            errors.push(format!(
                "{}: expected only run_id {:?}, found {:?}",
                path.display(),
                expected_run_id,
                run_ids
            ));
        }
    }
    tracker.end(
        "check_run_ids",
        Some("Check run_id consistency"),
        phase_start,
    );

    // A selected immutable run must never mix run IDs within one file.
    schema_test_require!(
        errors.is_empty(),
        "{} files do not carry the selected immutable run ID:\n{}",
        errors.len(),
        errors.join("\n")
    );

    tracker.complete();
    Ok(())
}
