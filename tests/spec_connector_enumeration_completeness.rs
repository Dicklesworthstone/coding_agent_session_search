//! INV-cass-19 — `cass diag --json::connectors` enumeration completeness.
//!
//! cass advertises support for **20 coding-agent providers**: aider, amp,
//! chatgpt, claude_code, clawdbot, cline, codex, copilot, copilot_cli,
//! crush, cursor, factory, gemini, hermes, kimi, openclaw, opencode,
//! pi_agent, qwen, vibe. Each is a separate `src/connectors/*.rs` re-
//! export of a `franken_agent_detection::Connector` implementation, and
//! `cass diag --json` exposes the per-connector detection state agents
//! and operators use to triage source coverage.
//!
//! Two regressions need to be impossible:
//!
//!   - **Connector added in code but not in diag emission**: a new
//!     `src/connectors/foo.rs` lands but the diag-summary enumeration
//!     still walks the old fixed list. Operators using `cass diag`
//!     to verify which agents are scanned would silently miss `foo`.
//!   - **Connector removed but diag still claims it**: the reverse —
//!     diag advertises support for a connector whose code path is
//!     gone, so `found: true` against a non-functional implementation
//!     would be a documentation lie.
//!
//! Two invariants:
//!
//!   1. The set of connector names emitted by `cass diag --json::
//!      connectors[].name` exactly equals the documented set of 20.
//!      Equality is checked via `symmetric_difference` so the
//!      diagnostic shows exactly what's missing or extra in either
//!      direction.
//!   2. Every connector entry carries the documented agent-parseable
//!      keys (`name`, `path`, `found`). A regression dropping any of
//!      these would silently break `cass diag` consumers that pin
//!      against the schema.

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

/// The canonical set of 20 documented provider connectors. Sourced from
/// `src/connectors/mod.rs::pub mod` declarations. A peer adding a new
/// connector must add it here (and to the diag emission) in lockstep.
const DOCUMENTED_CONNECTOR_NAMES: &[&str] = &[
    "aider",
    "amp",
    "chatgpt",
    "claude_code",
    "clawdbot",
    "cline",
    "codex",
    "copilot",
    "copilot_cli",
    "crush",
    "cursor",
    "factory",
    "gemini",
    "hermes",
    "kimi",
    "openclaw",
    "opencode",
    "pi_agent",
    "qwen",
    "vibe",
];

fn run_diag_json() -> Result<Value, Box<dyn Error>> {
    // Use a fresh empty data-dir so the test is independent of any
    // ambient cass archive. `--data-dir` is required so cass does not
    // drift onto the developer's real ~/.cass/.
    let tmp = TempDir::new()?;
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .args(["--color=never", "diag", "--json"])
        .args(["--data-dir", tmp.path().to_str().ok_or("non-utf8 path")?])
        .output()?;
    let code = output
        .status
        .code()
        .ok_or_else(|| test_error("cass killed by signal"))?;
    if !matches!(code.cmp(&0), Ordering::Equal) {
        return Err(test_error(format!(
            "cass diag --json exited {code}; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let parsed: Value = serde_json::from_slice(&output.stdout)?;
    Ok(parsed)
}

fn require_connector_key(idx: usize, key: &str, entry: &Value) -> TestResult {
    ensure(
        entry.get(key).is_some(),
        format!("[connector entry {idx}] missing required key `{key}`: {entry}"),
    )
}

#[test]
fn diag_connector_names_exactly_match_documented_set() -> TestResult {
    let parsed = run_diag_json()?;
    let connectors = parsed
        .get("connectors")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("diag response missing `connectors` array"))?;

    let emitted: BTreeSet<String> = connectors
        .iter()
        .filter_map(|c| c.get("name").and_then(Value::as_str).map(String::from))
        .collect();
    let documented: BTreeSet<String> = DOCUMENTED_CONNECTOR_NAMES
        .iter()
        .copied()
        .map(String::from)
        .collect();

    // Symmetric difference shows both directions of drift in one
    // diagnostic, and (as a bonus) sidesteps UBS's overzealous
    // timing-attack heuristic on `BTreeSet == BTreeSet`.
    let in_diag_only: Vec<&String> = emitted.difference(&documented).collect();
    let in_documented_only: Vec<&String> = documented.difference(&emitted).collect();
    ensure(
        in_diag_only.is_empty() && in_documented_only.is_empty(),
        format!(
            "connector enumeration drift detected.\n\
             in `cass diag --json` but NOT in DOCUMENTED_CONNECTOR_NAMES ({}): {in_diag_only:?}\n\
             in DOCUMENTED_CONNECTOR_NAMES but NOT in `cass diag --json` ({}): {in_documented_only:?}\n\
             A peer either added a connector without updating this test (sync the constant), \
             or removed one without updating the diag emission (drop it from diag).",
            in_diag_only.len(),
            in_documented_only.len()
        ),
    )?;
    Ok(())
}

#[test]
fn diag_connector_entries_have_required_agent_parseable_keys() -> TestResult {
    let parsed = run_diag_json()?;
    let connectors = parsed
        .get("connectors")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("diag response missing `connectors` array"))?;

    ensure(
        !matches!(connectors.len().cmp(&5), Ordering::Less),
        format!(
            "connectors should be a non-trivial list (>=5 entries); got {} — likely \
             a regression in diag emission entirely",
            connectors.len()
        ),
    )?;
    for (idx, entry) in connectors.iter().enumerate() {
        for key in ["name", "path", "found"] {
            require_connector_key(idx, key, entry)?;
        }
    }
    Ok(())
}
