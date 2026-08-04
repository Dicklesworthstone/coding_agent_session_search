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
//! with portable stand-in commands. The deterministic real-cass matrix drives
//! it from the integration tier; separately declared live-host scenarios remain
//! opt-in until an operator adapter is implemented.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Stable schema version for the run-event wire format.
pub const E2E_RUNNER_SCHEMA_VERSION: u32 = 2;

/// Stable schema version for the scenario artifact manifest.
pub const E2E_SCENARIO_MANIFEST_SCHEMA_VERSION: u32 = 1;

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
    /// dirs). Secret-bearing keys are omitted rather than persisted.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_overrides: BTreeMap<String, String>,
    /// Secret-bearing environment keys omitted from `env_overrides`. Values are
    /// never retained.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redacted_env_keys: Vec<String>,
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
    /// Every assertion that actually ran, including passes. An empty vector is
    /// therefore distinguishable from a generated-only artifact.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assertion_results: Vec<AssertionResult>,
    /// Artifact paths written for this run (logs, captured output, etc.).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_paths: Vec<String>,
    /// Captured stdout byte length (full text goes to artifacts, not the event).
    pub stdout_len: usize,
    /// Captured stderr byte length.
    pub stderr_len: usize,
}

/// One named assertion result retained in a [`RunEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssertionResult {
    pub name: String,
    pub passed: bool,
}

/// A structured run event plus the captured streams needed to persist proof
/// artifacts. Existing callers that only need the event continue to use
/// [`run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCapture {
    pub event: RunEvent,
    pub stdout: String,
    pub stderr: String,
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

/// Metadata and artifact policy for one scenario command.
pub struct ArtifactRunSpec<'a> {
    /// Root beneath which this run writes a new `<run>/<scenario>/<command>/`
    /// tree. Existing files are never reused or overwritten.
    pub artifact_root: &'a Path,
    pub run_id: &'a str,
    pub scenario_id: &'a str,
    pub command_id: &'a str,
    pub issue_ids: &'a [&'a str],
    pub privacy_note: &'a str,
    /// Start of the complete suite, in epoch milliseconds.
    pub suite_started_ms: u64,
    /// Exit codes that mean the diagnostic command behaved as expected. This
    /// permits an asserted `doctor`/`sources doctor` exit 1 to be a proof pass
    /// while preserving the actual exit code in [`RunEvent`].
    pub accepted_exit_codes: &'a [i32],
    /// Synthetic private/noise markers that must not appear in captured stdout
    /// or stderr.
    pub forbidden_stream_markers: &'a [&'a str],
    /// Scenario-owned fixture/trace artifacts that must exist and are cited by
    /// the manifest alongside the runner's standard four files.
    pub extra_artifact_paths: &'a [PathBuf],
}

/// One persisted scenario-command proof referenced by
/// [`ScenarioArtifactManifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioArtifactEntry {
    pub schema_version: u32,
    pub run_id: String,
    pub scenario_id: String,
    pub command_id: String,
    pub issue_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixture_id: Option<String>,
    pub privacy_note: String,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub actual_exit_code: Option<i32>,
    pub outcome: RunOutcome,
    pub proof_status: crate::proof_artifact::ProofStatus,
    pub assertions_ran: bool,
    pub generated_only: bool,
    pub redaction_safe: bool,
    pub fresh_for_suite: bool,
    pub stdout_path: String,
    pub stderr_path: String,
    pub event_path: String,
    pub proof_path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_artifact_paths: Vec<String>,
}

/// Complete proof manifest for a deterministic scenario suite.
///
/// `validation_errors()` is deliberately fail-closed: an empty manifest,
/// missing scenario, stale/reused entry, generated-only entry, unparseable
/// event/proof file, redaction failure, or any non-pass outcome is an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioArtifactManifest {
    pub schema_version: u32,
    pub run_id: String,
    pub mode: RunMode,
    pub suite_started_ms: u64,
    pub suite_finished_ms: u64,
    pub expected_scenario_ids: Vec<String>,
    /// Exact `<scenario_id>/<command_id>` keys required for this run.
    pub expected_command_keys: Vec<String>,
    pub expected_command_count: usize,
    pub entries: Vec<ScenarioArtifactEntry>,
}

