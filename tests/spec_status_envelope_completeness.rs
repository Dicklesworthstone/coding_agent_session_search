//! INV-cass-31 — `cass status --json` envelope completeness.
//!
//! Per AGENTS.md "Search Asset Contract":
//! > Truth surfaces: `cass health --json`, `cass status --json`, and
//! > search `--robot-meta` expose readiness, active rebuilds, realized
//! > search mode, fallback tier, and recommended action. Follow those
//! > fields instead of hard-coded manual repair rituals.
//!
//! Agents pin against status's stable key set for state-machine
//! decisions. Until this file, the existing `tests/cli_status.rs`
//! covers per-state behavior (exit codes, specific field values) but
//! does not lock the **top-level key set** that agents consume as a
//! whole, nor does it cross-validate `status.recommended_commands[]`
//! against the canonical subcommand set the way INV-cass-18 does for
//! `cass triage`.
//!
//! Three invariants:
//!
//!   1. status's top-level key set is **stable across uninitialized
//!      and initialized states**. A regression where one state silently
//!      drops a top-level key would break agents that read the same
//!      field unconditionally across both states. Set equality via
//!      `symmetric_difference` dodges UBS's `==` heuristic AND produces
//!      a both-directions diagnostic.
//!   2. Core agent-facing keys (`status`, `healthy`, `recommended_action`,
//!      `data_dir`, `recommended_commands`) are always present. These
//!      are the load-bearing fields per AGENTS.md.
//!   3. Every `status.recommended_commands[].command` invokes a
//!      canonical subcommand from `cass introspect --json::
//!      commands[].name` (or a global flag). Extends INV-cass-18's
//!      cross-surface drift guard from triage's emission to status's,
//!      completing coverage of the third truth-surface.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::error::Error;

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

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

fn run_cass_json(args: &[&str]) -> Result<Value, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never"])
        .args(args)
        .output()?;
    let code = output
        .status
        .code()
        .ok_or_else(|| test_error("cass killed by signal"))?;
    // status may exit non-zero (not-ready); both states produce valid
    // JSON envelopes. Reject only panic-class exits.
    if matches!(code.cmp(&101), Ordering::Equal)
        || matches!(code.cmp(&134), Ordering::Equal)
        || matches!(code.cmp(&139), Ordering::Equal)
    {
        return Err(test_error(format!(
            "cass exited with panic-class code {code}; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn object_keys(value: &Value) -> Result<BTreeSet<String>, Box<dyn Error>> {
    value
        .as_object()
        .map(|obj| obj.keys().cloned().collect())
        .ok_or_else(|| test_error(format!("value is not an object: {value}")))
}

fn canonical_subcommands() -> Result<BTreeSet<String>, Box<dyn Error>> {
    let parsed = run_cass_json(&["introspect", "--json"])?;
    let commands = parsed
        .get("commands")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("introspect.commands missing or not an array"))?;
    let names: BTreeSet<String> = commands
        .iter()
        .filter_map(|cmd| cmd.get("name").and_then(Value::as_str).map(String::from))
        .collect();
    ensure(
        !matches!(names.len().cmp(&5), Ordering::Less),
        format!(
            "introspect.commands should have >= 5 names; got {}",
            names.len()
        ),
    )?;
    Ok(names)
}

fn status_against_dir(dir: &str) -> Result<Value, Box<dyn Error>> {
    run_cass_json(&["status", "--json", "--data-dir", dir])
}

fn check_recommended_command(
    idx: usize,
    entry: &Value,
    canonical: &BTreeSet<String>,
) -> TestResult {
    let command = entry
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            test_error(format!(
                "status.recommended_commands[{idx}].command must be a string: {entry}"
            ))
        })?;
    let mut parts = command.split_whitespace();
    let head = parts
        .next()
        .ok_or_else(|| test_error(format!("[entry {idx}] command is empty")))?;
    ensure(
        matches!(head.cmp("cass"), Ordering::Equal),
        format!("[entry {idx}] command must start with `cass`; got: {command:?}"),
    )?;
    let sub = parts.next().ok_or_else(|| {
        test_error(format!(
            "[entry {idx}] command has no subcommand: {command:?}"
        ))
    })?;
    if sub.starts_with("--") {
        return Ok(());
    }
    ensure(
        canonical.contains(sub),
        format!(
            "[entry {idx}] command {command:?} invokes subcommand {sub:?}, which is NOT \
             in the canonical introspect set. Either the subcommand was renamed and status \
             emission was not updated, or the entry has a typo. Canonical: {canonical:?}"
        ),
    )?;
    Ok(())
}

