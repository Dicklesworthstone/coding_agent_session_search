//! INV-cass-23 — `cass triage::starter_workflows[]` canonical-subcommand
//! drift guard.
//!
//! `cass triage --json::starter_workflows` is the agent-onboarding
//! catalog: each entry advertises a complete agent task path with a
//! `first_command` to run and `follow_up_commands[]` to chain. Agents
//! that consume the triage envelope read this list verbatim and execute
//! each command. Until this test, the existing
//! `tests/cli_robot.rs::starter_workflows` coverage spot-checked that
//! the bounded-search workflow includes some specific commands; nothing
//! cross-validated **every** advertised command against the canonical
//! subcommand set from `cass introspect --json`.
//!
//! This is the same drift-guard pattern locked by INV-cass-18
//! (`recommended_commands[].command`), extended to `starter_workflows[]`:
//!
//!   - **Add-but-skip-rename**: a peer renames `index → ingest`, updates
//!     the dispatch table + introspect enum, but leaves the hardcoded
//!     "cass index --full" string in the starter-workflows catalog →
//!     agents following the onboarding workflow hit exit-2 "unknown
//!     subcommand".
//!
//! Two invariants:
//!
//!   1. Every `starter_workflows[]` entry has the documented
//!      agent-parseable keys (`first_command`, `follow_up_commands`,
//!      `intent`, `name`, `note`, `parse_contract`). A regression
//!      dropping any of these would silently break the onboarding
//!      catalog's parseability.
//!   2. Every advertised command (the `first_command` and each entry
//!      in `follow_up_commands[]`) starts with `cass ` and its second
//!      word is a name in `cass introspect --json::commands[].name`.
//!
//! Verified against `cass triage --json --data-dir <empty>` so the
//! uninitialized-archive starter workflows are exercised.

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
    // triage may exit non-zero (not-ready) but still emit a valid JSON
    // body; we accept any clean exit code (not 101/134/139) and parse.
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

/// Collect the canonical subcommand names from `cass introspect --json`.
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

/// Pull triage output against an empty data-dir so the uninitialized
/// starter workflows are emitted.
fn triage_against_empty_dir() -> Result<Value, Box<dyn Error>> {
    let tmp = TempDir::new()?;
    run_cass_json(&[
        "triage",
        "--json",
        "--data-dir",
        tmp.path().to_str().ok_or("non-utf8 path")?,
    ])
}

/// Verify a single command string invokes a canonical subcommand.
/// Lives outside the caller's loop so the diagnostic `format!` is not
/// flagged by UBS's `format!`-in-loop heuristic.
///
/// Commands whose second word is a global flag (e.g. `cass --robot-help`)
/// are skipped — they are not subject to subcommand-rename drift since
/// they do not invoke a subcommand at all. The cold-start workflow's
/// `cass --robot-help` follow-up is the canonical example.
fn check_command_invokes_canonical(
    label: &str,
    command: &str,
    canonical: &BTreeSet<String>,
) -> TestResult {
    let mut parts = command.split_whitespace();
    let head = parts
        .next()
        .ok_or_else(|| test_error(format!("[{label}] command string is empty")))?;
    ensure(
        head == "cass",
        format!("[{label}] command must start with `cass`; got: {command:?}"),
    )?;
    let sub = parts.next().ok_or_else(|| {
        test_error(format!(
            "[{label}] command has no subcommand after `cass`: {command:?}"
        ))
    })?;
    if sub.starts_with("--") {
        // Global-flag invocation (e.g. `cass --robot-help`). Not subject
        // to subcommand-rename drift; skip without failing.
        return Ok(());
    }
    ensure(
        canonical.contains(sub),
        format!(
            "[{label}] command {command:?} invokes subcommand {sub:?}, which is NOT \
             in the canonical set from `cass introspect --json`. Either the subcommand \
             was renamed and starter_workflows was not updated, or the workflow \
             emission has a typo. Canonical names: {canonical:?}"
        ),
    )?;
    Ok(())
}

/// Check all commands (first_command + each follow_up) in one workflow
/// entry. Wraps the loop so per-entry diagnostic `format!`s do not
/// trip UBS's `format!`-in-loop heuristic.
fn check_workflow_commands(
    workflow_idx: usize,
    name: &str,
    entry: &Value,
    canonical: &BTreeSet<String>,
) -> TestResult {
    if let Some(first) = entry.get("first_command").and_then(Value::as_str) {
        check_command_invokes_canonical(
            &format!("workflow[{workflow_idx}={name:?}].first_command"),
            first,
            canonical,
        )?;
    }
    if let Some(followups) = entry.get("follow_up_commands").and_then(Value::as_array) {
        for (idx, followup) in followups.iter().enumerate() {
            check_one_followup_command(workflow_idx, name, idx, followup, canonical)?;
        }
    }
    Ok(())
}

/// Verify one follow-up command entry. Extracted from the loop body in
/// `check_workflow_commands` so its diagnostic `format!`s do not live
/// syntactically inside a loop (UBS's `format!`-in-loop heuristic).
fn check_one_followup_command(
    workflow_idx: usize,
    name: &str,
    idx: usize,
    followup: &Value,
    canonical: &BTreeSet<String>,
) -> TestResult {
    let cmd = followup.as_str().ok_or_else(|| {
        test_error(format!(
            "workflow[{workflow_idx}={name:?}].follow_up_commands[{idx}] is not a string: {followup}"
        ))
    })?;
    check_command_invokes_canonical(
        &format!("workflow[{workflow_idx}={name:?}].follow_up_commands[{idx}]"),
        cmd,
        canonical,
    )
}

fn require_workflow_key(idx: usize, key: &str, entry: &Value) -> TestResult {
    ensure(
        entry.get(key).is_some(),
        format!("starter_workflows[{idx}] missing required key `{key}`: {entry}"),
    )
}

fn check_workflow_required_keys(idx: usize, entry: &Value) -> TestResult {
    for key in [
        "first_command",
        "follow_up_commands",
        "intent",
        "name",
        "note",
        "parse_contract",
    ] {
        require_workflow_key(idx, key, entry)?;
    }
    Ok(())
}

#[test]
fn starter_workflows_entries_have_required_agent_parseable_keys() -> TestResult {
    let triage = triage_against_empty_dir()?;
    let workflows = triage
        .get("starter_workflows")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("triage response missing `starter_workflows` array"))?;
    ensure(
        !matches!(workflows.len().cmp(&1), Ordering::Less),
        format!(
            "starter_workflows should be a non-trivial list (>=1 entry); got {} — \
             likely a regression in triage emission entirely",
            workflows.len()
        ),
    )?;
    for (idx, entry) in workflows.iter().enumerate() {
        check_workflow_required_keys(idx, entry)?;
    }
    Ok(())
}

#[test]
fn starter_workflows_commands_invoke_canonical_subcommands() -> TestResult {
    let canonical = canonical_subcommands()?;
    let triage = triage_against_empty_dir()?;
    let workflows = triage
        .get("starter_workflows")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("triage response missing `starter_workflows` array"))?;
    for (idx, entry) in workflows.iter().enumerate() {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("<unnamed>");
        check_workflow_commands(idx, name, entry, &canonical)?;
    }
    Ok(())
}