impl ScenarioArtifactManifest {
    #[must_use]
    pub fn new(
        run_id: impl Into<String>,
        mode: RunMode,
        suite_started_ms: u64,
        expected_scenario_ids: Vec<String>,
        expected_command_keys: Vec<String>,
        expected_command_count: usize,
    ) -> Self {
        Self {
            schema_version: E2E_SCENARIO_MANIFEST_SCHEMA_VERSION,
            run_id: run_id.into(),
            mode,
            suite_started_ms,
            suite_finished_ms: suite_started_ms,
            expected_scenario_ids,
            expected_command_keys,
            expected_command_count,
            entries: Vec::new(),
        }
    }

    pub fn record(&mut self, entry: ScenarioArtifactEntry) {
        self.suite_finished_ms = self.suite_finished_ms.max(entry.finished_at_ms);
        self.entries.push(entry);
    }

    /// Return every reason this manifest is not a current, complete pass.
    #[must_use]
    pub fn validation_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.schema_version != E2E_SCENARIO_MANIFEST_SCHEMA_VERSION {
            errors.push(format!(
                "unsupported manifest schema_version {}",
                self.schema_version
            ));
        }
        if self.entries.is_empty() {
            errors.push("manifest contains no command records".to_string());
        }
        if self.entries.len() != self.expected_command_count {
            errors.push(format!(
                "expected {} command records, found {}",
                self.expected_command_count,
                self.entries.len()
            ));
        }
        if self.expected_command_count != self.expected_command_keys.len() {
            errors.push(format!(
                "manifest contract is ambiguous: expected_command_count={} but {} command keys were declared",
                self.expected_command_count,
                self.expected_command_keys.len()
            ));
        }

        let expected: BTreeSet<&str> = self
            .expected_scenario_ids
            .iter()
            .map(String::as_str)
            .collect();
        if expected.len() != self.expected_scenario_ids.len() {
            errors.push("manifest contract contains duplicate expected scenario ids".to_string());
        }
        let actual: BTreeSet<&str> = self
            .entries
            .iter()
            .map(|entry| entry.scenario_id.as_str())
            .collect();
        if expected != actual {
            errors.push(format!(
                "scenario coverage mismatch: expected={expected:?} actual={actual:?}"
            ));
        }

        let expected_command_keys: BTreeSet<&str> = self
            .expected_command_keys
            .iter()
            .map(String::as_str)
            .collect();
        if expected_command_keys.len() != self.expected_command_keys.len() {
            errors.push("manifest contract contains duplicate expected command keys".to_string());
        }
        let actual_command_keys = self
            .entries
            .iter()
            .map(|entry| format!("{}/{}", entry.scenario_id, entry.command_id))
            .collect::<Vec<_>>();
        let actual_command_key_set: BTreeSet<&str> =
            actual_command_keys.iter().map(String::as_str).collect();
        if actual_command_key_set.len() != actual_command_keys.len() {
            errors.push("manifest contains ambiguous duplicate command records".to_string());
        }
        if expected_command_keys != actual_command_key_set {
            errors.push(format!(
                "command coverage mismatch: expected={expected_command_keys:?} actual={actual_command_key_set:?}"
            ));
        }

