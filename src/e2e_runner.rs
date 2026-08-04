//! Shared bounded E2E command runner with structured JSONL events.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.12.2
//! ("Build shared bounded E2E runner with structured detailed logs").
//!
//! Resilience E2E tests repeatedly hit commands that were too slow, noisy,
//! blocked on dependency logs, or ambiguous when run against the real binary. A
//! useful gate must be **bounded, parseable, and debuggable after the fact**.
//!
//! This module is the reusable core: it runs an arbitrary command (the
//! caller passes the resolved `cass` binary path — never bare interactive cass)
//! under a hard timeout with isolated env, captures stdout and stderr
//! *separately* (preserving the stdout=data / stderr=diagnostics contract),
//! classifies the outcome into one explicit [`RunOutcome`], and emits one
//! [`RunEvent`] (serializable to a JSONL line) carrying everything a future
//! agent needs to debug without rerunning: command line, binary path/version,
//! env overrides, cwd, fixture id, phase, timestamps, elapsed_ms, exit code,
//! timeout/signal status, `parsed_json_ok`, assertion results, and artifact
//! paths. A concise human summary is derived from the same event.
//!
//! The execution core is deliberately command-agnostic so it is unit-testable
//! with portable stand-in commands; the live cass-surface scenarios
//! (health/status/search/view/doctor/source/fleet) and the CI/quick/live mode
//! recipe drive this same runner from the integration tier.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Stable schema version for the run-event wire format.
pub const E2E_RUNNER_SCHEMA_VERSION: u32 = 1;

/// Execution mode. Quick/CI are deterministic and never require live hosts;
/// Live is opt-in and must never be mandatory for CI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// Local fast subset.
    Quick,
    /// Full deterministic CI suite (no live hosts).
    Ci,
    /// Opt-in live fleet mode (real remote hosts).
    Live,
}

impl RunMode {
    /// Stable wire label.
    pub fn as_str(self) -> &'static str {
        match self {
            RunMode::Quick => "quick",
            RunMode::Ci => "ci",
            RunMode::Live => "live",
        }
    }
}

/// The explicit outcome of a single bounded run — the failure taxonomy a
/// debugging agent branches on. Exactly one is reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunOutcome {
    /// Command exited 0, JSON parsed (if expected), all assertions passed.
    Success,
    /// Command exited non-zero.
    CommandFailure { exit_code: i32 },
    /// Command exceeded the bounded timeout and was killed.
    Timeout,
    /// Expected JSON on stdout could not be parsed.
    InvalidJson,
    /// An assertion against the output failed.
    AssertionFailure { failed: Vec<String> },
    /// A required fixture/input path was absent before running.
    MissingFixture { path: String },
    /// The run produced no usable log/artifact evidence (e.g. expected
    /// artifact path missing after a nominal run).
    LogArtifactLoss { detail: String },
}

impl RunOutcome {
    /// Stable kind label (mirrors the serde tag).
    pub fn kind(&self) -> &'static str {
        match self {
            RunOutcome::Success => "success",
            RunOutcome::CommandFailure { .. } => "command_failure",
            RunOutcome::Timeout => "timeout",
            RunOutcome::InvalidJson => "invalid_json",
            RunOutcome::AssertionFailure { .. } => "assertion_failure",
            RunOutcome::MissingFixture { .. } => "missing_fixture",
            RunOutcome::LogArtifactLoss { .. } => "log_artifact_loss",
        }
    }

    /// Whether the run fully succeeded.
    pub fn is_success(&self) -> bool {
        matches!(self, RunOutcome::Success)
    }
}

/// Raw process result captured by execution, before outcome classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRun {
    /// Process exit code, when it exited normally.
    pub exit_code: Option<i32>,
    /// Killed because it hit the timeout.
    pub timed_out: bool,
    /// Terminating signal number, when killed by a signal.
    pub signal: Option<i32>,
    /// Captured stdout (data channel).
    pub stdout: String,
    /// Captured stderr (diagnostics channel).
    pub stderr: String,
    /// Wall-clock the run took.
    pub elapsed_ms: u64,
}

