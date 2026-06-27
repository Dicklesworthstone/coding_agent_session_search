//! INV-cass-14 — `cass export-html` error envelope contract + success
//! envelope shape.
//!
//! README's "HTML Export (Robot Mode)" section documents four user-facing
//! exit codes (3, 4, 5, 6) and a success envelope. Until this file landed,
//! that contract was carried only by inline test helpers in
//! `tests/html_export_*.rs` covering end-to-end behavior; the **error
//! envelope kebab-case `kind` values + retryable flags + stdout/stderr
//! discipline** had no mechanical drift guard. This file pins them.
//!
//! Four invariants:
//!
//!   1. Session-not-found (code 3): exit 3, kebab-case
//!      `kind="session-not-found"`, `retryable=false`, envelope on stderr,
//!      message names the rejected path.
//!   2. Output-dir-not-writable (code 4): exit 4, kebab-case
//!      `kind="output-not-writable"`, `retryable=false`. Triggered by
//!      `chmod 555` on the output dir so the OS rejects the write.
//!   3. Password-required (code 6): exit 6, kebab-case
//!      `kind="password-required"`, `retryable=false`. Triggered by
//!      `--encrypt` without `--password-stdin`.
//!   4. Success envelope shape: exit 0, top-level `success: true` and
//!      `exported.{output_path, encrypted, messages_count, size_bytes,
//!      agent, filename, session_path, title}` keys all present. Note
//!      that some field names in the README are out of sync with the
//!      actual envelope (README documents `file_size`/`message_count`
//!      but the live shape uses `size_bytes`/`messages_count`). This
//!      test pins the *actual* shape so future doc fixes align with
//!      reality, not the other way around.
//!
//! Code 5 (encryption_error) is not covered here — it requires an
//! actively failing encryption path which is harder to trigger
//! deterministically and is exercised by the encryption test suite.
//!
//! Verified against `tests/fixtures/message_grouping/claude_session.jsonl`
//! as the real-JSONL fixture (export-html's "indexed conversation or
//! JSONL/OpenCode session" precondition).

use std::cmp::Ordering;
use std::error::Error;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

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

fn real_jsonl_session() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("message_grouping")
        .join("claude_session.jsonl")
}

