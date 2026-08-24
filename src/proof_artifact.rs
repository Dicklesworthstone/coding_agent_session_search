//! Proof artifacts that distinguish real passes from timeouts and stale evidence.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.11.4
//! ("Record proof artifacts so pass timeout and stale evidence are
//! distinguishable").
//!
//! The motivating failure: a `cargo test --lib` run once *appeared* successful
//! but had actually timed out at 7200s **before any test ran** — a warm-cache run
//! later passed. A proof artifact must therefore never let "exited 0" or "no
//! failures" masquerade as a real pass: it records the command, binary
//! path/version, data dir/fixture, exit code, elapsed time, timeout status,
//! stdout/stderr artifact paths, and crucially **whether assertions actually
//! ran**, then classifies the run into one explicit [`ProofStatus`].
//!
//! This is pure, deterministic logic over a recorded [`ProofRun`] — unit-testable
//! without invoking anything — so the classification can be trusted as the single
//! source of truth for closeout docs and quality gates.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use walkdir::WalkDir;

/// Stable schema version for the proof-artifact wire format.
pub const PROOF_ARTIFACT_SCHEMA_VERSION: u32 = 1;

/// The classified outcome of a proof run. Ordered so a fleet/gate rollup can take
/// the "worst" status with `max` (Pass is the floor of concern).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProofStatus {
    /// The command ran, assertions executed, and it succeeded.
    Pass,
    /// Assertions ran and at least one failed (a genuine, attributable failure).
    Fail,
    /// A partial proof: the run started and produced *some* evidence but did not
    /// complete (e.g. a bounded surface returned partial results).
    PartialProof,
    /// The run produced/refreshed artifacts but executed NO assertions — evidence
    /// exists but proves nothing about behavior (the "generated-only" trap).
    GeneratedOnly,
    /// The run was skipped (e.g. filtered out, precondition unmet).
    Skipped,
    /// The cited artifact is stale — older than the inputs/binary it claims to
    /// prove, so it must not be trusted as current evidence.
    StaleArtifact,
    /// The run hit its timeout. Critically this OUTRANKS a zero exit code: a
    /// timeout-before-tests-ran is a timeout, never a pass.
    Timeout,
}

impl ProofStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            ProofStatus::Pass => "pass",
            ProofStatus::Fail => "fail",
            ProofStatus::PartialProof => "partial-proof",
            ProofStatus::GeneratedOnly => "generated-only",
            ProofStatus::Skipped => "skipped",
            ProofStatus::StaleArtifact => "stale-artifact",
            ProofStatus::Timeout => "timeout",
        }
    }

    /// `true` only for [`ProofStatus::Pass`] — the single status a quality gate
    /// may treat as proven-good.
    pub const fn is_trustworthy_pass(self) -> bool {
        matches!(self, ProofStatus::Pass)
    }
}

/// The recorded facts of a single proof run, before classification. Timestamps
/// are epoch-millis; pass these in (don't read the clock here) so classification
/// stays pure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofRun {
    /// The exact command line (for reproduction).
    pub command: String,
    /// Path to the binary under test.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    /// Binary version / contract version, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_version: Option<String>,
    /// Data dir or fixture id the run used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir_or_fixture: Option<String>,
    /// Process exit code, if the process completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Wall-clock the run took.
    pub elapsed_ms: u64,
    /// The timeout the run was given (0 = none).
    pub timeout_ms: u64,
    /// Whether the run hit its timeout.
    pub timed_out: bool,
    /// Whether the run was explicitly skipped.
    #[serde(default)]
    pub skipped: bool,
    /// Whether any assertions actually executed. This is the linchpin: exit 0 with
    /// `assertions_ran = false` is NOT a pass.
    pub assertions_ran: bool,
    /// Whether the run produced/refreshed an artifact (logs, golden, manifest).
    #[serde(default)]
    pub produced_artifact: bool,
    /// Whether the run completed all intended work (false => partial).
    #[serde(default = "default_true")]
    pub completed: bool,
    /// Artifact age relative to the newest input/binary it claims to prove, in ms;
    /// `Some(age)` with `age` exceeding the freshness window marks the artifact
    /// stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_age_ms: Option<u64>,
    /// stdout artifact path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_path: Option<String>,
    /// stderr artifact path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_path: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Freshness window: an artifact older than this vs. its inputs is stale.
pub const DEFAULT_STALE_AFTER_MS: u64 = 24 * 60 * 60 * 1000;

/// Classify a [`ProofRun`] into a [`ProofStatus`] using `stale_after_ms` as the
/// freshness window. Precedence is deliberate and safety-first:
/// timeout > skipped > stale > assertions-didn't-run (generated-only) >
/// failed-or-missing-exit > incomplete (partial) > pass.
pub fn classify(run: &ProofRun, stale_after_ms: u64) -> ProofStatus {
    // A timeout outranks everything — even a zero exit code — so a run that timed
    // out before tests ran can never read as a pass.
    if run.timed_out || (run.timeout_ms > 0 && run.elapsed_ms >= run.timeout_ms) {
        return ProofStatus::Timeout;
    }
    if run.skipped {
        return ProofStatus::Skipped;
    }
    if let Some(age) = run.artifact_age_ms
        && age > stale_after_ms
    {
        return ProofStatus::StaleArtifact;
    }
    // Evidence exists but nothing was actually asserted -> proves nothing.
    if !run.assertions_ran {
        return ProofStatus::GeneratedOnly;
    }
    // Assertions ran; a non-zero exit is a genuine failure.
    if matches!(run.exit_code, Some(code) if code != 0) {
        return ProofStatus::Fail;
    }
    // No observed exit status means the harness never proved that the command
    // completed successfully. Even if it recorded assertions and marked its
    // own work complete, that evidence is partial rather than a pass.
    if run.exit_code.is_none() {
        return ProofStatus::PartialProof;
    }
    if !run.completed {
        return ProofStatus::PartialProof;
    }
    ProofStatus::Pass
}

/// A fully classified proof artifact: the recorded run plus its trustworthy
/// [`ProofStatus`] and a human summary. Stable snake_case JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofArtifact {
    pub schema_version: u32,
    pub status: ProofStatus,
    pub run: ProofRun,
    /// One-line, action-oriented summary (human convenience; facts live in `run`).
    pub summary: String,
}

impl ProofArtifact {
    /// Build a classified artifact from a run, using the default freshness window.
    pub fn from_run(run: ProofRun) -> Self {
        Self::from_run_with_window(run, DEFAULT_STALE_AFTER_MS)
    }

    /// Build a classified artifact with an explicit freshness window.
    pub fn from_run_with_window(run: ProofRun, stale_after_ms: u64) -> Self {
        let status = classify(&run, stale_after_ms);
        let summary = match status {
            ProofStatus::Pass => format!("pass in {}ms: {}", run.elapsed_ms, run.command),
            ProofStatus::Fail => format!(
                "FAIL (exit {}): {}",
                run.exit_code.unwrap_or(-1),
                run.command
            ),
            ProofStatus::Timeout => format!(
                "TIMEOUT after {}ms (cap {}ms) — assertions_ran={}: {}",
                run.elapsed_ms, run.timeout_ms, run.assertions_ran, run.command
            ),
            ProofStatus::GeneratedOnly => format!(
                "generated-only (no assertions ran) — not a pass: {}",
                run.command
            ),
            ProofStatus::Skipped => format!("skipped: {}", run.command),
            ProofStatus::StaleArtifact => format!(
                "stale artifact (age {}ms): {}",
                run.artifact_age_ms.unwrap_or(0),
                run.command
            ),
            ProofStatus::PartialProof => {
                format!("partial proof (incomplete run): {}", run.command)
            }
        };
        Self {
            schema_version: PROOF_ARTIFACT_SCHEMA_VERSION,
            status,
            run,
            summary,
        }
    }

    /// Whether this artifact may be cited as a current, trustworthy pass.
    pub fn is_trustworthy_pass(&self) -> bool {
        self.status.is_trustworthy_pass()
    }

    /// Serialize this classified artifact to `path` as pretty JSON, creating
    /// parent directories as needed. Emission is the step that turns an
    /// in-memory verdict into a citable artifact on disk (see [`emit_proof_artifact`]).
    pub fn write_json(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        std::fs::write(path, bytes)
    }
}

/// One emitted proof-artifact record, as referenced from a [`ProofManifest`]:
/// the caller's `label`, the classified [`ProofStatus`], the artifact file path,
/// and the reproduced command. This is the index entry agents cite — it points
/// at the full [`ProofArtifact`] JSON on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmittedProof {
    pub label: String,
    pub status: ProofStatus,
    pub path: String,
    pub command: String,
}

