//! E2E Logging utilities for structured JSONL output.
//!
//! This module provides helpers for Rust E2E tests to emit structured logs
//! following the unified schema defined in `test-results/e2e/SCHEMA.md`.
//!
//! # Usage
//!
//! ```ignore
//! use crate::util::e2e_log::{E2eLogger, E2eTestInfo};
//!
//! let logger = E2eLogger::new("rust")?;
//! logger.run_start()?;
//!
//! let test_info = E2eTestInfo::new("test_pages_export", "e2e_pages", file!(), line!());
//! logger.test_start(&test_info)?;
//!
//! // ... run test ...
//!
//! logger.test_end(&test_info, "pass", duration_ms, None)?;
//! logger.run_end(total, passed, failed, skipped, duration_ms)?;
//! ```

use super::EnvGuard;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Environment metadata captured at the start of a test run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct E2eEnvironment {
    pub git_sha: Option<String>,
    pub git_branch: Option<String>,
    pub os: String,
    pub arch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rust_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cass_version: Option<String>,
    pub ci: bool,
}

impl E2eEnvironment {
    /// Capture current environment metadata.
    pub fn capture() -> Self {
        Self {
            git_sha: Self::git_sha(),
            git_branch: Self::git_branch(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            rust_version: Self::rust_version(),
            node_version: Self::node_version(),
            cass_version: Self::cass_version(),
            ci: std::env::var("CI").is_ok() || std::env::var("GITHUB_ACTIONS").is_ok(),
        }
    }

    fn git_sha() -> Option<String> {
        Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    fn git_branch() -> Option<String> {
        Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    fn rust_version() -> Option<String> {
        Command::new("rustc")
            .args(["--version"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                let full = String::from_utf8_lossy(&o.stdout);
                // "rustc 1.84.0 (abc123 2025-01-01)" -> "1.84.0"
                full.split_whitespace().nth(1).unwrap_or(&full).to_string()
            })
    }

    fn node_version() -> Option<String> {
        Command::new("node")
            .args(["--version"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    fn cass_version() -> Option<String> {
        // Try to get from Cargo.toml or built binary
        std::env::var("CARGO_PKG_VERSION").ok()
    }
}

/// Test information for logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct E2eTestInfo {
    pub name: String,
    pub suite: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_paths: Option<E2eArtifactManifest>,
}

impl E2eTestInfo {
    pub fn new(name: &str, suite: &str, file: &str, line: u32) -> Self {
        Self {
            name: name.to_string(),
            suite: suite.to_string(),
            test_id: Some(format!("{suite}::{name}")),
            file: Some(file.to_string()),
            line: Some(line),
            trace_id: None,
            artifact_paths: None,
        }
    }

    pub fn simple(name: &str, suite: &str) -> Self {
        Self {
            name: name.to_string(),
            suite: suite.to_string(),
            test_id: Some(format!("{suite}::{name}")),
            file: None,
            line: None,
            trace_id: None,
            artifact_paths: None,
        }
    }
}

/// Test result for test_end events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct E2eTestResult {
    pub status: String,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    /// Performance metrics captured during test execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<E2ePerformanceMetrics>,
}

/// Error information for failed tests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct E2eError {
    pub message: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<E2eErrorContext>,
}

/// Additional context captured at the point of failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct E2eErrorContext {
    /// Relevant state values at failure point (key-value pairs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<std::collections::HashMap<String, serde_json::Value>>,
    /// Path to screenshot file (for browser tests)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot_path: Option<String>,
    /// Sanitized environment variables (sensitive values redacted)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_vars: Option<std::collections::HashMap<String, String>>,
    /// Current working directory
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Command that was being executed (if applicable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Stdout from failed command
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    /// Stderr from failed command
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
}

impl E2eError {
    /// Create a basic error with just a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error_type: None,
            stack: None,
            context: None,
        }
    }

    /// Create an error with type information.
    pub fn with_type(message: impl Into<String>, error_type: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error_type: Some(error_type.into()),
            stack: None,
            context: None,
        }
    }

    /// Add a stack trace to the error.
    pub fn with_stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = Some(stack.into());
        self
    }

    /// Add context to the error.
    pub fn with_context(mut self, context: E2eErrorContext) -> Self {
        self.context = Some(context);
        self
    }
}

impl E2eErrorContext {
    /// Create an empty error context.
    pub fn new() -> Self {
        Self {
            state: None,
            screenshot_path: None,
            env_vars: None,
            cwd: None,
            command: None,
            stdout: None,
            stderr: None,
        }
    }

    /// Add state values to the context.
    pub fn with_state(
        mut self,
        state: std::collections::HashMap<String, serde_json::Value>,
    ) -> Self {
        self.state = Some(state);
        self
    }

    /// Add a single state value.
    pub fn add_state(
        mut self,
        key: impl Into<String>,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        let state = self
            .state
            .get_or_insert_with(std::collections::HashMap::new);
        state.insert(key.into(), value.into());
        self
    }

    /// Add screenshot path.
    pub fn with_screenshot(mut self, path: impl Into<String>) -> Self {
        self.screenshot_path = Some(path.into());
        self
    }

    /// Capture and sanitize current environment variables.
    pub fn capture_env(mut self) -> Self {
        self.env_vars = Some(capture_sanitized_env());
        self
    }

    /// Add specific environment variables (sanitized).
    pub fn with_env(mut self, env: std::collections::HashMap<String, String>) -> Self {
        self.env_vars = Some(env);
        self
    }

    /// Capture current working directory.
    pub fn capture_cwd(mut self) -> Self {
        if let Ok(cwd) = std::env::current_dir() {
            self.cwd = Some(cwd.display().to_string());
        }
        self
    }

    /// Add command information.
    pub fn with_command(mut self, cmd: impl Into<String>) -> Self {
        self.command = Some(cmd.into());
        self
    }

    /// Add command output.
    pub fn with_output(mut self, stdout: Option<String>, stderr: Option<String>) -> Self {
        self.stdout = stdout;
        self.stderr = stderr;
        self
    }
}

impl Default for E2eErrorContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Capture environment variables with sensitive values redacted.
///
/// Sensitive keys are detected by patterns like:
/// - *_KEY, *_SECRET, *_TOKEN, *_PASSWORD, *_CREDENTIAL
/// - API_*, AUTH_*, AWS_*, GITHUB_TOKEN, etc.
pub fn capture_sanitized_env() -> std::collections::HashMap<String, String> {
    let sensitive_patterns = [
        "_KEY",
        "_SECRET",
        "_TOKEN",
        "_PASSWORD",
        "_CREDENTIAL",
        "_PASS",
        "API_",
        "AUTH_",
        "AWS_",
        "PRIVATE",
        "ENCRYPTION",
    ];
    let sensitive_exact = [
        "GITHUB_TOKEN",
        "NPM_TOKEN",
        "CARGO_REGISTRY_TOKEN",
        "DATABASE_URL",
        "REDIS_URL",
        "MONGODB_URI",
    ];

    std::env::vars()
        .filter(|(k, _)| {
            // Only include relevant env vars
            k.starts_with("RUST_")
                || k.starts_with("CARGO_")
                || k.starts_with("CI")
                || k.starts_with("GITHUB_")
                || k.starts_with("E2E_")
                || k.starts_with("TEST_")
                || k == "HOME"
                || k == "PATH"
                || k == "USER"
                || k == "SHELL"
                || k == "TERM"
        })
        .map(|(k, v)| {
            let is_sensitive = sensitive_exact.contains(&k.as_str())
                || sensitive_patterns.iter().any(|p| k.contains(p));

            if is_sensitive {
                (k, "[REDACTED]".to_string())
            } else {
                (k, v)
            }
        })
        .collect()
}

/// Run summary for run_end events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct E2eRunSummary {
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flaky: Option<u32>,
    pub duration_ms: u64,
}

/// Phase information for phase_start/phase_end events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct E2ePhase {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Performance metrics for tests.
///
/// Captures various performance indicators that can be analyzed post-run.
/// All fields are optional to allow incremental metric capture.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct E2ePerformanceMetrics {
    /// Test/phase duration in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Memory usage in bytes (resident set size).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// Peak memory usage in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_memory_bytes: Option<u64>,
    /// Number of file read operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_reads: Option<u64>,
    /// Number of file write operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_writes: Option<u64>,
    /// Bytes read from disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_read: Option<u64>,
    /// Bytes written to disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_written: Option<u64>,
    /// Throughput in items per second (e.g., messages indexed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub throughput_per_sec: Option<f64>,
    /// Number of items processed (for throughput calculation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items_processed: Option<u64>,
    /// Network requests made (for browser tests).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_requests: Option<u64>,
    /// Custom metrics as key-value pairs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<std::collections::HashMap<String, serde_json::Value>>,
}

#[allow(dead_code)]
impl E2ePerformanceMetrics {
    /// Create empty metrics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set duration.
    pub fn with_duration(mut self, ms: u64) -> Self {
        self.duration_ms = Some(ms);
        self
    }

    /// Set memory usage.
    pub fn with_memory(mut self, bytes: u64) -> Self {
        self.memory_bytes = Some(bytes);
        self
    }

    /// Set peak memory usage.
    pub fn with_peak_memory(mut self, bytes: u64) -> Self {
        self.peak_memory_bytes = Some(bytes);
        self
    }

    /// Set throughput metrics.
    pub fn with_throughput(mut self, items: u64, duration_ms: u64) -> Self {
        self.items_processed = Some(items);
        self.duration_ms = Some(duration_ms);
        if duration_ms > 0 {
            self.throughput_per_sec = Some((items as f64) / (duration_ms as f64 / 1000.0));
        }
        self
    }

    /// Set I/O metrics.
    pub fn with_io(mut self, reads: u64, writes: u64, bytes_read: u64, bytes_written: u64) -> Self {
        self.file_reads = Some(reads);
        self.file_writes = Some(writes);
        self.bytes_read = Some(bytes_read);
        self.bytes_written = Some(bytes_written);
        self
    }

