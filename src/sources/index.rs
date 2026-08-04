//! Remote cass indexing via SSH.
//!
//! This module provides functionality to trigger `cass index` on remote machines
//! after installation, ensuring session data is ready to sync.
//!
//! # Why This Matters
//!
//! Syncing works by pulling from the remote's indexed data. If the remote has
//! never run `cass index`, there's nothing meaningful to sync. This module
//! ensures remotes are indexed before attempting sync.
//!
//! # Example
//!
//! ```rust,ignore
//! use coding_agent_search::sources::index::{RemoteIndexer, IndexProgress};
//! use coding_agent_search::sources::probe::HostProbeResult;
//!
//! // Check if indexing is needed
//! if RemoteIndexer::needs_indexing(&probe_result) {
//!     let indexer = RemoteIndexer::new("laptop", 600);
//!
//!     indexer.run_index(|progress| {
//!         println!("{}: {}", progress.stage, progress.message);
//!     })?;
//! }
//! ```

use std::io::Write as IoWrite;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    configure_child_process_group, host_key_verification_error, is_host_key_verification_failure,
    probe::{CassStatus, HostProbeResult},
    strict_ssh_cli_tokens, wait_for_child_output_with_timeout,
};

// =============================================================================
// Constants
// =============================================================================

/// Default SSH connection timeout for index commands.
pub const DEFAULT_INDEX_TIMEOUT_SECS: u64 = 600; // 10 minutes

/// Poll interval when waiting for long-running index.
pub const INDEX_POLL_INTERVAL_SECS: u64 = 5;

/// Maximum wait time for indexing (30 minutes for large histories).
pub const MAX_INDEX_WAIT_SECS: u64 = 1800;

/// Remote load-per-core ceiling before offloaded indexing defers.
const REMOTE_INDEX_MAX_LOAD_PER_CPU: f64 = 1.50;

/// Minimum remote MemAvailable before offloaded indexing defers (512 MiB).
const REMOTE_INDEX_MIN_AVAILABLE_MEM_KIB: u64 = 512 * 1024;

// =============================================================================
// Error Types
// =============================================================================

/// Errors that can occur during remote indexing.
#[derive(Error, Debug)]
pub enum IndexError {
    #[error("SSH connection failed: {0}")]
    SshFailed(String),

    #[error("Index operation timed out after {0} seconds")]
    Timeout(u64),

    #[error("cass not found on remote host")]
    CassNotFound,

    #[error("Indexing failed: {stdout}\n{stderr}")]
    IndexFailed {
        stdout: String,
        stderr: String,
        exit_code: i32,
    },

    #[error("Disk full on remote host")]
    DiskFull,

    #[error("Permission denied accessing agent data directories")]
    PermissionDenied,

    #[error("Remote host pressure guard deferred indexing: {0}")]
    HostPressure(String),

    #[error("Indexing cancelled")]
    Cancelled,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl IndexError {
    /// Get a user-friendly help message for this error.
    pub fn help_message(&self) -> &'static str {
        match self {
            IndexError::DiskFull => "Free disk space on remote and retry.",
            IndexError::Timeout(_) => {
                "Index timed out. Try running manually: ssh host 'cass index'"
            }
            IndexError::PermissionDenied => "Check file permissions in agent data directories.",
            IndexError::CassNotFound => "cass is not installed. Run installation first.",
            IndexError::SshFailed(_) => "Check SSH connection and credentials.",
            IndexError::HostPressure(_) => {
                "Remote host is currently busy. Retry later or run indexing manually when idle."
            }
            _ => "See error details above.",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct RemoteHostPressureSnapshot {
    cpus: Option<u64>,
    load1: Option<f64>,
    mem_available_kib: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
struct RemoteHostPressureDecision {
    defer_index: bool,
    reason: String,
    snapshot: RemoteHostPressureSnapshot,
}

impl RemoteHostPressureSnapshot {
    fn from_command_output(output: &str) -> Self {
        let mut snapshot = Self {
            cpus: None,
            load1: None,
            mem_available_kib: None,
        };

        for line in output.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "CPUS" => snapshot.cpus = value.trim().parse::<u64>().ok().filter(|v| *v > 0),
                "LOAD1" => {
                    snapshot.load1 = value.trim().parse::<f64>().ok().filter(|v| v.is_finite())
                }
                "MEM_AVAILABLE_KIB" => {
                    snapshot.mem_available_kib = value.trim().parse::<u64>().ok()
                }
                _ => {}
            }
        }

        snapshot
    }

    fn decide(self) -> RemoteHostPressureDecision {
        let mut reasons = Vec::new();

        if let (Some(load1), Some(cpus)) = (self.load1, self.cpus) {
            let load_per_cpu = load1 / cpus as f64;
            if load_per_cpu > REMOTE_INDEX_MAX_LOAD_PER_CPU {
                reasons.push(format!(
                    "load_per_cpu={load_per_cpu:.2} exceeds ceiling {REMOTE_INDEX_MAX_LOAD_PER_CPU:.2}"
                ));
            }
        }

        if let Some(mem_available_kib) = self.mem_available_kib
            && mem_available_kib < REMOTE_INDEX_MIN_AVAILABLE_MEM_KIB
        {
            reasons.push(format!(
                "mem_available_kib={mem_available_kib} below floor {REMOTE_INDEX_MIN_AVAILABLE_MEM_KIB}"
            ));
        }

        let defer_index = !reasons.is_empty();
        let reason = if defer_index {
            reasons.join("; ")
        } else if self.cpus.is_none() || self.load1.is_none() || self.mem_available_kib.is_none() {
            "remote pressure metrics incomplete; allowing conservative fallback path".to_string()
        } else {
            "remote host pressure is within indexing budget".to_string()
        };

        RemoteHostPressureDecision {
            defer_index,
            reason,
            snapshot: self,
        }
    }
}

// =============================================================================
// Progress Types
// =============================================================================

/// Current stage of indexing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum IndexStage {
    /// Starting the index process.
    Starting,
    /// Scanning agent directories for sessions.
    Scanning { agent: String },
    /// Building the search index.
    Building,
    /// Index complete.
    Complete,
    /// Index failed.
    Failed { error: String },
}

impl std::fmt::Display for IndexStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexStage::Starting => write!(f, "Starting"),
            IndexStage::Scanning { agent } => write!(f, "Scanning {}", agent),
            IndexStage::Building => write!(f, "Building index"),
            IndexStage::Complete => write!(f, "Complete"),
            IndexStage::Failed { error } => write!(f, "Failed: {}", error),
        }
    }
}

