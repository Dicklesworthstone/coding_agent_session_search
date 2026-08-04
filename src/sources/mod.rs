//! Remote sources management for cass.
//!
//! This module provides functionality for configuring and syncing agent session
//! data from remote machines via SSH. It enables cass to search across conversation
//! history from multiple machines.
//!
//! # Architecture
//!
//! - **config**: Configuration types for defining remote sources
//! - **provenance**: Types for tracking conversation origins
//! - **sync**: Sync engine for pulling sessions from remotes via rsync/SSH
//! - **status** (future): Sync status tracking
//!
//! # Configuration
//!
//! Sources are configured in `~/.config/cass/sources.toml`:
//!
//! ```toml
//! [[sources]]
//! name = "laptop"
//! type = "ssh"
//! host = "user@laptop.local"
//! paths = ["~/.claude/projects", "~/.cursor"]
//! ```
//!
//! # Provenance
//!
//! Each conversation tracks where it came from via [`provenance::Origin`]:
//!
//! ```rust,ignore
//! use coding_agent_search::sources::provenance::{Origin, SourceKind};
//!
//! // Local conversation
//! let local = Origin::local();
//!
//! // Remote conversation
//! let remote = Origin::remote("work-laptop");
//! ```
//!
//! # Syncing
//!
//! The sync engine uses rsync over SSH for efficient delta transfers:
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
//!
//! # Usage
//!
//! ```rust,ignore
//! use coding_agent_search::sources::config::SourcesConfig;
//!
//! // Load configuration
//! let config = SourcesConfig::load()?;
//!
//! // Iterate remote sources
//! for source in config.remote_sources() {
//!     println!("Source: {} ({})", source.name, source.host.as_deref().unwrap_or("-"));
//! }
//! ```

pub mod config;
pub(crate) mod config_validation;
pub mod index;
pub mod install;
pub mod interactive;
pub mod probe;
pub mod provenance;
pub mod setup;
pub mod sync;

use std::io::Read as IoRead;
use std::process::{Child, Command, Output};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wait_timeout::ChildExt;

/// Canonical SSH stderr marker for host-key verification failures.
pub(crate) const HOST_KEY_VERIFICATION_FAILED: &str = "Host key verification failed";

/// Build strict SSH CLI tokens with consistent trust policy.
///
/// The returned vector contains full `ssh` argument tokens:
/// `-o BatchMode=yes -o ConnectTimeout=<secs> -o StrictHostKeyChecking=yes`.
pub(crate) fn strict_ssh_cli_tokens(connect_timeout_secs: u64) -> Vec<String> {
    let mut tokens = Vec::new();
    if let Some(config_path) = ssh_config_override() {
        tokens.push("-F".to_string());
        tokens.push(config_path);
    }
    tokens.extend([
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={connect_timeout_secs}"),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=yes".to_string(),
    ]);
    tokens
}

/// Build strict SSH command string for tools that require a single shell fragment.
pub(crate) fn strict_ssh_command_for_rsync(connect_timeout_secs: u64) -> String {
    let config_arg = ssh_config_override()
        .map(|path| format!(" -F {}", shell_quote_ssh_arg(&path)))
        .unwrap_or_default();
    format!(
        "ssh{config_arg} -o BatchMode=yes -o ConnectTimeout={connect_timeout_secs} -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -o StrictHostKeyChecking=yes"
    )
}

fn ssh_config_override() -> Option<String> {
    dotenvy::var("CASS_SSH_CONFIG")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn shell_quote_ssh_arg(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '@'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct ChildPipeReader {
    receiver: Receiver<std::io::Result<Vec<u8>>>,
    handle: JoinHandle<()>,
}

fn drain_child_pipe<R>(mut pipe: R) -> ChildPipeReader
where
    R: IoRead + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut output = Vec::new();
        let result = pipe.read_to_end(&mut output).map(|_| output);
        let _ = sender.send(result);
    });
    ChildPipeReader { receiver, handle }
}

fn finish_child_pipe(
    pipe_reader: Option<ChildPipeReader>,
    deadline: Instant,
) -> std::io::Result<Option<Vec<u8>>> {
    match pipe_reader {
        Some(reader) => {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);
            match reader.receiver.recv_timeout(remaining) {
                Ok(result) => {
                    reader
                        .handle
                        .join()
                        .map_err(|_| std::io::Error::other("child pipe reader panicked"))?;
                    result.map(Some)
                }
                Err(RecvTimeoutError::Timeout) => Ok(None),
                Err(RecvTimeoutError::Disconnected) => {
                    let reader_panicked = reader.handle.join().is_err();
                    let message = if reader_panicked {
                        "child pipe reader panicked before sending output"
                    } else {
                        "child pipe reader disconnected before sending output"
                    };
                    Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, message))
                }
            }
        }
        None => Ok(Some(Vec::new())),
    }
}

#[cfg(unix)]
pub(crate) fn configure_child_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    cmd.process_group(0);
}

#[cfg(not(unix))]
pub(crate) fn configure_child_process_group(_cmd: &mut Command) {}

#[cfg(unix)]
fn kill_child_process_group(pid: u32) {
    let process_group = format!("-{pid}");
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &process_group])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn kill_child_process_group(_pid: u32) {}

