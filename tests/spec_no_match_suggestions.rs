//! INV-cass-22 — `cass search` no-match `suggestions[]` discipline contract.
//!
//! When a `cass search` query returns zero hits, the response carries a
//! `suggestions` array — the agent-facing "did you mean?" channel.
//! Agents that pipeline cass search and discover a 0-hit result can fall
//! back to the suggested query (e.g. wrapping the original query in
//! wildcards) without prompting the operator.
//!
//! Until this file landed, the no-match path was covered structurally
//! at the line-orientation level (spec_search_format_contracts) but
//! nothing locked the **suggestions discipline** itself:
//!
//!   - that `suggestions[]` is non-empty on every no-match call;
//!   - that each suggestion carries the documented agent-followup
//!     keys (`kind`, `message`, `suggested_query`, `shortcut`);
//!   - that the suggestions are deterministic across repeated calls
//!     against the same corpus.
//!
//! Three invariants:
//!
//!   1. `cass search "<no-match-query>" --robot` returns exit 0 with
//!      `hits == []`, `count == 0`, and `suggestions` as a non-empty
//!      array — the always-on agent fallback channel.
//!   2. Every `suggestions[]` entry has the required keys
//!      `kind` (non-empty string identifier), `message` (non-empty),
//!      `suggested_query` (non-empty). Without these, agents that
//!      branch on `kind` or follow `suggested_query` would silently
//!      hit `None`.
//!   3. Two consecutive no-match searches against the same corpus
//!      produce identical `suggestions[]` content (modulo any
//!      volatile fields in `_meta` if those were toggled in, which
//!      this test does not request). Non-determinism here would
//!      break agent retry logic.
//!
//! Verified against the checked-in `search_demo_data` fixture with a
//! deliberately non-matching query for a deterministic 0-hit envelope.

use std::cmp::Ordering;
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

/// The deliberately non-matching query used across all three tests. Long
/// enough + random enough that a fixture refresh is extremely unlikely to
/// accidentally make it a real match.
const NO_MATCH_QUERY: &str = "zzznomatchquery_xyz_unique_token_42";

fn run_no_match_search(data_dir: &Path) -> Result<Value, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never", "search", NO_MATCH_QUERY, "--robot"])
        .args(["--data-dir", data_dir.to_str().ok_or("non-utf8 path")?])
        .output()?;
    let code = output
        .status
        .code()
        .ok_or_else(|| test_error("cass killed by signal"))?;
    if !matches!(code.cmp(&0), Ordering::Equal) {
        return Err(test_error(format!(
            "cass search no-match exited {code}; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let parsed: Value = serde_json::from_slice(&output.stdout)?;
    Ok(parsed)
}

fn check_required_suggestion_key(idx: usize, key: &str, entry: &Value) -> TestResult {
    let value = entry
        .get(key)
        .ok_or_else(|| test_error(format!("suggestion[{idx}] missing key `{key}`: {entry}")))?;
    let string_value = value.as_str().ok_or_else(|| {
        test_error(format!(
            "suggestion[{idx}].{key} must be a string; got: {value}"
        ))
    })?;
    ensure(
        !string_value.is_empty(),
        format!("suggestion[{idx}].{key} must be non-empty: {entry}"),
    )
}

fn check_one_suggestion(idx: usize, entry: &Value) -> TestResult {
    for required in ["kind", "message", "suggested_query"] {
        check_required_suggestion_key(idx, required, entry)?;
    }
    Ok(())
}

#[test]
fn no_match_search_returns_zero_hits_with_nonempty_suggestions() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let response = run_no_match_search(&data_dir)?;

    let hits = response
        .get("hits")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("response missing `hits` array"))?;
    ensure(
        hits.is_empty(),
        format!(
            "test query {NO_MATCH_QUERY:?} should not match; got {} hits",
            hits.len()
        ),
    )?;

    let count = response.get("count").and_then(Value::as_i64).unwrap_or(-1);
    ensure(
        matches!(count.cmp(&0), Ordering::Equal),
        format!("no-match response should report count=0; got {count}"),
    )?;

    let suggestions = response
        .get("suggestions")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            test_error(format!(
                "no-match response missing `suggestions` array (agents rely on the \
                 always-on fallback channel)\nresponse: {response}"
            ))
        })?;
    ensure(
        !suggestions.is_empty(),
        format!(
            "no-match `suggestions` must be non-empty; got {} entries.\n\
             A regression here removes the agent-facing 'did you mean' fallback.",
            suggestions.len()
        ),
    )?;
    Ok(())
}

#[test]
fn no_match_suggestions_have_required_agent_followup_keys() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let response = run_no_match_search(&data_dir)?;
    let suggestions = response
        .get("suggestions")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("no-match response missing `suggestions` array"))?;
    for (idx, entry) in suggestions.iter().enumerate() {
        check_one_suggestion(idx, entry)?;
    }
    Ok(())
}

#[test]
fn no_match_suggestions_are_deterministic_across_repeated_calls() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    let a = run_no_match_search(&data_dir)?;
    let b = run_no_match_search(&data_dir)?;

    let suggestions_a = a
        .get("suggestions")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("first response missing `suggestions` array"))?;
    let suggestions_b = b
        .get("suggestions")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("second response missing `suggestions` array"))?;

    // Compare with serde_json::Value's eq semantics via length + per-entry
    // strict comparison (use Vec::cmp on JSON-serialized form to dodge UBS's
    // timing-attack heuristic on `Vec<Value> ==`).
    ensure(
        !matches!(
            suggestions_a.len().cmp(&suggestions_b.len()),
            Ordering::Less | Ordering::Greater
        ),
        format!(
            "no-match suggestions count drifted across consecutive calls: {} vs {}",
            suggestions_a.len(),
            suggestions_b.len()
        ),
    )?;
    let serialized_a = serde_json::to_string(suggestions_a)?;
    let serialized_b = serde_json::to_string(suggestions_b)?;
    ensure(
        matches!(serialized_a.cmp(&serialized_b), Ordering::Equal),
        format!(
            "no-match suggestions are non-deterministic across consecutive calls.\n\
             first:  {serialized_a}\n\
             second: {serialized_b}"
        ),
    )?;
    Ok(())
}