        for entry in &self.entries {
            let label = format!("{}/{}", entry.scenario_id, entry.command_id);
            if entry.schema_version != E2E_SCENARIO_MANIFEST_SCHEMA_VERSION {
                errors.push(format!(
                    "{label}: unsupported entry schema_version {}",
                    entry.schema_version
                ));
            }
            if entry.run_id != self.run_id {
                errors.push(format!(
                    "{label}: entry run_id {:?} does not match manifest {:?}",
                    entry.run_id, self.run_id
                ));
            }
            if entry.issue_ids.is_empty() {
                errors.push(format!("{label}: entry cites no owning issue"));
            }
            if entry.privacy_note.trim().is_empty() {
                errors.push(format!("{label}: entry has no privacy note"));
            }
            if entry.started_at_ms < self.suite_started_ms
                || entry.finished_at_ms < entry.started_at_ms
                || entry.finished_at_ms > self.suite_finished_ms
                || !entry.fresh_for_suite
            {
                errors.push(format!(
                    "{label}: stale or internally inconsistent timestamps"
                ));
            }
            if !entry.outcome.is_success()
                || !entry.proof_status.is_trustworthy_pass()
                || !entry.assertions_ran
                || entry.generated_only
            {
                errors.push(format!(
                    "{label}: non-pass proof outcome={} status={} assertions_ran={} generated_only={}",
                    entry.outcome.kind(),
                    entry.proof_status.as_str(),
                    entry.assertions_ran,
                    entry.generated_only
                ));
            }
            if !entry.redaction_safe {
                errors.push(format!("{label}: artifact redaction scan failed"));
            }

            let required_paths = [
                ("stdout", &entry.stdout_path),
                ("stderr", &entry.stderr_path),
                ("event", &entry.event_path),
                ("proof", &entry.proof_path),
            ];
            for (kind, path) in required_paths {
                if !Path::new(path).is_file() {
                    errors.push(format!("{label}: missing {kind} artifact {path}"));
                }
            }
            for path in &entry.extra_artifact_paths {
                if !Path::new(path).is_file() {
                    errors.push(format!("{label}: missing scenario artifact {path}"));
                }
            }

            match fs::read(&entry.event_path) {
                Ok(bytes) => match serde_json::from_slice::<RunEvent>(&bytes) {
                    Ok(event) => {
                        if event.outcome != entry.outcome {
                            errors.push(format!("{label}: event/manifest outcome mismatch"));
                        }
                        if event.schema_version != E2E_RUNNER_SCHEMA_VERSION {
                            errors.push(format!(
                                "{label}: unsupported event schema_version {}",
                                event.schema_version
                            ));
                        }
                        if event.start_ms != entry.started_at_ms
                            || event.end_ms != entry.finished_at_ms
                            || event.exit_code != entry.actual_exit_code
                        {
                            errors.push(format!(
                                "{label}: event/manifest timing or exit-code mismatch"
                            ));
                        }
                        if event.command_line.is_empty()
                            || event
                                .binary_hash
                                .as_deref()
                                .is_none_or(|hash| hash.len() != 64)
                        {
                            errors.push(format!(
                                "{label}: event lacks exact command or binary hash provenance"
                            ));
                        }
                        if event.assertion_results.is_empty()
                            || event.assertion_results.iter().any(|result| !result.passed)
                        {
                            errors.push(format!(
                                "{label}: event has missing or failed assertion results"
                            ));
                        }
                        if event.env_overrides.keys().any(|key| is_secret_env_key(key)) {
                            errors.push(format!(
                                "{label}: event retained a secret-bearing environment key"
                            ));
                        }
                    }
                    Err(error) => {
                        errors.push(format!("{label}: event artifact is invalid JSON: {error}"));
                    }
                },
                Err(error) => {
                    errors.push(format!("{label}: event artifact is unreadable: {error}"));
                }
            }

            match fs::read(&entry.proof_path) {
                Ok(bytes) => {
                    match serde_json::from_slice::<crate::proof_artifact::ProofArtifact>(&bytes) {
                        Ok(proof)
                            if proof.status == entry.proof_status
                                && proof.run.assertions_ran
                                && proof.run.produced_artifact
                                && proof.run.completed
                                && proof.run.stdout_path.as_deref()
                                    == Some(entry.stdout_path.as_str())
                                && proof.run.stderr_path.as_deref()
                                    == Some(entry.stderr_path.as_str()) => {}
                        Ok(_) => errors.push(format!(
                            "{label}: proof/manifest status or provenance mismatch"
                        )),
                        Err(error) => {
                            errors
                                .push(format!("{label}: proof artifact is invalid JSON: {error}"));
                        }
                    }
                }
                Err(error) => {
                    errors.push(format!("{label}: proof artifact is unreadable: {error}"));
                }
            }
        }
        errors
    }

    #[must_use]
    pub fn is_clean_pass(&self) -> bool {
        self.validation_errors().is_empty()
    }

    /// Persist a pretty JSON manifest without overwriting prior evidence.
    pub fn write_json(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        write_new(path, &bytes)
    }

    /// Persist one manifest entry per JSONL line without overwriting prior
    /// evidence.
    pub fn write_jsonl(&self, path: &Path) -> io::Result<()> {
        let mut bytes = Vec::new();
        for entry in &self.entries {
            serde_json::to_writer(&mut bytes, entry).map_err(io::Error::other)?;
            bytes.push(b'\n');
        }
        write_new(path, &bytes)
    }
}

const SECRET_ENV_MARKERS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "API_KEY",
    "APIKEY",
    "CREDENTIAL",
    "PRIVATE_KEY",
    "SESSION",
];