    /// Set network requests count.
    pub fn with_network(mut self, requests: u64) -> Self {
        self.network_requests = Some(requests);
        self
    }

    /// Add a custom metric.
    pub fn with_custom(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        if self.custom.is_none() {
            self.custom = Some(std::collections::HashMap::new());
        }
        if let Some(ref mut map) = self.custom {
            map.insert(key.to_string(), value.into());
        }
        self
    }

    /// Capture current process memory usage (best effort).
    ///
    /// Returns the resident set size in bytes on Linux.
    /// Returns None on other platforms or if reading fails.
    pub fn capture_memory() -> Option<u64> {
        // Try to read from /proc/self/statm on Linux
        #[cfg(target_os = "linux")]
        {
            if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
                // statm format: size resident shared text lib data dt (in pages)
                if let Some(resident) = statm.split_whitespace().nth(1)
                    && let Ok(pages) = resident.parse::<u64>()
                {
                    // Page size is typically 4096 bytes
                    return Some(pages * 4096);
                }
            }
        }
        None
    }

    /// Capture current process I/O statistics (best effort).
    ///
    /// Returns (bytes_read, bytes_written) on Linux.
    /// Returns None on other platforms or if reading fails.
    pub fn capture_io() -> Option<(u64, u64)> {
        #[cfg(target_os = "linux")]
        {
            if let Ok(io_content) = std::fs::read_to_string("/proc/self/io") {
                let mut read_bytes = 0u64;
                let mut write_bytes = 0u64;
                for line in io_content.lines() {
                    if let Some(val) = line.strip_prefix("read_bytes: ") {
                        read_bytes = val.trim().parse().unwrap_or(0);
                    } else if let Some(val) = line.strip_prefix("write_bytes: ") {
                        write_bytes = val.trim().parse().unwrap_or(0);
                    }
                }
                return Some((read_bytes, write_bytes));
            }
        }
        None
    }
}

/// Log context for log events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct E2eLogContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_name: Option<String>,
}

/// Paths for per-test E2E artifacts.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct E2eArtifactPaths {
    pub dir: PathBuf,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub cass_log_path: PathBuf,
    pub trace_path: PathBuf,
    pub trace_id: String,
}

/// Serializable artifact paths for JSONL logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct E2eArtifactManifest {
    pub dir: String,
    pub stdout: String,
    pub stderr: String,
    pub cass_log: String,
    pub trace: String,
}

impl E2eArtifactManifest {
    fn from_paths(paths: &E2eArtifactPaths) -> Self {
        Self {
            dir: paths.dir.to_string_lossy().to_string(),
            stdout: paths.stdout_path.to_string_lossy().to_string(),
            stderr: paths.stderr_path.to_string_lossy().to_string(),
            cass_log: paths.cass_log_path.to_string_lossy().to_string(),
            trace: paths.trace_path.to_string_lossy().to_string(),
        }
    }
}

impl E2eArtifactPaths {
    pub fn prepare(suite: &str, test_name: &str, trace_id: &str) -> std::io::Result<Self> {
        let dir = artifact_dir(suite, test_name);
        fs::create_dir_all(&dir)?;

        let stdout_path = dir.join("stdout");
        let stderr_path = dir.join("stderr");
        let cass_log_path = dir.join("cass.log");
        let trace_path = dir.join("trace.jsonl");

        // Ensure files exist (truncate any previous run output)
        truncate_file(&stdout_path)?;
        truncate_file(&stderr_path)?;
        truncate_file(&cass_log_path)?;
        truncate_file(&trace_path)?;

        Ok(Self {
            dir,
            stdout_path,
            stderr_path,
            cass_log_path,
            trace_path,
            trace_id: trace_id.to_string(),
        })
    }
}

fn artifact_dir(suite: &str, test_name: &str) -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    manifest_dir
        .join("test-results")
        .join("e2e")
        .join(suite)
        .join(test_name)
}

fn truncate_file(path: &Path) -> std::io::Result<()> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    Ok(())
}

/// Configuration for the E2E logger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct E2eConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fail_fast: Option<bool>,
}

/// Base event structure that all events share.
#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
struct BaseEvent {
    ts: String,
    event: String,
    run_id: String,
    runner: String,
}

/// E2E Logger that writes structured JSONL events.
#[allow(dead_code)]
pub struct E2eLogger {
    run_id: String,
    runner: String,
    output_path: PathBuf,
    writer: Arc<Mutex<BufWriter<File>>>,
    env: E2eEnvironment,
}

#[allow(dead_code)]
impl E2eLogger {
    /// Create a new E2E logger.
    ///
    /// # Arguments
    /// * `runner` - The runner type ("rust", "shell", or "playwright")
    ///
    /// # Returns
    /// A new logger that writes to `test-results/e2e/{runner}_{timestamp}.jsonl`
    pub fn new(runner: &str) -> std::io::Result<Self> {
        let timestamp = Self::timestamp_id();
        let run_id = format!("{}_{}", timestamp, Self::random_suffix());
        let output_dir = Self::output_dir()?;
        let output_path = output_dir.join(format!("{}_{}.jsonl", runner, timestamp));

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&output_path)?;
        let writer = Arc::new(Mutex::new(BufWriter::new(file)));