/// Progress update during indexing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexProgress {
    /// Current stage.
    pub stage: IndexStage,
    /// Human-readable message.
    pub message: String,
    /// Number of sessions found during scanning.
    pub sessions_found: u64,
    /// Number of sessions indexed so far.
    pub sessions_indexed: u64,
    /// Optional progress percentage (0-100).
    pub percent: Option<u8>,
    /// Elapsed time since start.
    pub elapsed: Duration,
}

/// Result of a successful indexing operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexResult {
    /// Whether indexing completed successfully.
    pub success: bool,
    /// Total sessions indexed.
    pub sessions_indexed: u64,
    /// Total indexing time.
    pub duration: Duration,
    /// Error message if failed.
    pub error: Option<String>,
    /// Remote lexical artifact proof written after a successful index run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_manifest: Option<RemoteArtifactManifestResult>,
}

/// Result of writing a remote lexical artifact evidence manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteArtifactManifestResult {
    /// Whether the proof command completed and produced a complete manifest.
    pub success: bool,
    /// Path to evidence-bundle-manifest.json on the remote host.
    pub manifest_path: Option<String>,
    /// Deterministic content-addressed bundle id.
    pub bundle_id: Option<String>,
    /// Number of files described by the manifest.
    pub chunk_count: Option<usize>,
    /// Total bytes expected by the evidence report.
    pub expected_bytes: Option<u64>,
    /// Verification status reported by the remote command.
    pub verification_status: Option<String>,
    /// Error message when the proof command failed.
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RemoteArtifactManifestCommandOutput {
    manifest_path: Option<String>,
    bundle_id: Option<String>,
    chunk_count: Option<usize>,
    expected_bytes: Option<u64>,
    verification_status: Option<String>,
}

impl RemoteArtifactManifestCommandOutput {
    fn has_manifest_identity(&self) -> bool {
        self.manifest_path.is_some() || self.bundle_id.is_some()
    }

    fn has_complete_manifest_shape(&self) -> bool {
        self.manifest_path.is_some()
            && self.bundle_id.is_some()
            && self.verification_status.is_some()
    }
}

impl RemoteArtifactManifestResult {
    fn from_command_output(output: &str) -> Self {
        match parse_remote_artifact_manifest_output(output) {
            Ok(parsed) => {
                let complete = parsed.verification_status.as_deref() == Some("complete");
                Self {
                    success: complete,
                    manifest_path: parsed.manifest_path,
                    bundle_id: parsed.bundle_id,
                    chunk_count: parsed.chunk_count,
                    expected_bytes: parsed.expected_bytes,
                    verification_status: parsed.verification_status,
                    error: if complete {
                        None
                    } else {
                        Some("remote artifact manifest verification was not complete".to_string())
                    },
                }
            }
            Err(err) => Self {
                success: false,
                manifest_path: None,
                bundle_id: None,
                chunk_count: None,
                expected_bytes: None,
                verification_status: None,
                error: Some(format!(
                    "failed to parse remote artifact manifest output: {err}"
                )),
            },
        }
    }

    fn from_error(error: impl Into<String>) -> Self {
        Self {
            success: false,
            manifest_path: None,
            bundle_id: None,
            chunk_count: None,
            expected_bytes: None,
            verification_status: None,
            error: Some(error.into()),
        }
    }
}

fn parse_remote_artifact_manifest_output(
    output: &str,
) -> serde_json::Result<RemoteArtifactManifestCommandOutput> {
    let direct = serde_json::from_str::<RemoteArtifactManifestCommandOutput>(output);
    if direct.is_ok() {
        return direct;
    }

    let mut fallback = None;
    for (idx, _) in output.char_indices().filter(|(_, ch)| *ch == '{') {
        let mut deserializer = serde_json::Deserializer::from_str(&output[idx..]);
        if let Ok(parsed) = RemoteArtifactManifestCommandOutput::deserialize(&mut deserializer) {
            if parsed.has_complete_manifest_shape() {
                return Ok(parsed);
            }
            if fallback.is_none() && parsed.has_manifest_identity() {
                fallback = Some(parsed);
            }
        }
    }

    fallback.map_or(direct, Ok)
}