/// A named assertion over a run's `(stdout, stderr)`; returns `true` when it
/// passes.
pub type OutputAssertion<'a> = (String, Box<dyn Fn(&str, &str) -> bool + 'a>);

/// What to check about a run's output: whether stdout must be valid JSON, and
/// named assertions over the captured output.
#[derive(Default)]
pub struct RunExpectation<'a> {
    /// stdout must parse as JSON.
    pub expect_json: bool,
    /// Named assertions, evaluated against (stdout, stderr).
    pub assertions: Vec<OutputAssertion<'a>>,
}

/// Classify a raw run + expectation outcome into the explicit taxonomy. Pure
/// and unit-testable. Resolution order: timeout, then exit code, then JSON
/// validity, then assertions.
pub fn classify_outcome(raw: &RawRun, expect: &RunExpectation<'_>) -> RunOutcome {
    if raw.timed_out {
        return RunOutcome::Timeout;
    }
    match raw.exit_code {
        Some(0) => {}
        Some(code) => return RunOutcome::CommandFailure { exit_code: code },
        None => {
            // No normal exit and not flagged timeout => treat as a command
            // failure surfaced via signal (-1 sentinel keeps the field present).
            return RunOutcome::CommandFailure {
                exit_code: raw.signal.map(|s| -s).unwrap_or(-1),
            };
        }
    }
    let parsed_json_ok = if expect.expect_json {
        serde_json::from_str::<serde_json::Value>(raw.stdout.trim()).is_ok()
    } else {
        true
    };
    if expect.expect_json && !parsed_json_ok {
        return RunOutcome::InvalidJson;
    }
    let failed: Vec<String> = expect
        .assertions
        .iter()
        .filter(|(_, check)| !check(&raw.stdout, &raw.stderr))
        .map(|(name, _)| name.clone())
        .collect();
    if !failed.is_empty() {
        return RunOutcome::AssertionFailure { failed };
    }
    RunOutcome::Success
}

/// One structured run event — serialized as a single JSONL line per run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvent {
    /// Mirrors [`E2E_RUNNER_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Execution mode.
    pub mode: RunMode,
    /// Full command line as invoked (binary + args).
    pub command_line: Vec<String>,
    /// Resolved binary path.
    pub binary_path: String,
    /// Binary version, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_version: Option<String>,
    /// Binary content hash, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_hash: Option<String>,
    /// Environment overrides applied for isolation (e.g. CASS data/config/model
    /// dirs), redacted to keys+values the caller set.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_overrides: BTreeMap<String, String>,
    /// Working directory.
    pub cwd: String,
    /// Fixture identifier, when this run used one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixture_id: Option<String>,
    /// Logical phase (e.g. "setup", "probe", "assert").
    pub phase: String,
    /// Start/end epoch millis (caller-supplied for determinism).
    pub start_ms: u64,
    /// End epoch millis.
    pub end_ms: u64,
    /// Measured elapsed wall-clock.
    pub elapsed_ms: u64,
    /// Process exit code, when it exited normally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Killed by hitting the timeout.
    pub timed_out: bool,
    /// Terminating signal, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// Whether stdout parsed as JSON (true when JSON was not expected).
    pub parsed_json_ok: bool,
    /// Explicit outcome.
    pub outcome: RunOutcome,
    /// Names of assertions that failed (empty on success).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assertion_failures: Vec<String>,
    /// Artifact paths written for this run (logs, captured output, etc.).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_paths: Vec<String>,
    /// Captured stdout byte length (full text goes to artifacts, not the event).
    pub stdout_len: usize,
    /// Captured stderr byte length.
    pub stderr_len: usize,
}

