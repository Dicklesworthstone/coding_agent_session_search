//! INV-cass-28 — `cass introspect --json` schema integrity.
//!
//! `cass introspect --json` is the authoritative agent-facing
//! subcommand+arguments catalog. Other surface tests
//! (INV-cass-7 / INV-cass-18 / INV-cass-23 / INV-cass-24) cross-
//! validate other channels' command strings against
//! `introspect.commands[].name` as the source of truth. This file
//! locks the structural integrity of introspect itself so that
//! source-of-truth never silently degrades into a half-shape under
//! a refactor.
//!
//! Three invariants:
//!
//!   1. The introspect envelope has the documented top-level keys
//!      (`api_version`, `commands`, `contract_version`, `global_flags`,
//!      `response_schemas`). Drift detection on the catalog's outer
//!      shape.
//!   2. Every `commands[]` entry has a non-empty `name` and
//!      non-empty `description`. A regression that omitted either
//!      would silently break agents that pin help-text generation
//!      on the introspect envelope.
//!   3. Every `commands[].arguments[]` entry has the documented
//!      required keys (`arg_type`, `description`, `name`, `required`)
//!      with non-empty `name`. The ~340-entry argument catalog is
//!      the most-read introspect surface; structural drift here
//!      silently breaks the entire agent help-text pipeline.

use std::cmp::Ordering;
use std::error::Error;

use assert_cmd::Command;
use serde_json::Value;

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

fn run_introspect_json() -> Result<Value, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never", "introspect", "--json"])
        .output()?;
    let code = output
        .status
        .code()
        .ok_or_else(|| test_error("cass introspect killed by signal"))?;
    if !matches!(code.cmp(&0), Ordering::Equal) {
        return Err(test_error(format!(
            "cass introspect --json exited {code}; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn require_top_level_key(envelope: &Value, key: &str) -> TestResult {
    ensure(
        envelope.get(key).is_some(),
        format!(
            "introspect envelope missing top-level key `{key}`: keys present = {:?}",
            envelope
                .as_object()
                .map(|o| o.keys().collect::<Vec<_>>())
                .unwrap_or_default()
        ),
    )
}

fn check_command_name_and_description(idx: usize, cmd: &Value) -> TestResult {
    let name = cmd
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error(format!("commands[{idx}] missing string `name`: {cmd}")))?;
    ensure(
        !name.is_empty(),
        format!("commands[{idx}].name is empty: {cmd}"),
    )?;
    let description = cmd
        .get("description")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            test_error(format!(
                "commands[{idx}={name:?}] missing string `description`: {cmd}"
            ))
        })?;
    ensure(
        !description.trim().is_empty(),
        format!(
            "commands[{idx}={name:?}].description is empty — agents pinning help-text \
             generation on introspect would silently emit blank text for this command"
        ),
    )?;
    Ok(())
}

fn require_argument_key(
    cmd_idx: usize,
    cmd_name: &str,
    arg_idx: usize,
    arg: &Value,
    key: &str,
) -> TestResult {
    ensure(
        arg.get(key).is_some(),
        format!(
            "commands[{cmd_idx}={cmd_name:?}].arguments[{arg_idx}] missing required key \
             `{key}`: {arg}"
        ),
    )
}

fn check_argument_required_keys(
    cmd_idx: usize,
    cmd_name: &str,
    arg_idx: usize,
    arg: &Value,
) -> TestResult {
    for key in ["arg_type", "description", "name", "required"] {
        require_argument_key(cmd_idx, cmd_name, arg_idx, arg, key)?;
    }
    let arg_name = arg.get("name").and_then(Value::as_str).unwrap_or_default();
    ensure(
        !arg_name.is_empty(),
        format!("commands[{cmd_idx}={cmd_name:?}].arguments[{arg_idx}].name is empty: {arg}"),
    )?;
    Ok(())
}

fn check_one_command_arguments(cmd_idx: usize, cmd_name: &str, cmd: &Value) -> TestResult {
    let args = cmd
        .get("arguments")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            test_error(format!(
                "commands[{cmd_idx}={cmd_name:?}] missing `arguments` array: {cmd}"
            ))
        })?;
    for (arg_idx, arg) in args.iter().enumerate() {
        check_argument_required_keys(cmd_idx, cmd_name, arg_idx, arg)?;
    }
    Ok(())
}

#[test]
fn introspect_envelope_has_documented_top_level_keys() -> TestResult {
    let envelope = run_introspect_json()?;
    for key in [
        "api_version",
        "commands",
        "contract_version",
        "global_flags",
        "response_schemas",
    ] {
        require_top_level_key(&envelope, key)?;
    }
    Ok(())
}

#[test]
fn every_introspect_command_has_nonempty_name_and_description() -> TestResult {
    let envelope = run_introspect_json()?;
    let commands = envelope
        .get("commands")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("introspect.commands missing or not an array"))?;
    ensure(
        !matches!(commands.len().cmp(&5), Ordering::Less),
        format!(
            "introspect.commands should have >= 5 entries; got {} — likely a regression \
             in the catalog emission entirely",
            commands.len()
        ),
    )?;
    for (idx, cmd) in commands.iter().enumerate() {
        check_command_name_and_description(idx, cmd)?;
    }
    Ok(())
}

#[test]
fn every_introspect_command_argument_has_required_shape() -> TestResult {
    let envelope = run_introspect_json()?;
    let commands = envelope
        .get("commands")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("introspect.commands missing or not an array"))?;
    for (cmd_idx, cmd) in commands.iter().enumerate() {
        let cmd_name = cmd
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("<unnamed>");
        check_one_command_arguments(cmd_idx, cmd_name, cmd)?;
    }
    Ok(())
}
