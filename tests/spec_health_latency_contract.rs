//! INV-cass-26 — `cass health --json` latency contract.
//!
//! The README documents `cass health --json` as a sub-50ms pre-flight
//! probe agents call before trusted handoffs. Until this file, that
//! latency contract had no mechanical guard at the cass level. The
//! existing `tests/robot_perf.rs` covers `cass --version` and `cass
//! api-version` latency under 150ms — but not the most-load-bearing
//! agent gate: `cass health --json`.
//!
//! Three invariants:
//!
//!   1. `cass health --json` against an empty/uninitialized data-dir
//!      reports `latency_ms <= 150` (the documented 50ms target plus
//!      a generous CI margin matching the robot_perf.rs pattern). The
//!      health probe must remain fast even when there is nothing to
//!      measure, otherwise the pre-flight gate becomes a slow path.
//!   2. `cass health --json` against an initialized fixture data-dir
//!      reports `latency_ms <= 150`. Locked separately because the
//!      initialized path executes more code (DB open, asset scan) and
//!      is the *real* hot path in production.
//!   3. `latency_ms` is a non-negative integer. A regression to
//!      string-typed timing or fractional values would silently break
//!      agents that arithmetic-compare against the field.
//!
//! Sampled observation (dev box, source-built cass): 3-5ms on empty,
//! 5-15ms on initialized — so the 150ms threshold leaves ~10x headroom
//! for CI variance and remains a strong regression signal.

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

/// Maximum allowed in-envelope `latency_ms` for `cass health --json`.
/// The README documents a 50ms target; this 3x margin matches the
/// existing `tests/robot_perf.rs::api_version_latency_under_150ms`
/// pattern and gives CI headroom for shared-runner variance.
const HEALTH_LATENCY_THRESHOLD_MS: i64 = 150;

fn run_health(data_dir: &Path) -> Result<Value, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never", "health", "--json"])
        .args(["--data-dir", data_dir.to_str().ok_or("non-utf8 path")?])
        .output()?;
    let code = output
        .status
        .code()
        .ok_or_else(|| test_error("cass health killed by signal"))?;
    // health may exit 0 (healthy) OR 1 (not-ready); both produce a valid
    // JSON envelope. We reject only panic-class exits.
    if matches!(code.cmp(&101), Ordering::Equal)
        || matches!(code.cmp(&134), Ordering::Equal)
        || matches!(code.cmp(&139), Ordering::Equal)
    {
        return Err(test_error(format!(
            "cass health exited with panic-class code {code}; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let parsed: Value = serde_json::from_slice(&output.stdout)?;
    Ok(parsed)
}

fn latency_ms(envelope: &Value) -> Result<i64, Box<dyn Error>> {
    let value = envelope
        .get("latency_ms")
        .ok_or_else(|| test_error("health envelope missing `latency_ms` field"))?;
    value.as_i64().ok_or_else(|| {
        test_error(format!(
            "health envelope `latency_ms` must be an integer; got: {value}"
        ))
    })
}

#[test]
fn health_latency_under_threshold_on_empty_data_dir() -> TestResult {
    let tmp = TempDir::new()?;
    let empty = tmp.path().join("empty");
    fs::create_dir_all(&empty)?;
    let envelope = run_health(&empty)?;
    let n = latency_ms(&envelope)?;
    ensure(
        !matches!(n.cmp(&HEALTH_LATENCY_THRESHOLD_MS), Ordering::Greater),
        format!(
            "cass health --json against empty data-dir reported \
             latency_ms={n}, exceeding threshold {HEALTH_LATENCY_THRESHOLD_MS}ms \
             (README documents 50ms target). A regression here means the \
             pre-flight gate agents call before trusted handoffs is no \
             longer fast."
        ),
    )?;
    Ok(())
}

#[test]
fn health_latency_under_threshold_on_initialized_fixture() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let envelope = run_health(&data_dir)?;
    let n = latency_ms(&envelope)?;
    ensure(
        !matches!(n.cmp(&HEALTH_LATENCY_THRESHOLD_MS), Ordering::Greater),
        format!(
            "cass health --json against initialized fixture reported \
             latency_ms={n}, exceeding threshold {HEALTH_LATENCY_THRESHOLD_MS}ms. \
             The initialized path executes more code (DB open, asset scan) \
             and is the real hot path in production — slowing it here \
             breaks production-relevant agent flows."
        ),
    )?;
    Ok(())
}

#[test]
fn health_latency_ms_is_non_negative_integer_type() -> TestResult {
    let tmp = TempDir::new()?;
    let empty = tmp.path().join("empty");
    fs::create_dir_all(&empty)?;
    let envelope = run_health(&empty)?;
    let n = latency_ms(&envelope)?;
    ensure(
        !matches!(n.cmp(&0), Ordering::Less),
        format!(
            "health envelope `latency_ms` must be non-negative; got {n}. \
             A negative timing value indicates a clock-rollback bug at \
             measurement time."
        ),
    )?;
    Ok(())
}