        Ok(Self {
            run_id,
            runner: runner.to_string(),
            output_path,
            writer,
            env: E2eEnvironment::capture(),
        })
    }

    /// Create a logger with a specific output path (for testing).
    pub fn with_path(runner: &str, output_path: PathBuf) -> std::io::Result<Self> {
        let timestamp = Self::timestamp_id();
        let run_id = format!("{}_{}", timestamp, Self::random_suffix());

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&output_path)?;
        let writer = Arc::new(Mutex::new(BufWriter::new(file)));

        Ok(Self {
            run_id,
            runner: runner.to_string(),
            output_path,
            writer,
            env: E2eEnvironment::capture(),
        })
    }

    /// Get the run ID for this logger.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Get the output path for this logger.
    pub fn output_path(&self) -> &PathBuf {
        &self.output_path
    }

    /// Emit a run_start event.
    pub fn run_start(&self, config: Option<E2eConfig>) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct RunStartEvent {
            ts: String,
            event: String,
            run_id: String,
            runner: String,
            env: E2eEnvironment,
            #[serde(skip_serializing_if = "Option::is_none")]
            config: Option<E2eConfig>,
        }

        let event = RunStartEvent {
            ts: Self::iso_timestamp(),
            event: "run_start".to_string(),
            run_id: self.run_id.clone(),
            runner: self.runner.clone(),
            env: self.env.clone(),
            config,
        };

        self.write_event(&event)
    }

    /// Emit a test_start event.
    pub fn test_start(&self, test: &E2eTestInfo) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct TestStartEvent {
            ts: String,
            event: String,
            run_id: String,
            runner: String,
            test: E2eTestInfo,
        }

        let event = TestStartEvent {
            ts: Self::iso_timestamp(),
            event: "test_start".to_string(),
            run_id: self.run_id.clone(),
            runner: self.runner.clone(),
            test: test.clone(),
        };

        self.write_event(&event)
    }

    /// Emit a test_end event for a passing test.
    pub fn test_pass(
        &self,
        test: &E2eTestInfo,
        duration_ms: u64,
        retries: Option<u32>,
    ) -> std::io::Result<()> {
        self.test_end(test, "pass", duration_ms, retries, None)
    }

    /// Emit a test_end event for a failing test.
    pub fn test_fail(
        &self,
        test: &E2eTestInfo,
        duration_ms: u64,
        retries: Option<u32>,
        error: E2eError,
    ) -> std::io::Result<()> {
        self.test_end(test, "fail", duration_ms, retries, Some(error))
    }

    /// Emit a test_end event for a skipped test.
    pub fn test_skip(&self, test: &E2eTestInfo) -> std::io::Result<()> {
        self.test_end(test, "skip", 0, None, None)
    }

    /// Emit a test_end event with full control.
    pub fn test_end(
        &self,
        test: &E2eTestInfo,
        status: &str,
        duration_ms: u64,
        retries: Option<u32>,
        error: Option<E2eError>,
    ) -> std::io::Result<()> {
        self.test_end_with_metrics(test, status, duration_ms, retries, error, None)
    }

    /// Emit a test_end event with performance metrics.
    pub fn test_end_with_metrics(
        &self,
        test: &E2eTestInfo,
        status: &str,
        duration_ms: u64,
        retries: Option<u32>,
        error: Option<E2eError>,
        metrics: Option<E2ePerformanceMetrics>,
    ) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct TestEndEvent {
            ts: String,
            event: String,
            run_id: String,
            runner: String,
            test: E2eTestInfo,
            result: E2eTestResult,
            #[serde(skip_serializing_if = "Option::is_none")]
            error: Option<E2eError>,
        }

        let event = TestEndEvent {
            ts: Self::iso_timestamp(),
            event: "test_end".to_string(),
            run_id: self.run_id.clone(),
            runner: self.runner.clone(),
            test: test.clone(),
            result: E2eTestResult {
                status: status.to_string(),
                duration_ms,
                retries,
                metrics,
            },
            error,
        };

        self.write_event(&event)
    }

    /// Emit a test_end event for a passing test with performance metrics.
    pub fn test_pass_with_metrics(
        &self,
        test: &E2eTestInfo,
        duration_ms: u64,
        metrics: E2ePerformanceMetrics,
    ) -> std::io::Result<()> {
        self.test_end_with_metrics(test, "pass", duration_ms, None, None, Some(metrics))
    }

    /// Emit a run_end event.
    pub fn run_end(&self, summary: E2eRunSummary, exit_code: i32) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct RunEndEvent {
            ts: String,
            event: String,
            run_id: String,
            runner: String,
            summary: E2eRunSummary,
            exit_code: i32,
        }

        let event = RunEndEvent {
            ts: Self::iso_timestamp(),
            event: "run_end".to_string(),
            run_id: self.run_id.clone(),
            runner: self.runner.clone(),
            summary,
            exit_code,
        };

        self.write_event(&event)?;
        self.flush()
    }

    /// Emit a log event.
    pub fn log(
        &self,
        level: &str,
        msg: &str,
        context: Option<E2eLogContext>,
    ) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct LogEvent {
            ts: String,
            event: String,
            run_id: String,
            runner: String,
            level: String,
            msg: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            context: Option<E2eLogContext>,
        }

        let event = LogEvent {
            ts: Self::iso_timestamp(),
            event: "log".to_string(),
            run_id: self.run_id.clone(),
            runner: self.runner.clone(),
            level: level.to_string(),
            msg: msg.to_string(),
            context,
        };

        self.write_event(&event)
    }

    /// Convenience: log at INFO level.
    pub fn info(&self, msg: &str) -> std::io::Result<()> {
        self.log("INFO", msg, None)
    }

    /// Convenience: log at WARN level.
    pub fn warn(&self, msg: &str) -> std::io::Result<()> {
        self.log("WARN", msg, None)
    }

    /// Convenience: log at ERROR level.
    pub fn error(&self, msg: &str) -> std::io::Result<()> {
        self.log("ERROR", msg, None)
    }

    /// Convenience: log at DEBUG level.
    pub fn debug(&self, msg: &str) -> std::io::Result<()> {
        self.log("DEBUG", msg, None)
    }

    /// Emit a phase_start event.
    pub fn phase_start(&self, phase: &E2ePhase) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct PhaseStartEvent {
            ts: String,
            event: String,
            run_id: String,
            runner: String,
            phase: E2ePhase,
        }

        let event = PhaseStartEvent {
            ts: Self::iso_timestamp(),
            event: "phase_start".to_string(),
            run_id: self.run_id.clone(),
            runner: self.runner.clone(),
            phase: phase.clone(),
        };

        self.write_event(&event)
    }

    /// Emit a phase_end event.
    pub fn phase_end(&self, phase: &E2ePhase, duration_ms: u64) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct PhaseEndEvent {
            ts: String,
            event: String,
            run_id: String,
            runner: String,
            phase: E2ePhase,
            duration_ms: u64,
        }

        let event = PhaseEndEvent {
            ts: Self::iso_timestamp(),
            event: "phase_end".to_string(),
            run_id: self.run_id.clone(),
            runner: self.runner.clone(),
            phase: phase.clone(),
            duration_ms,
        };

        self.write_event(&event)
    }

    /// Emit a metrics event with performance data.
    ///
    /// Use this to log performance metrics for a test or phase.
    /// The `name` parameter identifies what the metrics are for (e.g., "index", "search", "export").
    pub fn metrics(&self, name: &str, metrics: &E2ePerformanceMetrics) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct MetricsEvent {
            ts: String,
            event: String,
            run_id: String,
            runner: String,
            name: String,
            metrics: E2ePerformanceMetrics,
        }

        let event = MetricsEvent {
            ts: Self::iso_timestamp(),
            event: "metrics".to_string(),
            run_id: self.run_id.clone(),
            runner: self.runner.clone(),
            name: name.to_string(),
            metrics: metrics.clone(),
        };

        self.write_event(&event)
    }

    /// Flush the writer to ensure all events are persisted.
    pub fn flush(&self) -> std::io::Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.flush()
    }

    /// Emit a single metric value.
    ///
    /// Convenience method for emitting individual metrics without building a full
    /// E2ePerformanceMetrics struct.
    ///
    /// # Example
    /// ```ignore
    /// logger.emit_metric("search_latency_p50_ms", 42.5, "ms")?;
    /// ```
    pub fn emit_metric(&self, name: &str, value: f64, unit: &str) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct SingleMetricEvent {
            ts: String,
            event: String,
            run_id: String,
            runner: String,
            name: String,
            value: f64,
            unit: String,
        }

        let event = SingleMetricEvent {
            ts: Self::iso_timestamp(),
            event: "metric".to_string(),
            run_id: self.run_id.clone(),
            runner: self.runner.clone(),
            name: name.to_string(),
            value,
            unit: unit.to_string(),
        };

        self.write_event(&event)
    }

    // Internal helpers

    fn write_event<T: Serialize>(&self, event: &T) -> std::io::Result<()> {
        let json = serde_json::to_string(event)?;
        let mut writer = self.writer.lock().unwrap();
        writeln!(writer, "{}", json)?;
        Ok(())
    }

    fn output_dir() -> std::io::Result<PathBuf> {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let output_dir = manifest_dir.join("test-results").join("e2e");
        fs::create_dir_all(&output_dir)?;
        Ok(output_dir)
    }

    fn iso_timestamp() -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let millis = now.subsec_millis();

        // Convert to ISO-8601 format
        let datetime = chrono::DateTime::from_timestamp(secs as i64, millis * 1_000_000)
            .unwrap_or(chrono::DateTime::UNIX_EPOCH);
        datetime.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
    }

    fn timestamp_id() -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let datetime = chrono::DateTime::from_timestamp(secs as i64, 0)
            .unwrap_or(chrono::DateTime::UNIX_EPOCH);
        datetime.format("%Y%m%d_%H%M%S").to_string()
    }

    fn random_suffix() -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        std::process::id().hash(&mut hasher);
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .hash(&mut hasher);
        format!("{:x}", hasher.finish() & 0xFFFFFF)
    }
}

/// Phase tracker for structured logging in Rust E2E tests.
///
/// Emits test_start/test_end events and provides helpers for phase timing.
/// When E2E_VERBOSE=1 is set, also writes human-readable logs to a separate file.
#[allow(dead_code)]
pub struct PhaseTracker {
    logger: Option<E2eLogger>,
    test_info: E2eTestInfo,
    start_time: Instant,
    completed: bool,
    artifacts: E2eArtifactPaths,
    verbose_enabled: bool,
}

#[allow(dead_code)]
impl PhaseTracker {
    /// Create a new PhaseTracker for the given test.
    pub fn new(suite: &str, test_name: &str) -> Self {
        let trace_id = generate_trace_id();
        let artifacts = E2eArtifactPaths::prepare(suite, test_name, &trace_id)
            .expect("Failed to prepare E2E artifacts");

        let logger = if std::env::var("E2E_LOG").is_ok() {
            E2eLogger::with_path("rust", artifacts.cass_log_path.clone()).ok()
        } else {
            None
        };

        // Initialize verbose logging if E2E_VERBOSE is set
        let verbose_enabled = std::env::var("E2E_VERBOSE").is_ok();
        if verbose_enabled {
            let verbose_log_path = artifacts.dir.join("verbose.log");
            let _ = super::init_verbose_log(&verbose_log_path);
            super::verbose_log(&format!(
                "=== Verbose log for {suite}::{test_name} (trace_id={trace_id}) ==="
            ));
        }

        let mut test_info = E2eTestInfo::simple(test_name, suite);
        test_info.trace_id = Some(trace_id.clone());
        test_info.artifact_paths = Some(E2eArtifactManifest::from_paths(&artifacts));

        if let Some(ref lg) = logger {
            let _ = lg.test_start(&test_info);
        }

        if verbose_enabled {
            super::verbose_log(&format!("TEST_START name={test_name} suite={suite}"));
        }

        Self {
            logger,
            test_info,
            start_time: Instant::now(),
            completed: false,
            artifacts,
            verbose_enabled,
        }
    }

    /// Return artifact paths for this test.
    pub fn artifacts(&self) -> &E2eArtifactPaths {
        &self.artifacts
    }

    /// Return the trace ID for this test.
    pub fn trace_id(&self) -> &str {
        &self.artifacts.trace_id
    }

    /// Set environment variables to route trace output to this test's artifacts.
    pub fn trace_env_guard(&self) -> E2eTraceGuard {
        E2eTraceGuard::new(&self.artifacts)
    }

    /// Execute a phase and log start/end events.
    pub fn phase<F, R>(&self, name: &str, description: &str, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let phase = E2ePhase {
            name: name.to_string(),
            description: Some(description.to_string()),
        };

        if let Some(ref lg) = self.logger {
            let _ = lg.phase_start(&phase);
        }
        if self.verbose_enabled {
            super::verbose_log(&format!(
                "PHASE_START name={name} description=\"{description}\""
            ));
        }

        let start = Instant::now();
        let result = f();
        let duration_ms = start.elapsed().as_millis() as u64;

        if let Some(ref lg) = self.logger {
            let _ = lg.phase_end(&phase, duration_ms);
        }
        if self.verbose_enabled {
            super::verbose_log(&format!("PHASE_END name={name} duration_ms={duration_ms}"));
        }

        result
    }

    /// Start a phase and return the start time for manual timing.
    pub fn start(&self, name: &str, description: Option<&str>) -> Instant {
        let phase = E2ePhase {
            name: name.to_string(),
            description: description.map(String::from),
        };
        if let Some(ref lg) = self.logger {
            let _ = lg.phase_start(&phase);
        }
        if self.verbose_enabled {
            if let Some(desc) = description {
                super::verbose_log(&format!("PHASE_START name={name} description=\"{desc}\""));
            } else {
                super::verbose_log(&format!("PHASE_START name={name}"));
            }
        }
        Instant::now()
    }

    /// End a phase, logging duration.
    pub fn end(&self, name: &str, description: Option<&str>, start: Instant) {
        let duration_ms = start.elapsed().as_millis() as u64;
        let phase = E2ePhase {
            name: name.to_string(),
            description: description.map(String::from),
        };
        if let Some(ref lg) = self.logger {
            let _ = lg.phase_end(&phase, duration_ms);
        }
        if self.verbose_enabled {
            super::verbose_log(&format!("PHASE_END name={name} duration_ms={duration_ms}"));
        }
    }

