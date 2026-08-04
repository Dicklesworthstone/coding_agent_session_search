//! Real-binary gate for the opt-in `cass guide --apply` runner (bead
//! `coding_agent_session_search-guided-ops-repro-trust-5u82n.17`).
//!
//! These scenarios use fixtures intentionally: fixture apply is permanently
//! non-mutating, which makes the complete gate/transcript contract repeatable
//! while still proving that readiness, privacy, cost, rch, stop conditions,
//! and exact per-step confirmation are enforced before a mutation adapter can
//! run. Closed-allowlist and shell-metacharacter rejection are unit-tested in
//! `guide_runner` because the workflow registry itself cannot inject commands.

mod util;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use serde_json::{Value, json};

use util::timeout::spawn_with_timeout_or_diag;

type TestResult = Result<(), Box<dyn Error>>;
const TIMEOUT: Duration = Duration::from_secs(60);

fn ensure(condition: bool, message: impl FnOnce() -> String) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message().into())
    }
}

fn isolated_home() -> Result<(tempfile::TempDir, PathBuf, PathBuf), Box<dyn Error>> {
    let temp = tempfile::TempDir::new()?;
    let home = temp.path().join("home");
    let data_dir = temp.path().join("data");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&data_dir)?;
    Ok((temp, home, data_dir))
}

fn fixture_file(value: &Value) -> Result<(tempfile::TempDir, PathBuf), Box<dyn Error>> {
    let temp = tempfile::TempDir::new()?;
    let path = temp.path().join("guide-apply.json");
    std::fs::write(&path, serde_json::to_vec_pretty(value)?)?;
    Ok((temp, path))
}

fn healthy_context(facts: Value) -> Value {
    json!({
        "facts": facts,
        "triggered_stop_conditions": [],
        "privacy_preview": {
            "providers": [{
                "name": "codex",
                "source_class": "jsonl",
                "enabled": true,
                "roots": ["<redacted>"],
                "file_count": 1,
                "total_bytes": 128,
                "min_file_bytes": 128,
                "max_file_bytes": 128,
                "unreadable_count": 0,
                "secret_sample_count": 0
            }],
            "raw_mirror": {"enabled": false, "manifest_count": 0, "total_storage_bytes": 0},
            "exports": {"chatgpt_encrypted_present": false, "html_export_tier": "redacted"},
            "support_capsule": {"requested": false},
            "repro_capsule": {"requested": false},
            "source_mirror_capture": {"requested": false}
        },
        "resource_plan": {
            "host": {
                "profile": "standard",
                "cpu_count": 16,
                "memory_total_mb": 32768,
                "memory_available_mb": 24576,
                "disk_available_mb": 102400
            },
            "cass": {
                "db_size_mb": 10,
                "message_count": 100,
                "semantic_model_installed": true,
                "active_rebuild": false
            },
            "build_pressure": {"level": "low"}
        }
    })
}

fn run(home: &Path, data_dir: &Path, fixture: &Path, args: &[&str]) -> TestResultValue {
    let mut command = Command::new(cargo_bin("cass"));
    command
        .arg("guide")
        .args(args)
        .arg("--fixture")
        .arg(fixture)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--json")
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("NO_COLOR", "1")
        .env("CODEX_HOME", home.join(".codex-empty"))
        .env("CLAUDE_HOME", home.join(".claude-empty"))
        .env("GEMINI_HOME", home.join(".gemini-empty"))
        .env_remove("CLAUDE_CONFIG_DIR");
    let output = spawn_with_timeout_or_diag(command, "guide-apply", None, TIMEOUT);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let value = serde_json::from_str(stdout.trim()).map_err(|error| {
        format!(
            "guide apply stdout was not JSON: {error}; stdout={}; stderr={}",
            stdout.chars().take(500).collect::<String>(),
            stderr.chars().take(500).collect::<String>()
        )
    })?;
    Ok(value)
}

type TestResultValue = Result<Value, Box<dyn Error>>;

fn gate_result<'a>(payload: &'a Value, name: &str) -> Option<&'a str> {
    payload
        .pointer("/execution/global_gates")?
        .as_array()?
        .iter()
        .find(|gate| gate.get("gate").and_then(Value::as_str) == Some(name))?
        .get("result")?
        .as_str()
}