/// Sanitize an arbitrary label into a filesystem-safe artifact stem.
fn safe_stem(label: &str) -> String {
    let stem: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
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

/// Classify `run`, write `<dir>/<label>.proof.json`, and return its index entry.
/// Uses the default freshness window ([`DEFAULT_STALE_AFTER_MS`]).
pub fn emit_proof_artifact(dir: &Path, label: &str, run: ProofRun) -> io::Result<EmittedProof> {
    emit_proof_artifact_with_window(dir, label, run, DEFAULT_STALE_AFTER_MS)
}

/// [`emit_proof_artifact`] with an explicit freshness window.
pub fn emit_proof_artifact_with_window(
    dir: &Path,
    label: &str,
    run: ProofRun,
    stale_after_ms: u64,
) -> io::Result<EmittedProof> {
    let artifact = ProofArtifact::from_run_with_window(run, stale_after_ms);
    let path = dir.join(format!("{}.proof.json", safe_stem(label)));
    artifact.write_json(&path)?;
    Ok(EmittedProof {
        label: label.to_string(),
        status: artifact.status,
        path: path.to_string_lossy().into_owned(),
        command: artifact.run.command,
    })
}

/// A manifest of the proofs one suite/run emitted. Its log-completeness verdict
/// is what makes a gate "unable to pass by doing nothing": an empty manifest is
/// never a clean pass, and a single timeout / stale / generated-only / fail /
/// partial / skipped entry sinks the verdict — exactly the confusable outcomes
/// the proof taxonomy exists to separate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofManifest {
    pub entries: Vec<EmittedProof>,
}

impl ProofManifest {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one emitted proof to the manifest.
    pub fn record(&mut self, emitted: EmittedProof) {
        self.entries.push(emitted);
    }

    /// The worst status across all entries (`Pass` is the floor of concern);
    /// `None` when the manifest is empty.
    #[must_use]
    pub fn worst_status(&self) -> Option<ProofStatus> {
        self.entries.iter().map(|entry| entry.status).max()
    }

    /// `true` only when there is at least one entry and EVERY entry is a
    /// trustworthy pass. Empty manifests return `false` ("cannot pass by doing
    /// nothing"); any non-pass entry returns `false`.
    #[must_use]
    pub fn is_clean_pass(&self) -> bool {
        !self.entries.is_empty()
            && self
                .entries
                .iter()
                .all(|entry| entry.status.is_trustworthy_pass())
    }

    /// Count of entries carrying `status`.
    #[must_use]
    pub fn count_with(&self, status: ProofStatus) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.status == status)
            .count()
    }

    /// Write the manifest as JSONL (one [`EmittedProof`] per line) to `path`,
    /// creating parent directories as needed.
    pub fn write_jsonl(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut buf = String::new();
        for entry in &self.entries {
            let line = serde_json::to_string(entry).map_err(io::Error::other)?;
            buf.push_str(&line);
            buf.push('\n');
        }
        std::fs::write(path, buf)
    }
}

/// Stable schema for one immutable, run-scoped E2E evidence bundle.
pub const E2E_RUN_BUNDLE_SCHEMA_VERSION: u32 = 2;
/// Canonical manifest filename inside a run directory.
pub const E2E_RUN_MANIFEST_FILENAME: &str = "manifest.json";
/// Completion receipt written only after the manifest is durable.
pub const E2E_RUN_COMPLETION_FILENAME: &str = "complete.json";
const REQUIRED_E2E_ACCEPTANCE_GATES: [&str; 6] = [
    "cargo-tests",
    "jsonl-schema",
    "bounded-capture-complete",
    "artifact-set-nonempty",
    "trace-artifacts-nonempty",
    "required-event-coverage",
];

fn e2e_manifest_path(root: &Path) -> PathBuf {
    root.join(E2E_RUN_MANIFEST_FILENAME) // ubs:ignore — fixed control filename, never caller input.
}

fn e2e_completion_path(root: &Path) -> PathBuf {
    root.join(E2E_RUN_COMPLETION_FILENAME) // ubs:ignore — fixed control filename, never caller input.
}

/// Fail-closed errors for E2E run creation and verification.
#[derive(Debug, Error)]
pub enum E2eRunBundleError {
    #[error("invalid E2E run id `{0}`; expected 8-128 ASCII alphanumeric, `_`, or `-` bytes")]
    InvalidRunId(String),
    #[error("invalid RCH worker id `{0}`; expected 1-64 ASCII alphanumeric, `_`, or `-` bytes")]
    InvalidWorkerId(String),
    #[error("invalid {field} digest `{value}`; expected {expected} lowercase hexadecimal bytes")]
    InvalidDigest {
        field: &'static str,
        value: String,
        expected: usize,
    },
    #[error("unsafe E2E bundle entry `{path}`: {reason}")]
    UnsafeEntry { path: String, reason: String },
    #[error("invalid E2E bundle manifest: {0}")]
    InvalidManifest(String),
    #[error("I/O error at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("invalid JSON in `{path}`: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

fn bundle_io(path: &Path, source: io::Error) -> E2eRunBundleError {
    E2eRunBundleError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn bundle_json(path: &Path, source: serde_json::Error) -> E2eRunBundleError {
    E2eRunBundleError::Json {
        path: path.display().to_string(),
        source,
    }
}

fn invalid_manifest(message: impl fmt::Display) -> E2eRunBundleError {
    E2eRunBundleError::InvalidManifest(message.to_string())
}

fn unsafe_bundle_entry(path: &Path, reason: impl fmt::Display) -> E2eRunBundleError {
    E2eRunBundleError::UnsafeEntry {
        path: path.display().to_string(),
        reason: reason.to_string(),
    }
}

/// Validate a collision-resistant run id before using it in any path or shell handoff.
pub fn validate_e2e_run_id(run_id: &str) -> Result<(), E2eRunBundleError> {
    if !(8..=128).contains(&run_id.len())
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        || !run_id
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !run_id
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(E2eRunBundleError::InvalidRunId(run_id.to_string()));
    }
    Ok(())
}

/// Validate the explicit RCH worker id used by both compilation and SSH handoff.
pub fn validate_rch_worker_id(worker_id: &str) -> Result<(), E2eRunBundleError> {
    if worker_id.is_empty()
        || worker_id.len() > 64
        || !worker_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        || !worker_id
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !worker_id
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(E2eRunBundleError::InvalidWorkerId(worker_id.to_string()));
    }
    Ok(())
}

fn validate_hex_digest(
    field: &'static str,
    value: &str,
    expected: usize,
) -> Result<(), E2eRunBundleError> {
    if value.len() != expected
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(E2eRunBundleError::InvalidDigest {
            field,
            value: value.to_string(),
            expected,
        });
    }
    Ok(())
}

/// Validate the exact committed source and working-tree digest bound into a run.
pub fn validate_e2e_source_identity(
    source_sha: &str,
    source_diff_sha256: &str,
    source_tree_sha256: &str,
) -> Result<(), E2eRunBundleError> {
    validate_hex_digest("source_sha", source_sha, 40)?;
    validate_hex_digest("source_diff_sha256", source_diff_sha256, 64)?;
    validate_hex_digest("source_tree_sha256", source_tree_sha256, 64)
}

fn validate_worker_hostname(hostname: &str) -> Result<(), E2eRunBundleError> {
    if hostname.is_empty()
        || hostname.len() > 255
        || !hostname.is_ascii()
        || !hostname
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(E2eRunBundleError::InvalidManifest(
            "worker_hostname must be 1-255 ASCII hostname bytes".to_string(),
        ));
    }
    Ok(())
}

fn validate_relative_artifact_path(path: &Path) -> Result<String, E2eRunBundleError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(E2eRunBundleError::UnsafeEntry {
            path: path.display().to_string(),
            reason: "path must be a non-empty normalized relative path".to_string(),
        });
    }
    path.to_str()
        .map(ToString::to_string)
        .ok_or_else(|| E2eRunBundleError::UnsafeEntry {
            path: path.display().to_string(),
            reason: "path is not valid UTF-8".to_string(),
        })
}

