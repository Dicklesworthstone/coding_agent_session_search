//! Metamorphic contract check for `cass introspect --json`.
//!
//! `coding_agent_session_search-eq69o`: `response_schemas` is a
//! hand-written schema registry. Golden tests pin the registry and
//! several runtime payloads independently, but they did not prove the
//! registry still describes the JSON emitted by the corresponding
//! runtime commands. This test closes that gap by deriving a lightweight
//! shape from live command output and comparing it to the advertised
//! introspection schema.

use assert_cmd::Command;
use serde_json::{Map, Value, json};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

#[allow(deprecated)]
fn cass_cmd(test_home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("cass").expect("cass binary");
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home)
        .env("XDG_CONFIG_HOME", test_home.join(".config"))
        .env("HOME", test_home)
        .env("CODEX_HOME", test_home.join(".codex"))
        .env("CLAUDE_HOME", test_home.join(".claude"))
        .env("GEMINI_HOME", test_home.join(".gemini"))
        .env("OPENCODE_STORAGE_ROOT", test_home.join(".opencode"))
        .env("CASS_AIDER_DATA_ROOT", test_home.join(".aider-missing"))
        .env("PI_SESSIONS_DIR", test_home.join(".pi-sessions-missing"))
        .env("PI_CODING_AGENT_DIR", test_home.join(".pi-agent-missing"))
        .env(
            "PI_CODING_AGENT_SESSION_DIR",
            test_home.join(".pi-coding-agent-sessions-missing"),
        )
        .env_remove("PI_CONFIG_DIR")
        .env_remove("PI_PROFILE")
        .env("CASS_AUTO_REFRESH", "0")
        .current_dir(test_home)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd
}

fn fixture_path(parts: &[&str]) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for part in parts {
        path.push(part);
    }
    path
}

fn isolated_search_demo_data(test_home: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let dst_root = test_home.join("search_demo_data");
    let sessions = test_home.join(".codex/sessions/2025/11/25");
    fs::create_dir_all(&sessions)?;
    fs::copy(
        fixture_path(&[
            "codex_real",
            "sessions",
            "2025",
            "11",
            "25",
            "rollout-test.jsonl",
        ]),
        sessions.join("rollout-test.jsonl"),
    )?;
    // Exercise live response shapes from a current archive and publication.
    // The frozen search_demo_data database is a legacy migration fixture with
    // duplicate FTS schema rows; full indexing correctly refuses to replace it.
    cass_cmd(test_home)
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&dst_root)
        .assert()
        .success();
    // Exercise the same publication path that build-hnsw consumes, including
    // its semantic manifest. Legacy vector files alone are not a publication.
    cass_cmd(test_home)
        .args(["models", "backfill", "--tier", "fast", "--embedder", "hash"])
        .args(["--json", "--data-dir"])
        .arg(&dst_root)
        .assert()
        .success();
    Ok(dst_root)
}

fn json_value_schema(value: &Value) -> Value {
    match value {
        Value::Null => json!({ "type": "null" }),
        Value::Bool(_) => json!({ "type": "boolean" }),
        Value::Number(number) => {
            if number.is_f64() {
                json!({ "type": "number" })
            } else {
                json!({ "type": "integer" })
            }
        }
        Value::String(_) => json!({ "type": "string" }),
        Value::Array(values) => {
            let items = values
                .first()
                .map(json_value_schema)
                .unwrap_or_else(|| json!({ "type": "unknown" }));
            json!({
                "type": "array",
                "items": items
            })
        }
        Value::Object(map) => {
            let properties = map
                .iter()
                .map(|(key, value)| (key.clone(), json_value_schema(value)))
                .collect::<Map<String, Value>>();
            json!({
                "type": "object",
                "properties": properties
            })
        }
    }
}

#[derive(Clone, Copy)]
enum ExpectStatus {
    ExitOk,
    ExitAny,
}

fn run_json(test_home: &Path, args: &[String], expect_status: ExpectStatus) -> Value {
    let output = cass_cmd(test_home)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("run cass {args:?}: {err}"));
    if matches!(expect_status, ExpectStatus::ExitOk) {
        assert!(
            output.status.success(),
            "cass {args:?} exited non-zero: status={:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "cass {args:?} stdout is not JSON: {err}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    })
}

fn advertised_types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn schema_allows_type(schema: &Value, actual_type: &str) -> bool {
    let advertised = advertised_types(schema);
    advertised.contains(&actual_type)
        || (actual_type == "integer" && advertised.contains(&"number"))
        || advertised.contains(&"unknown")
}

