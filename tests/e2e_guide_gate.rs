//! Real-binary E2E gate for the `cass guide` intent-to-command planner (bead
//! `coding_agent_session_search-guided-ops-repro-trust-5u82n.1`,
//! "Add intent-to-command planner for guided safe workflows").
//!
//! `src/guide_planner.rs` is the pure, unit-tested core (intent resolution +
//! plan rendering over the workflow macro registry). This gate proves the live
//! surface: that the real `cass` binary maps an operator intent to a safe
//! advisory command plan via `cass guide <intent> --json`, that readiness
//! tracks the supplied facts (deterministic `--fixture` scenarios: healthy →
//! ready, missing-index → blocked), that unknown intents fall back to the
//! known-intent catalog + docs, and — critically — that the surface is
//! **read-only**: running it never creates an archive DB and every plan
//! self-reports `mutation_contract.read_only=true` with no bare `cass`/`bv`
//! examples inside the step recipes.
//!
//! Per the bead's logging requirement, each scenario emits a structured log line
//! (argv, parsed intent, selected facts, rejected unsafe actions, assertion
//! summary) to stderr so the artifact is auditable.
//!
//! Isolation mirrors the onboarding gate: a fresh `tempdir` with `HOME`/`XDG_*`/
//! cwd redirected into it, empty agent homes, `CASS_IGNORE_SOURCES_CONFIG=1`,
//! `CASS_SEMANTIC_EMBEDDER=hash`, and `NO_COLOR=1`. Written panic-free (Result +
//! an `ensure` helper) so the new file stays UBS 0-critical.

mod util;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use serde_json::Value;

use util::timeout::spawn_with_timeout_or_diag;

type TestResult = Result<(), Box<dyn Error>>;

const GUIDE_TIMEOUT: Duration = Duration::from_secs(60);

fn ensure(cond: bool, msg: impl FnOnce() -> String) -> TestResult {
    if cond { Ok(()) } else { Err(msg().into()) }
}

fn head(s: &str) -> String {
    s.chars().take(400).collect()
}

/// A fresh isolated `(tempdir guard, home, data_dir)`.
fn isolated_home() -> Result<(tempfile::TempDir, PathBuf, PathBuf), Box<dyn Error>> {
    let tmp = tempfile::TempDir::new()?;
    let home = tmp.path().join("home");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&data_dir)?;
    Ok((tmp, home, data_dir))
}

fn cass_cmd(home: &Path, args: &[String]) -> Command {
    let mut cmd = Command::new(cargo_bin("cass"));
    cmd.args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("NO_COLOR", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CODEX_HOME", home.join(".codex-empty"))
        .env("CLAUDE_HOME", home.join(".claude-empty"))
        .env("GEMINI_HOME", home.join(".gemini-empty"))
        .env_remove("CLAUDE_CONFIG_DIR");
    cmd
}

/// Run a guide argv and return `(parsed_json, argv_for_logging)`.
fn run_guide(home: &Path, args: &[String]) -> Result<(Value, String), Box<dyn Error>> {
    let argv = args.join(" ");
    let cmd = cass_cmd(home, args);
    let out = spawn_with_timeout_or_diag(cmd, "guide", None, GUIDE_TIMEOUT);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let value: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        format!(
            "guide stdout not JSON: {e}; argv: {argv}; stdout head: {}; stderr head: {}",
            head(&stdout),
            head(&stderr)
        )
    })?;
    Ok((value, argv))
}

/// Structured audit log line for the artifact (argv, intent, facts, rejected
/// unsafe actions, assertion summary).
fn log_scenario(argv: &str, payload: &Value, assertion: &str) {
    let intent = payload
        .pointer("/intent/raw")
        .and_then(Value::as_str)
        .unwrap_or("(catalog)");
    let readiness = payload
        .get("readiness")
        .and_then(Value::as_str)
        .unwrap_or("(n/a)");
    let prereqs = payload
        .pointer("/plan/prerequisites")
        .cloned()
        .unwrap_or(Value::Null);
    let forbidden = payload
        .pointer("/plan/forbidden_shortcuts")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    eprintln!(
        "[guide-e2e] argv=`{argv}` intent={intent} readiness={readiness} \
         rejected_unsafe_actions={forbidden} prerequisites={prereqs} :: {assertion}"
    );
}

/// Every guide plan must be read-only and free of runnable cass/bv in its steps.
fn check_read_only_contract(payload: &Value) -> TestResult {
    ensure(
        payload
            .pointer("/mutation_contract/read_only")
            .and_then(Value::as_bool)
            == Some(true),
        || "guide must self-report mutation_contract.read_only=true".to_string(),
    )?;
    ensure(
        payload
            .pointer("/mutation_contract/mutates_db")
            .and_then(Value::as_bool)
            == Some(false),
        || "guide must not mutate the DB".to_string(),
    )?;
    ensure(
        payload.get("schema_version").and_then(Value::as_str) == Some("cass.guide.plan.v1"),
        || "guide schema_version must be cass.guide.plan.v1".to_string(),
    )?;
    if let Some(steps) = payload.pointer("/plan/steps") {
        let steps_text = steps.to_string();
        ensure(
            !steps_text.contains("cass ") && !steps_text.contains("bv "),
            || "step recipes must not embed runnable cass/bv commands".to_string(),
        )?;
    }
    Ok(())
}

