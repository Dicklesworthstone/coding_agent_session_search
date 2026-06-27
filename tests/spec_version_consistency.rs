//! INV-cass-15 — `cass` version consistency contract.
//!
//! Three version surfaces must agree:
//!
//!   - `cargo`'s package version (`Cargo.toml::version`), exposed at
//!     compile time as `env!("CARGO_PKG_VERSION")`;
//!   - the plain-text `cass --version` output (what humans/scripts
//!     `grep` for);
//!   - the `crate_version` field of `cass api-version --json` (what
//!     agents `jq` on).
//!
//! When these drift, real users get confused: a support transcript
//! says "0.6.4" while the JSON envelope reports "0.6.2" while the
//! release notes describe "0.6.3". The existing
//! `lifecycle_matrix::api_version_json_matches_golden` test pins
//! the **shape** of `api-version --json` via a scrubbed golden, but
//! scrubbing necessarily wipes the actual version value — so the
//! shape guard alone cannot catch a version-skew bug. This file fills
//! that gap.
//!
//! Four invariants:
//!
//!   1. `api-version --json::crate_version` exactly equals
//!      `env!("CARGO_PKG_VERSION")` baked into the test binary. A
//!      regression that hard-codes a version in code or forgets to
//!      update one of the surfaces is caught immediately.
//!   2. `cass --version` plain-text output contains that same
//!      `crate_version` (the human-readable surface stays in sync).
//!   3. `api_version` is a non-negative integer. The pinned major-API
//!      version that agents may branch on.
//!   4. `contract_version` is a non-empty string. The independent
//!      contract-channel version.

use std::cmp::Ordering;
use std::error::Error;

use assert_cmd::Command;
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

#[test]
fn api_version_crate_version_equals_cargo_package_version() -> TestResult {
    let outcome = assert_success("api-version --json", run_cass(&["api-version", "--json"])?)?;
    let parsed: Value = serde_json::from_str(outcome.stdout.trim())?;
    let crate_version = parsed
        .get("crate_version")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("api-version envelope missing string `crate_version`"))?;
    // CARGO_PKG_VERSION is baked into the test binary at compile time from
    // Cargo.toml::version. Compile-time vs runtime drift here means a
    // hard-coded version somewhere that has not been kept in sync.
    let cargo_version = env!("CARGO_PKG_VERSION");
    ensure(
        crate_version == cargo_version,
        format!(
            "api-version.crate_version ({crate_version:?}) does not match \
             Cargo.toml::version ({cargo_version:?}) — a hard-coded version \
             somewhere has drifted from the package manifest"
        ),
    )?;
    Ok(())
}

#[test]
fn plain_version_output_contains_cargo_package_version() -> TestResult {
    let outcome = assert_success("cass --version", run_cass(&["--version"])?)?;
    let cargo_version = env!("CARGO_PKG_VERSION");
    // `cass --version` is a one-line "cass <version>" string. The
    // contract here is just that the cargo version appears somewhere
    // in stdout — leaving room for the format to evolve (e.g. add
    // a build SHA suffix) without breaking the human-readable surface.
    ensure(
        outcome.stdout.contains(cargo_version),
        format!(
            "`cass --version` output must contain the cargo package version {cargo_version:?}.\n\
             stdout: {:?}",
            outcome.stdout
        ),
    )?;
    Ok(())
}

#[test]
fn api_version_field_is_non_negative_integer() -> TestResult {
    let outcome = assert_success("api-version --json", run_cass(&["api-version", "--json"])?)?;
    let parsed: Value = serde_json::from_str(outcome.stdout.trim())?;
    let api_version = parsed
        .get("api_version")
        .ok_or_else(|| test_error("api-version envelope missing `api_version` field"))?;
    let n = api_version.as_i64().ok_or_else(|| {
        test_error(format!(
            "`api_version` must be an integer; got: {api_version}"
        ))
    })?;
    ensure(
        !matches!(n.cmp(&0), Ordering::Less),
        format!("`api_version` must be non-negative; got: {n}"),
    )?;
    Ok(())
}

#[test]
fn contract_version_field_is_nonempty_string() -> TestResult {
    let outcome = assert_success("api-version --json", run_cass(&["api-version", "--json"])?)?;
    let parsed: Value = serde_json::from_str(outcome.stdout.trim())?;
    let contract_version = parsed
        .get("contract_version")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("api-version envelope missing string `contract_version`"))?;
    ensure(
        !contract_version.is_empty(),
        "`contract_version` must be a non-empty string",
    )?;
    Ok(())
}
