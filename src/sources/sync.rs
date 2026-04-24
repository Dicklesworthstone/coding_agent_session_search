//! Sync engine for pulling agent sessions from remote sources.
//!
//! This module provides the core sync functionality using rsync over SSH
//! for efficient delta transfers, with progress reporting and error recovery.
//!
//! # Safety
//!
//! **IMPORTANT**: The sync engine uses rsync WITHOUT the `--delete` flag
//! to ensure safe additive syncs. This prevents accidental data loss if
//! a remote is misconfigured or temporarily empty.
//!
//! # Example
//!
//! ```rust,ignore
//! use coding_agent_search::sources::sync::SyncEngine;
//! use coding_agent_search::sources::config::SourcesConfig;
//!
//! let config = SourcesConfig::load()?;
//! let engine = SyncEngine::new(&data_dir);
//!
//! for source in config.remote_sources() {
//!     let report = engine.sync_source(source)?;
//!     println!("Synced {}: {} files", source.name, report.total_files());
//! }
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Instant;

use thiserror::Error;

use super::{
    config::{SourceDefinition, discover_ssh_hosts},
    host_key_verification_error, is_host_key_verification_failure, strict_ssh_cli_tokens,
    strict_ssh_command_for_rsync,
};
use ssh2::{FileStat, Session, Sftp};
use std::io::{Read as IoRead, Write as IoWrite};
use std::net::{Shutdown, TcpStream};

/// Which variant of rsync's "pass args protected to the remote" flag the
/// system `rsync` accepts. The flag was introduced in rsync 3.0.0 as
/// `--protect-args`; rsync 3.4.0 renamed the primary form to
/// `--secluded-args` (`-s`) and current Homebrew `rsync 3.4.1` prints only
/// the new name in `--help`, so a simple substring probe for `--protect-args`
/// mis-classifies it as unsupported and falls through to the quoted-path
/// rsync branch — which breaks (#191). openrsync (macOS 15+) supports
/// neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RsyncArgProtection {
    /// Neither flag supported — callers must manually quote remote paths for
    /// the remote login shell.
    None,
    /// rsync 3.0.0..3.4.0 — original flag name.
    ProtectArgs,
    /// rsync 3.4.0+ (incl. Homebrew 3.4.1) — renamed primary form.
    SecludedArgs,
}

impl RsyncArgProtection {
    fn is_supported(self) -> bool {
        !matches!(self, Self::None)
    }

    /// CLI flag to pass to rsync, or `None` if no protection variant is
    /// available.
    fn flag(self) -> Option<&'static str> {
        match self {
            Self::ProtectArgs => Some("--protect-args"),
            Self::SecludedArgs => Some("--secluded-args"),
            Self::None => None,
        }
    }
}

fn detect_rsync_arg_protection() -> RsyncArgProtection {
    static CACHED: OnceLock<RsyncArgProtection> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let Some(out) = Command::new("rsync").arg("--help").output().ok() else {
            return RsyncArgProtection::None;
        };
        // rsync prints to stdout on GNU/Linux and Homebrew macOS, but some
        // forks / older builds print help on stderr — check both so we never
        // misclassify a supported rsync as unsupported.
        let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
        // Prefer the newer name when both are listed (forward-compat with a
        // hypothetical rsync that keeps both as aliases): `--secluded-args`
        // is what current rsync actually prints in help output, and using
        // the printed name is the one guaranteed to be accepted.
        if combined.contains("--secluded-args") {
            RsyncArgProtection::SecludedArgs
        } else if combined.contains("--protect-args") {
            RsyncArgProtection::ProtectArgs
        } else {
            RsyncArgProtection::None
        }
    })
}

fn quote_remote_shell_path(path: &str) -> String {
    // POSIX shell single-quote escape:
    // 1. Wrap the whole thing in single quotes.
    // 2. Escape existing single quotes by closing the current quote,
    //    inserting a backslash-escaped quote, and opening a new one.
    // Result: 'foo'\''bar'
    format!("'{}'", path.replace('\'', r#"'\''"#))
}

fn remote_spec_for_shell_bound_copy(host: &str, remote_path: &str) -> String {
    // host itself might contain user@ or be an alias, but we should not quote it
    // if it's already a single token. However, if it contains spaces or other
    // weirdness it's already broken for SSH. We focus on the path part.
    format!("{host}:{}", quote_remote_shell_path(remote_path))
}

fn remote_spec_for_scp(host: &str, remote_path: &str) -> String {
    format!("{host}:{remote_path}")
}

fn remote_spec_for_rsync(host: &str, remote_path: &str, protect_args_supported: bool) -> String {
    if protect_args_supported {
        // With --protect-args, rsync handles its own escaping over the wire
        remote_spec_for_scp(host, remote_path)
    } else {
        // Without it (e.g. openrsync), we must manually quote for the remote shell
        remote_spec_for_shell_bound_copy(host, remote_path)
    }
}

fn remote_find_regular_files_command(remote_path: &str) -> String {
    format!(
        "find -P {} -type f -print0",
        quote_remote_shell_path(remote_path)
    )
}

fn parse_null_terminated_utf8_paths(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .filter_map(|part| std::str::from_utf8(part).ok())
        .map(ToOwned::to_owned)
        .collect()
}

fn remote_file_to_safe_local_path(
    remote_root: &Path,
    remote_file: &Path,
    local_container: &Path,
    leaf_name: &str,
) -> Option<PathBuf> {
    let mut local_path = local_container.join(leaf_name);
    if remote_file == remote_root {
        return Some(local_path);
    }

    let relative = remote_file.strip_prefix(remote_root).ok()?;
    for component in relative.components() {
        match component {
            std::path::Component::Normal(name) => local_path.push(name),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }

    Some(local_path)
}

fn sftp_file_stat_is_symlink(stat: &FileStat) -> bool {
    stat.file_type().is_symlink()
}

/// Errors that can occur during sync operations.
#[derive(Error, Debug)]
pub enum SyncError {
    #[error("Source has no host configured")]
    NoHost,

    #[error("Source has no paths configured")]
    NoPaths,

    #[error("rsync command failed: {0}")]
    RsyncFailed(String),

    #[error("Failed to create local directory: {0}")]
    CreateDirFailed(#[from] std::io::Error),

    #[error("SSH connection failed: {0}")]
    SshFailed(String),

    #[error("Connection timed out after {0} seconds")]
    Timeout(u64),

    #[error("Sync cancelled")]
    Cancelled,
}

/// Method used for syncing files from remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMethod {
    /// rsync over SSH - preferred for delta transfers
    Rsync,
    /// rsync invoked via WSL (`wsl rsync`) - used on Windows when native rsync is unavailable
    /// but WSL is installed with rsync available inside it.
    WslRsync,
    /// SCP-based transfer using the system `scp` command.
    ///
    /// Used on Windows (and other platforms) when rsync is unavailable. Delegates all
    /// authentication to the system `ssh`/`scp` binary so it inherits OpenSSH agent,
    /// `~/.ssh/` keys, and `~/.ssh/config` correctly – avoiding the `ssh2` library
    /// which does not integrate with the Windows OpenSSH agent.
    Scp,
    /// SFTP fallback using the `ssh2` crate – last resort only.
    ///
    /// Deprecated in favour of [`SyncMethod::Scp`] which uses the native system SSH
    /// binary. Kept for backward compatibility with callers that pattern-match on this
    /// variant.
    Sftp,
}

impl std::fmt::Display for SyncMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rsync => write!(f, "rsync"),
            Self::WslRsync => write!(f, "wsl-rsync"),
            Self::Scp => write!(f, "scp"),
            Self::Sftp => write!(f, "sftp"),
        }
    }
}

/// Result of syncing a single path.
#[derive(Debug, Clone, Default)]
pub struct PathSyncResult {
    /// Remote path that was synced.
    pub remote_path: String,
    /// Local destination path.
    pub local_path: PathBuf,
    /// Number of files transferred.
    pub files_transferred: u64,
    /// Total bytes transferred.
    pub bytes_transferred: u64,
    /// Whether the sync succeeded.
    pub success: bool,
    /// Error message if sync failed.
    pub error: Option<String>,
    /// Duration of the sync operation.
    pub duration_ms: u64,
}

/// Report from syncing an entire source.
#[derive(Debug, Clone)]
pub struct SyncReport {
    /// Name of the source that was synced.
    pub source_name: String,
    /// Method used for syncing.
    pub method: SyncMethod,
    /// Results for each path.
    pub path_results: Vec<PathSyncResult>,
    /// Total duration of the sync.
    pub total_duration_ms: u64,
    /// Whether all paths synced successfully.
    pub all_succeeded: bool,
}

impl SyncReport {
    /// Create a new report for a source.
    pub fn new(source_name: impl Into<String>, method: SyncMethod) -> Self {
        Self {
            source_name: source_name.into(),
            method,
            path_results: Vec::new(),
            total_duration_ms: 0,
            all_succeeded: true,
        }
    }

    /// Create a failed report when sync couldn't even start.
    pub fn failed(source_name: impl Into<String>, error: SyncError) -> Self {
        Self {
            source_name: source_name.into(),
            method: SyncMethod::Rsync,
            path_results: vec![PathSyncResult {
                error: Some(error.to_string()),
                success: false,
                ..Default::default()
            }],
            total_duration_ms: 0,
            all_succeeded: false,
        }
    }

    /// Add a path result to the report.
    pub fn add_path_result(&mut self, result: PathSyncResult) {
        if !result.success {
            self.all_succeeded = false;
        }
        self.path_results.push(result);
    }

    /// Get total files transferred across all paths.
    pub fn total_files(&self) -> u64 {
        self.path_results.iter().map(|r| r.files_transferred).sum()
    }