fn reject_aliasing_file(path: &Path, metadata: &fs::Metadata) -> Result<(), E2eRunBundleError> {
    if !metadata.is_file() {
        return Err(E2eRunBundleError::UnsafeEntry {
            path: path.display().to_string(),
            reason: "entry is not a regular file".to_string(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(E2eRunBundleError::UnsafeEntry {
                path: path.display().to_string(),
                reason: format!(
                    "regular file has {} hard links; immutable bundles require one owner",
                    metadata.nlink()
                ),
            });
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, E2eRunBundleError> {
    let mut file = File::open(path).map_err(|error| bundle_io(path, error))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| bundle_io(path, error))?;
        if read == 0 {
            break;
        }
        let chunk = buffer
            .get(..read)
            .ok_or_else(|| invalid_manifest("read returned more bytes than the hash buffer"))?;
        hasher.update(chunk);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn is_transient_e2e_source_entry(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let Some(Component::Normal(first)) = relative.components().next() else {
        return false;
    };
    let Some(name) = first.to_str() else {
        return false;
    };
    matches!(
        name,
        ".git" | ".beads" | ".rch-tmp" | "target" | "test-results"
    ) || name.starts_with(".rch-target-")
}

/// Hash every non-transient source file using GNU `sha256sum --zero` framing.
///
/// RCH clean overlays do not contain `.git`, and Cargo/RCH create transient
/// output directories while the runner is starting. Those top-level transport
/// and output directories are excluded; every other entry is part of the
/// source identity. Symbolic links, hard links, non-files, and non-UTF-8 paths
/// fail closed so an overlay cannot alias bytes outside the attested tree.
pub fn e2e_source_tree_sha256(root: &Path) -> Result<String, E2eRunBundleError> {
    let root_metadata = fs::symlink_metadata(root).map_err(|error| bundle_io(root, error))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(unsafe_bundle_entry(
            root,
            "source root must be a real directory",
        ));
    }

    let mut files = Vec::new();
    let entries = WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_transient_e2e_source_entry(root, entry.path()));
    for entry in entries {
        let entry = entry.map_err(invalid_manifest)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(path).map_err(|error| bundle_io(path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(unsafe_bundle_entry(
                path,
                "symbolic source entries are forbidden",
            ));
        }
        if metadata.is_dir() {
            continue;
        }
        reject_aliasing_file(path, &metadata)?;
        let relative = path
            .strip_prefix(root)
            .map_err(|_| unsafe_bundle_entry(path, "source entry is outside its root"))?;
        let relative = validate_relative_artifact_path(relative)?;
        if relative.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(unsafe_bundle_entry(
                path,
                "source path contains an ASCII control byte",
            ));
        }
        files.push((relative, path.to_path_buf()));
    }
    files.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));

    let mut tree_hasher = Sha256::new();
    for (relative, path) in files {
        let file_sha256 = sha256_file(&path)?;
        tree_hasher.update(file_sha256.as_bytes());
        tree_hasher.update(b"  ");
        tree_hasher.update(relative.as_bytes());
        tree_hasher.update([0]);
    }
    Ok(hex::encode(tree_hasher.finalize()))
}

/// The schema recognized for one captured artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum E2eRunArtifactSchema {
    Raw,
    Jsonl,
    E2eLogV1,
    CassTraceV1,
}

/// One immutable file declared by an E2E run manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct E2eRunArtifact {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub events: u64,
    pub schema: E2eRunArtifactSchema,
}

/// One gate evaluated before a bundle may be cited as a pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct E2eRunGateResult {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

/// Aggregated, deterministic statistics over the declared file set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct E2eRunAggregates {
    pub files: u64,
    pub bytes: u64,
    pub events: u64,
    pub trace_files: u64,
    pub trace_bytes: u64,
    pub trace_events: u64,
    pub event_histogram: BTreeMap<String, u64>,
}

/// Exact source, worker, command, outcome, and artifact identity for one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct E2eRunBundleManifest {
    pub schema_version: u32,
    pub run_id: String,
    pub source_sha: String,
    pub source_diff_sha256: String,
    pub source_tree_sha256: String,
    pub worker_id: String,
    pub worker_hostname: String,
    pub command: Vec<String>,
    pub command_sha256: String,
    pub cargo_exit_code: i32,
    pub started_at_unix_ms: u64,
    pub ended_at_unix_ms: u64,
    pub files: Vec<E2eRunArtifact>,
    pub aggregates: E2eRunAggregates,
    pub gates: Vec<E2eRunGateResult>,
    pub complete: bool,
}

impl E2eRunBundleManifest {
    /// A citable pass requires a completed manifest, a successful test command,
    /// a non-empty artifact set, and every named gate to pass.
    #[must_use]
    pub fn is_acceptance_pass(&self) -> bool {
        self.complete
            && self.cargo_exit_code == 0
            && !self.files.is_empty()
            && self.gates.iter().all(|gate| gate.passed)
            && REQUIRED_E2E_ACCEPTANCE_GATES.iter().all(|required| {
                self.gates
                    .iter()
                    .any(|gate| gate.name == *required && gate.passed)
            })
    }
}

/// Durable receipt binding the completion marker to the exact manifest bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct E2eRunCompletionReceipt {
    pub schema_version: u32,
    pub run_id: String,
    pub manifest_sha256: String,
    pub finalized_at_unix_ms: u64,
}

/// Caller-supplied facts used to create one run manifest.
#[derive(Debug, Clone)]
pub struct E2eRunBundleMetadata {
    pub run_id: String,
    pub source_sha: String,
    pub source_diff_sha256: String,
    pub source_tree_sha256: String,
    pub worker_id: String,
    pub worker_hostname: String,
    pub command: Vec<String>,
    pub cargo_exit_code: i32,
    pub started_at_unix_ms: u64,
    pub ended_at_unix_ms: u64,
    pub gates: Vec<E2eRunGateResult>,
}

/// Optional exact expectations checked again after worker-to-local transfer.
#[derive(Debug, Clone, Default)]
pub struct E2eRunBundleExpectation {
    pub run_id: Option<String>,
    pub source_sha: Option<String>,
    pub source_diff_sha256: Option<String>,
    pub source_tree_sha256: Option<String>,
    pub worker_id: Option<String>,
    pub worker_hostname: Option<String>,
}

fn increment_event_histogram(histogram: &mut BTreeMap<String, u64>, event: &str) {
    *histogram.entry(event.to_string()).or_insert(0) += 1;
}

fn inspect_jsonl_artifact(
    path: &Path,
) -> Result<(u64, E2eRunArtifactSchema, BTreeMap<String, u64>), E2eRunBundleError> {
    let file = File::open(path).map_err(|error| bundle_io(path, error))?;
    let mut events = 0_u64;
    let mut schema = None;
    let mut histogram = BTreeMap::new();
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|error| bundle_io(path, error))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(&line).map_err(|source| bundle_json(path, source))?;
        let line_schema = if value.get("schema_version").and_then(|value| value.as_str())
            == Some("cass-trace-v1")
        {
            E2eRunArtifactSchema::CassTraceV1
        } else if value
            .get("event")
            .and_then(|value| value.as_str())
            .is_some()
        {
            E2eRunArtifactSchema::E2eLogV1
        } else {
            E2eRunArtifactSchema::Jsonl
        };
        if let Some(expected) = schema
            && expected != line_schema
        {
            return Err(invalid_manifest(format_args!(
                "{} mixes {:?} and {:?} records",
                path.display(),
                expected,
                line_schema
            )));
        }
        schema = Some(line_schema);
        events = events.saturating_add(1);
        let event = match line_schema {
            E2eRunArtifactSchema::CassTraceV1 => value
                .pointer("/fields/event")
                .or_else(|| value.pointer("/fields/message"))
                .and_then(serde_json::Value::as_str),
            E2eRunArtifactSchema::E2eLogV1 => {
                value.get("event").and_then(serde_json::Value::as_str)
            }
            E2eRunArtifactSchema::Raw | E2eRunArtifactSchema::Jsonl => None,
        };
        if let Some(event) = event {
            increment_event_histogram(&mut histogram, event);
        }
    }
    let schema = schema.unwrap_or_else(|| {
        if path.file_name().and_then(|name| name.to_str()) == Some("trace.jsonl") {
            E2eRunArtifactSchema::CassTraceV1
        } else {
            E2eRunArtifactSchema::Jsonl
        }
    });
    Ok((events, schema, histogram))
}

fn inspect_artifact(
    root: &Path,
    path: &Path,
) -> Result<(E2eRunArtifact, BTreeMap<String, u64>), E2eRunBundleError> {
    let symlink_metadata = fs::symlink_metadata(path).map_err(|error| bundle_io(path, error))?;
    if symlink_metadata.file_type().is_symlink() {
        return Err(E2eRunBundleError::UnsafeEntry {
            path: path.display().to_string(),
            reason: "symbolic links are forbidden".to_string(),
        });
    }
    reject_aliasing_file(path, &symlink_metadata)?;
    let relative = path
        .strip_prefix(root)
        .map_err(|_| E2eRunBundleError::UnsafeEntry {
            path: path.display().to_string(),
            reason: format!("entry is outside bundle root {}", root.display()),
        })?;
    let relative = validate_relative_artifact_path(relative)?;
    let bytes = symlink_metadata.len();
    let is_jsonl = path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
        || path.file_name().and_then(|name| name.to_str()) == Some("cass.log");
    let (events, schema, histogram) = if is_jsonl {
        inspect_jsonl_artifact(path)?
    } else {
        (0, E2eRunArtifactSchema::Raw, BTreeMap::new())
    };
    Ok((
        E2eRunArtifact {
            path: relative,
            sha256: sha256_file(path)?,
            bytes,
            events,
            schema,
        },
        histogram,
    ))
}