// =============================================================================
// RemoteIndexer
// =============================================================================

fn effective_ssh_command_timeout(requested: Duration, configured_secs: u64) -> Duration {
    let configured = if configured_secs == 0 {
        requested
    } else {
        Duration::from_secs(configured_secs)
    };
    let effective = requested.min(configured);
    if effective.is_zero() {
        Duration::from_secs(1)
    } else {
        effective
    }
}

fn wait_for_command_output_with_timeout(
    child: Child,
    timeout: Duration,
) -> Result<Output, IndexError> {
    let timeout_secs = timeout.as_secs().max(1);
    wait_for_child_output_with_timeout(child, timeout)?.ok_or(IndexError::Timeout(timeout_secs))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteCassPresence {
    Found,
    NotFound,
    Unknown,
}

fn parse_remote_cass_presence(output: &str) -> RemoteCassPresence {
    let mut found = false;
    let mut not_found = false;

    for line in output.lines().map(str::trim) {
        match line {
            "CASS_FOUND" => found = true,
            "CASS_NOT_FOUND" => not_found = true,
            _ => {}
        }
    }

    match (found, not_found) {
        (true, false) => RemoteCassPresence::Found,
        (false, true) => RemoteCassPresence::NotFound,
        _ => RemoteCassPresence::Unknown,
    }
}

fn summarize_remote_output(output: &str) -> String {
    const MAX_REMOTE_OUTPUT_ERROR_CHARS: usize = 512;
    let summary: String = output
        .chars()
        .take(MAX_REMOTE_OUTPUT_ERROR_CHARS)
        .collect::<String>()
        .trim()
        .to_string();
    if output.chars().count() > MAX_REMOTE_OUTPUT_ERROR_CHARS {
        format!("{summary}...")
    } else {
        summary
    }
}

fn poll_status(output: &str) -> Option<&str> {
    output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("STATUS="))
        .next_back()
}

/// Indexer for triggering cass index on remote machines.
pub struct RemoteIndexer {
    /// SSH host alias.
    host: String,
    /// SSH timeout in seconds.
    ssh_timeout: u64,
}

impl RemoteIndexer {
    /// Create a new indexer for a remote host.
    pub fn new(host: impl Into<String>, ssh_timeout: u64) -> Self {
        Self {
            host: host.into(),
            ssh_timeout,
        }
    }

    /// Create an indexer with default timeout.
    pub fn with_defaults(host: impl Into<String>) -> Self {
        Self::new(host, DEFAULT_INDEX_TIMEOUT_SECS)
    }

    /// Get the host name.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Check if indexing is needed based on probe result.
    ///
    /// Returns true if the remote should be indexed:
    /// - cass installed but never indexed
    /// - Index exists but has zero sessions
    ///
    /// Returns false if:
    /// - cass not found (can't index without cass)
    /// - Already has indexed sessions
    pub fn needs_indexing(probe: &HostProbeResult) -> bool {
        match &probe.cass_status {
            // Not found - can't index without cass installed
            CassStatus::NotFound => false,
            // Explicitly not indexed - needs indexing
            CassStatus::InstalledNotIndexed { .. } => true,
            // Indexed but empty - try indexing again
            CassStatus::Indexed { session_count, .. } => *session_count == 0,
            // Unknown status - assume we should try
            CassStatus::Unknown => true,
        }
    }

    /// Run indexing on the remote host.
    ///
    /// Streams progress updates via the callback as indexing proceeds.
    /// For hosts with large session histories (100k+), uses background
    /// execution with polling to avoid SSH timeout.
    pub fn run_index<F>(&self, on_progress: F) -> Result<IndexResult, IndexError>
    where
        F: Fn(IndexProgress) + Send + Sync,
    {
        let start = Instant::now();

        on_progress(IndexProgress {
            stage: IndexStage::Starting,
            message: format!("Starting index on {}...", self.host),
            sessions_found: 0,
            sessions_indexed: 0,
            percent: Some(0),
            elapsed: start.elapsed(),
        });

        // First check if cass is available
        self.verify_cass_installed()?;
        self.verify_remote_host_pressure()?;

        // Run indexing in background with log file for progress tracking
        let mut result = self.run_index_with_polling(&on_progress, start)?;
        if result.success {
            result.artifact_manifest = Some(self.write_remote_artifact_manifest());
        }

        // Report final result
        if result.success {
            on_progress(IndexProgress {
                stage: IndexStage::Complete,
                message: format!(
                    "Indexed {} sessions on {} ({:.1}s)",
                    result.sessions_indexed,
                    self.host,
                    result.duration.as_secs_f64()
                ),
                sessions_found: result.sessions_indexed,
                sessions_indexed: result.sessions_indexed,
                percent: Some(100),
                elapsed: start.elapsed(),
            });
        } else {
            on_progress(IndexProgress {
                stage: IndexStage::Failed {
                    error: result.error.clone().unwrap_or_default(),
                },
                message: result
                    .error
                    .clone()
                    .unwrap_or_else(|| "Unknown error".into()),
                sessions_found: 0,
                sessions_indexed: 0,
                percent: None,
                elapsed: start.elapsed(),
            });
        }

        Ok(result)
    }

