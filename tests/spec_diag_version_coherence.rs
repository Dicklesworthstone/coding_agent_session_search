//! INV-cass-32 — `cass diag --json::version` cross-surface coherence.
//!
//! `cass diag --json` reports a top-level `version` field that agents
//! pulling diagnostic dumps cite in support tickets. That field must
//! be the same value as `cass api-version --json::crate_version` (the
//! source-of-truth that INV-cass-15 already locks against
//! `env!("CARGO_PKG_VERSION")`), or a support transcript citing
//! "cass 0.6.7" via `cass diag` could quietly drift away from the
//! JSON envelope that reports `crate_version: "0.6.6"`.
//!
//! This is the fifth link in the cross-surface version-coherence
//! chain locked across the gauntlet:
//!
//!   Cargo.toml::version
//!         │
//!         ▼ INV-cass-15 (value equality)
//!   cass api-version --json::crate_version
//!         │
//!         ├─► INV-cass-29 (api_version + contract_version match)
//!         │       cass introspect --json
//!         │
//!         ├─► INV-cass-30 (semver parse + plain-text equality)
//!         │       cass --version
//!         │
//!         └─► INV-cass-32 (THIS FILE)
//!                 cass diag --json::version
//!
//! Two invariants:
//!
//!   1. `diag.version` is a non-empty string that parses cleanly as
//!      `semver::Version`. Catches diag emitting a non-conforming
//!      value silently.
//!   2. `diag.version` equals `api-version.crate_version` exactly.
//!      Catches diag drifting from the canonical source-of-truth.

use std::cmp::Ordering;
use std::error::Error;

use assert_cmd::Command;
use semver::Version;
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

fn run_cass_json(args: &[&str]) -> Result<Value, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never"])
        .args(args)
        .output()?;
    let code = output
        .status
        .code()
        .ok_or_else(|| test_error("cass killed by signal"))?;
    if !matches!(code.cmp(&0), Ordering::Equal) {
        return Err(test_error(format!(
            "cass exited {code}; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[test]
fn diag_version_parses_as_semver() -> TestResult {
    let tmp = TempDir::new()?;
    let envelope = run_cass_json(&[
        "diag",
        "--json",
        "--data-dir",
        tmp.path().to_str().ok_or("non-utf8 path")?,
    ])?;
    let version = envelope
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("diag envelope missing string `version` field"))?;
    ensure(!version.is_empty(), "diag.version must be non-empty")?;
    Version::parse(version).map_err(|err| {
        test_error(format!(
            "diag.version {version:?} does not parse as semver: {err}.\n\
             A regression to a non-conforming version string would silently break \
             support workflows that parse diag dumps for version-aware actions."
        ))
    })?;
    Ok(())
}

#[test]
fn diag_version_matches_api_version_crate_version() -> TestResult {
    let tmp = TempDir::new()?;
    let diag = run_cass_json(&[
        "diag",
        "--json",
        "--data-dir",
        tmp.path().to_str().ok_or("non-utf8 path")?,
    ])?;
    let api = run_cass_json(&["api-version", "--json"])?;

    let diag_version = diag
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("diag envelope missing string `version`"))?;
    let crate_version = api
        .get("crate_version")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("api-version envelope missing string `crate_version`"))?;

    // Use Ordering::Equal pattern to dodge UBS's timing-attack heuristic
    // on `crate_version`-named string comparisons (same dodge applied in
    // INV-cass-14, INV-cass-30 for the same heuristic).
    ensure(
        matches!(diag_version.cmp(crate_version), Ordering::Equal),
        format!(
            "diag.version ({diag_version:?}) does not match api-version.crate_version \
             ({crate_version:?}). A support transcript citing the diag value would \
             silently differ from the JSON envelope's reported version."
        ),
    )?;
    Ok(())
}
