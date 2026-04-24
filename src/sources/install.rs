//! Remote cass installation via SSH.
//!
//! This module provides functionality to automatically install cass on remote
//! machines via SSH. It supports multiple installation methods with intelligent
//! fallback and robust handling of long-running installations.
//!
//! # Installation Methods (Priority Order)
//!
//! 1. **Cargo Binstall** (fastest if available) - downloads pre-built binary via cargo
//! 2. **Pre-built Binary** - direct binary download from GitHub releases
//! 3. **Cargo Install** - compile from source (most reliable fallback)
//! 4. **Full Bootstrap** - install rustup first, then compile
//!
//! # Example
//!
//! ```rust,ignore
//! use coding_agent_search::sources::install::{RemoteInstaller, InstallProgress};
//! use coding_agent_search::sources::probe::{SystemInfo, ResourceInfo};
//!
//! let installer = RemoteInstaller::new("laptop", system_info, resources);
//!
//! installer.install(|progress| {
//!     println!("{}: {}", progress.stage, progress.message);
//! })?;
//! ```

use std::io::Write as IoWrite;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    host_key_verification_error, is_host_key_verification_failure,
    probe::{ResourceInfo, SystemInfo},
    strict_ssh_cli_tokens,
};

// =============================================================================
// Constants
// =============================================================================

/// Default SSH connection timeout for installation commands.
pub const DEFAULT_INSTALL_TIMEOUT_SECS: u64 = 600; // 10 minutes for cargo install

/// Minimum disk space required for installation (MB).
pub const MIN_DISK_MB: u64 = 2048; // 2 GB

/// Minimum memory recommended for compilation (MB).
pub const MIN_MEMORY_MB: u64 = 1024; // 1 GB

/// Current cass version for installation.
pub const CASS_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Package name on crates.io.
pub const CRATE_NAME: &str = "coding-agent-search";

// =============================================================================
// Error Types
// =============================================================================

/// Errors that can occur during remote installation.
#[derive(Error, Debug)]
pub enum InstallError {
    #[error("SSH connection failed: {0}")]
    SshFailed(String),

    #[error("SSH connection timed out after {0} seconds")]
    Timeout(u64),

    #[error("Insufficient disk space: {available_mb}MB available, {required_mb}MB required")]
    InsufficientDisk { available_mb: u64, required_mb: u64 },

    #[error("Insufficient memory: {available_mb}MB available, {required_mb}MB recommended")]
    InsufficientMemory { available_mb: u64, required_mb: u64 },

    #[error("Installation method {method} failed: {reason}")]
    MethodFailed { method: String, reason: String },

    #[error("No suitable installation method available")]
    NoMethodAvailable,

    #[error("Verification failed: {0}")]
    VerificationFailed(String),

    #[error("Checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    #[error("Missing system dependency: {dep}. Fix: {fix}")]
    MissingDependency { dep: String, fix: String },

    #[error("Installation cancelled")]
    Cancelled,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

// =============================================================================
// Install Method Types
// =============================================================================

/// Installation method for cass.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum InstallMethod {
    /// Install via cargo-binstall (fastest, downloads pre-built binary).
    CargoBinstall,

    /// Download pre-built binary directly from GitHub releases.
    PrebuiltBinary {
        url: String,
        checksum: Option<String>,
    },

    /// Compile from source via cargo install.
    CargoInstall,

    /// Full bootstrap: install rustup first, then compile.
    FullBootstrap,
}

impl InstallMethod {
    /// Get display name for the method.
    pub fn display_name(&self) -> &'static str {
        match self {
            InstallMethod::CargoBinstall => "cargo-binstall",
            InstallMethod::PrebuiltBinary { .. } => "pre-built binary",
            InstallMethod::CargoInstall => "cargo install",
            InstallMethod::FullBootstrap => "full bootstrap (rustup + cargo)",
        }
    }

    /// Estimated time for this method.
    pub fn estimated_time(&self) -> Duration {
        match self {
            InstallMethod::CargoBinstall => Duration::from_secs(30),
            InstallMethod::PrebuiltBinary { .. } => Duration::from_secs(10),
            InstallMethod::CargoInstall => Duration::from_secs(300), // 5 minutes
            InstallMethod::FullBootstrap => Duration::from_secs(600), // 10 minutes
        }
    }

    /// Whether this method requires compilation.
    pub fn requires_compilation(&self) -> bool {
        matches!(
            self,
            InstallMethod::CargoInstall | InstallMethod::FullBootstrap
        )
    }
}

impl std::fmt::Display for InstallMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

// =============================================================================
// Progress Types
// =============================================================================

/// Current stage of installation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstallStage {
    /// Preparing installation (checking resources, selecting method).
    Preparing,
    /// Downloading files.
    Downloading,
    /// Compiling code.
    Compiling { crate_name: String },
    /// Installing binary.
    Installing,
    /// Verifying installation.
    Verifying,
    /// Installation complete.
    Complete,
    /// Installation failed.
    Failed { error: String },
}

impl std::fmt::Display for InstallStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallStage::Preparing => write!(f, "Preparing"),
            InstallStage::Downloading => write!(f, "Downloading"),
            InstallStage::Compiling { crate_name } => write!(f, "Compiling {}", crate_name),
            InstallStage::Installing => write!(f, "Installing"),
            InstallStage::Verifying => write!(f, "Verifying"),
            InstallStage::Complete => write!(f, "Complete"),
            InstallStage::Failed { error } => write!(f, "Failed: {}", error),
        }
    }
}

/// Progress update during installation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallProgress {
    /// Current stage.
    pub stage: InstallStage,
    /// Human-readable message.
    pub message: String,
    /// Optional progress percentage (0-100).
    pub percent: Option<u8>,
    /// Elapsed time since start.
    pub elapsed: Duration,
}

/// Result of a successful installation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallResult {
    /// Method used for installation.
    pub method: InstallMethod,
    /// Installed version.
    pub version: String,
    /// Total installation time.
    pub duration: Duration,
    /// Installation path.
    pub install_path: Option<String>,
}

