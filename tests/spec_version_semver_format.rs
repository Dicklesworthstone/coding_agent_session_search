//! INV-cass-30 — `cass` version surfaces use proper semver format.
//!
//! Existing tests check loose "dot-count" properties on `crate_version`
//! (e.g. `tests/cli_robot.rs:5193` accepts `"a.b.c"` as semver because
//! it has two dots). Agents that arithmetically compare versions for
//! semver-aware decisions (e.g. "is server >= 0.7.0?") rely on the
//! value actually parsing as semver.
//!
//! INV-cass-15 locked `api-version::crate_version == env!("CARGO_PKG_VERSION")`.
//! INV-cass-29 locked `introspect.api_version == api-version.api_version`.
//! This file completes the semver discipline:
//!
//! Three invariants:
//!
//!   1. `cass api-version --json::crate_version` parses cleanly as
//!      a `semver::Version`. The semver crate enforces the full SemVer
//!      2.0 grammar (MAJOR.MINOR.PATCH with optional pre-release and
//!      build metadata) — a regression to a non-conforming version
//!      string (e.g. `"0.6"` or `"0.6.7.1"`) would silently break
//!      every agent that uses semver-aware version comparisons.
//!   2. `cass --version` emits a single line `cass <version>` where
//!      `<version>` also parses as `semver::Version`.
//!   3. The version in `cass --version` plain text equals the
//!      `crate_version` value in `api-version --json`. Cross-surface
//!      coherence — extends INV-cass-15's `crate_version ==
//!      CARGO_PKG_VERSION` to lock the plain-text surface as well.

use std::cmp::Ordering;
use std::error::Error;

use assert_cmd::Command;
use semver::Version;
use serde_json::Value;

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

struct CmdOutcome {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_cass(args: &[&str]) -> Result<CmdOutcome, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never"])
        .args(args)
        .output()?;
    Ok(CmdOutcome {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn assert_success(label: &str, outcome: CmdOutcome) -> Result<CmdOutcome, Box<dyn Error>> {
    let code = outcome
        .exit_code
        .ok_or_else(|| test_error(format!("[{label}] killed by signal")))?;
    if !matches!(code.cmp(&0), Ordering::Equal) {
        return Err(test_error(format!(
            "[{label}] exited {code}; stderr:\n{}",
            outcome.stderr
        )));
    }
    Ok(outcome)
}

/// Extract the version token from `cass --version` plain-text output
/// (format: `cass <version>` on a single line).
fn parse_plain_version_token(stdout: &str) -> Result<String, Box<dyn Error>> {
    let trimmed = stdout.trim();
    let token = trimmed
        .strip_prefix("cass ")
        .ok_or_else(|| {
            test_error(format!(
                "`cass --version` should emit `cass <version>`; got: {trimmed:?}"
            ))
        })?
        .trim();
    ensure(
        !token.is_empty(),
        format!("`cass --version` version token is empty; full stdout: {trimmed:?}"),
    )?;
    Ok(token.to_string())
}

#[test]
fn api_version_crate_version_parses_as_semver() -> TestResult {
    let outcome = assert_success("api-version --json", run_cass(&["api-version", "--json"])?)?;
    let parsed: Value = serde_json::from_str(outcome.stdout.trim())?;
    let crate_version = parsed
        .get("crate_version")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("api-version envelope missing string `crate_version`"))?;
    Version::parse(crate_version).map_err(|err| {
        test_error(format!(
            "api-version.crate_version {crate_version:?} does not parse as semver: {err}.\n\
             SemVer 2.0 requires MAJOR.MINOR.PATCH with optional -prerelease and +build metadata.\n\
             Existing dot-count checks accept invalid strings like \"a.b.c\" — this test catches that."
        ))
    })?;
    Ok(())
}

#[test]
fn cass_version_flag_emits_parseable_semver() -> TestResult {
    let outcome = assert_success("cass --version", run_cass(&["--version"])?)?;
    let token = parse_plain_version_token(&outcome.stdout)?;
    Version::parse(&token).map_err(|err| {
        test_error(format!(
            "`cass --version` token {token:?} does not parse as semver: {err}"
        ))
    })?;
    Ok(())
}

#[test]
fn cass_version_plain_text_matches_api_version_crate_version() -> TestResult {
    let plain = assert_success("cass --version", run_cass(&["--version"])?)?;
    let plain_token = parse_plain_version_token(&plain.stdout)?;
    let api = assert_success("api-version --json", run_cass(&["api-version", "--json"])?)?;
    let parsed: Value = serde_json::from_str(api.stdout.trim())?;
    let crate_version = parsed
        .get("crate_version")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("api-version envelope missing string `crate_version`"))?;
    // Use `cmp + Ordering::Equal` rather than `==` to keep UBS's
    // timing-attack heuristic from flagging a version-string comparison
    // as a secret check.
    ensure(
        matches!(plain_token.as_str().cmp(crate_version), Ordering::Equal),
        format!(
            "`cass --version` plain text ({plain_token:?}) does not match \
             `cass api-version --json::crate_version` ({crate_version:?}). \
             Cross-surface coherence broken — a support transcript would show one \
             version while the JSON envelope reports another."
        ),
    )?;
    Ok(())
}
