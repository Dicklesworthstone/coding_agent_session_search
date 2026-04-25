#![allow(dead_code)] // Test utilities may not all be used in every test
//! SSH test helper for integration tests.
//!
//! This module provides utilities for running integration tests against a real
//! SSH server in a Docker container. Tests that require this infrastructure
//! should be marked with `#[ignore = "requires Docker"]` and run explicitly
//! with `cargo test -- --ignored`.
//!
//! # Example
//!
//! ```rust,ignore
//! use crate::ssh_test_helper::SshTestServer;
//!
//! #[test]
//! #[ignore = "requires Docker"]
//! fn test_ssh_sync() {
//!     let server = SshTestServer::start().expect("SSH server should start");
//!     // Use server.ssh_target() for connections
//! }
//! ```

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Counter for unique container names across test runs.
static CONTAINER_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Errors that can occur during SSH test setup.
#[derive(Debug, thiserror::Error)]
pub enum SshTestError {
    #[error("Docker is not available")]
    DockerNotAvailable,
    #[error("Failed to start container: {0}")]
    ContainerStartFailed(String),
    #[error("Failed to generate SSH key: {0}")]
    SshKeyGenFailed(String),
    #[error("SSH connection failed: {0}")]
    SshConnectionFailed(String),
    #[error("Command failed: {0}")]
    CommandFailed(String),
    #[error("Operation timed out")]
    Timeout,
}

/// RAII guard that manages an SSH test server container.
///
/// The container is automatically stopped and removed when this struct is dropped.
#[allow(dead_code)] // Fields are held for RAII or debugging purposes
pub struct SshTestServer {
    container_id: String,
    container_name: String,
    host: String,
    port: u16,
    key_dir: tempfile::TempDir,
    private_key_path: PathBuf,
}

impl SshTestServer {
    /// Docker image name for the SSH test server.
    const IMAGE_NAME: &'static str = "cass-ssh-test:latest";

    /// Start a new SSH test server container.
    ///
    /// This builds the Docker image if needed, starts a container with a unique name,
    /// generates an ephemeral SSH key pair, and waits for SSH to be ready.
    ///
    /// # Errors
    ///
    /// Returns an error if Docker is not available, the container fails to start,
    /// or SSH doesn't become ready within the timeout.
    pub fn start() -> Result<Self, SshTestError> {
        Self::start_with_timeout(Duration::from_secs(60))
    }

