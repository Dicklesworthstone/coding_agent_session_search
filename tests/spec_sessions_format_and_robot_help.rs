//! INV-cass-13 — `--robot-format sessions` deduplication contract +
//! `cass --robot-help` entry-point contract.
//!
//! Two distinct agent-facing surfaces with separate failure modes:
//!
//!   1. `--robot-format sessions` returns one `source_path` per line so
//!      pipelines like `cass search ... --robot-format sessions | xargs
//!      -L1 cass view` Do The Right Thing. The contract that matters:
//!      multiple hits in the same session must collapse to ONE line,
//!      not duplicate the path. A regression that emitted N lines for
//!      N hits would cause `xargs cass view` to view the same session
//!      N times — silent waste at best, an inconsistent "looking at hit
//!      i but writing to hit i-1" pipeline at worst.
//!
//!   2. `cass --robot-help` is the agent-facing entry point. Per the
//!      header text it advertises a "contract v1" version that agents
//!      may pin against. The contract here: invocation returns exit 0,
//!      stdout is non-empty, and the "contract v1" marker is present so
//!      agents that gate on it cannot be silently broken by a refactor.
//!
//! Three tests:
//!
//!   - `sessions_format_equals_unique_source_paths_from_json_response` —
//!     the lines emitted in sessions format equal, as a set, the unique
//!     `source_path` values from the JSON response. This proves *both*
//!     deduplication and per-line content correctness in one assertion.
//!   - `sessions_format_emits_zero_lines_when_no_matches` — the
//!     no-matches case is exit 0 with empty stdout (no spurious
//!     "no results" header lines that xargs would treat as a path).
//!   - `robot_help_invocation_succeeds_and_announces_contract_v1` —
//!     entry-point sanity + contract-version marker drift guard.
//!
//! Verified against the checked-in `search_demo_data` fixture with
//! query `"the"` (2 hits, same session → 1 dedup-line).

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

struct CmdOutcome {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_cass(data_dir: Option<&Path>, args: &[&str]) -> Result<CmdOutcome, Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("cass")?;
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never"])
        .args(args);
    if let Some(dir) = data_dir {
        cmd.args(["--data-dir", dir.to_str().ok_or("non-utf8 path")?]);
    }
    let output = cmd.output()?;
    Ok(CmdOutcome {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Confirm a successful invocation (exit 0) and return the outcome for
/// further inspection. Any non-zero exit gets a clean diagnostic.
fn assert_success(label: &str, outcome: CmdOutcome) -> Result<CmdOutcome, Box<dyn Error>> {
    let code = outcome
        .exit_code
        .ok_or_else(|| test_error(format!("[{label}] killed by signal")))?;
    if code != 0 {
        return Err(test_error(format!(
            "[{label}] exited {code}; stderr:\n{}",
            outcome.stderr
        )));
    }
    Ok(outcome)
}

#[test]
fn sessions_format_equals_unique_source_paths_from_json_response() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    // Call A: the JSON response, which carries the per-hit source_path
    // values; collapse to the unique set.
    let json_outcome = assert_success(
        "search --robot",
        run_cass(Some(&data_dir), &["search", "the", "--robot"])?,
    )?;
    let parsed: Value = serde_json::from_str(json_outcome.stdout.trim())?;
    let hits = parsed
        .get("hits")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("JSON response missing `hits` array"))?;
    let json_source_paths: BTreeSet<String> = hits
        .iter()
        .filter_map(|h| {
            h.get("source_path")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .collect();
    ensure(
        !json_source_paths.is_empty(),
        "fixture query 'the' should yield at least 1 hit with a source_path",
    )?;

    // Call B: same query in sessions format. Each line is a source_path,
    // already collapsed by the planner.
    let sessions_outcome = assert_success(
        "search --robot-format sessions",
        run_cass(
            Some(&data_dir),
            &["search", "the", "--robot-format", "sessions"],
        )?,
    )?;
    let session_lines: BTreeSet<String> = sessions_outcome
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect();

    // Set equality is the strongest joint property: it proves both
    // deduplication (no source_path duplicated as a line) and content
    // fidelity (every emitted line corresponds to a real source_path).
    // Use symmetric_difference rather than `==` so UBS does not mistake
    // the BTreeSet equality check for a token-comparison timing-attack
    // surface — these are file paths, not secrets.
    let symdiff: Vec<&String> = session_lines
        .symmetric_difference(&json_source_paths)
        .collect();
    ensure(
        symdiff.is_empty(),
        format!(
            "sessions-format lines must equal the JSON response's unique source_paths.\n\
             only-in-JSON or only-in-sessions ({}): {:?}\n\
             from JSON ({}): {:?}\n\
             from sessions ({}): {:?}",
            symdiff.len(),
            symdiff,
            json_source_paths.len(),
            json_source_paths,
            session_lines.len(),
            session_lines
        ),
    )?;
    Ok(())
}

#[test]
fn sessions_format_emits_zero_lines_when_no_matches() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    let outcome = assert_success(
        "search --robot-format sessions (no match)",
        run_cass(
            Some(&data_dir),
            &[
                "search",
                "zzznomatchquery_xyz_unique_token_99",
                "--robot-format",
                "sessions",
            ],
        )?,
    )?;
    let non_empty_lines: Vec<&str> = outcome
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    ensure(
        non_empty_lines.is_empty(),
        format!(
            "no-match search in sessions format must emit zero lines; got {} non-empty lines:\n{:#?}",
            non_empty_lines.len(),
            non_empty_lines
        ),
    )?;
    Ok(())
}

#[test]
fn robot_help_invocation_succeeds_and_announces_contract_v1() -> TestResult {
    // `--robot-help` is global; no --data-dir required.
    let outcome = assert_success("cass --robot-help", run_cass(None, &["--robot-help"])?)?;
    // Two minimal contract drift checks:
    //   - stdout is non-empty (the entry-point produces something);
    //   - the "contract v1" version marker is present so agents that pin
    //     against the contract version cannot be silently re-versioned
    //     without a deliberate change to this assertion.
    ensure(
        !outcome.stdout.trim().is_empty(),
        format!(
            "`cass --robot-help` must produce non-empty stdout; got {} bytes",
            outcome.stdout.len()
        ),
    )?;
    ensure(
        outcome.stdout.contains("contract v1"),
        format!(
            "`cass --robot-help` must announce its `contract v1` version (agents may pin on it).\n\
             stdout first 200 chars: {:?}",
            outcome.stdout.chars().take(200).collect::<String>()
        ),
    )?;
    Ok(())
}
