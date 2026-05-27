//! Golden-file regression tests for cass robot-mode JSON outputs.
//!
//! Bead `u9osp`: cass ships a robot/LLM discovery surface via
//! `cass capabilities --json`, `cass robot-docs --json`, `cass health --json`,
//! and `cass models status --json`. These payloads are the contract every
//! downstream agent consumes — a single renamed field or moved key silently
//! breaks every consumer without failing any existing test.
//!
//! This file freezes the **shape** of those payloads against scrubbed golden
//! files under `tests/golden/robot/`. Scrubbing rules live in
//! [`scrub_robot_json`] below; see `tests/golden/robot/PROVENANCE.md` for
//! regeneration procedure.
//!
//! ## Regenerating a golden
//!
//! ```bash
//! UPDATE_GOLDENS=1 rch exec -- env CARGO_TARGET_DIR=/tmp/cass-golden-target cargo test --test golden_robot_json
//! git diff tests/golden/        # review EVERY change
//! git add tests/golden/
//! git commit -m "Update robot-mode goldens: <reason>"
//! ```
//!
//! Any diff between `actual` and golden is either a bug or an intentional
//! schema change that requires human review before it ships.

use assert_cmd::Command;
use coding_agent_search::search::tantivy::expected_index_dir;
use serde_json::{Value, json};
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use walkdir::WalkDir;

/// Build a `cass` binary invocation with the env knobs required for
/// deterministic test output (no update check, no ambient data-dir surprise).
fn cass_cmd(test_home: &std::path::Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        // Pin data dir so the test never touches the user's real cache.
        .env("XDG_DATA_HOME", test_home)
        .env("HOME", test_home)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        // Keep resource-policy goldens stable across hosts; dynamic default
        // scaling is covered by responsiveness unit tests.
        .env("CASS_RESPONSIVENESS_MAX_INFLIGHT_BYTES", "536870912");
    cmd
}

fn write_quarantined_manifest(generation_dir: &std::path::Path) {
    std::fs::create_dir_all(generation_dir).expect("create generation dir");
    std::fs::write(
        generation_dir.join("lexical-generation-manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "manifest_version": 1,
            "generation_id": "gen-quarantined",
            "attempt_id": "attempt-1",
            "created_at_ms": 1_733_000_000_000_i64,
            "updated_at_ms": 1_733_000_000_321_i64,
            "source_db_fingerprint": "fp-test",
            "conversation_count": 3,
            "message_count": 9,
            "indexed_doc_count": 9,
            "equivalence_manifest_fingerprint": null,
            "shard_plan": null,
            "build_budget": null,
            "shards": [{
                "shard_id": "shard-a",
                "shard_ordinal": 0,
                "state": "quarantined",
                "updated_at_ms": 1_733_000_000_222_i64,
                "indexed_doc_count": 9,
                "message_count": 9,
                "artifact_bytes": 512,
                "stable_hash": "stable-hash-a",
                "reclaimable": false,
                "pinned": false,
                "recovery_reason": null,
                "quarantine_reason": "validation_failed"
            }],
            "merge_debt": {
                "state": "none",
                "updated_at_ms": null,
                "pending_shard_count": 0,
                "pending_artifact_bytes": 0,
                "reason": null,
                "controller_reason": null
            },
            "build_state": "failed",
            "publish_state": "quarantined",
            "failure_history": []
        }))
        .expect("serialize manifest"),
    )
    .expect("write manifest");
}

fn seed_diag_quarantine_fixture(test_home: &std::path::Path) -> PathBuf {
    let data_dir = test_home.join("cass-data");
    let backups_dir = data_dir.join("backups");
    std::fs::create_dir_all(&backups_dir).expect("create backups dir");

    let failed_seed_root =
        backups_dir.join("agent_search.db.20260423T120000.12345.deadbeef.failed-baseline-seed.bak");
    std::fs::write(&failed_seed_root, b"seed-backup").expect("write failed seed bundle");
    std::fs::write(
        failed_seed_root.with_file_name(format!(
            "{}-wal",
            failed_seed_root
                .file_name()
                .and_then(|name| name.to_str())
                .expect("file name")
        )),
        b"seed-wal",
    )
    .expect("write failed seed wal");

    let index_path = expected_index_dir(&data_dir);
    std::fs::create_dir_all(&index_path).expect("create expected index dir");
    let retained_publish_dir = index_path
        .parent()
        .expect("index parent")
        .join(".lexical-publish-backups");
    std::fs::create_dir_all(&retained_publish_dir).expect("create retained publish dir");

    let older_backup = retained_publish_dir.join("prior-live-older");
    std::fs::create_dir_all(&older_backup).expect("create older retained backup");
    std::fs::write(older_backup.join("segment-a"), b"retained-live-segment-old")
        .expect("write older retained publish backup");
    std::thread::sleep(Duration::from_millis(20));
    let newer_backup = retained_publish_dir.join("prior-live-newer");
    std::fs::create_dir_all(&newer_backup).expect("create newer retained backup");
    std::fs::write(newer_backup.join("segment-b"), b"retained-live-segment-new")
        .expect("write newer retained publish backup");

    let generation_dir = index_path
        .parent()
        .expect("index parent")
        .join("generation-quarantined");
    write_quarantined_manifest(&generation_dir);
    std::fs::write(
        generation_dir.join("segment-a"),
        b"quarantined-generation-bytes",
    )
    .expect("write quarantined generation artifact");

    data_dir
}

fn safe_fixture_destination(dst_root: &Path, rel: &Path) -> io::Result<PathBuf> {
    let mut dst = dst_root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => dst.push(part),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "fixture path escaped source root",
                ));
            }
        }
    }
    Ok(dst)
}

fn isolated_search_demo_data(test_home: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("search_demo_data");
    let dst_root = test_home.join("search_demo_data");
    for entry in WalkDir::new(&src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(&src)?;
        let dst = safe_fixture_destination(&dst_root, rel)?;
        if entry.file_type().is_dir() {
            fs::create_dir_all(&dst)?;
        } else {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &dst)?;
        }
    }
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
                .collect::<serde_json::Map<String, Value>>();
            json!({
                "type": "object",
                "properties": properties
            })
        }
    }
}

fn sort_example_paths(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Array(paths)) = map.get_mut("example_paths") {
                paths.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
            }
            for child in map.values_mut() {
                sort_example_paths(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                sort_example_paths(child);
            }
        }
        _ => {}
    }
}

fn is_json_schema_type_value(value: &Value) -> bool {
    fn is_schema_type_name(value: &str) -> bool {
        matches!(
            value,
            "array" | "boolean" | "integer" | "null" | "number" | "object" | "string"
        )
    }

    match value {
        Value::String(value) => is_schema_type_name(value),
        Value::Array(values) => {
            !values.is_empty()
                && values
                    .iter()
                    .all(|value| value.as_str().is_some_and(is_schema_type_name))
        }
        _ => false,
    }
}

fn looks_like_json_schema_object(value: &Value) -> bool {
    let Value::Object(map) = value else {
        return false;
    };
    map.get("type").is_some_and(is_json_schema_type_value)
        && map.keys().all(|key| {
            matches!(
                key.as_str(),
                "$defs"
                    | "$id"
                    | "$ref"
                    | "$schema"
                    | "additionalProperties"
                    | "allOf"
                    | "anyOf"
                    | "const"
                    | "contains"
                    | "default"
                    | "dependentRequired"
                    | "dependentSchemas"
                    | "definitions"
                    | "description"
                    | "else"
                    | "enum"
                    | "examples"
                    | "exclusiveMaximum"
                    | "exclusiveMinimum"
                    | "format"
                    | "if"
                    | "items"
                    | "maxItems"
                    | "maxLength"
                    | "maximum"
                    | "minItems"
                    | "minLength"
                    | "minimum"
                    | "multipleOf"
                    | "not"
                    | "oneOf"
                    | "pattern"
                    | "patternProperties"
                    | "properties"
                    | "required"
                    | "then"
                    | "title"
                    | "uniqueItems"
                    | "type"
            )
        })
}