fn mutation_step(payload: &Value) -> Option<&Value> {
    payload
        .pointer("/execution/transcript")?
        .as_array()?
        .iter()
        .find(|step| step.get("mutation_class").and_then(Value::as_str) != Some("read-only-proof"))
}

#[test]
fn dry_run_and_apply_transcripts_are_distinct_and_fixture_safe() -> TestResult {
    let (_home_guard, home, data_dir) = isolated_home()?;
    let context = healthy_context(json!({"db_present": true}));
    let (_fixture_guard, fixture) = fixture_file(&context)?;

    let dry = run(&home, &data_dir, &fixture, &["support-capsule"])?;
    ensure(
        dry.pointer("/execution/mode") == Some(&json!("dry-run")),
        || "default guide execution must be dry-run".to_string(),
    )?;
    ensure(
        dry.pointer("/execution/transcript")
            .and_then(Value::as_array)
            .is_some_and(|steps| {
                steps
                    .iter()
                    .all(|step| step.get("result").and_then(Value::as_str) == Some("planned"))
            }),
        || "dry-run must plan, never execute, every step".to_string(),
    )?;

    let apply_args = [
        "support-capsule",
        "--apply",
        "--confirm-step",
        "3",
        "--accept-privacy-tier",
        "redacted",
        "--accept-cost-risk",
        "medium",
        "--confirm-stop-conditions-clear",
    ];
    let applied_a = run(&home, &data_dir, &fixture, &apply_args)?;
    let applied_b = run(&home, &data_dir, &fixture, &apply_args)?;
    ensure(
        applied_a.pointer("/execution/mode") == Some(&json!("apply")),
        || "--apply must identify apply mode".to_string(),
    )?;
    ensure(
        applied_a.pointer("/execution/applied_mutation_count") == Some(&json!(0)),
        || "fixture apply must never mutate".to_string(),
    )?;
    ensure(
        mutation_step(&applied_a).and_then(|step| step.get("result"))
            == Some(&json!("fixture-protected")),
        || "confirmed fixture mutation must be fixture-protected".to_string(),
    )?;
    ensure(
        applied_a.get("execution") == applied_b.get("execution"),
        || "execution transcript must be byte-semantically deterministic".to_string(),
    )?;
    ensure(
        applied_a
            .pointer("/execution/transcript")
            .and_then(Value::as_array)
            .is_some_and(|steps| {
                steps.iter().all(|step| {
                    step.get("argv").is_some()
                        && step.get("mutation_class").and_then(Value::as_str).is_some()
                        && step
                            .pointer("/proof_gate/result")
                            .and_then(Value::as_str)
                            .is_some()
                })
            }),
        || "every transcript row must carry argv, mutation class, and proof result".to_string(),
    )?;
    ensure(!data_dir.join("agent_search.db").exists(), || {
        "fixture apply created an archive database".to_string()
    })?;
    Ok(())
}

#[test]
fn apply_requires_confirmation_for_the_exact_mutating_step() -> TestResult {
    let (_home_guard, home, data_dir) = isolated_home()?;
    let (_fixture_guard, fixture) = fixture_file(&healthy_context(json!({"db_present": true})))?;
    let payload = run(
        &home,
        &data_dir,
        &fixture,
        &[
            "support-capsule",
            "--run",
            "--confirm-step",
            "2",
            "--accept-privacy-tier",
            "redacted",
            "--accept-cost-risk",
            "medium",
            "--confirm-stop-conditions-clear",
        ],
    )?;
    let step = mutation_step(&payload).ok_or("missing mutation step")?;
    ensure(step.get("result") == Some(&json!("blocked")), || {
        format!("wrong-step confirmation authorized a mutation: {step}")
    })?;
    ensure(
        gate_result(&payload, "step-confirmations") == Some("blocked"),
        || "confirmation for a read-only step must be rejected".to_string(),
    )?;
    ensure(
        step.pointer("/confirmation/required") == Some(&json!(true))
            && step.pointer("/confirmation/provided") == Some(&json!(false)),
        || "transcript must carry per-step confirmation evidence".to_string(),
    )?;
    Ok(())
}

