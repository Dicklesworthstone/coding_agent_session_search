//! Real-binary E2E gate for the `cass onboarding` first-run readiness surface
//! (bead `coding_agent_session_search-guided-ops-repro-trust-5u82n.6`,
//! "Build first-run source onboarding and readiness wizard").
//!
//! `src/source_onboarding.rs` is the pure, unit-tested decision core
//! (`OnboardingObservation` → `OnboardingReport`). This gate proves the live
//! surface: that the real `cass` binary gathers a live observation and emits a
//! deterministic report via `cass onboarding --json`, that the recommendation
//! tracks the machine state (empty → `discover_sources`; a seeded+indexed
//! machine → `ready_to_search`), and — critically — that the surface is
//! **read-only**: running it on an empty machine creates no archive DB and the
//! report self-reports `mutation_free=true`.
//!
//! Isolation mirrors the other gates: a fresh `tempdir` with `HOME`/`XDG_*`/cwd
//! redirected into it, agent-home env vars pointed at empty subdirs,
//! `CASS_IGNORE_SOURCES_CONFIG=1` for the empty case, `CASS_SEMANTIC_EMBEDDER=hash`
//! to keep semantic acquisition offline, and `NO_COLOR=1`. The `.12.2` bounded
//! runner turns a hang into a loud diagnostic instead of a silent pass.
//!
//! Written panic-free (Result + an `ensure` helper) so the new file stays UBS
//! 0-critical.

mod util;

use std::error::Error;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::Value;

use util::timeout::spawn_with_timeout_or_diag;

type TestResult = Result<(), Box<dyn Error>>;

const INDEX_TIMEOUT: Duration = Duration::from_secs(120);
const ONBOARDING_TIMEOUT: Duration = Duration::from_secs(60);
const KEYWORD: &str = "onboardingfixtureunique";
const SEEDED_SESSION: &str = "rollout-2026-04-23T10-00-00-onboarding.jsonl";

fn ensure(cond: bool, msg: impl FnOnce() -> String) -> TestResult {
    if cond { Ok(()) } else { Err(msg().into()) }
}

fn head(s: &str) -> String {
    s.chars().take(400).collect()
}

fn spawn_pty_capture(
    mut reader: Box<dyn Read + Send>,
) -> (Arc<Mutex<Vec<u8>>>, thread::JoinHandle<()>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_for_thread = Arc::clone(&captured);
    let handle = thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let Some(bytes) = chunk.get(..n) else {
                        break;
                    };
                    if let Ok(mut output) = captured_for_thread.lock() {
                        output.extend_from_slice(bytes);
                    } else {
                        break;
                    }
                }
            }
        }
    });
    (captured, handle)
}

fn wait_for_pty_text(
    captured: &Arc<Mutex<Vec<u8>>>,
    needle: &str,
    timeout: Duration,
) -> TestResult {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        let found = captured
            .lock()
            .map(|output| String::from_utf8_lossy(&output).contains(needle))
            .unwrap_or(false);
        if found {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }

    let output = captured
        .lock()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_else(|_| "<PTY capture lock poisoned>".to_string());
    Err(format!("timed out waiting for PTY text `{needle}`; output:\n{output}").into())
}

/// A fresh isolated `(tempdir guard, home, data_dir)`.
fn isolated_home() -> Result<(tempfile::TempDir, PathBuf, PathBuf), Box<dyn Error>> {
    let tmp = tempfile::TempDir::new()?;
    let home = tmp.path().join("home");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&data_dir)?;
    Ok((tmp, home, data_dir))
}

/// Build a `cass` command with isolation env. When `codex_home` is `Some`, point
/// `CODEX_HOME` at it (the indexed case); otherwise ignore sources config and
/// pin the agent homes at empty subdirs so detection finds nothing.
fn cass_cmd(home: &Path, codex_home: Option<&Path>, args: &[String]) -> Command {
    let mut cmd = Command::new(cargo_bin("cass"));
    cmd.args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("NO_COLOR", "1")
        .env_remove("CLAUDE_CONFIG_DIR");
    match codex_home {
        Some(ch) => {
            cmd.env("CODEX_HOME", ch);
        }
        None => {
            cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1")
                .env("CODEX_HOME", home.join(".codex-empty"))
                .env("CLAUDE_HOME", home.join(".claude-empty"))
                .env("GEMINI_HOME", home.join(".gemini-empty"));
        }
    }
    cmd
}