    /// Emit a metrics event.
    pub fn metrics(&self, name: &str, metrics: &E2ePerformanceMetrics) {
        if let Some(ref lg) = self.logger {
            let _ = lg.metrics(name, metrics);
        }
        if self.verbose_enabled {
            super::verbose_log(&format!("METRICS name={name} data={:?}", metrics));
        }
    }

    /// Log a verbose message (only if E2E_VERBOSE is set).
    pub fn verbose(&self, msg: &str) {
        if self.verbose_enabled {
            super::verbose_log(msg);
        }
    }

    /// Mark test as completed successfully.
    pub fn complete(mut self) {
        self.completed = true;
        let duration_ms = self.start_time.elapsed().as_millis() as u64;
        if let Some(ref lg) = self.logger {
            let _ = lg.test_end(&self.test_info, "pass", duration_ms, None, None);
            let _ = lg.flush();
        }
        if self.verbose_enabled {
            super::verbose_log(&format!(
                "TEST_END name={} suite={} status=pass duration_ms={duration_ms}",
                self.test_info.name, self.test_info.suite
            ));
        }
    }

    /// Mark test as failed with error.
    pub fn fail(mut self, error: E2eError) {
        self.completed = true;
        let duration_ms = self.start_time.elapsed().as_millis() as u64;
        if let Some(ref lg) = self.logger {
            let _ = lg.test_end(
                &self.test_info,
                "fail",
                duration_ms,
                None,
                Some(error.clone()),
            );
            let _ = lg.flush();
        }
        if self.verbose_enabled {
            super::verbose_log(&format!(
                "TEST_END name={} suite={} status=fail duration_ms={duration_ms} error=\"{}\"",
                self.test_info.name, self.test_info.suite, error.message
            ));
        }
    }

    /// Flush the logger if present.
    pub fn flush(&self) {
        if let Some(ref lg) = self.logger {
            let _ = lg.flush();
        }
    }
}

/// Guard that configures trace env vars for a test run.
pub struct E2eTraceGuard {
    _trace_file: EnvGuard,
    _trace_id: EnvGuard,
}

impl E2eTraceGuard {
    fn new(artifacts: &E2eArtifactPaths) -> Self {
        let trace_file = artifacts.trace_path.to_string_lossy().to_string();
        let trace_id = artifacts.trace_id.clone();
        Self {
            _trace_file: EnvGuard::set("CASS_TRACE_FILE", trace_file),
            _trace_id: EnvGuard::set("CASS_TRACE_ID", trace_id),
        }
    }
}

fn generate_trace_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    std::process::id().hash(&mut hasher);
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    format!("{:x}", hasher.finish() & 0xFFFFFF)
}

impl Drop for PhaseTracker {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let duration_ms = self.start_time.elapsed().as_millis() as u64;
        let panicking = std::thread::panicking();

        // Dump failure state if panicking
        if panicking {
            let dump = FailureDump::new(&self.test_info.name, &self.test_info.suite);
            if let Err(e) = dump.write(&self.artifacts.dir) {
                eprintln!("[FailureDump] Failed to write dump: {}", e);
            }
        }

        if let Some(ref lg) = self.logger {
            let error = if panicking {
                Some(E2eError::new("panic"))
            } else {
                None
            };
            let status = if panicking { "fail" } else { "pass" };
            let _ = lg.test_end(&self.test_info, status, duration_ms, None, error);
            let _ = lg.flush();
        }
    }
}

// =============================================================================
// Failure State Dump
// =============================================================================

/// Captures comprehensive diagnostic state on test failure.
///
/// When a test panics, this struct captures:
/// 1. Environment variables (sanitized)
/// 2. Temp directory listing
/// 3. Log tail (last 100 lines)
/// 4. Database state (if SQLite exists)
/// 5. Git state (branch, uncommitted changes)
/// 6. Process info (memory, open files)
#[allow(dead_code)]
pub struct FailureDump {
    test_name: String,
    suite: String,
    timestamp: String,
}

#[allow(dead_code)]
impl FailureDump {
    /// Create a new FailureDump for the given test.
    pub fn new(test_name: &str, suite: &str) -> Self {
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
        Self {
            test_name: test_name.to_string(),
            suite: suite.to_string(),
            timestamp,
        }
    }

    /// Write the failure dump to the specified directory.
    ///
    /// Creates `test-results/failure_dumps/{suite}_{test_name}_{timestamp}.txt`
    pub fn write(&self, artifact_dir: &Path) -> std::io::Result<()> {
        // Create failure_dumps directory
        let dump_dir = Self::dump_dir()?;
        fs::create_dir_all(&dump_dir)?;

        let dump_path = dump_dir.join(format!(
            "{}_{}_{}.txt",
            self.suite, self.test_name, self.timestamp
        ));

        let mut f = File::create(&dump_path)?;

        // Header
        writeln!(
            f,
            "==============================================================================="
        )?;
        writeln!(f, "FAILURE STATE DUMP")?;
        writeln!(
            f,
            "==============================================================================="
        )?;
        writeln!(f, "Test: {}::{}", self.suite, self.test_name)?;
        writeln!(f, "Time: {}", chrono::Utc::now().to_rfc3339())?;
        writeln!(f, "Artifact Dir: {}", artifact_dir.display())?;
        writeln!(f)?;

        // 1. Environment
        self.dump_environment(&mut f)?;

        // 2. Temp directory listing
        self.dump_directory_listing(&mut f, artifact_dir)?;

        // 3. Log tail
        self.dump_log_tail(&mut f, artifact_dir)?;

        // 4. Database state
        self.dump_database_state(&mut f, artifact_dir)?;

        // 5. Git state
        self.dump_git_state(&mut f)?;

        // 6. Process info
        self.dump_process_info(&mut f)?;

        writeln!(f)?;
        writeln!(
            f,
            "==============================================================================="
        )?;
        writeln!(f, "END OF FAILURE DUMP")?;
        writeln!(
            f,
            "==============================================================================="
        )?;

        eprintln!("[FailureDump] Written to: {}", dump_path.display());
        Ok(())
    }

    fn dump_dir() -> std::io::Result<PathBuf> {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        Ok(manifest_dir.join("test-results").join("failure_dumps"))
    }

    fn dump_environment(&self, f: &mut File) -> std::io::Result<()> {
        writeln!(f, "=== ENVIRONMENT ===")?;
        writeln!(f, "Working Directory: {:?}", std::env::current_dir().ok())?;
        writeln!(f, "User: {:?}", std::env::var("USER").ok())?;
        writeln!(f, "Home: {:?}", std::env::var("HOME").ok())?;
        writeln!(f)?;

        // Sanitized env vars (reuse existing function)
        let env = capture_sanitized_env();
        let mut keys: Vec<_> = env.keys().collect();
        keys.sort();
        for key in keys {
            if let Some(val) = env.get(key) {
                writeln!(f, "{}={}", key, val)?;
            }
        }
        writeln!(f)?;
        Ok(())
    }

    fn dump_directory_listing(&self, f: &mut File, dir: &Path) -> std::io::Result<()> {
        writeln!(f, "=== TEMP DIRECTORY LISTING ===")?;
        writeln!(f, "Directory: {}", dir.display())?;
        writeln!(f)?;

        if dir.exists() {
            self.list_dir_recursive(f, dir, 0, 3)?; // Max depth of 3
        } else {
            writeln!(f, "(directory does not exist)")?;
        }
        writeln!(f)?;
        Ok(())
    }

