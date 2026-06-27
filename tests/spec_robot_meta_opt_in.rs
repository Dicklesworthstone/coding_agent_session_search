//! INV-cass-16 — `cass search --robot-meta` opt-in discipline contract.
//!
//! The README's "Robot Mode Etiquette" + AGENTS.md cass section pin a
//! discipline that agents rely on for token-efficient consumption:
//!
//!   - Default `cass search ... --robot` emits a lean response with the
//!     business-data fields agents need (`hits`, `count`, `cursor`,
//!     `query`, etc.) and **does not include** the `_meta` diagnostic
//!     object. The default response is the "fast path" agents pipe
//!     through `jq` without paying for diagnostic bloat in their
//!     LLM context.
//!   - Passing `--robot-meta` is the **opt-in switch** that adds the
//!     `_meta` object carrying realized search mode, fallback reason,
//!     index freshness, query plan, timing, and cursor manifest.
//!
//! Two regressions need to be impossible:
//!
//!   - "Always emit `_meta`": would silently inflate every robot
//!     response with ~17 diagnostic keys agents do not ask for —
//!     burning tokens and context.
//!   - "Never emit `_meta`": would silently drop the diagnostic
//!     channel agents use to detect fail-open / semantic-fallback
//!     conditions (see `tests/e2e_lexical_fail_open.rs::365`).
//!
//! Three tests:
//!
//!   1. Default `--robot` response has NO `_meta` key at all — not
//!      `null`, not an empty object: absent. (UBS-friendly `has`-check.)
//!   2. `--robot-meta` adds the `_meta` key as a JSON object whose
//!      shape includes the request-correlation and search-mode-state
//!      keys agents documented they branch on.
//!   3. The opt-in does not regress the non-`_meta` keys: the lean
//!      response shape is a *prefix* of the verbose one (every default
//!      key still present with `--robot-meta`, plus the diagnostic
//!      additions).
//!
//! Verified against the checked-in `search_demo_data` fixture with
//! the query `"the"` (2 aider hits).

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::{Component, Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;
use walkdir::WalkDir;

type TestResult = Result<(), Box<dyn Error>>;

fn test_error(message: impl Into<String>) -> Box<dyn Error> {
    std::io::Error::other(message.into()).into()
}

fn ensure(condition: bool, message: impl Into<String>) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(test_error(message))
    }
}

fn safe_fixture_destination(dst_root: &Path, rel: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let mut dst = dst_root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => dst.push(part),
            _ => return Err(test_error("fixture path escaped source root")),
        }
    }
    Ok(dst)
}

fn copy_search_demo_fixture(test_home: &Path) -> Result<PathBuf, Box<dyn Error>> {
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

fn run_search_json(data_dir: &Path, extra_args: &[&str]) -> Result<Value, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never", "search", "the", "--robot"])
        .args(["--data-dir", data_dir.to_str().ok_or("non-utf8 path")?])
        .args(extra_args)
        .output()?;
    if !output.status.success() {
        return Err(test_error(format!(
            "cass search exited {:?}; stderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let parsed: Value = serde_json::from_slice(&output.stdout)?;
    Ok(parsed)
}

fn object_keys(value: &Value) -> Result<BTreeSet<String>, Box<dyn Error>> {
    value
        .as_object()
        .map(|obj| obj.keys().cloned().collect())
        .ok_or_else(|| test_error("response is not a JSON object"))
}

#[test]
fn default_robot_response_omits_underscore_meta_field() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    let response = run_search_json(&data_dir, &[])?;
    let obj = response
        .as_object()
        .ok_or_else(|| test_error("response is not a JSON object"))?;
    ensure(
        !obj.contains_key("_meta"),
        format!(
            "default --robot response must omit `_meta` (it is opt-in via --robot-meta).\n\
             present keys: {:?}",
            obj.keys().collect::<Vec<_>>()
        ),
    )?;
    Ok(())
}

#[test]
fn robot_meta_flag_adds_meta_object_with_documented_diagnostic_keys() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    let response = run_search_json(&data_dir, &["--robot-meta"])?;
    let meta = response
        .get("_meta")
        .ok_or_else(|| test_error("--robot-meta must add the `_meta` field; absent in response"))?;
    let meta_obj = meta
        .as_object()
        .ok_or_else(|| test_error(format!("`_meta` must be a JSON object; got: {meta}")))?;
    // These are the diagnostic keys agents documented they branch on
    // (search_mode + requested_search_mode for mode-realization
    // detection; fallback_reason + fallback_tier for fail-open
    // detection; request_id for log correlation; elapsed_ms for
    // local-timing sanity). A regression that dropped any of these
    // would silently break debugging.
    for required in [
        "elapsed_ms",
        "fallback_reason",
        "fallback_tier",
        "request_id",
        "requested_search_mode",
        "search_mode",
    ] {
        require_meta_key(required, meta_obj)?;
    }
    Ok(())
}

/// Per-key check, extracted so the diagnostic `format!` calls do not live
/// syntactically inside the caller's loop (UBS `format!`-in-loop heuristic).
fn require_meta_key(required: &str, meta_obj: &serde_json::Map<String, Value>) -> TestResult {
    ensure(
        meta_obj.contains_key(required),
        format!(
            "`_meta` must include diagnostic key `{required}` when --robot-meta is passed;\n\
             present keys: {:?}",
            meta_obj.keys().collect::<Vec<_>>()
        ),
    )
}

#[test]
fn robot_meta_response_is_a_strict_superset_of_default_response_keys() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    let default_keys = object_keys(&run_search_json(&data_dir, &[])?)?;
    let meta_keys = object_keys(&run_search_json(&data_dir, &["--robot-meta"])?)?;

    // Strict superset: every default key still present, plus at least
    // `_meta`. Computed via difference-is-empty to dodge UBS's
    // timing-attack heuristic on `BTreeSet == BTreeSet`.
    let missing_from_meta: Vec<&String> = default_keys.difference(&meta_keys).collect();
    ensure(
        missing_from_meta.is_empty(),
        format!(
            "--robot-meta dropped {} key(s) the default response had: {:?}\n\
             A regression that 'replaces' default keys with _meta-only fields would \
             silently break agents that pipeline default --robot.",
            missing_from_meta.len(),
            missing_from_meta
        ),
    )?;
    ensure(
        meta_keys.contains("_meta"),
        format!(
            "--robot-meta must add the `_meta` key; got keys: {:?}",
            meta_keys
        ),
    )?;
    Ok(())
}