    /// Verify cass is installed on the remote.
    fn verify_cass_installed(&self) -> Result<(), IndexError> {
        let script = r#"
source ~/.cargo/env 2>/dev/null || true
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
command -v cass >/dev/null 2>&1 && echo "CASS_FOUND" || echo "CASS_NOT_FOUND"
"#;

        let output = self.run_ssh_command(script, Duration::from_secs(30))?;

        match parse_remote_cass_presence(&output) {
            RemoteCassPresence::Found => Ok(()),
            RemoteCassPresence::NotFound => Err(IndexError::CassNotFound),
            RemoteCassPresence::Unknown => Err(IndexError::SshFailed(format!(
                "Unexpected cass availability probe output: {}",
                summarize_remote_output(&output)
            ))),
        }
    }

    fn host_pressure_script() -> &'static str {
        r#"
CPUS=$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || echo "")
LOAD1=$(awk '{print $1}' /proc/loadavg 2>/dev/null || echo "")
MEM_AVAILABLE_KIB=$(awk '/MemAvailable:/ {print $2}' /proc/meminfo 2>/dev/null || echo "")
printf 'CPUS=%s\n' "$CPUS"
printf 'LOAD1=%s\n' "$LOAD1"
printf 'MEM_AVAILABLE_KIB=%s\n' "$MEM_AVAILABLE_KIB"
"#
    }

    fn verify_remote_host_pressure(&self) -> Result<(), IndexError> {
        let output = self.run_ssh_command(Self::host_pressure_script(), Duration::from_secs(15))?;
        let decision = RemoteHostPressureSnapshot::from_command_output(&output).decide();
        if decision.defer_index {
            Err(IndexError::HostPressure(decision.reason))
        } else {
            Ok(())
        }
    }

    fn artifact_manifest_script() -> &'static str {
        r#"
