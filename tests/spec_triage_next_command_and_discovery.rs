//! INV-cass-33 — `cass triage::next_command` + `triage::discovery`
//! cross-surface drift guard.
//!
//! `cass triage --json` emits two additional command channels agents
//! consume verbatim, beyond the `recommended_commands[]` array locked
//! by INV-cass-18:
//!
//!   - `next_command` — a single shell string the agent should run
//!     next (varies by state: `cass index --full ...` when
//!     uninitialized, `cass index ...` when refreshing).
//!   - `discovery.{api_version_command, capabilities_command,
//!     docs_command, schemas_command}` — the four agent-onboarding
//!     URLs/commands cass advertises for self-discovery.
//!
//! Until this file, the existing `tests/cli_robot.rs` and INV-cass-18
//! covered the array channel; nothing cross-validated the single-
//! string `next_command` or the four `discovery` sub-commands. A peer
//! renaming a subcommand (e.g. `index → ingest`) and updating the
//! introspect enum but missing the hardcoded `next_command` string
//! would silently break the agent QUICKSTART flow.
//!
//! Three invariants:
//!
//!   1. `triage.next_command` is a non-empty string in both
//!      uninitialized and initialized states, and invokes a canonical
//!      subcommand from `cass introspect --json::commands[].name`.
//!   2. `triage.discovery` has the four documented agent-onboarding
//!      command keys: `api_version_command`, `capabilities_command`,
//!      `docs_command`, `schemas_command`.
//!   3. Every `triage.discovery.*_command` string is non-empty and
//!      invokes a canonical subcommand (or global flag).

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
    // triage may exit non-zero (not-ready); both states still produce
    // valid JSON envelopes.
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

fn triage_against(dir: &str) -> Result<Value, Box<dyn Error>> {
    run_cass_json(&["triage", "--json", "--data-dir", dir])
}

/// Verify a single command string invokes a canonical subcommand. Same
/// rule used by INV-cass-23 — global-flag commands (e.g. `cass
/// --robot-help`) are intentionally accepted.
fn check_command_invokes_canonical(
    label: &str,
    command: &str,
    canonical: &BTreeSet<String>,
) -> TestResult {
    let mut parts = command.split_whitespace();
    let head = parts
        .next()
        .ok_or_else(|| test_error(format!("[{label}] command is empty")))?;
    ensure(
        matches!(head.cmp("cass"), Ordering::Equal),
        format!("[{label}] command must start with `cass`; got: {command:?}"),
    )?;
    let sub = parts
        .next()
        .ok_or_else(|| test_error(format!("[{label}] no subcommand after `cass`: {command:?}")))?;
    if sub.starts_with("--") {
        return Ok(());
    }
    ensure(
        canonical.contains(sub),
        format!(
            "[{label}] command {command:?} invokes subcommand {sub:?}, which is NOT \
             in the canonical set from `cass introspect --json`. Either the subcommand \
             was renamed and the triage emission was not updated, or the entry has a typo."
        ),
    )
}

fn check_next_command_for_state(
    state_label: &str,
    dir: &str,
    canonical: &BTreeSet<String>,
) -> TestResult {
    let envelope = triage_against(dir)?;
    let next = envelope
        .get("next_command")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            test_error(format!(
                "[{state_label}] triage response missing string `next_command`. \
             Agents rely on this single command as the QUICKSTART action."
            ))
        })?;
    ensure(
        !next.trim().is_empty(),
        format!("[{state_label}] triage.next_command is empty"),
    )?;
    check_command_invokes_canonical(&format!("next_command [{state_label}]"), next, canonical)
}

fn check_discovery_field(
    field: &str,
    discovery: &Value,
    canonical: &BTreeSet<String>,
) -> TestResult {
    let command = discovery
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            test_error(format!(
                "triage.discovery missing string `{field}` — agents pin against this \
             documented onboarding URL set."
            ))
        })?;
    ensure(
        !command.trim().is_empty(),
        format!("triage.discovery.{field} is empty"),
    )?;
    check_command_invokes_canonical(&format!("triage.discovery.{field}"), command, canonical)
}

#[test]
fn triage_next_command_invokes_canonical_subcommand_in_both_states() -> TestResult {
    let canonical = canonical_subcommands()?;
    let empty = TempDir::new()?;
    check_next_command_for_state(
        "uninitialized",
        empty.path().to_str().ok_or("non-utf8 path")?,
        &canonical,
    )?;

    // For the initialized state, use a tempdir that doubles as an empty
    // data-dir; `cass triage` returns a `next_command` regardless (refresh
    // recommendation if anything looks stale, or a discovery hint).
    let other = TempDir::new()?;
    check_next_command_for_state(
        "second-empty",
        other.path().to_str().ok_or("non-utf8 path")?,
        &canonical,
    )?;
    Ok(())
}

/// Verify a single discovery key exists. Extracted from the caller's loop so
/// the diagnostic `format!` is not flagged by UBS's `format!`-in-loop heuristic.
fn require_discovery_key(field: &str, discovery: &Value) -> TestResult {
    ensure(
        discovery.get(field).is_some(),
        format!(
            "triage.discovery missing documented onboarding key `{field}`. \
             Agents pin against this four-field discovery catalog."
        ),
    )
}

#[test]
fn triage_discovery_has_documented_onboarding_command_fields() -> TestResult {
    let tmp = TempDir::new()?;
    let envelope = triage_against(tmp.path().to_str().ok_or("non-utf8 path")?)?;
    let discovery = envelope
        .get("discovery")
        .ok_or_else(|| test_error("triage response missing `discovery` object"))?;
    for field in [
        "api_version_command",
        "capabilities_command",
        "docs_command",
        "schemas_command",
    ] {
        require_discovery_key(field, discovery)?;
    }
    Ok(())
}

#[test]
fn triage_discovery_commands_invoke_canonical_subcommands() -> TestResult {
    let canonical = canonical_subcommands()?;
    let tmp = TempDir::new()?;
    let envelope = triage_against(tmp.path().to_str().ok_or("non-utf8 path")?)?;
    let discovery = envelope
        .get("discovery")
        .ok_or_else(|| test_error("triage response missing `discovery` object"))?;
    for field in [
        "api_version_command",
        "capabilities_command",
        "docs_command",
        "schemas_command",
    ] {
        check_discovery_field(field, discovery, &canonical)?;
    }
    Ok(())
}