fn collect_run_artifacts(
    root: &Path,
) -> Result<(Vec<E2eRunArtifact>, E2eRunAggregates), E2eRunBundleError> {
    let root_metadata = fs::symlink_metadata(root).map_err(|error| bundle_io(root, error))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(E2eRunBundleError::UnsafeEntry {
            path: root.display().to_string(),
            reason: "bundle root must be a real directory".to_string(),
        });
    }

    let mut files = Vec::new();
    let mut aggregates = E2eRunAggregates::default();
    for entry in WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry.map_err(invalid_manifest)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(path).map_err(|error| bundle_io(path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(unsafe_bundle_entry(path, "symbolic links are forbidden"));
        }
        if metadata.is_dir() {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| invalid_manifest(path.display()))?;
        if relative == Path::new(E2E_RUN_MANIFEST_FILENAME)
            || relative == Path::new(E2E_RUN_COMPLETION_FILENAME)
        {
            continue;
        }
        let (artifact, histogram) = inspect_artifact(root, path)?;
        aggregates.files = aggregates.files.saturating_add(1);
        aggregates.bytes = aggregates.bytes.saturating_add(artifact.bytes);
        aggregates.events = aggregates.events.saturating_add(artifact.events);
        if artifact.schema == E2eRunArtifactSchema::CassTraceV1 {
            aggregates.trace_files = aggregates.trace_files.saturating_add(1);
            aggregates.trace_bytes = aggregates.trace_bytes.saturating_add(artifact.bytes);
            aggregates.trace_events = aggregates.trace_events.saturating_add(artifact.events);
        }
        for (event, count) in histogram {
            *aggregates.event_histogram.entry(event).or_insert(0) += count;
        }
        files.push(artifact);
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((files, aggregates))
}

fn write_durable_json<T: Serialize>(path: &Path, value: &T) -> Result<Vec<u8>, E2eRunBundleError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| E2eRunBundleError::Json {
        path: path.display().to_string(),
        source,
    })?;
    let parent = path
        .parent()
        .ok_or_else(|| E2eRunBundleError::InvalidManifest(path.display().to_string()))?;
    let temp = parent.join(format!(
        ".{}.partial-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("bundle"),
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .map_err(|error| bundle_io(&temp, error))?;
    file.write_all(&bytes)
        .map_err(|error| bundle_io(&temp, error))?;
    file.sync_all().map_err(|error| bundle_io(&temp, error))?;
    fs::rename(&temp, path).map_err(|error| bundle_io(path, error))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| bundle_io(parent, error))?;
    Ok(bytes)
}

fn hash_command_argument(hasher: &mut Sha256, argument: &str) {
    hasher.update(argument.len().to_string().as_bytes());
    hasher.update(b":");
    hasher.update(argument.as_bytes());
    hasher.update(b"\n");
}

fn command_sha256(command: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"cass-e2e-command-v1\n");
    for argument in command {
        hash_command_argument(&mut hasher, argument);
    }
    hex::encode(hasher.finalize())
}

fn validate_e2e_command(command: &[String]) -> Result<(), E2eRunBundleError> {
    let mut prefix = command.iter();
    if prefix.next().map(String::as_str) != Some("cargo")
        || prefix.next().map(String::as_str) != Some("test")
    {
        return Err(invalid_manifest(
            "command must begin with the exact `cargo test` argv",
        ));
    }
    if command.iter().any(|argument| {
        argument.is_empty()
            || !argument.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(
                        byte,
                        b'_' | b'.' | b'/' | b':' | b'=' | b'+' | b',' | b'@' | b'-'
                    )
            })
    }) {
        return Err(invalid_manifest(
            "command arguments must be non-empty shell-independent ASCII argv",
        ));
    }
    let cargo_arguments = || {
        command
            .iter()
            .take_while(|argument| argument.as_str() != "--")
    };
    if !cargo_arguments().any(|argument| argument == "--locked") {
        return Err(invalid_manifest(
            "command must contain `--locked` before the test-harness separator",
        ));
    }
    if !cargo_arguments().any(|argument| argument == "--test") {
        return Err(invalid_manifest(
            "command must name an explicit `--test` target",
        ));
    }
    if cargo_arguments().any(|argument| argument == "--no-run") {
        return Err(invalid_manifest(
            "command cannot use `--no-run` as acceptance evidence",
        ));
    }
    Ok(())
}

fn validate_manifest_shape(manifest: &E2eRunBundleManifest) -> Result<(), E2eRunBundleError> {
    if manifest.schema_version != E2E_RUN_BUNDLE_SCHEMA_VERSION {
        return Err(E2eRunBundleError::InvalidManifest(format!(
            "unsupported schema version {}",
            manifest.schema_version
        )));
    }
    validate_e2e_run_id(&manifest.run_id)?;
    validate_rch_worker_id(&manifest.worker_id)?;
    validate_e2e_source_identity(
        &manifest.source_sha,
        &manifest.source_diff_sha256,
        &manifest.source_tree_sha256,
    )?;
    validate_hex_digest("command_sha256", &manifest.command_sha256, 64)?;
    validate_worker_hostname(&manifest.worker_hostname)?;
    validate_e2e_command(&manifest.command)?;
    if command_sha256(&manifest.command)
        .cmp(&manifest.command_sha256)
        .is_ne()
    {
        return Err(E2eRunBundleError::InvalidManifest(
            "command_sha256 does not match command".to_string(),
        ));
    }
    if manifest.ended_at_unix_ms < manifest.started_at_unix_ms {
        return Err(E2eRunBundleError::InvalidManifest(
            "end timestamp precedes start timestamp".to_string(),
        ));
    }
    if !manifest.complete {
        return Err(E2eRunBundleError::InvalidManifest(
            "manifest is not marked complete".to_string(),
        ));
    }
    let mut file_paths = BTreeSet::new();
    for artifact in &manifest.files {
        let relative = validate_relative_artifact_path(Path::new(&artifact.path))?;
        if relative == E2E_RUN_MANIFEST_FILENAME || relative == E2E_RUN_COMPLETION_FILENAME {
            return Err(invalid_manifest(format_args!(
                "control file `{relative}` must not appear in the artifact list"
            )));
        }
        if file_paths.contains(&relative) {
            return Err(invalid_manifest(format_args!(
                "duplicate artifact path `{relative}`"
            )));
        }
        file_paths.insert(relative);
        validate_hex_digest("artifact sha256", &artifact.sha256, 64)?;
    }
    let mut gate_names = BTreeSet::new();
    for gate in &manifest.gates {
        if gate.name.is_empty() || gate.detail.is_empty() || !gate_names.insert(gate.name.as_str())
        {
            return Err(invalid_manifest(
                "gate names must be non-empty and unique, and gate details must be non-empty",
            ));
        }
    }
    let missing_gates = REQUIRED_E2E_ACCEPTANCE_GATES
        .iter()
        .filter(|required| !gate_names.contains(*required))
        .copied()
        .collect::<Vec<_>>();
    if !missing_gates.is_empty() {
        return Err(E2eRunBundleError::InvalidManifest(format!(
            "missing required acceptance gates: {}",
            missing_gates.join(", ")
        )));
    }
    Ok(())
}

/// Finalize the already-populated run directory with a durable manifest and receipt.
///
/// Existing manifest/receipt files are never overwritten. A crash before the
/// receipt leaves an explicitly incomplete, recoverable directory that cannot
/// pass verification.
pub fn finalize_e2e_run_bundle(
    root: &Path,
    metadata: E2eRunBundleMetadata,
) -> Result<E2eRunBundleManifest, E2eRunBundleError> {
    validate_e2e_run_id(&metadata.run_id)?;
    validate_rch_worker_id(&metadata.worker_id)?;
    validate_e2e_source_identity(
        &metadata.source_sha,
        &metadata.source_diff_sha256,
        &metadata.source_tree_sha256,
    )?;
    validate_worker_hostname(&metadata.worker_hostname)?;
    let root_name = root.file_name().and_then(|name| name.to_str());
    if root_name != Some(metadata.run_id.as_str()) {
        return Err(E2eRunBundleError::InvalidManifest(format!(
            "bundle root `{}` does not end in run id `{}`",
            root.display(),
            metadata.run_id
        )));
    }
    for control in [E2E_RUN_MANIFEST_FILENAME, E2E_RUN_COMPLETION_FILENAME] {
        let path = if control == E2E_RUN_MANIFEST_FILENAME {
            e2e_manifest_path(root)
        } else {
            e2e_completion_path(root)
        };
        if fs::symlink_metadata(&path).is_ok() {
            return Err(invalid_manifest(format_args!(
                "refusing to overwrite existing control file `{}`",
                path.display()
            )));
        }
    }
    let (files, aggregates) = collect_run_artifacts(root)?;
    let mut gates = metadata.gates;
    gates.push(E2eRunGateResult {
        name: "artifact-set-nonempty".to_string(),
        passed: !files.is_empty(),
        detail: format!("{} declared files", files.len()),
    });
    gates.push(E2eRunGateResult {
        name: "trace-artifacts-nonempty".to_string(),
        passed: aggregates.trace_files > 0 && aggregates.trace_events > 0,
        detail: format!(
            "{} trace files, {} events, {} bytes",
            aggregates.trace_files, aggregates.trace_events, aggregates.trace_bytes
        ),
    });
    let required_events = ["run_start", "test_start", "test_end", "run_end"];
    let missing_events = required_events
        .iter()
        .filter(|event| !aggregates.event_histogram.contains_key(**event))
        .copied()
        .collect::<Vec<_>>();
    gates.push(E2eRunGateResult {
        name: "required-event-coverage".to_string(),
        passed: missing_events.is_empty(),
        detail: if missing_events.is_empty() {
            "run_start, test_start, test_end, and run_end are present".to_string()
        } else {
            format!(
                "missing aggregate event types: {}",
                missing_events.join(", ")
            )
        },
    });
    let manifest = E2eRunBundleManifest {
        schema_version: E2E_RUN_BUNDLE_SCHEMA_VERSION,
        run_id: metadata.run_id,
        source_sha: metadata.source_sha,
        source_diff_sha256: metadata.source_diff_sha256,
        source_tree_sha256: metadata.source_tree_sha256,
        worker_id: metadata.worker_id,
        worker_hostname: metadata.worker_hostname,
        command_sha256: command_sha256(&metadata.command),
        command: metadata.command,
        cargo_exit_code: metadata.cargo_exit_code,
        started_at_unix_ms: metadata.started_at_unix_ms,
        ended_at_unix_ms: metadata.ended_at_unix_ms,
        files,
        aggregates,
        gates,
        complete: true,
    };
    validate_manifest_shape(&manifest)?;
    let manifest_path = e2e_manifest_path(root);
    let manifest_bytes = write_durable_json(&manifest_path, &manifest)?;
    let receipt = E2eRunCompletionReceipt {
        schema_version: E2E_RUN_BUNDLE_SCHEMA_VERSION,
        run_id: manifest.run_id.clone(),
        manifest_sha256: hex::encode(Sha256::digest(&manifest_bytes)),
        finalized_at_unix_ms: manifest.ended_at_unix_ms,
    };
    write_durable_json(&e2e_completion_path(root), &receipt)?;
    Ok(manifest)
}