    fn list_dir_recursive(
        &self,
        f: &mut File,
        dir: &Path,
        depth: usize,
        max_depth: usize,
    ) -> std::io::Result<()> {
        if depth > max_depth {
            writeln!(f, "{}... (max depth reached)", "  ".repeat(depth))?;
            return Ok(());
        }

        let indent = "  ".repeat(depth);

        match fs::read_dir(dir) {
            Ok(entries) => {
                let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
                entries.sort_by_key(|e| e.file_name());

                for entry in entries.iter().take(50) {
                    // Limit entries per directory
                    let path = entry.path();
                    let name = entry.file_name();
                    let metadata = entry.metadata().ok();

                    let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
                    let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);

                    if is_dir {
                        writeln!(f, "{}{}/", indent, name.to_string_lossy())?;
                        self.list_dir_recursive(f, &path, depth + 1, max_depth)?;
                    } else {
                        writeln!(f, "{}{} ({} bytes)", indent, name.to_string_lossy(), size)?;
                    }
                }

                if entries.len() > 50 {
                    writeln!(f, "{}... ({} more entries)", indent, entries.len() - 50)?;
                }
            }
            Err(e) => {
                writeln!(f, "{}(error reading directory: {})", indent, e)?;
            }
        }
        Ok(())
    }

    fn dump_log_tail(&self, f: &mut File, artifact_dir: &Path) -> std::io::Result<()> {
        writeln!(f, "=== LOG TAIL (last 100 lines) ===")?;

        // Check for common log files
        let log_files = [
            artifact_dir.join("cass.log"),
            artifact_dir.join("stdout"),
            artifact_dir.join("stderr"),
            artifact_dir.join("verbose.log"),
        ];

        for log_path in &log_files {
            if log_path.exists() {
                writeln!(f, "--- {} ---", log_path.display())?;
                match fs::read_to_string(log_path) {
                    Ok(content) => {
                        let lines: Vec<_> = content.lines().collect();
                        let start = lines.len().saturating_sub(100);
                        for line in &lines[start..] {
                            writeln!(f, "{}", line)?;
                        }
                    }
                    Err(e) => {
                        writeln!(f, "(error reading file: {})", e)?;
                    }
                }
                writeln!(f)?;
            }
        }
        Ok(())
    }

    fn dump_database_state(&self, f: &mut File, artifact_dir: &Path) -> std::io::Result<()> {
        writeln!(f, "=== DATABASE STATE ===")?;

        // Look for SQLite databases
        let _db_patterns = ["*.db", "*.sqlite", "*.sqlite3"];
        let mut found_any = false;

        if let Ok(entries) = fs::read_dir(artifact_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if let Some(ext) = path.extension() {
                    let ext_str = ext.to_string_lossy().to_lowercase();
                    if ext_str == "db" || ext_str == "sqlite" || ext_str == "sqlite3" {
                        found_any = true;
                        writeln!(f, "--- {} ---", path.display())?;
                        self.dump_sqlite_info(f, &path)?;
                    }
                }
            }
        }

        if !found_any {
            writeln!(f, "(no SQLite databases found in artifact directory)")?;
        }
        writeln!(f)?;
        Ok(())
    }

    fn dump_sqlite_info(&self, f: &mut File, db_path: &Path) -> std::io::Result<()> {
        // Try to get schema and row counts using sqlite3 command
        let schema_output = std::process::Command::new("sqlite3")
            .arg(db_path)
            .arg(".schema")
            .output();

        match schema_output {
            Ok(output) if output.status.success() => {
                let schema = String::from_utf8_lossy(&output.stdout);
                let lines: Vec<_> = schema.lines().take(50).collect();
                writeln!(f, "Schema (first 50 lines):")?;
                for line in lines {
                    writeln!(f, "  {}", line)?;
                }
                if schema.lines().count() > 50 {
                    writeln!(f, "  ... (truncated)")?;
                }
            }
            _ => {
                writeln!(f, "(sqlite3 command not available or failed)")?;
            }
        }

        // Get table counts
        let tables_output = std::process::Command::new("sqlite3")
            .arg(db_path)
            .arg("SELECT name FROM sqlite_master WHERE type='table';")
            .output();

        if let Ok(output) = tables_output {
            if output.status.success() {
                let tables = String::from_utf8_lossy(&output.stdout);
                writeln!(f, "Tables:")?;
                for table in tables.lines().take(20) {
                    // Get row count for each table
                    let count_output = std::process::Command::new("sqlite3")
                        .arg(db_path)
                        .arg(format!("SELECT COUNT(*) FROM \"{}\";", table))
                        .output();
                    let count = count_output
                        .ok()
                        .filter(|o| o.status.success())
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                        .unwrap_or_else(|| "?".to_string());
                    writeln!(f, "  {} ({} rows)", table, count)?;
                }
            }
        }
        Ok(())
    }

    fn dump_git_state(&self, f: &mut File) -> std::io::Result<()> {
        writeln!(f, "=== GIT STATE ===")?;

        // Current branch
        let branch_output = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output();
        if let Ok(output) = branch_output {
            if output.status.success() {
                writeln!(
                    f,
                    "Branch: {}",
                    String::from_utf8_lossy(&output.stdout).trim()
                )?;
            }
        }

        // Current commit
        let commit_output = std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output();
        if let Ok(output) = commit_output {
            if output.status.success() {
                writeln!(
                    f,
                    "Commit: {}",
                    String::from_utf8_lossy(&output.stdout).trim()
                )?;
            }
        }

        // Uncommitted changes (short status)
        let status_output = std::process::Command::new("git")
            .args(["status", "--short"])
            .output();
        if let Ok(output) = status_output {
            if output.status.success() {
                let status = String::from_utf8_lossy(&output.stdout);
                if status.trim().is_empty() {
                    writeln!(f, "Status: (clean)")?;
                } else {
                    writeln!(f, "Uncommitted changes:")?;
                    for line in status.lines().take(20) {
                        writeln!(f, "  {}", line)?;
                    }
                    if status.lines().count() > 20 {
                        writeln!(f, "  ... (truncated)")?;
                    }
                }
            }
        }
        writeln!(f)?;
        Ok(())
    }

    fn dump_process_info(&self, f: &mut File) -> std::io::Result<()> {
        writeln!(f, "=== PROCESS INFO ===")?;
        writeln!(f, "PID: {}", std::process::id())?;

        // Memory usage on Linux
        #[cfg(target_os = "linux")]
        {
            if let Ok(statm) = fs::read_to_string("/proc/self/statm") {
                let parts: Vec<_> = statm.split_whitespace().collect();
                if parts.len() >= 2 {
                    let page_size = 4096u64; // Typical page size
                    if let Ok(resident) = parts[1].parse::<u64>() {
                        let rss_mb = (resident * page_size) as f64 / (1024.0 * 1024.0);
                        writeln!(f, "Memory (RSS): {:.2} MB", rss_mb)?;
                    }
                }
            }

            // Open file handles
            if let Ok(entries) = fs::read_dir("/proc/self/fd") {
                let count = entries.count();
                writeln!(f, "Open file handles: {}", count)?;
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            writeln!(f, "(detailed process info not available on this platform)")?;
        }

        writeln!(f)?;
        Ok(())
    }
}

/// Standalone function to dump failure state from any test context.
///
/// Can be called from shell scripts via a wrapper binary or from Rust tests directly.
///
/// # Example
/// ```ignore
/// use crate::util::e2e_log::dump_failure_state;
///
/// // In a test's panic handler or cleanup:
/// if let Err(e) = dump_failure_state("test_name", "suite_name", "/path/to/temp/dir") {
///     eprintln!("Failed to dump state: {}", e);
/// }
/// ```
#[allow(dead_code)]
pub fn dump_failure_state(
    test_name: &str,
    suite: &str,
    artifact_dir: impl AsRef<Path>,
) -> std::io::Result<PathBuf> {
    let dump = FailureDump::new(test_name, suite);
    dump.write(artifact_dir.as_ref())?;

    let dump_dir = FailureDump::dump_dir()?;
    Ok(dump_dir.join(format!("{}_{}_{}.txt", suite, test_name, dump.timestamp)))
}

// =============================================================================
// Standalone Metric Emission & Baseline Tracking
// =============================================================================

/// Global metric file for standalone metric emission.
static METRIC_FILE: std::sync::LazyLock<Mutex<Option<File>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

/// Initialize the standalone metric file for a test run.
///
/// Call this at the start of a test suite to set up metric collection.
#[allow(dead_code)]
pub fn init_metrics_file(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(path)?;
    let mut guard = METRIC_FILE.lock().unwrap();
    *guard = Some(file);
    Ok(())
}

/// Emit a single metric to the global metric file.
///
/// This is a convenience function that doesn't require an E2eLogger instance.
/// Useful for quick metrics in shell-like test scenarios.
///
/// # Example
/// ```ignore
/// emit_metric("indexing_duration_ms", start.elapsed().as_millis() as f64, "ms")?;
/// emit_metric("memory_peak_kb", peak_mem as f64, "KB")?;
/// ```
#[allow(dead_code)]
pub fn emit_metric(name: &str, value: f64, unit: &str) -> std::io::Result<()> {
    let run_id = std::env::var("CASS_RUN_ID").unwrap_or_else(|_| "unknown".to_string());
    let ts = chrono::Utc::now().to_rfc3339();

    let json = serde_json::json!({
        "ts": ts,
        "event": "metric",
        "run_id": run_id,
        "name": name,
        "value": value,
        "unit": unit
    });

    // Try global file first, fall back to stderr
    if let Ok(mut guard) = METRIC_FILE.lock()
        && let Some(ref mut file) = *guard
    {
        writeln!(file, "{}", json)?;
        file.flush()?;
    } else {
        eprintln!("[METRIC] {}", json);
    }

    Ok(())
}

/// Metric baseline for regression tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricBaseline {
    pub name: String,
    pub value: f64,
    pub unit: String,
    pub timestamp: String,
    pub commit: Option<String>,
}

/// Baseline comparison result.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct BaselineComparison {
    pub name: String,
    pub current: f64,
    pub baseline: f64,
    pub unit: String,
    pub diff_pct: f64,
    pub is_regression: bool,
}

/// Load baselines from the baselines.json file.
#[allow(dead_code)]
pub fn load_baselines() -> std::io::Result<HashMap<String, MetricBaseline>> {
    let path = baselines_path()?;
    if !path.exists() {
        return Ok(HashMap::new());
    }

    let content = fs::read_to_string(&path)?;
    let baselines: Vec<MetricBaseline> = serde_json::from_str(&content)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    Ok(baselines.into_iter().map(|b| (b.name.clone(), b)).collect())
}

/// Save baselines to the baselines.json file.
#[allow(dead_code)]
pub fn save_baselines(baselines: &HashMap<String, MetricBaseline>) -> std::io::Result<()> {
    let path = baselines_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let values: Vec<_> = baselines.values().cloned().collect();
    let content = serde_json::to_string_pretty(&values)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    fs::write(&path, content)
}

/// Update a single baseline value.
#[allow(dead_code)]
pub fn update_baseline(name: &str, value: f64, unit: &str) -> std::io::Result<()> {
    let mut baselines = load_baselines()?;

    // Get current git commit
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    baselines.insert(
        name.to_string(),
        MetricBaseline {
            name: name.to_string(),
            value,
            unit: unit.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            commit,
        },
    );

    save_baselines(&baselines)
}

/// Compare a metric value against its baseline.
///
/// Returns a comparison result indicating whether this is a regression (>20% worse).
#[allow(dead_code)]
pub fn compare_to_baseline(
    name: &str,
    value: f64,
    unit: &str,
) -> std::io::Result<BaselineComparison> {
    let baselines = load_baselines()?;

    let baseline = baselines.get(name).map(|b| b.value).unwrap_or(value);

    let diff_pct = if baseline > 0.0 {
        ((value - baseline) / baseline) * 100.0
    } else {
        0.0
    };

    // For most metrics, higher is worse (duration, memory).
    // Consider >20% increase as regression.
    let is_regression = diff_pct > 20.0;

    Ok(BaselineComparison {
        name: name.to_string(),
        current: value,
        baseline,
        unit: unit.to_string(),
        diff_pct,
        is_regression,
    })
}

