//! INV-cass-11 (canonical slice) — `cass search --robot-format jsonl|compact`
//! emits output that satisfies the format's canonical line/JSON contract.
//!
//! Agents consume these streaming formats by splitting on newlines and parsing
//! each line. The contract that matters — independent of the volatile `_meta`
//! payload — is:
//!
//!   - `jsonl`:   every non-empty stdout line is independently valid JSON.
//!   - `compact`: stdout is exactly one line of valid JSON.
//!
//! Both invariants are corpus-independent (verified against the checked-in
//! search-demo fixture with a deliberately non-matching query for a
//! deterministic 0-hit envelope). They do NOT freeze the `_meta` content,
//! which is heavily host/time-dependent (host parallelism, loadavg, elapsed_ms,
//! age_seconds, paths, timestamps) and inappropriate to lock at the line-shape
//! level — the existing `golden_robot_json` harness owns content goldens via
//! `scrub_robot_json`.
//!
//! Gauntlet provenance: F7-3 / SURF-cass-001 (output-format goldens). The
//! 11-format content-golden push is intentionally not done here; this is the
//! canonical contract layer that protects every agent-facing format consumer
//! and is the right kind of guard to land cleanly (low-churn, no fixture
//! introspection, no host-dependent scrubbing).

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
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

/// Copy the checked-in search-demo fixture into a fresh temp data-dir so the
/// test reads from an isolated, byte-identical copy of the canonical DB +
/// lexical index (mirrors `tests/golden_robot_json::isolated_search_demo_data`).
fn copy_search_demo_fixture(test_home: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("search_demo_data");
    let dst_root = test_home.join("search_demo_data");
    for entry in WalkDir::new(&src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(&src)?;
        let dst = dst_root.join(rel);
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

/// Run `cass search <args>` against the seeded fixture and return stdout.
fn run_search(data_dir: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["search"])
        .args(args)
        .args(["--data-dir", data_dir.to_str().ok_or("non-utf8 path")?])
        .output()?;
    if !output.status.success() {
        return Err(test_error(format!(
            "cass search exited with {:?}; stderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// Split stdout into non-empty trimmed lines (the unit consumers parse).
fn output_lines(stdout: &str) -> Vec<&str> {
    stdout.lines().filter(|l| !l.trim().is_empty()).collect()
}

fn parse_each_line_as_json(lines: &[&str]) -> Result<(), Box<dyn Error>> {
    for (i, line) in lines.iter().enumerate() {
        if serde_json::from_str::<serde_json::Value>(line).is_err() {
            return Err(test_error(format!(
                "jsonl line {i} failed to parse as independent JSON: {}…",
                line.chars().take(120).collect::<String>()
            )));
        }
    }
    Ok(())
}

/// A deliberately non-matching query yields a deterministic 0-hit envelope —
/// the only fully corpus-independent shape we can rely on for a structural
/// contract test (we do NOT know the fixture's exact term inventory).
const NO_MATCH_QUERY: &str = "zzznomatchquery_xyz_unique_token_99";

#[test]
fn jsonl_every_line_is_independent_valid_json_with_robot_meta() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let stdout = run_search(
        &data_dir,
        &[NO_MATCH_QUERY, "--robot-format", "jsonl", "--robot-meta"],
    )?;
    let lines = output_lines(&stdout);
    ensure(
        !lines.is_empty(),
        "jsonl with --robot-meta must emit at least one envelope line",
    )?;
    parse_each_line_as_json(&lines)?;
    Ok(())
}

#[test]
fn jsonl_every_line_is_independent_valid_json_without_robot_meta() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let stdout = run_search(&data_dir, &[NO_MATCH_QUERY, "--robot-format", "jsonl"])?;
    let lines = output_lines(&stdout);
    parse_each_line_as_json(&lines)?;
    Ok(())
}

#[test]
fn compact_format_is_exactly_one_line_of_valid_json() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let stdout = run_search(&data_dir, &[NO_MATCH_QUERY, "--robot-format", "compact"])?;
    let lines = output_lines(&stdout);
    ensure(
        lines.len() == 1,
        format!(
            "compact format must be exactly one line; got {} non-empty lines",
            lines.len()
        ),
    )?;
    parse_each_line_as_json(&lines)?;
    Ok(())
}

#[test]
fn json_format_parses_as_a_single_json_document() -> TestResult {
    // The default `--robot` (pretty JSON) output is one document across
    // possibly-many pretty-printed lines. Concatenated stdout must parse as a
    // single JSON value — this is the contract distinct from jsonl/compact.
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let stdout = run_search(&data_dir, &[NO_MATCH_QUERY, "--robot"])?;
    serde_json::from_str::<serde_json::Value>(stdout.trim()).map_err(|err| {
        test_error(format!(
            "pretty --robot output is not a single JSON doc: {err}"
        ))
    })?;
    Ok(())
}