#[test]
fn blocked_readiness_prevents_mutation() -> TestResult {
    let (_home_guard, home, data_dir) = isolated_home()?;
    let (_fixture_guard, fixture) = fixture_file(&healthy_context(json!({"db_present": false})))?;
    let payload = run(
        &home,
        &data_dir,
        &fixture,
        &[
            "support-capsule",
            "--apply",
            "--confirm-step",
            "3",
            "--accept-privacy-tier",
            "redacted",
            "--accept-cost-risk",
            "medium",
            "--confirm-stop-conditions-clear",
        ],
    )?;
    ensure(
        gate_result(&payload, "readiness") == Some("blocked"),
        || "missing db must block readiness".to_string(),
    )?;
    ensure(
        mutation_step(&payload).and_then(|step| step.get("result")) == Some(&json!("blocked")),
        || "blocked readiness must prevent mutation".to_string(),
    )?;
    Ok(())
}

#[test]
fn privacy_acceptance_must_match_the_declared_tier() -> TestResult {
    let (_home_guard, home, data_dir) = isolated_home()?;
    let (_fixture_guard, fixture) = fixture_file(&healthy_context(json!({"db_present": true})))?;
    let payload = run(
        &home,
        &data_dir,
        &fixture,
        &[
            "support-capsule",
            "--apply",
            "--confirm-step",
            "3",
            "--accept-privacy-tier",
            "sensitive",
            "--accept-cost-risk",
            "medium",
            "--confirm-stop-conditions-clear",
        ],
    )?;
    ensure(
        gate_result(&payload, "privacy-tier") == Some("needs-confirmation"),
        || "wrong privacy-tier acceptance must not pass".to_string(),
    )?;
    ensure(
        mutation_step(&payload).and_then(|step| step.get("result")) == Some(&json!("blocked")),
        || "privacy mismatch must prevent mutation".to_string(),
    )?;
    Ok(())
}

#[test]
fn observed_stop_condition_cannot_be_overridden() -> TestResult {
    let (_home_guard, home, data_dir) = isolated_home()?;
    let mut context = healthy_context(json!({"db_present": true}));
    let context_map = context
        .as_object_mut()
        .ok_or("healthy context must be an object")?;
    context_map.insert(
        "triggered_stop_conditions".to_string(),
        json!(["required evidence is unavailable"]),
    );
    let (_fixture_guard, fixture) = fixture_file(&context)?;
    let payload = run(
        &home,
        &data_dir,
        &fixture,
        &[
            "support-capsule",
            "--apply",
            "--confirm-step",
            "3",
            "--accept-privacy-tier",
            "redacted",
            "--accept-cost-risk",
            "medium",
            "--confirm-stop-conditions-clear",
        ],
    )?;
    ensure(
        gate_result(&payload, "stop-conditions") == Some("blocked"),
        || "observed stop condition must override clear assertion".to_string(),
    )?;
    ensure(
        mutation_step(&payload).and_then(|step| step.get("result")) == Some(&json!("blocked")),
        || "triggered stop condition must prevent mutation".to_string(),
    )?;
    Ok(())
}

#[test]
fn offloaded_mutation_requires_rch_and_exact_cost_acceptance() -> TestResult {
    let (_home_guard, home, data_dir) = isolated_home()?;
    let mut context = healthy_context(json!({"db_present": true, "disk_headroom_ok": true}));
    let context_map = context
        .as_object_mut()
        .ok_or("healthy context must be an object")?;
    context_map.insert(
        "proof_results".to_string(),
        json!({"assets-classified": true, "rebuild-planned": true}),
    );
    let (_fixture_guard, fixture) = fixture_file(&context)?;
    let payload = run(
        &home,
        &data_dir,
        &fixture,
        &[
            "repair-assets",
            "--apply",
            "--confirm-step",
            "3",
            "--accept-cost-risk",
            "low",
            "--confirm-stop-conditions-clear",
        ],
    )?;
    ensure(
        gate_result(&payload, "rch") == Some("needs-confirmation"),
        || "offloaded mutation must require --allow-rch".to_string(),
    )?;
    ensure(
        gate_result(&payload, "cost-risk") == Some("needs-confirmation"),
        || "wrong cost-risk acceptance must not pass".to_string(),
    )?;
    ensure(
        mutation_step(&payload).and_then(|step| step.get("result")) == Some(&json!("blocked")),
        || "rch/cost gates must prevent mutation".to_string(),
    )?;
    Ok(())
}
