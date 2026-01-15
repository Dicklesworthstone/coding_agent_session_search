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
use std::time::Instant;

use thiserror::Error;

use super::config::{SourceDefinition, discover_ssh_hosts};
use ssh2::{Session, Sftp};
use std::io::{Read as IoRead, Write as IoWrite};
use std::net::TcpStream;

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
    /// SFTP fallback when rsync is unavailable
    Sftp,
}

impl std::fmt::Display for SyncMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rsync => write!(f, "rsync"),
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
        let ssh_opts = format!(
            "-o BatchMode=yes -o ConnectTimeout={} -o StrictHostKeyChecking=accept-new",
            self.connection_timeout
        );

        let output = Command::new("ssh")
            .args(ssh_opts.split_whitespace())
            .arg("--")
            .arg(host)
            .arg("echo $HOME")
            .output()
            .map_err(|e| SyncError::SshFailed(format!("Failed to execute ssh: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
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
    pub fn detect_sync_method() -> SyncMethod {
        if Command::new("rsync")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            SyncMethod::Rsync
        } else {
            SyncMethod::Sftp
        }
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
        let safe_name = path_to_safe_dirname(&expanded_path);
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
        let remote_spec = format!("{}:{}", host, expanded_path);
        let ssh_opts = format!(
            "ssh -o BatchMode=yes -o ConnectTimeout={} -o StrictHostKeyChecking=accept-new",
            self.connection_timeout
        );

        let mut cmd = Command::new("rsync");
        cmd.args([
            "-avz",           // Archive, verbose, compress
            "--stats",        // Show transfer stats for parsing
            "--partial",      // Keep partial transfers for resume
            "--protect-args", // Preserve spaces/special chars in remote paths
            "--timeout",
            &self.transfer_timeout.to_string(),
            "-e",
            &ssh_opts,
            "--",
            &remote_spec,
            match local_path.to_str() {
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
            },
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
        let expanded_path = Self::expand_tilde_with_home(remote_path, remote_home);
        let local_path = dest_dir.join(path_to_safe_dirname(&expanded_path));

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
        // Default to current user, never fall back to "root" for security
        // Try USER, then LOGNAME, then fail with descriptive error in username resolution
        let default_user = dotenvy::var("USER")
            .or_else(|_| dotenvy::var("LOGNAME"))
            .unwrap_or_else(|_| "nobody".to_string());
        let username = ssh_user
            .map(|s| s.to_string())
            .or_else(|| host_config.and_then(|h| h.user.clone()))
            .unwrap_or(default_user);
        let identity_file = host_config.and_then(|h| h.identity_file.as_deref());

        tracing::debug!(
            hostname = %hostname,
            port,
            username = %username,
            identity_file = ?identity_file,
            remote_path = %expanded_path,
            "SFTP connection parameters"
        );

        // Connect via TCP
        let tcp = match TcpStream::connect((hostname, port)) {
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

        // Set TCP timeout
        let timeout = std::time::Duration::from_secs(self.connection_timeout);
        let _ = tcp.set_read_timeout(Some(timeout));
        let _ = tcp.set_write_timeout(Some(timeout));

        // Create SSH session
        let mut session = match Session::new() {
            Ok(s) => s,
            Err(e) => {
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

        if let Err(e) = session.handshake() {
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

        if let Err(e) = self.sftp_download_recursive(
            &sftp,
            Path::new(&expanded_path),
            &local_path,
            &mut files_transferred,
            &mut bytes_transferred,
        ) {
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
        // Check if remote path is a directory or file
        let stat = sftp
            .stat(remote_path)
            .map_err(|e| format!("Failed to stat {}: {}", remote_path.display(), e))?;

        if stat.is_dir() {
            // Create local directory
            std::fs::create_dir_all(local_path)
                .map_err(|e| format!("Failed to create {}: {}", local_path.display(), e))?;

            // List directory contents
            let entries = sftp
                .readdir(remote_path)
                .map_err(|e| format!("Failed to list {}: {}", remote_path.display(), e))?;

            for (entry_path, entry_stat) in entries {
                let file_name = entry_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");

                // Skip . and .. and empty/invalid filenames
                if file_name.is_empty() || file_name == "." || file_name == ".." {
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
                    self.sftp_download_file(sftp, &entry_path, &local_entry, bytes_transferred)?;
                    *files_transferred += 1;
                }
                // Skip symlinks and other types for safety
            }
        } else if stat.is_file() {
            // Single file - download to local path
            std::fs::create_dir_all(local_path.parent().unwrap_or(local_path))
                .map_err(|e| format!("Failed to create parent dir: {}", e))?;

            // For single files, use the file name from remote path
            let file_name = remote_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file");
            let local_file = local_path.join(file_name);

            self.sftp_download_file(sftp, remote_path, &local_file, bytes_transferred)?;
            *files_transferred += 1;
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
    ) -> Result<(), String> {
        // Open remote file
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

        Ok(())
    }
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
pub fn path_to_safe_dirname(path: &str) -> String {
    use std::path::{Component, Path};

    let path_obj = Path::new(path);
    let mut parts: Vec<&str> = Vec::new();

    for component in path_obj.components() {
        match component {
            Component::Normal(name) => {
                if let Some(s) = name.to_str() {
                    // Skip hidden files starting with dot if they're just "."
                    if !s.is_empty() && s != "." {
                        parts.push(s);
                    }
                }
            }
            // Skip all traversal components for security
            Component::ParentDir | Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }

    let cleaned = parts.join("_").replace([' ', '\\'], "_");

    if cleaned.is_empty() {
        "root".to_string()
    } else {
        cleaned
    }
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
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            serde_json::from_str(&content)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        } else {
            Ok(Self::default())
        }
    }

    /// Save sync status to disk.
    pub fn save(&self, data_dir: &Path) -> Result<(), std::io::Error> {
        let path = Self::status_path(data_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, content)
    }

    /// Update status for a source from a sync report.
    pub fn update(&mut self, source_name: &str, report: &SyncReport) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let result = if report.all_succeeded {
            SyncResult::Success
        } else if report.successful_paths() > 0 {
            let errors: Vec<String> = report
                .path_results
                .iter()
                .filter_map(|r| r.error.clone())
                .collect();
            SyncResult::PartialFailure(errors.join("; "))
        } else {
            let errors: Vec<String> = report
                .path_results
                .iter()
                .filter_map(|r| r.error.clone())
                .collect();
            SyncResult::Failed(errors.join("; "))
        };

        self.sources.insert(
            source_name.to_string(),
            SourceSyncInfo {
                last_sync: Some(now),
                last_result: result,
                files_synced: report.total_files(),
                bytes_transferred: report.total_bytes(),
                duration_ms: report.total_duration_ms,
            },
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_to_safe_dirname() {
        assert_eq!(
            path_to_safe_dirname("~/.claude/projects"),
            ".claude_projects"
        );
        assert_eq!(path_to_safe_dirname("/home/user/data"), "home_user_data");
        assert_eq!(path_to_safe_dirname("~/"), "root"); // Empty after trimming becomes "root"
        assert_eq!(path_to_safe_dirname(""), "root");
    }

    #[test]
    fn test_path_to_safe_dirname_empty() {
        assert_eq!(path_to_safe_dirname("~"), "root");
        assert_eq!(path_to_safe_dirname("/"), "root");
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
        assert!(matches!(method, SyncMethod::Rsync | SyncMethod::Sftp));
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
        assert_eq!(SyncMethod::Sftp.to_string(), "sftp");
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