fn schema_allows_dynamic_properties(schema: &Value) -> bool {
    match schema.get("additionalProperties") {
        Some(Value::Bool(value)) => *value,
        Some(Value::Object(_)) => true,
        _ => false,
    }
}

fn collect_runtime_shape_gaps(
    surface: &str,
    path: &str,
    runtime: &Value,
    advertised: &Value,
    gaps: &mut Vec<String>,
) {
    let runtime_type = runtime
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if !schema_allows_type(advertised, runtime_type) {
        gaps.push(format!(
            "{surface}{path}: runtime type {runtime_type:?} is not allowed by introspect schema {advertised}"
        ));
        return;
    }

    match runtime_type {
        "object" => {
            let Some(runtime_props) = runtime.get("properties").and_then(Value::as_object) else {
                return;
            };
            let advertised_props = advertised.get("properties").and_then(Value::as_object);
            if advertised_props.is_none() && schema_allows_dynamic_properties(advertised) {
                return;
            }
            let Some(advertised_props) = advertised_props else {
                gaps.push(format!(
                    "{surface}{path}: runtime object has properties but introspect schema has none"
                ));
                return;
            };
            for (key, runtime_child) in runtime_props {
                let child_path = format!("{path}.{key}");
                if let Some(advertised_child) = advertised_props.get(key) {
                    collect_runtime_shape_gaps(
                        surface,
                        &child_path,
                        runtime_child,
                        advertised_child,
                        gaps,
                    );
                } else {
                    gaps.push(format!(
                        "{surface}{child_path}: runtime field is missing from introspect schema"
                    ));
                }
            }
        }
        "array" => {
            let Some(runtime_items) = runtime.get("items") else {
                return;
            };
            if runtime_items
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == "unknown")
            {
                return;
            }
            if let Some(advertised_items) = advertised.get("items") {
                collect_runtime_shape_gaps(
                    surface,
                    &format!("{path}[]"),
                    runtime_items,
                    advertised_items,
                    gaps,
                );
            } else {
                gaps.push(format!("{surface}{path}: array schema missing items"));
            }
        }
        _ => {}
    }
}

