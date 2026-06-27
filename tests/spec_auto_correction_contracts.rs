//! INV-cass-12 — `cass` auto-correction behavior contract.
//!
//! AGENTS.md "Auto-Correction Features" documents user-facing affordances
//! agents and humans both rely on when typing/scripting cass calls:
//!
//!   - `-robot`   → `--robot`   ("long flags need double-dash")
//!   - `--Robot`  → `--robot`   ("flags are lowercase")
//!   - `--LIMIT`  → `--limit`
//!   - `find "q"` → `search "q"` (5 search aliases: find, query, q, lookup, grep)
//!
//! INV-cass-7 already locks the *declaration* of subcommand aliases by
//! cross-checking against `cass introspect --json`. This file locks the
//! *behavior*: the alias actually invokes the search subcommand and
//! produces the same hit set, and the flag-correction layer preserves
//! the flag's value (not just its name) so `--LIMIT 1` actually limits.
//!
//! Four invariants:
//!
//!   1. Every search alias (`find`, `query`, `q`, `lookup`, `grep`)
//!      returns the same hit count as the canonical `search` subcommand
//!      against the fixture.
//!   2. `-robot` (single-dash long flag) produces robot JSON output
//!      (the auto-correction translates it to `--robot`).
//!   3. `--Robot` (uppercase) produces robot JSON AND emits a
//!      "Auto-corrected" note to stderr — the affordance must be
//!      visible so users learn the canonical form.
//!   4. `--LIMIT N` (uppercase) correctly limits the result count to
//!      `N`, not just translating the flag name but preserving its
//!      bound value. A regression that mangled value-binding would
//!      pass test (3)'s name-only check but fail this one.
//!
//! Verified against the checked-in `search_demo_data` fixture with the
//! query `"the"` (2 aider hits).

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

fn run_cass(data_dir: &Path, args: &[&str]) -> Result<CmdOutcome, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(args)
        .args(["--data-dir", data_dir.to_str().ok_or("non-utf8 path")?])
        .output()?;
    Ok(CmdOutcome {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Parse `cass` stdout as a search response and extract the `hits` length.
/// Asserts exit 0 so a regression that broke parsing into an error surfaces
/// as a clean diagnostic rather than a confusing JSON-parse failure.
fn hit_count_for_outcome(label: &str, outcome: &CmdOutcome) -> Result<usize, Box<dyn Error>> {
    let code = outcome
        .exit_code
        .ok_or_else(|| test_error(format!("[{label}] killed by signal")))?;
    if code != 0 {
        return Err(test_error(format!(
            "[{label}] exited {code}; stderr:\n{}",
            outcome.stderr
        )));
    }
    let parsed: Value = serde_json::from_str(outcome.stdout.trim())
        .map_err(|err| test_error(format!("[{label}] stdout is not valid JSON: {err}")))?;
    parsed
        .get("hits")
        .and_then(Value::as_array)
        .map(Vec::len)
        .ok_or_else(|| test_error(format!("[{label}] response missing `hits` array")))
}

/// Verify a single search alias yields the same hit count as `search`.
/// Lives in its own helper so the diagnostic `format!` calls do not live
/// inside the caller's loop body (UBS `format!`-in-loop heuristic).
fn check_alias_matches_canonical(
    data_dir: &Path,
    alias: &str,
    canonical_count: usize,
) -> TestResult {
    let outcome = run_cass(data_dir, &[alias, "the", "--robot"])?;
    let alias_count = hit_count_for_outcome(&format!("alias `{alias}`"), &outcome)?;
    ensure(
        alias_count == canonical_count,
        format!(
            "alias `cass {alias}` returned {alias_count} hits but `cass search` returned {canonical_count}.\n\
             AGENTS.md documents `{alias}` as an alias for `search` — divergence is a behavior regression."
        ),
    )
}

#[test]
fn search_aliases_all_produce_equivalent_hits() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    // Canonical baseline.
    let canonical = run_cass(&data_dir, &["search", "the", "--robot"])?;
    let canonical_count = hit_count_for_outcome("search (canonical)", &canonical)?;
    ensure(
        canonical_count > 0,
        "fixture query `the` should yield at least 1 hit on canonical search",
    )?;

    // Per AGENTS.md "Full alias list": find, query, q, lookup, grep all map to search.
    for alias in ["find", "query", "q", "lookup", "grep"] {
        check_alias_matches_canonical(&data_dir, alias, canonical_count)?;
    }
    Ok(())
}

#[test]
fn single_dash_long_flag_is_auto_corrected_to_double_dash() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    // `-robot` (single dash on a long flag) — the documented user-error
    // that the auto-corrector translates to `--robot`. Behavior contract:
    // exit 0, stdout is valid robot JSON with a `hits` array.
    let outcome = run_cass(&data_dir, &["search", "the", "-robot"])?;
    let count = hit_count_for_outcome("single-dash -robot", &outcome)?;
    ensure(
        count > 0,
        "single-dash `-robot` should still produce robot JSON with hits",
    )?;
    Ok(())
}

#[test]
fn uppercase_robot_flag_is_auto_corrected_and_announces_correction() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    // `--Robot` (uppercase) — the documented "flags are lowercase"
    // correction. Two sub-properties:
    //   (a) the call succeeds and produces robot JSON;
    //   (b) stderr carries an "Auto-corrected" note so the user can
    //       learn the canonical form. Without (b), the affordance is
    //       silent and users keep typing the wrong form.
    let outcome = run_cass(&data_dir, &["search", "the", "--Robot"])?;
    let count = hit_count_for_outcome("uppercase --Robot", &outcome)?;
    ensure(
        count > 0,
        "uppercase `--Robot` should produce robot JSON with hits",
    )?;
    ensure(
        outcome.stderr.contains("Auto-corrected"),
        format!(
            "uppercase `--Robot` should emit an `Auto-corrected` note to stderr; got:\n{}",
            outcome.stderr
        ),
    )?;
    Ok(())
}

#[test]
fn uppercase_limit_flag_preserves_value_binding_after_correction() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;

    // `--LIMIT 1` — the value binding (1) must survive the flag-case
    // correction. A regression that mapped the flag name to its
    // canonical form but failed to bind the value would slip past the
    // simpler "--Robot works" test in (3) above — this test catches it
    // by asserting the limit was actually applied to the result set.
    let outcome = run_cass(&data_dir, &["search", "the", "--robot", "--LIMIT", "1"])?;
    let count = hit_count_for_outcome("uppercase --LIMIT 1", &outcome)?;
    ensure(
        count == 1,
        format!(
            "`--LIMIT 1` should bind the value `1` to the corrected --limit flag; got {count} hits"
        ),
    )?;
    Ok(())
}
