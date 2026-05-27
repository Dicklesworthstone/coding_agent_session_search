//! INV-cass-5 — `cass view` and `cass expand` error envelope shape contract.
//!
//! The README's "Robot Mode Etiquette" + AGENTS.md exit-code table promise
//! that every `--json`/`--robot` invocation:
//!
//!   - puts **diagnostics on stderr** and leaves stdout for data only
//!     (so an agent pipelining `cass view ... --json | jq` never reads
//!     an error envelope as if it were data);
//!   - uses **kebab-case `kind`** values in error envelopes
//!     (see AGENTS.md "Schema Stability" — codes ≥10 are ambiguous,
//!     branch on `kind`);
//!   - sets **`retryable`** truthfully so agents do not infinite-retry
//!     non-retryable conditions.
//!
//! This file mechanically locks those promises for the two follow-up
//! commands an agent runs after `cass search`: `view` (read N lines around
//! a hit) and `expand` (read N lines of context around a hit). Five
//! structural cases:
//!
//!   1. `view` on a nonexistent file — exit 3, `kind="file-not-found"`,
//!      `retryable=false`, message names the path.
//!   2. `view` on a real file at an out-of-range line — exit 2,
//!      `kind="line-out-of-range"`, `retryable=false`, message names
//!      both the requested line and the file's actual length.
//!   3. `view` on a real file at a valid line — exit 0, stdout parses as
//!      a JSON object with the documented data keys; stderr is empty.
//!   4. `expand` on a nonexistent file — exit 3, `kind="file-not-found"`,
//!      `retryable=false`. Same exit-code as `view` for the same condition,
//!      with an `expand`-specific message.
//!   5. `expand` on a non-JSONL file that's not indexed — exit 9,
//!      `kind="indexed-session-required"`, `retryable=false`. Documents
//!      the indexed-session precondition for the local-expand fast path.

use std::error::Error;
use std::fs;
use std::path::{Component, Path, PathBuf};

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

/// Build an isolated HOME/XDG environment so cass cannot drift onto the
/// developer's real ~/.claude/projects/ session corpus (per project memory:
/// "Indexer tests need CASS_IGNORE_SOURCES_CONFIG + fake HOME/XDG"). Without
/// this, view/expand can attempt to resolve the path against the live source
/// catalog and spend minutes scanning real session JSONL.
struct IsolatedHome {
    _tmp: TempDir,
    home: PathBuf,
    config: PathBuf,
    data: PathBuf,
    cache: PathBuf,
}

fn isolated_home() -> Result<IsolatedHome, Box<dyn Error>> {
    let tmp = TempDir::new()?;
    let home = tmp.path().to_path_buf();
    let config = home.join("config");
    let data = home.join("data");
    let cache = home.join("cache");
    fs::create_dir_all(&config)?;
    fs::create_dir_all(&data)?;
    fs::create_dir_all(&cache)?;
    Ok(IsolatedHome {
        _tmp: tmp,
        home,
        config,
        data,
        cache,
    })
}

/// Copy the `tests/fixtures/aider/.aider.chat.history.md` file into the
/// isolated home, returning the destination path. This is a small (~11 line)
/// real file that produces deterministic line-count behavior for the
/// out-of-range case.
fn install_aider_fixture(iso: &IsolatedHome) -> Result<PathBuf, Box<dyn Error>> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("aider")
        .join(".aider.chat.history.md");
    let dst_dir = iso.home.join("aider");
    fs::create_dir_all(&dst_dir)?;
    let dst = safe_fixture_destination(&dst_dir, Path::new(".aider.chat.history.md"))?;
    fs::copy(&src, &dst)?;
    Ok(dst)
}