fn normalize_live_robot_values(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let redact_result_content = map.contains_key("source_path")
                && map.contains_key("line_number")
                && map.contains_key("agent");
            for (key, child) in map.iter_mut() {
                if key == "response_schemas" || looks_like_json_schema_object(child) {
                    continue;
                }

                if redact_result_content && key == "content" && child.is_string() {
                    *child = json!("[RESULT_CONTENT]");
                    continue;
                }
                if redact_result_content && key == "snippet" && child.is_string() {
                    *child = json!("[RESULT_SNIPPET]");
                    continue;
                }

                match key.as_str() {
                    "current_capacity_pct" => {
                        *child = json!(100);
                        continue;
                    }
                    "shrink_count" | "grow_count" => {
                        *child = json!(0);
                        continue;
                    }
                    "recent_decisions" => {
                        *child = json!([]);
                        continue;
                    }
                    "topology_class" => {
                        *child = json!("many_core_single_socket");
                        continue;
                    }
                    "logical_cpus" => {
                        *child = json!(128);
                        continue;
                    }
                    "physical_cores" => {
                        *child = json!(64);
                        continue;
                    }
                    "sockets" | "numa_nodes" => {
                        *child = json!(1);
                        continue;
                    }
                    "llc_groups" => {
                        *child = json!(8);
                        continue;
                    }
                    "smt_threads_per_core" => {
                        *child = json!(2);
                        continue;
                    }
                    "semantic_batchers" => {
                        *child = json!(8);
                        continue;
                    }
                    "steady_batch_fetch_conversations" => {
                        *child = json!(1024);
                        continue;
                    }
                    "startup_batch_fetch_conversations" => {
                        *child = json!(32);
                        continue;
                    }
                    "controller_loadavg_high_watermark_1m" => {
                        if child.is_string() {
                            *child = json!("121.0");
                        } else {
                            *child = json!(121.0);
                        }
                        continue;
                    }
                    "controller_loadavg_low_watermark_1m" => {
                        if child.is_string() {
                            *child = json!("120.0");
                        } else {
                            *child = json!(120.0);
                        }
                        continue;
                    }
                    _ => {}
                }

                if let Some(text) = child.as_str() {
                    if text.starts_with("planned from ") {
                        *child = json!(
                            "planned from ManyCoreSingleSocket: 128 logical CPUs, 64 physical cores, 1 socket(s), 1 NUMA node(s), 8 LLC group(s)"
                        );
                        continue;
                    }
                    if text.starts_with("reserve ") && text.contains(" logical CPUs ") {
                        *child = json!(
                            "reserve 16 of 128 logical CPUs for interactive work, IO, and NUMA/LLC service headroom"
                        );
                        continue;
                    }
                }

                normalize_live_robot_values(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                normalize_live_robot_values(child);
            }
        }
        _ => {}
    }
}

#[test]
fn live_value_scrubbing_preserves_response_schema_properties() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let input = serde_json::to_string_pretty(&json!({
        "response_schemas": {
            "health": {
                "type": "object",
                "properties": {
                    "current_capacity_pct": {
                        "type": "integer",
                        "description": "Published capacity scalar."
                    },
                    "semantic_batchers": {
                        "type": "integer"
                    }
                }
            }
        },
        "schema_fragment": {
            "title": "Runtime budget schema",
            "type": "object",
            "properties": {
                "logical_cpus": {
                    "type": "integer"
                }
            }
        },
        "state": {
            "resource_policy": {
                "topology": {
                    "topology_class": "single_socket",
                    "logical_cpus": 999
                },
                "advisory_budgets": {
                    "semantic_batchers": 99,
                    "steady_batch_fetch_conversations": 768,
                    "startup_batch_fetch_conversations": 16
                }
            }
        }
    }))
    .expect("serialize fixture");

    let scrubbed = scrub_robot_json(&input, test_home.path());
    let scrubbed: Value = serde_json::from_str(&scrubbed).expect("parse scrubbed fixture");

    assert_eq!(
        scrubbed["response_schemas"]["health"]["properties"]["current_capacity_pct"]["type"],
        "integer"
    );
    assert_eq!(
        scrubbed["response_schemas"]["health"]["properties"]["semantic_batchers"]["type"],
        "integer"
    );
    assert_eq!(
        scrubbed["schema_fragment"]["properties"]["logical_cpus"]["type"],
        "integer"
    );
    assert_eq!(
        scrubbed["state"]["resource_policy"]["topology"]["topology_class"],
        "many_core_single_socket"
    );
    assert_eq!(
        scrubbed["state"]["resource_policy"]["topology"]["logical_cpus"],
        128
    );
    assert_eq!(
        scrubbed["state"]["resource_policy"]["advisory_budgets"]["semantic_batchers"],
        8
    );
    assert_eq!(
        scrubbed["state"]["resource_policy"]["advisory_budgets"]["steady_batch_fetch_conversations"],
        1024
    );
    assert_eq!(
        scrubbed["state"]["resource_policy"]["advisory_budgets"]["startup_batch_fetch_conversations"],
        32
    );
}

#[test]
fn live_value_scrubbing_normalizes_runtime_objects_with_type_fields() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let input = serde_json::to_string_pretty(&json!({
        "response_schemas": {
            "health": {
                "type": "object",
                "properties": {
                    "logical_cpus": {
                        "type": "integer"
                    }
                }
            }
        },
        "runtime": {
            "type": "resource_policy",
            "description": "runtime status, not JSON Schema",
            "default": "host sampled",
            "logical_cpus": 999,
            "advisory_budgets": {
                "semantic_batchers": 99
            }
        }
    }))
    .expect("serialize fixture");

    let scrubbed = scrub_robot_json(&input, test_home.path());
    let scrubbed: Value = serde_json::from_str(&scrubbed).expect("parse scrubbed fixture");

    assert_eq!(
        scrubbed["response_schemas"]["health"]["properties"]["logical_cpus"]["type"],
        "integer"
    );
    assert_eq!(scrubbed["runtime"]["type"], "resource_policy");
    assert_eq!(
        scrubbed["runtime"]["description"],
        "runtime status, not JSON Schema"
    );
    assert_eq!(scrubbed["runtime"]["default"], "host sampled");
    assert_eq!(scrubbed["runtime"]["logical_cpus"], 128);
    assert_eq!(
        scrubbed["runtime"]["advisory_budgets"]["semantic_batchers"],
        8
    );
}

#[test]
fn live_value_scrubbing_redacts_repo_paths_and_result_content() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let repo_root = env!("CARGO_MANIFEST_DIR");
    let input = serde_json::to_string_pretty(&json!({
        "results": [{
            "source_path": format!("{repo_root}/.aider.chat.history.md"),
            "workspace": repo_root,
            "line_number": 42,
            "agent": "aider",
            "snippet": "private snippet",
            "content": "private prompt and assistant transcript"
        }]
    }))
    .expect("serialize fixture");

    let scrubbed = scrub_robot_json(&input, test_home.path());
    let scrubbed: Value = serde_json::from_str(&scrubbed).expect("parse scrubbed fixture");

    assert_eq!(
        scrubbed["results"][0]["source_path"],
        "[REPO]/.aider.chat.history.md"
    );
    assert_eq!(scrubbed["results"][0]["workspace"], "[REPO]");
    assert_eq!(scrubbed["results"][0]["snippet"], "[RESULT_SNIPPET]");
    assert_eq!(scrubbed["results"][0]["content"], "[RESULT_CONTENT]");
    assert!(
        !serde_json::to_string(&scrubbed)
            .expect("serialize scrubbed fixture")
            .contains("private prompt")
    );
}

