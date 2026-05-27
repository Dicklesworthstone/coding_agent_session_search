//! INV-cass-3 — `cass search` fails open to lexical results regardless of the
//! state of the semantic stack.
//!
//! The README's "Search Asset Contract" pins this as a core promise: lexical
//! search is the required fast path; semantic refinement is opportunistic
//! enrichment. When semantic assets are missing, stale, disabled, or
//! corrupt, `cass search`:
//!
//!   1. MUST NOT panic. The process must exit cleanly with a documented
//!      exit code.
//!   2. MUST return lexical results (exit 0, valid JSON on stdout) when
//!      lexical assets are healthy, no matter what is wrong with the
//!      semantic stack.
//!   3. MAY annotate the realized mode + fallback reason in the `_meta`,
//!      but MUST NOT block on semantic readiness.
//!
//! Verified against the checked-in `search_demo_data` fixture (lexical index
//! present, no semantic assets) under four adverse semantic configurations:
//! default hybrid mode (must fall open), explicit `--mode lexical`, forced
//! `CASS_SEMANTIC_EMBEDDER=hash`, and a fixture copy with the vector_index
//! directory deleted. None may panic; all must produce valid JSON; none may
//! exit with a signal-kill class code (134 SIGABRT, 139 SIGSEGV) or the
//! cargo-test panic exit (101) when run as a release-style invocation.

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

/// Captured outcome of one `cass search` invocation under an adverse semantic
/// state.
struct SearchOutcome {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Invoke `cass search "<q>" --robot --data-dir <dir> ...extra_args` with the
/// supplied extra envs, returning the raw outcome. Does not assert anything;
/// each call site validates the lexical fail-open contract on its own terms.
fn run_search_capture(
    data_dir: &Path,
    extra_args: &[&str],
    extra_envs: &[(&str, &str)],
) -> Result<SearchOutcome, Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("cass")?;
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    for (k, v) in extra_envs {
        cmd.env(k, v);
    }
    cmd.args(["--color=never", "search", "the", "--robot"])
        .args(["--data-dir", data_dir.to_str().ok_or("non-utf8 path")?])
        .args(extra_args);
    let output = cmd.output()?;
    Ok(SearchOutcome {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// The lexical fail-open contract — applied to every adverse-state outcome.
///
///   - exit code is documented (0 success or one of the kebab-error codes,
///     never 134/139/101/signal-kill).
///   - when exit == 0, stdout parses as a single JSON value containing a
///     `hits` array — lexical results were produced.
///   - the process did not abort (no `panicked at` in stderr).
fn assert_lexical_fail_open(label: &str, outcome: &SearchOutcome) -> TestResult {
    let code = outcome.exit_code.ok_or_else(|| {
        test_error(format!(
            "[{label}] process was killed by signal (no exit code) — \
             violates INV-cass-3 (must exit cleanly)"
        ))
    })?;
    // 134 = SIGABRT (panic abort), 139 = SIGSEGV, 101 = Rust panic via libtest.
    ensure(
        !matches!(code, 101 | 134 | 139),
        format!(
            "[{label}] cass exited with panic-class code {code} — violates INV-cass-3 \
             (lexical fail-open must never panic).\nstderr:\n{}",
            outcome.stderr
        ),
    )?;
    ensure(
        !outcome.stderr.contains("panicked at"),
        format!(
            "[{label}] cass stderr contains `panicked at` — violates INV-cass-3.\n{}",
            outcome.stderr
        ),
    )?;
    ensure(
        code == 0,
        format!(
            "[{label}] cass exited non-zero ({code}); lexical assets are healthy so the \
             contract requires exit 0 with lexical results.\nstderr:\n{}",
            outcome.stderr
        ),
    )?;
    let parsed: Value = serde_json::from_str(outcome.stdout.trim())
        .map_err(|err| test_error(format!("[{label}] stdout is not valid JSON: {err}")))?;
    ensure(
        parsed.get("hits").and_then(Value::as_array).is_some(),
        format!("[{label}] response missing `hits` array — lexical envelope not produced"),
    )?;
    Ok(())
}

#[test]
fn default_hybrid_mode_falls_open_to_lexical_when_semantic_assets_absent() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    // No --mode flag: cass defaults to hybrid-preferred. The fixture has no
    // semantic assets, so the contract requires graceful fall-open to lexical.
    let outcome = run_search_capture(&data_dir, &[], &[])?;
    assert_lexical_fail_open("default-mode", &outcome)
}

#[test]
fn explicit_lexical_mode_never_touches_semantic() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    let outcome = run_search_capture(&data_dir, &["--mode", "lexical"], &[])?;
    assert_lexical_fail_open("mode=lexical", &outcome)
}

#[test]
fn forced_hash_embedder_env_does_not_break_lexical() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    // CASS_SEMANTIC_EMBEDDER=hash forces the deterministic FNV-1a fallback even
    // if an ML model is otherwise reachable; lexical results must still flow.
    let outcome = run_search_capture(&data_dir, &[], &[("CASS_SEMANTIC_EMBEDDER", "hash")])?;
    assert_lexical_fail_open("embedder=hash", &outcome)
}

#[test]
fn deleted_vector_index_dir_does_not_panic_and_returns_lexical() -> TestResult {
    let tmp = TempDir::new()?;
    let data_dir = copy_search_demo_fixture(tmp.path())?;
    // Simulate a corrupted/missing semantic asset by deleting the (possibly
    // empty) vector_index dir. `cass search` must continue to serve lexical.
    let vector_index = data_dir.join("vector_index");
    if vector_index.exists() {
        fs::remove_dir_all(&vector_index)?;
    }
    let outcome = run_search_capture(&data_dir, &[], &[])?;
    assert_lexical_fail_open("vector_index removed", &outcome)
}