    /// Get total bytes transferred across all paths.
    pub fn total_bytes(&self) -> u64 {
        self.path_results.iter().map(|r| r.bytes_transferred).sum()
    }

    /// Get count of successful path syncs.
    pub fn successful_paths(&self) -> usize {
        self.path_results.iter().filter(|r| r.success).count()
    }

    /// Get count of failed path syncs.
    pub fn failed_paths(&self) -> usize {
        self.path_results.iter().filter(|r| !r.success).count()
    }

    /// Summarize the overall sync outcome.
    pub fn sync_result(&self) -> SyncResult {
        if self.all_succeeded {
            SyncResult::Success
        } else {
            let errors: Vec<String> = self
                .path_results
                .iter()
                .filter_map(|r| r.error.clone())
                .collect();
            if self.successful_paths() > 0 {
                SyncResult::PartialFailure(errors.join("; "))
            } else {
                SyncResult::Failed(errors.join("; "))
            }
        }
    }
}

/// Statistics parsed from rsync output.
#[derive(Debug, Default)]
struct RsyncStats {
    files_transferred: u64,
    bytes_transferred: u64,
}

/// Sync engine for pulling sessions from remote sources.
pub struct SyncEngine {
    /// Base directory for storing synced data.
    /// Structure: `{local_store}/remotes/{source_name}/mirror/`
    local_store: PathBuf,
    /// Connection timeout in seconds.
    connection_timeout: u64,
    /// Transfer timeout in seconds (0 = no timeout).
    transfer_timeout: u64,
}

impl SyncEngine {
    /// Create a new sync engine.
    ///
    /// # Arguments
    /// * `data_dir` - The cass data directory (e.g., ~/.local/share/coding-agent-search)
    pub fn new(data_dir: &Path) -> Self {
        Self {
            local_store: data_dir.to_path_buf(),
            connection_timeout: 10,
            transfer_timeout: 300, // 5 minutes
        }
    }

    /// Set the connection timeout.
    pub fn with_connection_timeout(mut self, seconds: u64) -> Self {
        self.connection_timeout = seconds;
        self
    }

    /// Set the transfer timeout.
    pub fn with_transfer_timeout(mut self, seconds: u64) -> Self {
        self.transfer_timeout = seconds;
        self
    }

    /// Get the local mirror directory for a source.
    pub fn mirror_dir(&self, source_name: &str) -> PathBuf {
        self.local_store
            .join("remotes")
            .join(source_name)
            .join("mirror")
    }