/// Write a `{ "facts": {...} }` fixture file and return its path (kept alive by
/// the returned tempdir guard).
fn facts_fixture(pairs: &[(&str, bool)]) -> Result<(tempfile::TempDir, PathBuf), Box<dyn Error>> {
    let tmp = tempfile::TempDir::new()?;
    let path = tmp.path().join("facts.json");
    let mut facts = serde_json::Map::new();
    for (k, v) in pairs {
        facts.insert((*k).to_string(), Value::Bool(*v));
    }
    let doc = serde_json::json!({ "facts": Value::Object(facts) });
    std::fs::write(&path, serde_json::to_vec_pretty(&doc)?)?;
    Ok((tmp, path))
}

fn guide_argv(intent: &[&str], extra: &[&str], data_dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = vec!["guide".to_string()];
    v.extend(intent.iter().map(|s| (*s).to_string()));
    v.extend(extra.iter().map(|s| (*s).to_string()));
    v.push("--json".to_string());
    v.push("--data-dir".to_string());
    v.push(data_dir.to_string_lossy().into_owned());
    v
}

#[test]
fn guide_catalog_lists_intents_and_is_readonly() -> TestResult {
    let (_tmp, home, data_dir) = isolated_home()?;
    let args = guide_argv(&[], &[], &data_dir);
    let (payload, argv) = run_guide(&home, &args)?;

    ensure(
        payload.get("status").and_then(Value::as_str) == Some("ok"),
        || "catalog status must be ok".to_string(),
    )?;
    ensure(
        payload.get("recommended_action").and_then(Value::as_str) == Some("select-an-intent"),
        || "catalog must recommend select-an-intent".to_string(),
    )?;
    ensure(
        payload
            .get("known_intents")
            .and_then(Value::as_array)
            .is_some_and(|a| a.len() >= 7),
        || "catalog must list >= 7 known intents".to_string(),
    )?;
    ensure(
        payload
            .pointer("/mutation_contract/read_only")
            .and_then(Value::as_bool)
            == Some(true),
        || "catalog must be read-only".to_string(),
    )?;
    // Read-only proof: no archive DB created.
    ensure(!data_dir.join("agent_search.db").exists(), || {
        "guide catalog must not create an archive DB".to_string()
    })?;
    log_scenario(&argv, &payload, "catalog ok");
    Ok(())
}

#[test]
fn guide_fix_ci_live_is_readonly_and_well_formed() -> TestResult {
    let (_tmp, home, data_dir) = isolated_home()?;
    let args = guide_argv(&["fix-ci"], &[], &data_dir);
    let (payload, argv) = run_guide(&home, &args)?;

    ensure(
        payload
            .pointer("/intent/recognized")
            .and_then(Value::as_bool)
            == Some(true),
        || "fix-ci must be recognized".to_string(),
    )?;
    ensure(
        payload.pointer("/plan/macro_id").and_then(Value::as_str) == Some("fix-ci-regression"),
        || "fix-ci must resolve to fix-ci-regression".to_string(),
    )?;
    ensure(
        payload
            .pointer("/plan/steps")
            .and_then(Value::as_array)
            .is_some_and(|s| !s.is_empty()),
        || "plan must have steps".to_string(),
    )?;
    ensure(
        payload
            .pointer("/plan/forbidden_shortcuts")
            .and_then(Value::as_array)
            .is_some_and(|f| !f.is_empty()),
        || "plan must enumerate forbidden shortcuts".to_string(),
    )?;
    check_read_only_contract(&payload)?;
    ensure(!data_dir.join("agent_search.db").exists(), || {
        "guide must not create an archive DB".to_string()
    })?;
    log_scenario(&argv, &payload, "fix-ci well-formed + read-only");
    Ok(())
}

#[test]
fn guide_unknown_intent_falls_back_to_docs() -> TestResult {
    let (_tmp, home, data_dir) = isolated_home()?;
    let args = guide_argv(&["teleport-the-database"], &[], &data_dir);
    let (payload, argv) = run_guide(&home, &args)?;

    ensure(
        payload.get("status").and_then(Value::as_str) == Some("warning"),
        || "unknown intent status must be warning".to_string(),
    )?;
    ensure(
        payload
            .pointer("/intent/recognized")
            .and_then(Value::as_bool)
            == Some(false),
        || "unknown intent must not be recognized".to_string(),
    )?;
    ensure(
        payload.pointer("/fallback/docs").and_then(Value::as_str) == Some("cass robot-docs guide"),
        || "unknown intent must point at robot-docs guide".to_string(),
    )?;
    ensure(
        payload
            .pointer("/mutation_contract/read_only")
            .and_then(Value::as_bool)
            == Some(true),
        || "unknown intent fallback must be read-only".to_string(),
    )?;
    log_scenario(&argv, &payload, "unknown intent → docs fallback");
    Ok(())
}