impl RunEvent {
    /// Serialize to a single JSONL line.
    pub fn to_jsonl(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// A concise one-line human summary derived from the same event.
    pub fn human_summary(&self) -> String {
        let cmd = self.command_line.join(" ");
        format!(
            "[{}] {} -> {} ({}ms, exit={})",
            self.mode.as_str(),
            cmd,
            self.outcome.kind(),
            self.elapsed_ms,
            self.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| if self.timed_out {
                    "timeout".into()
                } else {
                    "signal".into()
                }),
        )
    }
}

/// Configuration for a bounded run.
pub struct RunSpec {
    /// Resolved binary path (e.g. the test-built cass). Never bare cass.
    pub binary_path: String,
    /// Arguments (callers add robot/json + --color=never as appropriate).
    pub args: Vec<String>,
    /// Hard timeout.
    pub timeout: Duration,
    /// Isolation env overrides (e.g. CASS_DATA_DIR, HOME).
    pub env_overrides: BTreeMap<String, String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Optional fixture id + a path that must exist before running.
    pub fixture_id: Option<String>,
    /// Required fixture path; missing => MissingFixture without executing.
    pub require_path: Option<PathBuf>,
    /// Logical phase label.
    pub phase: String,
    /// Execution mode.
    pub mode: RunMode,
}

/// Spawn the command and wait up to `timeout`, draining stdout/stderr on
/// background threads so a chatty process cannot deadlock on a full pipe, and
/// killing the process if the deadline passes.
fn execute_bounded(spec: &RunSpec) -> std::io::Result<RawRun> {
    let start = Instant::now();
    let mut cmd = Command::new(&spec.binary_path);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &spec.env_overrides {
        cmd.env(k, v);
    }
    // Own process group so a timeout kill reaches grandchildren too (otherwise
    // an orphaned grandchild keeps the stdout pipe open and the drain blocks,
    // defeating the bound).
    crate::sources::configure_child_process_group(&mut cmd);
    let mut child = cmd.spawn()?;
    let pid = child.id();

    // Drain pipes on threads to avoid deadlock on large output.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });

    let deadline = start + spec.timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    // Kill the whole group so orphaned grandchildren die and
                    // release the pipes, keeping the drain (and thus the run)
                    // bounded.
                    kill_process_group(pid);
                    let _ = child.kill();
                    timed_out = true;
                    break child.wait()?;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };

    // Measure elapsed at process completion/kill time, BEFORE draining: a
    // (group-killed) pipe close is prompt, but the metric must reflect the
    // bounded run, not drain bookkeeping.
    let elapsed_ms = start.elapsed().as_millis() as u64;
    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();

    let (exit_code, signal) = decode_status(&status);
    Ok(RawRun {
        exit_code: if timed_out { None } else { exit_code },
        timed_out,
        signal,
        stdout,
        stderr,
        elapsed_ms,
    })
}

/// Kill an entire process group (the child placed itself in its own group via
/// `process_group(0)`). Uses `/bin/kill -KILL -<pid>` to avoid a libc
/// dependency, matching the sources runner's approach.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let group = format!("-{pid}");
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

#[cfg(unix)]
fn decode_status(status: &std::process::ExitStatus) -> (Option<i32>, Option<i32>) {
    use std::os::unix::process::ExitStatusExt;
    (status.code(), status.signal())
}

#[cfg(not(unix))]
fn decode_status(status: &std::process::ExitStatus) -> (Option<i32>, Option<i32>) {
    (status.code(), None)
}