/// Check a metric against baseline and emit an alert if regression detected.
///
/// Emits the metric, compares to baseline, and logs a warning if >20% regression.
///
/// # Example
/// ```ignore
/// let latency = measure_search_latency();
/// check_metric_regression("search_latency_p50_ms", latency, "ms")?;
/// ```
#[allow(dead_code)]
pub fn check_metric_regression(
    name: &str,
    value: f64,
    unit: &str,
) -> std::io::Result<BaselineComparison> {
    // Emit the metric
    emit_metric(name, value, unit)?;

    // Compare to baseline
    let comparison = compare_to_baseline(name, value, unit)?;

    // Log if regression detected
    if comparison.is_regression {
        eprintln!(
            "[REGRESSION ALERT] {}: {:.2}{} (baseline: {:.2}{}, +{:.1}%)",
            name, value, unit, comparison.baseline, unit, comparison.diff_pct
        );
    }

    Ok(comparison)
}

fn baselines_path() -> std::io::Result<PathBuf> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    Ok(manifest_dir.join("test-results").join("baselines.json"))
}

/// Standard E2E metrics to collect per test run.
///
/// Use this as a checklist for what to measure in E2E tests.
#[allow(dead_code)]
pub mod standard_metrics {
    /// Time to index the test corpus (ms)
    pub const INDEXING_DURATION_MS: &str = "indexing_duration_ms";
    /// Median search query time (ms)
    pub const SEARCH_LATENCY_P50_MS: &str = "search_latency_p50_ms";
    /// 99th percentile search time (ms)
    pub const SEARCH_LATENCY_P99_MS: &str = "search_latency_p99_ms";
    /// Peak memory usage (KB)
    pub const MEMORY_PEAK_KB: &str = "memory_peak_kb";
    /// Size of search index (bytes)
    pub const INDEX_SIZE_BYTES: &str = "index_size_bytes";
    /// Number of files indexed
    pub const FILES_PROCESSED: &str = "files_processed";
    /// Search throughput (queries per second)
    pub const QUERIES_PER_SECOND: &str = "queries_per_second";
}

/// Run a test and emit structured logging events when E2E_LOG is enabled.
#[allow(dead_code)]
pub fn run_logged_test<F>(name: &str, suite: &str, file: &str, line: u32, test_fn: F)
where
    F: FnOnce() -> Result<(), Box<dyn std::error::Error>>,
{
    let trace_id = generate_trace_id();
    let artifacts = E2eArtifactPaths::prepare(suite, name, &trace_id)
        .expect("Failed to prepare E2E artifacts for run_logged_test");

    let logger = if std::env::var("E2E_LOG").is_ok() {
        E2eLogger::with_path("rust", artifacts.cass_log_path.clone()).ok()
    } else {
        None
    };

    let mut test_info = E2eTestInfo::new(name, suite, file, line);
    test_info.trace_id = Some(trace_id);
    test_info.artifact_paths = Some(E2eArtifactManifest::from_paths(&artifacts));
    if let Some(ref lg) = logger {
        let _ = lg.test_start(&test_info);
    }

    let start = Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test_fn));
    let duration_ms = start.elapsed().as_millis() as u64;

    let (is_pass, error_msg, panic_type) = match &result {
        Ok(Ok(())) => (true, None, None),
        Ok(Err(e)) => (false, Some(e.to_string()), None),
        Err(panic) => {
            let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic.downcast_ref::<String>() {
                s.clone()
            } else {
                "Unknown panic".to_string()
            };
            (false, Some(msg), Some("Panic"))
        }
    };

    if let Some(ref lg) = logger {
        if is_pass {
            let _ = lg.test_pass(&test_info, duration_ms, None);
        } else {
            let _ = lg.test_fail(
                &test_info,
                duration_ms,
                None,
                E2eError {
                    message: error_msg.unwrap_or_default(),
                    error_type: panic_type.map(String::from),
                    stack: None,
                    context: None,
                },
            );
        }
        let _ = lg.flush();
    }

    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Convenience macro for creating E2eTestInfo with file and line.
#[macro_export]
macro_rules! e2e_test_info {
    ($name:expr, $suite:expr) => {
        $crate::util::e2e_log::E2eTestInfo::new($name, $suite, file!(), line!())
    };
}

