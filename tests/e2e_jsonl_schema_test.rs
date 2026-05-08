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

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_jsonl_schema_test", test_name)
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

fn is_log_file(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        // Exclude trace/combined files (internal aggregates)
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

fn collect_jsonl_logs(root: &Path) -> Vec<PathBuf> {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, out);
                } else if is_log_file(&path) {
                    out.push(path);
                }
            }
        }
    }

    let mut logs = Vec::new();
    visit(root, &mut logs);
    logs.sort();
    logs
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
fn shell_validator_rejects_test_end_without_result_status() {
    let tracker = tracker_for("shell_validator_rejects_test_end_without_result_status");
    let _trace_guard = tracker.trace_env_guard();

    let temp = tempfile::TempDir::new().expect("tempdir");
    let log_path = temp.path().join("missing_status.jsonl");
    fs::write(
        &log_path,
        r#"{"ts":"2026-01-01T00:00:00Z","event":"run_start","run_id":"r1","runner":"rust","env":{}}
{"ts":"2026-01-01T00:00:01Z","event":"test_start","run_id":"r1","runner":"rust","test":{"name":"status_required"}}
{"ts":"2026-01-01T00:00:02Z","event":"test_end","run_id":"r1","runner":"rust","test":{"name":"status_required"}}
{"ts":"2026-01-01T00:00:03Z","event":"run_end","run_id":"r1","runner":"rust","summary":{}}
"#,
    )
    .expect("write malformed jsonl fixture");

    let output = Command::new("bash")
        .arg("scripts/validate-e2e-jsonl.sh")
        .arg(&log_path)
        .output()
        .expect("run shell JSONL validator");

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
}

#[test]
fn jsonl_files_valid_schema() {
    let tracker = tracker_for("jsonl_files_valid_schema");
    let _trace_guard = tracker.trace_env_guard();

    let e2e_dir = Path::new("test-results/e2e");
    if !e2e_dir.exists() {
        eprintln!("No test-results/e2e directory — skipping JSONL validation");
        tracker.complete();
        return;
    }

    let phase_start = tracker.start("discover_files", Some("Find JSONL files"));
    let jsonl_files = collect_jsonl_logs(e2e_dir);
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
fn jsonl_timestamps_are_rfc3339() {
    let tracker = tracker_for("jsonl_timestamps_are_rfc3339");
    let _trace_guard = tracker.trace_env_guard();

    let e2e_dir = Path::new("test-results/e2e");
    if !e2e_dir.exists() {
        tracker.complete();
        return;
    }

    let phase_start = tracker.start("check_timestamps", Some("Validate all timestamps"));
    let mut checked = 0usize;
    let mut bad = Vec::new();

    for path in collect_jsonl_logs(e2e_dir) {
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
fn jsonl_run_ids_consistent_within_file() {
    let tracker = tracker_for("jsonl_run_ids_consistent_within_file");
    let _trace_guard = tracker.trace_env_guard();

    let e2e_dir = Path::new("test-results/e2e");
    if !e2e_dir.exists() {
        tracker.complete();
        return;
    }

    let phase_start = tracker.start("check_run_ids", Some("Check run_id consistency"));
    let mut errors = Vec::new();

    for path in collect_jsonl_logs(e2e_dir) {
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

        // A single file should have at most one run_id (one run per file)
        if run_ids.len() > 1 {
            errors.push(format!(
                "{}: Multiple run_ids found: {:?}",
                path.display(),
                run_ids
            ));
        }
    }
    tracker.end(
        "check_run_ids",
        Some("Check run_id consistency"),
        phase_start,
    );

    // Multiple run_ids per file is a warning, not necessarily an error.
    // Some files may accumulate from multiple runs.
    if !errors.is_empty() {
        eprintln!(
            "Warning: {} files have multiple run_ids (may be accumulated):\n{}",
            errors.len(),
            errors.join("\n")
        );
    }

    tracker.complete();
}