fn is_secret_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    SECRET_ENV_MARKERS
        .iter()
        .any(|marker| upper.contains(marker))
}

fn sanitized_env(env: &BTreeMap<String, String>) -> (BTreeMap<String, String>, Vec<String>) {
    let mut retained = BTreeMap::new();
    let mut redacted = Vec::new();
    for (key, value) in env {
        if is_secret_env_key(key) {
            redacted.push(key.clone());
        } else {
            retained.insert(key.clone(), value.clone());
        }
    }
    (retained, redacted)
}

fn safe_stem(value: &str) -> String {
    let stem: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if stem.is_empty() {
        "proof".to_string()
    } else {
        stem
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

fn binary_hash(path: &str) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut hasher = blake3::Hasher::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Some(hasher.finalize().to_hex().to_string())
}

fn binary_version(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| *name == "cass" || name.starts_with("cass-"))
        .map(|_| env!("CARGO_PKG_VERSION").to_string())
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

fn classify_capture(
    raw: &RawRun,
    expect: &RunExpectation<'_>,
    accepted_exit_codes: &[i32],
) -> (RunOutcome, bool, Vec<AssertionResult>) {
    if raw.timed_out {
        return (RunOutcome::Timeout, false, Vec::new());
    }
    match raw.exit_code {
        Some(code) if accepted_exit_codes.contains(&code) => {}
        Some(code) => {
            return (
                RunOutcome::CommandFailure { exit_code: code },
                false,
                Vec::new(),
            );
        }
        None => {
            return (
                RunOutcome::CommandFailure {
                    exit_code: raw.signal.map(|signal| -signal).unwrap_or(-1),
                },
                false,
                Vec::new(),
            );
        }
    }

    let parsed_json_ok =
        !expect.expect_json || serde_json::from_str::<serde_json::Value>(raw.stdout.trim()).is_ok();
    if !parsed_json_ok {
        return (RunOutcome::InvalidJson, false, Vec::new());
    }

    let assertion_results: Vec<AssertionResult> = expect
        .assertions
        .iter()
        .map(|(name, check)| AssertionResult {
            name: name.clone(),
            passed: check(&raw.stdout, &raw.stderr),
        })
        .collect();
    let failed: Vec<String> = assertion_results
        .iter()
        .filter(|result| !result.passed)
        .map(|result| result.name.clone())
        .collect();
    let outcome = if failed.is_empty() {
        RunOutcome::Success
    } else {
        RunOutcome::AssertionFailure { failed }
    };
    (outcome, parsed_json_ok, assertion_results)
}

/// Run a command spec under the bounded runner and retain the captured streams.
/// `accepted_exit_codes` permits diagnostic commands whose contractually
/// expected result is non-zero to pass only after their JSON assertions run.
pub fn run_capture_with_exit_codes(
    spec: &RunSpec,
    expect: &RunExpectation<'_>,
    accepted_exit_codes: &[i32],
    now_ms: u64,
) -> RunCapture {
    let command_line = {
        let mut v = vec![spec.binary_path.clone()];
        v.extend(spec.args.iter().cloned());
        v
    };
    let (env_overrides, redacted_env_keys) = sanitized_env(&spec.env_overrides);
    let base = |raw: RawRun,
                outcome: RunOutcome,
                parsed_json_ok: bool,
                assertion_results: Vec<AssertionResult>| {
        let failures = assertion_results
            .iter()
            .filter(|result| !result.passed)
            .map(|result| result.name.clone())
            .collect();
        RunCapture {
            stdout: raw.stdout.clone(),
            stderr: raw.stderr.clone(),
            event: RunEvent {
                schema_version: E2E_RUNNER_SCHEMA_VERSION,
                mode: spec.mode,
                command_line: command_line.clone(),
                binary_path: spec.binary_path.clone(),
                binary_version: None,
                binary_hash: None,
                env_overrides: env_overrides.clone(),
                redacted_env_keys: redacted_env_keys.clone(),
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
                assertion_results,
                artifact_paths: Vec::new(),
                stdout_len: raw.stdout.len(),
                stderr_len: raw.stderr.len(),
            },
        }
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
            raw,
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
                raw,
                RunOutcome::CommandFailure { exit_code: -1 },
                true,
                Vec::new(),
            );
        }
    };

    let accepted = if accepted_exit_codes.is_empty() {
        &[0][..]
    } else {
        accepted_exit_codes
    };
    let (outcome, parsed_json_ok, assertion_results) = classify_capture(&raw, expect, accepted);
    base(raw, outcome, parsed_json_ok, assertion_results)
}