    /// Start with a custom timeout for SSH readiness.
    pub fn start_with_timeout(timeout: Duration) -> Result<Self, SshTestError> {
        // Check Docker is available
        if !Self::is_docker_available() {
            return Err(SshTestError::DockerNotAvailable);
        }

        // Build image if needed
        Self::ensure_image_built()?;

        // Generate ephemeral SSH key
        let key_dir = tempfile::TempDir::new().map_err(|e| {
            SshTestError::SshKeyGenFailed(format!("Failed to create temp dir: {}", e))
        })?;
        let private_key_path = key_dir.path().join("id_ed25519");
        let public_key_path = key_dir.path().join("id_ed25519.pub");

        let keygen_output = Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-f",
                private_key_path.to_str().unwrap(),
                "-N",
                "",
                "-q",
            ])
            .output()
            .map_err(|e| {
                SshTestError::SshKeyGenFailed(format!("Failed to run ssh-keygen: {}", e))
            })?;

        if !keygen_output.status.success() {
            return Err(SshTestError::SshKeyGenFailed(
                String::from_utf8_lossy(&keygen_output.stderr).to_string(),
            ));
        }

        // Read public key
        let public_key = std::fs::read_to_string(&public_key_path).map_err(|e| {
            SshTestError::SshKeyGenFailed(format!("Failed to read public key: {}", e))
        })?;

        // Start container with unique name
        let counter = CONTAINER_COUNTER.fetch_add(1, Ordering::SeqCst);
        let container_name = format!("cass-ssh-test-{}-{}", std::process::id(), counter);

        let start_output = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--name",
                &container_name,
                "-p",
                "0:22", // Dynamic port assignment
                "-e",
                &format!("SSH_AUTHORIZED_KEY={}", public_key.trim()),
                Self::IMAGE_NAME,
            ])
            .output()
            .map_err(|e| {
                SshTestError::ContainerStartFailed(format!("Failed to run docker: {}", e))
            })?;

        if !start_output.status.success() {
            return Err(SshTestError::ContainerStartFailed(
                String::from_utf8_lossy(&start_output.stderr).to_string(),
            ));
        }

        let container_id = String::from_utf8_lossy(&start_output.stdout)
            .trim()
            .to_string();

        // Get the assigned port
        let port = Self::get_container_port(&container_name)?;

        let server = Self {
            container_id,
            container_name,
            host: "127.0.0.1".to_string(),
            port,
            key_dir,
            private_key_path,
        };

        // Wait for SSH to be ready
        server.wait_for_ssh(timeout)?;

        Ok(server)
    }

    /// Check if Docker is available.
    fn is_docker_available() -> bool {
        Command::new("docker")
            .arg("version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Ensure the test image is built.
    fn ensure_image_built() -> Result<(), SshTestError> {
        // Check if image exists
        let inspect = Command::new("docker")
            .args(["image", "inspect", Self::IMAGE_NAME])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if inspect {
            return Ok(());
        }

        // Build the image
        let build_output = Command::new("docker")
            .args([
                "build",
                "-t",
                Self::IMAGE_NAME,
                "-f",
                "tests/docker/Dockerfile.sshd",
                "tests/docker/",
            ])
            .output()
            .map_err(|e| {
                SshTestError::ContainerStartFailed(format!("Failed to build image: {}", e))
            })?;

        if !build_output.status.success() {
            return Err(SshTestError::ContainerStartFailed(format!(
                "Image build failed: {}",
                String::from_utf8_lossy(&build_output.stderr)
            )));
        }

        Ok(())
    }

    /// Get the host port mapped to container port 22.
    fn get_container_port(container_name: &str) -> Result<u16, SshTestError> {
        let output = Command::new("docker")
            .args(["port", container_name, "22/tcp"])
            .output()
            .map_err(|e| {
                SshTestError::ContainerStartFailed(format!("Failed to get port: {}", e))
            })?;

        if !output.status.success() {
            return Err(SshTestError::ContainerStartFailed(
                "Failed to get container port".to_string(),
            ));
        }

        // Parse "0.0.0.0:12345" or ":::12345" format
        let port_str = String::from_utf8_lossy(&output.stdout);
        let port = port_str
            .trim()
            .split(':')
            .next_back()
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| {
                SshTestError::ContainerStartFailed(format!("Invalid port format: {}", port_str))
            })?;

        Ok(port)
    }

    /// Wait for SSH to become ready.
    fn wait_for_ssh(&self, timeout: Duration) -> Result<(), SshTestError> {
        let start = Instant::now();

        while start.elapsed() < timeout {
            if self.check_ssh_ready() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(500));
        }

        Err(SshTestError::Timeout)
    }

    /// Check if SSH is accepting connections.
    fn check_ssh_ready(&self) -> bool {
        Command::new("ssh")
            .args(self.ssh_base_args())
            .arg(self.ssh_user_host())
            .arg("echo ready")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Get base SSH arguments (timeout, key, no host key checking).
    fn ssh_base_args(&self) -> Vec<String> {
        vec![
            "-o".to_string(),
            "StrictHostKeyChecking=no".to_string(),
            "-o".to_string(),
            "UserKnownHostsFile=/dev/null".to_string(),
            "-o".to_string(),
            "ConnectTimeout=5".to_string(),
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-i".to_string(),
            self.private_key_path.to_string_lossy().to_string(),
            "-p".to_string(),
            self.port.to_string(),
        ]
    }

    /// Get user@host string.
    fn ssh_user_host(&self) -> String {
        format!("root@{}", self.host)
    }

    /// Get the SSH connection target for use with SyncEngine.
    ///
    /// This returns a string suitable for rsync/SSH, e.g., "root@127.0.0.1".
    /// The port is handled via SSH config or -p option.
    pub fn ssh_target(&self) -> String {
        self.ssh_user_host()
    }

    /// Get the SSH port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Get the path to the private key.
    pub fn private_key_path(&self) -> &PathBuf {
        &self.private_key_path
    }

    /// Get the remote home directory (always /root for the test container).
    pub fn remote_home(&self) -> &str {
        "/root"
    }

    /// Execute a command on the SSH server.
    pub fn ssh_exec(&self, cmd: &str) -> Result<String, SshTestError> {
        let output = Command::new("ssh")
            .args(self.ssh_base_args())
            .arg(self.ssh_user_host())
            .arg(cmd)
            .output()
            .map_err(|e| SshTestError::CommandFailed(format!("Failed to execute ssh: {}", e)))?;

        if !output.status.success() {
            return Err(SshTestError::CommandFailed(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Execute a command on the SSH server, piping stdin.
    pub fn ssh_exec_with_stdin(&self, stdin_data: &str) -> Result<String, SshTestError> {
        let mut child = Command::new("ssh")
            .args(self.ssh_base_args())
            .arg(self.ssh_user_host())
            .arg("bash -s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| SshTestError::CommandFailed(format!("Failed to spawn ssh: {}", e)))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(stdin_data.as_bytes()).map_err(|e| {
                SshTestError::CommandFailed(format!("Failed to write stdin: {}", e))
            })?;
        }

        let output = child
            .wait_with_output()
            .map_err(|e| SshTestError::CommandFailed(format!("Failed to wait for ssh: {}", e)))?;

        if !output.status.success() {
            return Err(SshTestError::CommandFailed(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Get the rsync destination for a path.
    ///
    /// Returns a string like "root@127.0.0.1:/root/.claude".
    pub fn rsync_target(&self, path: &str) -> String {
        format!("{}:{}", self.ssh_user_host(), path)
    }

    /// Build rsync SSH options string.
    pub fn rsync_ssh_opts(&self) -> String {
        format!(
            "ssh -p {} -i {} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes",
            self.port,
            self.private_key_path.to_string_lossy()
        )
    }
}

impl Drop for SshTestServer {
    fn drop(&mut self) {
        // Stop and remove the container
        let _ = Command::new("docker")
            .args(["stop", &self.container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        // Container is --rm so it's removed automatically
    }
}

/// Helper function to check if Docker is available for tests.
pub fn docker_available() -> bool {
    Command::new("docker")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_docker_available_check() {
        // This test just verifies the function doesn't panic
        let _ = docker_available();
    }
}
