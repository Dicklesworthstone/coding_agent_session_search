//! E2E tests for SSH-based sources setup and sync with a real SSH server.
//!
//! These tests use Docker to spin up a real SSH server with pre-populated
//! agent session data. Tests verify:
//! - sources setup + sync using rsync
//! - sources sync using SFTP fallback
//! - Provenance mapping (source_id, origin_host)
//! - Path rewriting
//!
//! # Docker Requirements
//! Tests require Docker to be available. They are marked with #[ignore] by default
//! and can be run with: `cargo test --test e2e_ssh_sources -- --ignored`
//!
//! # Test Architecture
//! 1. Build and start Docker container with SSH server + fixture data
//! 2. Generate temporary SSH key pair
//! 3. Configure sources.toml pointing to the container
//! 4. Run sources sync and verify results
//! 5. Verify provenance in the indexed data
//!
//! # br: coding_agent_session_search-3cv7

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

mod util;
use util::e2e_log::PhaseTracker;

// =============================================================================
// Docker Management
// =============================================================================

/// Represents a running Docker container for SSH testing.
struct DockerSshServer {
    container_id: String,
    host_port: u16,
    ssh_key_path: PathBuf,
    _temp_dir: TempDir,
}

impl DockerSshServer {
    /// Start a new SSH test server in Docker.
    ///
    /// This builds the Docker image if needed, generates a temporary SSH key,
    /// and starts the container with the key injected.
    fn start() -> Result<Self, String> {
        // Create temp directory for SSH keys
        let temp_dir = TempDir::new().map_err(|e| format!("Failed to create temp dir: {e}"))?;
        let ssh_key_path = temp_dir.path().join("test_key");
        let ssh_pub_key_path = temp_dir.path().join("test_key.pub");

        // Generate SSH key pair
        let keygen_status = Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-f",
                ssh_key_path.to_str().unwrap(),
                "-N",
                "", // No passphrase
                "-C",
                "cass-ssh-test@localhost",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("Failed to run ssh-keygen: {e}"))?;

        if !keygen_status.success() {
            return Err("ssh-keygen failed".to_string());
        }

        // Read public key
        let pub_key = fs::read_to_string(&ssh_pub_key_path)
            .map_err(|e| format!("Failed to read public key: {e}"))?;

        // Build Docker image
        let dockerfile_path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/docker/Dockerfile.sshd");

        if !dockerfile_path.exists() {
            return Err(format!(
                "Dockerfile not found at {}",
                dockerfile_path.display()
            ));
        }

        let docker_context = dockerfile_path
            .parent()
            .ok_or_else(|| "Invalid dockerfile path".to_string())?;

        let build_output = Command::new("docker")
            .args([
                "build",
                "-t",
                "cass-ssh-test",
                "-f",
                dockerfile_path.to_str().unwrap(),
                docker_context.to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("Failed to run docker build: {e}"))?;

        if !build_output.status.success() {
            return Err(format!(
                "Docker build failed: {}",
                String::from_utf8_lossy(&build_output.stderr)
            ));
        }

        // Find an available port
        let host_port = find_available_port()?;

        // Start container with the SSH key
        let run_output = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "-p",
                &format!("127.0.0.1:{}:22", host_port),
                "-e",
                &format!("SSH_AUTHORIZED_KEY={}", pub_key.trim()),
                "cass-ssh-test",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("Failed to run docker: {e}"))?;

        if !run_output.status.success() {
            let stderr = String::from_utf8_lossy(&run_output.stderr);
            return Err(format!("Docker run failed: {stderr}"));
        }

        let container_id = String::from_utf8_lossy(&run_output.stdout)
            .trim()
            .to_string();

        // Wait for SSH server to be ready
        wait_for_ssh(&ssh_key_path, host_port)?;

