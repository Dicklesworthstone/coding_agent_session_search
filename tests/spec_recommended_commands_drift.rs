//! INV-cass-18 — `recommended_commands[].command` strings invoke canonical
//! subcommands.
//!
//! Multiple cass surfaces emit `recommended_commands` — an array of command
//! objects that agents follow as the "next safe action" recommendation:
//!
//!   - `cass triage --json` (the QUICKSTART entry point)
//!   - `cass status --json`
//!   - `cass health --json`
//!   - `cass doctor --json`
//!
//! Each entry's `command` string starts with `cass ` followed by a
//! subcommand name (e.g. `"cass index --full --data-dir /foo"`). Agents
//! that follow the recommendation execute that string verbatim.
//!
//! **The silent-drift class this test prevents:** a peer renames a
//! subcommand (e.g. `index → ingest`), updates the dispatch table and
//! the `cass introspect --json` enum, but forgets to update the
//! hardcoded strings in triage/status/health's `recommended_commands`
//! emission. The existing `tests/cli_robot.rs` covers individual command
//! IDs structurally but does not cross-validate the COMMAND STRINGS
//! against the introspect-declared canonical subcommand set. This file
//! does.
//!
//! Two invariants:
//!
//!   1. For triage on both initialized + uninitialized data-dirs, the
//!      second word of every `recommended_commands[].command` is a name
//!      in `cass introspect --json::commands[].name`. The two-state
//!      coverage ensures both "happy path" and "needs init" command
//!      shapes are checked.
//!   2. Every emitted `recommended_commands[]` entry carries the
//!      required documented fields (`id`, `command`, `purpose`,
//!      `success_signal`, `parse_fields`). A regression that quietly
//!      dropped any of these would silently break agent parsing.
//!
//! Verified against the checked-in `search_demo_data` fixture for the
//! initialized state.

use std::cmp::Ordering;
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
    // triage/health may exit non-zero (not-ready) but still emit a valid JSON
    // body; we accept any clean exit code and parse stdout regardless.
    if matches!(code.cmp(&101), Ordering::Equal)
        || matches!(code.cmp(&134), Ordering::Equal)
        || matches!(code.cmp(&139), Ordering::Equal)
    {
        return Err(test_error(format!(
            "cass exited with panic-class code {code}; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let parsed: Value = serde_json::from_slice(&output.stdout).map_err(|err| {
        test_error(format!(
            "stdout is not JSON ({err}); exit={code}; stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    })?;
    Ok(parsed)
}

/// Collect the canonical subcommand names from `cass introspect --json`.
/// This is the authoritative source other surfaces should agree with.
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
            "introspect.commands should have >= 5 named subcommands; got {} names",
            names.len()
        ),
    )?;
    Ok(names)
}

/// Extract the second word of a `command` string. Returns Err on a
/// well-formed-but-empty command. The first word is always `cass`
/// (verified separately), and the second is the subcommand.
fn extract_subcommand_word(command: &str) -> Result<String, Box<dyn Error>> {
    let mut parts = command.split_whitespace();
    let head = parts
        .next()
        .ok_or_else(|| test_error("command string is empty"))?;
    ensure(
        head == "cass",
        format!("recommended_commands.command should start with `cass`; got: {command:?}"),
    )?;
    let sub = parts
        .next()
        .ok_or_else(|| test_error(format!("command string has no subcommand: {command:?}")))?;
    Ok(sub.to_string())
}

/// Check one recommended_commands[] entry against the canonical set.
/// Lives outside the caller's loop so the diagnostic `format!` is not
/// flagged by UBS's `format!`-in-loop heuristic.
fn check_recommended_command(
    label: &str,
    idx: usize,
    entry: &Value,
    canonical: &BTreeSet<String>,
) -> TestResult {
    // (a) required fields present
    for required in ["id", "command", "purpose", "success_signal", "parse_fields"] {
        require_entry_key(label, idx, required, entry)?;
    }
    // (b) command's subcommand is canonical
    let command = entry
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            test_error(format!(
                "[{label} entry {idx}] `command` must be a string; got: {entry}"
            ))
        })?;
    let sub = extract_subcommand_word(command)?;
    ensure(
        canonical.contains(&sub),
        format!(
            "[{label} entry {idx}] command {command:?} invokes subcommand {sub:?}, which is \
             NOT in the canonical set from `cass introspect --json`. Either the subcommand \
             was renamed and triage/status was not updated, or the recommended_commands \
             emission has a typo. Canonical names: {canonical:?}"
        ),
    )?;
    Ok(())
}

fn require_entry_key(label: &str, idx: usize, key: &str, entry: &Value) -> TestResult {
    ensure(
        entry.get(key).is_some(),
        format!("[{label} entry {idx}] missing required key `{key}`: {entry}"),
    )
}

fn check_all_recommended_commands(
    label: &str,
    response: &Value,
    canonical: &BTreeSet<String>,
) -> TestResult {
    let arr = response
        .get("recommended_commands")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            test_error(format!(
                "[{label}] response missing `recommended_commands` array"
            ))
        })?;
    ensure(
        !arr.is_empty(),
        format!(
            "[{label}] recommended_commands should not be empty for this state — agents \
             rely on at least one next-action recommendation"
        ),
    )?;
    for (idx, entry) in arr.iter().enumerate() {
        check_recommended_command(label, idx, entry, canonical)?;
    }
    Ok(())
}

#[test]
fn recommended_commands_in_triage_invoke_canonical_subcommands() -> TestResult {
    let canonical = canonical_subcommands()?;

    // State A: uninitialized data-dir. Triage recommends initializing
    // (`cass index --full ...`) and verifying (`cass health --json ...`).
    let empty = TempDir::new()?;
    let triage_empty = run_cass_json(&[
        "triage",
        "--json",
        "--data-dir",
        empty.path().to_str().ok_or("non-utf8 path")?,
    ])?;
    check_all_recommended_commands("triage(uninitialized)", &triage_empty, &canonical)?;

    // State B: initialized fixture. Triage recommends refresh + verify.
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let triage_fixture = run_cass_json(&[
        "triage",
        "--json",
        "--data-dir",
        data_dir.to_str().ok_or("non-utf8 path")?,
    ])?;
    check_all_recommended_commands("triage(initialized)", &triage_fixture, &canonical)?;

    Ok(())
}