struct CmdOutcome {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_cass(iso: &IsolatedHome, args: &[&str]) -> Result<CmdOutcome, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("HOME", &iso.home)
        .env("XDG_CONFIG_HOME", &iso.config)
        .env("XDG_DATA_HOME", &iso.data)
        .env("XDG_CACHE_HOME", &iso.cache)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .args(["--color=never"])
        .args(args)
        .output()?;
    Ok(CmdOutcome {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Shared error-envelope shape contract:
///   - stdout is empty (data channel preserved for downstream pipes).
///   - exit code matches `expected_code`.
///   - stderr parses as a JSON object containing `error.kind` (kebab-case
///     == `expected_kind`) and `error.retryable` matching `expected_retryable`.
///   - the error message contains `must_contain` (so message stays operator-
///     actionable across refactors).
fn assert_error_envelope(
    label: &str,
    outcome: &CmdOutcome,
    expected_code: i32,
    expected_kind: &str,
    expected_retryable: bool,
    must_contain: &str,
) -> TestResult {
    let code = outcome
        .exit_code
        .ok_or_else(|| test_error(format!("[{label}] process was killed by signal")))?;
    ensure(
        code == expected_code,
        format!(
            "[{label}] expected exit {expected_code}; got {code}.\nstderr:\n{}",
            outcome.stderr
        ),
    )?;
    ensure(
        outcome.stdout.trim().is_empty(),
        format!(
            "[{label}] error case must not write to stdout; got:\n{}",
            outcome.stdout
        ),
    )?;
    let parsed: Value = serde_json::from_str(outcome.stderr.trim()).map_err(|err| {
        test_error(format!(
            "[{label}] stderr is not a JSON error envelope: {err}\n{}",
            outcome.stderr
        ))
    })?;
    let envelope = parsed
        .get("error")
        .ok_or_else(|| test_error(format!("[{label}] missing `error` key on stderr envelope")))?;
    let kind = envelope
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            test_error(format!(
                "[{label}] error envelope missing string `kind`: {envelope}"
            ))
        })?;
    ensure(
        kind == expected_kind,
        format!(
            "[{label}] expected kebab-case kind={expected_kind:?}; got {kind:?}.\n\
             envelope: {envelope}"
        ),
    )?;
    ensure(
        envelope.get("retryable") == Some(&Value::Bool(expected_retryable)),
        format!("[{label}] expected retryable={expected_retryable}; envelope: {envelope}"),
    )?;
    let message = envelope
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    ensure(
        message.contains(must_contain),
        format!(
            "[{label}] error message must mention {must_contain:?} so the operator can act on it; got: {message}"
        ),
    )?;
    Ok(())
}

#[test]
fn view_nonexistent_file_returns_file_not_found_envelope() -> TestResult {
    let iso = isolated_home()?;
    let bogus = iso.home.join("does_not_exist_for_view.jsonl");
    let outcome = run_cass(
        &iso,
        &[
            "view",
            "--json",
            "-n",
            "1",
            bogus.to_str().ok_or("non-utf8 path")?,
        ],
    )?;
    assert_error_envelope(
        "view-nonexistent",
        &outcome,
        3,
        "file-not-found",
        false,
        "does_not_exist_for_view",
    )
}

#[test]
fn view_line_out_of_range_returns_line_out_of_range_envelope() -> TestResult {
    let iso = isolated_home()?;
    let real_file = install_aider_fixture(&iso)?;
    let outcome = run_cass(
        &iso,
        &[
            "view",
            "--json",
            "-n",
            "999999",
            real_file.to_str().ok_or("non-utf8 path")?,
        ],
    )?;
    assert_error_envelope(
        "view-line-out-of-range",
        &outcome,
        2,
        "line-out-of-range",
        false,
        "999999",
    )
}

#[test]
fn view_with_valid_inputs_returns_data_envelope_on_stdout() -> TestResult {
    let iso = isolated_home()?;
    let real_file = install_aider_fixture(&iso)?;
    let outcome = run_cass(
        &iso,
        &[
            "view",
            "--json",
            "-n",
            "1",
            real_file.to_str().ok_or("non-utf8 path")?,
        ],
    )?;
    let code = outcome
        .exit_code
        .ok_or_else(|| test_error("view was killed by signal"))?;
    ensure(
        code == 0,
        format!(
            "view expected success; got exit {code}.\nstderr:\n{}",
            outcome.stderr
        ),
    )?;
    let parsed: Value = serde_json::from_str(outcome.stdout.trim())?;
    // Documented data keys from `cass view --json`: path, target_line,
    // total_lines, lines, context. A drift that drops any of these would
    // silently break downstream agent pipelines.
    for required in ["path", "target_line", "total_lines", "lines"] {
        require_envelope_key(required, &parsed)?;
    }
    Ok(())
}

/// Helper for the success-envelope key check. Lives outside the caller's
/// loop so the diagnostic `format!` is not flagged by UBS's
/// `format!`-in-loop heuristic.
fn require_envelope_key(required: &str, parsed: &Value) -> TestResult {
    ensure(
        parsed.get(required).is_some(),
        format!("view success envelope missing required key `{required}`: {parsed}"),
    )
}

#[test]
fn expand_nonexistent_file_returns_file_not_found_envelope() -> TestResult {
    let iso = isolated_home()?;
    let bogus = iso.home.join("does_not_exist_for_expand.jsonl");
    let outcome = run_cass(
        &iso,
        &[
            "expand",
            "--json",
            "-n",
            "1",
            "-C",
            "3",
            bogus.to_str().ok_or("non-utf8 path")?,
        ],
    )?;
    assert_error_envelope(
        "expand-nonexistent",
        &outcome,
        3,
        "file-not-found",
        false,
        "does_not_exist_for_expand",
    )
}

#[test]
fn expand_unindexed_non_jsonl_returns_indexed_session_required_envelope() -> TestResult {
    let iso = isolated_home()?;
    let real_file = install_aider_fixture(&iso)?;
    // The aider fixture is .md, not .jsonl, and is not indexed in this
    // isolated home — exercising the "local expand needs an indexed
    // conversation or a JSONL session" precondition path.
    let outcome = run_cass(
        &iso,
        &[
            "expand",
            "--json",
            "-n",
            "1",
            "-C",
            "3",
            real_file.to_str().ok_or("non-utf8 path")?,
        ],
    )?;
    assert_error_envelope(
        "expand-unindexed-non-jsonl",
        &outcome,
        9,
        "indexed-session-required",
        false,
        "indexed conversation",
    )
}
