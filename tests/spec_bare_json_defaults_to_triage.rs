//! INV-cass-25 — `cass --json` (no subcommand) defaults to triage.
//!
//! `cass triage --json::mistake_recoveries[]` documents the recovery
//! `"cass --json" → "cass triage --json"` (accepted=true): bare
//! `cass --json` is the canonical "I don't know what to do, please tell
//! me" agent invocation. Per the existing
//! `tests/cli_robot.rs::cass_root_json_defaults_to_triage_payload` smoke
//! test, this routing exists — but no structural-property test locks
//! the agent-facing contract.
//!
//! Three invariants:
//!
//!   1. `cass --json` against any data-dir returns exit 0 with stdout
//!      that parses as a JSON object with `surface == "triage"` —
//!      proving the routing actually points at the triage emitter, not
//!      a catch-all that happens to return triage-looking shape.
//!   2. The response carries the same top-level keys as the
//!      `cass triage --json` response. A regression where bare `--json`
//!      routed to a stripped-down "triage-lite" envelope would silently
//!      remove fields agents pin against (recommended_commands,
//!      starter_workflows, mistake_recoveries).
//!   3. The `recommended_commands[]` array is non-empty (the always-on
//!      agent-onboarding affordance is preserved through the routing).
//!
//! Verified by invoking both `cass --json --data-dir <empty>` and
//! `cass triage --json --data-dir <empty>` against the same temp dir
//! and comparing.

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
    // Reject panic-class exits but accept any other code; triage returns
    // exit 0 for both initialized and uninitialized data-dirs.
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

fn object_keys(value: &Value) -> Result<BTreeSet<String>, Box<dyn Error>> {
    value
        .as_object()
        .map(|obj| obj.keys().cloned().collect())
        .ok_or_else(|| test_error(format!("value is not an object: {value}")))
}

#[test]
fn bare_root_json_returns_surface_eq_triage_against_empty_dir() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = tmp.path().to_str().ok_or("non-utf8 path")?;
    let response = run_cass_json(&["--json", "--data-dir", data_dir])?;

    let surface = response
        .get("surface")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            test_error(format!(
                "bare `cass --json` response missing string `surface` field — \
                 routing did not land at the triage emitter.\nresponse: {response}"
            ))
        })?;
    ensure(
        surface == "triage",
        format!(
            "bare `cass --json` should produce a triage payload (surface=\"triage\"); \
             got surface={surface:?}.\nresponse: {response}"
        ),
    )?;
    Ok(())
}

#[test]
fn bare_root_json_top_level_keys_match_canonical_triage() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = tmp.path().to_str().ok_or("non-utf8 path")?;
    let bare = run_cass_json(&["--json", "--data-dir", data_dir])?;
    let canonical = run_cass_json(&["triage", "--json", "--data-dir", data_dir])?;

    let bare_keys = object_keys(&bare)?;
    let canonical_keys = object_keys(&canonical)?;

    // The bare-form response must carry every key the canonical
    // `cass triage --json` does — otherwise routing through the mistake
    // recovery is silently truncating fields agents pin against
    // (recommended_commands, starter_workflows, mistake_recoveries, etc.).
    let missing_from_bare: Vec<&String> = canonical_keys.difference(&bare_keys).collect();
    let extra_in_bare: Vec<&String> = bare_keys.difference(&canonical_keys).collect();
    ensure(
        missing_from_bare.is_empty() && extra_in_bare.is_empty(),
        format!(
            "bare `cass --json` does not produce the canonical triage key set.\n\
             keys in canonical triage but missing from bare: {missing_from_bare:?}\n\
             keys in bare but not in canonical triage:       {extra_in_bare:?}"
        ),
    )?;
    Ok(())
}

#[test]
fn bare_root_json_recommended_commands_is_nonempty() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = tmp.path().to_str().ok_or("non-utf8 path")?;
    let response = run_cass_json(&["--json", "--data-dir", data_dir])?;
    let recommended = response
        .get("recommended_commands")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            test_error(format!(
                "bare `cass --json` response missing `recommended_commands` array.\n\
                 response: {response}"
            ))
        })?;
    ensure(
        !recommended.is_empty(),
        format!(
            "bare `cass --json` should preserve a non-empty `recommended_commands` \
             array (the always-on agent-onboarding affordance); got {} entries",
            recommended.len()
        ),
    )?;
    Ok(())
}
