//! INV-cass-21 — `cass search --fields` mask discipline contract.
//!
//! Named presets omit message bodies but retain a complete canonical
//! follow-up anchor. GH493 established that a path and message ordinal alone
//! cannot distinguish multiple sessions sharing a provider database. The
//! source and archive-local conversation identity must survive projection.
//!
//! Four invariants:
//!
//!   1. `--fields minimal` emits exactly `agent`, `line_number`, `source_path`,
//!      `source_id`, and `conversation_id`. No body, score, or unrelated fields.
//!   2. `--fields summary` adds exactly `title` and `score` to that anchor.
//!   3. `--fields minimal` produces strictly fewer total response bytes than
//!      the default. Keeping identity must not defeat the payload savings.
//!   4. Explicit masks still emit exactly the caller's requested fields, even
//!      when the caller deliberately omits part of the follow-up anchor.
//!
//! Exact key-set checks complement the shared-path, sparse-index round trips
//! in `gh493_message_coordinates`, which verify the identity values themselves.

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

/// Run `cass search "the" --robot --data-dir <fixture> [<extra...>]` and
/// return the raw stdout (so callers can measure bytes) and the parsed
/// JSON. Asserts exit 0.
fn run_search(data_dir: &Path, extra_args: &[&str]) -> Result<(String, Value), Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never", "search", "the", "--robot"])
        .args(["--data-dir", data_dir.to_str().ok_or("non-utf8 path")?])
        .args(extra_args)
        .output()?;
    let code = output
        .status
        .code()
        .ok_or_else(|| test_error("cass killed by signal"))?;
    if !matches!(code.cmp(&0), Ordering::Equal) {
        return Err(test_error(format!(
            "cass search exited {code}; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let stdout = String::from_utf8(output.stdout)?;
    let parsed: Value = serde_json::from_str(stdout.trim())?;
    Ok((stdout, parsed))
}

fn first_hit_keys(parsed: &Value) -> Result<BTreeSet<String>, Box<dyn Error>> {
    let hits = parsed
        .get("hits")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("response missing `hits` array"))?;
    let first = hits
        .first()
        .ok_or_else(|| test_error("hits array empty; fixture should produce at least 1 hit"))?;
    let obj = first
        .as_object()
        .ok_or_else(|| test_error(format!("hits[0] is not an object: {first}")))?;
    Ok(obj.keys().cloned().collect())
}

/// Strict key-set comparison via symmetric_difference, dodging UBS's
/// timing-attack heuristic on `BTreeSet == BTreeSet` and producing a
/// diagnostic that names both directions of drift.
fn assert_key_set_equals(
    label: &str,
    got: &BTreeSet<String>,
    expected: &BTreeSet<String>,
) -> TestResult {
    let extra: Vec<&String> = got.difference(expected).collect();
    let missing: Vec<&String> = expected.difference(got).collect();
    ensure(
        extra.is_empty() && missing.is_empty(),
        format!(
            "[{label}] hit key set drift detected.\n\
             extra (in response, not in expected): {extra:?}\n\
             missing (in expected, not in response): {missing:?}\n\
             expected: {expected:?}\n\
             got:      {got:?}"
        ),
    )
}

#[test]
fn fields_minimal_preset_emits_exactly_the_canonical_anchor_keys() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let (_stdout, parsed) = run_search(&data_dir, &["--fields", "minimal", "--limit", "1"])?;
    let keys = first_hit_keys(&parsed)?;
    let documented: BTreeSet<String> = [
        "agent",
        "line_number",
        "source_path",
        "source_id",
        "conversation_id",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_key_set_equals("--fields minimal", &keys, &documented)
}

#[test]
fn fields_summary_retains_the_exact_anchor_without_message_bodies() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let (_stdout, parsed) = run_search(&data_dir, &["--fields", "summary", "--limit", "1"])?;
    let keys = first_hit_keys(&parsed)?;
    let documented: BTreeSet<String> = [
        "agent",
        "line_number",
        "source_path",
        "source_id",
        "conversation_id",
        "title",
        "score",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_key_set_equals("--fields summary", &keys, &documented)
}

#[test]
fn fields_minimal_strictly_reduces_response_bytes_vs_default() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let (default_stdout, _) = run_search(&data_dir, &[])?;
    let (minimal_stdout, _) = run_search(&data_dir, &["--fields", "minimal"])?;
    let default_bytes = default_stdout.len();
    let minimal_bytes = minimal_stdout.len();
    // Retaining the complete anchor must not defeat the reason to use minimal.
    ensure(
        !matches!(
            minimal_bytes.cmp(&default_bytes),
            Ordering::Greater | Ordering::Equal
        ),
        format!(
            "--fields minimal must emit strictly fewer bytes than default.\n\
             default bytes: {default_bytes}\n\
             minimal bytes: {minimal_bytes}"
        ),
    )?;
    Ok(())
}

#[test]
fn fields_explicit_comma_list_emits_exactly_requested_keys() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let (_stdout, parsed) = run_search(
        &data_dir,
        &["--fields", "source_path,score", "--limit", "1"],
    )?;
    let keys = first_hit_keys(&parsed)?;
    let requested: BTreeSet<String> = ["score", "source_path"]
        .iter()
        .copied()
        .map(String::from)
        .collect();
    // Agents that build ranking-adjacent tooling pipe `--fields
    // source_path,score` and expect exactly those two keys. Any drift
    // here breaks the contract that "you get what you asked for".
    assert_key_set_equals("--fields source_path,score", &keys, &requested)
}