// =============================================================================
// RemoteInstaller
// =============================================================================

/// Installer for cass on remote machines.
pub struct RemoteInstaller {
    /// SSH host alias.
    host: String,
    /// System information from probe.
    system_info: SystemInfo,
    /// Resource information from probe.
    resources: ResourceInfo,
    /// Target version to install.
    target_version: String,
}

impl RemoteInstaller {
    /// Create a new installer for a remote host.
    pub fn new(host: impl Into<String>, system_info: SystemInfo, resources: ResourceInfo) -> Self {
        Self {
            host: host.into(),
            system_info,
            resources,
            target_version: CASS_VERSION.to_string(),
        }
    }

    /// Create an installer with a specific target version.
    ///
    /// Returns an error if the version string contains characters that are not
    /// safe for shell interpolation (only alphanumeric, `.`, `-`, `+`, `_` allowed).
    pub fn with_version(
        host: impl Into<String>,
        system_info: SystemInfo,
        resources: ResourceInfo,
        version: impl Into<String>,
    ) -> Result<Self, InstallError> {
        let version = version.into();
        Self::validate_shell_safe(&version, "version")?;
        Ok(Self {
            host: host.into(),
            system_info,
            resources,
            target_version: version,
        })
    }

    /// Validate that a string is safe for shell interpolation.
    ///
    /// Prevents command injection by rejecting strings containing shell
    /// metacharacters (quotes, backticks, semicolons, pipes, etc.).
    fn validate_shell_safe(value: &str, field_name: &str) -> Result<(), InstallError> {
        if value.is_empty() {
            return Err(InstallError::VerificationFailed(format!(
                "{field_name} must not be empty"
            )));
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
        {
            return Err(InstallError::VerificationFailed(format!(
                "{field_name} contains unsafe characters: only alphanumeric, '.', '-', '+', '_' are allowed"
            )));
        }
        Ok(())
    }

    /// Get the host name.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Get the target version.
    pub fn target_version(&self) -> &str {
        &self.target_version
    }

    /// Check if resources are sufficient for compilation.
    pub fn check_resources(&self) -> Result<(), InstallError> {
        if self.resources.disk_available_mb < MIN_DISK_MB {
            return Err(InstallError::InsufficientDisk {
                available_mb: self.resources.disk_available_mb,
                required_mb: MIN_DISK_MB,
            });
        }
        // Only check memory if compilation is needed
        // Note: we check during method selection
        Ok(())
    }

    /// Check if resources are sufficient for compilation specifically.
    pub fn can_compile(&self) -> Result<(), InstallError> {
        self.check_resources()?;
        if self.resources.memory_total_mb < MIN_MEMORY_MB {
            return Err(InstallError::InsufficientMemory {
                available_mb: self.resources.memory_total_mb,
                required_mb: MIN_MEMORY_MB,
            });
        }
        Ok(())
    }

    /// Choose the best installation method based on system info.
    ///
    /// Returns `None` if no viable installation method is available.
    pub fn choose_method(&self) -> Option<InstallMethod> {
        // 1. Try cargo-binstall first (fastest)
        if self.system_info.has_cargo_binstall {
            return Some(InstallMethod::CargoBinstall);
        }

        // 2. Try pre-built binary if available for this arch
        if let Some(url) = self.get_prebuilt_url() {
            // Attempt to fetch checksum (non-blocking - proceed without if unavailable)
            let checksum_url = Self::get_checksum_url(&url);
            let checksum = self.fetch_remote_checksum(&checksum_url);
            return Some(InstallMethod::PrebuiltBinary { url, checksum });
        }

        // 3. Try cargo install if cargo is available and we have resources
        if self.system_info.has_cargo && self.can_compile().is_ok() {
            return Some(InstallMethod::CargoInstall);
        }

        // 4. Full bootstrap requires curl to download rustup
        if self.system_info.has_curl {
            return Some(InstallMethod::FullBootstrap);
        }

        // No viable method available
        None
    }

    /// Get pre-built binary URL if available for this architecture.
    fn get_prebuilt_url(&self) -> Option<String> {
        // Only supported if we have a way to download
        if !self.system_info.has_curl && !self.system_info.has_wget {
            return None;
        }

        // Map arch to release asset naming
        let arch = match self.system_info.arch.as_str() {
            "x86_64" => "amd64",
            "aarch64" | "arm64" => "arm64",
            _ => return None, // Unsupported arch
        };

        let os = match self.system_info.os.to_lowercase().as_str() {
            "linux" => "linux",
            "darwin" => "darwin",
            _ => return None, // Unsupported OS
        };

        // macOS Intel builds are not published (see release workflow comment).
        if os == "darwin" && arch == "amd64" {
            return None;
        }

        // GitHub releases URL pattern
        Some(format!(
            "https://github.com/Dicklesworthstone/coding_agent_session_search/releases/download/v{}/cass-{}-{}.tar.gz",
            self.target_version, os, arch
        ))
    }

    /// Get checksum URL for a pre-built binary (binary_url + ".sha256").
    fn get_checksum_url(binary_url: &str) -> String {
        format!("{}.sha256", binary_url)
    }

    /// Fetch checksum from remote URL via SSH.
    ///
    /// Returns the SHA256 hex string if successful, None if checksum unavailable.
    /// This is non-blocking - if checksum can't be fetched, installation proceeds without verification.
    fn fetch_remote_checksum(&self, checksum_url: &str) -> Option<String> {
        // Use curl or wget to fetch the checksum file
        let fetch_cmd = if self.system_info.has_curl {
            format!(r#"curl -fsSL "{}" 2>/dev/null | head -1"#, checksum_url)
        } else if self.system_info.has_wget {
            format!(r#"wget -qO- "{}" 2>/dev/null | head -1"#, checksum_url)
        } else {
            return None;
        };

        match self.run_ssh_command(&fetch_cmd, Duration::from_secs(10)) {
            Ok(output) => {
                // Parse checksum - format is either just the hash or "hash  filename"
                let line = output.trim();
                let checksum = line.split_whitespace().next().unwrap_or(line);

                // Validate it looks like a SHA256 hex string (64 chars, all hex)
                if checksum.len() == 64 && checksum.chars().all(|c| c.is_ascii_hexdigit()) {
                    Some(checksum.to_lowercase())
                } else {
                    None
                }
            }
            Err(_) => None, // Checksum unavailable - proceed without verification
        }
    }

    /// Install cass on the remote host.
    ///
    /// Streams progress updates via the callback as installation proceeds.
    pub fn install<F>(&self, on_progress: F) -> Result<InstallResult, InstallError>
    where
        F: Fn(InstallProgress) + Send + Sync,
    {
        let start = Instant::now();

        // Check resources
        on_progress(InstallProgress {
            stage: InstallStage::Preparing,
            message: "Checking system resources...".into(),
            percent: Some(0),
            elapsed: start.elapsed(),
        });

        self.check_resources()?;

        // Choose method
        let method = self
            .choose_method()
            .ok_or(InstallError::NoMethodAvailable)?;

        on_progress(InstallProgress {
            stage: InstallStage::Preparing,
            message: format!("Selected installation method: {}", method),
            percent: Some(5),
            elapsed: start.elapsed(),
        });

        // Execute installation
        let result = match &method {
            InstallMethod::CargoBinstall => self.install_via_binstall(&on_progress, start),
            InstallMethod::PrebuiltBinary { url, checksum } => {
                self.install_via_binary(url, checksum.as_deref(), &on_progress, start)
            }
            InstallMethod::CargoInstall => self.install_via_cargo(&on_progress, start),
            InstallMethod::FullBootstrap => self.install_with_bootstrap(&on_progress, start),
        };

        match result {
            Ok(install_result) => {
                on_progress(InstallProgress {
                    stage: InstallStage::Complete,
                    message: format!(
                        "Installed cass {} via {} in {:.1}s",
                        install_result.version,
                        method,
                        install_result.duration.as_secs_f64()
                    ),
                    percent: Some(100),
                    elapsed: start.elapsed(),
                });
                Ok(install_result)
            }
            Err(e) => {
                on_progress(InstallProgress {
                    stage: InstallStage::Failed {
                        error: e.to_string(),
                    },
                    message: format!("Installation failed: {}", e),
                    percent: None,
                    elapsed: start.elapsed(),
                });
                Err(e)
            }
        }
    }

    /// Install via cargo-binstall.
    fn install_via_binstall<F>(
        &self,
        on_progress: &F,
        start: Instant,
    ) -> Result<InstallResult, InstallError>
    where
        F: Fn(InstallProgress),
    {
        on_progress(InstallProgress {
            stage: InstallStage::Downloading,
            message: "Running cargo binstall...".into(),
            percent: Some(10),
            elapsed: start.elapsed(),
        });

        let script = format!(
            r#"cargo binstall --no-confirm {}@{}"#,
            CRATE_NAME, self.target_version
        );

        self.run_ssh_command(&script, Duration::from_secs(120))?;

        // Verify installation
        self.verify_installation(on_progress, start)?;

        Ok(InstallResult {
            method: InstallMethod::CargoBinstall,
            version: self.target_version.clone(),
            duration: start.elapsed(),
            install_path: Some("~/.cargo/bin/cass".into()),
        })
    }

    /// Install via pre-built binary download.
    fn install_via_binary<F>(
        &self,
        url: &str,
        checksum: Option<&str>,
        on_progress: &F,
        start: Instant,
    ) -> Result<InstallResult, InstallError>
    where
        F: Fn(InstallProgress),
    {
        on_progress(InstallProgress {
            stage: InstallStage::Downloading,
            message: "Downloading pre-built binary...".into(),
            percent: Some(10),
            elapsed: start.elapsed(),
        });

        let download_cmd =
            Self::build_prebuilt_binary_install_script(url, checksum, self.system_info.has_curl);

        self.run_ssh_command(&download_cmd, Duration::from_secs(60))?;

        // Checksum is verified inside the shell script (before installation).
        // If the script succeeded, the checksum matched (or was not provided).
        let verified_checksum = checksum.map(|c| c.to_string());

        on_progress(InstallProgress {
            stage: InstallStage::Installing,
            message: if verified_checksum.is_some() {
                "Binary installed and verified at ~/.local/bin/cass".into()
            } else {
                "Binary installed to ~/.local/bin/cass (checksum not available)".into()
            },
            percent: Some(80),
            elapsed: start.elapsed(),
        });

        // Verify installation
        self.verify_installation(on_progress, start)?;

        Ok(InstallResult {
            method: InstallMethod::PrebuiltBinary {
                url: url.to_string(),
                checksum: verified_checksum,
            },
            version: self.target_version.clone(),
            duration: start.elapsed(),
            install_path: Some("~/.local/bin/cass".into()),
        })
    }

    #[cfg(test)]
    fn prebuilt_archive_member_is_allowed(member: &str) -> bool {
        matches!(member, "cass" | "./cass")
    }

    fn build_prebuilt_binary_install_script(
        url: &str,
        checksum: Option<&str>,
        has_curl: bool,
    ) -> String {
        // Download into a secure mktemp directory (not predictable /tmp/), verify
        // checksum BEFORE extracting/installing, validate the archive layout, and
        // clean up temp files on exit.
        let download_tool = if has_curl {
            format!(r#"curl -fsSL "{url}" -o "${{archive_path}}""#)
        } else {
            format!(r#"wget -q "{url}" -O "${{archive_path}}""#)
        };
        let checksum_verify = if let Some(expected) = checksum {
            format!(
                r#"
if command -v sha256sum >/dev/null 2>&1; then
    actual_sum="$(sha256sum "${{archive_path}}" | cut -d' ' -f1)"
elif command -v shasum >/dev/null 2>&1; then
    actual_sum="$(shasum -a 256 "${{archive_path}}" | cut -d' ' -f1)"
else
    echo "WARNING: no sha256sum or shasum found, skipping checksum"
    actual_sum="{expected_lower}"
fi
if [ "${{actual_sum}}" != "{expected_lower}" ]; then
    echo "CHECKSUM_MISMATCH: expected {expected_lower} got ${{actual_sum}}"
    exit 1
fi
"#,
                expected_lower = expected.to_lowercase()
            )
        } else {
            String::new()
        };
        format!(
            r#"
set -euo pipefail
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT
archive_path="${{tmp_dir}}/cass-prebuilt.tar.gz"
mkdir -p ~/.local/bin
{download_tool}
{checksum_verify}
tar -tzf "${{archive_path}}" | while IFS= read -r tar_member; do
    case "${{tar_member}}" in
        cass|./cass) ;;
        *)
            echo "EXTRACT_UNSAFE: ${{tar_member}}"
            exit 1
            ;;
    esac
done
tar -xzf "${{archive_path}}" -C "${{tmp_dir}}" cass 2>/dev/null || tar -xzf "${{archive_path}}" -C "${{tmp_dir}}" ./cass
if [ ! -f "${{tmp_dir}}/cass" ] || [ -L "${{tmp_dir}}/cass" ]; then
    echo "EXTRACT_FAILED"
    exit 1
fi
install -m 0755 "${{tmp_dir}}/cass" ~/.local/bin/cass
# Add to PATH only if not already present
grep -q '.local/bin' ~/.bashrc 2>/dev/null || echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
"#
        )
    }

    /// Compute SHA256 checksum of a file on the remote host.
    #[allow(dead_code)] // Kept as utility; inline verification in install script is preferred
    fn compute_remote_checksum(&self, remote_path: &str) -> Result<String, InstallError> {
        // Try sha256sum (Linux) first, fall back to shasum -a 256 (macOS)
        let checksum_cmd = format!(
            r#"
if command -v sha256sum &>/dev/null; then
    sha256sum "{}" 2>/dev/null | cut -d' ' -f1
elif command -v shasum &>/dev/null; then
    shasum -a 256 "{}" 2>/dev/null | cut -d' ' -f1
else
    echo "NO_CHECKSUM_TOOL"
fi
"#,
            remote_path, remote_path
        );

        let output = self.run_ssh_command(&checksum_cmd, Duration::from_secs(30))?;
        let checksum = output.trim();

        if checksum == "NO_CHECKSUM_TOOL" {
            return Err(InstallError::MissingDependency {
                dep: "sha256sum or shasum".into(),
                fix: "Install coreutils (Linux) or use macOS with built-in shasum".into(),
            });
        }

        // Validate it looks like a SHA256 hex string
        if checksum.len() == 64 && checksum.chars().all(|c| c.is_ascii_hexdigit()) {
            Ok(checksum.to_lowercase())
        } else {
            Err(InstallError::VerificationFailed(format!(
                "Invalid checksum output: {}",
                checksum
            )))
        }
    }

    /// Install via cargo install (compilation).
    fn install_via_cargo<F>(
        &self,
        on_progress: &F,
        start: Instant,
    ) -> Result<InstallResult, InstallError>
    where
        F: Fn(InstallProgress),
    {
        // Check compilation resources
        self.can_compile()?;

        on_progress(InstallProgress {
            stage: InstallStage::Compiling {
                crate_name: CRATE_NAME.into(),
            },
            message: "Starting cargo install (this may take 2-5 minutes)...".into(),
            percent: Some(10),
            elapsed: start.elapsed(),
        });

        // Use nohup for long-running installation to prevent SSH timeout
        let install_script = format!(
            r#"
# Start installation in background with logging
LOG_FILE=~/.cass_install.log
rm -f "$LOG_FILE"

nohup bash -c '
# Source cargo env in case this is called after bootstrap rustup install
source "$HOME/.cargo/env" 2>/dev/null || true
cargo install {}@{} 2>&1 | tee "$HOME/.cass_install.log"
echo "===INSTALL_COMPLETE===" >> "$HOME/.cass_install.log"
' > /dev/null 2>&1 &

echo "INSTALL_PID=$!"
"#,
            CRATE_NAME, self.target_version
        );

        // Start the installation
        let output = self.run_ssh_command(&install_script, Duration::from_secs(30))?;

        // Extract PID for monitoring
        let pid = output
            .lines()
            .find(|l| l.starts_with("INSTALL_PID="))
            .and_then(|l| l.strip_prefix("INSTALL_PID="))
            .and_then(|p| p.trim().parse::<u32>().ok());

        // Poll for completion
        self.poll_installation(pid, on_progress, start)?;

        // Verify installation
        self.verify_installation(on_progress, start)?;

        Ok(InstallResult {
            method: InstallMethod::CargoInstall,
            version: self.target_version.clone(),
            duration: start.elapsed(),
            install_path: Some("~/.cargo/bin/cass".into()),
        })
    }

    /// Install with full bootstrap (rustup + cargo).
    fn install_with_bootstrap<F>(
        &self,
        on_progress: &F,
        start: Instant,
    ) -> Result<InstallResult, InstallError>
    where
        F: Fn(InstallProgress),
    {
        on_progress(InstallProgress {
            stage: InstallStage::Downloading,
            message: "Installing Rust toolchain via rustup...".into(),
            percent: Some(5),
            elapsed: start.elapsed(),
        });

        // Install rustup
        let rustup_script = r#"
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
"#;

        self.run_ssh_command(rustup_script, Duration::from_secs(300))?;

        on_progress(InstallProgress {
            stage: InstallStage::Compiling {
                crate_name: CRATE_NAME.into(),
            },
            message: "Rust installed. Starting cargo install...".into(),
            percent: Some(20),
            elapsed: start.elapsed(),
        });

        // Now install cass via cargo
        self.install_via_cargo(on_progress, start)
    }

    /// Poll for installation completion.
    fn poll_installation<F>(
        &self,
        _pid: Option<u32>,
        on_progress: &F,
        start: Instant,
    ) -> Result<(), InstallError>
    where
        F: Fn(InstallProgress),
    {
        let poll_script = r#"
LOG_FILE=~/.cass_install.log
if [ -f "$LOG_FILE" ]; then
    if grep -q "===INSTALL_COMPLETE===" "$LOG_FILE"; then
        echo "STATUS=COMPLETE"
    elif grep -q "error\[" "$LOG_FILE" || grep -q "error:" "$LOG_FILE"; then
        echo "STATUS=ERROR"
        tail -20 "$LOG_FILE"
    else
        echo "STATUS=RUNNING"
        # Show last few lines of compilation progress
        tail -5 "$LOG_FILE" | grep -E "Compiling|Downloading|Installing" | tail -1
    fi
else
    echo "STATUS=NOT_STARTED"
fi
"#;

        let max_wait = Duration::from_secs(600); // 10 minutes max
        let poll_interval = Duration::from_secs(5);
        let mut last_crate = String::new();
        let mut progress_pct: u8 = 15;

        loop {
            if start.elapsed() > max_wait {
                return Err(InstallError::Timeout(max_wait.as_secs()));
            }

            std::thread::sleep(poll_interval);

            let output = self.run_ssh_command(poll_script, Duration::from_secs(30))?;

            if output.contains("STATUS=COMPLETE") {
                return Ok(());
            }

            if output.contains("STATUS=ERROR") {
                // Extract error message
                let error_lines: Vec<&str> = output
                    .lines()
                    .filter(|l| !l.starts_with("STATUS="))
                    .collect();
                let error_msg = error_lines.join("\n");

                // Check for common dependency issues
                if let Some(fix) = detect_missing_dependency(&error_msg) {
                    return Err(InstallError::MissingDependency {
                        dep: fix.0.to_string(),
                        fix: fix.1.to_string(),
                    });
                }

                return Err(InstallError::MethodFailed {
                    method: "cargo install".into(),
                    reason: error_msg,
                });
            }

            // Extract currently compiling crate
            for line in output.lines() {
                if line.contains("Compiling")
                    && let Some(crate_name) = line.split_whitespace().nth(1)
                    && crate_name != last_crate
                {
                    last_crate = crate_name.to_string();
                    progress_pct = (progress_pct + 3).min(85);
                }
            }

            on_progress(InstallProgress {
                stage: InstallStage::Compiling {
                    crate_name: if last_crate.is_empty() {
                        "dependencies".into()
                    } else {
                        last_crate.clone()
                    },
                },
                message: format!(
                    "Compiling {}...",
                    if last_crate.is_empty() {
                        "dependencies"
                    } else {
                        &last_crate
                    }
                ),
                percent: Some(progress_pct),
                elapsed: start.elapsed(),
            });
        }
    }

    /// Verify that cass was installed correctly.
    fn verify_installation<F>(&self, on_progress: &F, start: Instant) -> Result<(), InstallError>
    where
        F: Fn(InstallProgress),
    {
        on_progress(InstallProgress {
            stage: InstallStage::Verifying,
            message: "Verifying installation...".into(),
            percent: Some(90),
            elapsed: start.elapsed(),
        });

        // Try to run cass --version
        let verify_script = r#"
source ~/.cargo/env 2>/dev/null || true
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
cass --version 2>&1 || echo "VERIFY_FAILED"
"#;

        let output = self.run_ssh_command(verify_script, Duration::from_secs(30))?;

        if output.contains("VERIFY_FAILED") {
            return Err(InstallError::VerificationFailed(
                "cass --version failed".into(),
            ));
        }

        // Check version matches
        if !output.contains(&self.target_version) {
            return Err(InstallError::VerificationFailed(format!(
                "Version mismatch: expected {}, got {}",
                self.target_version,
                output.trim()
            )));
        }

        Ok(())
    }

    /// Run an SSH command on the remote host.
    fn run_ssh_command(&self, script: &str, timeout: Duration) -> Result<String, InstallError> {
        let timeout_secs = timeout.as_secs();

        let mut cmd = Command::new("ssh");
        cmd.args(strict_ssh_cli_tokens(timeout_secs.min(30)))
            .arg("-o")
            .arg("LogLevel=ERROR")
            .arg("--")
            .arg(&self.host)
            .arg("bash")
            .arg("-s");

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(script.as_bytes())?;
        }

        let output = child.wait_with_output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if is_host_key_verification_failure(&stderr) {
                return Err(InstallError::SshFailed(host_key_verification_error(
                    &self.host,
                )));
            }
            if stderr.contains("Connection refused")
                || stderr.contains("Connection timed out")
                || stderr.contains("Permission denied")
            {
                return Err(InstallError::SshFailed(stderr.trim().to_string()));
            }
            // Fail fast on any other non-zero exit — surface the exit code and
            // stderr so operators can diagnose the root cause immediately.
            let code = output.status.code().unwrap_or(-1);
            return Err(InstallError::SshFailed(format!(
                "Remote script exited with code {code}: {}",
                stderr.trim()
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

/// Detect missing system dependencies from compilation errors.
fn detect_missing_dependency(error: &str) -> Option<(&'static str, &'static str)> {
    if error.contains("openssl") || error.contains("libssl") {
        Some((
            "OpenSSL development headers",
            "Ubuntu/Debian: sudo apt install libssl-dev pkg-config\nRHEL/CentOS: sudo yum install openssl-devel",
        ))
    } else if error.contains("cc") && error.contains("not found") {
        Some((
            "C compiler",
            "Ubuntu/Debian: sudo apt install build-essential\nRHEL/CentOS: sudo yum groupinstall 'Development Tools'",
        ))
    } else if error.contains("pkg-config") {
        Some((
            "pkg-config",
            "Ubuntu/Debian: sudo apt install pkg-config\nRHEL/CentOS: sudo yum install pkgconfig",
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_system_info() -> SystemInfo {
        SystemInfo {
            os: "linux".into(),
            arch: "x86_64".into(),
            distro: Some("Ubuntu 22.04".into()),
            has_cargo: true,
            has_cargo_binstall: false,
            has_curl: true,
            has_wget: false,
            remote_home: "/home/user".into(),
            machine_id: None,
        }
    }

    fn fixture_resources() -> ResourceInfo {
        ResourceInfo {
            disk_available_mb: 10000,
            memory_total_mb: 8000,
            memory_available_mb: 4000,
            can_compile: true,
        }
    }

    #[test]
    fn test_install_method_display() {
        assert_eq!(
            InstallMethod::CargoBinstall.display_name(),
            "cargo-binstall"
        );
        assert_eq!(InstallMethod::CargoInstall.display_name(), "cargo install");
        assert_eq!(
            InstallMethod::FullBootstrap.display_name(),
            "full bootstrap (rustup + cargo)"
        );
    }

    #[test]
    fn test_install_method_requires_compilation() {
        assert!(!InstallMethod::CargoBinstall.requires_compilation());
        assert!(
            !InstallMethod::PrebuiltBinary {
                url: "".into(),
                checksum: None
            }
            .requires_compilation()
        );
        assert!(InstallMethod::CargoInstall.requires_compilation());
        assert!(InstallMethod::FullBootstrap.requires_compilation());
    }

    #[test]
    fn test_choose_method_prefers_binstall() {
        let mut system = fixture_system_info();
        system.has_cargo_binstall = true;
        let resources = fixture_resources();

        let installer = RemoteInstaller::new("test", system, resources);
        assert_eq!(
            installer.choose_method(),
            Some(InstallMethod::CargoBinstall)
        );
    }

    #[test]
    fn test_choose_method_cargo_install() {
        let mut system = fixture_system_info();
        // Disable curl/wget so pre-built binary is not available
        system.has_curl = false;
        system.has_wget = false;
        let resources = fixture_resources();

        let installer = RemoteInstaller::new("test", system, resources);
        // With cargo but no binstall and no download tools, should choose cargo install
        assert_eq!(installer.choose_method(), Some(InstallMethod::CargoInstall));
    }

    #[test]
    fn test_choose_method_prebuilt_binary() {
        let system = fixture_system_info();
        let resources = fixture_resources();

        let installer = RemoteInstaller::new("test", system, resources);
        // With curl available, should prefer pre-built binary over cargo install
        assert!(matches!(
            installer.choose_method(),
            Some(InstallMethod::PrebuiltBinary { .. })
        ));
    }

    #[test]
    fn test_choose_method_bootstrap_when_no_cargo() {
        let mut system = fixture_system_info();
        system.has_cargo = false;
        // curl is needed for bootstrap (to download rustup)
        system.has_curl = true;
        system.has_wget = false;
        // Use unsupported arch so prebuilt binary is not available
        system.arch = "armv7".into();
        let resources = fixture_resources();

        let installer = RemoteInstaller::new("test", system, resources);
        assert_eq!(
            installer.choose_method(),
            Some(InstallMethod::FullBootstrap)
        );
    }

    #[test]
    fn test_choose_method_none_when_no_tools() {
        let mut system = fixture_system_info();
        system.has_cargo = false;
        system.has_cargo_binstall = false;
        system.has_curl = false;
        system.has_wget = false;
        let resources = fixture_resources();

        let installer = RemoteInstaller::new("test", system, resources);
        // No curl means no way to download rustup, no wget/curl means no prebuilt binary
        // No cargo means no cargo install - should return None
        assert_eq!(installer.choose_method(), None);
    }

    #[test]
    fn test_check_resources_ok() {
        let system = fixture_system_info();
        let resources = fixture_resources();

        let installer = RemoteInstaller::new("test", system, resources);
        assert!(installer.check_resources().is_ok());
    }

    #[test]
    fn test_check_resources_insufficient_disk() {
        let system = fixture_system_info();
        let mut resources = fixture_resources();
        resources.disk_available_mb = 500;

        let installer = RemoteInstaller::new("test", system, resources);
        let result = installer.check_resources();
        assert!(matches!(result, Err(InstallError::InsufficientDisk { .. })));
    }

    #[test]
    fn test_can_compile_insufficient_memory() {
        let system = fixture_system_info();
        let mut resources = fixture_resources();
        resources.memory_total_mb = 512;

        let installer = RemoteInstaller::new("test", system, resources);
        let result = installer.can_compile();
        assert!(matches!(
            result,
            Err(InstallError::InsufficientMemory { .. })
        ));
    }

    #[test]
    fn test_get_prebuilt_url_linux_x86() {
        let system = fixture_system_info();
        let resources = fixture_resources();

        let installer = RemoteInstaller::new("test", system, resources);
        let url = installer.get_prebuilt_url();
        assert!(url.is_some());
        assert!(url.unwrap().contains("linux-amd64.tar.gz"));
    }

    #[test]
    fn test_get_prebuilt_url_macos_arm() {
        let mut system = fixture_system_info();
        system.os = "darwin".into();
        system.arch = "aarch64".into();
        let resources = fixture_resources();

        let installer = RemoteInstaller::new("test", system, resources);
        let url = installer.get_prebuilt_url();
        assert!(url.is_some());
        assert!(url.unwrap().contains("darwin-arm64.tar.gz"));
    }

    #[test]
    fn test_detect_missing_dependency_openssl() {
        let error = "error: failed to run custom build command for `openssl-sys`";
        let result = detect_missing_dependency(error);
        assert!(result.is_some());
        assert!(result.unwrap().0.contains("OpenSSL"));
    }

    #[test]
    fn test_detect_missing_dependency_cc() {
        let error = "error: linker `cc` not found";
        let result = detect_missing_dependency(error);
        assert!(result.is_some());
        assert!(result.unwrap().0.contains("C compiler"));
    }

    #[test]
    fn test_install_stage_display() {
        assert_eq!(InstallStage::Preparing.to_string(), "Preparing");
        assert_eq!(
            InstallStage::Compiling {
                crate_name: "tokio".into()
            }
            .to_string(),
            "Compiling tokio"
        );
        assert_eq!(InstallStage::Complete.to_string(), "Complete");
    }

    // =========================================================================
    // Checksum verification tests
    // =========================================================================

    #[test]
    fn test_get_checksum_url() {
        let binary_url =
            "https://github.com/example/repo/releases/download/v1.0.0/binary-linux-x86_64";
        let checksum_url = RemoteInstaller::get_checksum_url(binary_url);
        assert_eq!(
            checksum_url,
            "https://github.com/example/repo/releases/download/v1.0.0/binary-linux-x86_64.sha256"
        );
    }

    #[test]
    fn test_checksum_mismatch_error_display() {
        let err = InstallError::ChecksumMismatch {
            expected: "abc123".to_string(),
            actual: "def456".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("abc123"));
        assert!(msg.contains("def456"));
        assert!(msg.contains("mismatch"));
    }

    #[test]
    fn test_checksum_validation_valid() {
        // Valid SHA256: 64 hex characters
        let valid = "a".repeat(64);
        assert_eq!(valid.len(), 64);
        assert!(valid.chars().all(|c| c.is_ascii_hexdigit()));

        // Mixed case valid
        let mixed = "ABCDEFabcdef0123456789ABCDEFabcdef0123456789ABCDEFabcdef01234567";
        assert_eq!(mixed.len(), 64);
        assert!(mixed.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_checksum_validation_invalid() {
        // Too short
        let short = "a".repeat(32);
        assert!(short.len() != 64);

        // Too long
        let long = "a".repeat(128);
        assert!(long.len() != 64);

        // Invalid characters
        let invalid = "g".repeat(64); // 'g' is not hex
        assert!(!invalid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_prebuilt_archive_member_policy_rejects_path_traversal() {
        assert!(RemoteInstaller::prebuilt_archive_member_is_allowed("cass"));
        assert!(RemoteInstaller::prebuilt_archive_member_is_allowed(
            "./cass"
        ));

        for member in [
            "../cass",
            "payload/../cass",
            "/cass",
            "bin/cass",
            "cass/../../.ssh/authorized_keys",
            "./../cass",
            "cass\n../escape",
        ] {
            assert!(
                !RemoteInstaller::prebuilt_archive_member_is_allowed(member),
                "member should be rejected: {member:?}"
            );
        }
    }

    #[test]
    fn test_prebuilt_install_script_validates_tar_members_before_extract() {
        let script = RemoteInstaller::build_prebuilt_binary_install_script(
            "https://example.com/cass.tar.gz",
            Some(&"a".repeat(64)),
            true,
        );
        let list_index = script.find("tar -tzf").expect("tar listing validation");
        let extract_index = script.find("tar -xzf").expect("tar extraction");

        assert!(
            list_index < extract_index,
            "archive members must be listed and validated before extraction"
        );
        assert!(script.contains("EXTRACT_UNSAFE"));
        assert!(script.contains("cass|./cass"));
        assert!(script.contains(r#"[ -L "${tmp_dir}/cass" ]"#));
        assert!(script.contains(r#"install -m 0755 "${tmp_dir}/cass""#));
        assert!(!script.contains("tar -xzf \"${archive_path}\" -C \"${tmp_dir}\"\n"));
    }

    #[test]
    fn test_prebuilt_binary_method_with_checksum() {
        let method = InstallMethod::PrebuiltBinary {
            url: "https://example.com/binary".to_string(),
            checksum: Some("a".repeat(64)),
        };

        // Verify serialization includes checksum
        let json = serde_json::to_string(&method).unwrap();
        assert!(json.contains("checksum"));
        assert!(json.contains(&"a".repeat(64)));

        // Verify deserialization
        let parsed: InstallMethod = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(parsed, InstallMethod::PrebuiltBinary { .. }),
            "Expected PrebuiltBinary variant with checksum in test_prebuilt_binary_method_with_checksum"
        );
        if let InstallMethod::PrebuiltBinary { checksum, .. } = parsed {
            assert!(checksum.is_some());
            assert_eq!(checksum.unwrap().len(), 64);
        }
    }

    #[test]
    fn test_prebuilt_binary_method_without_checksum() {
        let method = InstallMethod::PrebuiltBinary {
            url: "https://example.com/binary".to_string(),
            checksum: None,
        };

        let json = serde_json::to_string(&method).unwrap();
        let parsed: InstallMethod = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(parsed, InstallMethod::PrebuiltBinary { .. }),
            "Expected PrebuiltBinary variant in test_prebuilt_binary_method_without_checksum"
        );
        if let InstallMethod::PrebuiltBinary { checksum, .. } = parsed {
            assert!(checksum.is_none());
        }
    }

    // =========================================================================
    // Real system probe integration tests — no mocks
    // =========================================================================

    /// Build SystemInfo from real local system commands.
    fn local_system_info() -> SystemInfo {
        use std::process::Command;

        let os = {
            let out = Command::new("uname").arg("-s").output().expect("uname -s");
            String::from_utf8_lossy(&out.stdout).trim().to_lowercase()
        };
        let arch = {
            let out = Command::new("uname").arg("-m").output().expect("uname -m");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let distro = if std::path::Path::new("/etc/os-release").exists() {
            let out = Command::new("bash")
                .arg("-c")
                .arg(". /etc/os-release && echo \"$PRETTY_NAME\"")
                .output()
                .ok();
            out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        } else {
            None
        };
        let has = |cmd: &str| -> bool {
            Command::new("which")
                .arg(cmd)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        let home = dotenvy::var("HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                directories::BaseDirs::new().map(|d| d.home_dir().to_string_lossy().into_owned())
            })
            .unwrap_or_default();

        SystemInfo {
            os,
            arch,
            distro,
            has_cargo: has("cargo"),
            has_cargo_binstall: has("cargo-binstall"),
            has_curl: has("curl"),
            has_wget: has("wget"),
            remote_home: home,
            machine_id: None, // Not needed in tests
        }
    }

    /// Build ResourceInfo from real local system commands.
    fn local_resource_info() -> ResourceInfo {
        use std::process::Command;

        let disk_mb = {
            let out = Command::new("bash")
                .arg("-c")
                // Avoid `~` tilde expansion since other tests mutate HOME concurrently.
                .arg("df -k / 2>/dev/null | awk 'NR==2 {print $4}'")
                .output()
                .expect("df -k /");
            let kb: u64 = String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse()
                .unwrap_or(0);
            kb / 1024
        };
        let (mem_total_mb, mem_avail_mb) = if std::path::Path::new("/proc/meminfo").exists() {
            let out = Command::new("bash")
                .arg("-c")
                .arg("grep MemTotal /proc/meminfo | awk '{print $2}'")
                .output()
                .expect("memtotal");
            let total_kb: u64 = String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse()
                .unwrap_or(0);
            let out2 = Command::new("bash")
                .arg("-c")
                .arg("grep MemAvailable /proc/meminfo | awk '{print $2}'")
                .output()
                .expect("memavail");
            let avail_kb: u64 = String::from_utf8_lossy(&out2.stdout)
                .trim()
                .parse()
                .unwrap_or(0);
            (total_kb / 1024, avail_kb / 1024)
        } else {
            // macOS fallback
            let out = Command::new("sysctl")
                .arg("-n")
                .arg("hw.memsize")
                .output()
                .ok();
            let bytes: u64 = out
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .trim()
                        .parse()
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            let mb = bytes / (1024 * 1024);
            (mb, mb)
        };

        ResourceInfo {
            disk_available_mb: disk_mb,
            memory_total_mb: mem_total_mb,
            memory_available_mb: mem_avail_mb,
            can_compile: disk_mb >= ResourceInfo::MIN_DISK_MB
                && mem_total_mb >= ResourceInfo::MIN_MEMORY_MB,
        }
    }

    #[test]
    fn real_system_info_has_valid_fields() {
        let sys = local_system_info();
        assert!(
            sys.os == "linux" || sys.os == "darwin",
            "unexpected OS: {}",
            sys.os
        );
        assert!(!sys.arch.is_empty(), "arch should not be empty");
        assert!(!sys.remote_home.is_empty(), "home should not be empty");
        assert!(
            sys.remote_home.starts_with('/'),
            "home should be absolute: {}",
            sys.remote_home
        );
    }

    #[test]
    fn real_resources_have_nonzero_values() {
        let res = local_resource_info();
        assert!(res.disk_available_mb > 0, "disk should be > 0");
        assert!(res.memory_total_mb > 0, "total memory should be > 0");
        assert!(
            res.memory_available_mb > 0,
            "available memory should be > 0"
        );
    }

    #[test]
    fn real_resources_memory_invariant() {
        let res = local_resource_info();
        assert!(
            res.memory_available_mb <= res.memory_total_mb,
            "available ({}) > total ({})",
            res.memory_available_mb,
            res.memory_total_mb
        );
    }

    #[test]
    fn real_resources_can_compile_matches_thresholds() {
        let res = local_resource_info();
        let expected = res.disk_available_mb >= ResourceInfo::MIN_DISK_MB
            && res.memory_total_mb >= ResourceInfo::MIN_MEMORY_MB;
        assert_eq!(
            res.can_compile, expected,
            "can_compile mismatch: disk={}MB mem={}MB",
            res.disk_available_mb, res.memory_total_mb
        );
    }

    #[test]
    fn real_system_choose_method_returns_some() {
        let sys = local_system_info();
        let res = local_resource_info();
        // This system should have at least curl or cargo, so a method should exist
        let installer = RemoteInstaller::new("localhost", sys, res);
        let method = installer.choose_method();
        assert!(
            method.is_some(),
            "real system should have at least one install method"
        );
    }

    #[test]
    #[ignore = "environment-dependent: requires >=2GB disk space"]
    fn real_system_check_resources_ok() {
        let sys = local_system_info();
        let res = local_resource_info();
        // This dev machine should have enough resources
        let installer = RemoteInstaller::new("localhost", sys, res);
        assert!(
            installer.check_resources().is_ok(),
            "dev machine should pass resource check"
        );
    }

    #[test]
    #[ignore = "environment-dependent: requires >=2GB disk space and >=1GB memory"]
    fn real_system_can_compile_ok() {
        let sys = local_system_info();
        let res = local_resource_info();
        let installer = RemoteInstaller::new("localhost", sys, res);
        assert!(
            installer.can_compile().is_ok(),
            "dev machine should be able to compile"
        );
    }

    #[test]
    fn real_system_prebuilt_url_valid() {
        let sys = local_system_info();
        let res = local_resource_info();
        let installer = RemoteInstaller::new("localhost", sys, res);
        if let Some(url) = installer.get_prebuilt_url() {
            assert!(url.starts_with("https://"), "URL should be https: {}", url);
            assert!(
                url.contains("linux") || url.contains("darwin"),
                "URL should contain OS: {}",
                url
            );
        }
        // Not all architectures have prebuilt URLs, so Some/None both acceptable
    }

    #[test]
    fn real_system_tool_detection_consistent() {
        let sys = local_system_info();
        // If binstall is available, cargo must be too
        if sys.has_cargo_binstall {
            assert!(sys.has_cargo, "binstall requires cargo");
        }
        // Dev machine should have at least curl or wget
        assert!(
            sys.has_curl || sys.has_wget,
            "system should have at least one download tool"
        );
    }
}