    /// Get the remote home directory by SSH-ing to the host and running `echo $HOME`.
    ///
    /// This is called once per source sync to avoid repeated SSH calls for each path.
    fn get_remote_home(&self, host: &str) -> Result<String, SyncError> {
        // Validate host doesn't contain shell metacharacters to prevent injection
        if host
            .chars()
            .any(|c| !c.is_alphanumeric() && c != '.' && c != '-' && c != '_' && c != '@')
        {
            return Err(SyncError::SshFailed(format!(
                "Invalid characters in host: {}",
                host
            )));
        }

        let output = Command::new("ssh")
            .args(strict_ssh_cli_tokens(self.connection_timeout))
            .arg("--")
            .arg(host)
            .arg("echo $HOME")
            .output()
            .map_err(|e| SyncError::SshFailed(format!("Failed to execute ssh: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if is_host_key_verification_failure(&stderr) {
                return Err(SyncError::SshFailed(host_key_verification_error(host)));
            }
            return Err(SyncError::SshFailed(format!(
                "Failed to get remote home directory: {}",
                stderr.trim()
            )));
        }

        let remote_home = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if remote_home.is_empty() {
            return Err(SyncError::SshFailed(
                "Remote home directory is empty".to_string(),
            ));
        }

        tracing::debug!(host = %host, remote_home = %remote_home, "got remote home directory");
        Ok(remote_home)
    }

    /// Expand ~ in a remote path using the provided home directory.
    ///
    /// If `remote_home` is None, returns the path unchanged.
    fn expand_tilde_with_home(path: &str, remote_home: Option<&str>) -> String {
        if !path.starts_with('~') {
            return path.to_string();
        }

        let Some(home) = remote_home else {
            return path.to_string();
        };

        if path == "~" {
            home.to_string()
        } else if let Some(rest) = path.strip_prefix("~/") {
            format!("{}/{}", home, rest)
        } else {
            // ~user/path case - not supported, return as-is
            path.to_string()
        }
    }

    /// Detect the available sync method.
    ///
    /// Detection order:
    /// 1. Native `rsync` → [`SyncMethod::Rsync`]
    /// 2. `wsl rsync` (Windows only) → [`SyncMethod::WslRsync`]
    /// 3. System `scp` available → [`SyncMethod::Scp`]
    /// 4. Last resort → [`SyncMethod::Sftp`] (ssh2-based, no native-agent integration)
    ///
    /// On Windows the `ssh2` SFTP path is intentionally avoided whenever possible
    /// because it bypasses the Windows OpenSSH agent and `~/.ssh/config`, leading to
    /// "No valid authentication method found" errors even when SSH keys are properly
    /// configured. Using the system `scp` binary instead lets OpenSSH handle auth the
    /// same way `ssh` and `cass sources doctor` do.
    pub fn detect_sync_method() -> SyncMethod {
        // 1. Native rsync
        if Command::new("rsync")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return SyncMethod::Rsync;
        }

        // 2. WSL rsync (Windows-only: rsync inside WSL invoked via `wsl rsync`)
        #[cfg(target_os = "windows")]
        if Command::new("wsl")
            .args(["rsync", "--version"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return SyncMethod::WslRsync;
        }

        // 3. System scp – preferred over ssh2/SFTP because it inherits the native
        //    OpenSSH agent and ~/.ssh/config on all platforms (especially Windows).
        if Command::new("scp")
            .arg("-S")
            .arg("ssh")
            .arg("--")
            // pass a harmless flag; scp prints usage and exits non-zero, but if the
            // binary exists the spawn itself succeeds which is all we need to check.
            .output()
            .is_ok()
        {
            // Confirm scp is a real binary by checking for the executable
            if which_scp_exists() {
                return SyncMethod::Scp;
            }
        }

        // 4. Last resort: ssh2-based SFTP
        SyncMethod::Sftp
    }

    /// Sync a single source.
    ///
    /// Syncs all configured paths from the source to the local mirror directory.
    /// Individual path failures don't abort the entire sync.
    pub fn sync_source(&self, source: &SourceDefinition) -> Result<SyncReport, SyncError> {
        if !source.is_remote() {
            return Err(SyncError::NoHost);
        }

        let host = source.host.as_ref().ok_or(SyncError::NoHost)?;

        if source.paths.is_empty() {
            return Err(SyncError::NoPaths);
        }

        let method = Self::detect_sync_method();
        let mut report = SyncReport::new(&source.name, method);
        let overall_start = Instant::now();

        // Create the mirror directory
        let mirror_dir = self.mirror_dir(&source.name);
        std::fs::create_dir_all(&mirror_dir)?;

        // Pre-fetch remote home directory if any paths use tilde (avoids multiple SSH calls)
        let remote_home = if source.paths.iter().any(|p| p.starts_with('~')) {
            match self.get_remote_home(host) {
                Ok(home) => Some(home),
                Err(e) => {
                    tracing::warn!(host = %host, error = %e, "Failed to get remote home directory");
                    None
                }
            }
        } else {
            None
        };

        for remote_path in &source.paths {
            let result = match method {
                SyncMethod::Rsync => {
                    self.sync_path_rsync(host, remote_path, &mirror_dir, remote_home.as_deref())
                }
                SyncMethod::WslRsync => {
                    self.sync_path_wsl_rsync(host, remote_path, &mirror_dir, remote_home.as_deref())
                }
                SyncMethod::Scp => {
                    self.sync_path_scp(host, remote_path, &mirror_dir, remote_home.as_deref())
                }
                SyncMethod::Sftp => {
                    self.sync_path_sftp(host, remote_path, &mirror_dir, remote_home.as_deref())
                }
            };
            report.add_path_result(result);
        }

        report.total_duration_ms = overall_start.elapsed().as_millis() as u64;
        Ok(report)
    }

    /// Sync all remote sources from a config.
    ///
    /// Continues even if individual sources fail.
    pub fn sync_all(
        &self,
        sources: impl Iterator<Item = impl std::borrow::Borrow<SourceDefinition>>,
    ) -> Vec<SyncReport> {
        sources
            .map(|source| {
                let source = source.borrow();
                self.sync_source(source)
                    .unwrap_or_else(|e| SyncReport::failed(&source.name, e))
            })
            .collect()
    }

    /// Sync a single path using rsync.
    ///
    /// **IMPORTANT**: Uses rsync WITHOUT --delete for safe additive syncs.
    ///
    /// The `remote_home` parameter should be pre-fetched via `get_remote_home()` to avoid
    /// repeated SSH calls for each path.
    fn sync_path_rsync(
        &self,
        host: &str,
        remote_path: &str,
        dest_dir: &Path,
        remote_home: Option<&str>,
    ) -> PathSyncResult {
        let start = Instant::now();
        if remote_path.starts_with('~') && remote_home.is_none() {
            let local_path = dest_dir.join(path_to_safe_dirname(remote_path));
            return PathSyncResult {
                remote_path: remote_path.to_string(),
                local_path,
                success: false,
                error: Some(
                    "Cannot expand '~' in remote path; failed to determine remote home directory"
                        .to_string(),
                ),
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            };
        }

        // Expand ~ using pre-fetched home directory (no SSH call here)
        let expanded_path = Self::expand_tilde_with_home(remote_path, remote_home);

        // If tilde expansion failed (no remote_home provided), log a warning
        if remote_path.starts_with('~') && expanded_path == remote_path {
            tracing::warn!(
                remote_path = %remote_path,
                "Could not expand tilde in path (remote home directory not available)"
            );
        }

        // Convert remote path to safe local directory name
        // Use raw remote_path for stability (independent of home expansion success)
        let safe_name = path_to_safe_dirname(remote_path);
        let local_path = dest_dir.join(&safe_name);

        // Create local directory
        if let Err(e) = std::fs::create_dir_all(&local_path) {
            return PathSyncResult {
                remote_path: remote_path.to_string(),
                local_path: local_path.clone(),
                success: false,
                error: Some(format!("Failed to create directory: {}", e)),
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            };
        }

        // Build rsync command
        // NOTE: NO --delete flag! Safe additive sync only.
        let arg_protection = detect_rsync_arg_protection();
        let protect_args_supported = arg_protection.is_supported();
        let remote_spec = remote_spec_for_rsync(host, &expanded_path, protect_args_supported);
        let ssh_opts = strict_ssh_command_for_rsync(self.connection_timeout);

        let local_path_str = match local_path.to_str() {
            Some(s) => s,
            None => {
                return PathSyncResult {
                    remote_path: remote_path.to_string(),
                    local_path,
                    success: false,
                    error: Some("Local path contains invalid UTF-8".to_string()),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }
        };

        let timeout_str = self.transfer_timeout.to_string();
        let mut cmd = Command::new("rsync");
        cmd.args(["-avz", "--links", "--safe-links", "--stats", "--partial"]);
        if let Some(flag) = arg_protection.flag() {
            cmd.arg(flag);
        }
        cmd.args([
            "--timeout",
            &timeout_str,
            "-e",
            &ssh_opts,
            "--",
            &remote_spec,
            local_path_str,
        ]);

        tracing::debug!(
            host = %host,
            remote_path = %expanded_path,
            local_path = %local_path.display(),
            "starting rsync"
        );

        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                return PathSyncResult {
                    remote_path: remote_path.to_string(),
                    local_path,
                    success: false,
                    error: Some(format!("Failed to execute rsync: {}", e)),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            // Check for specific error types
            let error_msg = if stderr.contains("Connection refused")
                || stderr.contains("Connection timed out")
            {
                format!("SSH connection failed: {}", stderr.trim())
            } else if is_host_key_verification_failure(&stderr) {
                host_key_verification_error(host)
            } else if stderr.contains("No such file or directory") {
                format!("Remote path not found: {}", expanded_path)
            } else if stderr.contains("Permission denied") {
                format!("Permission denied: {}", stderr.trim())
            } else {
                format!("rsync failed: {}", stderr.trim())
            };

            tracing::warn!(
                host = %host,
                remote_path = %expanded_path,
                error = %error_msg,
                "rsync failed"
            );

            return PathSyncResult {
                remote_path: remote_path.to_string(),
                local_path,
                success: false,
                error: Some(error_msg),
                duration_ms,
                ..Default::default()
            };
        }

        // Parse stats from rsync output
        let stats = parse_rsync_stats(&stdout);

        tracing::info!(
            host = %host,
            remote_path = %expanded_path,
            files = stats.files_transferred,
            bytes = stats.bytes_transferred,
            duration_ms,
            "rsync completed"
        );

        PathSyncResult {
            remote_path: remote_path.to_string(),
            local_path,
            files_transferred: stats.files_transferred,
            bytes_transferred: stats.bytes_transferred,
            success: true,
            error: None,
            duration_ms,
        }
    }

    /// Sync a single path using rsync invoked through WSL (`wsl rsync …`).
    ///
    /// Used on Windows when native rsync is absent but WSL with rsync is available.
    /// WSL paths use the `\\wsl$\…` UNC convention for the local destination.
    fn sync_path_wsl_rsync(
        &self,
        host: &str,
        remote_path: &str,
        dest_dir: &Path,
        remote_home: Option<&str>,
    ) -> PathSyncResult {
        let start = Instant::now();

        if remote_path.starts_with('~') && remote_home.is_none() {
            let local_path = dest_dir.join(path_to_safe_dirname(remote_path));
            return PathSyncResult {
                remote_path: remote_path.to_string(),
                local_path,
                success: false,
                error: Some(
                    "Cannot expand '~' in remote path; failed to determine remote home directory"
                        .to_string(),
                ),
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            };
        }

        let expanded_path = Self::expand_tilde_with_home(remote_path, remote_home);
        let safe_name = path_to_safe_dirname(remote_path);
        let local_path = dest_dir.join(&safe_name);

        if let Err(e) = std::fs::create_dir_all(&local_path) {
            return PathSyncResult {
                remote_path: remote_path.to_string(),
                local_path,
                success: false,
                error: Some(format!("Failed to create directory: {}", e)),
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            };
        }

        let local_path_str = match local_path.to_str() {
            Some(s) => s,
            None => {
                return PathSyncResult {
                    remote_path: remote_path.to_string(),
                    local_path,
                    success: false,
                    error: Some("Local path contains invalid UTF-8".to_string()),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }
        };

        // Convert Windows path to a WSL-accessible path.
        // WSL can access Windows paths via /mnt/<drive>/... conventions.
        // E.g. C:\Users\george\AppData\... → /mnt/c/Users/george/AppData/...
        let wsl_dest = windows_path_to_wsl(local_path_str);

        let remote_spec = remote_spec_for_rsync(host, &expanded_path, true);
        let ssh_opts = strict_ssh_command_for_rsync(self.connection_timeout);
        let timeout_str = self.transfer_timeout.to_string();

        let mut cmd = Command::new("wsl");
        cmd.args([
            "rsync",
            "-avz",
            "--links",
            "--safe-links",
            "--stats",
            "--partial",
        ]);
        // WSL rsync is the real rsync (not openrsync), so --protect-args is safe.
        cmd.arg("--protect-args");
        cmd.args([
            "--timeout",
            &timeout_str,
            "-e",
            &ssh_opts,
            "--",
            &remote_spec,
            &wsl_dest,
        ]);

        tracing::debug!(
            host = %host,
            remote_path = %expanded_path,
            local_path = %local_path.display(),
            wsl_dest = %wsl_dest,
            "starting wsl rsync"
        );

        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                return PathSyncResult {
                    remote_path: remote_path.to_string(),
                    local_path,
                    success: false,
                    error: Some(format!("Failed to execute wsl rsync: {}", e)),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            let error_msg = if stderr.contains("Connection refused")
                || stderr.contains("Connection timed out")
            {
                format!("SSH connection failed: {}", stderr.trim())
            } else if is_host_key_verification_failure(&stderr) {
                host_key_verification_error(host)
            } else if stderr.contains("No such file or directory") {
                format!("Remote path not found: {}", expanded_path)
            } else if stderr.contains("Permission denied") {
                format!("Permission denied: {}", stderr.trim())
            } else {
                format!("wsl rsync failed: {}", stderr.trim())
            };

            tracing::warn!(
                host = %host,
                remote_path = %expanded_path,
                error = %error_msg,
                "wsl rsync failed"
            );

            return PathSyncResult {
                remote_path: remote_path.to_string(),
                local_path,
                success: false,
                error: Some(error_msg),
                duration_ms,
                ..Default::default()
            };
        }

        let stats = parse_rsync_stats(&stdout);

        tracing::info!(
            host = %host,
            remote_path = %expanded_path,
            files = stats.files_transferred,
            bytes = stats.bytes_transferred,
            duration_ms,
            "wsl rsync completed"
        );

        PathSyncResult {
            remote_path: remote_path.to_string(),
            local_path,
            files_transferred: stats.files_transferred,
            bytes_transferred: stats.bytes_transferred,
            success: true,
            error: None,
            duration_ms,
        }
    }

    /// Sync a single path using SCP after a physical `find -P` regular-file listing.
    ///
    /// This method delegates all authentication to the native system `scp`/`ssh`
    /// binary, which correctly reads `~/.ssh/config`, the OpenSSH agent (including
    /// the Windows OpenSSH agent on Windows), and all standard key locations.
    ///
    /// This avoids the "No valid authentication method found" failure that occurs
    /// in the `ssh2`-based SFTP path on Windows, where the library does not
    /// integrate with the Windows OpenSSH agent (`ssh-agent.exe`).
    fn sync_path_scp(
        &self,
        host: &str,
        remote_path: &str,
        dest_dir: &Path,
        remote_home: Option<&str>,
    ) -> PathSyncResult {
        let start = Instant::now();

        if remote_path.starts_with('~') && remote_home.is_none() {
            let local_path = dest_dir.join(path_to_safe_dirname(remote_path));
            return PathSyncResult {
                remote_path: remote_path.to_string(),
                local_path,
                success: false,
                error: Some(
                    "Cannot expand '~' in remote path; failed to determine remote home directory"
                        .to_string(),
                ),
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            };
        }

        let expanded_path = Self::expand_tilde_with_home(remote_path, remote_home);
        let safe_name = path_to_safe_dirname(remote_path);
        let local_path = dest_dir.join(&safe_name);

        if let Err(e) = std::fs::create_dir_all(&local_path) {
            return PathSyncResult {
                remote_path: remote_path.to_string(),
                local_path,
                success: false,
                error: Some(format!("Failed to create directory: {}", e)),
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            };
        }

        // `scp -r` follows symlinks on some OpenSSH paths. Enumerate only regular
        // files with physical traversal first, then copy those files individually.
        let connect_timeout = self.connection_timeout.to_string();
        let find_command = remote_find_regular_files_command(&expanded_path);

        tracing::debug!(
            host = %host,
            remote_path = %expanded_path,
            local_path = %local_path.display(),
            "listing regular files for scp sync"
        );

        let output = match Command::new("ssh")
            .args(strict_ssh_cli_tokens(self.connection_timeout))
            .arg("--")
            .arg(host)
            .arg(&find_command)
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                return PathSyncResult {
                    remote_path: remote_path.to_string(),
                    local_path,
                    success: false,
                    error: Some(format!("Failed to execute ssh file listing: {}", e)),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }
        };

        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            let error_msg = if stderr.contains("Connection refused")
                || stderr.contains("Connection timed out")
            {
                format!("SSH connection failed: {}", stderr.trim())
            } else if is_host_key_verification_failure(&stderr) {
                host_key_verification_error(host)
            } else if stderr.contains("No such file or directory") {
                format!("Remote path not found: {}", expanded_path)
            } else if stderr.contains("Permission denied") {
                format!("Permission denied: {}", stderr.trim())
            } else {
                format!("Remote file listing failed: {}", stderr.trim())
            };

            tracing::warn!(
                host = %host,
                remote_path = %expanded_path,
                error = %error_msg,
                "scp file listing failed"
            );

            return PathSyncResult {
                remote_path: remote_path.to_string(),
                local_path,
                success: false,
                error: Some(error_msg),
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            };
        }

        let remote_files = parse_null_terminated_utf8_paths(&output.stdout);
        let remote_root = Path::new(&expanded_path);
        let leaf_name = Path::new(remote_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("remote");
        let mut files_transferred = 0u64;
        let mut bytes_transferred = 0u64;

        for remote_file in remote_files {
            let remote_file_path = Path::new(&remote_file);
            let Some(local_file) = remote_file_to_safe_local_path(
                remote_root,
                remote_file_path,
                &local_path,
                leaf_name,
            ) else {
                tracing::warn!(
                    remote_path = %remote_file,
                    root = %expanded_path,
                    "skipping scp file outside listed root"
                );
                continue;
            };

            if let Some(parent) = local_file.parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                return PathSyncResult {
                    remote_path: remote_path.to_string(),
                    local_path,
                    success: false,
                    error: Some(format!("Failed to create {}: {}", parent.display(), e)),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }

            let Some(local_file_str) = local_file.to_str() else {
                return PathSyncResult {
                    remote_path: remote_path.to_string(),
                    local_path,
                    success: false,
                    error: Some("Local path contains invalid UTF-8".to_string()),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            };

            let remote_spec = remote_spec_for_scp(host, &remote_file);
            let mut cmd = Command::new("scp");
            cmd.args([
                "-B",
                "-o",
                &format!("ConnectTimeout={}", connect_timeout),
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "StrictHostKeyChecking=yes",
                "--",
                &remote_spec,
                local_file_str,
            ]);

            let output = match cmd.output() {
                Ok(o) => o,
                Err(e) => {
                    return PathSyncResult {
                        remote_path: remote_path.to_string(),
                        local_path,
                        success: false,
                        error: Some(format!("Failed to execute scp: {}", e)),
                        duration_ms: start.elapsed().as_millis() as u64,
                        ..Default::default()
                    };
                }
            };

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let error_msg = if is_host_key_verification_failure(&stderr) {
                    host_key_verification_error(host)
                } else if stderr.contains("Permission denied") {
                    format!("Permission denied: {}", stderr.trim())
                } else {
                    format!("scp failed: {}", stderr.trim())
                };

                tracing::warn!(
                    host = %host,
                    remote_path = %remote_file,
                    error = %error_msg,
                    "scp file transfer failed"
                );

                return PathSyncResult {
                    remote_path: remote_path.to_string(),
                    local_path,
                    success: false,
                    error: Some(error_msg),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }

            files_transferred += 1;
            if let Ok(metadata) = std::fs::metadata(&local_file) {
                bytes_transferred = bytes_transferred.saturating_add(metadata.len());
            }
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            host = %host,
            remote_path = %expanded_path,
            files = files_transferred,
            bytes = bytes_transferred,
            duration_ms,
            "scp sync completed"
        );

        PathSyncResult {
            remote_path: remote_path.to_string(),
            local_path,
            files_transferred,
            bytes_transferred,
            success: true,
            error: None,
            duration_ms,
        }
    }

    /// Sync a single path using SFTP (fallback when rsync unavailable).
    ///
    /// Uses the ssh2 crate for SFTP transfers. Authenticates via SSH agent
    /// or key file from SSH config.
    fn sync_path_sftp(
        &self,
        host: &str,
        remote_path: &str,
        dest_dir: &Path,
        remote_home: Option<&str>,
    ) -> PathSyncResult {
        let start = Instant::now();
        if remote_path.starts_with('~') && remote_home.is_none() {
            let local_path = dest_dir.join(path_to_safe_dirname(remote_path));
            return PathSyncResult {
                remote_path: remote_path.to_string(),
                local_path,
                success: false,
                error: Some(
                    "Cannot expand '~' in remote path; failed to determine remote home directory"
                        .to_string(),
                ),
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            };
        }
        let expanded_path = Self::expand_tilde_with_home(remote_path, remote_home);
        // Use raw remote_path for stability (independent of home expansion success)
        let local_path = dest_dir.join(path_to_safe_dirname(remote_path));

        // Create local directory
        if let Err(e) = std::fs::create_dir_all(&local_path) {
            return PathSyncResult {
                remote_path: remote_path.to_string(),
                local_path,
                success: false,
                error: Some(format!("Failed to create directory: {}", e)),
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            };
        }

        // Parse host to extract user if present (user@host format)
        let (ssh_user, ssh_host) = parse_ssh_host(host);

        // Look up host in SSH config for connection details
        // First try matching by SSH config alias (Host line), then by actual hostname
        let ssh_config = discover_ssh_hosts();
        let host_config = ssh_config.iter().find(|h| h.name == ssh_host).or_else(|| {
            ssh_config
                .iter()
                .find(|h| h.hostname.as_deref() == Some(ssh_host))
        });

        // Determine connection parameters
        let hostname = host_config
            .and_then(|h| h.hostname.as_deref())
            .unwrap_or(ssh_host);
        let port = host_config.and_then(|h| h.port).unwrap_or(22);
        // Resolve username deterministically; never guess with a sentinel value.
        let normalize_username = |value: Option<String>| {
            value.and_then(|candidate| {
                let trimmed = candidate.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
        };
        let username = match normalize_username(ssh_user.map(|s| s.to_string()))
            .or_else(|| normalize_username(host_config.and_then(|h| h.user.clone())))
            .or_else(|| normalize_username(dotenvy::var("USER").ok()))
            .or_else(|| normalize_username(dotenvy::var("LOGNAME").ok()))
        {
            Some(user) => user,
            None => {
                return PathSyncResult {
                    remote_path: remote_path.to_string(),
                    local_path,
                    success: false,
                    error: Some(format!(
                        "Unable to determine SSH username for host '{}' (missing/blank user@host, SSH config user, USER, and LOGNAME)",
                        host
                    )),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }
        };
        let identity_file = host_config.and_then(|h| h.identity_file.as_deref());

        tracing::debug!(
            hostname = %hostname,
            port,
            username = %username,
            identity_file = ?identity_file,
            remote_path = %expanded_path,
            "SFTP connection parameters"
        );

        // Connect via TCP with connection timeout
        let conn_timeout = std::time::Duration::from_secs(self.connection_timeout);
        let addr = format!("{}:{}", hostname, port);
        let sock_addr: std::net::SocketAddr = match addr.parse().or_else(|_| {
            // Resolve hostname to socket address
            use std::net::ToSocketAddrs;
            (hostname, port)
                .to_socket_addrs()
                .ok()
                .and_then(|mut addrs| addrs.next())
                .ok_or(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "cannot resolve hostname",
                ))
        }) {
            Ok(a) => a,
            Err(e) => {
                return PathSyncResult {
                    remote_path: remote_path.to_string(),
                    local_path,
                    success: false,
                    error: Some(format!("DNS resolution failed for {hostname}:{port}: {e}")),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }
        };
        let tcp = match TcpStream::connect_timeout(&sock_addr, conn_timeout) {
            Ok(t) => t,
            Err(e) => {
                return PathSyncResult {
                    remote_path: remote_path.to_string(),
                    local_path,
                    success: false,
                    error: Some(format!(
                        "TCP connection failed to {}:{}: {}",
                        hostname, port, e
                    )),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }
        };

        // Set TCP read/write timeout (use transfer_timeout, not connection_timeout)
        let timeout = std::time::Duration::from_secs(self.transfer_timeout);
        if let Err(e) = tcp.set_read_timeout(Some(timeout)) {
            tracing::warn!("Failed to set TCP read timeout: {}", e);
        }
        if let Err(e) = tcp.set_write_timeout(Some(timeout)) {
            tracing::warn!("Failed to set TCP write timeout: {}", e);
        }
        let tcp_shutdown = tcp.try_clone().ok();

        // Create SSH session
        let mut session = match Session::new() {
            Ok(s) => s,
            Err(e) => {
                let _ = tcp.shutdown(Shutdown::Both);
                return PathSyncResult {
                    remote_path: remote_path.to_string(),
                    local_path,
                    success: false,
                    error: Some(format!("Failed to create SSH session: {}", e)),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }
        };

        session.set_tcp_stream(tcp);
        let close_connections = |session: &mut Session, reason: &str| {
            let _ = session.disconnect(None, reason, None);
            if let Some(stream) = tcp_shutdown.as_ref() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        };

        if let Err(e) = session.handshake() {
            close_connections(&mut session, "handshake failed");
            return PathSyncResult {
                remote_path: remote_path.to_string(),
                local_path,
                success: false,
                error: Some(format!("SSH handshake failed: {}", e)),
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            };
        }

        // Authenticate - try agent first, then key file
        if let Err(e) = self.authenticate_ssh(&session, &username, identity_file) {
            close_connections(&mut session, "authentication failed");
            return PathSyncResult {
                remote_path: remote_path.to_string(),
                local_path,
                success: false,
                error: Some(format!("SSH authentication failed: {}", e)),
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            };
        }

        // Open SFTP session
        let sftp = match session.sftp() {
            Ok(s) => s,
            Err(e) => {
                close_connections(&mut session, "sftp open failed");
                return PathSyncResult {
                    remote_path: remote_path.to_string(),
                    local_path,
                    success: false,
                    error: Some(format!("Failed to open SFTP session: {}", e)),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }
        };

        tracing::info!(
            host = %host,
            remote_path = %expanded_path,
            local_path = %local_path.display(),
            "starting SFTP sync"
        );

        // Recursively download the remote path
        let mut files_transferred = 0u64;
        let mut bytes_transferred = 0u64;

        // For consistency with rsync and scp, we should create a subdirectory
        // with the remote path's leaf name inside the container directory.
        let leaf_name = Path::new(remote_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("remote");
        let target_local_path = local_path.join(leaf_name);

        if let Err(e) = self.sftp_download_recursive(
            &sftp,
            Path::new(&expanded_path),
            &target_local_path,
            &mut files_transferred,
            &mut bytes_transferred,
        ) {
            close_connections(&mut session, "sftp download failed");
            return PathSyncResult {
                remote_path: remote_path.to_string(),
                local_path,
                files_transferred,
                bytes_transferred,
                success: false,
                error: Some(format!("SFTP download failed: {}", e)),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            host = %host,
            remote_path = %expanded_path,
            files = files_transferred,
            bytes = bytes_transferred,
            duration_ms,
            "SFTP sync completed"
        );

        close_connections(&mut session, "sync complete");
        PathSyncResult {
            remote_path: remote_path.to_string(),
            local_path,
            files_transferred,
            bytes_transferred,
            success: true,
            error: None,
            duration_ms,
        }
    }

    /// Authenticate SSH session using agent or key file.
    fn authenticate_ssh(
        &self,
        session: &Session,
        username: &str,
        identity_file: Option<&str>,
    ) -> Result<(), String> {
        // Try SSH agent first
        if let Ok(mut agent) = session.agent()
            && agent.connect().is_ok()
            && agent.list_identities().is_ok()
        {
            for identity in agent.identities().unwrap_or_default() {
                if agent.userauth(username, &identity).is_ok() && session.authenticated() {
                    tracing::debug!("Authenticated via SSH agent");
                    return Ok(());
                }
            }
        }

        // Try key file if specified
        if let Some(key_path) = identity_file {
            let key_path_expanded = expand_tilde_local(key_path);
            let key_path_buf = Path::new(&key_path_expanded);

            if key_path_buf.exists()
                && session
                    .userauth_pubkey_file(username, None, key_path_buf, None)
                    .is_ok()
                && session.authenticated()
            {
                tracing::debug!(key = %key_path_buf.display(), "Authenticated via key file");
                return Ok(());
            }
        }

        // Try default key locations
        if let Some(home) = dirs::home_dir() {
            for key_name in ["id_ed25519", "id_rsa", "id_ecdsa"] {
                let key_path = home.join(".ssh").join(key_name);
                if key_path.exists()
                    && session
                        .userauth_pubkey_file(username, None, &key_path, None)
                        .is_ok()
                    && session.authenticated()
                {
                    tracing::debug!(key = %key_path.display(), "Authenticated via default key");
                    return Ok(());
                }
            }
        }

        Err(format!(
            "No valid authentication method found for user '{}'",
            username
        ))
    }

    /// Recursively download a remote path via SFTP.
    fn sftp_download_recursive(
        &self,
        sftp: &Sftp,
        remote_path: &Path,
        local_path: &Path,
        files_transferred: &mut u64,
        bytes_transferred: &mut u64,
    ) -> Result<(), String> {
        // Use lstat so a remote symlink is classified as a symlink rather than
        // followed to a file or directory outside the configured source root.
        let stat = sftp
            .lstat(remote_path)
            .map_err(|e| format!("Failed to lstat {}: {}", remote_path.display(), e))?;

        if sftp_file_stat_is_symlink(&stat) {
            tracing::warn!(
                path = %remote_path.display(),
                "Skipping remote symlink during SFTP sync"
            );
            return Ok(());
        }

        if stat.is_dir() {
            // Create local directory for this directory item
            std::fs::create_dir_all(local_path)
                .map_err(|e| format!("Failed to create {}: {}", local_path.display(), e))?;

            // List directory contents
            let entries = sftp
                .readdir(remote_path)
                .map_err(|e| format!("Failed to list {}: {}", remote_path.display(), e))?;

            for (entry_path, _entry_stat) in entries {
                let Some(file_name) = sftp_entry_file_name(&entry_path, remote_path) else {
                    continue;
                };

                let entry_stat = sftp
                    .lstat(&entry_path)
                    .map_err(|e| format!("Failed to lstat {}: {}", entry_path.display(), e))?;
                if sftp_file_stat_is_symlink(&entry_stat) {
                    tracing::warn!(
                        path = %entry_path.display(),
                        "Skipping remote symlink during SFTP sync"
                    );
                    continue;
                }

                let local_entry = local_path.join(file_name);

                if entry_stat.is_dir() {
                    // Recurse into subdirectory
                    self.sftp_download_recursive(
                        sftp,
                        &entry_path,
                        &local_entry,
                        files_transferred,
                        bytes_transferred,
                    )?;
                } else if entry_stat.is_file() {
                    // Download file
                    if self.sftp_download_file(
                        sftp,
                        &entry_path,
                        &local_entry,
                        bytes_transferred,
                    )? {
                        *files_transferred += 1;
                    }
                }
                // Skip symlinks and other types for safety
            }
        } else if stat.is_file() {
            // Ensure the parent directory exists
            if let Some(parent) = local_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    format!("Failed to create local dir {}: {}", parent.display(), e)
                })?;
            }

            if self.sftp_download_file(sftp, remote_path, local_path, bytes_transferred)? {
                *files_transferred += 1;
            }
        } else {
            // Not a regular file or directory (symlink, socket, etc.) - skip with warning
            tracing::warn!(
                path = %remote_path.display(),
                "Skipping remote path: not a regular file or directory"
            );
        }

        Ok(())
    }

    /// Download a single file via SFTP.
    fn sftp_download_file(
        &self,
        sftp: &Sftp,
        remote_path: &Path,
        local_path: &Path,
        bytes_transferred: &mut u64,
    ) -> Result<bool, String> {
        let stat = sftp
            .lstat(remote_path)
            .map_err(|e| format!("Failed to lstat {}: {}", remote_path.display(), e))?;
        if sftp_file_stat_is_symlink(&stat) {
            tracing::warn!(
                path = %remote_path.display(),
                "Skipping remote symlink during SFTP sync"
            );
            return Ok(false);
        }
        if !stat.is_file() {
            tracing::warn!(
                path = %remote_path.display(),
                "Skipping remote path: not a regular file"
            );
            return Ok(false);
        }

        let mut remote_file = sftp
            .open(remote_path)
            .map_err(|e| format!("Failed to open {}: {}", remote_path.display(), e))?;

        // Create local file
        let mut local_file = std::fs::File::create(local_path)
            .map_err(|e| format!("Failed to create {}: {}", local_path.display(), e))?;

        // Transfer in chunks
        let mut buffer = [0u8; 32768]; // 32KB chunks
        loop {
            let bytes_read = remote_file
                .read(&mut buffer)
                .map_err(|e| format!("Failed to read {}: {}", remote_path.display(), e))?;

            if bytes_read == 0 {
                break;
            }

            local_file
                .write_all(&buffer[..bytes_read])
                .map_err(|e| format!("Failed to write {}: {}", local_path.display(), e))?;

            *bytes_transferred += bytes_read as u64;
        }

        tracing::trace!(
            remote = %remote_path.display(),
            local = %local_path.display(),
            "downloaded file"
        );

        Ok(true)
    }
}

/// Resolve an SFTP entry's basename for local mirroring.
fn sftp_entry_file_name<'a>(entry_path: &'a Path, parent_path: &Path) -> Option<&'a str> {
    let Some(file_name) = entry_path.file_name() else {
        tracing::warn!(
            parent = %parent_path.display(),
            entry = ?entry_path,
            "Skipping SFTP entry without a file name"
        );
        return None;
    };

    let Some(file_name) = file_name.to_str() else {
        tracing::warn!(
            parent = %parent_path.display(),
            entry = ?entry_path,
            "Skipping SFTP entry with non-UTF-8 file name"
        );
        return None;
    };

    if file_name.is_empty() {
        tracing::warn!(
            parent = %parent_path.display(),
            entry = ?entry_path,
            "Skipping SFTP entry with empty file name"
        );
        return None;
    }

    if file_name == "." || file_name == ".." {
        return None;
    }

    Some(file_name)
}

/// Check whether the `scp` executable exists on this system.
///
/// Uses a simple PATH search rather than running `scp` (which exits non-zero
/// when invoked without arguments on many platforms).
fn which_scp_exists() -> bool {
    std::env::var_os("PATH")
        .map(|path_var| {
            std::env::split_paths(&path_var).any(|dir| {
                let candidate = dir.join(if cfg!(target_os = "windows") {
                    "scp.exe"
                } else {
                    "scp"
                });
                candidate.is_file()
            })
        })
        .unwrap_or(false)
}

/// Convert a Windows absolute path to a WSL-accessible `/mnt/<drive>/…` path.
///
/// E.g. `C:\Users\george\AppData\Roaming\cass` →
///      `/mnt/c/Users/george/AppData/Roaming/cass`
///
/// If the path does not look like a Windows drive path it is returned unchanged.
fn windows_path_to_wsl(path: &str) -> String {
    // Match "C:\..." or "C:/..."
    if path.len() >= 3 {
        let bytes = path.as_bytes();
        if bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
            let drive = (bytes[0] as char).to_lowercase().next().unwrap_or('c');
            let rest = path[3..].replace('\\', "/");
            return format!("/mnt/{}/{}", drive, rest);
        }
    }
    path.to_string()
}

/// Parse SSH host string into (optional_user, host).
///
/// Examples:
/// - "myserver" -> (None, "myserver")
/// - "user@myserver" -> (Some("user"), "myserver")
fn parse_ssh_host(host: &str) -> (Option<&str>, &str) {
    if let Some(at_pos) = host.find('@') {
        let user = &host[..at_pos];
        let hostname = &host[at_pos + 1..];
        (Some(user), hostname)
    } else {
        (None, host)
    }
}

/// Expand tilde in local paths.
fn expand_tilde_local(path: &str) -> String {
    if let Some(stripped) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return format!("{}/{}", home.display(), stripped);
    } else if path == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home.display().to_string();
    }
    path.to_string()
}

/// Convert a remote path to a safe directory name.
///
/// Sanitizes path by:
/// - Removing leading `~` and `/`
/// - Replacing path separators and spaces with underscores
/// - Removing parent directory references (`..`) to prevent traversal attacks
/// - Removing current directory references (`.`)
/// - Appending a stable hash to prevent collisions (e.g., "foo/bar" vs "foo_bar")
pub fn path_to_safe_dirname(path: &str) -> String {
    use std::path::{Component, Path};

    let path_obj = Path::new(path);
    let mut parts: Vec<&str> = Vec::new();

    for component in path_obj.components() {
        match component {
            Component::Normal(name) => {
                if let Some(s) = name.to_str() {
                    // Skip "~" (home directory marker) and empty/dot-only components
                    if !s.is_empty() && s != "." && s != "~" {
                        parts.push(s);
                    }
                }
            }
            // Skip all traversal components for security
            Component::ParentDir
            | Component::CurDir
            | Component::RootDir
            | Component::Prefix(_) => {}
        }
    }

    let cleaned = parts.join("_").replace([' ', '\\'], "_");

    // Append stable hash to prevent collisions
    let hash = fnv1a_hash(path);
    let hash_suffix = format!("{:08x}", hash);

    if cleaned.is_empty() {
        format!("root_{}", hash_suffix)
    } else {
        format!("{}_{}", cleaned, hash_suffix)
    }
}

fn fnv1a_hash(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Parse transfer statistics from rsync --stats output.
fn parse_rsync_stats(output: &str) -> RsyncStats {
    let mut stats = RsyncStats::default();

    for line in output.lines() {
        let line = line.trim();

        // Parse "Number of regular files transferred: N"
        if line.starts_with("Number of regular files transferred:")
            && let Some(num_str) = line.split(':').nth(1)
        {
            stats.files_transferred = num_str.trim().replace(',', "").parse().unwrap_or(0);
        }

        // Parse "Total transferred file size: N bytes"
        if line.starts_with("Total transferred file size:")
            && let Some(size_part) = line.split(':').nth(1)
        {
            // Handle formats like "1,234 bytes" or "1234"
            let size_str = size_part
                .split_whitespace()
                .next()
                .unwrap_or("0")
                .replace(',', "");
            stats.bytes_transferred = size_str.parse().unwrap_or(0);
        }
    }

    stats
}

// =============================================================================
// Sync Status Persistence
// =============================================================================

/// Result of a sync operation for a source.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncResult {
    /// Sync completed successfully.
    Success,
    /// Some paths synced, some failed.
    PartialFailure(String),
    /// Sync failed completely.
    Failed(String),
    /// Sync was skipped (e.g., dry run).
    #[default]
    Skipped,
}

impl SyncResult {
    /// Short display label for the result.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::PartialFailure(_) => "partial",
            Self::Failed(_) => "failed",
            Self::Skipped => "never",
        }
    }

    /// Error text for partial/full failures.
    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::PartialFailure(error) | Self::Failed(error) => Some(error.as_str()),
            Self::Success | Self::Skipped => None,
        }
    }
}