/// Run a command spec under the bounded runner and retain captured streams.
pub fn run_capture(spec: &RunSpec, expect: &RunExpectation<'_>, now_ms: u64) -> RunCapture {
    run_capture_with_exit_codes(spec, expect, &[0], now_ms)
}

/// Run a command spec under the bounded runner and produce a structured event.
/// A missing required fixture short-circuits to [`RunOutcome::MissingFixture`]
/// **without executing** the binary.
pub fn run(spec: &RunSpec, expect: &RunExpectation<'_>, now_ms: u64) -> RunEvent {
    run_capture(spec, expect, now_ms).event
}

/// Execute one scenario command and persist its stdout, stderr, structured
/// event, and classified proof artifact beneath a never-overwritten run tree.
pub fn run_with_artifacts(
    spec: &RunSpec,
    expect: &RunExpectation<'_>,
    artifact: &ArtifactRunSpec<'_>,
    now_ms: u64,
) -> io::Result<ScenarioArtifactEntry> {
    let mut capture =
        run_capture_with_exit_codes(spec, expect, artifact.accepted_exit_codes, now_ms);
    capture.event.binary_version = binary_version(&spec.binary_path);
    capture.event.binary_hash = binary_hash(&spec.binary_path);

    let command_dir = artifact
        .artifact_root
        .join(safe_stem(artifact.run_id))
        .join(safe_stem(artifact.scenario_id))
        .join(safe_stem(artifact.command_id));
    fs::create_dir_all(&command_dir)?;
    let stdout_path = command_dir.join("stdout.json");
    let stderr_path = command_dir.join("stderr.log");
    let event_path = command_dir.join("event.json");
    let proof_path = command_dir.join(format!("{}.proof.json", safe_stem(artifact.command_id)));
    for path in [&stdout_path, &stderr_path, &event_path, &proof_path] {
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "refusing to reuse existing proof artifact {}",
                    path.display()
                ),
            ));
        }
    }

    capture.event.artifact_paths = vec![
        stdout_path.display().to_string(),
        stderr_path.display().to_string(),
        event_path.display().to_string(),
        proof_path.display().to_string(),
    ];
    write_new(&stdout_path, capture.stdout.as_bytes())?;
    write_new(&stderr_path, capture.stderr.as_bytes())?;
    let event_bytes = serde_json::to_vec_pretty(&capture.event).map_err(io::Error::other)?;
    write_new(&event_path, &event_bytes)?;

    let assertions_ran = !capture.event.assertion_results.is_empty();
    let completed = capture.event.outcome.is_success();
    let proof_run = crate::proof_artifact::ProofRun {
        command: capture.event.command_line.join(" "),
        binary_path: Some(spec.binary_path.clone()),
        binary_version: capture.event.binary_version.clone(),
        data_dir_or_fixture: spec.fixture_id.clone(),
        // An expected non-zero diagnostic is a pass only because its assertions
        // ran. The actual code remains in RunEvent + ScenarioArtifactEntry.
        exit_code: if completed {
            Some(0)
        } else {
            capture.event.exit_code
        },
        elapsed_ms: capture.event.elapsed_ms,
        timeout_ms: spec.timeout.as_millis().try_into().unwrap_or(u64::MAX),
        timed_out: capture.event.timed_out,
        skipped: matches!(capture.event.outcome, RunOutcome::MissingFixture { .. }),
        assertions_ran,
        produced_artifact: true,
        completed,
        artifact_age_ms: Some(0),
        stdout_path: Some(stdout_path.display().to_string()),
        stderr_path: Some(stderr_path.display().to_string()),
    };
    let emitted =
        crate::proof_artifact::emit_proof_artifact(&command_dir, artifact.command_id, proof_run)?;
    if Path::new(&emitted.path) != proof_path {
        return Err(io::Error::other(format!(
            "proof path mismatch: expected {}, wrote {}",
            proof_path.display(),
            emitted.path
        )));
    }

    let stdout_lower = capture.stdout.to_ascii_lowercase();
    let stderr_lower = capture.stderr.to_ascii_lowercase();
    let forbidden_found = artifact.forbidden_stream_markers.iter().any(|marker| {
        let marker = marker.to_ascii_lowercase();
        !marker.is_empty() && (stdout_lower.contains(&marker) || stderr_lower.contains(&marker))
    });
    let redaction_safe = !forbidden_found
        && capture
            .event
            .env_overrides
            .keys()
            .all(|key| !is_secret_env_key(key));
    let fresh_for_suite = capture.event.start_ms >= artifact.suite_started_ms
        && capture.event.end_ms >= capture.event.start_ms;

    Ok(ScenarioArtifactEntry {
        schema_version: E2E_SCENARIO_MANIFEST_SCHEMA_VERSION,
        run_id: artifact.run_id.to_string(),
        scenario_id: artifact.scenario_id.to_string(),
        command_id: artifact.command_id.to_string(),
        issue_ids: artifact
            .issue_ids
            .iter()
            .map(|id| (*id).to_string())
            .collect(),
        fixture_id: spec.fixture_id.clone(),
        privacy_note: artifact.privacy_note.to_string(),
        started_at_ms: capture.event.start_ms,
        finished_at_ms: capture.event.end_ms,
        actual_exit_code: capture.event.exit_code,
        outcome: capture.event.outcome,
        proof_status: emitted.status,
        assertions_ran,
        generated_only: !assertions_ran,
        redaction_safe,
        fresh_for_suite,
        stdout_path: stdout_path.display().to_string(),
        stderr_path: stderr_path.display().to_string(),
        event_path: event_path.display().to_string(),
        proof_path: proof_path.display().to_string(),
        extra_artifact_paths: artifact
            .extra_artifact_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
    })
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

    fn json_expectation() -> RunExpectation<'static> {
        RunExpectation {
            expect_json: true,
            assertions: vec![(
                "ok-is-true".to_string(),
                Box::new(|stdout: &str, _stderr: &str| {
                    serde_json::from_str::<serde_json::Value>(stdout)
                        .ok()
                        .and_then(|value| value.get("ok").cloned())
                        == Some(serde_json::Value::Bool(true))
                }),
            )],
        }
    }

    #[test]
    fn expected_nonzero_diagnostic_can_pass_only_after_assertions() {
        let spec = sh_spec("printf '{\"ok\":true}'; exit 7", 5_000);
        let capture = run_capture_with_exit_codes(&spec, &json_expectation(), &[7], 10);
        assert_eq!(capture.event.exit_code, Some(7));
        assert_eq!(capture.event.outcome, RunOutcome::Success);
        assert_eq!(
            capture.event.assertion_results,
            vec![AssertionResult {
                name: "ok-is-true".to_string(),
                passed: true,
            }]
        );
    }

    #[test]
    fn artifact_run_writes_streams_event_proof_and_complete_manifest() {
        let temp = tempfile::tempdir().expect("tempdir").keep();
        let mut spec = sh_spec("printf '{\"ok\":true}'; printf 'diag' 1>&2", 5_000);
        spec.fixture_id = Some("synthetic-fixture".to_string());
        spec.env_overrides.insert(
            "CASS_API_TOKEN".to_string(),
            "fixture-sensitive-value".to_string(),
        );
        let request = ArtifactRunSpec {
            artifact_root: &temp,
            run_id: "run-1",
            scenario_id: "scenario-1",
            command_id: "command-1",
            issue_ids: &["bead-1"],
            privacy_note: "synthetic data only",
            suite_started_ms: 1_000,
            accepted_exit_codes: &[0],
            forbidden_stream_markers: &["fixture-sensitive-value"],
            extra_artifact_paths: &[],
        };
        let entry = run_with_artifacts(&spec, &json_expectation(), &request, 1_000).expect("run");
        assert_eq!(entry.proof_status, crate::proof_artifact::ProofStatus::Pass);
        assert!(entry.redaction_safe);
        let event: RunEvent =
            serde_json::from_slice(&fs::read(&entry.event_path).expect("read event"))
                .expect("parse event");
        assert!(!event.env_overrides.contains_key("CASS_API_TOKEN"));
        assert_eq!(event.redacted_env_keys, vec!["CASS_API_TOKEN"]);
        assert!(
            !fs::read_to_string(&entry.event_path)
                .expect("event text")
                .contains("fixture-sensitive-value")
        );

        let mut manifest = ScenarioArtifactManifest::new(
            "run-1",
            RunMode::Quick,
            1_000,
            vec!["scenario-1".to_string()],
            vec!["scenario-1/command-1".to_string()],
            1,
        );
        manifest.record(entry);
        assert!(
            manifest.is_clean_pass(),
            "{:?}",
            manifest.validation_errors()
        );
        let manifest_path = temp.join("run-1.manifest.json");
        let jsonl_path = temp.join("run-1.manifest.jsonl");
        manifest.write_json(&manifest_path).expect("manifest");
        manifest.write_jsonl(&jsonl_path).expect("manifest jsonl");
        assert!(manifest_path.is_file());
        assert_eq!(
            fs::read_to_string(jsonl_path)
                .expect("jsonl")
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn completeness_gate_rejects_empty_stale_generated_and_redaction_failures() {
        let empty = ScenarioArtifactManifest::new(
            "run",
            RunMode::Quick,
            10,
            vec!["s".to_string()],
            vec!["s/c".to_string()],
            1,
        );
        assert!(!empty.is_clean_pass());

        let temp = tempfile::tempdir().expect("tempdir").keep();
        let request = ArtifactRunSpec {
            artifact_root: &temp,
            run_id: "run",
            scenario_id: "s",
            command_id: "c",
            issue_ids: &["bead"],
            privacy_note: "synthetic",
            suite_started_ms: 10,
            accepted_exit_codes: &[0],
            forbidden_stream_markers: &[],
            extra_artifact_paths: &[],
        };
        let entry = run_with_artifacts(
            &sh_spec("printf '{\"ok\":true}'", 5_000),
            &json_expectation(),
            &request,
            10,
        )
        .expect("run");

        let mutations: [fn(&mut ScenarioArtifactEntry); 3] = [
            |entry: &mut ScenarioArtifactEntry| entry.fresh_for_suite = false,
            |entry: &mut ScenarioArtifactEntry| entry.generated_only = true,
            |entry: &mut ScenarioArtifactEntry| entry.redaction_safe = false,
        ];
        for mutate in mutations {
            let mut rejected = ScenarioArtifactManifest::new(
                "run",
                RunMode::Quick,
                10,
                vec!["s".to_string()],
                vec!["s/c".to_string()],
                1,
            );
            let mut bad = entry.clone();
            mutate(&mut bad);
            rejected.record(bad);
            assert!(!rejected.is_clean_pass());
        }

        let mut ambiguous = ScenarioArtifactManifest::new(
            "run",
            RunMode::Quick,
            10,
            vec!["s".to_string()],
            vec!["s/c".to_string(), "s/other".to_string()],
            2,
        );
        ambiguous.record(entry.clone());
        ambiguous.record(entry.clone());
        assert!(!ambiguous.is_clean_pass());

        let mut wrong_command = ScenarioArtifactManifest::new(
            "run",
            RunMode::Quick,
            10,
            vec!["s".to_string()],
            vec!["s/c".to_string()],
            1,
        );
        let mut unexpected = entry.clone();
        unexpected.command_id = "unexpected".to_string();
        wrong_command.record(unexpected);
        assert!(!wrong_command.is_clean_pass());

        let invalid_event_path = temp.join("invalid-event.json");
        fs::write(&invalid_event_path, b"{not-json").expect("write invalid event fixture");
        let mut invalid_event = ScenarioArtifactManifest::new(
            "run",
            RunMode::Quick,
            10,
            vec!["s".to_string()],
            vec!["s/c".to_string()],
            1,
        );
        let mut invalid_event_entry = entry.clone();
        invalid_event_entry.event_path = invalid_event_path.display().to_string();
        invalid_event.record(invalid_event_entry);
        assert!(!invalid_event.is_clean_pass());

        let invalid_proof_path = temp.join("invalid-proof.json");
        fs::write(&invalid_proof_path, b"{not-json").expect("write invalid proof fixture");
        let mut invalid_proof = ScenarioArtifactManifest::new(
            "run",
            RunMode::Quick,
            10,
            vec!["s".to_string()],
            vec!["s/c".to_string()],
            1,
        );
        let mut invalid_proof_entry = entry;
        invalid_proof_entry.proof_path = invalid_proof_path.display().to_string();
        invalid_proof.record(invalid_proof_entry);
        assert!(!invalid_proof.is_clean_pass());
    }
}