/// Log a test run with optional structured output (E2E_LOG=1).
#[macro_export]
macro_rules! logged_test {
    ($name:expr, $suite:expr, $body:block) => {{
        $crate::util::e2e_log::run_logged_test($name, $suite, file!(), line!(), || {
            let result: Result<(), Box<dyn std::error::Error>> = (|| {
                $body
                Ok(())
            })();
            result
        });
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    #[test]
    fn test_environment_capture() {
        let env = E2eEnvironment::capture();
        assert!(!env.os.is_empty());
        assert!(!env.arch.is_empty());
    }

    #[test]
    fn test_logger_creates_file() {
        let tmp = TempDir::new().unwrap();
        let output_path = tmp.path().join("test.jsonl");

        let logger = E2eLogger::with_path("rust", output_path.clone()).unwrap();
        logger.run_start(None).unwrap();
        logger.flush().unwrap();

        assert!(output_path.exists());
        let content = fs::read_to_string(&output_path).unwrap();
        assert!(content.contains("run_start"));
        assert!(content.contains(&logger.run_id));
    }

    #[test]
    fn test_logger_test_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let output_path = tmp.path().join("lifecycle.jsonl");

        let logger = E2eLogger::with_path("rust", output_path.clone()).unwrap();

        let test_info = E2eTestInfo::new("test_example", "unit", "test.rs", 42);

        logger.run_start(None).unwrap();
        logger.test_start(&test_info).unwrap();
        logger.test_pass(&test_info, 100, None).unwrap();
        logger
            .run_end(
                E2eRunSummary {
                    total: 1,
                    passed: 1,
                    failed: 0,
                    skipped: 0,
                    flaky: None,
                    duration_ms: 100,
                },
                0,
            )
            .unwrap();

        let content = fs::read_to_string(&output_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("run_start"));
        assert!(lines[1].contains("test_start"));
        assert!(lines[2].contains("test_end"));
        assert!(lines[3].contains("run_end"));
    }

    #[test]
    fn test_logger_error_event() {
        let tmp = TempDir::new().unwrap();
        let output_path = tmp.path().join("error.jsonl");

        let logger = E2eLogger::with_path("rust", output_path.clone()).unwrap();
        let test_info = E2eTestInfo::simple("failing_test", "e2e");

        logger
            .test_fail(
                &test_info,
                500,
                Some(1),
                E2eError {
                    message: "assertion failed".to_string(),
                    error_type: Some("AssertionError".to_string()),
                    stack: Some("at line 42".to_string()),
                    context: None,
                },
            )
            .unwrap();
        logger.flush().unwrap();

        let content = fs::read_to_string(&output_path).unwrap();
        assert!(content.contains("assertion failed"));
        assert!(content.contains("AssertionError"));
        assert!(content.contains("\"status\":\"fail\""));
    }

    // ==================== E2eError builder tests ====================

    #[test]
    fn test_e2e_error_new() {
        let error = E2eError::new("test error");
        assert_eq!(error.message, "test error");
        assert!(error.error_type.is_none());
        assert!(error.stack.is_none());
        assert!(error.context.is_none());
    }

    #[test]
    fn test_e2e_error_with_type() {
        let error = E2eError::with_type("assertion failed", "AssertionError");
        assert_eq!(error.message, "assertion failed");
        assert_eq!(error.error_type, Some("AssertionError".to_string()));
    }

    #[test]
    fn test_e2e_error_with_stack() {
        let error = E2eError::new("error").with_stack("stack trace here");
        assert_eq!(error.stack, Some("stack trace here".to_string()));
    }

    #[test]
    fn test_e2e_error_builder_chain() {
        let context = E2eErrorContext::new()
            .add_state("variable", serde_json::json!("value"))
            .capture_cwd();

        let error = E2eError::with_type("test failure", "TestError")
            .with_stack("at test.rs:42")
            .with_context(context);

        assert_eq!(error.message, "test failure");
        assert_eq!(error.error_type, Some("TestError".to_string()));
        assert_eq!(error.stack, Some("at test.rs:42".to_string()));
        assert!(error.context.is_some());
    }

    // ==================== E2eErrorContext tests ====================

    #[test]
    fn test_error_context_new() {
        let ctx = E2eErrorContext::new();
        assert!(ctx.state.is_none());
        assert!(ctx.screenshot_path.is_none());
        assert!(ctx.env_vars.is_none());
        assert!(ctx.cwd.is_none());
        assert!(ctx.command.is_none());
        assert!(ctx.stdout.is_none());
        assert!(ctx.stderr.is_none());
    }

    #[test]
    fn test_error_context_default() {
        let ctx = E2eErrorContext::default();
        assert!(ctx.state.is_none());
    }

    #[test]
    fn test_error_context_add_state() {
        let ctx = E2eErrorContext::new()
            .add_state("count", serde_json::json!(42))
            .add_state("name", serde_json::json!("test"));

        let state = ctx.state.unwrap();
        assert_eq!(state.get("count"), Some(&serde_json::json!(42)));
        assert_eq!(state.get("name"), Some(&serde_json::json!("test")));
    }

    #[test]
    fn test_error_context_with_state() {
        let mut state = HashMap::new();
        state.insert("phase".to_string(), serde_json::json!("init"));
        state.insert("count".to_string(), serde_json::json!(3));

        let ctx = E2eErrorContext::new().with_state(state.clone());
        assert_eq!(ctx.state, Some(state));
    }

    #[test]
    fn test_error_context_with_screenshot() {
        let ctx = E2eErrorContext::new().with_screenshot("/tmp/failure.png");
        assert_eq!(ctx.screenshot_path, Some("/tmp/failure.png".to_string()));
    }

    #[test]
    fn test_error_context_capture_cwd() {
        let ctx = E2eErrorContext::new().capture_cwd();
        assert!(ctx.cwd.is_some());
        // CWD should be a valid path
        assert!(!ctx.cwd.unwrap().is_empty());
    }

    #[test]
    fn test_error_context_with_command() {
        let ctx = E2eErrorContext::new()
            .with_command("cargo test")
            .with_output(
                Some("test output".to_string()),
                Some("error output".to_string()),
            );

        assert_eq!(ctx.command, Some("cargo test".to_string()));
        assert_eq!(ctx.stdout, Some("test output".to_string()));
        assert_eq!(ctx.stderr, Some("error output".to_string()));
    }

    #[test]
    fn test_error_context_capture_env() {
        let ctx = E2eErrorContext::new().capture_env();
        assert!(ctx.env_vars.is_some());
        // Should have some env vars
        let env = ctx.env_vars.unwrap();
        // PATH is usually present
        assert!(env.contains_key("PATH") || env.contains_key("HOME") || !env.is_empty());
    }

    #[test]
    fn test_error_context_with_env() {
        let mut env = HashMap::new();
        env.insert("E2E_TEST_VAR".to_string(), "value".to_string());
        let ctx = E2eErrorContext::new().with_env(env.clone());
        assert_eq!(ctx.env_vars, Some(env));
    }

    // ==================== capture_sanitized_env tests ====================

    #[test]
    fn test_sanitized_env_redacts_sensitive() {
        // Set a test sensitive env var
        // SAFETY: This test runs in isolation and the env var is cleaned up afterwards
        unsafe {
            std::env::set_var("TEST_SECRET_KEY", "super_secret_value");
        }

        let env = capture_sanitized_env();

        // Check that sensitive keys are redacted
        if let Some(value) = env.get("TEST_SECRET_KEY") {
            assert_eq!(value, "[REDACTED]");
        }

        // Clean up
        // SAFETY: Cleaning up the env var we set above
        unsafe {
            std::env::remove_var("TEST_SECRET_KEY");
        }
    }

    #[test]
    fn test_sanitized_env_preserves_safe() {
        // Set a safe test env var
        // SAFETY: This test runs in isolation and the env var is cleaned up afterwards
        unsafe {
            std::env::set_var("TEST_SAFE_VAR", "safe_value");
        }

        let env = capture_sanitized_env();

        // Safe vars should be preserved
        if let Some(value) = env.get("TEST_SAFE_VAR") {
            assert_eq!(value, "safe_value");
        }

        // Clean up
        // SAFETY: Cleaning up the env var we set above
        unsafe {
            std::env::remove_var("TEST_SAFE_VAR");
        }
    }

    // ==================== Error with context in logger tests ====================

    #[test]
    fn test_logger_error_with_context() {
        let tmp = TempDir::new().unwrap();
        let output_path = tmp.path().join("error_context.jsonl");

        let logger = E2eLogger::with_path("rust", output_path.clone()).unwrap();
        let test_info = E2eTestInfo::simple("context_test", "e2e");

        let context = E2eErrorContext::new()
            .add_state("iteration", serde_json::json!(5))
            .with_command("cargo test")
            .capture_cwd();

        let error = E2eError::with_type("assertion failed", "AssertionError")
            .with_stack("at test.rs:100")
            .with_context(context);

        logger.test_fail(&test_info, 100, None, error).unwrap();
        logger.flush().unwrap();

        let content = fs::read_to_string(&output_path).unwrap();
        assert!(content.contains("context"));
        assert!(content.contains("iteration"));
        assert!(content.contains("cargo test"));
    }

    #[test]
    fn test_log_levels() {
        let tmp = TempDir::new().unwrap();
        let output_path = tmp.path().join("logs.jsonl");

        let logger = E2eLogger::with_path("rust", output_path.clone()).unwrap();

        logger.info("info message").unwrap();
        logger.warn("warning message").unwrap();
        logger.error("error message").unwrap();
        logger.debug("debug message").unwrap();
        logger.flush().unwrap();

        let content = fs::read_to_string(&output_path).unwrap();
        assert!(content.contains("\"level\":\"INFO\""));
        assert!(content.contains("\"level\":\"WARN\""));
        assert!(content.contains("\"level\":\"ERROR\""));
        assert!(content.contains("\"level\":\"DEBUG\""));
    }

    // ==================== E2ePerformanceMetrics tests ====================

    #[test]
    fn test_performance_metrics_new() {
        let metrics = E2ePerformanceMetrics::new();
        assert!(metrics.duration_ms.is_none());
        assert!(metrics.memory_bytes.is_none());
        assert!(metrics.custom.is_none());
    }

    #[test]
    fn test_performance_metrics_with_duration() {
        let metrics = E2ePerformanceMetrics::new().with_duration(1000);
        assert_eq!(metrics.duration_ms, Some(1000));
    }

    #[test]
    fn test_performance_metrics_with_memory() {
        let metrics = E2ePerformanceMetrics::new()
            .with_memory(1024 * 1024)
            .with_peak_memory(2 * 1024 * 1024);
        assert_eq!(metrics.memory_bytes, Some(1024 * 1024));
        assert_eq!(metrics.peak_memory_bytes, Some(2 * 1024 * 1024));
    }

    #[test]
    fn test_performance_metrics_with_throughput() {
        let metrics = E2ePerformanceMetrics::new().with_throughput(1000, 2000);
        assert_eq!(metrics.items_processed, Some(1000));
        assert_eq!(metrics.duration_ms, Some(2000));
        assert_eq!(metrics.throughput_per_sec, Some(500.0));
    }

    #[test]
    fn test_performance_metrics_with_io() {
        let metrics = E2ePerformanceMetrics::new().with_io(100, 50, 10240, 5120);
        assert_eq!(metrics.file_reads, Some(100));
        assert_eq!(metrics.file_writes, Some(50));
        assert_eq!(metrics.bytes_read, Some(10240));
        assert_eq!(metrics.bytes_written, Some(5120));
    }

    #[test]
    fn test_performance_metrics_with_network() {
        let metrics = E2ePerformanceMetrics::new().with_network(42);
        assert_eq!(metrics.network_requests, Some(42));
    }

    #[test]
    fn test_performance_metrics_with_custom() {
        let metrics = E2ePerformanceMetrics::new()
            .with_custom("cache_hits", serde_json::json!(99))
            .with_custom("retries", serde_json::json!(3));
        let custom = metrics.custom.unwrap();
        assert_eq!(custom.get("cache_hits"), Some(&serde_json::json!(99)));
        assert_eq!(custom.get("retries"), Some(&serde_json::json!(3)));
    }

    #[test]
    fn test_performance_metrics_builder_chain() {
        let metrics = E2ePerformanceMetrics::new()
            .with_duration(1500)
            .with_memory(2048)
            .with_io(10, 5, 1024, 512)
            .with_custom("index_count", serde_json::json!(100));

        assert_eq!(metrics.duration_ms, Some(1500));
        assert_eq!(metrics.memory_bytes, Some(2048));
        assert_eq!(metrics.file_reads, Some(10));
        assert!(metrics.custom.is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_performance_metrics_capture_memory() {
        let memory = E2ePerformanceMetrics::capture_memory();
        // On Linux, this should succeed
        assert!(memory.is_some());
        assert!(memory.unwrap() > 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_performance_metrics_capture_io() {
        let io = E2ePerformanceMetrics::capture_io();
        // On Linux, this should succeed
        assert!(io.is_some());
    }

    // ==================== Logger with metrics tests ====================

    #[test]
    fn test_logger_test_pass_with_metrics() {
        let tmp = TempDir::new().unwrap();
        let output_path = tmp.path().join("metrics.jsonl");

        let logger = E2eLogger::with_path("rust", output_path.clone()).unwrap();
        let test_info = E2eTestInfo::simple("metrics_test", "e2e");

        let metrics = E2ePerformanceMetrics::new()
            .with_duration(500)
            .with_memory(1024)
            .with_throughput(100, 500);

        logger
            .test_pass_with_metrics(&test_info, 500, metrics)
            .unwrap();
        logger.flush().unwrap();

        let content = fs::read_to_string(&output_path).unwrap();
        assert!(content.contains("\"metrics\""));
        assert!(content.contains("\"memory_bytes\":1024"));
        assert!(content.contains("\"throughput_per_sec\":200"));
    }

    #[test]
    fn test_logger_test_end_with_metrics() {
        let tmp = TempDir::new().unwrap();
        let output_path = tmp.path().join("metrics_full.jsonl");

        let logger = E2eLogger::with_path("rust", output_path.clone()).unwrap();
        let test_info = E2eTestInfo::simple("full_metrics_test", "e2e");

        let metrics = E2ePerformanceMetrics::new()
            .with_io(50, 25, 5000, 2500)
            .with_network(10)
            .with_custom("search_latency_p99", serde_json::json!(45.5));

        logger
            .test_end_with_metrics(&test_info, "pass", 1000, None, None, Some(metrics))
            .unwrap();
        logger.flush().unwrap();

        let content = fs::read_to_string(&output_path).unwrap();
        assert!(content.contains("\"file_reads\":50"));
        assert!(content.contains("\"network_requests\":10"));
        assert!(content.contains("search_latency_p99"));
    }

    #[test]
    fn test_logger_test_end_without_metrics_still_works() {
        let tmp = TempDir::new().unwrap();
        let output_path = tmp.path().join("no_metrics.jsonl");

        let logger = E2eLogger::with_path("rust", output_path.clone()).unwrap();
        let test_info = E2eTestInfo::simple("no_metrics_test", "e2e");

        // Old API should still work
        logger.test_pass(&test_info, 100, None).unwrap();
        logger.flush().unwrap();

        let content = fs::read_to_string(&output_path).unwrap();
        assert!(content.contains("\"status\":\"pass\""));
        // metrics should not be present when not provided
        assert!(!content.contains("\"metrics\""));
    }

    // ==================== PhaseTracker Tests ====================

    #[test]
    fn test_phase_tracker_new() {
        // PhaseTracker initializes without panic even without E2E_LOG
        let tracker = PhaseTracker::new("test_suite", "tracker_init");
        tracker.complete();
    }

    #[test]
    fn test_phase_tracker_phase_lifecycle() {
        let tracker = PhaseTracker::new("test_suite", "phase_lifecycle");

        // phase() executes the closure and returns its result
        let result = tracker.phase("setup", "Setting up test fixtures", || 42);
        assert_eq!(result, 42, "phase() must return the closure's result");

        // Multiple phases execute sequentially
        let result2 = tracker.phase("verify", "Verifying results", || "hello");
        assert_eq!(
            result2, "hello",
            "Sequential phases must each return correctly"
        );

        tracker.complete();
    }

    #[test]
    fn test_phase_tracker_manual_timing() {
        let tracker = PhaseTracker::new("test_suite", "manual_timing");

        // start()/end() pair with description
        let start = tracker.start("setup", Some("Manual phase with description"));
        std::thread::sleep(std::time::Duration::from_millis(5));
        tracker.end("setup", Some("Manual phase with description"), start);

        // start()/end() pair without description
        let start2 = tracker.start("run", None);
        tracker.end("run", None, start2);

        tracker.complete();
    }

    #[test]
    fn test_phase_tracker_nested_phases() {
        let tracker = PhaseTracker::new("test_suite", "nested_phases");

        // Nested phases: closure-based outer with manual inner
        let outer_result = tracker.phase("outer", "Outer phase", || {
            let inner_start = tracker.start("inner", Some("Inner phase"));
            let value = 100 + 42;
            tracker.end("inner", Some("Inner phase"), inner_start);
            value
        });
        assert_eq!(outer_result, 142, "Nested phases must compose correctly");

        tracker.complete();
    }

    #[test]
    fn test_phase_tracker_complete_prevents_double_log() {
        // After complete(), the Drop impl should no-op (completed flag is true)
        let tracker = PhaseTracker::new("test_suite", "complete_idempotent");
        tracker.complete();
        // Drop runs here but should detect completed=true and skip
    }

    #[test]
    fn test_phase_tracker_drop_without_complete() {
        // When complete() is not called and thread is not panicking,
        // Drop should handle gracefully with status "pass"
        let _tracker = PhaseTracker::new("test_suite", "drop_implicit");
        // Drop runs here - should not panic
    }

    #[test]
    fn test_phase_tracker_fail() {
        let tracker = PhaseTracker::new("test_suite", "fail_test");
        tracker.fail(E2eError::new("Deliberate test failure"));
        // Should not panic; Drop should detect completed=true and skip
    }

    #[test]
    fn test_phase_tracker_metrics() {
        let tracker = PhaseTracker::new("test_suite", "metrics_emission");

        let metrics = E2ePerformanceMetrics::new()
            .with_duration(100)
            .with_memory(1024);

        // metrics() should not panic even when logger is None
        tracker.metrics("test_operation", &metrics);

        tracker.complete();
    }

    // ==================== FailureDump tests ====================

    #[test]
    fn test_failure_dump_new() {
        let dump = FailureDump::new("my_test", "my_suite");
        assert_eq!(dump.test_name, "my_test");
        assert_eq!(dump.suite, "my_suite");
        assert!(!dump.timestamp.is_empty());
    }

    #[test]
    fn test_failure_dump_write() {
        let tmp = TempDir::new().unwrap();
        let artifact_dir = tmp.path().to_path_buf();

        // Create some test files in artifact dir
        fs::write(artifact_dir.join("stdout"), "test stdout output\n").unwrap();
        fs::write(artifact_dir.join("stderr"), "test stderr output\n").unwrap();

        let dump = FailureDump::new("test_write", "unit");
        let result = dump.write(&artifact_dir);

        assert!(result.is_ok(), "FailureDump::write should succeed");

        // Check that the dump file was created
        let dump_dir = FailureDump::dump_dir().unwrap();
        let entries: Vec<_> = fs::read_dir(&dump_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("unit_test_write_"))
            .collect();

        assert!(
            !entries.is_empty(),
            "Should have created a dump file matching pattern"
        );

        // Read the dump and verify content
        let dump_content = fs::read_to_string(entries[0].path()).unwrap();
        assert!(dump_content.contains("FAILURE STATE DUMP"));
        assert!(dump_content.contains("Test: unit::test_write"));
        assert!(dump_content.contains("=== ENVIRONMENT ==="));
        assert!(dump_content.contains("=== TEMP DIRECTORY LISTING ==="));
        assert!(dump_content.contains("=== LOG TAIL"));
        assert!(dump_content.contains("=== GIT STATE ==="));
        assert!(dump_content.contains("=== PROCESS INFO ==="));

        // Clean up
        let _ = fs::remove_file(entries[0].path());
    }

    #[test]
    fn test_failure_dump_standalone_function() {
        let tmp = TempDir::new().unwrap();
        let artifact_dir = tmp.path();

        let result = dump_failure_state("standalone_test", "integration", artifact_dir);
        assert!(result.is_ok(), "dump_failure_state should succeed");

        let dump_path = result.unwrap();
        assert!(
            dump_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("integration_standalone_test_"),
            "Dump file should have expected naming pattern"
        );

        // Clean up
        let _ = fs::remove_file(&dump_path);
    }

    #[test]
    fn test_failure_dump_captures_log_tail() {
        let tmp = TempDir::new().unwrap();
        let artifact_dir = tmp.path();

        // Create a log file with >100 lines
        let log_content: String = (0..150).map(|i| format!("Log line {}\n", i)).collect();
        fs::write(artifact_dir.join("cass.log"), &log_content).unwrap();

        let dump = FailureDump::new("log_tail_test", "unit");
        dump.write(artifact_dir).unwrap();

        let dump_dir = FailureDump::dump_dir().unwrap();
        let entries: Vec<_> = fs::read_dir(&dump_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("unit_log_tail_test_")
            })
            .collect();

        let dump_content = fs::read_to_string(entries[0].path()).unwrap();

        // Should contain the last lines (50-149) but not the first lines (0-49)
        assert!(dump_content.contains("Log line 149"));
        assert!(dump_content.contains("Log line 100"));
        // First 50 lines should be truncated
        assert!(!dump_content.contains("Log line 0\n"));

        // Clean up
        let _ = fs::remove_file(entries[0].path());
    }

    // ==================== Metric & Baseline tests ====================

    #[test]
    fn test_emit_metric_without_file() {
        // emit_metric should not panic even without initialized file
        let result = emit_metric("test_metric", 42.5, "ms");
        assert!(result.is_ok());
    }

    #[test]
    fn test_emit_metric_with_file() {
        let tmp = TempDir::new().unwrap();
        let metric_path = tmp.path().join("metrics.jsonl");

        init_metrics_file(&metric_path).unwrap();
        emit_metric("indexing_duration_ms", 123.45, "ms").unwrap();
        emit_metric("memory_peak_kb", 50000.0, "KB").unwrap();

        // Reset the global file for other tests
        {
            let mut guard = METRIC_FILE.lock().unwrap();
            *guard = None;
        }

        // Verify content
        let content = fs::read_to_string(&metric_path).unwrap();
        assert!(content.contains("indexing_duration_ms"));
        assert!(content.contains("123.45"));
        assert!(content.contains("memory_peak_kb"));
        assert!(content.contains("50000"));
    }

    #[test]
    fn test_baseline_save_load_roundtrip() {
        // Use a temporary baselines file
        let tmp = TempDir::new().unwrap();
        let baseline_path = tmp.path().join("baselines.json");

        // Create test baselines
        let mut baselines = HashMap::new();
        baselines.insert(
            "test_metric".to_string(),
            MetricBaseline {
                name: "test_metric".to_string(),
                value: 100.0,
                unit: "ms".to_string(),
                timestamp: "2026-01-27T00:00:00Z".to_string(),
                commit: Some("abc123".to_string()),
            },
        );

        // Write directly to test path
        let values: Vec<_> = baselines.values().cloned().collect();
        let content = serde_json::to_string_pretty(&values).unwrap();
        fs::write(&baseline_path, &content).unwrap();

        // Read back
        let loaded: Vec<MetricBaseline> =
            serde_json::from_str(&fs::read_to_string(&baseline_path).unwrap()).unwrap();
        let loaded_map: HashMap<_, _> = loaded.into_iter().map(|b| (b.name.clone(), b)).collect();

        assert_eq!(loaded_map.len(), 1);
        let loaded_metric = loaded_map.get("test_metric").unwrap();
        assert_eq!(loaded_metric.value, 100.0);
        assert_eq!(loaded_metric.unit, "ms");
    }

    #[test]
    fn test_compare_to_baseline_no_regression() {
        // When baseline doesn't exist, no regression is detected
        let comparison = compare_to_baseline("nonexistent_metric", 100.0, "ms").unwrap();
        assert!(!comparison.is_regression);
        assert_eq!(comparison.current, 100.0);
        assert_eq!(comparison.baseline, 100.0); // Falls back to current value
    }

    #[test]
    fn test_standard_metrics_constants() {
        // Verify standard metric names are defined
        assert_eq!(
            standard_metrics::INDEXING_DURATION_MS,
            "indexing_duration_ms"
        );
        assert_eq!(
            standard_metrics::SEARCH_LATENCY_P50_MS,
            "search_latency_p50_ms"
        );
        assert_eq!(
            standard_metrics::SEARCH_LATENCY_P99_MS,
            "search_latency_p99_ms"
        );
        assert_eq!(standard_metrics::MEMORY_PEAK_KB, "memory_peak_kb");
        assert_eq!(standard_metrics::INDEX_SIZE_BYTES, "index_size_bytes");
        assert_eq!(standard_metrics::FILES_PROCESSED, "files_processed");
        assert_eq!(standard_metrics::QUERIES_PER_SECOND, "queries_per_second");
    }
}