/// Sync information for a single source.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SourceSyncInfo {
    /// Timestamp of last sync attempt.
    pub last_sync: Option<i64>,
    /// Result of last sync.
    pub last_result: SyncResult,
    /// Number of files synced in last sync.
    pub files_synced: u64,
    /// Number of bytes transferred in last sync.
    pub bytes_transferred: u64,
    /// Duration of last sync in milliseconds.
    pub duration_ms: u64,
}

impl SourceSyncInfo {
    /// Build sync info from a sync report using the current wall clock time.
    pub fn from_report(report: &SyncReport) -> Self {
        Self {
            last_sync: Some(current_unix_ms()),
            last_result: report.sync_result(),
            files_synced: report.total_files(),
            bytes_transferred: report.total_bytes(),
            duration_ms: report.total_duration_ms,
        }
    }
}

/// Persistent sync status for all sources.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SyncStatus {
    /// Sync info per source (keyed by source name).
    pub sources: std::collections::HashMap<String, SourceSyncInfo>,
}

impl SyncStatus {
    /// Load sync status from disk.
    pub fn load(data_dir: &Path) -> Result<Self, std::io::Error> {
        let path = Self::status_path(data_dir);
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Save sync status to disk.
    ///
    /// Uses an atomic rename on Unix. On Windows, falls back to remove-then-rename
    /// because replacing an existing destination with `std::fs::rename` fails.
    pub fn save(&self, data_dir: &Path) -> Result<(), std::io::Error> {
        let path = Self::status_path(data_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        let tmp_path = unique_atomic_temp_path(&path);
        std::fs::write(&tmp_path, content)?;
        sync_file_path(&tmp_path)?;
        replace_file_from_temp(&tmp_path, &path)
    }

    /// Update status for a source from a sync report.
    pub fn update(&mut self, source_name: &str, report: &SyncReport) {
        self.set_info(source_name, SourceSyncInfo::from_report(report));
    }

    /// Set status for a source from precomputed sync info.
    pub fn set_info(&mut self, source_name: &str, info: SourceSyncInfo) {
        self.sources.insert(source_name.to_string(), info);
    }

    /// Drop sync status entries for sources that no longer exist.
    ///
    /// Returns `true` when at least one stale entry was removed.
    pub fn retain_sources<'a>(&mut self, source_names: impl IntoIterator<Item = &'a str>) -> bool {
        let allowed: std::collections::HashSet<&str> = source_names.into_iter().collect();
        let previous_len = self.sources.len();
        self.sources
            .retain(|source_name, _| allowed.contains(source_name.as_str()));
        self.sources.len() != previous_len
    }

    /// Get sync info for a source.
    pub fn get(&self, source_name: &str) -> Option<&SourceSyncInfo> {
        self.sources.get(source_name)
    }

    /// Get the path to the status file.
    fn status_path(data_dir: &Path) -> PathBuf {
        data_dir.join("sync_status.json")
    }
}

fn current_unix_ms() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(now).unwrap_or(i64::MAX)
}