/// Wait for a child process while draining stdout/stderr without letting either
/// process execution or pipe collection outlive the same wall-clock deadline.
///
/// Callers should pass children configured with
/// [`configure_child_process_group`] before `spawn()`. Without that, the direct
/// child can be killed but shell grandchildren may keep inherited pipe FDs open
/// until they exit naturally.
pub(crate) fn wait_for_child_output_with_timeout(
    mut child: Child,
    timeout: Duration,
) -> std::io::Result<Option<Output>> {
    let timeout = if timeout.is_zero() {
        Duration::from_secs(1)
    } else {
        timeout
    };
    let start = Instant::now();
    let deadline = start.checked_add(timeout).unwrap_or(start);
    let child_pid = child.id();
    let stdout_reader = child.stdout.take().map(drain_child_pipe);
    let stderr_reader = child.stderr.take().map(drain_child_pipe);

    match child.wait_timeout(timeout)? {
        Some(status) => {
            let Some(stdout) = finish_child_pipe(stdout_reader, deadline)? else {
                kill_child_process_group(child_pid);
                return Ok(None);
            };
            let Some(stderr) = finish_child_pipe(stderr_reader, deadline)? else {
                kill_child_process_group(child_pid);
                return Ok(None);
            };
            Ok(Some(Output {
                status,
                stdout,
                stderr,
            }))
        }
        None => {
            kill_child_process_group(child_pid);
            let _ = child.kill();
            let _ = child.wait();
            Ok(None)
        }
    }
}

/// Whether stderr indicates SSH host-key verification failure.
pub(crate) fn is_host_key_verification_failure(stderr: &str) -> bool {
    stderr.contains(HOST_KEY_VERIFICATION_FAILED)
}

/// Standard user-facing error for host-key verification failures.
pub(crate) fn host_key_verification_error(host: &str) -> String {
    format!(
        "Host key verification failed for {host} (add/verify host key in ~/.ssh/known_hosts first)"
    )
}

// Re-export commonly used config types
pub use config::{
    BackupInfo, ConfigError, ConfigPreview, DiscoveredHost, MergeResult, PathMapping, Platform,
    SkipReason, SourceConfigGenerator, SourceDefinition, SourcesConfig, SyncSchedule,
    discover_ssh_hosts, get_preset_paths,
};

// Re-export commonly used provenance types
pub use provenance::{LOCAL_SOURCE_ID, Origin, Source, SourceFilter, SourceKind};

// Re-export commonly used sync types
pub use sync::{
    PathSyncResult, SourceHealthKind, SourceSyncAction, SourceSyncDecision, SourceSyncInfo,
    SyncEngine, SyncError, SyncMethod, SyncReport, SyncResult, SyncStatus,
};

// Re-export commonly used probe types
pub use probe::{
    CassStatus, DetectedAgent, HostProbeResult, ProbeCache, ResourceInfo, SystemInfo, probe_host,
    probe_hosts_parallel,
};

// Re-export commonly used install types
pub use install::{
    InstallError, InstallMethod, InstallProgress, InstallResult, InstallStage, RemoteInstaller,
};

// Re-export commonly used index types
pub use index::{IndexError, IndexProgress, IndexResult, IndexStage, RemoteIndexer};

// Re-export commonly used interactive types
pub use interactive::{
    CassStatusDisplay, HostDisplayInfo, HostSelectionResult, HostSelector, HostState,
    InteractiveError, confirm_action, confirm_with_details, probe_to_display_info,
    run_host_selection,
};

// Re-export commonly used setup types
pub use setup::{SetupError, SetupOptions, SetupResult, SetupState, run_setup};

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = dotenvy::var(key).ok();
            // SAFETY: test helper toggles a process-local env var for isolation.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                // SAFETY: test helper restores prior process env for isolation.
                unsafe {
                    std::env::set_var(self.key, value);
                }
            } else {
                // SAFETY: test helper restores prior process env for isolation.
                unsafe {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn strict_ssh_cli_tokens_include_config_override() {
        let _guard = EnvGuard::set("CASS_SSH_CONFIG", "/tmp/cass ssh/config");

        let tokens = strict_ssh_cli_tokens(5);

        assert_eq!(tokens[0], "-F");
        assert_eq!(tokens[1], "/tmp/cass ssh/config");
        assert!(tokens.contains(&"StrictHostKeyChecking=yes".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn strict_ssh_command_for_rsync_quotes_config_override() {
        let _guard = EnvGuard::set("CASS_SSH_CONFIG", "/tmp/cass'ssh/config");

        let command = strict_ssh_command_for_rsync(5);

        assert!(command.starts_with("ssh -F '/tmp/cass'\\''ssh/config' "));
        assert!(command.contains("StrictHostKeyChecking=yes"));
    }

    #[test]
    #[serial_test::serial]
    fn strict_ssh_helpers_ignore_empty_config_override() {
        let _guard = EnvGuard::set("CASS_SSH_CONFIG", "   ");

        let tokens = strict_ssh_cli_tokens(5);
        let command = strict_ssh_command_for_rsync(5);

        assert!(!tokens.contains(&"-F".to_string()));
        assert!(!command.contains(" -F "));
    }
}
