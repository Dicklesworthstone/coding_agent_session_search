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
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[allow(deprecated)]
fn cass_cmd(test_home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("cass").expect("cass binary");
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home)
        .env("HOME", test_home)
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

fn isolated_search_demo_data(test_home: &Path) -> PathBuf {
    let src = fixture_path(&["search_demo_data"]);
    let dst_root = test_home.join("search_demo_data");
    for entry in WalkDir::new(&src) {
        let entry = entry.expect("walk search demo data");
        let rel = entry
            .path()
            .strip_prefix(&src)
            .expect("relative fixture path");
        let dst = dst_root.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&dst).expect("create fixture dir");
        } else {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent).expect("create fixture parent");
            }
            fs::copy(entry.path(), &dst).expect("copy fixture file");
        }
    }
    dst_root
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

fn assert_runtime_shape_covered(surface: &str, path: &str, runtime: &Value, advertised: &Value) {
    let runtime_type = runtime
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    assert!(
        schema_allows_type(advertised, runtime_type),
        "{surface}{path}: runtime type {runtime_type:?} is not allowed by introspect schema {}",
        serde_json::to_string_pretty(advertised).expect("schema pretty-print"),
    );

    match runtime_type {
        "object" => {
            let Some(runtime_props) = runtime.get("properties").and_then(Value::as_object) else {
                return;
            };
            let advertised_props = advertised.get("properties").and_then(Value::as_object);
            if advertised_props.is_none() && schema_allows_dynamic_properties(advertised) {
                return;
            }
            let advertised_props = advertised_props.unwrap_or_else(|| {
                panic!(
                    "{surface}{path}: runtime object has properties but introspect schema has none"
                )
            });
            for (key, runtime_child) in runtime_props {
                let child_path = format!("{path}.{key}");
                let advertised_child = advertised_props.get(key).unwrap_or_else(|| {
                    panic!("{surface}{child_path}: runtime field is missing from introspect schema")
                });
                let runtime_child_type = runtime_child
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                assert!(
                    schema_allows_type(advertised_child, runtime_child_type),
                    "{surface}{child_path}: runtime type {runtime_child_type:?} is not allowed by introspect schema {}",
                    serde_json::to_string_pretty(advertised_child).expect("schema pretty-print"),
                );
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
            let advertised_items = advertised
                .get("items")
                .unwrap_or_else(|| panic!("{surface}{path}: array schema missing items"));
            assert_runtime_shape_covered(
                surface,
                &format!("{path}[]"),
                runtime_items,
                advertised_items,
            );
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
        "models-check-update" => vec!["models", "check-update", "--json"],
        "models-status" => vec!["models", "status", "--json"],
        "models-verify" => vec!["models", "verify", "--json"],
        "search" => {
            return Some((
                vec![
                    "search".to_string(),
                    "hello".to_string(),
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
fn introspect_response_schemas_cover_runtime_json_shapes() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let demo_data = isolated_search_demo_data(test_home.path());
    let introspect = run_json(
        test_home.path(),
        &["introspect".to_string(), "--json".to_string()],
        ExpectStatus::ExitOk,
    );
    let response_schemas = introspect["response_schemas"]
        .as_object()
        .expect("introspect.response_schemas is an object");

    for (surface, advertised_schema) in response_schemas {
        let Some((args, expect_status)) = surface_command(surface, test_home.path(), &demo_data)
        else {
            panic!("no runtime command sample mapped for introspect response schema {surface}");
        };
        let payload = run_json(test_home.path(), &args, expect_status);
        let runtime_schema = json_value_schema(&payload);
        assert_runtime_shape_covered(surface, "$", &runtime_schema, advertised_schema);
    }
}