fn unique_atomic_temp_path(path: &Path) -> PathBuf {
    unique_atomic_sidecar_path(path, "tmp", "sync_status.json")
}

fn replace_file_from_temp(temp_path: &Path, final_path: &Path) -> Result<(), std::io::Error> {
    #[cfg(windows)]
    {
        match std::fs::rename(temp_path, final_path) {
            Ok(()) => sync_parent_directory(final_path),
            Err(first_err)
                if final_path.exists()
                    && matches!(
                        first_err.kind(),
                        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                    ) =>
            {
                let backup_path = unique_replace_backup_path(final_path);
                std::fs::rename(final_path, &backup_path).map_err(|backup_err| {
                    let _ = std::fs::remove_file(temp_path);
                    std::io::Error::other(format!(
                        "failed preparing backup {} before replacing {}: first error: {}; backup error: {}",
                        backup_path.display(),
                        final_path.display(),
                        first_err,
                        backup_err
                    ))
                })?;
                match std::fs::rename(temp_path, final_path) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&backup_path);
                        sync_parent_directory(final_path)
                    }
                    Err(second_err) => {
                        let restore_result = std::fs::rename(&backup_path, final_path);
                        match restore_result {
                            Ok(()) => {
                                let _ = std::fs::remove_file(temp_path);
                                sync_parent_directory(final_path).map_err(|sync_err| {
                                    std::io::Error::other(format!(
                                        "failed replacing {} with {}: first error: {}; second error: {}; restored original file but failed syncing parent directory: {}",
                                        final_path.display(),
                                        temp_path.display(),
                                        first_err,
                                        second_err,
                                        sync_err
                                    ))
                                })?;
                                Err(std::io::Error::new(
                                    second_err.kind(),
                                    format!(
                                        "failed replacing {} with {}: first error: {}; second error: {}; restored original file",
                                        final_path.display(),
                                        temp_path.display(),
                                        first_err,
                                        second_err
                                    ),
                                ))
                            }
                            Err(restore_err) => Err(std::io::Error::other(format!(
                                "failed replacing {} with {}: first error: {}; second error: {}; restore error: {}; temp file retained at {}",
                                final_path.display(),
                                temp_path.display(),
                                first_err,
                                second_err,
                                restore_err,
                                temp_path.display()
                            ))),
                        }
                    }
                }
            }
            Err(rename_err) => Err(rename_err),
        }
    }

    #[cfg(not(windows))]
    {
        std::fs::rename(temp_path, final_path)?;
        sync_parent_directory(final_path)
    }
}

