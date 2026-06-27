//! INV-cass-2: `cass search` is deterministic across repeated calls against
//! the same archive, and `--limit N` is a strict prefix of `--limit 2N` (the
//! cursor-pagination correctness guarantee).
//!
//! Foundational invariants every cache, pager, and cursor consumer depends on:
//!
//!   - **Determinism**: query + filters + corpus -> same `hits` array.
//!     If the same call returns different orderings or scores on consecutive
//!     invocations, then `--cursor`, prefix-warming, the BM25 ratchet, and the
//!     two-tier semantic refinement all break in subtle ways agents see only
//!     as flake.
//!   - **Limit-prefix**: the first N hits of `--limit 2N` are the N hits of
//!     `--limit N` in the same order. This is the cursor-paging soundness
//!     property; `--cursor` token correctness depends on it.
//!
//! Verified against the checked-in `search_demo_data` fixture with a query
//! known to return at least 2 hits ("the"). The `hits` array is the user-visible /
//! agent-consumed payload; volatile `_meta` fields (elapsed_ms, timestamps,
//! age_seconds, host-dependent pipeline counts) are deliberately excluded;
//! their non-determinism is by design.

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

/// Run `cass search <args>` against the fixture and return the parsed JSON.
fn run_search(data_dir: &Path, args: &[&str]) -> Result<Value, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never", "search"])
        .args(args)
        .args(["--data-dir", data_dir.to_str().ok_or("non-utf8 path")?])
        .args(["--robot"])
        .output()?;
    if !output.status.success() {
        return Err(test_error(format!(
            "cass search exited with {:?}; stderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let parsed: Value = serde_json::from_slice(&output.stdout)?;
    Ok(parsed)
}

/// Extract the `hits` array. The hit identity payload (source_path,
/// line_number, score, snippet, title, agent) is the user-visible projection
/// whose determinism this test guards; volatile `_meta` is not compared.
fn hits(response: &Value) -> Result<&[Value], Box<dyn Error>> {
    response
        .get("hits")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| test_error("response missing `hits` array"))
}

/// A query known to match the fixture (yields 2 hits at the time of writing).
/// Stable across the v0.6.x fixture; if a future fixture refresh changes the
/// hit count, the limit-prefix test catches it explicitly.
const QUERY: &str = "the";

#[test]
fn search_returns_deterministic_hits_across_repeated_calls() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    let a = run_search(&data_dir, &[QUERY, "--limit", "5"])?;
    let b = run_search(&data_dir, &[QUERY, "--limit", "5"])?;

    let hits_a = hits(&a)?;
    let hits_b = hits(&b)?;

    ensure(
        !hits_a.is_empty(),
        format!("test query {QUERY:?} should return at least 1 hit against the fixture"),
    )?;
    ensure(
        hits_a == hits_b,
        format!(
            "search is non-deterministic: same query + corpus returned different `hits`\n\
             first call:  {} hit(s)\n\
             second call: {} hit(s)",
            hits_a.len(),
            hits_b.len()
        ),
    )?;
    Ok(())
}

#[test]
fn search_limit_n_is_strict_prefix_of_limit_2n() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    let small = run_search(&data_dir, &[QUERY, "--limit", "1"])?;
    let large = run_search(&data_dir, &[QUERY, "--limit", "2"])?;

    let small_hits = hits(&small)?;
    let large_hits = hits(&large)?;

    ensure(
        !small_hits.is_empty(),
        format!("limit=1 against {QUERY:?} should return at least 1 hit"),
    )?;
    ensure(
        large_hits.len() >= small_hits.len(),
        format!(
            "--limit 2 must not return fewer hits than --limit 1; got {} vs {}",
            large_hits.len(),
            small_hits.len()
        ),
    )?;
    let large_prefix = large_hits
        .get(..small_hits.len())
        .ok_or_else(|| test_error("--limit 2 did not contain enough hits for prefix check"))?;

    // Strict prefix: hits[0..N] of the larger result equals the entire smaller result.
    ensure(
        large_prefix == small_hits,
        format!(
            "--limit 1 result is not a prefix of --limit 2; cursor-paging soundness broken.\n\
             limit=1: {} hit(s)\n\
             limit=2: {} hit(s) (first {} prefix should match limit=1 byte-for-byte)",
            small_hits.len(),
            large_hits.len(),
            small_hits.len()
        ),
    )?;
    Ok(())
}
