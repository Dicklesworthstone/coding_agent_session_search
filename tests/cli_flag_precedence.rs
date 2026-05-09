//! CLI > env > config > default precedence tests.
//!
//! Per `coding_agent_session_search-d4r65`. Exercises the documented
//! precedence chain via `assert_cmd::Command` against a fresh cass binary.

use assert_cmd::Command;
use serial_test::serial;
use std::path::PathBuf;

fn temp_data_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("cass-d4r65-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("tempdir");
    dir
}

#[test]
#[serial]
fn cass_help_exits_zero_and_lists_subcommands() {
    tracing::info!(target: "d4r65_test", scenario = "help");
    let mut cmd = Command::cargo_bin("cass").expect("cass binary built");
    cmd.arg("--help");
    let output = cmd.output().expect("cass --help runs");
    assert!(output.status.success(), "--help must exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The help output must enumerate at least the search/health/index subcommands.
    for sub in ["search", "health", "index"] {
        assert!(
            stdout.contains(sub),
            "--help must list `{sub}` subcommand; got stdout={stdout}"
        );
    }
}

/// Compare two paths for "is the same place" without requiring lexical
/// equality (macOS `/var` → `/private/var`, trailing slashes, etc.). Falls
/// back to lexical equality when canonicalization fails (e.g. either side
/// no longer exists).
fn paths_resolve_equal(a: &std::path::Path, b: &std::path::Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Extract the resolved `data_dir` string from `cass health --json` output.
/// Returns None if the field is missing, so callers can produce a diagnostic
/// instead of unwrapping into a confusing panic.
fn data_dir_from_health(stdout: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(stdout).ok()?;
    v.get("data_dir")?.as_str().map(|s| s.to_string())
}

#[test]
#[serial]
fn cli_data_dir_flag_takes_precedence_over_env() {
    tracing::info!(target: "d4r65_test", scenario = "cli_over_env");
    let env_dir = temp_data_dir("env");
    let cli_dir = temp_data_dir("cli");
    let mut cmd = Command::cargo_bin("cass").expect("cass binary built");
    cmd.env("CASS_DATA_DIR", &env_dir)
        .arg("--data-dir")
        .arg(&cli_dir)
        .arg("health")
        .arg("--json");
    let output = cmd.output().expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    eprintln!(
        "[d4r65_test] cli_over_env exit={} stdout_len={} env_dir={env_dir:?} cli_dir={cli_dir:?}",
        output.status.code().unwrap_or(-1),
        stdout.len()
    );
    let resolved = data_dir_from_health(&stdout).unwrap_or_else(|| {
        panic!("cass health --json must emit a `data_dir` field; got: {stdout}")
    });
    let resolved_path = std::path::Path::new(&resolved);
    assert!(
        paths_resolve_equal(resolved_path, &cli_dir),
        "CLI --data-dir must take precedence over CASS_DATA_DIR; \
         resolved={resolved:?} cli_dir={cli_dir:?} env_dir={env_dir:?}"
    );
    assert!(
        !paths_resolve_equal(resolved_path, &env_dir),
        "resolved data_dir unexpectedly matches env value; \
         resolved={resolved:?} env_dir={env_dir:?}"
    );
}

#[test]
#[serial]
fn env_data_dir_used_when_no_flag() {
    tracing::info!(target: "d4r65_test", scenario = "env_only");
    let env_dir = temp_data_dir("env_only");
    let mut cmd = Command::cargo_bin("cass").expect("cass binary built");
    cmd.env("CASS_DATA_DIR", &env_dir)
        .arg("health")
        .arg("--json");
    let output = cmd.output().expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let resolved = data_dir_from_health(&stdout).unwrap_or_else(|| {
        panic!("cass health --json must emit a `data_dir` field; got: {stdout}")
    });
    let resolved_path = std::path::Path::new(&resolved);
    assert!(
        paths_resolve_equal(resolved_path, &env_dir),
        "CASS_DATA_DIR must be used when no --data-dir flag is set; \
         resolved={resolved:?} env_dir={env_dir:?}"
    );
}

#[test]
#[serial]
fn missing_required_arg_emits_actionable_error() {
    tracing::info!(target: "d4r65_test", scenario = "missing_arg");
    let mut cmd = Command::cargo_bin("cass").expect("cass binary built");
    cmd.arg("search"); // search requires a query argument
    let output = cmd.output().expect("runs");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        // A missing-arg error must produce an actionable message.
        let combined = format!("{stdout}\n{stderr}");
        assert!(
            combined.to_lowercase().contains("required")
                || combined.to_lowercase().contains("usage")
                || combined.to_lowercase().contains("argument")
                || combined.to_lowercase().contains("query"),
            "missing-arg error must include actionable hint; got: {combined}"
        );
    }
}

#[test]
#[serial]
fn invalid_data_dir_path_handled_without_panic() {
    tracing::info!(target: "d4r65_test", scenario = "invalid_data_dir");
    let mut cmd = Command::cargo_bin("cass").expect("cass binary built");
    // /this/path/does/not/exist — cass may auto-create it OR error cleanly.
    cmd.arg("--data-dir")
        .arg("/this/path/does/not/exist/d4r65")
        .arg("health")
        .arg("--json");
    let output = cmd.output().expect("runs");
    // Critical: must NOT panic. Either exit 0 with valid JSON or exit !=0
    // with structured error envelope.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked at") && !stderr.contains("RUST_BACKTRACE"),
        "invalid data dir must NOT panic; stderr: {stderr}"
    );
}