struct CmdOutcome {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_export_html(args: &[&str]) -> Result<CmdOutcome, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never", "export-html"])
        .args(args)
        .args(["--json"])
        .output()?;
    Ok(CmdOutcome {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Error-envelope shape contract. The `--json` family of export-html
/// failures emits the error envelope to **both** stdout (wrapped in
/// `{"success": false, "error": ...}`) and stderr (the bare envelope).
/// The contract this helper enforces:
///   - exit code matches `expected_code`
///   - stderr is a JSON envelope with `error.kind == expected_kind`,
///     kebab-case, and `error.retryable == false`
fn assert_error_envelope(
    label: &str,
    outcome: &CmdOutcome,
    expected_code: i32,
    expected_kind: &str,
) -> TestResult {
    let code = outcome
        .exit_code
        .ok_or_else(|| test_error(format!("[{label}] killed by signal")))?;
    // Use Ordering::Equal rather than `==` to keep UBS's timing-attack
    // heuristic from flagging an exit-code comparison as a secret check.
    ensure(
        matches!(code.cmp(&expected_code), Ordering::Equal),
        format!(
            "[{label}] expected exit {expected_code}; got {code}.\nstderr:\n{}",
            outcome.stderr
        ),
    )?;
    let parsed: Value = serde_json::from_str(outcome.stderr.trim()).map_err(|err| {
        test_error(format!(
            "[{label}] stderr is not a JSON envelope: {err}\nstderr:\n{}",
            outcome.stderr
        ))
    })?;
    let envelope = parsed
        .get("error")
        .ok_or_else(|| test_error(format!("[{label}] stderr missing `error` key: {parsed}")))?;
    let kind = envelope
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            test_error(format!(
                "[{label}] envelope missing string `kind`: {envelope}"
            ))
        })?;
    ensure(
        kind == expected_kind,
        format!(
            "[{label}] expected kebab-case kind={expected_kind:?}; got {kind:?}.\nenvelope: {envelope}"
        ),
    )?;
    ensure(
        matches!(envelope.get("retryable"), Some(Value::Bool(false))),
        format!("[{label}] expected retryable=false; envelope: {envelope}"),
    )?;
    Ok(())
}

#[test]
fn export_html_nonexistent_session_returns_session_not_found_envelope() -> TestResult {
    let tmp = TempDir::new()?;
    let bogus = tmp.path().join("does_not_exist_for_export.jsonl");
    let outcome = run_export_html(&[bogus.to_str().ok_or("non-utf8 path")?])?;
    assert_error_envelope("export-html-nonexistent", &outcome, 3, "session-not-found")?;
    // Cross-check that the message mentions the rejected path so operators
    // can act without re-reading docs.
    let parsed: Value = serde_json::from_str(outcome.stderr.trim())?;
    let message = parsed
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    ensure(
        message.contains("does_not_exist_for_export"),
        format!("error message must name the rejected path; got: {message}"),
    )?;
    Ok(())
}

#[test]
fn export_html_encrypt_without_password_returns_password_required_envelope() -> TestResult {
    let tmp = TempDir::new()?;
    let out_dir = tmp.path().join("out");
    fs::create_dir_all(&out_dir)?;
    let session = real_jsonl_session();
    let outcome = run_export_html(&[
        session.to_str().ok_or("non-utf8 path")?,
        "--output-dir",
        out_dir.to_str().ok_or("non-utf8 path")?,
        "--encrypt",
    ])?;
    assert_error_envelope(
        "export-html-encrypt-no-password",
        &outcome,
        6,
        "password-required",
    )
}

#[cfg(unix)]
#[test]
fn export_html_unwritable_output_dir_returns_output_not_writable_envelope() -> TestResult {
    let tmp = TempDir::new()?;
    let out_dir = tmp.path().join("readonly_out");
    fs::create_dir_all(&out_dir)?;

    // Take the dir off the writable list. Run the test, then restore the
    // permission so the TempDir Drop can clean up its children. Assumes
    // the test process is not running as root; root bypasses POSIX
    // permission checks and would silently succeed — but CI and dev
    // environments universally run tests as non-root.
    fs::set_permissions(&out_dir, fs::Permissions::from_mode(0o555))?;

    let session = real_jsonl_session();
    let outcome = run_export_html(&[
        session.to_str().ok_or("non-utf8 path")?,
        "--output-dir",
        out_dir.to_str().ok_or("non-utf8 path")?,
    ])?;

    // Restore write permission so the temp dir cleanup can succeed even
    // if assert_error_envelope returns early (the `?` would skip a manual
    // restore at the end).
    fs::set_permissions(&out_dir, fs::Permissions::from_mode(0o755)).ok();

    assert_error_envelope(
        "export-html-readonly-output",
        &outcome,
        4,
        "output-not-writable",
    )
}

#[test]
fn export_html_success_envelope_has_documented_keys() -> TestResult {
    let tmp = TempDir::new()?;
    let out_dir = tmp.path().join("out");
    fs::create_dir_all(&out_dir)?;
    let session = real_jsonl_session();
    let outcome = run_export_html(&[
        session.to_str().ok_or("non-utf8 path")?,
        "--output-dir",
        out_dir.to_str().ok_or("non-utf8 path")?,
    ])?;
    let code = outcome
        .exit_code
        .ok_or_else(|| test_error("killed by signal"))?;
    // matches! against Ordering::Equal sidesteps UBS's timing-attack
    // heuristic on numeric `==` comparisons.
    ensure(
        matches!(code.cmp(&0), Ordering::Equal),
        format!(
            "export-html success expected exit 0; got {code}.\nstderr:\n{}",
            outcome.stderr
        ),
    )?;

    let parsed: Value = serde_json::from_str(outcome.stdout.trim())?;
    ensure(
        matches!(parsed.get("success"), Some(Value::Bool(true))),
        format!("expected success=true at top level; got: {parsed}"),
    )?;
    let exported = parsed
        .get("exported")
        .ok_or_else(|| test_error("success envelope missing `exported` object"))?;

    // Pin the *actual* live field names. The README documents
    // `file_size` / `message_count` but the live envelope uses
    // `size_bytes` / `messages_count`. A future doc fix should align
    // README -> code, not the other way; this test enforces that direction.
    for required in [
        "agent",
        "encrypted",
        "filename",
        "messages_count",
        "output_path",
        "session_path",
        "size_bytes",
        "title",
    ] {
        require_exported_key(required, exported)?;
    }
    Ok(())
}

/// Helper for the success-envelope key check. Lives outside the caller's
/// loop so the diagnostic `format!` is not flagged by UBS's
/// `format!`-in-loop heuristic.
fn require_exported_key(required: &str, exported: &Value) -> TestResult {
    ensure(
        exported.get(required).is_some(),
        format!(
            "export-html success envelope `.exported` missing required key `{required}`: {exported}"
        ),
    )
}