source ~/.cargo/env 2>/dev/null || true
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
cass sources artifact-manifest --write --json
"#
    }

    fn write_remote_artifact_manifest(&self) -> RemoteArtifactManifestResult {
        match self.run_ssh_command(Self::artifact_manifest_script(), Duration::from_secs(60)) {
            Ok(output) => RemoteArtifactManifestResult::from_command_output(&output),
            Err(err) => RemoteArtifactManifestResult::from_error(err.to_string()),
        }
    }

    /// Run indexing with background execution and polling.
    ///
    /// This approach prevents SSH timeout for large indexes:
    /// 1. Start `cass index` in background with nohup, logging to file
    /// 2. Poll log file for progress and completion
    fn run_index_with_polling<F>(
        &self,
        on_progress: &F,
        start: Instant,
    ) -> Result<IndexResult, IndexError>
    where
        F: Fn(IndexProgress),
    {
        // Start indexing in background
        let start_script = r#"
source ~/.cargo/env 2>/dev/null || true
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"

LOG_FILE=~/.cass_index.log
rm -f "$LOG_FILE"

nohup bash -c '
set -o pipefail
source "$HOME/.cargo/env" 2>/dev/null || true
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
cass index --progress 2>&1 | tee "$HOME/.cass_index.log"
STATUS=${PIPESTATUS[0]}
if [ "$STATUS" -eq 0 ]; then
    echo "===INDEX_COMPLETE===" >> "$HOME/.cass_index.log"
else
    echo "===INDEX_FAILED:${STATUS}===" >> "$HOME/.cass_index.log"
fi
' > /dev/null 2>&1 &

echo "INDEX_PID=$!"
"#;

        let output = self.run_ssh_command(start_script, Duration::from_secs(30))?;

        // Extract PID (for potential future use)
        let _pid = output
            .lines()
            .find(|l| l.starts_with("INDEX_PID="))
            .and_then(|l| l.strip_prefix("INDEX_PID="))
            .and_then(|p| p.trim().parse::<u32>().ok());

        // Poll for progress and completion
        self.poll_index_progress(on_progress, start)
    }

    /// Poll the remote log file for indexing progress.
    fn poll_index_progress<F>(
        &self,
        on_progress: &F,
        start: Instant,
    ) -> Result<IndexResult, IndexError>
    where
        F: Fn(IndexProgress),
    {
        let poll_script = r#"
LOG_FILE=~/.cass_index.log
if [ -f "$LOG_FILE" ]; then
    if grep -q "===INDEX_FAILED:" "$LOG_FILE"; then
        echo "STATUS=ERROR"
        tail -30 "$LOG_FILE"
    elif grep -q "===INDEX_COMPLETE===" "$LOG_FILE"; then
        echo "STATUS=COMPLETE"
        # Get session count from health
        source ~/.cargo/env 2>/dev/null || true
        export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
        STATS=$(cass stats --json 2>/dev/null || echo '{}')
        SESSIONS=$(echo "$STATS" | tr -d '\n' | sed -n 's/.*"conversations"[[:space:]]*:[[:space:]]*\\([0-9][0-9]*\\).*/\\1/p')
        echo "SESSIONS=${SESSIONS:-0}"
    elif grep -qi "error" "$LOG_FILE" && ! grep -q "===INDEX_COMPLETE===" "$LOG_FILE"; then
        # Check if it's a real error or just log noise
        if grep -qE "(FATAL|panicked|No such file|Permission denied|disk full)" "$LOG_FILE"; then
            echo "STATUS=ERROR"
            tail -30 "$LOG_FILE"
        else
            echo "STATUS=RUNNING"
            tail -10 "$LOG_FILE" | grep -E "(Scanning|Building|Indexed|Processing)" | tail -3
        fi
    else
        echo "STATUS=RUNNING"
        tail -10 "$LOG_FILE" | grep -E "(Scanning|Building|Indexed|Processing)" | tail -3
    fi
else
    echo "STATUS=NOT_STARTED"
fi
"#;

        let max_wait = Duration::from_secs(MAX_INDEX_WAIT_SECS);
        let poll_interval = Duration::from_secs(INDEX_POLL_INTERVAL_SECS);
        let mut sessions_found: u64 = 0;
        let mut last_agent = String::new();
        let mut progress_pct: u8 = 5;

        loop {
            if start.elapsed() > max_wait {
                return Err(IndexError::Timeout(max_wait.as_secs()));
            }

            std::thread::sleep(poll_interval);

            let output = self.run_ssh_command(poll_script, Duration::from_secs(30))?;
            // Track if we've seen Building this poll cycle (avoid multiple increments per poll)
            let mut saw_building_this_poll = false;

            if poll_status(&output) == Some("COMPLETE") {
                // Extract session count
                let sessions = output
                    .lines()
                    .find(|l| l.starts_with("SESSIONS="))
                    .and_then(|l| l.strip_prefix("SESSIONS="))
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .unwrap_or(0);

                return Ok(IndexResult {
                    success: true,
                    sessions_indexed: sessions,
                    duration: start.elapsed(),
                    error: None,
                    artifact_manifest: None,
                });
            }

            if poll_status(&output) == Some("ERROR") {
                let error_lines: Vec<&str> = output
                    .lines()
                    .filter(|l| !l.trim_start().starts_with("STATUS="))
                    .collect();
                let error_msg = error_lines.join("\n");

                // Detect specific errors
                if error_msg.contains("disk full") || error_msg.contains("No space left") {
                    return Err(IndexError::DiskFull);
                }
                if error_msg.contains("Permission denied") {
                    return Err(IndexError::PermissionDenied);
                }

                return Ok(IndexResult {
                    success: false,
                    sessions_indexed: 0,
                    duration: start.elapsed(),
                    error: Some(error_msg),
                    artifact_manifest: None,
                });
            }

            // Parse progress from output
            for line in output.lines() {
                // Look for scanning progress
                if line.contains("Scanning")
                    && let Some(agent) = extract_agent_from_line(line)
                    && agent != last_agent
                {
                    progress_pct = (progress_pct + 5).min(40);
                    on_progress(IndexProgress {
                        stage: IndexStage::Scanning {
                            agent: agent.clone(),
                        },
                        message: format!("Scanning {}...", agent),
                        sessions_found,
                        sessions_indexed: 0,
                        percent: Some(progress_pct),
                        elapsed: start.elapsed(),
                    });
                    last_agent = agent;
                }

                // Look for session count updates
                if let Some(count) = extract_session_count(line) {
                    sessions_found = count;
                }

                // Look for building phase (only report once per poll to avoid racing progress)
                if !saw_building_this_poll
                    && (line.contains("Building") || line.contains("Indexing"))
                {
                    saw_building_this_poll = true;
                    progress_pct = (progress_pct + 5).min(85);
                    on_progress(IndexProgress {
                        stage: IndexStage::Building,
                        message: "Building search index...".into(),
                        sessions_found,
                        sessions_indexed: 0,
                        percent: Some(progress_pct),
                        elapsed: start.elapsed(),
                    });
                }
            }
        }
    }

    /// Run an SSH command on the remote host.
    fn run_ssh_command(&self, script: &str, timeout: Duration) -> Result<String, IndexError> {
        let command_timeout = effective_ssh_command_timeout(timeout, self.ssh_timeout);
        let connect_timeout_secs = command_timeout.as_secs().clamp(1, 30);

        let mut cmd = Command::new("ssh");
        cmd.args(strict_ssh_cli_tokens(connect_timeout_secs))
            .arg("-o")
            .arg("LogLevel=ERROR")
            .arg("--")
            .arg(&self.host)
            .arg("bash")
            .arg("-s");

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_child_process_group(&mut cmd);

        let mut child = cmd.spawn()?;

        let write_error = if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(script.as_bytes()).err()
        } else {
            None
        };

        let output = wait_for_command_output_with_timeout(child, command_timeout)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if is_host_key_verification_failure(&stderr) {
                return Err(IndexError::SshFailed(host_key_verification_error(
                    &self.host,
                )));
            }
            if stderr.contains("Connection refused")
                || stderr.contains("Connection timed out")
                || stderr.contains("Permission denied")
            {
                return Err(IndexError::SshFailed(stderr.trim().to_string()));
            }
            // Fail fast on any other non-zero exit — surface the exit code and
            // stderr so operators can diagnose the root cause immediately.
            let code = output.status.code().unwrap_or(-1);
            return Err(IndexError::SshFailed(format!(
                "Remote script exited with code {code}: {}",
                stderr.trim()
            )));
        }
        if let Some(err) = write_error {
            return Err(IndexError::Io(err));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

/// Extract agent name from a scanning log line.
fn extract_agent_from_line(line: &str) -> Option<String> {
    // Match patterns like "Scanning ~/.claude/projects" or "Scanning claude_code"
    if let Some(idx) = line.find("Scanning") {
        let rest = &line[idx + 8..].trim();
        // Extract first word or path segment, stripping leading dots from hidden dirs
        let agent = rest
            .split(|c: char| c.is_whitespace() || c == '/')
            .filter(|s| !s.is_empty() && *s != "~" && *s != ".")
            .map(|s| s.trim_start_matches('.'))
            .find(|s| !s.is_empty())?;

        // Map path components to agent names
        let agent_name = match agent {
            "claude" => "claude_code",
            "codex" => "codex",
            "cursor" => "cursor",
            "gemini" => "gemini",
            "aider" => "aider",
            "goose" => "goose",
            "continue" => "continue",
            _ => agent,
        };

        return Some(agent_name.to_string());
    }
    None
}

/// Extract session count from a log line.
fn extract_session_count(line: &str) -> Option<u64> {
    // Match patterns like "found 234 sessions" or "Indexed 291 sessions"
    // Avoid picking unrelated numbers (timestamps, IDs) by anchoring near
    // session/conversation keywords.
    let lower = line.to_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();

    for (idx, token) in tokens.iter().enumerate() {
        let word = token.trim_matches(|c: char| !c.is_ascii_alphabetic());
        if matches!(
            word,
            "session" | "sessions" | "conversation" | "conversations"
        ) {
            if idx > 0
                && let Some(count) = parse_count(tokens[idx - 1])
            {
                return Some(count);
            }
            if idx + 1 < tokens.len()
                && let Some(count) = parse_count(tokens[idx + 1])
            {
                return Some(count);
            }
        }
    }

    None
}

fn parse_count(token: &str) -> Option<u64> {
    let trimmed = token.trim_matches(|c: char| !c.is_ascii_digit() && c != '/');
    let candidate = trimmed.split('/').next().unwrap_or(trimmed);
    let digits: String = candidate.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse::<u64>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::probe::HostProbeResult;
    use std::path::PathBuf;

    /// Load a probe fixture from the tests/fixtures/sources/probe directory.
    fn load_probe_fixture(name: &str) -> HostProbeResult {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/sources/probe")
            .join(format!("{}.json", name));
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read fixture {}: {}", path.display(), e));
        serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("Failed to parse fixture {}: {}", path.display(), e))
    }

    #[test]
    fn test_no_indexing_when_not_found() {
        // Can't index if cass isn't installed
        let probe = load_probe_fixture("no_cass_host");
        assert!(!RemoteIndexer::needs_indexing(&probe));
    }

    #[test]
    fn test_needs_indexing_when_not_indexed() {
        let probe = load_probe_fixture("not_indexed_host");
        assert!(RemoteIndexer::needs_indexing(&probe));
    }

    #[test]
    fn test_needs_indexing_when_empty_index() {
        let probe = load_probe_fixture("empty_index_host");
        assert!(RemoteIndexer::needs_indexing(&probe));
    }

    #[test]
    fn test_no_indexing_needed_when_has_sessions() {
        let probe = load_probe_fixture("indexed_host");
        assert!(!RemoteIndexer::needs_indexing(&probe));
    }

    #[test]
    fn test_needs_indexing_when_unknown() {
        let probe = load_probe_fixture("unknown_status_host");
        assert!(RemoteIndexer::needs_indexing(&probe));
    }

    #[test]
    fn test_extract_agent_from_line() {
        assert_eq!(
            extract_agent_from_line("Scanning ~/.claude/projects..."),
            Some("claude_code".into())
        );
        assert_eq!(
            extract_agent_from_line("Scanning ~/.codex/sessions..."),
            Some("codex".into())
        );
        assert_eq!(
            extract_agent_from_line("Scanning cursor data..."),
            Some("cursor".into())
        );
        assert_eq!(extract_agent_from_line("Some other line"), None);
    }

    #[test]
    fn test_extract_session_count() {
        assert_eq!(extract_session_count("found 234 sessions"), Some(234));
        assert_eq!(extract_session_count("Indexed 291 sessions"), Some(291));
        assert_eq!(
            extract_session_count("Processing 42 conversations"),
            Some(42)
        );
        assert_eq!(
            extract_session_count("2026-01-11 12:00:00 Indexed 291 sessions"),
            Some(291)
        );
        assert_eq!(extract_session_count("Indexed 5/10 conversations"), Some(5));
        assert_eq!(extract_session_count("conversations: 17 total"), Some(17));
        assert_eq!(extract_session_count("Some other line"), None);
    }

    #[test]
    fn test_index_stage_display() {
        assert_eq!(IndexStage::Starting.to_string(), "Starting");
        assert_eq!(
            IndexStage::Scanning {
                agent: "claude_code".into()
            }
            .to_string(),
            "Scanning claude_code"
        );
        assert_eq!(IndexStage::Building.to_string(), "Building index");
        assert_eq!(IndexStage::Complete.to_string(), "Complete");
    }

    #[test]
    fn test_index_error_help_messages() {
        assert!(IndexError::DiskFull.help_message().contains("Free disk"));
        assert!(IndexError::Timeout(600).help_message().contains("manually"));
        assert!(
            IndexError::PermissionDenied
                .help_message()
                .contains("permissions")
        );
        assert!(
            IndexError::CassNotFound
                .help_message()
                .contains("installed")
        );
        assert!(
            IndexError::HostPressure("load".into())
                .help_message()
                .contains("busy")
        );
    }

    #[test]
    fn test_remote_indexer_new() {
        let indexer = RemoteIndexer::new("laptop", 300);
        assert_eq!(indexer.host(), "laptop");

        let indexer2 = RemoteIndexer::with_defaults("server");
        assert_eq!(indexer2.host(), "server");
    }

    #[test]
    fn test_effective_ssh_command_timeout_clamps_to_smaller_deadline() {
        assert_eq!(
            effective_ssh_command_timeout(Duration::from_secs(60), 10),
            Duration::from_secs(10)
        );
        assert_eq!(
            effective_ssh_command_timeout(Duration::from_secs(15), 60),
            Duration::from_secs(15)
        );
        assert_eq!(
            effective_ssh_command_timeout(Duration::from_secs(15), 0),
            Duration::from_secs(15)
        );
        assert_eq!(
            effective_ssh_command_timeout(Duration::ZERO, 0),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn test_parse_remote_cass_presence_requires_unambiguous_status_line() {
        assert_eq!(
            parse_remote_cass_presence("Welcome to host\nCASS_FOUND\n"),
            RemoteCassPresence::Found
        );
        assert_eq!(
            parse_remote_cass_presence("CASS_NOT_FOUND\n"),
            RemoteCassPresence::NotFound
        );
        assert_eq!(
            parse_remote_cass_presence("Welcome to host\n"),
            RemoteCassPresence::Unknown
        );
        assert_eq!(
            parse_remote_cass_presence("CASS_FOUND\nCASS_NOT_FOUND\n"),
            RemoteCassPresence::Unknown
        );
    }

    #[test]
    fn test_poll_status_uses_exact_status_line() {
        assert_eq!(
            poll_status("banner mentions STATUS=ERROR in prose\nSTATUS=COMPLETE\nSESSIONS=7\n"),
            Some("COMPLETE")
        );
        assert_eq!(
            poll_status("STATUS=ERROR\nstartup banner\nSTATUS=COMPLETE\nSESSIONS=7\n"),
            Some("COMPLETE")
        );
        assert_eq!(poll_status("  STATUS=ERROR\npanic\n"), Some("ERROR"));
        assert_eq!(poll_status("no structured status"), None);
    }

    #[cfg(unix)]
    #[test]
    fn test_wait_for_command_output_with_timeout_kills_stalled_child() -> anyhow::Result<()> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 2")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_child_process_group(&mut cmd);
        let child = cmd.spawn()?;

        let started = Instant::now();
        let Err(err) = wait_for_command_output_with_timeout(child, Duration::from_millis(50))
        else {
            return Err(anyhow::anyhow!("stalled command should time out"));
        };
        anyhow::ensure!(
            matches!(err, IndexError::Timeout(1)),
            "unexpected timeout error: {err}"
        );
        anyhow::ensure!(
            started.elapsed() < Duration::from_secs(1),
            "timeout helper waited too long for stalled child"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_wait_for_command_output_with_timeout_drains_large_output() -> anyhow::Result<()> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("yes stdout | head -c 200000; yes stderr | head -c 200000 >&2")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_child_process_group(&mut cmd);
        let child = cmd.spawn()?;

        let output = wait_for_command_output_with_timeout(child, Duration::from_secs(5))?;
        anyhow::ensure!(output.status.success(), "large-output command failed");
        anyhow::ensure!(
            output.stdout.len() == 200_000,
            "stdout was not fully drained"
        );
        anyhow::ensure!(
            output.stderr.len() == 200_000,
            "stderr was not fully drained"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_wait_for_command_output_with_timeout_bounds_inherited_pipe_waits() -> anyhow::Result<()>
    {
        let temp = tempfile::TempDir::new()?;
        let pid_file = temp.path().join("grandchild.pid");
        let mut cmd = Command::new("sh");
        cmd.env("PID_FILE", &pid_file);
        cmd.arg("-c")
            .arg("(sleep 30) & printf '%s\\n' \"$!\" > \"$PID_FILE\"; printf parent-done")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_child_process_group(&mut cmd);
        let child = cmd.spawn()?;

        let started = Instant::now();
        let Err(err) = wait_for_command_output_with_timeout(child, Duration::from_millis(100))
        else {
            return Err(anyhow::anyhow!(
                "inherited pipe should not outlive command deadline"
            ));
        };
        anyhow::ensure!(
            matches!(err, IndexError::Timeout(1)),
            "unexpected inherited-pipe timeout error: {err}"
        );
        anyhow::ensure!(
            started.elapsed() < Duration::from_secs(1),
            "timeout helper waited too long for inherited pipe"
        );

        let grandchild_pid = std::fs::read_to_string(&pid_file)?;
        anyhow::ensure!(
            wait_until_unix_process_stops(grandchild_pid.trim(), Duration::from_secs(1)),
            "timeout must kill inherited-pipe grandchild process {}",
            grandchild_pid.trim()
        );
        Ok(())
    }

    #[cfg(unix)]
    fn wait_until_unix_process_stops(pid: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !unix_process_is_running(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        !unix_process_is_running(pid)
    }

    #[cfg(unix)]
    fn unix_process_is_running(pid: &str) -> bool {
        let Ok(output) = Command::new("ps").args(["-o", "stat=", "-p", pid]).output() else {
            return true;
        };
        if !output.status.success() {
            return false;
        }
        let stat = String::from_utf8_lossy(&output.stdout);
        let stat = stat.trim();
        !stat.is_empty() && !stat.starts_with('Z')
    }

    #[test]
    fn test_artifact_manifest_script_uses_robot_safe_write_command() {
        let script = RemoteIndexer::artifact_manifest_script();
        assert!(script.contains("cass sources artifact-manifest --write --json"));
        assert!(!script.contains("cass sources artifact-manifest --write\n"));
    }

    #[test]
    fn test_host_pressure_script_reads_cheap_linux_metrics() {
        let script = RemoteIndexer::host_pressure_script();
        assert!(script.contains("_NPROCESSORS_ONLN"));
        assert!(script.contains("/proc/loadavg"));
        assert!(script.contains("MemAvailable"));
    }

    #[test]
    fn test_remote_host_pressure_allows_incomplete_metrics() {
        let decision = RemoteHostPressureSnapshot::from_command_output("CPUS=\nLOAD1=\n").decide();

        assert!(!decision.defer_index);
        assert!(
            decision.reason.contains("metrics incomplete"),
            "{decision:?}"
        );
    }

    #[test]
    fn test_remote_host_pressure_defers_high_load() {
        let decision = RemoteHostPressureSnapshot::from_command_output(
            "CPUS=4\nLOAD1=7.20\nMEM_AVAILABLE_KIB=1048576\n",
        )
        .decide();

        assert!(decision.defer_index);
        assert!(decision.reason.contains("load_per_cpu"), "{decision:?}");
    }

    #[test]
    fn test_remote_host_pressure_defers_low_memory() {
        let decision = RemoteHostPressureSnapshot::from_command_output(
            "CPUS=64\nLOAD1=12.00\nMEM_AVAILABLE_KIB=131072\n",
        )
        .decide();

        assert!(decision.defer_index);
        assert!(
            decision.reason.contains("mem_available_kib"),
            "{decision:?}"
        );
    }

    #[test]
    fn test_remote_artifact_manifest_result_parses_command_output() {
        let result = RemoteArtifactManifestResult::from_command_output(
            r#"{
              "status": "ok",
              "manifest_path": "/home/user/.local/share/cass/index/v1/evidence-bundle-manifest.json",
              "bundle_id": "cass-lexical-abc",
              "chunk_count": 3,
              "expected_bytes": 42,
              "verification_status": "complete"
            }"#,
        );

        assert!(result.success);
        assert_eq!(result.bundle_id.as_deref(), Some("cass-lexical-abc"));
        assert_eq!(result.chunk_count, Some(3));
        assert_eq!(result.expected_bytes, Some(42));
        assert_eq!(result.error, None);
    }

    #[test]
    fn test_remote_artifact_manifest_result_parses_json_with_ssh_noise() {
        let result = RemoteArtifactManifestResult::from_command_output(
            r#"
Welcome to remote host {}
MOTD: maintenance starts at 02:00
{
  "status": "ok",
  "manifest_path": "/home/user/.local/share/cass/index/v1/evidence-bundle-manifest.json",
  "bundle_id": "cass-lexical-noisy",
  "chunk_count": 2,
  "expected_bytes": 99,
  "verification_status": "complete"
}
"#,
        );

        assert!(result.success);
        assert_eq!(result.bundle_id.as_deref(), Some("cass-lexical-noisy"));
        assert_eq!(result.chunk_count, Some(2));
        assert_eq!(result.expected_bytes, Some(99));
        assert_eq!(result.error, None);
    }

    #[test]
    fn test_remote_artifact_manifest_result_skips_partial_noise_json() {
        let result = RemoteArtifactManifestResult::from_command_output(
            r#"
Welcome to remote host
{"verification_status": "complete"}
{
  "status": "ok",
  "manifest_path": "/home/user/.local/share/cass/index/v1/evidence-bundle-manifest.json",
  "bundle_id": "cass-lexical-real",
  "chunk_count": 4,
  "expected_bytes": 123,
  "verification_status": "complete"
}
"#,
        );

        assert!(result.success);
        assert_eq!(result.bundle_id.as_deref(), Some("cass-lexical-real"));
        assert_eq!(result.chunk_count, Some(4));
        assert_eq!(result.expected_bytes, Some(123));
        assert_eq!(result.error, None);
    }
}