fn surface_command(
    surface: &str,
    test_home: &Path,
    demo_data: &Path,
) -> Option<(Vec<String>, ExpectStatus)> {
    let demo_data = demo_data.to_str().expect("utf8 demo data");
    let session = fixture_path(&["html_export", "real_sessions", "claude_code_auth_fix.jsonl"]);
    let session = session.to_str().expect("utf8 session path");
    let empty_data_dir = test_home.join(format!("{surface}-data"));
    let empty_data_dir = empty_data_dir.to_str().expect("utf8 data dir");

    let args = match surface {
        "analytics-incidents" => {
            return Some((
                vec![
                    "analytics".to_string(),
                    "incidents".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    demo_data.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "api-version" => vec!["api-version", "--json"],
        "capabilities" => vec!["capabilities", "--json"],
        "diag" => vec!["diag", "--json"],
        "doctor" => vec!["doctor", "--json"],
        "health" => {
            return Some((
                vec![
                    "health".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    empty_data_dir.to_string(),
                ],
                ExpectStatus::ExitAny,
            ));
        }
        "index" => {
            return Some((
                vec![
                    "index".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    empty_data_dir.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "introspect" => vec!["introspect", "--json"],
        "selftest" => vec!["selftest", "--json"],
        "models-build-hnsw" => {
            return Some((
                vec![
                    "models".to_string(),
                    "build-hnsw".to_string(),
                    "--check".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    demo_data.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "models-check-update" => vec!["models", "check-update", "--json"],
        "models-status" => vec!["models", "status", "--json"],
        "models-verify" => vec!["models", "verify", "--json"],
        "pack" => {
            return Some((
                vec![
                    "pack".to_string(),
                    "matrix".to_string(),
                    "--json".to_string(),
                    "--limit".to_string(),
                    "2".to_string(),
                    "--max-evidence".to_string(),
                    "2".to_string(),
                    "--max-tokens".to_string(),
                    "1200".to_string(),
                    "--data-dir".to_string(),
                    demo_data.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "search" => {
            return Some((
                vec![
                    "search".to_string(),
                    "matrix".to_string(),
                    "--json".to_string(),
                    "--limit".to_string(),
                    "2".to_string(),
                    "--data-dir".to_string(),
                    demo_data.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "sessions" => {
            return Some((
                vec![
                    "sessions".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    demo_data.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "state" => {
            return Some((
                vec![
                    "state".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    empty_data_dir.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "stats" => {
            return Some((
                vec![
                    "stats".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    demo_data.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "status" => {
            return Some((
                vec![
                    "status".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    empty_data_dir.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "triage" => {
            return Some((
                vec![
                    "triage".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    empty_data_dir.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "view" => {
            return Some((
                vec![
                    "view".to_string(),
                    session.to_string(),
                    "-n".to_string(),
                    "1".to_string(),
                    "--json".to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        _ => return None,
    }
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();

    Some((args, ExpectStatus::ExitOk))
}

#[test]
fn introspect_response_schemas_cover_runtime_json_shapes() -> Result<(), Box<dyn Error>> {
    let test_home = tempfile::tempdir().expect("create temp home");
    let demo_data = isolated_search_demo_data(test_home.path())?;
    let introspect = run_json(
        test_home.path(),
        &["introspect".to_string(), "--json".to_string()],
        ExpectStatus::ExitOk,
    );
    let response_schemas = introspect["response_schemas"]
        .as_object()
        .expect("introspect.response_schemas is an object");
    let mut gaps = Vec::new();

    for (surface, advertised_schema) in response_schemas {
        let Some((args, expect_status)) = surface_command(surface, test_home.path(), &demo_data)
        else {
            if surface.starts_with("doctor-") {
                continue;
            }
            panic!("no runtime command sample mapped for introspect response schema {surface}");
        };
        let payload = run_json(test_home.path(), &args, expect_status);
        let sampled_items = match surface.as_str() {
            "search" => Some("hits"),
            "pack" => Some("evidence"),
            _ => None,
        };
        if let Some(field) = sampled_items {
            assert!(
                payload[field]
                    .as_array()
                    .is_some_and(|items| !items.is_empty()),
                "{surface} must exercise nonempty {field} item schemas: {payload}"
            );
        }
        let runtime_schema = json_value_schema(&payload);
        collect_runtime_shape_gaps(surface, "$", &runtime_schema, advertised_schema, &mut gaps);
    }

    collect_zero_budget_triage_schema_gaps(
        test_home.path(),
        &demo_data,
        response_schemas,
        &mut gaps,
    );
    assert!(
        gaps.is_empty(),
        "runtime payloads are not covered by introspect schemas:\n{}",
        gaps.join("\n")
    );
    Ok(())
}

fn collect_zero_budget_triage_schema_gaps(
    test_home: &Path,
    demo_data: &Path,
    response_schemas: &Map<String, Value>,
    gaps: &mut Vec<String>,
) {
    // Zero is a real exhausted triage budget, so this deterministically
    // exercises uninspected output without delaying or substituting probes.
    let uninspected = run_json(
        test_home,
        &[
            "triage".to_string(),
            "--json".to_string(),
            "--timeout".to_string(),
            "0".to_string(),
            "--data-dir".to_string(),
            demo_data.to_string_lossy().into_owned(),
        ],
        ExpectStatus::ExitOk,
    );
    assert_eq!(uninspected["budget"]["budget_ms"], 0);
    assert_eq!(uninspected["budget"]["timed_out"], true);
    assert_eq!(uninspected["search_completeness"]["inspected"], false);
    assert_eq!(uninspected["root_cause"]["inspected"], false);
    assert_eq!(
        uninspected["search_completeness"]["quarantine_status"],
        "not_inspected"
    );
    for field in [
        "quarantined_conversations",
        "complete",
        "can_search",
        "coverage_suspect",
    ] {
        assert_eq!(
            uninspected["search_completeness"][field],
            Value::Null,
            "{field}"
        );
    }
    for section in [
        "index",
        "database",
        "pending",
        "rebuild",
        "rebuild_progress",
        "semantic",
        "ingest_quarantine",
    ] {
        assert_eq!(
            uninspected["readiness"][section]["inspected"], false,
            "{section}"
        );
    }
    for section in ["index", "database"] {
        assert_eq!(
            uninspected["readiness"][section]["exists"],
            Value::Null,
            "{section}"
        );
    }
    let triage_schema = &response_schemas["triage"];
    assert!(triage_schema["properties"]["search_completeness"]["properties"]["quarantine_status"]["enum"]
        .as_array().expect("triage quarantine enum")
        .contains(&uninspected["search_completeness"]["quarantine_status"]));
    collect_runtime_shape_gaps(
        "triage-zero-budget",
        "$",
        &json_value_schema(&uninspected),
        triage_schema,
        gaps,
    );
    // Budget fallback nulls belong to triage, not observed status verdicts.
    let status_completeness = &response_schemas["status"]["properties"]["search_completeness"];
    assert!(!schema_allows_type(
        &status_completeness["properties"]["complete"],
        "null"
    ));
    assert_eq!(
        status_completeness["properties"]["quarantine_status"]["enum"],
        json!(["ok", "degraded"])
    );
}