fn argv(base: &[&str], data_dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = base.iter().map(|s| (*s).to_string()).collect();
    v.push("--data-dir".to_string());
    v.push(data_dir.to_string_lossy().into_owned());
    v
}

/// Run `cass onboarding --json` and return the parsed report payload.
fn run_onboarding(
    home: &Path,
    codex_home: Option<&Path>,
    data_dir: &Path,
) -> Result<Value, Box<dyn Error>> {
    let args = argv(&["onboarding", "--json"], data_dir);
    let cmd = cass_cmd(home, codex_home, &args);
    let out = spawn_with_timeout_or_diag(cmd, "onboarding", Some(data_dir), ONBOARDING_TIMEOUT);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("onboarding stdout not JSON: {e}; head: {}", head(&stdout)))?;
    Ok(value)
}

fn action_of(report: &Value) -> &str {
    report
        .get("recommended_action")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Assert invariants every onboarding report must satisfy.
fn check_report_shape(report: &Value) -> TestResult {
    ensure(
        report.get("schema_version").and_then(Value::as_u64) == Some(1),
        || "onboarding schema_version must be 1".to_string(),
    )?;
    ensure(
        report.get("mutation_free").and_then(Value::as_bool) == Some(true),
        || "onboarding must self-report mutation_free=true".to_string(),
    )?;
    let action = action_of(report);
    ensure(
        matches!(
            action,
            "discover_sources" | "fix_source_permissions" | "run_first_index" | "ready_to_search"
        ),
        || format!("unexpected recommended_action `{action}`"),
    )?;
    let command = report
        .get("recommended_command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // The recommended command is never a destructive operation.
    ensure(
        !command.contains("rm ")
            && !command.contains("--delete")
            && !command.contains("reset --hard"),
        || format!("recommended_command must be non-destructive, got `{command}`"),
    )?;
    Ok(())
}

#[test]
fn onboarding_empty_machine_is_readonly_and_recommends_discovery() -> TestResult {
    let (_tmp, home, data_dir) = isolated_home()?;

    let report = run_onboarding(&home, None, &data_dir)?;
    check_report_shape(&report)?;
    ensure(action_of(&report) == "discover_sources", || {
        format!(
            "empty machine should recommend discover_sources, got `{}`",
            action_of(&report)
        )
    })?;
    ensure(
        report
            .get("indexed_conversation_count")
            .and_then(Value::as_u64)
            == Some(0),
        || "empty machine should report 0 indexed conversations".to_string(),
    )?;

    // Read-only proof: onboarding must NOT create the archive DB.
    let db_path = data_dir.join("agent_search.db");
    ensure(!db_path.exists(), || {
        "onboarding on an empty machine must not create the archive DB".to_string()
    })?;

    Ok(())
}

#[test]
fn onboarding_indexed_machine_is_ready_to_search() -> TestResult {
    let (_tmp, home, data_dir) = isolated_home()?;
    let codex_home = home.join(".codex");
    util::seed_codex_session(&codex_home, SEEDED_SESSION, KEYWORD, true);

    // Index the seeded session once.
    let index_args = argv(
        &["index", "--full", "--json", "--no-progress-events"],
        &data_dir,
    );
    let index_cmd = cass_cmd(&home, Some(&codex_home), &index_args);
    let index_out = spawn_with_timeout_or_diag(
        index_cmd,
        "onboarding_index",
        Some(&data_dir),
        INDEX_TIMEOUT,
    );
    let index_stdout = String::from_utf8_lossy(&index_out.stdout);
    let index_json: Value = serde_json::from_str(index_stdout.trim())
        .map_err(|e| format!("index stdout not JSON: {e}; head: {}", head(&index_stdout)))?;
    ensure(
        index_json.get("success").and_then(Value::as_bool) == Some(true),
        || {
            format!(
                "index did not report success=true: {}",
                head(&index_json.to_string())
            )
        },
    )?;

    let report = run_onboarding(&home, Some(&codex_home), &data_dir)?;
    check_report_shape(&report)?;
    ensure(action_of(&report) == "ready_to_search", || {
        format!(
            "indexed machine should recommend ready_to_search, got `{}`",
            action_of(&report)
        )
    })?;
    ensure(
        report
            .get("indexed_conversation_count")
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 0),
        || "indexed machine should report >0 indexed conversations".to_string(),
    )?;
    Ok(())
}

#[test]
fn onboarding_tty_runs_readonly_wizard_and_exits_without_bare_tui() -> TestResult {
    let (_tmp, home, data_dir) = isolated_home()?;
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 32,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let reader = pair.master.try_clone_reader()?;
    let (captured, capture_thread) = spawn_pty_capture(reader);
    let mut writer = pair.master.take_writer()?;

    let cass_bin = cargo_bin("cass");
    let mut command = CommandBuilder::new(cass_bin.to_string_lossy().as_ref());
    command.arg("onboarding");
    command.arg("--data-dir");
    command.arg(data_dir.to_string_lossy().as_ref());
    command.cwd(home.to_string_lossy().as_ref());
    command.env("HOME", home.to_string_lossy().as_ref());
    command.env(
        "XDG_DATA_HOME",
        home.join("xdg-data").to_string_lossy().as_ref(),
    );
    command.env(
        "XDG_CONFIG_HOME",
        home.join("xdg-config").to_string_lossy().as_ref(),
    );
    command.env(
        "XDG_CACHE_HOME",
        home.join("xdg-cache").to_string_lossy().as_ref(),
    );
    command.env("CASS_IGNORE_SOURCES_CONFIG", "1");
    command.env(
        "CODEX_HOME",
        home.join(".codex-empty").to_string_lossy().as_ref(),
    );
    command.env(
        "CLAUDE_HOME",
        home.join(".claude-empty").to_string_lossy().as_ref(),
    );
    command.env(
        "GEMINI_HOME",
        home.join(".gemini-empty").to_string_lossy().as_ref(),
    );
    command.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    command.env("CASS_SEMANTIC_EMBEDDER", "hash");
    command.env("NO_COLOR", "1");
    command.env("TERM", "xterm-256color");

    let mut child = pair.slave.spawn_command(command)?;
    wait_for_pty_text(
        &captured,
        "What would you like to review?",
        ONBOARDING_TIMEOUT,
    )?;

    // Accept the default "safest next command" panel, then the default `no`
    // for reviewing another section. No recommended command is executed.
    writer.write_all(b"\r")?;
    writer.flush()?;
    wait_for_pty_text(&captured, "Review another section?", ONBOARDING_TIMEOUT)?;
    writer.write_all(b"\r")?;
    writer.flush()?;

    let wait_started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if wait_started.elapsed() >= ONBOARDING_TIMEOUT {
            child.kill()?;
            return Err("interactive onboarding wizard did not exit after its final prompt".into());
        }
        thread::sleep(Duration::from_millis(25));
    };

    drop(writer);
    drop(pair);
    let _ = capture_thread.join();
    ensure(status.success(), || {
        format!("interactive onboarding exited unsuccessfully: {status}")
    })?;

    let output = captured
        .lock()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|_| "PTY capture lock poisoned")?;
    let expected_fragments = [
        "CASS onboarding",
        "Read-only guided review",
        "What would you like to review?",
        "Recommended next command",
        "cass sources discover --json",
        "This wizard did not run that command.",
        "No changes were made.",
    ];
    if let Some(expected) = expected_fragments
        .into_iter()
        .find(|expected| !output.contains(expected))
    {
        return Err(format!("interactive output missing `{expected}`; output:\n{output}").into());
    }
    ensure(!data_dir.join("agent_search.db").exists(), || {
        "interactive onboarding must not create the archive DB".to_string()
    })?;
    Ok(())
}