/// Strip non-deterministic values from a robot-mode JSON payload so the
/// golden captures *shape* rather than ephemeral facts.
///
/// - `crate_version` / top-level `version` → `"[VERSION]"` so the test survives cargo version bumps
/// - ISO timestamps → `"[TIMESTAMP]"`
/// - Absolute paths under the test `HOME` → `"[PATH]"`
/// - UUID-ish tokens → `"[UUID]"`
fn scrub_robot_json(input: &str, test_home: &std::path::Path) -> String {
    let mut out = input.to_string();

    // 1. `crate_version` field in capabilities output. Match the exact JSON
    //    key so we don't inadvertently touch version strings inside features.
    let crate_version_re = regex::Regex::new(r#""crate_version"\s*:\s*"[^"]*""#).unwrap();
    out = crate_version_re
        .replace_all(&out, r#""crate_version": "[VERSION]""#)
        .to_string();
    let top_level_version_re = regex::Regex::new(r#"(?m)^  "version"\s*:\s*"[^"]*""#).unwrap();
    out = top_level_version_re
        .replace_all(&out, r#"  "version": "[VERSION]""#)
        .to_string();

    // 2. ISO-8601 timestamps (match with optional fractional seconds / tz).
    let ts_re =
        regex::Regex::new(r#"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})?"#)
            .unwrap();
    out = ts_re.replace_all(&out, "[TIMESTAMP]").to_string();

    // 3. Absolute paths rooted at the isolated test HOME. Anything else is
    //    either a constant relative path or a configured mount — both are
    //    shape-relevant and stay in the golden.
    let home_str = test_home.display().to_string();
    if !home_str.is_empty() {
        out = out.replace(&home_str, "[TEST_HOME]");
    }

    // 3b. Fixture databases can intentionally contain the repository root as
    // historical source metadata. Keep the field shape, but never freeze the
    // local checkout path into a public contract golden.
    let repo_root = env!("CARGO_MANIFEST_DIR");
    if !repo_root.is_empty() {
        out = out.replace(repo_root, "[REPO]");
    }

    // 4. UUIDs.
    let uuid_re =
        regex::Regex::new(r#"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"#)
            .unwrap();
    out = uuid_re.replace_all(&out, "[UUID]").to_string();

    // 5. Wall-clock durations vary run to run and by host. Keep the fields in
    // the golden to prove the shape, but scrub the values so drift on them
    // does not fail the contract test.
    for (key, replacement) in [
        ("latency_ms", "[LATENCY_MS]"),
        ("elapsed_ms", "[ELAPSED_MS]"),
        ("probe_duration_ms", "[ELAPSED_MS]"),
        ("slowest_elapsed_ms", "[ELAPSED_MS]"),
    ] {
        let re = regex::Regex::new(&format!(r#""{key}"\s*:\s*\d+"#)).unwrap();
        out = re
            .replace_all(&out, format!(r#""{key}": "{replacement}""#).as_str())
            .to_string();
    }
    let slowest_operation_re = regex::Regex::new(r#""slowest_operation"\s*:\s*"[^"]*""#).unwrap();
    out = slowest_operation_re
        .replace_all(&out, r#""slowest_operation": "[LIVE_OPERATION]""#)
        .to_string();

    // 6. Live-sampled kernel metrics in health --json (load average per
    // core and PSI CPU pressure). These float values change between runs
    // based on whatever else is happening on the box. Scrub to placeholders
    // so the golden locks the shape without chasing host noise.
    for key in ["load_per_core", "psi_cpu_some_avg10"] {
        let re = regex::Regex::new(&format!(
            r#""{key}"\s*:\s*(-?\d+(\.\d+)?([eE][+-]?\d+)?|null)"#
        ))
        .unwrap();
        out = re
            .replace_all(&out, format!(r#""{key}": "[LIVE_METRIC]""#).as_str())
            .to_string();
    }

    // 7. Watchdog sampler counters in health --json. These tick each time
    // the responsiveness sampler fires; the test can race with that timer
    // (0 ticks before the first sample, 1+ ticks after). Scrub the integer
    // to a placeholder so the golden locks the *shape* of the counter
    // surface without chasing sampler-timing drift.
    for key in [
        "healthy_streak",
        "ticks_total",
        "load_window_len",
        "psi_window_len",
        "observations_total",
    ] {
        let re = regex::Regex::new(&format!(r#""{key}"\s*:\s*\d+"#)).unwrap();
        out = re
            .replace_all(&out, format!(r#""{key}": "[LIVE_COUNTER]""#).as_str())
            .to_string();
    }

    // 8. `last_snapshot` + `last_reason` in health --json vary between
    // `null` (sampler has not yet fired) and a populated object/string
    // (sampler has fired at least once) depending on timing. The content
    // of the populated form already has its inner floats scrubbed by
    // rule 6; the remaining difference is whether the sampler fired. Fold
    // both forms to a single sentinel so the golden does not race the
    // sampler timer. We match `null`, a string value, or a `{...}` object
    // by consuming everything up to the next unescaped `"..."` key at the
    // same indentation — kept narrow so the scrub only fires on the
    // health watchdog block.
    //
    // The object form is multi-line pretty-printed JSON; `(?s)` enables
    // `.` to match newlines. Non-greedy match `.*?` stops at the first
    // closing `}` on its own line at the correct indent. We rely on the
    // outer scrub-then-compare discipline: any false-positive collapse
    // would still fail the golden because the sentinel would differ
    // between runs — the goal is deterministic scrubbing, not semantic
    // parsing.
    let last_snapshot_obj_re = regex::Regex::new(r#"(?s)"last_snapshot"\s*:\s*\{[^}]*\}"#).unwrap();
    out = last_snapshot_obj_re
        .replace_all(&out, r#""last_snapshot": "[LIVE_SAMPLE]""#)
        .to_string();
    let last_snapshot_null_re = regex::Regex::new(r#""last_snapshot"\s*:\s*null"#).unwrap();
    out = last_snapshot_null_re
        .replace_all(&out, r#""last_snapshot": "[LIVE_SAMPLE]""#)
        .to_string();

    let last_reason_re = regex::Regex::new(r#""last_reason"\s*:\s*(null|"[^"]*")"#).unwrap();
    out = last_reason_re
        .replace_all(&out, r#""last_reason": "[LIVE_SAMPLE]""#)
        .to_string();

    // Resource policy status reports include host-live CPU and memory budgets.
    // The shape is contractual; the sampled worker/byte counts are not.
    for key in [
        "available_parallelism",
        "reserved_cores",
        "max_workers",
        "effective_worker_ceiling",
        "workers",
        "tantivy_writer_threads",
        "shard_builders",
        "merge_workers",
        "staged_shard_builders",
        "staged_merge_workers",
        "page_prep_workers",
    ] {
        let re = regex::Regex::new(&format!(r#""{key}"\s*:\s*("?\d+"?)"#)).unwrap();
        out = re
            .replace_all(&out, format!(r#""{key}": "[LIVE_COUNTER]""#).as_str())
            .to_string();
    }

    for key in [
        "memory_available_bytes",
        "memory_total_bytes",
        "cache_cap_bytes",
        "available_bytes",
        "max_inflight_bytes",
        "pipeline_max_message_bytes_in_flight",
    ] {
        let re = regex::Regex::new(&format!(r#""{key}"\s*:\s*("?\d+"?)"#)).unwrap();
        out = re
            .replace_all(&out, format!(r#""{key}": "[LIVE_BYTES]""#).as_str())
            .to_string();
    }

    let age_seconds_re = regex::Regex::new(r#""age_seconds"\s*:\s*(\d+|null)"#).unwrap();
    out = age_seconds_re
        .replace_all(&out, r#""age_seconds": "[AGE_SECONDS]""#)
        .to_string();

    let last_read_re = regex::Regex::new(r#""last_read_at_ms"\s*:\s*(\d+|null)"#).unwrap();
    out = last_read_re
        .replace_all(&out, r#""last_read_at_ms": "[LAST_READ_MS]""#)
        .to_string();

    if let Ok(mut parsed) = serde_json::from_str::<Value>(&out) {
        normalize_live_robot_values(&mut parsed);
        sort_example_paths(&mut parsed);
        if let Ok(canonical) = serde_json::to_string_pretty(&parsed) {
            out = canonical;
        }
    }

    out
}

/// Compare `actual` against the golden at `tests/golden/<name>`. Writes /
/// overwrites the golden when `UPDATE_GOLDENS=1` is set in the env.
fn assert_golden(name: &str, actual: &str) {
    let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name);

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        std::fs::create_dir_all(golden_path.parent().unwrap()).expect("create golden parent dir");
        std::fs::write(&golden_path, actual).expect("write golden file");
        eprintln!("[GOLDEN] Updated: {}", golden_path.display());
        return;
    }

    let expected = std::fs::read_to_string(&golden_path).unwrap_or_else(|err| {
        panic!(
            "Golden file missing or unreadable: {}\n{err}\n\n\
             Run with UPDATE_GOLDENS=1 to create it, then review and commit:\n\
             \tUPDATE_GOLDENS=1 rch exec -- env CARGO_TARGET_DIR=/tmp/cass-golden-target cargo test --test golden_robot_json\n\
             \tgit diff tests/golden/\n\
             \tgit add tests/golden/",
            golden_path.display(),
        )
    });

    if actual != expected {
        // Dump actual next to golden for easy diffing.
        let actual_path = golden_path.with_extension("actual");
        std::fs::write(&actual_path, actual).expect("write .actual file");
        panic!(
            "GOLDEN MISMATCH: {name}\n\n\
             Expected: {}\n\
             Actual:   {}\n\n\
             diff the two files to see the drift, then either:\n\
             \t- fix the code if this was unintentional, or\n\
             \t- regenerate: UPDATE_GOLDENS=1 rch exec -- env CARGO_TARGET_DIR=/tmp/cass-golden-target cargo test --test golden_robot_json \\\n\
             \t              && git diff tests/golden/ && git add tests/golden/",
            golden_path.display(),
            actual_path.display(),
        );
    }
}

#[test]
fn robot_json_goldens_do_not_embed_repo_paths_or_raw_session_content() {
    if std::env::var("UPDATE_GOLDENS").is_ok() {
        return;
    }

    let golden_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("robot");
    let repo_root = env!("CARGO_MANIFEST_DIR");
    let forbidden = [
        repo_root,
        "/data/projects/coding_agent_session_search",
        ".venv/bin/aider --no-git --message",
        "Using openrouter/deepseek",
        "Aider v0.",
        "https://aider.chat/HISTORY.html",
    ];

    for entry in WalkDir::new(&golden_root) {
        let entry = entry.expect("walk robot goldens");
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|ext| ext.to_str()) != Some("golden")
        {
            continue;
        }
        let contents = fs::read_to_string(entry.path())
            .unwrap_or_else(|err| panic!("read {}: {err}", entry.path().display()));
        for needle in forbidden {
            assert!(
                needle.is_empty() || !contents.contains(needle),
                "{} contains unredacted golden-only sensitive fixture text: {needle}",
                entry.path().display()
            );
        }
    }
}

fn should_scan_for_bare_golden_regeneration_recipe(rel_path: &str) -> bool {
    if rel_path == "AGENTS.md" || rel_path == "README.md" {
        return true;
    }
    if rel_path.starts_with("docs/artifacts/") || rel_path == "docs/planning/UPGRADE_LOG.md" {
        return false;
    }
    if rel_path.starts_with("docs/") {
        return rel_path.ends_with(".md");
    }
    if rel_path.starts_with("tests/golden/") {
        return rel_path.ends_with(".golden")
            || rel_path.ends_with(".json")
            || rel_path.ends_with(".md");
    }
    rel_path.starts_with("tests/") && rel_path.ends_with(".rs")
}

#[test]
fn golden_regeneration_hints_do_not_use_bare_cargo() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let forbidden = concat!("UPDATE_GOLDENS=1 ", "cargo test");
    let mut violations = Vec::new();

    for root in ["AGENTS.md", "README.md", "docs", "tests"] {
        let root = repo_root.join(root);
        for entry in WalkDir::new(&root) {
            let entry = entry.expect("walk golden recipe policy files");
            if !entry.file_type().is_file() {
                continue;
            }

            let rel_path = entry
                .path()
                .strip_prefix(&repo_root)
                .expect("entry under repo root")
                .to_string_lossy()
                .replace('\\', "/");
            if !should_scan_for_bare_golden_regeneration_recipe(&rel_path) {
                continue;
            }

            let contents = fs::read_to_string(entry.path())
                .unwrap_or_else(|err| panic!("read {}: {err}", entry.path().display()));
            if contents.contains(forbidden) {
                violations.push(rel_path);
            }
        }
    }

    assert!(
        violations.is_empty(),
        "golden regeneration hints must use rch offload, not bare cargo:\n{}",
        violations.join("\n")
    );
}

#[test]
fn agents_md_does_not_recommend_bare_cargo_commands() {
    let agents_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("AGENTS.md");
    let contents = fs::read_to_string(&agents_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", agents_path.display()));
    let bare_cargo_line = regex::Regex::new(
        r"^(UPDATE_GOLDENS=1\s+)?(env\s+CARGO_TARGET_DIR=[^\s]+\s+)?cargo\s+(build|test|check|clippy|fmt)\b",
    )
    .expect("compile bare cargo command regex");

    let violations = contents
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let trimmed = line.trim();
            bare_cargo_line
                .is_match(trimmed)
                .then(|| format!("{}:{}", index + 1, trimmed))
        })
        .collect::<Vec<_>>();

    assert!(
        violations.is_empty(),
        "AGENTS.md must show rch-offloaded cargo command examples:\n{}",
        violations.join("\n")
    );
}

/// Capture stdout of `cass <args>` in the isolated test home and return
/// the scrubbed canonical-JSON form (keys-sorted by serde_json's default
/// `BTreeMap` insertion preservation, pretty-printed, dynamic values
/// scrubbed). Returns the parsed-then-reserialized string so the golden
/// survives whitespace drift.
///
/// `expect_status` selects the exit-code contract: `ExitOk` for commands
/// that must succeed (capabilities, models status), `ExitAny` for
/// commands that legitimately exit non-zero when reporting a problem
/// (health, which exits 1 when the DB / index is not initialised — that
/// non-zero status *is* part of the contract and we freeze its JSON).
fn capture_robot_json(
    test_home: &std::path::Path,
    args: &[&str],
    expect_status: ExpectStatus,
) -> String {
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
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("cass {args:?} stdout is not JSON: {err}\nstdout:\n{stdout}"));
    let canonical = serde_json::to_string_pretty(&parsed).expect("pretty-print JSON");
    scrub_robot_json(&canonical, test_home)
}

fn capture_robot_json_value(
    test_home: &std::path::Path,
    args: &[&str],
    expect_status: ExpectStatus,
) -> Value {
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
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|err| panic!("cass {args:?} stdout is not JSON: {err}"))
}

#[derive(Clone, Copy)]
enum ExpectStatus {
    ExitOk,
    ExitAny,
}

#[test]
fn capabilities_json_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let scrubbed = capture_robot_json(
        test_home.path(),
        &["capabilities", "--json"],
        ExpectStatus::ExitOk,
    );
    assert_golden("robot/capabilities.json.golden", &scrubbed);
}

#[test]
fn capabilities_shape_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let capabilities = capture_robot_json_value(
        test_home.path(),
        &["capabilities", "--json"],
        ExpectStatus::ExitOk,
    );
    let canonical =
        serde_json::to_string_pretty(&json_value_schema(&capabilities)).expect("pretty-print JSON");
    assert_golden("robot/capabilities_shape.json.golden", &canonical);
}

#[test]
fn models_status_json_matches_golden() {
    // `cass models status --json` reads XDG_DATA_HOME for the model cache
    // directory. In our isolated test home the cache is always empty, so
    // the output is deterministic: state=not_installed across every field.
    // Absolute paths inside the payload (`model_dir`, `files[].actual_path`)
    // get scrubbed by `scrub_robot_json` → `[TEST_HOME]` prefix.
    let test_home = tempfile::tempdir().expect("create temp home");
    let scrubbed = capture_robot_json(
        test_home.path(),
        &["models", "status", "--json"],
        ExpectStatus::ExitOk,
    );
    assert_golden("robot/models_status.json.golden", &scrubbed);
}

#[test]
fn models_status_shape_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let status = capture_robot_json_value(
        test_home.path(),
        &["models", "status", "--json"],
        ExpectStatus::ExitOk,
    );
    let canonical =
        serde_json::to_string_pretty(&json_value_schema(&status)).expect("pretty-print JSON");
    assert_golden("robot/models_status_shape.json.golden", &canonical);
}

#[test]
fn health_json_matches_golden() {
    // `cass health --json` reports readiness for an isolated empty HOME:
    // status=not_initialized, healthy=false, db.exists=false,
    // state.index.status=missing, state.semantic.availability=...
    // All paths scrub to [TEST_HOME], latency_ms scrubs to [LATENCY_MS].
    // The golden freezes the full readiness contract (ibuuh.9 scope):
    // top-level status/healthy/initialized/errors/recommended_action
    // plus the per-subsystem state.* nested blocks.
    let test_home = tempfile::tempdir().expect("create temp home");
    // `cass health` exits 1 when reporting an unhealthy / uninitialised
    // state — that non-zero exit is part of the contract and the golden
    // below freezes the JSON body that accompanies it. ExitAny lets the
    // capture proceed regardless of status.
    let scrubbed = capture_robot_json(
        test_home.path(),
        &["health", "--json"],
        ExpectStatus::ExitAny,
    );
    assert_golden("robot/health.json.golden", &scrubbed);
}

#[test]
fn health_shape_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let health = capture_robot_json(
        test_home.path(),
        &["health", "--json"],
        ExpectStatus::ExitAny,
    );
    let health: Value = serde_json::from_str(&health).expect("parse scrubbed health JSON");
    let canonical =
        serde_json::to_string_pretty(&json_value_schema(&health)).expect("pretty-print JSON");
    assert_golden("robot/health_shape.json.golden", &canonical);
}

#[test]
fn diag_json_matches_golden() {
    // `cass diag --json` is the artifact-inventory surface that
    // ibuuh.36's verification matrix wants frozen alongside manifest
    // snapshots and golden-query digests: version, platform, paths,
    // database counts, index presence, and per-connector detection. Under
    // an isolated empty HOME every field is deterministic (no connectors
    // detected, database/index absent, paths scrub to [TEST_HOME]).
    // Freezing this makes drift on any connector-detection or path-layout
    // field fail in CI instead of silently misreporting to operators.
    let test_home = tempfile::tempdir().expect("create temp home");
    let scrubbed = capture_robot_json(test_home.path(), &["diag", "--json"], ExpectStatus::ExitOk);
    assert_golden("robot/diag.json.golden", &scrubbed);
}

#[test]
fn diag_shape_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let diag =
        capture_robot_json_value(test_home.path(), &["diag", "--json"], ExpectStatus::ExitOk);
    let canonical =
        serde_json::to_string_pretty(&json_value_schema(&diag)).expect("pretty-print JSON");
    assert_golden("robot/diag_shape.json.golden", &canonical);
}

#[test]
fn diag_quarantine_json_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let data_dir = seed_diag_quarantine_fixture(test_home.path());
    let output = cass_cmd(test_home.path())
        .env("CASS_LEXICAL_PUBLISH_BACKUP_RETENTION", "1")
        .args([
            "diag",
            "--json",
            "--quarantine",
            "--data-dir",
            data_dir.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run cass diag --json --quarantine");
    assert!(
        output.status.success(),
        "cass diag --json --quarantine exited non-zero: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|err| {
        panic!("diag --quarantine stdout is not JSON: {err}\nstdout:\n{stdout}")
    });
    let canonical = serde_json::to_string_pretty(&parsed).expect("pretty-print JSON");
    let scrubbed = scrub_robot_json(&canonical, test_home.path());
    assert_golden("robot/diag_quarantine.json.golden", &scrubbed);
}

#[test]
fn doctor_quarantine_json_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let data_dir = seed_diag_quarantine_fixture(test_home.path());
    let output = cass_cmd(test_home.path())
        .env("CASS_LEXICAL_PUBLISH_BACKUP_RETENTION", "1")
        .args([
            "doctor",
            "--json",
            "--data-dir",
            data_dir.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run cass doctor --json");
    assert!(
        output.status.success(),
        "cass doctor --json exited non-zero: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("doctor stdout is not JSON: {err}\nstdout:\n{stdout}"));
    let canonical = serde_json::to_string_pretty(&parsed).expect("pretty-print JSON");
    let scrubbed = scrub_robot_json(&canonical, test_home.path());
    assert_golden("robot/doctor_quarantine.json.golden", &scrubbed);
}

#[test]
fn status_quarantine_json_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let data_dir = seed_diag_quarantine_fixture(test_home.path());
    let output = cass_cmd(test_home.path())
        .env("CASS_LEXICAL_PUBLISH_BACKUP_RETENTION", "1")
        .args([
            "status",
            "--json",
            "--data-dir",
            data_dir.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run cass status --json");
    assert!(
        output.status.success(),
        "cass status --json exited non-zero: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("status stdout is not JSON: {err}\nstdout:\n{stdout}"));
    let canonical = serde_json::to_string_pretty(&parsed).expect("pretty-print JSON");
    let scrubbed = scrub_robot_json(&canonical, test_home.path());
    assert_golden("robot/status_quarantine.json.golden", &scrubbed);
}

#[test]
fn status_quarantine_full_json_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let data_dir = seed_diag_quarantine_fixture(test_home.path());
    let output = cass_cmd(test_home.path())
        .env("CASS_LEXICAL_PUBLISH_BACKUP_RETENTION", "1")
        .args([
            "status",
            "--json",
            "--data-dir",
            data_dir.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run cass status --json");
    assert!(
        output.status.success(),
        "cass status --json exited non-zero: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("status stdout is not JSON: {err}\nstdout:\n{stdout}"));
    let canonical = serde_json::to_string_pretty(&parsed).expect("pretty-print JSON");
    let scrubbed = scrub_robot_json(&canonical, test_home.path());
    assert_golden("robot/status_quarantine_full.json.golden", &scrubbed);
}

#[test]
fn quarantine_summary_shape_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let data_dir = seed_diag_quarantine_fixture(test_home.path());

    fn command_json(
        test_home: &std::path::Path,
        data_dir: &std::path::Path,
        args: &[&str],
    ) -> Value {
        let output = cass_cmd(test_home)
            .env("CASS_LEXICAL_PUBLISH_BACKUP_RETENTION", "1")
            .args(args)
            .arg(data_dir)
            .output()
            .expect("run cass command");
        assert!(
            output.status.success(),
            "cass {args:?} exited non-zero: status={:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        serde_json::from_slice(&output.stdout).expect("valid JSON")
    }

    let status = command_json(
        test_home.path(),
        &data_dir,
        &["status", "--json", "--data-dir"],
    );
    let diag = command_json(
        test_home.path(),
        &data_dir,
        &["diag", "--json", "--quarantine", "--data-dir"],
    );
    let doctor = command_json(
        test_home.path(),
        &data_dir,
        &["doctor", "--json", "--data-dir"],
    );

    let status_shape = json_value_schema(&status["quarantine"]["summary"]);
    let diag_shape = json_value_schema(&diag["quarantine"]["summary"]);
    let doctor_shape = json_value_schema(&doctor["quarantine"]["summary"]);

    assert_eq!(
        status_shape, diag_shape,
        "status and diag quarantine summaries must expose the same schema"
    );
    assert_eq!(
        status_shape, doctor_shape,
        "status and doctor quarantine summaries must expose the same schema"
    );

    let canonical = serde_json::to_string_pretty(&status_shape).expect("pretty-print JSON");
    assert_golden("robot/quarantine_summary_shape.json.golden", &canonical);
}

#[test]
fn api_version_json_matches_golden() {
    // `cass api-version --json` is the smallest LLM contract surface —
    // three fields (crate_version, api_version, contract_version) that
    // together tell an agent "am I talking to a compatible cass build".
    // A silent bump of api_version or contract_version without a
    // coordinated client update breaks every downstream agent. Freezing
    // here catches the drift at commit time.
    let test_home = tempfile::tempdir().expect("create temp home");
    let scrubbed = capture_robot_json(
        test_home.path(),
        &["api-version", "--json"],
        ExpectStatus::ExitOk,
    );
    assert_golden("robot/api_version.json.golden", &scrubbed);
}

#[test]
fn api_version_shape_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let api_version = capture_robot_json_value(
        test_home.path(),
        &["api-version", "--json"],
        ExpectStatus::ExitOk,
    );
    let canonical =
        serde_json::to_string_pretty(&json_value_schema(&api_version)).expect("pretty-print JSON");
    assert_golden("robot/api_version_shape.json.golden", &canonical);
}

#[test]
fn stats_json_missing_db_error_envelope_matches_golden() {
    // `cass stats --json` against an isolated empty HOME emits the
    // error-envelope variant of the robot-mode JSON contract: a structured
    // `{"error": {"code", "kind", "message", "hint", "retryable"}}` payload
    // documented in robot-docs' exit-codes topic. Freezing this catches
    // silent drift in the error-envelope shape — important because agent
    // error-handling branches key on these exact fields.
    //
    // Robot-mode success payloads emit on stdout; fatal error envelopes
    // emit on stderr so stdout remains data-only for successful commands.
    let test_home = tempfile::tempdir().expect("create temp home");
    let out = cass_cmd(test_home.path())
        .args([
            "stats",
            "--json",
            "--data-dir",
            test_home.path().to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run cass stats --json");
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    let parsed: serde_json::Value =
        serde_json::from_str(&stderr).expect("stats error envelope is JSON on stderr");
    let canonical = serde_json::to_string_pretty(&parsed).expect("pretty-print JSON");
    let scrubbed = scrub_robot_json(&canonical, test_home.path());
    assert_golden("robot/stats_missing_db.json.golden", &scrubbed);
}

#[test]
fn stats_json_happy_path_matches_golden() -> Result<(), Box<dyn Error>> {
    // `coding_agent_session_search-zefv4`: the error envelope has been
    // pinned (stats_missing_db* goldens) but the success envelope had no
    // freeze — regressions to a field name or a new mandatory key on
    // the common-case happy-path would pass CI silently. Seeds the
    // existing search_demo_data fixture (324 KB canonical DB with a
    // known conversation/message count), invokes `cass stats --json`,
    // and freezes the scrubbed envelope.
    let test_home = tempfile::tempdir().expect("create temp home");
    let data_dir = isolated_search_demo_data(test_home.path())?;
    let out = cass_cmd(test_home.path())
        .args([
            "stats",
            "--json",
            "--data-dir",
            data_dir.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run cass stats --json on fixture DB");
    assert!(
        out.status.success(),
        "cass stats --json must succeed on fixture DB; status={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("stats happy-path envelope is JSON");
    let canonical = serde_json::to_string_pretty(&parsed).expect("pretty-print JSON");
    let scrubbed = scrub_robot_json(&canonical, test_home.path());
    assert_golden("robot/stats_full_payload.json.golden", &scrubbed);
    Ok(())
}

#[test]
fn stats_json_happy_path_shape_matches_golden() -> Result<(), Box<dyn Error>> {
    // Shape-only pin for the happy-path envelope so a future refactor
    // of the scrubber (or drift in fixture contents) can't accidentally
    // mask structural regressions. json_value_schema diff tolerates
    // value changes; keys + types must hold.
    let test_home = tempfile::tempdir().expect("create temp home");
    let data_dir = isolated_search_demo_data(test_home.path())?;
    let out = cass_cmd(test_home.path())
        .args([
            "stats",
            "--json",
            "--data-dir",
            data_dir.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run cass stats --json on fixture DB");
    assert!(out.status.success(), "stats must succeed");
    let parsed: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stats happy-path envelope is JSON");
    let canonical =
        serde_json::to_string_pretty(&json_value_schema(&parsed)).expect("pretty-print JSON");
    assert_golden("robot/stats_full_payload_shape.json.golden", &canonical);
    Ok(())
}

#[test]
fn stats_json_missing_db_error_envelope_shape_matches_golden() {
    // Error envelope lives on stderr; stdout stays reserved for successful data.
    let test_home = tempfile::tempdir().expect("create temp home");
    let out = cass_cmd(test_home.path())
        .args([
            "stats",
            "--json",
            "--data-dir",
            test_home.path().to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run cass stats --json");
    let parsed: serde_json::Value =
        serde_json::from_slice(&out.stderr).expect("stats error envelope is JSON on stderr");
    let canonical =
        serde_json::to_string_pretty(&json_value_schema(&parsed)).expect("pretty-print JSON");
    assert_golden("robot/stats_missing_db_shape.json.golden", &canonical);
}

#[test]
fn introspect_json_matches_golden() {
    // `cass introspect --json` is the full API schema surface — every
    // subcommand, its flags, positional args, and response-schema
    // references. Agents that bind to cass programmatically use this
    // to generate typed clients; silent drift breaks every downstream
    // client.
    //
    // Was #[ignore]'d when first captured (HashMap-based schema registry
    // emitted non-deterministic key order — filed as bead
    // coding_agent_session_search-8sl73). The underlying HashMap was
    // swapped for BTreeMap in the same commit that re-enabled this test;
    // byte-identical output is now verified across independent runs.
    let test_home = tempfile::tempdir().expect("create temp home");
    let scrubbed = capture_robot_json(
        test_home.path(),
        &["introspect", "--json"],
        ExpectStatus::ExitOk,
    );
    assert_golden("robot/introspect.json.golden", &scrubbed);
}

#[test]
fn introspect_shape_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let introspect = capture_robot_json_value(
        test_home.path(),
        &["introspect", "--json"],
        ExpectStatus::ExitOk,
    );
    let canonical =
        serde_json::to_string_pretty(&json_value_schema(&introspect)).expect("pretty-print JSON");
    assert_golden("robot/introspect_shape.json.golden", &canonical);
}

#[derive(Debug)]
struct PackContractRequirement {
    level: &'static str,
    requirement: &'static str,
    proof_tests: &'static [&'static str],
    status: &'static str,
}

const PACK_CONTRACT_MATRIX: &[PackContractRequirement] = &[
    PackContractRequirement {
        level: "MUST",
        requirement: "introspect exposes the pack command and response schema",
        proof_tests: &[
            "introspect_json_matches_golden",
            "introspect_shape_matches_golden",
            "pack_introspect_contract_matrix_is_current",
        ],
        status: "covered",
    },
    PackContractRequirement {
        level: "MUST",
        requirement: "pack accepts agent, workspace, source, time, and sessions-from filters",
        proof_tests: &[
            "parse_pack_robot_contract_flags",
            "pack_introspect_contract_matrix_is_current",
        ],
        status: "covered",
    },
    PackContractRequirement {
        level: "MUST",
        requirement: "pack exposes token, session, evidence, context, and excerpt budgets",
        proof_tests: &[
            "parse_pack_robot_contract_flags",
            "exact_token_budget_boundary_selects_until_budget_exhausted",
            "pack_introspect_contract_matrix_is_current",
        ],
        status: "covered",
    },
    PackContractRequirement {
        level: "MUST",
        requirement: "pack reports readiness, freshness, realized mode, privacy, evidence, and omissions",
        proof_tests: &[
            "introspect_json_matches_golden",
            "render_json_includes_readiness_and_warning_fields",
            "pack_introspect_contract_matrix_is_current",
        ],
        status: "covered",
    },
    PackContractRequirement {
        level: "MUST",
        requirement: "pack robot errors keep stdout data-only and use JSON error envelopes",
        proof_tests: &[
            "pack_empty_query_json_error_uses_stderr_only",
            "pack_invalid_field_with_sessions_from_stdin_is_json_error",
            "pack_rejects_sessions_robot_format_before_search",
        ],
        status: "covered",
    },
    PackContractRequirement {
        level: "SHOULD",
        requirement: "operators have an rch-based golden regeneration workflow",
        proof_tests: &[
            "golden_regeneration_hints_do_not_use_bare_cargo",
            "pack_robot_docs_contract_matrix_is_current",
        ],
        status: "covered",
    },
];

fn assert_pack_contract_matrix_complete() {
    assert!(
        PACK_CONTRACT_MATRIX
            .iter()
            .all(|row| matches!(row.level, "MUST" | "SHOULD")),
        "pack contract matrix levels must be explicit MUST/SHOULD entries"
    );
    assert!(
        PACK_CONTRACT_MATRIX
            .iter()
            .all(|row| row.status == "covered"),
        "pack contract matrix contains uncovered rows: {PACK_CONTRACT_MATRIX:#?}"
    );
    assert!(
        PACK_CONTRACT_MATRIX
            .iter()
            .all(|row| !row.requirement.trim().is_empty() && !row.proof_tests.is_empty()),
        "pack contract matrix rows must list a requirement and proof tests"
    );
}

fn find_introspect_command<'a>(introspect: &'a Value, name: &str) -> &'a Value {
    introspect["commands"]
        .as_array()
        .expect("introspect.commands is an array")
        .iter()
        .find(|command| command["name"] == name)
        .unwrap_or_else(|| panic!("introspect missing command {name}"))
}

fn find_introspect_argument<'a>(command: &'a Value, name: &str) -> &'a Value {
    command["arguments"]
        .as_array()
        .expect("command.arguments is an array")
        .iter()
        .find(|argument| argument["name"] == name)
        .unwrap_or_else(|| panic!("command missing argument {name}"))
}

fn assert_introspect_argument(
    command: &Value,
    name: &str,
    arg_type: &str,
    value_type: Option<&str>,
) {
    let argument = find_introspect_argument(command, name);
    assert_eq!(argument["arg_type"], arg_type, "{name} arg_type drifted");
    if let Some(value_type) = value_type {
        assert_eq!(
            argument["value_type"], value_type,
            "{name} value_type drifted"
        );
    }
}

#[test]
fn pack_introspect_contract_matrix_is_current() {
    assert_pack_contract_matrix_complete();

    let test_home = tempfile::tempdir().expect("create temp home");
    let capabilities = capture_robot_json_value(
        test_home.path(),
        &["capabilities", "--json"],
        ExpectStatus::ExitOk,
    );
    let features = capabilities["features"]
        .as_array()
        .expect("capabilities.features is an array");
    assert!(
        features
            .iter()
            .any(|feature| feature == "answer_pack_command"),
        "capabilities must advertise answer_pack_command"
    );

    let introspect = capture_robot_json_value(
        test_home.path(),
        &["introspect", "--json"],
        ExpectStatus::ExitOk,
    );
    let pack = find_introspect_command(&introspect, "pack");
    assert_eq!(pack["has_json_output"], true);

    for (name, arg_type, value_type) in [
        ("query", "positional", Some("string")),
        ("agent", "option", Some("string")),
        ("workspace", "option", Some("string")),
        ("limit", "option", Some("integer")),
        ("json", "flag", None),
        ("fields", "option", Some("string")),
        ("max-tokens", "option", Some("integer")),
        ("max-sessions", "option", Some("integer")),
        ("max-evidence", "option", Some("integer")),
        ("context-lines", "option", Some("integer")),
        ("max-excerpt-chars", "option", Some("integer")),
        ("request-id", "option", Some("string")),
        ("display", "option", Some("enum")),
        ("data-dir", "option", Some("path")),
        ("days", "option", Some("integer")),
        ("source", "option", Some("string")),
        ("sessions-from", "option", Some("string")),
        ("mode", "option", Some("enum")),
        ("freshness-policy", "option", Some("enum")),
        ("freshness-window-seconds", "option", Some("integer")),
        ("require-evidence", "flag", None),
        ("explain-selection", "flag", None),
        ("refresh", "flag", None),
        ("timeout", "option", Some("integer")),
        ("robot-format", "option", Some("enum")),
    ] {
        assert_introspect_argument(pack, name, arg_type, value_type);
    }

    let pack_schema = &introspect["response_schemas"]["pack"]["properties"];
    for field in [
        "schema_version",
        "query",
        "_meta",
        "limits",
        "realized",
        "health",
        "freshness",
        "pack",
        "evidence",
        "omitted",
        "privacy",
        "warnings",
    ] {
        assert!(
            pack_schema.get(field).is_some(),
            "pack response schema missing {field}"
        );
    }
}

#[test]
fn search_robot_json_matches_golden() -> Result<(), Box<dyn Error>> {
    let test_home = tempfile::tempdir().expect("create temp home");
    let data_dir = isolated_search_demo_data(test_home.path())?;
    let output = cass_cmd(test_home.path())
        .args([
            "search",
            "hello",
            "--json",
            "--limit",
            "2",
            "--data-dir",
            data_dir.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run cass search --json");
    assert!(
        output.status.success(),
        "cass search --json exited non-zero: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("search stdout is not JSON: {err}\nstdout:\n{stdout}"));
    let canonical = serde_json::to_string_pretty(&parsed).expect("pretty-print JSON");
    let scrubbed = scrub_robot_json(&canonical, test_home.path());
    assert_golden("robot/search_robot.json.golden", &scrubbed);
    Ok(())
}

#[test]
fn search_robot_shape_matches_golden() -> Result<(), Box<dyn Error>> {
    let test_home = tempfile::tempdir().expect("create temp home");
    let data_dir = isolated_search_demo_data(test_home.path())?;
    let output = cass_cmd(test_home.path())
        .args([
            "search",
            "hello",
            "--json",
            "--limit",
            "2",
            "--data-dir",
            data_dir.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run cass search --json");
    assert!(
        output.status.success(),
        "cass search --json exited non-zero: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid search JSON");
    let canonical =
        serde_json::to_string_pretty(&json_value_schema(&parsed)).expect("pretty-print JSON");
    assert_golden("robot/search_robot_shape.json.golden", &canonical);
    Ok(())
}

// ========================================================================
// Bead coding_agent_session_search-v4kz1 (child of ibuuh.10):
// Golden-artifact freeze for `cass export-html --json` envelope.
//
// Existing tests/pages_export_golden.rs spot-asserts three fields on
// the export-html JSON payload (`success`, `exported.encrypted`,
// `exported.messages_count`) but nothing pins the ENVELOPE SHAPE.
// Any regression that renamed / added / removed fields across the
// `success=true` branch ships through every consumer silently.
//
// Freeze the schema (types + keys, values scrubbed) exactly the way
// the sibling `capabilities_shape_matches_golden`,
// `health_shape_matches_golden`, and `quarantine_summary_shape_matches_golden`
// tests do. The golden file lives at
// `tests/golden/robot/export_html_shape.json.golden` and follows the
// standard UPDATE_GOLDENS=1 regeneration procedure documented at
// the top of this file.
// ========================================================================

#[test]
fn export_html_shape_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let session_path = test_home.path().join("rollout-export-shape.jsonl");
    // Minimal but complete Codex rollout: session_meta + one user +
    // one assistant message. Matches the shape the main
    // pages_export_golden.rs suite uses so the fixture mirrors real
    // export input.
    fs::write(
        &session_path,
        concat!(
            r#"{"timestamp":"2024-04-24T00:00:00Z","type":"session_meta","payload":{"id":"export-golden","cwd":"/tmp","cli_version":"0.42.0"}}"#,
            "\n",
            r#"{"timestamp":"2024-04-24T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#,
            "\n",
            r#"{"timestamp":"2024-04-24T00:00:02Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"hi"}]}}"#,
            "\n",
        ),
    )
    .expect("write session fixture");

    let output_dir = test_home.path().join("export-out");
    fs::create_dir_all(&output_dir).expect("create output dir");

    let output = cass_cmd(test_home.path())
        .arg("export-html")
        .arg(&session_path)
        .arg("--json")
        .arg("--no-cdns")
        .arg("--output-dir")
        .arg(&output_dir)
        .arg("--filename")
        .arg("shape-probe")
        .output()
        .expect("run cass export-html");

    assert!(
        output.status.success(),
        "cass export-html --json must succeed on a valid rollout; status={:?}\n\
         stdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let payload: Value = serde_json::from_slice(&output.stdout).expect("export-html emits JSON");
    let canonical =
        serde_json::to_string_pretty(&json_value_schema(&payload)).expect("pretty-print JSON");
    assert_golden("robot/export_html_shape.json.golden", &canonical);
}

// `coding_agent_session_search-oy4fd`: README line 103 advertises
// sessions / models-verify / models-check-update as golden-pinned
// JSON contract surfaces, but no goldens existed for them. The three
// tests below close that gap with shape goldens (json_value_schema
// diffs tolerate run-time values like timestamps while still pinning
// the envelope keys and types). Each test seeds the minimal state
// needed to reach a deterministic branch: sessions hits the
// missing-db error envelope (stderr, mirrors stats_missing_db
// convention); models verify/check-update run against an empty
// data_dir where the model is not yet acquired, so they reach the
// stable `not_acquired` / `model_not_installed` branches.

// `coding_agent_session_search-q931h`: status and doctor had only
// variant-scoped goldens (status_quarantine{_full}, status_semantic_*,
// doctor_quarantine). The base not-initialized envelopes emitted
// for `cass status --json` / `cass doctor --json` against a fresh
// empty data_dir — the most common shape agent harnesses see before
// the first index — had no shape pin at all. A regression that
// added, removed, or re-typed a field in the base envelope would
// compile clean and pass the existing suite. The two tests below
// close that gap via json_value_schema diffs (same pattern as
// health_shape_matches_golden / diag_shape_matches_golden).

// `coding_agent_session_search-ut3v8`: the --quarantine subset of
// cass doctor --json is frozen via doctor_quarantine.json.golden,
// but the DEFAULT base-state invocation (no --quarantine, no seeded
// fixture) had no instance freeze. Regressions to the top-level
// status / recommended_action / checks[] envelope on the fresh
// empty data_dir — the shape agent harnesses see before any index
// exists — would not fail at golden time. Closes the instance-side
// of the pin; the shape-side lives in doctor_shape.json.golden
// (bead q931h).
#[test]
fn doctor_json_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let scrubbed = capture_robot_json(
        test_home.path(),
        &["doctor", "--json"],
        ExpectStatus::ExitOk,
    );
    assert_golden("robot/doctor.json.golden", &scrubbed);
}

#[test]
fn status_shape_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let mut status = capture_robot_json_value(
        test_home.path(),
        &["status", "--json"],
        ExpectStatus::ExitOk,
    );
    // Keep the warnings array item schema pinned even when this fixture has no
    // warning instances.
    if let Some(warnings) = status
        .pointer_mut("/quarantine/warnings")
        .and_then(Value::as_array_mut)
        && warnings.is_empty()
    {
        warnings.push(Value::String("[SHAPE_STRING]".to_string()));
    }
    let canonical =
        serde_json::to_string_pretty(&json_value_schema(&status)).expect("pretty-print JSON");
    assert_golden("robot/status_shape.json.golden", &canonical);
}

#[test]
fn doctor_shape_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let doctor = capture_robot_json_value(
        test_home.path(),
        &["doctor", "--json"],
        ExpectStatus::ExitOk,
    );
    let canonical =
        serde_json::to_string_pretty(&json_value_schema(&doctor)).expect("pretty-print JSON");
    assert_golden("robot/doctor_shape.json.golden", &canonical);
}

#[test]
fn sessions_json_missing_db_error_envelope_shape_matches_golden() {
    // Mirrors stats_json_missing_db_error_envelope_shape_matches_golden:
    // no DB on a fresh data_dir means cass emits the `missing-db` error
    // envelope on stderr with exit 3. Pinning the envelope shape lets agent
    // harnesses branch on kind="missing-db" without worrying about
    // silent contract drift.
    let test_home = tempfile::tempdir().expect("create temp home");
    let out = cass_cmd(test_home.path())
        .args([
            "sessions",
            "--current",
            "--json",
            "--data-dir",
            test_home.path().to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run cass sessions --current --json");
    let parsed: serde_json::Value =
        serde_json::from_slice(&out.stderr).expect("sessions error envelope is JSON on stderr");
    let canonical =
        serde_json::to_string_pretty(&json_value_schema(&parsed)).expect("pretty-print JSON");
    assert_golden("robot/sessions_missing_db_shape.json.golden", &canonical);
}

#[test]
fn models_verify_json_not_acquired_shape_matches_golden() {
    // Empty data_dir ⇒ model is not acquired, `cass models verify
    // --json` emits the stable not_acquired envelope on stdout with
    // exit 0. Shape golden pins: status, state_detail, next_step,
    // lexical_fail_open, model_dir, all_valid, cache_lifecycle
    // (nested), error.
    let test_home = tempfile::tempdir().expect("create temp home");
    let out = cass_cmd(test_home.path())
        .args([
            "models",
            "verify",
            "--json",
            "--data-dir",
            test_home.path().to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run cass models verify --json");
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("models verify stdout is not JSON: {err}\nstdout:\n{stdout}"));
    let canonical =
        serde_json::to_string_pretty(&json_value_schema(&parsed)).expect("pretty-print JSON");
    assert_golden(
        "robot/models_verify_not_acquired_shape.json.golden",
        &canonical,
    );
}

#[test]
fn models_check_update_json_not_installed_shape_matches_golden() {
    // Empty data_dir ⇒ `cass models check-update --json` returns
    // `reason=model_not_installed` with current_revision=null +
    // latest_revision=<pinned sha>. Shape golden pins the 4-field
    // envelope so a regression that renamed/removed any field would
    // trip CI.
    let test_home = tempfile::tempdir().expect("create temp home");
    let out = cass_cmd(test_home.path())
        .args([
            "models",
            "check-update",
            "--json",
            "--data-dir",
            test_home.path().to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run cass models check-update --json");
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|err| {
        panic!("models check-update stdout is not JSON: {err}\nstdout:\n{stdout}")
    });
    let canonical =
        serde_json::to_string_pretty(&json_value_schema(&parsed)).expect("pretty-print JSON");
    assert_golden(
        "robot/models_check_update_not_installed_shape.json.golden",
        &canonical,
    );
}