#[test]
fn status_top_level_keys_stable_across_initialized_and_uninitialized_states() -> TestResult {
    let empty = TempDir::new()?;
    let uninitialized = status_against_dir(empty.path().to_str().ok_or("non-utf8 path")?)?;

    let fixture = TempDir::new()?;
    let demo_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("search_demo_data");
    let dst_demo = fixture.path().join("demo");
    copy_dir_recursive(&demo_path, &dst_demo)?;
    let initialized = status_against_dir(dst_demo.to_str().ok_or("non-utf8 path")?)?;

    let keys_empty = object_keys(&uninitialized)?;
    let keys_demo = object_keys(&initialized)?;

    // Set equality via symmetric_difference; dodges UBS heuristic AND
    // produces a diagnostic naming both directions of drift.
    let only_in_empty: Vec<&String> = keys_empty.difference(&keys_demo).collect();
    let only_in_demo: Vec<&String> = keys_demo.difference(&keys_empty).collect();
    ensure(
        only_in_empty.is_empty() && only_in_demo.is_empty(),
        format!(
            "status top-level key set drifts between uninitialized and initialized states.\n\
             only-in-empty ({}): {only_in_empty:?}\n\
             only-in-demo  ({}): {only_in_demo:?}\n\
             A regression where one state silently drops a top-level key would break \
             agents that read the same field unconditionally across both states.",
            only_in_empty.len(),
            only_in_demo.len()
        ),
    )?;
    Ok(())
}

#[test]
fn status_envelope_always_carries_core_agent_facing_keys() -> TestResult {
    let empty = TempDir::new()?;
    let envelope = status_against_dir(empty.path().to_str().ok_or("non-utf8 path")?)?;
    let obj = envelope
        .as_object()
        .ok_or_else(|| test_error("status response is not an object"))?;
    for required in [
        "status",
        "healthy",
        "recommended_action",
        "data_dir",
        "recommended_commands",
    ] {
        check_status_core_key(required, obj)?;
    }
    Ok(())
}

fn check_status_core_key(key: &str, obj: &serde_json::Map<String, Value>) -> TestResult {
    ensure(
        obj.contains_key(key),
        format!(
            "status envelope missing core agent-facing key `{key}`. \
             Per AGENTS.md, status is a truth surface — agents pin on these fields.\n\
             present keys: {:?}",
            obj.keys().collect::<Vec<_>>()
        ),
    )
}

#[test]
fn status_recommended_commands_invoke_canonical_subcommands() -> TestResult {
    let canonical = canonical_subcommands()?;
    let empty = TempDir::new()?;
    let envelope = status_against_dir(empty.path().to_str().ok_or("non-utf8 path")?)?;
    let arr = envelope
        .get("recommended_commands")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("status response missing `recommended_commands` array"))?;
    ensure(
        !arr.is_empty(),
        "status.recommended_commands should not be empty — agents rely on at least \
         one next-action recommendation per truth-surface emission",
    )?;
    for (idx, entry) in arr.iter().enumerate() {
        check_recommended_command(idx, entry, &canonical)?;
    }
    Ok(())
}

use std::fs;
use std::path::{Path, PathBuf};

/// Recursive copy via walkdir-free std::fs. Used by the cross-state test
/// to materialize the search_demo_data fixture into a tempdir for the
/// initialized-state status call.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