fn sync_file_path(path: &Path) -> Result<(), std::io::Error> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> Result<(), std::io::Error> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(windows)]
fn unique_replace_backup_path(path: &Path) -> PathBuf {
    unique_atomic_sidecar_path(path, "bak", "sync_status.json")
}

fn unique_atomic_sidecar_path(path: &Path, suffix: &str, fallback_name: &str) -> PathBuf {
    static NEXT_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let nonce = NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(fallback_name);

    path.with_file_name(format!(
        ".{file_name}.{suffix}.{}.{}.{}",
        std::process::id(),
        timestamp,
        nonce
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_path_to_safe_dirname() {
        let res = path_to_safe_dirname("~/.claude/projects");
        assert!(res.starts_with(".claude_projects_"));

        let res = path_to_safe_dirname("/home/user/data");
        assert!(res.starts_with("home_user_data_"));

        let res = path_to_safe_dirname("~/");
        assert!(res.starts_with("root_"));

        let res = path_to_safe_dirname("");
        assert!(res.starts_with("root_"));
    }

    #[test]
    fn test_path_to_safe_dirname_empty() {
        let res = path_to_safe_dirname("~");
        assert!(res.starts_with("root_"));

        let res = path_to_safe_dirname("/");
        assert!(res.starts_with("root_"));
    }

    #[test]
    fn test_remote_find_regular_files_command_uses_physical_traversal() {
        assert_eq!(
            remote_find_regular_files_command("/tmp/has space"),
            "find -P '/tmp/has space' -type f -print0"
        );
        assert_eq!(
            remote_find_regular_files_command("/tmp/that's all"),
            "find -P '/tmp/that'\\''s all' -type f -print0"
        );
    }

    #[test]
    fn test_parse_null_terminated_utf8_paths_skips_invalid_entries() {
        let paths = parse_null_terminated_utf8_paths(
            b"/remote/sessions/a.jsonl\0bad-\xff-name\0/remote/sessions/b.jsonl\0",
        );
        assert_eq!(
            paths,
            vec![
                "/remote/sessions/a.jsonl".to_string(),
                "/remote/sessions/b.jsonl".to_string()
            ]
        );
    }

    #[test]
    fn test_remote_file_to_safe_local_path_rejects_outside_root() {
        let root = Path::new("/remote/sessions");
        let local = Path::new("/mirror/root");

        assert_eq!(
            remote_file_to_safe_local_path(
                root,
                Path::new("/remote/sessions/a/b.jsonl"),
                local,
                "sessions"
            ),
            Some(PathBuf::from("/mirror/root/sessions/a/b.jsonl"))
        );
        assert_eq!(
            remote_file_to_safe_local_path(
                Path::new("/remote/session.jsonl"),
                Path::new("/remote/session.jsonl"),
                local,
                "session.jsonl"
            ),
            Some(PathBuf::from("/mirror/root/session.jsonl"))
        );
        assert_eq!(
            remote_file_to_safe_local_path(
                root,
                Path::new("/remote/sessions/../secret.txt"),
                local,
                "sessions"
            ),
            None
        );
        assert_eq!(
            remote_file_to_safe_local_path(
                root,
                Path::new("/remote/other/secret.txt"),
                local,
                "sessions"
            ),
            None
        );
    }

    #[test]
    fn test_sftp_file_stat_is_symlink_detects_link_modes() {
        let symlink = FileStat {
            size: None,
            uid: None,
            gid: None,
            perm: Some(0o120000 | 0o777),
            atime: None,
            mtime: None,
        };
        let regular = FileStat {
            size: None,
            uid: None,
            gid: None,
            perm: Some(0o100000 | 0o644),
            atime: None,
            mtime: None,
        };

        assert!(sftp_file_stat_is_symlink(&symlink));
        assert!(!sftp_file_stat_is_symlink(&regular));
    }

    #[test]
    fn test_sftp_entry_file_name_accepts_regular_names() {
        let parent = Path::new("/remote");
        let entry = parent.join("session.jsonl");

        assert_eq!(sftp_entry_file_name(&entry, parent), Some("session.jsonl"));
    }

    #[test]
    fn test_sftp_entry_file_name_skips_dot_entries() {
        let parent = Path::new("/remote");

        assert_eq!(sftp_entry_file_name(Path::new("."), parent), None);
        assert_eq!(sftp_entry_file_name(Path::new(".."), parent), None);
    }

    #[cfg(unix)]
    #[test]
    fn test_sftp_entry_file_name_rejects_non_utf8_names() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let parent = Path::new("/remote");
        let bad_component = Path::new(OsStr::from_bytes(b"bad-\xff-name"));
        let entry = parent.join(bad_component);

        assert_eq!(sftp_entry_file_name(&entry, parent), None);
    }

    #[test]
    fn test_parse_rsync_stats() {
        let output = r#"
Number of files: 42
Number of regular files transferred: 10
Total transferred file size: 1,234 bytes
        "#;

        let stats = parse_rsync_stats(output);
        assert_eq!(stats.files_transferred, 10);
        assert_eq!(stats.bytes_transferred, 1234);
    }

    #[test]
    fn test_parse_rsync_stats_empty() {
        let stats = parse_rsync_stats("");
        assert_eq!(stats.files_transferred, 0);
        assert_eq!(stats.bytes_transferred, 0);
    }

    #[test]
    fn test_quote_remote_shell_path_handles_spaces_and_quotes() {
        assert_eq!(
            quote_remote_shell_path("/Users/me/Library/Application Support/Cursor"),
            "'/Users/me/Library/Application Support/Cursor'"
        );
        assert_eq!(
            quote_remote_shell_path("/tmp/that's all"),
            "'/tmp/that'\\''s all'"
        );
    }

    #[test]
    fn test_remote_spec_for_rsync_quotes_only_when_needed() {
        assert_eq!(
            remote_spec_for_rsync("work-mac", "/tmp/has space", true),
            "work-mac:/tmp/has space"
        );
        assert_eq!(
            remote_spec_for_rsync("work-mac", "/tmp/has space", false),
            "work-mac:'/tmp/has space'"
        );
    }

    #[test]
    fn rsync_arg_protection_enum_maps_flags_correctly() {
        // Regression for #191: Homebrew rsync 3.4.1 renamed the flag to
        // --secluded-args; earlier 3.0–3.3 use --protect-args. The caller
        // must pass the name the installed rsync actually accepts in its
        // own --help listing.
        assert_eq!(
            RsyncArgProtection::ProtectArgs.flag(),
            Some("--protect-args")
        );
        assert_eq!(
            RsyncArgProtection::SecludedArgs.flag(),
            Some("--secluded-args")
        );
        assert_eq!(RsyncArgProtection::None.flag(), None);
        assert!(RsyncArgProtection::ProtectArgs.is_supported());
        assert!(RsyncArgProtection::SecludedArgs.is_supported());
        assert!(!RsyncArgProtection::None.is_supported());
    }

    #[test]
    fn test_remote_spec_for_shell_bound_copy_quotes_remote_path() {
        assert_eq!(
            remote_spec_for_shell_bound_copy("work-mac", "/tmp/has space"),
            "work-mac:'/tmp/has space'"
        );
    }

    #[test]
    fn test_remote_spec_for_scp_preserves_raw_path() {
        assert_eq!(
            remote_spec_for_scp("work-mac", "/tmp/has space"),
            "work-mac:/tmp/has space"
        );
        assert_eq!(
            remote_spec_for_scp("work-mac", "/tmp/that's all"),
            "work-mac:/tmp/that's all"
        );
    }

    #[test]
    fn test_sync_report_totals() {
        let mut report = SyncReport::new("test", SyncMethod::Rsync);
        report.add_path_result(PathSyncResult {
            files_transferred: 5,
            bytes_transferred: 100,
            success: true,
            ..Default::default()
        });
        report.add_path_result(PathSyncResult {
            files_transferred: 3,
            bytes_transferred: 50,
            success: true,
            ..Default::default()
        });

        assert_eq!(report.total_files(), 8);
        assert_eq!(report.total_bytes(), 150);
        assert!(report.all_succeeded);
    }

    #[test]
    fn test_sync_report_with_failure() {
        let mut report = SyncReport::new("test", SyncMethod::Rsync);
        report.add_path_result(PathSyncResult {
            success: true,
            ..Default::default()
        });
        report.add_path_result(PathSyncResult {
            success: false,
            error: Some("Connection refused".into()),
            ..Default::default()
        });

        assert!(!report.all_succeeded);
        assert_eq!(report.successful_paths(), 1);
        assert_eq!(report.failed_paths(), 1);
    }

    #[test]
    fn test_detect_sync_method() {
        // This test is platform-dependent but should at least not panic
        let method = SyncEngine::detect_sync_method();
        assert!(matches!(
            method,
            SyncMethod::Rsync | SyncMethod::WslRsync | SyncMethod::Scp | SyncMethod::Sftp
        ));
    }

    #[test]
    fn test_sync_engine_mirror_dir() {
        let engine = SyncEngine::new(Path::new("/data/cass"));
        let mirror = engine.mirror_dir("laptop");
        assert_eq!(mirror, PathBuf::from("/data/cass/remotes/laptop/mirror"));
    }

    #[test]
    fn test_sync_method_display() {
        assert_eq!(SyncMethod::Rsync.to_string(), "rsync");
        assert_eq!(SyncMethod::WslRsync.to_string(), "wsl-rsync");
        assert_eq!(SyncMethod::Scp.to_string(), "scp");
        assert_eq!(SyncMethod::Sftp.to_string(), "sftp");
    }

    #[test]
    fn test_windows_path_to_wsl_drive() {
        assert_eq!(
            windows_path_to_wsl("C:\\Users\\george\\AppData\\Roaming\\cass"),
            "/mnt/c/Users/george/AppData/Roaming/cass"
        );
    }

    #[test]
    fn test_windows_path_to_wsl_forward_slash() {
        assert_eq!(
            windows_path_to_wsl("C:/Users/george/data"),
            "/mnt/c/Users/george/data"
        );
    }

    #[test]
    fn test_windows_path_to_wsl_non_windows_path_unchanged() {
        // A Unix absolute path should pass through unchanged.
        assert_eq!(
            windows_path_to_wsl("/home/george/data"),
            "/home/george/data"
        );
    }

    #[test]
    fn test_expand_tilde_with_home() {
        // No tilde - returns unchanged
        assert_eq!(
            SyncEngine::expand_tilde_with_home("/home/user/projects", Some("/home/user")),
            "/home/user/projects"
        );

        // Tilde with home provided
        assert_eq!(
            SyncEngine::expand_tilde_with_home("~/.claude/projects", Some("/home/user")),
            "/home/user/.claude/projects"
        );

        // Just tilde
        assert_eq!(
            SyncEngine::expand_tilde_with_home("~", Some("/home/user")),
            "/home/user"
        );

        // Tilde without home - returns unchanged
        assert_eq!(
            SyncEngine::expand_tilde_with_home("~/.claude/projects", None),
            "~/.claude/projects"
        );

        // ~otheruser/path case - not expanded
        assert_eq!(
            SyncEngine::expand_tilde_with_home("~otheruser/projects", Some("/home/user")),
            "~otheruser/projects"
        );
    }

    #[test]
    fn test_sync_report_failed() {
        let report = SyncReport::failed("test-source", SyncError::NoHost);
        assert_eq!(report.source_name, "test-source");
        assert!(!report.all_succeeded);
        assert_eq!(report.path_results.len(), 1);
        assert!(!report.path_results[0].success);
        assert!(report.path_results[0].error.is_some());
    }

    #[test]
    fn test_sync_result_default() {
        let result = SyncResult::default();
        assert!(matches!(result, SyncResult::Skipped));
        assert_eq!(result.label(), "never");
    }

    #[test]
    fn test_source_sync_info_default() {
        let info = SourceSyncInfo::default();
        assert!(info.last_sync.is_none());
        assert_eq!(info.files_synced, 0);
        assert_eq!(info.bytes_transferred, 0);
        assert_eq!(info.duration_ms, 0);
    }

    #[test]
    fn test_sync_status_update() {
        let mut status = SyncStatus::default();

        let mut report = SyncReport::new("laptop", SyncMethod::Rsync);
        report.add_path_result(PathSyncResult {
            files_transferred: 10,
            bytes_transferred: 1000,
            success: true,
            ..Default::default()
        });
        report.total_duration_ms = 500;

        status.update("laptop", &report);

        let info = status.get("laptop").unwrap();
        assert!(info.last_sync.is_some());
        assert!(matches!(info.last_result, SyncResult::Success));
        assert_eq!(info.files_synced, 10);
        assert_eq!(info.bytes_transferred, 1000);
        assert_eq!(info.duration_ms, 500);
    }

    #[test]
    fn test_sync_status_partial_failure() {
        let mut status = SyncStatus::default();

        let mut report = SyncReport::new("server", SyncMethod::Rsync);
        report.add_path_result(PathSyncResult {
            success: true,
            files_transferred: 5,
            ..Default::default()
        });
        report.add_path_result(PathSyncResult {
            success: false,
            error: Some("Connection refused".into()),
            ..Default::default()
        });

        status.update("server", &report);

        let info = status.get("server").unwrap();
        assert!(matches!(info.last_result, SyncResult::PartialFailure(_)));
    }

    #[test]
    fn test_sync_status_full_failure() {
        let mut status = SyncStatus::default();

        let mut report = SyncReport::new("dead-host", SyncMethod::Rsync);
        report.add_path_result(PathSyncResult {
            success: false,
            error: Some("Host unreachable".into()),
            ..Default::default()
        });

        status.update("dead-host", &report);

        let info = status.get("dead-host").unwrap();
        assert!(matches!(info.last_result, SyncResult::Failed(_)));
    }

    #[test]
    fn test_sync_status_save_round_trips() {
        let temp = TempDir::new().expect("tempdir");
        let mut status = SyncStatus::default();
        let mut report = SyncReport::new("laptop", SyncMethod::Rsync);
        report.add_path_result(PathSyncResult {
            files_transferred: 3,
            bytes_transferred: 42,
            success: true,
            ..Default::default()
        });
        status.update("laptop", &report);

        status.save(temp.path()).expect("save status");
        let loaded = SyncStatus::load(temp.path()).expect("load status");

        let info = loaded.get("laptop").expect("round-tripped source");
        assert_eq!(info.files_synced, 3);
        assert_eq!(info.bytes_transferred, 42);
        assert!(matches!(info.last_result, SyncResult::Success));
    }

    #[test]
    fn test_sync_status_retain_sources_prunes_removed_entries() {
        let mut status = SyncStatus::default();
        status.sources.insert(
            "laptop".into(),
            SourceSyncInfo {
                files_synced: 3,
                ..Default::default()
            },
        );
        status.sources.insert(
            "desktop".into(),
            SourceSyncInfo {
                files_synced: 5,
                ..Default::default()
            },
        );

        let removed_any = status.retain_sources(["laptop"]);

        assert!(removed_any);
        assert!(status.get("laptop").is_some());
        assert!(status.get("desktop").is_none());
    }

    #[test]
    fn test_unique_atomic_temp_path_changes_each_call() {
        let final_path = Path::new("/tmp/sync_status.json");
        let first = unique_atomic_temp_path(final_path);
        let second = unique_atomic_temp_path(final_path);

        assert_ne!(first, second);
        assert_eq!(first.parent(), final_path.parent());
        assert_eq!(second.parent(), final_path.parent());
    }

    #[test]
    fn test_replace_file_from_temp_overwrites_existing_file() {
        let temp = TempDir::new().expect("tempdir");
        let final_path = temp.path().join("sync_status.json");
        let first_tmp = temp.path().join("first.tmp");
        let second_tmp = temp.path().join("second.tmp");

        std::fs::write(&first_tmp, "{\"first\":true}").expect("write first temp");
        replace_file_from_temp(&first_tmp, &final_path).expect("initial replace");
        assert_eq!(
            std::fs::read_to_string(&final_path).expect("read first final"),
            "{\"first\":true}"
        );

        std::fs::write(&second_tmp, "{\"second\":true}").expect("write second temp");
        replace_file_from_temp(&second_tmp, &final_path).expect("overwrite replace");
        assert_eq!(
            std::fs::read_to_string(&final_path).expect("read second final"),
            "{\"second\":true}"
        );
    }

    #[test]
    fn test_sync_engine_with_timeouts() {
        let engine = SyncEngine::new(Path::new("/data"))
            .with_connection_timeout(30)
            .with_transfer_timeout(600);

        assert_eq!(engine.connection_timeout, 30);
        assert_eq!(engine.transfer_timeout, 600);
    }

    #[test]
    fn test_sync_error_display() {
        assert_eq!(
            SyncError::NoHost.to_string(),
            "Source has no host configured"
        );
        assert_eq!(
            SyncError::NoPaths.to_string(),
            "Source has no paths configured"
        );
        assert_eq!(
            SyncError::Timeout(30).to_string(),
            "Connection timed out after 30 seconds"
        );
        assert_eq!(SyncError::Cancelled.to_string(), "Sync cancelled");
    }

    // =========================================================================
    // SFTP helper function tests
    // =========================================================================

    #[test]
    fn test_parse_ssh_host_simple() {
        let (user, host) = parse_ssh_host("myserver");
        assert!(user.is_none());
        assert_eq!(host, "myserver");
    }

    #[test]
    fn test_parse_ssh_host_with_user() {
        let (user, host) = parse_ssh_host("admin@myserver");
        assert_eq!(user, Some("admin"));
        assert_eq!(host, "myserver");
    }

    #[test]
    fn test_parse_ssh_host_with_domain() {
        let (user, host) = parse_ssh_host("deploy@server.example.com");
        assert_eq!(user, Some("deploy"));
        assert_eq!(host, "server.example.com");
    }

    #[test]
    fn test_parse_ssh_host_email_like() {
        // Edge case: user looks like email prefix
        let (user, host) = parse_ssh_host("user@host");
        assert_eq!(user, Some("user"));
        assert_eq!(host, "host");
    }

    #[test]
    fn test_expand_tilde_local_with_tilde_prefix() {
        let expanded = expand_tilde_local("~/Documents/file.txt");
        // Should start with home directory, not tilde
        assert!(!expanded.starts_with('~'));
        assert!(expanded.ends_with("/Documents/file.txt"));
    }

    #[test]
    fn test_expand_tilde_local_just_tilde() {
        let expanded = expand_tilde_local("~");
        // Should be just home directory
        assert!(!expanded.starts_with('~'));
        assert!(!expanded.is_empty());
    }

    #[test]
    fn test_expand_tilde_local_no_tilde() {
        let path = "/absolute/path/to/file";
        let expanded = expand_tilde_local(path);
        assert_eq!(expanded, path);
    }

    #[test]
    fn test_expand_tilde_local_tilde_in_middle() {
        // Tilde in middle should not be expanded
        let path = "/path/with/~tilde/inside";
        let expanded = expand_tilde_local(path);
        assert_eq!(expanded, path);
    }
}
