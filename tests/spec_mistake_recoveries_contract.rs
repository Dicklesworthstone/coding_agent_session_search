//! INV-cass-24 — `cass triage::mistake_recoveries[]` discipline contract.
//!
//! `mistake_recoveries[]` is cass's documented catalog of forgiving-CLI
//! affordances: each entry advertises a `wrong` command that an agent
//! might mistakenly type and the `canonical` form it gets corrected to,
//! with `accepted: true` meaning the recovery actually fires. Agents
//! consume this list to validate their own command-construction code
//! against the published recovery set, and humans read it to learn the
//! canonical syntax.
//!
//! Existing `tests/cli_robot.rs::recommended_commands` coverage spot-
//! checks SPECIFIC entries (e.g. that `cass searh` → `cass search` is
//! advertised). Nothing locks:
//!
//!   - the EXACT required-key shape across every entry;
//!   - the invariant that every entry's `accepted == true` (the catalog
//!     is the set of ACCEPTED recoveries; any `false` entry is stale
//!     documentation);
//!   - the cross-surface drift guard that every `canonical` invokes a
//!     real subcommand from `cass introspect --json::commands[].name`
//!     (extending INV-cass-18 + INV-cass-23 to this third channel).
//!
//! Four invariants:
//!
//!   1. `mistake_recoveries[]` is non-empty (the affordance catalog
//!      exists; baseline observed: 47 entries).
//!   2. Every entry has required keys (`wrong`, `canonical`, `accepted`,
//!      `behavior`).
//!   3. Every `accepted` is `true`. A `false` entry is stale
//!      documentation: it advertises a recovery the parser does not
//!      actually accept.
//!   4. Every `canonical` command's second word is either a canonical
//!      subcommand from introspect, or a global flag (starts with `--`).
//!      Catches the same rename-drift class as INV-cass-18 + INV-cass-23.

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
    if matches!(code.cmp(&101), Ordering::Equal)
        || matches!(code.cmp(&134), Ordering::Equal)
        || matches!(code.cmp(&139), Ordering::Equal)
    {
        return Err(test_error(format!(
            "cass exited with panic-class code {code}; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let parsed: Value = serde_json::from_slice(&output.stdout)?;
    Ok(parsed)
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
            "introspect.commands should have >= 5 named subcommands; got {} names",
            names.len()
        ),
    )?;
    Ok(names)
}

fn triage_against_empty_dir() -> Result<Value, Box<dyn Error>> {
    let tmp = TempDir::new()?;
    run_cass_json(&[
        "triage",
        "--json",
        "--data-dir",
        tmp.path().to_str().ok_or("non-utf8 path")?,
    ])
}

fn mistake_recoveries(triage: &Value) -> Result<&Vec<Value>, Box<dyn Error>> {
    triage
        .get("mistake_recoveries")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("triage response missing `mistake_recoveries` array"))
}

fn require_entry_key(idx: usize, key: &str, entry: &Value) -> TestResult {
    ensure(
        entry.get(key).is_some(),
        format!("mistake_recoveries[{idx}] missing required key `{key}`: {entry}"),
    )
}

fn check_entry_required_keys(idx: usize, entry: &Value) -> TestResult {
    for key in ["wrong", "canonical", "accepted", "behavior"] {
        require_entry_key(idx, key, entry)?;
    }
    Ok(())
}

fn check_entry_accepted_is_true(idx: usize, entry: &Value) -> TestResult {
    let accepted = entry
        .get("accepted")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            test_error(format!(
                "mistake_recoveries[{idx}].accepted must be a bool; got: {entry}"
            ))
        })?;
    ensure(
        accepted,
        format!(
            "mistake_recoveries[{idx}].accepted is false — the catalog is the set of \
             ACCEPTED recoveries; a `false` entry is stale documentation advertising \
             a recovery the parser does not fire.\nentry: {entry}"
        ),
    )
}

fn check_canonical_invokes_subcommand_or_global_flag(
    idx: usize,
    entry: &Value,
    canonical_subs: &BTreeSet<String>,
) -> TestResult {
    let canonical = entry
        .get("canonical")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            test_error(format!(
                "mistake_recoveries[{idx}].canonical must be a string: {entry}"
            ))
        })?;
    let mut parts = canonical.split_whitespace();
    let head = parts.next().ok_or_else(|| {
        test_error(format!(
            "mistake_recoveries[{idx}].canonical is empty: {entry}"
        ))
    })?;
    ensure(
        head == "cass",
        format!("mistake_recoveries[{idx}].canonical must start with `cass`; got: {canonical:?}"),
    )?;
    let sub = parts.next().ok_or_else(|| {
        test_error(format!(
            "mistake_recoveries[{idx}].canonical has no second word: {canonical:?}"
        ))
    })?;
    // Global-flag invocations like `cass --robot-help` are valid canonical
    // forms that don't invoke a subcommand. Skip subcommand validation
    // for those (same exception as INV-cass-23).
    if sub.starts_with("--") {
        return Ok(());
    }
    ensure(
        canonical_subs.contains(sub),
        format!(
            "mistake_recoveries[{idx}].canonical {canonical:?} invokes subcommand \
             {sub:?}, which is NOT in the canonical set from `cass introspect --json`. \
             Either the subcommand was renamed and mistake_recoveries was not updated, \
             or the entry has a typo. Canonical names: {canonical_subs:?}"
        ),
    )
}

#[test]
fn mistake_recoveries_array_is_nonempty() -> TestResult {
    let triage = triage_against_empty_dir()?;
    let recoveries = mistake_recoveries(&triage)?;
    ensure(
        !recoveries.is_empty(),
        format!(
            "mistake_recoveries should be a non-trivial list; got {} entries — \
             likely a regression in triage emission entirely",
            recoveries.len()
        ),
    )?;
    Ok(())
}

#[test]
fn mistake_recoveries_entries_have_required_keys() -> TestResult {
    let triage = triage_against_empty_dir()?;
    let recoveries = mistake_recoveries(&triage)?;
    for (idx, entry) in recoveries.iter().enumerate() {
        check_entry_required_keys(idx, entry)?;
    }
    Ok(())
}

#[test]
fn every_mistake_recovery_entry_advertises_accepted_true() -> TestResult {
    let triage = triage_against_empty_dir()?;
    let recoveries = mistake_recoveries(&triage)?;
    for (idx, entry) in recoveries.iter().enumerate() {
        check_entry_accepted_is_true(idx, entry)?;
    }
    Ok(())
}

#[test]
fn every_canonical_form_invokes_a_canonical_subcommand_or_global_flag() -> TestResult {
    let canonical_subs = canonical_subcommands()?;
    let triage = triage_against_empty_dir()?;
    let recoveries = mistake_recoveries(&triage)?;
    for (idx, entry) in recoveries.iter().enumerate() {
        check_canonical_invokes_subcommand_or_global_flag(idx, entry, &canonical_subs)?;
    }
    Ok(())
}