/// Run a command spec under the bounded runner and produce a structured event.
/// `now_ms` is the caller-supplied wall-clock start (kept out of the runner for
/// deterministic tests); `expect` drives JSON/assertion classification.
///
/// A missing required fixture short-circuits to [`RunOutcome::MissingFixture`]
/// **without executing** the binary.
pub fn run(spec: &RunSpec, expect: &RunExpectation<'_>, now_ms: u64) -> RunEvent {
    let command_line = {
        let mut v = vec![spec.binary_path.clone()];
        v.extend(spec.args.iter().cloned());
        v
    };
    let base =
        |raw: &RawRun, outcome: RunOutcome, parsed_json_ok: bool, failures: Vec<String>| RunEvent {
            schema_version: E2E_RUNNER_SCHEMA_VERSION,
            mode: spec.mode,
            command_line: command_line.clone(),
            binary_path: spec.binary_path.clone(),
            binary_version: None,
            binary_hash: None,
            env_overrides: spec.env_overrides.clone(),
            cwd: spec.cwd.display().to_string(),
            fixture_id: spec.fixture_id.clone(),
            phase: spec.phase.clone(),
            start_ms: now_ms,
            end_ms: now_ms.saturating_add(raw.elapsed_ms),
            elapsed_ms: raw.elapsed_ms,
            exit_code: raw.exit_code,
            timed_out: raw.timed_out,
            signal: raw.signal,
            parsed_json_ok,
            outcome,
            assertion_failures: failures,
            artifact_paths: Vec::new(),
            stdout_len: raw.stdout.len(),
            stderr_len: raw.stderr.len(),
        };

    // Fixture precondition: report MissingFixture without executing.
    if let Some(path) = &spec.require_path
        && !path.exists()
    {
        let raw = RawRun {
            exit_code: None,
            timed_out: false,
            signal: None,
            stdout: String::new(),
            stderr: String::new(),
            elapsed_ms: 0,
        };
        return base(
            &raw,
            RunOutcome::MissingFixture {
                path: path.display().to_string(),
            },
            true,
            Vec::new(),
        );
    }

    let raw = match execute_bounded(spec) {
        Ok(raw) => raw,
        Err(err) => {
            let raw = RawRun {
                exit_code: Some(-1),
                timed_out: false,
                signal: None,
                stdout: String::new(),
                stderr: format!("spawn failed: {err}"),
                elapsed_ms: 0,
            };
            return base(
                &raw,
                RunOutcome::CommandFailure { exit_code: -1 },
                true,
                Vec::new(),
            );
        }
    };

    let parsed_json_ok = if expect.expect_json {
        serde_json::from_str::<serde_json::Value>(raw.stdout.trim()).is_ok()
    } else {
        true
    };
    let outcome = classify_outcome(&raw, expect);
    let failures = match &outcome {
        RunOutcome::AssertionFailure { failed } => failed.clone(),
        _ => Vec::new(),
    };
    base(&raw, outcome, parsed_json_ok, failures)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A portable stand-in command via `sh -c`, so the runner mechanics are
    /// proven without building the cass binary. The live cass-surface scenarios
    /// drive this same `run()` from the integration tier.
    fn sh_spec(script: &str, timeout_ms: u64) -> RunSpec {
        RunSpec {
            binary_path: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            timeout: Duration::from_millis(timeout_ms),
            env_overrides: BTreeMap::new(),
            cwd: std::env::temp_dir(),
            fixture_id: None,
            require_path: None,
            phase: "test".to_string(),
            mode: RunMode::Quick,
        }
    }

    #[test]
    fn success_with_valid_json_classifies_success() {
        let spec = sh_spec("printf '{\"ok\":true}'", 5_000);
        let mut expect = RunExpectation {
            expect_json: true,
            ..Default::default()
        };
        expect.assertions.push((
            "has_ok".to_string(),
            Box::new(|out: &str, _err: &str| out.contains("\"ok\"")),
        ));
        let ev = run(&spec, &expect, 1_000);
        assert_eq!(ev.outcome, RunOutcome::Success);
        assert!(ev.parsed_json_ok);
        assert_eq!(ev.exit_code, Some(0));
        assert!(!ev.timed_out);
        assert_eq!(ev.end_ms, 1_000 + ev.elapsed_ms);
    }

    #[test]
    fn nonzero_exit_is_command_failure() {
        let ev = run(&sh_spec("exit 3", 5_000), &RunExpectation::default(), 0);
        assert_eq!(ev.outcome, RunOutcome::CommandFailure { exit_code: 3 });
        assert_eq!(ev.exit_code, Some(3));
    }

    #[test]
    fn slow_command_hits_bounded_timeout() {
        let ev = run(&sh_spec("sleep 5", 150), &RunExpectation::default(), 0);
        assert_eq!(ev.outcome, RunOutcome::Timeout);
        assert!(ev.timed_out);
        assert_eq!(ev.exit_code, None);
        // Bounded: well under the 5s the command wanted.
        assert!(
            ev.elapsed_ms < 3_000,
            "timeout was not bounded: {}ms",
            ev.elapsed_ms
        );
    }

    #[test]
    fn invalid_json_when_json_expected() {
        let spec = sh_spec("printf 'not json at all'", 5_000);
        let expect = RunExpectation {
            expect_json: true,
            ..Default::default()
        };
        let ev = run(&spec, &expect, 0);
        assert_eq!(ev.outcome, RunOutcome::InvalidJson);
        assert!(!ev.parsed_json_ok);
    }

    #[test]
    fn failed_assertion_is_reported_with_name() {
        let spec = sh_spec("printf 'hello'", 5_000);
        let mut expect = RunExpectation::default();
        expect.assertions.push((
            "contains_world".to_string(),
            Box::new(|out: &str, _e: &str| out.contains("world")),
        ));
        let ev = run(&spec, &expect, 0);
        assert_eq!(
            ev.outcome,
            RunOutcome::AssertionFailure {
                failed: vec!["contains_world".to_string()]
            }
        );
        assert_eq!(ev.assertion_failures, vec!["contains_world".to_string()]);
    }

    #[test]
    fn missing_fixture_short_circuits_without_executing() {
        let mut spec = sh_spec("echo should-not-run", 5_000);
        spec.require_path = Some(PathBuf::from("/no/such/fixture/path-xyz"));
        let ev = run(&spec, &RunExpectation::default(), 0);
        assert!(matches!(ev.outcome, RunOutcome::MissingFixture { .. }));
        // Did not execute: no elapsed, no exit.
        assert_eq!(ev.exit_code, None);
        assert_eq!(ev.stdout_len, 0);
    }

    #[test]
    fn stdout_and_stderr_are_captured_separately() {
        let spec = sh_spec("printf 'DATA' ; printf 'DIAG' 1>&2", 5_000);
        let ev = run(&spec, &RunExpectation::default(), 0);
        assert_eq!(ev.outcome, RunOutcome::Success);
        assert_eq!(ev.stdout_len, 4); // "DATA"
        assert_eq!(ev.stderr_len, 4); // "DIAG"
    }

    #[test]
    fn run_event_jsonl_and_summary_are_stable_and_round_trip() {
        let ev = run(
            &sh_spec("printf '{}'", 5_000),
            &RunExpectation {
                expect_json: true,
                ..Default::default()
            },
            42,
        );
        let line = ev.to_jsonl();
        // One line, no embedded newline.
        assert!(!line.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["schema_version"], E2E_RUNNER_SCHEMA_VERSION);
        assert_eq!(value["mode"], "quick");
        assert_eq!(value["outcome"]["kind"], "success");
        let back: RunEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(back, ev);
        assert!(ev.human_summary().contains("success"));
    }

    #[test]
    fn classify_outcome_precedence_is_timeout_then_exit_then_json_then_assert() {
        // timeout wins even with a non-zero exit recorded.
        let raw = RawRun {
            exit_code: Some(2),
            timed_out: true,
            signal: None,
            stdout: String::new(),
            stderr: String::new(),
            elapsed_ms: 10,
        };
        assert_eq!(
            classify_outcome(&raw, &RunExpectation::default()),
            RunOutcome::Timeout
        );
        // exit wins over json.
        let raw = RawRun {
            exit_code: Some(2),
            timed_out: false,
            signal: None,
            stdout: "bad".into(),
            stderr: String::new(),
            elapsed_ms: 1,
        };
        assert_eq!(
            classify_outcome(
                &raw,
                &RunExpectation {
                    expect_json: true,
                    ..Default::default()
                }
            ),
            RunOutcome::CommandFailure { exit_code: 2 }
        );
    }
}