#[test]
fn guide_fixture_healthy_repair_is_ready() -> TestResult {
    let (_tmp, home, data_dir) = isolated_home()?;
    let (_fx, fixture) = facts_fixture(&[("db_present", true), ("disk_headroom_ok", true)])?;
    let args = guide_argv(
        &["repair-assets"],
        &["--fixture", &fixture.to_string_lossy()],
        &data_dir,
    );
    let (payload, argv) = run_guide(&home, &args)?;

    ensure(
        payload.get("readiness").and_then(Value::as_str) == Some("ready"),
        || {
            format!(
                "healthy repair-assets must be ready, got {:?}",
                payload.get("readiness")
            )
        },
    )?;
    ensure(
        payload.get("status").and_then(Value::as_str) == Some("ok"),
        || "healthy scenario status must be ok".to_string(),
    )?;
    ensure(
        payload.pointer("/plan/macro_id").and_then(Value::as_str) == Some("repair-derived-assets"),
        || "repair-assets must resolve to repair-derived-assets".to_string(),
    )?;
    check_read_only_contract(&payload)?;
    log_scenario(&argv, &payload, "healthy repair → ready");
    Ok(())
}

#[test]
fn guide_fixture_missing_index_blocks_search_miss() -> TestResult {
    let (_tmp, home, data_dir) = isolated_home()?;
    let (_fx, fixture) =
        facts_fixture(&[("index_present", false), ("search_assets_ready", false)])?;
    let args = guide_argv(
        &["investigate-search-miss"],
        &["--fixture", &fixture.to_string_lossy()],
        &data_dir,
    );
    let (payload, argv) = run_guide(&home, &args)?;

    ensure(
        payload.get("readiness").and_then(Value::as_str) == Some("blocked"),
        || {
            format!(
                "missing-index search-miss must be blocked, got {:?}",
                payload.get("readiness")
            )
        },
    )?;
    ensure(
        payload.get("status").and_then(Value::as_str) == Some("warning"),
        || "blocked scenario status must be warning".to_string(),
    )?;
    let unmet = payload
        .pointer("/plan/prerequisites")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter().any(|p| {
                p.get("fact").and_then(Value::as_str) == Some("index_present")
                    && p.get("status").and_then(Value::as_str) == Some("unmet")
            })
        })
        .unwrap_or(false);
    ensure(unmet, || "index_present must be reported unmet".to_string())?;
    check_read_only_contract(&payload)?;
    log_scenario(&argv, &payload, "missing index → blocked");
    Ok(())
}

#[test]
fn guide_export_session_is_sensitive_high_risk() -> TestResult {
    let (_tmp, home, data_dir) = isolated_home()?;
    let args = guide_argv(&["export-session"], &[], &data_dir);
    let (payload, argv) = run_guide(&home, &args)?;

    ensure(
        payload
            .pointer("/plan/privacy_tier")
            .and_then(Value::as_str)
            == Some("sensitive"),
        || "export-session must be sensitive tier".to_string(),
    )?;
    ensure(
        payload
            .pointer("/plan/cost_risk/risk_level")
            .and_then(Value::as_str)
            == Some("high"),
        || "export-session must be high risk".to_string(),
    )?;
    ensure(
        payload
            .pointer("/plan/privacy/preview_via")
            .and_then(Value::as_str)
            == Some("cass swarm privacy-preview --json"),
        || "export-session must point at the privacy-preview surface".to_string(),
    )?;
    check_read_only_contract(&payload)?;
    log_scenario(&argv, &payload, "export-session → sensitive/high-risk");
    Ok(())
}

#[test]
fn guide_release_and_onboard_intents_resolve() -> TestResult {
    let (_tmp, home, data_dir) = isolated_home()?;
    for (intent, macro_id) in [
        ("prepare-release", "prepare-release"),
        ("onboard-source", "onboard-source"),
    ] {
        let args = guide_argv(&[intent], &[], &data_dir);
        let (payload, argv) = run_guide(&home, &args)?;
        ensure(
            payload.pointer("/plan/macro_id").and_then(Value::as_str) == Some(macro_id),
            || format!("{intent} must resolve to {macro_id}"),
        )?;
        check_read_only_contract(&payload)?;
        log_scenario(&argv, &payload, "intent resolves");
    }
    Ok(())
}