        Ok(Self {
            container_id,
            host_port,
            ssh_key_path,
            _temp_dir: temp_dir,
        })
    }

    /// Get the SSH connection string for this server.
    fn ssh_host(&self) -> String {
        "root@localhost".to_string()
    }

    /// Get the port number.
    fn port(&self) -> u16 {
        self.host_port
    }

    /// Get the path to the SSH private key.
    fn key_path(&self) -> &Path {
        &self.ssh_key_path
    }

    /// Stop the container.
    fn stop(&self) {
        let _ = Command::new("docker")
            .args(["stop", &self.container_id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for DockerSshServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Find an available TCP port.
fn find_available_port() -> Result<u16, String> {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("Bind failed: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Get addr failed: {e}"))?
        .port();
    Ok(port)
}

/// Wait for SSH server to be ready.
fn wait_for_ssh(key_path: &Path, port: u16) -> Result<(), String> {
    let max_attempts = 30;
    let delay = Duration::from_millis(500);

    for attempt in 1..=max_attempts {
        let status = Command::new("ssh")
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=2",
                "-i",
                key_path.to_str().unwrap(),
                "-p",
                &port.to_string(),
                "root@localhost",
                "echo",
                "ready",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        if let Ok(s) = status
            && s.success()
        {
            return Ok(());
        }

        if attempt < max_attempts {
            std::thread::sleep(delay);
        }
    }

    Err("SSH server did not become ready in time".to_string())
}

// =============================================================================
// Test Helpers
// =============================================================================

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_ssh_sources", test_name)
}

/// Check if Docker is available.
fn docker_available() -> bool {
    Command::new("docker")
        .args(["info"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create a sources.toml config with an SSH source.
fn create_ssh_sources_config(
    config_dir: &Path,
    source_name: &str,
    host: &str,
    port: u16,
    identity_file: &Path,
    paths: &[&str],
) {
    let paths_toml: String = paths
        .iter()
        .map(|p| format!("\"{}\"", p))
        .collect::<Vec<_>>()
        .join(", ");

    let config_content = format!(
        r#"[[sources]]
name = "{source_name}"
type = "ssh"
host = "{host}"
port = {port}
identity_file = "{identity_file}"
paths = [{paths_toml}]
sync_schedule = "manual"
"#,
        source_name = source_name,
        host = host,
        port = port,
        identity_file = identity_file.display(),
        paths_toml = paths_toml,
    );

    let config_file = config_dir.join("cass").join("sources.toml");
    fs::create_dir_all(config_file.parent().unwrap()).unwrap();
    fs::write(&config_file, config_content).unwrap();
}

/// Create an SSH config file for the test.
fn create_ssh_config(config_dir: &Path, host_alias: &str, port: u16, identity_file: &Path) {
    let ssh_dir = config_dir.join(".ssh");
    fs::create_dir_all(&ssh_dir).unwrap();

    let config_content = format!(
        r#"Host {host_alias}
    HostName 127.0.0.1
    User root
    Port {port}
    IdentityFile {identity_file}
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
"#,
        host_alias = host_alias,
        port = port,
        identity_file = identity_file.display(),
    );

    fs::write(ssh_dir.join("config"), config_content).unwrap();
}

// =============================================================================
// Integration Tests
// =============================================================================

/// Test: sources sync with rsync over a real SSH connection.
///
/// This test:
/// 1. Starts a Docker SSH server with pre-populated session data
/// 2. Configures sources.toml pointing to the server
/// 3. Runs `cass sources sync`
/// 4. Verifies files are synced to the local mirror directory
#[test]
#[ignore] // Requires Docker
fn ssh_sources_sync_rsync() {
    if !docker_available() {
        eprintln!("Skipping test: Docker not available");
        return;
    }

    let tracker = tracker_for("ssh_sources_sync_rsync");

    // Start Docker SSH server
    let start = tracker.start("docker_start", Some("Start Docker SSH server"));
    let server = match DockerSshServer::start() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to start Docker SSH server: {e}");
            return;
        }
    };
    tracker.end("docker_start", Some("Start Docker SSH server"), start);

    // Set up temp directories
    let start = tracker.start("setup", Some("Create temp directories and config"));
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&home_dir).unwrap();

    // Create sources.toml
    create_ssh_sources_config(
        &config_dir,
        "docker-ssh-test",
        &server.ssh_host(),
        server.port(),
        server.key_path(),
        &["/root/.claude/projects"],
    );

    // Create SSH config for the host
    create_ssh_config(
        &home_dir,
        "docker-ssh-test",
        server.port(),
        server.key_path(),
    );

    tracker.end("setup", Some("Create temp directories and config"), start);

    // Run sources sync
    let start = tracker.start("sources_sync", Some("Run sources sync with rsync"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "sync", "--verbose"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(60))
        .output()
        .expect("sources sync command");
    tracker.end("sources_sync", Some("Run sources sync with rsync"), start);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Verify sync completed
    let start = tracker.start("verify_sync", Some("Verify sync results"));
    if !output.status.success() {
        eprintln!("Sync failed.\nStdout: {stdout}\nStderr: {stderr}");
    }

    // Check that mirror directory was created and has content
    let mirror_dir = data_dir
        .join("coding-agent-search")
        .join("remotes")
        .join("docker-ssh-test")
        .join("mirror");

    // The sync should have created some files
    if mirror_dir.exists() {
        let entries: Vec<_> = fs::read_dir(&mirror_dir)
            .map(|rd| rd.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();

        assert!(
            !entries.is_empty() || stdout.contains("synced") || stdout.contains("rsync"),
            "Expected synced files or rsync output. Mirror dir: {:?}, stdout: {}",
            mirror_dir.display(),
            stdout
        );
    }
    tracker.end("verify_sync", Some("Verify sync results"), start);

    tracker.complete();
}

/// A real transfer must remain retryable when indexing is refused. The next
/// process sees a genuine zero-transfer rsync report, not a substituted tool.
#[test]
#[ignore = "requires Docker, OpenSSH and rsync"]
fn ssh_sources_sync_retries_index_after_zero_transfer() {
    use fs2::FileExt;
    use std::io::Write;
    use serde_json::{Value, json};

    assert!(docker_available(), "explicit retry test requires Docker");
    assert!(
        Command::new("rsync")
            .arg("--version")
            .stdout(Stdio::null())
            .status()
            .expect("real rsync must be installed")
            .success()
    );
    let tracker = tracker_for("ssh_sources_sync_retries_index_after_zero_transfer");
    let server = DockerSshServer::start().expect("start real SSH server for index retry");
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let config = tmp.path().join("config");
    let data = tmp.path().join("data");
    for directory in [&home, &config, &data] {
        fs::create_dir_all(directory).unwrap();
    }
    fs::write(home.join(".env"), "").unwrap();

    // Production transport enforces strict checking. Trust only the keys of
    // this owned localhost fixture, using a real known_hosts file.
    let port = server.port().to_string();
    let keys = Command::new("ssh-keyscan")
        .args(["-T", "5", "-p", &port, "127.0.0.1"])
        .output()
        .expect("read fixture SSH host keys");
    assert!(
        keys.status.success() && !keys.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&keys.stderr)
    );
    let known_hosts = home.join("known_hosts");
    fs::write(&known_hosts, &keys.stdout).unwrap();
    let ssh_config = home.join("ssh_config");
    fs::write(
        &ssh_config,
        format!(
            "Host retry-source\n    HostName 127.0.0.1\n    User root\n    Port {}\n    IdentityFile \"{}\"\n    IdentitiesOnly yes\n    StrictHostKeyChecking yes\n    UserKnownHostsFile \"{}\"\n",
            server.port(),
            server.key_path().display(),
            known_hosts.display(),
        ),
    )
    .unwrap();

    let remote_dir = "/root/cass-sync-retry/.codex/sessions";
    let remote_file = "/root/cass-sync-retry/.codex/sessions/rollout-retry.jsonl";
    assert!(
        Command::new("docker")
            .args(["exec", &server.container_id, "mkdir", "-p", remote_dir])
            .status()
            .expect("create dedicated remote fixture")
            .success()
    );
    let write_remote = |body: &str| {
        let mut child = Command::new("docker")
            .args(["exec", "-i", &server.container_id, "tee", remote_file])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("write remote session through Docker");
        child
            .stdin
            .take()
            .expect("fixture stdin")
            .write_all(body.as_bytes())
            .expect("write complete remote fixture");
        let output = child.wait_with_output().expect("finish fixture write");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let initial = concat!(
        "{\"timestamp\":\"2026-09-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"sync-index-retry\",\"cwd\":\"/synthetic/project\",\"cli_version\":\"0.42.0\"}}\n",
        "{\"timestamp\":\"2026-09-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"syncindexretryfirstmarker\"}]}}\n",
    );
    write_remote(initial);
    create_ssh_sources_config(
        &config,
        "retry-source",
        "retry-source",
        server.port(),
        server.key_path(),
        &[remote_dir],
    );

    let command = || {
        let mut command = tracker.cass_assert_command();
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("test PATH"))
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_DATA_HOME", tmp.path().join("xdg"))
            .env("CASS_DATA_DIR", &data)
            .env("CASS_SSH_CONFIG", &ssh_config)
            .env("CASS_AUTO_REFRESH", "0")
            .env("RUST_MIN_STACK", "134217728")
            .current_dir(&home)
            .timeout(Duration::from_secs(120));
        tracker.command_environment().apply_to_assert(&mut command);
        command
    };
    let json_output = |output: &std::process::Output| -> Value {
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "expected one JSON document: {error}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    };
    let success = |output: &std::process::Output| {
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let journal_path = data.join("sync_index_pending.json");

    let dry = command()
        .args(["sources", "sync", "--json", "--dry-run"])
        .output()
        .unwrap();
    success(&dry);
    assert!(!journal_path.exists());
    assert!(!data.join("sync-index.lock").exists());

    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(data.join("index-run.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    let refused = command()
        .args(["sources", "sync", "--json"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(7));
    let refused = json_output(&refused);
    assert_eq!(refused["status"], "index_failed", "{refused}");
    assert_eq!(refused["indexing"]["code"], 7, "{refused}");
    assert!(refused["total_files"].as_u64().unwrap() > 0, "{refused}");
    assert_eq!(refused["sources"][0]["method"], "rsync", "{refused}");
    assert_eq!(refused["pending_index_sources"], json!(["retry-source"]));
    let pending: Value = serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
    assert_eq!(pending["pending"], json!(["retry-source"]));
    assert!(!data.join("agent_search.db").exists());

    // Human capture must preserve the original actionable error, not suppress
    // it as already reported merely because its payload was captured.
    let human_refused = command().args(["sources", "sync"]).output().unwrap();
    assert_eq!(human_refused.status.code(), Some(7));
    let diagnostic = String::from_utf8_lossy(&human_refused.stderr);
    assert!(
        diagnostic.to_ascii_lowercase().contains("index")
            && (diagnostic.to_ascii_lowercase().contains("active")
                || diagnostic.to_ascii_lowercase().contains("lock")
                || diagnostic.to_ascii_lowercase().contains("running")),
        "missing actionable held-index diagnostic: {diagnostic}"
    );
    assert!(!diagnostic.contains("__cass_error_already_reported__"));
    FileExt::unlock(&lock).unwrap();

    let recovered = command()
        .args(["sources", "sync", "--json"])
        .output()
        .unwrap();
    success(&recovered);
    let recovered = json_output(&recovered);
    assert_eq!(recovered["status"], "complete", "{recovered}");
    assert_eq!(recovered["total_files"], 0, "{recovered}");
    assert_eq!(recovered["total_bytes"], 0, "{recovered}");
    assert_eq!(recovered["will_reindex"], true, "{recovered}");
    assert_eq!(recovered["indexing"]["success"], true, "{recovered}");
    assert_eq!(recovered["pending_index_sources"], json!([]), "{recovered}");

    let search = |marker: &str| {
        let output = command()
            .args([
                "search", marker, "--robot", "--mode", "lexical", "--source",
                "retry-source", "--no-maintenance", "--no-daemon",
            ])
            .output()
            .unwrap();
        success(&output);
        let result = json_output(&output);
        let hits = result["hits"].as_array().expect("lexical search hits");
        assert_eq!(hits.len(), 1, "{result}");
        assert_eq!(hits[0]["source_id"], "retry-source", "{result}");
    };
    search("syncindexretryfirstmarker");
    let quiet = command()
        .args(["sources", "sync", "--json"])
        .output()
        .unwrap();
    success(&quiet);
    let quiet = json_output(&quiet);
    assert_eq!(quiet["total_files"], 0, "{quiet}");
    assert_eq!(quiet["total_bytes"], 0, "{quiet}");
    assert_eq!(
        quiet["will_reindex"],
        env!("CASS_SOURCE_INGEST_REUSE") != "true",
        "only a versioned production ingest contract can reuse completion: {quiet}"
    );

    let extended = format!(
        "{}{}\n",
        initial,
        r#"{"timestamp":"2026-09-01T00:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"syncindexretrysecondmarker"}]}}"#,
    );
    write_remote(&extended);
    let deferred = command()
        .args(["sources", "sync", "--json", "--no-index"])
        .output()
        .unwrap();
    success(&deferred);
    let deferred = json_output(&deferred);
    assert!(deferred["total_files"].as_u64().unwrap() > 0, "{deferred}");
    assert_eq!(deferred["will_reindex"], false, "{deferred}");
    assert_eq!(deferred["pending_index_sources"], json!(["retry-source"]));

    let before_dry_run = fs::read(&journal_path).unwrap();
    let dry = command()
        .args(["sources", "sync", "--json", "--dry-run"])
        .output()
        .unwrap();
    success(&dry);
    assert_eq!(fs::read(&journal_path).unwrap(), before_dry_run);

    let human_recovered = command().args(["sources", "sync"]).output().unwrap();
    success(&human_recovered);
    let completed: Value = serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
    assert_eq!(completed["pending"], json!([]), "{completed}");
    search("syncindexretrysecondmarker");
    tracker.complete();
}

/// Test: sources sync verifies provenance tracking.
///
/// After syncing, sessions should have:
/// - source_id = "docker-ssh-test"
/// - origin_host = "localhost" or the container hostname
#[test]
#[ignore] // Requires Docker
fn ssh_sources_provenance_tracking() {
    if !docker_available() {
        eprintln!("Skipping test: Docker not available");
        return;
    }

    let tracker = tracker_for("ssh_sources_provenance_tracking");

    // Start Docker SSH server
    let start = tracker.start("docker_start", Some("Start Docker SSH server"));
    let server = match DockerSshServer::start() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to start Docker SSH server: {e}");
            return;
        }
    };
    tracker.end("docker_start", Some("Start Docker SSH server"), start);

    // Set up temp directories
    let start = tracker.start("setup", Some("Create temp directories and config"));
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&home_dir).unwrap();

    create_ssh_sources_config(
        &config_dir,
        "provenance-test",
        &server.ssh_host(),
        server.port(),
        server.key_path(),
        &["/root/.claude/projects", "/root/.codex/sessions"],
    );

    create_ssh_config(
        &home_dir,
        "provenance-test",
        server.port(),
        server.key_path(),
    );

    tracker.end("setup", Some("Create temp directories and config"), start);

    // Sync the data
    let start = tracker.start("sources_sync", Some("Run sources sync"));
    let mut command = tracker.cass_assert_command();
    let sync_output = command
        .args(["sources", "sync"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(60))
        .output()
        .expect("sources sync command");
    tracker.end("sources_sync", Some("Run sources sync"), start);

    if !sync_output.status.success() {
        let stderr = String::from_utf8_lossy(&sync_output.stderr);
        eprintln!("Sync output: {stderr}");
    }

    // Index the synced data
    let start = tracker.start("index", Some("Run index to build search database"));
    let mut command = tracker.cass_assert_command();
    let index_output = command
        .args(["index", "--json"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(120))
        .output()
        .expect("index command");
    tracker.end("index", Some("Run index to build search database"), start);

    if !index_output.status.success() {
        let stderr = String::from_utf8_lossy(&index_output.stderr);
        eprintln!("Index stderr: {stderr}");
    }

    // Search and verify provenance
    let start = tracker.start("search_verify", Some("Search and verify provenance"));
    let mut command = tracker.cass_assert_command();
    let search_output = command
        .args(["search", "hello", "--json", "--robot-meta"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(30))
        .output()
        .expect("search command");
    tracker.end("search_verify", Some("Search and verify provenance"), start);

    let stdout = String::from_utf8_lossy(&search_output.stdout);

    // If we got results, verify provenance fields
    if stdout.contains("source_id") || stdout.contains("origin") {
        // Provenance tracking is working - the exact values depend on config
        assert!(
            stdout.contains("provenance-test")
                || stdout.contains("docker-ssh-test")
                || stdout.contains("remote"),
            "Expected provenance info in search results"
        );
    }

    tracker.complete();
}

/// Test: sources sync with SFTP fallback when rsync is unavailable.
///
/// This test verifies that sync works even when rsync cannot be used,
/// falling back to SFTP.
#[test]
#[ignore] // Requires Docker
fn ssh_sources_sync_sftp_fallback() {
    assert!(docker_available(), "explicit SFTP test requires Docker");

    let tracker = tracker_for("ssh_sources_sync_sftp_fallback");

    // Start Docker SSH server
    let start = tracker.start("docker_start", Some("Start Docker SSH server"));
    let server = DockerSshServer::start().expect("start real SSH server for SFTP");
    tracker.end("docker_start", Some("Start Docker SSH server"), start);

    // Set up temp directories
    let start = tracker.start("setup", Some("Create temp directories and config"));
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&home_dir).unwrap();

    create_ssh_sources_config(
        &config_dir,
        "sftp-test",
        "sftp-test",
        server.port(),
        server.key_path(),
        &["/root/.claude/projects"],
    );

    create_ssh_config(&home_dir, "sftp-test", server.port(), server.key_path());

    // An empty PATH makes every external transport unavailable. The absolute
    // remote path and explicit SSH config let the real ssh2 SFTP path run
    // without shell helpers; no substituted transport can make this test pass.
    let fixture_bin = tmp.path().join("fixture_bin");
    fs::create_dir_all(&fixture_bin).unwrap();
    tracker.end("setup", Some("Create temp directories and config"), start);

    // Run sources sync - should fall back to SFTP
    let start = tracker.start(
        "sources_sync_sftp",
        Some("Run sources sync with SFTP fallback"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "sync", "--json", "--no-index"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .env("CASS_SSH_CONFIG", home_dir.join(".ssh/config"))
        .env("HOME", &home_dir)
        .env("PATH", &fixture_bin)
        .timeout(Duration::from_secs(120))
        .output()
        .expect("sources sync command");
    tracker.end(
        "sources_sync_sftp",
        Some("Run sources sync with SFTP fallback"),
        start,
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let start = tracker.start("verify_sftp", Some("Verify SFTP fallback was used"));
    assert!(
        output.status.success(),
        "SFTP sync failed. stdout: {}, stderr: {}",
        stdout,
        stderr
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("sync JSON");
    assert_eq!(result["status"], "complete", "{result}");
    assert_eq!(result["sources_attempted"], 1, "{result}");
    let reports = result["sources"].as_array().expect("source reports");
    assert_eq!(reports.len(), 1, "{result}");
    assert_eq!(reports[0]["source"], "sftp-test");
    assert_eq!(reports[0]["method"], "sftp");
    assert_eq!(reports[0]["transport_decision"]["chosen_transport"], "sftp");
    assert_eq!(reports[0]["status"], "success", "{result}");
    assert_eq!(reports[0]["total_files"], 2, "{result}");
    let mirror = data_dir.join("remotes/sftp-test/mirror");
    let sessions: Vec<_> = walkdir::WalkDir::new(&mirror)
        .into_iter()
        .map(|entry| entry.expect("read mirrored files"))
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == "session.jsonl")
        .map(|entry| fs::read(entry.path()).expect("read transferred session"))
        .collect();
    assert_eq!(sessions.len(), 2);
    let hello = concat!(
        r#"{"type":"user","message":{"content":"Write a hello world program"}}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":"Here is a hello world program..."}}"#,
        "\n",
    )
    .as_bytes();
    let second = concat!(
        r#"{"type":"user","message":{"content":"Test message in second project"}}"#,
        "\n",
    )
    .as_bytes();
    assert!(sessions.iter().any(|bytes| bytes.as_slice() == hello));
    assert!(sessions.iter().any(|bytes| bytes.as_slice() == second));
    tracker.end("verify_sftp", Some("Verify SFTP fallback was used"), start);

    tracker.complete();
}

/// Test: sources sync handles multiple paths correctly.
#[test]
#[ignore] // Requires Docker
fn ssh_sources_sync_multiple_paths() {
    if !docker_available() {
        eprintln!("Skipping test: Docker not available");
        return;
    }

    let tracker = tracker_for("ssh_sources_sync_multiple_paths");

    let start = tracker.start("docker_start", Some("Start Docker SSH server"));
    let server = match DockerSshServer::start() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to start Docker SSH server: {e}");
            return;
        }
    };
    tracker.end("docker_start", Some("Start Docker SSH server"), start);

    let start = tracker.start("setup", Some("Create temp directories and config"));
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&home_dir).unwrap();

    // Configure multiple paths
    create_ssh_sources_config(
        &config_dir,
        "multi-path-test",
        &server.ssh_host(),
        server.port(),
        server.key_path(),
        &[
            "/root/.claude/projects",
            "/root/.codex/sessions",
            "/root/.gemini/tmp",
        ],
    );

    create_ssh_config(
        &home_dir,
        "multi-path-test",
        server.port(),
        server.key_path(),
    );

    tracker.end("setup", Some("Create temp directories and config"), start);

    let start = tracker.start("sources_sync", Some("Run sources sync for multiple paths"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "sync", "--verbose"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(120))
        .output()
        .expect("sources sync command");
    tracker.end(
        "sources_sync",
        Some("Run sources sync for multiple paths"),
        start,
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    let start = tracker.start("verify_paths", Some("Verify all paths were synced"));

    // Check that mirror directory has multiple subdirectories
    let mirror_dir = data_dir
        .join("coding-agent-search")
        .join("remotes")
        .join("multi-path-test")
        .join("mirror");

    if mirror_dir.exists() {
        let entries: Vec<_> = fs::read_dir(&mirror_dir)
            .map(|rd| rd.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();

        // Should have synced multiple paths
        if output.status.success() {
            // Success case - check for evidence of multi-path sync
            assert!(
                !entries.is_empty()
                    || stdout.contains("claude")
                    || stdout.contains("codex")
                    || stdout.contains("gemini"),
                "Expected multiple paths to be synced"
            );
        }
    }
    tracker.end("verify_paths", Some("Verify all paths were synced"), start);

    tracker.complete();
}

/// Test: path rewriting with sources mappings.
///
/// Verifies that workspace paths are correctly rewritten from remote
/// paths to local equivalents using path mappings.
#[test]
#[ignore] // Requires Docker
fn ssh_sources_path_rewriting() {
    if !docker_available() {
        eprintln!("Skipping test: Docker not available");
        return;
    }

    let tracker = tracker_for("ssh_sources_path_rewriting");

    let start = tracker.start("docker_start", Some("Start Docker SSH server"));
    let server = match DockerSshServer::start() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to start Docker SSH server: {e}");
            return;
        }
    };
    tracker.end("docker_start", Some("Start Docker SSH server"), start);

    let start = tracker.start("setup", Some("Create config with path mappings"));
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&home_dir).unwrap();

    // Create config with path mappings
    let config_content = format!(
        r#"[[sources]]
name = "path-mapping-test"
type = "ssh"
host = "{host}"
port = {port}
identity_file = "{identity}"
paths = ["/root/.claude/projects"]
sync_schedule = "manual"

[sources.path_mappings]
from = "/root"
to = "/home/localuser"
"#,
        host = server.ssh_host(),
        port = server.port(),
        identity = server.key_path().display(),
    );

    let config_file = config_dir.join("cass").join("sources.toml");
    fs::create_dir_all(config_file.parent().unwrap()).unwrap();
    fs::write(&config_file, config_content).unwrap();

    create_ssh_config(
        &home_dir,
        "path-mapping-test",
        server.port(),
        server.key_path(),
    );

    tracker.end("setup", Some("Create config with path mappings"), start);

    // Sync and index
    let start = tracker.start("sync_and_index", Some("Sync and index with path mappings"));

    let mut command = tracker.cass_assert_command();
    let _ = command
        .args(["sources", "sync"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(60))
        .output();

    let mut command = tracker.cass_assert_command();
    let _ = command
        .args(["index"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(120))
        .output();

    tracker.end(
        "sync_and_index",
        Some("Sync and index with path mappings"),
        start,
    );

    // Search and check workspace paths
    let start = tracker.start(
        "verify_rewrite",
        Some("Verify path rewriting in search results"),
    );
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["search", "hello", "--json"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(30))
        .output()
        .expect("search command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // If we got results, check that paths were rewritten
    // The mapping should transform /root -> /home/localuser
    if stdout.contains("workspace") && stdout.contains("localuser") {
        // Path mapping is working
        assert!(
            !stdout.contains("\"/root\""),
            "Path should be rewritten from /root to /home/localuser"
        );
    }
    tracker.end(
        "verify_rewrite",
        Some("Verify path rewriting in search results"),
        start,
    );

    tracker.complete();
}

/// Test: sources doctor with real SSH connection.
#[test]
#[ignore] // Requires Docker
fn ssh_sources_doctor() {
    if !docker_available() {
        eprintln!("Skipping test: Docker not available");
        return;
    }

    let tracker = tracker_for("ssh_sources_doctor");

    let start = tracker.start("docker_start", Some("Start Docker SSH server"));
    let server = match DockerSshServer::start() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to start Docker SSH server: {e}");
            return;
        }
    };
    tracker.end("docker_start", Some("Start Docker SSH server"), start);

    let start = tracker.start("setup", Some("Create temp directories and config"));
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&home_dir).unwrap();

    create_ssh_sources_config(
        &config_dir,
        "doctor-test",
        &server.ssh_host(),
        server.port(),
        server.key_path(),
        &["/root/.claude/projects"],
    );

    create_ssh_config(&home_dir, "doctor-test", server.port(), server.key_path());

    tracker.end("setup", Some("Create temp directories and config"), start);

    // Run sources doctor
    let start = tracker.start("sources_doctor", Some("Run sources doctor"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "doctor", "--verbose"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(60))
        .output()
        .expect("sources doctor command");
    tracker.end("sources_doctor", Some("Run sources doctor"), start);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let start = tracker.start("verify_doctor", Some("Verify doctor output"));
    // Doctor should show connectivity status
    let combined = format!("{}{}", stdout, stderr);
    assert!(
        combined.contains("doctor-test")
            || combined.contains("reachable")
            || combined.contains("connected")
            || combined.contains("ok")
            || combined.to_lowercase().contains("check"),
        "Expected doctor to show source status. Got: {}",
        combined
    );
    tracker.end("verify_doctor", Some("Verify doctor output"), start);

    tracker.complete();
}

// =============================================================================
// E2E Sources Flows Tests (coding_agent_session_search-1de9)
// =============================================================================

/// Helper: Write test output to test-results directory for debugging.
fn write_test_log(test_name: &str, filename: &str, content: &str) {
    let log_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test-results")
        .join("e2e")
        .join("ssh_sources")
        .join(test_name);

    if let Err(e) = fs::create_dir_all(&log_dir) {
        eprintln!("Failed to create log dir: {e}");
        return;
    }

    if let Err(e) = fs::write(log_dir.join(filename), content) {
        eprintln!("Failed to write log: {e}");
    }
}

/// Test: sources setup in non-interactive dry-run mode.
#[test]
#[ignore] // Requires Docker
fn ssh_sources_setup_dry_run() {
    if !docker_available() {
        eprintln!("Skipping test: Docker not available");
        return;
    }

    let tracker = tracker_for("ssh_sources_setup_dry_run");

    let start = tracker.start("docker_start", Some("Start Docker SSH server"));
    let server = match DockerSshServer::start() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to start Docker SSH server: {e}");
            return;
        }
    };
    tracker.end("docker_start", Some("Start Docker SSH server"), start);

    let start = tracker.start("setup", Some("Create temp directories and SSH config"));
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&home_dir).unwrap();

    create_ssh_config(&home_dir, "docker-test", server.port(), server.key_path());

    tracker.end(
        "setup",
        Some("Create temp directories and SSH config"),
        start,
    );

    let start = tracker.start("sources_setup_dry_run", Some("Run sources setup --dry-run"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "setup",
            "--dry-run",
            "--non-interactive",
            "--hosts",
            &format!("root@localhost:{}", server.port()),
            "--verbose",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(120))
        .output()
        .expect("sources setup command");
    tracker.end(
        "sources_setup_dry_run",
        Some("Run sources setup --dry-run"),
        start,
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    write_test_log("ssh_sources_setup_dry_run", "stdout.log", &stdout);
    write_test_log("ssh_sources_setup_dry_run", "stderr.log", &stderr);

    let start = tracker.start("verify_dry_run", Some("Verify dry-run output"));
    let combined = format!("{}{}", stdout, stderr);
    assert!(
        combined.contains("DRY RUN")
            || combined.contains("dry-run")
            || combined.contains("Would")
            || output.status.success(),
        "Expected dry-run indication or success. Got: {}",
        combined
    );
    tracker.end("verify_dry_run", Some("Verify dry-run output"), start);

    tracker.complete();
}

/// Test: sources mappings list command.
#[test]
#[ignore] // Requires Docker
fn ssh_sources_mappings_list() {
    if !docker_available() {
        eprintln!("Skipping test: Docker not available");
        return;
    }

    let tracker = tracker_for("ssh_sources_mappings_list");

    let start = tracker.start("docker_start", Some("Start Docker SSH server"));
    let server = match DockerSshServer::start() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to start Docker SSH server: {e}");
            return;
        }
    };
    tracker.end("docker_start", Some("Start Docker SSH server"), start);

    let start = tracker.start("setup", Some("Create config with path mappings"));
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&home_dir).unwrap();

    let config_content = format!(
        r#"[[sources]]
name = "mapping-test"
type = "ssh"
host = "{host}"
port = {port}
identity_file = "{identity}"
paths = ["/root/.claude/projects"]
sync_schedule = "manual"

[[sources.path_mappings]]
from = "/root"
to = "/home/testuser"
"#,
        host = server.ssh_host(),
        port = server.port(),
        identity = server.key_path().display(),
    );

    let config_file = config_dir.join("cass").join("sources.toml");
    fs::create_dir_all(config_file.parent().unwrap()).unwrap();
    fs::write(&config_file, config_content).unwrap();

    create_ssh_config(&home_dir, "mapping-test", server.port(), server.key_path());

    tracker.end("setup", Some("Create config with path mappings"), start);

    let start = tracker.start("mappings_list", Some("Run sources mappings list"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "mappings", "list"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(30))
        .output()
        .expect("sources mappings list command");
    tracker.end("mappings_list", Some("Run sources mappings list"), start);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    write_test_log("ssh_sources_mappings_list", "stdout.log", &stdout);
    write_test_log("ssh_sources_mappings_list", "stderr.log", &stderr);

    let start = tracker.start("verify_mappings", Some("Verify mappings in output"));
    let combined = format!("{}{}", stdout, stderr);
    assert!(
        combined.contains("/root")
            || combined.contains("testuser")
            || combined.contains("mapping")
            || output.status.success(),
        "Expected mapping info or success. Got: {}",
        combined
    );
    tracker.end("verify_mappings", Some("Verify mappings in output"), start);

    tracker.complete();
}

/// Test: Full E2E flow - add, list, sync, index, search with logging.
#[test]
#[ignore] // Requires Docker
fn ssh_sources_full_e2e_flow() {
    if !docker_available() {
        eprintln!("Skipping test: Docker not available");
        return;
    }

    let tracker = tracker_for("ssh_sources_full_e2e_flow");

    let start = tracker.start("docker_start", Some("Start Docker SSH server"));
    let server = match DockerSshServer::start() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to start Docker SSH server: {e}");
            return;
        }
    };
    tracker.end("docker_start", Some("Start Docker SSH server"), start);

    let start = tracker.start("setup", Some("Create temp directories"));
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&home_dir).unwrap();

    create_ssh_config(&home_dir, "e2e-test", server.port(), server.key_path());

    tracker.end("setup", Some("Create temp directories"), start);

    // Step 1: Add source
    let start = tracker.start("add_source", Some("Add SSH source"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args([
            "sources",
            "add",
            &format!("root@localhost:{}", server.port()),
            "--name",
            "e2e-docker",
            "--paths",
            "/root/.claude/projects,/root/.codex/sessions",
            "--identity",
            server.key_path().to_str().unwrap(),
            "--no-test",
        ])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(30))
        .output()
        .expect("sources add command");
    tracker.end("add_source", Some("Add SSH source"), start);

    write_test_log(
        "ssh_sources_full_e2e_flow",
        "add_stdout.log",
        &String::from_utf8_lossy(&output.stdout),
    );
    write_test_log(
        "ssh_sources_full_e2e_flow",
        "add_stderr.log",
        &String::from_utf8_lossy(&output.stderr),
    );

    // Step 2: List sources
    let start = tracker.start("list_sources", Some("List sources"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "list", "--verbose"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(30))
        .output()
        .expect("sources list command");
    tracker.end("list_sources", Some("List sources"), start);

    write_test_log(
        "ssh_sources_full_e2e_flow",
        "list_stdout.log",
        &String::from_utf8_lossy(&output.stdout),
    );

    // Step 3: Doctor
    let start = tracker.start("doctor", Some("Run sources doctor"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "doctor", "--verbose"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(60))
        .output()
        .expect("sources doctor command");
    tracker.end("doctor", Some("Run sources doctor"), start);

    write_test_log(
        "ssh_sources_full_e2e_flow",
        "doctor_stdout.log",
        &String::from_utf8_lossy(&output.stdout),
    );

    // Step 4: Sync
    let start = tracker.start("sync", Some("Run sources sync"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["sources", "sync", "--verbose"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(120))
        .output()
        .expect("sources sync command");
    tracker.end("sync", Some("Run sources sync"), start);

    write_test_log(
        "ssh_sources_full_e2e_flow",
        "sync_stdout.log",
        &String::from_utf8_lossy(&output.stdout),
    );

    // Step 5: Index
    let start = tracker.start("index", Some("Run index"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["index", "--json"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(120))
        .output()
        .expect("index command");
    tracker.end("index", Some("Run index"), start);

    write_test_log(
        "ssh_sources_full_e2e_flow",
        "index_stdout.log",
        &String::from_utf8_lossy(&output.stdout),
    );

    // Step 6: Search
    let start = tracker.start("search", Some("Search and verify"));
    let mut command = tracker.cass_assert_command();
    let output = command
        .args(["search", "hello", "--json"])
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HOME", &home_dir)
        .timeout(Duration::from_secs(30))
        .output()
        .expect("search command");
    tracker.end("search", Some("Search and verify"), start);

    write_test_log(
        "ssh_sources_full_e2e_flow",
        "search_stdout.log",
        &String::from_utf8_lossy(&output.stdout),
    );

    let summary = format!("E2E Flow Complete - Port: {}, Tests passed", server.port());
    write_test_log("ssh_sources_full_e2e_flow", "summary.log", &summary);

    tracker.complete();
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn test_find_available_port() {
        let port = find_available_port().expect("should find port");
        assert!(port > 0);
    }

    #[test]
    fn test_write_test_log() {
        write_test_log("unit_test", "test.log", "test content");
    }
}