fn read_regular_control_file(path: &Path) -> Result<Vec<u8>, E2eRunBundleError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| bundle_io(path, error))?;
    if metadata.file_type().is_symlink() {
        return Err(E2eRunBundleError::UnsafeEntry {
            path: path.display().to_string(),
            reason: "symbolic control file is forbidden".to_string(),
        });
    }
    reject_aliasing_file(path, &metadata)?;
    fs::read(path).map_err(|error| bundle_io(path, error))
}

/// Recompute every file identity and aggregate after transfer, rejecting stale,
/// missing, extra, aliased, or path-injected evidence.
pub fn verify_e2e_run_bundle(
    root: &Path,
    expected: &E2eRunBundleExpectation,
) -> Result<E2eRunBundleManifest, E2eRunBundleError> {
    let manifest_path = e2e_manifest_path(root);
    let manifest_bytes = read_regular_control_file(&manifest_path)?;
    let manifest: E2eRunBundleManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|source| E2eRunBundleError::Json {
            path: manifest_path.display().to_string(),
            source,
        })?;
    validate_manifest_shape(&manifest)?;
    if root.file_name().and_then(|name| name.to_str()) != Some(manifest.run_id.as_str()) {
        return Err(E2eRunBundleError::InvalidManifest(
            "bundle directory name does not match manifest run_id".to_string(),
        ));
    }
    for (label, actual, wanted) in [
        (
            "run_id",
            manifest.run_id.as_str(),
            expected.run_id.as_deref(),
        ),
        (
            "source_sha",
            manifest.source_sha.as_str(),
            expected.source_sha.as_deref(),
        ),
        (
            "source_diff_sha256",
            manifest.source_diff_sha256.as_str(),
            expected.source_diff_sha256.as_deref(),
        ),
        (
            "source_tree_sha256",
            manifest.source_tree_sha256.as_str(),
            expected.source_tree_sha256.as_deref(),
        ),
        (
            "worker_id",
            manifest.worker_id.as_str(),
            expected.worker_id.as_deref(),
        ),
        (
            "worker_hostname",
            manifest.worker_hostname.as_str(),
            expected.worker_hostname.as_deref(),
        ),
    ] {
        if let Some(wanted) = wanted
            && actual != wanted
        {
            return Err(invalid_manifest(format_args!(
                "{label} mismatch: expected `{wanted}`, found `{actual}`"
            )));
        }
    }
    let receipt_path = e2e_completion_path(root);
    let receipt_bytes = read_regular_control_file(&receipt_path)?;
    let receipt: E2eRunCompletionReceipt =
        serde_json::from_slice(&receipt_bytes).map_err(|source| E2eRunBundleError::Json {
            path: receipt_path.display().to_string(),
            source,
        })?;
    let manifest_digest = hex::encode(Sha256::digest(&manifest_bytes));
    if receipt
        .schema_version
        .cmp(&E2E_RUN_BUNDLE_SCHEMA_VERSION)
        .is_ne()
        || receipt.run_id.cmp(&manifest.run_id).is_ne()
        || receipt.manifest_sha256.cmp(&manifest_digest).is_ne()
        || receipt
            .finalized_at_unix_ms
            .cmp(&manifest.ended_at_unix_ms)
            .is_ne()
    {
        return Err(E2eRunBundleError::InvalidManifest(
            "completion receipt does not bind the exact manifest bytes".to_string(),
        ));
    }

    let (actual_files, actual_aggregates) = collect_run_artifacts(root)?;
    if actual_files != manifest.files {
        return Err(E2eRunBundleError::InvalidManifest(
            "declared file set, bytes, schema, event count, or SHA-256 does not match disk"
                .to_string(),
        ));
    }
    if actual_aggregates != manifest.aggregates {
        return Err(E2eRunBundleError::InvalidManifest(
            "aggregate counts or event histogram do not match disk".to_string(),
        ));
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn base_run() -> ProofRun {
        ProofRun {
            command: "cargo test --lib".to_string(),
            binary_path: Some("/tmp/cass-tgt/debug/cass".to_string()),
            binary_version: Some("0.6.13".to_string()),
            data_dir_or_fixture: Some("fixture:healthy".to_string()),
            exit_code: Some(0),
            elapsed_ms: 1_200,
            timeout_ms: 60_000,
            timed_out: false,
            skipped: false,
            assertions_ran: true,
            produced_artifact: true,
            completed: true,
            artifact_age_ms: Some(1_000),
            stdout_path: Some("/tmp/proof/out.log".to_string()),
            stderr_path: Some("/tmp/proof/err.log".to_string()),
        }
    }

    #[test]
    fn clean_run_is_pass() {
        let a = ProofArtifact::from_run(base_run());
        assert_eq!(a.status, ProofStatus::Pass);
        assert!(a.is_trustworthy_pass());
    }

    #[test]
    fn timeout_before_tests_ran_is_timeout_not_pass() {
        // The exact motivating failure: exit 0-ish but timed out before assertions.
        let mut run = base_run();
        run.exit_code = Some(0);
        run.assertions_ran = false;
        run.elapsed_ms = 7_200_000;
        run.timeout_ms = 7_200_000;
        run.timed_out = true;
        let a = ProofArtifact::from_run(run);
        assert_eq!(
            a.status,
            ProofStatus::Timeout,
            "timeout must outrank a zero exit"
        );
        assert!(!a.is_trustworthy_pass());
    }

    #[test]
    fn elapsed_exceeding_timeout_is_timeout_even_without_flag() {
        let mut run = base_run();
        run.timed_out = false;
        run.timeout_ms = 1_000;
        run.elapsed_ms = 5_000;
        assert_eq!(classify(&run, DEFAULT_STALE_AFTER_MS), ProofStatus::Timeout);
    }

    #[test]
    fn assertions_not_run_is_generated_only() {
        let mut run = base_run();
        run.assertions_ran = false;
        run.produced_artifact = true;
        let a = ProofArtifact::from_run(run);
        assert_eq!(a.status, ProofStatus::GeneratedOnly);
        assert!(!a.is_trustworthy_pass(), "generated-only is never a pass");
    }

    #[test]
    fn nonzero_exit_with_assertions_is_fail() {
        let mut run = base_run();
        run.exit_code = Some(101);
        assert_eq!(classify(&run, DEFAULT_STALE_AFTER_MS), ProofStatus::Fail);
    }

    #[test]
    fn missing_exit_status_is_partial_proof_not_pass() {
        let mut run = base_run();
        run.exit_code = None;
        run.assertions_ran = true;
        run.completed = true;
        let artifact = ProofArtifact::from_run(run);
        assert_eq!(artifact.status, ProofStatus::PartialProof);
        assert!(!artifact.is_trustworthy_pass());
    }

    #[test]
    fn skipped_run_is_skipped() {
        let mut run = base_run();
        run.skipped = true;
        assert_eq!(classify(&run, DEFAULT_STALE_AFTER_MS), ProofStatus::Skipped);
    }

    #[test]
    fn old_artifact_is_stale() {
        let mut run = base_run();
        run.artifact_age_ms = Some(48 * 60 * 60 * 1000);
        assert_eq!(
            classify(&run, DEFAULT_STALE_AFTER_MS),
            ProofStatus::StaleArtifact
        );
    }

    #[test]
    fn incomplete_run_is_partial_proof() {
        let mut run = base_run();
        run.completed = false;
        assert_eq!(
            classify(&run, DEFAULT_STALE_AFTER_MS),
            ProofStatus::PartialProof
        );
    }

    #[test]
    fn precedence_timeout_outranks_skipped_and_stale() {
        let mut run = base_run();
        run.timed_out = true;
        run.skipped = true;
        run.artifact_age_ms = Some(u64::MAX);
        assert_eq!(classify(&run, DEFAULT_STALE_AFTER_MS), ProofStatus::Timeout);
    }

    #[test]
    fn artifact_serializes_with_stable_fields_and_round_trips() {
        let a = ProofArtifact::from_run(base_run());
        let value = serde_json::to_value(&a).unwrap();
        assert_eq!(value["schema_version"], PROOF_ARTIFACT_SCHEMA_VERSION);
        assert_eq!(value["status"], "pass");
        assert_eq!(value["run"]["command"], "cargo test --lib");
        assert_eq!(value["run"]["assertions_ran"], true);
        assert_eq!(value["run"]["exit_code"], 0);
        assert_eq!(value["run"]["stdout_path"], "/tmp/proof/out.log");
        let back: ProofArtifact = serde_json::from_value(value).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn status_wire_values_are_kebab() {
        for (s, w) in [
            (ProofStatus::Pass, "pass"),
            (ProofStatus::Fail, "fail"),
            (ProofStatus::PartialProof, "partial-proof"),
            (ProofStatus::GeneratedOnly, "generated-only"),
            (ProofStatus::Skipped, "skipped"),
            (ProofStatus::StaleArtifact, "stale-artifact"),
            (ProofStatus::Timeout, "timeout"),
        ] {
            assert_eq!(serde_json::to_string(&s).unwrap(), format!("\"{w}\""));
            assert_eq!(s.as_str(), w);
        }
    }

    // ---- emission + manifest (the "record proof artifacts" half of .11.4) ----

    #[test]
    fn emit_writes_a_classified_artifact_file_that_round_trips() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let emitted = emit_proof_artifact(dir.path(), "clean run", base_run()).expect("emit");
        assert_eq!(emitted.status, ProofStatus::Pass);
        // The label is sanitized into the file stem (space -> '_').
        assert!(
            emitted.path.ends_with("clean_run.proof.json"),
            "{}",
            emitted.path
        );

        let bytes = std::fs::read(&emitted.path).expect("read artifact");
        let back: ProofArtifact = serde_json::from_slice(&bytes).expect("parse artifact");
        assert_eq!(back.status, ProofStatus::Pass);
        assert_eq!(back.run.command, "cargo test --lib");
        assert_eq!(back.schema_version, PROOF_ARTIFACT_SCHEMA_VERSION);
    }

    #[test]
    fn emitted_timeout_before_tests_is_recorded_as_timeout_not_pass() {
        // The motivating trap: exit 0-ish but timed out before assertions ran.
        let mut run = base_run();
        run.exit_code = Some(0);
        run.assertions_ran = false;
        run.timed_out = true;
        run.elapsed_ms = 7_200_000;
        run.timeout_ms = 7_200_000;

        let dir = tempfile::TempDir::new().expect("tempdir");
        let emitted = emit_proof_artifact(dir.path(), "lib-tests", run).expect("emit");
        assert_eq!(emitted.status, ProofStatus::Timeout);

        // The persisted artifact agrees — a reader cannot mistake it for a pass.
        let bytes = std::fs::read(&emitted.path).expect("read");
        let back: ProofArtifact = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(back.status, ProofStatus::Timeout);
        assert!(!back.is_trustworthy_pass());
    }

    #[test]
    fn empty_manifest_cannot_pass_by_doing_nothing() {
        let manifest = ProofManifest::new();
        assert!(
            !manifest.is_clean_pass(),
            "an empty manifest is never a pass"
        );
        assert_eq!(manifest.worst_status(), None);
    }

    #[test]
    fn manifest_of_all_passes_is_a_clean_pass() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let mut manifest = ProofManifest::new();
        for label in ["a", "b", "c"] {
            manifest.record(emit_proof_artifact(dir.path(), label, base_run()).expect("emit"));
        }
        assert!(manifest.is_clean_pass());
        assert_eq!(manifest.worst_status(), Some(ProofStatus::Pass));
        assert_eq!(manifest.count_with(ProofStatus::Pass), 3);
    }

    #[test]
    fn a_single_timeout_entry_sinks_the_manifest_verdict() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let mut manifest = ProofManifest::new();
        manifest.record(emit_proof_artifact(dir.path(), "ok", base_run()).expect("emit"));
        let mut timed = base_run();
        timed.timed_out = true;
        manifest.record(emit_proof_artifact(dir.path(), "hang", timed).expect("emit"));

        assert!(
            !manifest.is_clean_pass(),
            "one timeout must sink the verdict"
        );
        // `max` over the status ordering surfaces the worst outcome.
        assert_eq!(manifest.worst_status(), Some(ProofStatus::Timeout));
        assert_eq!(manifest.count_with(ProofStatus::Timeout), 1);
        assert_eq!(manifest.count_with(ProofStatus::Pass), 1);
    }

    #[test]
    fn manifest_jsonl_round_trips_every_entry() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let mut manifest = ProofManifest::new();
        manifest.record(emit_proof_artifact(dir.path(), "one", base_run()).expect("emit"));
        manifest.record(emit_proof_artifact(dir.path(), "two", base_run()).expect("emit"));
        let manifest_path = dir.path().join("proof-manifest.jsonl");
        manifest
            .write_jsonl(&manifest_path)
            .expect("write manifest");

        let text = std::fs::read_to_string(&manifest_path).expect("read manifest");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let entry: EmittedProof = serde_json::from_str(line).expect("parse entry");
            assert_eq!(entry.status, ProofStatus::Pass);
            assert!(entry.path.ends_with(".proof.json"));
        }
    }

    #[test]
    fn blank_label_falls_back_to_a_stable_stem() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let emitted = emit_proof_artifact(dir.path(), "///", base_run()).expect("emit");
        assert!(emitted.path.ends_with("___.proof.json"), "{}", emitted.path);
        let emitted_empty = emit_proof_artifact(dir.path(), "", base_run()).expect("emit");
        assert!(
            emitted_empty.path.ends_with("proof.proof.json"),
            "{}",
            emitted_empty.path
        );
    }

    fn bundle_metadata(run_id: &str) -> E2eRunBundleMetadata {
        E2eRunBundleMetadata {
            run_id: run_id.to_string(),
            source_sha: "a".repeat(40),
            source_diff_sha256: "b".repeat(64),
            source_tree_sha256: "c".repeat(64),
            worker_id: "vmi1149989".to_string(),
            worker_hostname: "worker-a".to_string(),
            command: vec![
                "cargo".to_string(),
                "test".to_string(),
                "--locked".to_string(),
                "--test".to_string(),
                "e2e_semantic_search".to_string(),
                "--".to_string(),
                "--test-threads=1".to_string(),
            ],
            cargo_exit_code: 0,
            started_at_unix_ms: 1_000,
            ended_at_unix_ms: 2_000,
            gates: vec![
                E2eRunGateResult {
                    name: "cargo-tests".to_string(),
                    passed: true,
                    detail: "exit 0".to_string(),
                },
                E2eRunGateResult {
                    name: "jsonl-schema".to_string(),
                    passed: true,
                    detail: "validator exit 0".to_string(),
                },
                E2eRunGateResult {
                    name: "bounded-capture-complete".to_string(),
                    passed: true,
                    detail: "all command output captured".to_string(),
                },
            ],
        }
    }

    type BundleTestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn populated_bundle(parent: &Path, run_id: &str) -> BundleTestResult<PathBuf> {
        let root = parent.join(run_id);
        std::fs::create_dir_all(root.join("suite/test"))?;
        std::fs::write(
            root.join("suite/test/trace.jsonl"),
            concat!(
                "{\"schema_version\":\"cass-trace-v1\",\"timestamp\":\"2026-01-01T00:00:00Z\",",
                "\"level\":\"INFO\",\"target\":\"cass::search\",\"trace_id\":\"trace-a\",",
                "\"test_id\":\"suite/test\",\"fields\":{\"event\":\"command_summary\"}}\n"
            ),
        )?;
        std::fs::write(
            root.join("suite/test/events.jsonl"),
            concat!(
                "{\"ts\":\"2026-01-01T00:00:00Z\",\"event\":\"run_start\",",
                "\"run_id\":\"fixture\",\"runner\":\"rust\"}\n",
                "{\"ts\":\"2026-01-01T00:00:01Z\",\"event\":\"test_start\",",
                "\"run_id\":\"fixture\",\"runner\":\"rust\"}\n",
                "{\"ts\":\"2026-01-01T00:00:02Z\",\"event\":\"test_end\",",
                "\"run_id\":\"fixture\",\"runner\":\"rust\"}\n",
                "{\"ts\":\"2026-01-01T00:00:03Z\",\"event\":\"run_end\",",
                "\"run_id\":\"fixture\",\"runner\":\"rust\"}\n",
            ),
        )?;
        std::fs::write(root.join("suite/test/stdout"), b"bounded output\n")?;
        Ok(root)
    }

    fn expected_bundle_error<T>(
        result: Result<T, E2eRunBundleError>,
        context: &str,
    ) -> BundleTestResult<E2eRunBundleError> {
        match result {
            Ok(_) => Err(format!("{context}: operation unexpectedly succeeded").into()),
            Err(error) => Ok(error),
        }
    }

    fn bundle_test_failure(arguments: fmt::Arguments<'_>) -> Box<dyn std::error::Error> {
        arguments.to_string().into()
    }

    fn bundle_error_contains(error: &E2eRunBundleError, needle: &str) -> bool {
        error.to_string().contains(needle)
    }

    #[test]
    fn run_and_worker_ids_reject_path_and_shell_injection() -> BundleTestResult {
        for invalid in ["", "short", "../escape", "run id", "-leading", "trailing-"] {
            if validate_e2e_run_id(invalid).is_ok() {
                return Err(bundle_test_failure(format_args!(
                    "accepted invalid run id {invalid:?}"
                )));
            }
        }
        for valid in [
            "acceptance-20260728-a1b2c3",
            "run_12345678",
            "ABCdef-012345",
        ] {
            validate_e2e_run_id(valid)?;
        }
        for invalid in ["", "../worker", "worker id", "-worker", "worker-"] {
            if validate_rch_worker_id(invalid).is_ok() {
                return Err(bundle_test_failure(format_args!(
                    "accepted invalid worker id {invalid:?}"
                )));
            }
        }
        validate_rch_worker_id("vmi1149989")?;
        validate_rch_worker_id("a")?;
        validate_e2e_source_identity(&"a".repeat(40), &"b".repeat(64), &"c".repeat(64))?;
        if validate_e2e_source_identity(&"A".repeat(40), &"b".repeat(64), &"c".repeat(64)).is_ok() {
            return Err("accepted uppercase source digest".into());
        }
        if validate_e2e_source_identity(&"a".repeat(40), &"b".repeat(64), &"C".repeat(64)).is_ok() {
            return Err("accepted uppercase source-tree digest".into());
        }
        Ok(())
    }

    #[test]
    fn source_tree_digest_is_order_independent_and_shell_framed() -> BundleTestResult {
        let first = tempfile::TempDir::new()?;
        let second = tempfile::TempDir::new()?;
        std::fs::create_dir_all(first.path().join("src"))?;
        std::fs::create_dir_all(second.path().join("src"))?;
        std::fs::write(first.path().join("src/z.rs"), b"z\n")?;
        std::fs::write(first.path().join("Cargo.toml"), b"[package]\n")?;
        std::fs::write(second.path().join("Cargo.toml"), b"[package]\n")?;
        std::fs::write(second.path().join("src/z.rs"), b"z\n")?;

        let first_digest = e2e_source_tree_sha256(first.path())?;
        let second_digest = e2e_source_tree_sha256(second.path())?;
        if first_digest.cmp(&second_digest).is_ne() {
            return Err("source-tree digest depends on filesystem creation order".into());
        }

        let cargo_digest = hex::encode(Sha256::digest(b"[package]\n"));
        let source_digest = hex::encode(Sha256::digest(b"z\n"));
        let mut expected = Sha256::new();
        expected.update(cargo_digest.as_bytes());
        expected.update(b"  Cargo.toml\0");
        expected.update(source_digest.as_bytes());
        expected.update(b"  src/z.rs\0");
        let expected = hex::encode(expected.finalize());
        if first_digest.cmp(&expected).is_ne() {
            return Err(format!(
                "source-tree digest does not match sha256sum --zero framing: expected {expected}, found {first_digest}"
            )
            .into());
        }
        Ok(())
    }

    #[test]
    fn source_tree_digest_detects_changed_and_extra_source_files() -> BundleTestResult {
        let dir = tempfile::TempDir::new()?;
        std::fs::create_dir_all(dir.path().join("src"))?;
        std::fs::write(dir.path().join("src/lib.rs"), b"pub fn original() {}\n")?;
        let original = e2e_source_tree_sha256(dir.path())?;

        std::fs::write(dir.path().join("src/lib.rs"), b"pub fn changed() {}\n")?;
        let changed = e2e_source_tree_sha256(dir.path())?;
        if changed == original {
            return Err("changed source bytes preserved the tree digest".into());
        }

        std::fs::write(dir.path().join("src/extra.rs"), b"pub fn extra() {}\n")?;
        let extra = e2e_source_tree_sha256(dir.path())?;
        if extra == changed {
            return Err("extra source file preserved the tree digest".into());
        }
        Ok(())
    }

    #[test]
    fn source_tree_digest_ignores_only_declared_top_level_transients() -> BundleTestResult {
        let dir = tempfile::TempDir::new()?;
        std::fs::create_dir_all(dir.path().join("src/target"))?;
        std::fs::write(dir.path().join("src/target/tracked.rs"), b"tracked\n")?;
        let original = e2e_source_tree_sha256(dir.path())?;

        for transient in [
            ".git",
            ".beads",
            ".rch-tmp",
            ".rch-target-debug",
            "target",
            "test-results",
        ] {
            let transient_root = dir.path().join(transient);
            std::fs::create_dir_all(&transient_root)?;
            std::fs::write(transient_root.join("ignored"), transient.as_bytes())?;
        }
        let with_transients = e2e_source_tree_sha256(dir.path())?;
        if with_transients != original {
            return Err("declared top-level transport output changed source digest".into());
        }

        std::fs::write(dir.path().join("src/target/tracked.rs"), b"changed\n")?;
        if e2e_source_tree_sha256(dir.path())? == with_transients {
            return Err("nested directory with a transient name was incorrectly excluded".into());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn source_tree_digest_rejects_symlink_and_hardlink_aliases() -> BundleTestResult {
        use std::os::unix::fs::symlink;

        let symlink_dir = tempfile::TempDir::new()?;
        std::fs::write(symlink_dir.path().join("real"), b"source\n")?;
        symlink("real", symlink_dir.path().join("alias"))?;
        let symlink_error =
            expected_bundle_error(e2e_source_tree_sha256(symlink_dir.path()), "symlink source")?;
        if !bundle_error_contains(&symlink_error, "symbolic source entries") {
            return Err(format!("unexpected symlink source error: {symlink_error}").into());
        }

        let hardlink_dir = tempfile::TempDir::new()?;
        let first = hardlink_dir.path().join("first");
        std::fs::write(&first, b"source\n")?;
        std::fs::hard_link(&first, hardlink_dir.path().join("second"))?;
        let hardlink_error = expected_bundle_error(
            e2e_source_tree_sha256(hardlink_dir.path()),
            "hardlink source",
        )?;
        if !bundle_error_contains(&hardlink_error, "hard links") {
            return Err(format!("unexpected hardlink source error: {hardlink_error}").into());
        }
        Ok(())
    }

    #[test]
    fn command_digest_has_a_fixed_shell_reproducible_test_vector() -> BundleTestResult {
        let command =
            ["cargo", "test", "--locked", "--test", "e2e_semantic_search"].map(ToString::to_string);
        let actual = command_sha256(&command);
        let expected = "f3cd72a9cac25d52fe925957068c8f3a58a2bad11dffe64d0fab10587c2cb088";
        if actual.as_str().cmp(expected).is_ne() {
            return Err(
                format!("command digest mismatch: expected {expected}, found {actual}").into(),
            );
        }
        Ok(())
    }

    #[test]
    fn manifest_rejects_an_incomplete_or_reclassified_cargo_command() -> BundleTestResult {
        for (run_id, command, expected_error) in [
            (
                "acceptance-20260728-missing-target",
                vec!["cargo", "test", "--locked"],
                "explicit `--test` target",
            ),
            (
                "acceptance-20260728-unlocked",
                vec!["cargo", "test", "--test", "e2e_semantic_search"],
                "`--locked` before",
            ),
            (
                "acceptance-20260728-no-run",
                vec![
                    "cargo",
                    "test",
                    "--locked",
                    "--test",
                    "e2e_semantic_search",
                    "--no-run",
                ],
                "cannot use `--no-run`",
            ),
            (
                "acceptance-20260728-shell-text",
                vec!["cargo", "test", "--locked", "--test", "e2e semantic search"],
                "shell-independent ASCII argv",
            ),
        ] {
            let parent = tempfile::TempDir::new()?;
            let root = populated_bundle(parent.path(), run_id)?;
            let mut metadata = bundle_metadata(run_id);
            metadata.command = command.into_iter().map(ToString::to_string).collect();
            let error = expected_bundle_error(
                finalize_e2e_run_bundle(&root, metadata),
                "incomplete command",
            )?;
            if !bundle_error_contains(&error, expected_error) {
                return Err(bundle_test_failure(format_args!(
                    "unexpected command validation error: {error}"
                )));
            }
        }
        Ok(())
    }

    #[test]
    fn finalized_bundle_round_trips_exact_files_and_receipt() -> BundleTestResult {
        let parent = tempfile::TempDir::new()?;
        let run_id = "acceptance-20260728-roundtrip";
        let root = populated_bundle(parent.path(), run_id)?;
        let manifest = finalize_e2e_run_bundle(&root, bundle_metadata(run_id))?;
        if !manifest.is_acceptance_pass()
            || manifest.aggregates.files != 3
            || manifest.aggregates.trace_files != 1
            || manifest.aggregates.trace_events != 1
            || manifest
                .aggregates
                .event_histogram
                .get("command_summary")
                .copied()
                != Some(1)
        {
            return Err(format!("unexpected finalized manifest: {manifest:?}").into());
        }

        let verified = verify_e2e_run_bundle(
            &root,
            &E2eRunBundleExpectation {
                run_id: Some(run_id.to_string()),
                source_sha: Some("a".repeat(40)),
                source_diff_sha256: Some("b".repeat(64)),
                source_tree_sha256: Some("c".repeat(64)),
                worker_id: Some("vmi1149989".to_string()),
                worker_hostname: None,
            },
        )?;
        if verified.ne(&manifest) {
            return Err("verified bundle differs from finalized manifest".into());
        }
        Ok(())
    }

    #[test]
    fn acceptance_requires_every_named_producer_and_automatic_gate() -> BundleTestResult {
        let parent = tempfile::TempDir::new()?;
        let run_id = "acceptance-20260728-required-gates";
        let root = populated_bundle(parent.path(), run_id)?;
        let mut metadata = bundle_metadata(run_id);
        metadata
            .gates
            .retain(|gate| gate.name != "bounded-capture-complete");
        let error = expected_bundle_error(
            finalize_e2e_run_bundle(&root, metadata),
            "manifest missing a required producer gate",
        )?;
        if !error
            .to_string()
            .contains("missing required acceptance gates: bounded-capture-complete")
        {
            return Err(format!("unexpected error: {error}").into());
        }
        Ok(())
    }

    #[test]
    fn unfinalized_bundle_without_control_files_never_verifies() -> BundleTestResult {
        let parent = tempfile::TempDir::new()?;
        let run_id = "acceptance-20260728-unfinalized";
        let root = populated_bundle(parent.path(), run_id)?;
        let error = expected_bundle_error(
            verify_e2e_run_bundle(&root, &E2eRunBundleExpectation::default()),
            "unfinalized bundle",
        )?;
        if !error.to_string().contains("manifest.json") {
            return Err(format!("unexpected error: {error}").into());
        }
        Ok(())
    }

    #[test]
    fn changed_or_extra_file_cannot_reuse_a_valid_receipt() -> BundleTestResult {
        let changed_parent = tempfile::TempDir::new()?;
        let changed_id = "acceptance-20260728-changed";
        let changed_root = populated_bundle(changed_parent.path(), changed_id)?;
        finalize_e2e_run_bundle(&changed_root, bundle_metadata(changed_id))?;
        std::fs::write(changed_root.join("suite/test/stdout"), b"tampered\n")?;
        let changed_error = expected_bundle_error(
            verify_e2e_run_bundle(&changed_root, &E2eRunBundleExpectation::default()),
            "changed artifact",
        )?;
        if !changed_error.to_string().contains("declared file set") {
            return Err(format!("unexpected changed-file error: {changed_error}").into());
        }

        let extra_parent = tempfile::TempDir::new()?;
        let extra_id = "acceptance-20260728-extra";
        let extra_root = populated_bundle(extra_parent.path(), extra_id)?;
        finalize_e2e_run_bundle(&extra_root, bundle_metadata(extra_id))?;
        std::fs::write(extra_root.join("untracked-extra"), b"stale evidence\n")?;
        let extra_error = expected_bundle_error(
            verify_e2e_run_bundle(&extra_root, &E2eRunBundleExpectation::default()),
            "extra artifact",
        )?;
        if !extra_error.to_string().contains("declared file set") {
            return Err(format!("unexpected extra-file error: {extra_error}").into());
        }
        Ok(())
    }

    #[test]
    fn worker_or_source_drift_fails_after_integrity_verification() -> BundleTestResult {
        let parent = tempfile::TempDir::new()?;
        let run_id = "acceptance-20260728-drift";
        let root = populated_bundle(parent.path(), run_id)?;
        finalize_e2e_run_bundle(&root, bundle_metadata(run_id))?;
        let expectations = [
            E2eRunBundleExpectation {
                worker_id: Some("vmi1152480".to_string()),
                ..Default::default()
            },
            E2eRunBundleExpectation {
                source_sha: Some("c".repeat(40)),
                ..Default::default()
            },
            E2eRunBundleExpectation {
                source_diff_sha256: Some("d".repeat(64)),
                ..Default::default()
            },
            E2eRunBundleExpectation {
                source_tree_sha256: Some("e".repeat(64)),
                ..Default::default()
            },
        ];
        for expectation in expectations {
            let error = expected_bundle_error(
                verify_e2e_run_bundle(&root, &expectation),
                "provenance drift",
            )?;
            if !bundle_error_contains(&error, "mismatch") {
                return Err(bundle_test_failure(format_args!(
                    "unexpected provenance error: {error}"
                )));
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_hardlink_aliases_are_rejected_before_manifest_creation() -> BundleTestResult {
        use std::os::unix::fs::symlink;

        let symlink_parent = tempfile::TempDir::new()?;
        let symlink_id = "acceptance-20260728-symlink";
        let symlink_root = populated_bundle(symlink_parent.path(), symlink_id)?;
        let outside = symlink_parent.path().join("outside");
        std::fs::write(&outside, b"outside\n")?;
        symlink(&outside, symlink_root.join("suite/test/alias"))?;
        let symlink_error = expected_bundle_error(
            finalize_e2e_run_bundle(&symlink_root, bundle_metadata(symlink_id)),
            "symlink artifact",
        )?;
        if !symlink_error.to_string().contains("symbolic links") {
            return Err(format!("unexpected symlink error: {symlink_error}").into());
        }

        let hardlink_parent = tempfile::TempDir::new()?;
        let hardlink_id = "acceptance-20260728-hardlink";
        let hardlink_root = populated_bundle(hardlink_parent.path(), hardlink_id)?;
        std::fs::hard_link(
            hardlink_root.join("suite/test/stdout"),
            hardlink_root.join("suite/test/stdout-alias"),
        )?;
        let hardlink_error = expected_bundle_error(
            finalize_e2e_run_bundle(&hardlink_root, bundle_metadata(hardlink_id)),
            "hardlink artifact",
        )?;
        if !hardlink_error.to_string().contains("hard links") {
            return Err(format!("unexpected hardlink error: {hardlink_error}").into());
        }
        Ok(())
    }

    #[test]
    fn two_run_ids_never_consume_each_others_artifacts() -> BundleTestResult {
        let parent = tempfile::TempDir::new()?;
        let left_id = "acceptance-20260728-left";
        let right_id = "acceptance-20260728-right";
        let left = populated_bundle(parent.path(), left_id)?;
        let right = populated_bundle(parent.path(), right_id)?;
        std::fs::write(left.join("left-only"), b"left\n")?;
        std::fs::write(right.join("right-only"), b"right\n")?;
        let left_manifest = finalize_e2e_run_bundle(&left, bundle_metadata(left_id))?;
        let right_manifest = finalize_e2e_run_bundle(&right, bundle_metadata(right_id))?;
        let left_isolated = left_manifest
            .files
            .iter()
            .any(|artifact| artifact.path == "left-only")
            && left_manifest
                .files
                .iter()
                .all(|artifact| artifact.path != "right-only");
        let right_isolated = right_manifest
            .files
            .iter()
            .any(|artifact| artifact.path == "right-only")
            && right_manifest
                .files
                .iter()
                .all(|artifact| artifact.path != "left-only");
        if !left_isolated || !right_isolated {
            return Err("parallel run manifests consumed each other's artifacts".into());
        }
        Ok(())
    }

    #[test]
    fn failed_underlying_cargo_is_integrity_valid_but_never_an_acceptance_pass() -> BundleTestResult
    {
        let parent = tempfile::TempDir::new()?;
        let run_id = "acceptance-20260728-failed";
        let root = populated_bundle(parent.path(), run_id)?;
        let mut metadata = bundle_metadata(run_id);
        metadata.cargo_exit_code = 101;
        let cargo_gate = metadata
            .gates
            .first_mut()
            .ok_or("test metadata did not contain the cargo-tests gate")?;
        cargo_gate.passed = false;
        cargo_gate.detail = "exit 101".to_string();
        let manifest = finalize_e2e_run_bundle(&root, metadata)?;
        if manifest.is_acceptance_pass() {
            return Err("failed cargo run passed acceptance".into());
        }
        let verified = verify_e2e_run_bundle(&root, &E2eRunBundleExpectation::default())?;
        if verified.cargo_exit_code != 101 || verified.is_acceptance_pass() {
            return Err(format!("failed cargo run verified incorrectly: {verified:?}").into());
        }
        Ok(())
    }
}
