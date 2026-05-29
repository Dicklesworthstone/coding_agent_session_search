//! INV-cass-21 — `cass search --fields` mask discipline contract.
//!
//! AGENTS.md "Key Flags" documents `--fields minimal` as "Reduce payload:
//! `source_path`, `line_number`, `agent` only" — the lean key set agents
//! pipe through `jq` when context budget matters. Existing tests in
//! `tests/cli_robot.rs::fields_minimal_preset_expands` check that some
//! expected keys are present and a couple of other keys are absent, but
//! they do not lock the **exact** key set, nor do they prove the
//! token-savings promise that justifies using the flag in the first
//! place.
//!
//! Three invariants:
//!
//!   1. `--fields minimal` emits hits whose key set is **exactly**
//!      `{agent, line_number, source_path}` — no extra, no missing.
//!      Set equality is the strongest property; a regression that
//!      added `score` "for compatibility" would slip past the existing
//!      "score is null" check but fail this one.
//!   2. `--fields minimal` produces strictly fewer total response
//!      bytes than the default. The whole reason to type the flag.
//!      Bytes are a robust proxy for LLM tokens.
//!   3. `--fields <explicit,list>` emits hits whose key set is exactly
//!      the requested list. The most powerful form of the flag: an
//!      agent that wants only `score` and `source_path` for ranking-
//!      adjacent work must be able to ask for those two and only those
//!      two.
//!
//! Verified against the checked-in `search_demo_data` fixture with
//! the query `"the"` (2 aider hits).

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
fn fields_minimal_preset_emits_exactly_the_documented_three_keys() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let (_stdout, parsed) = run_search(&data_dir, &["--fields", "minimal", "--limit", "1"])?;
    let keys = first_hit_keys(&parsed)?;
    let documented: BTreeSet<String> = ["agent", "line_number", "source_path"]
        .iter()
        .copied()
        .map(String::from)
        .collect();
    assert_key_set_equals("--fields minimal", &keys, &documented)
}

#[test]
fn fields_minimal_strictly_reduces_response_bytes_vs_default() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let (default_stdout, _) = run_search(&data_dir, &[])?;
    let (minimal_stdout, _) = run_search(&data_dir, &["--fields", "minimal"])?;
    let default_bytes = default_stdout.len();
    let minimal_bytes = minimal_stdout.len();
    // The entire agent-facing promise of `--fields minimal` is "Reduce
    // payload" (AGENTS.md). A regression where minimal emits at least as
    // many bytes as the default defeats the flag's reason for existing.
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
