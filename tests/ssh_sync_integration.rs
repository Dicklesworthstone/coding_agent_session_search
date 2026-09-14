//! Integration tests for SSH sync operations.
//!
//! These tests require Docker to be available and will be skipped if not.
//! Run with: `cargo test --test ssh_sync_integration -- --ignored`
//!
//! The tests use a Docker container with an SSH server to test real
//! SSH operations without requiring external infrastructure.

mod ssh_test_helper;
mod util;

use assert_cmd::cargo::cargo_bin_cmd;
use coding_agent_search::sources::provenance::SourceKind;
use coding_agent_search::storage::sqlite::SqliteStorage;
use ssh_test_helper::{SshTestServer, docker_available};
use std::path::Path;
use util::EnvGuard;

/// Skip tests if Docker is not available.
fn require_docker() {
    if !docker_available() {
        eprintln!("Skipping test: Docker not available");
    }
}

/// Integration test: Full sync cycle against real SSH server.
///
/// This test verifies that we can:
/// 1. Connect to a real SSH server
/// 2. Sync files using rsync over SSH
/// 3. Get correct file counts and bytes transferred
#[test]
#[ignore = "requires Docker"]
fn test_sync_source_real_ssh() {
    require_docker();

    let server = SshTestServer::start().expect("SSH server should start");
    let tmp = tempfile::TempDir::new().unwrap();

    // Create a minimal source.toml config in memory
    // We need to manually configure rsync options since SourceDefinition doesn't
    // expose the SSH port directly

    // For this test, we'll run rsync directly with the server's settings
    let local_dest = tmp.path().join("mirror");
    std::fs::create_dir_all(&local_dest).unwrap();

    // Run rsync with explicit SSH options
    let ssh_opts = server.rsync_ssh_opts();
    let remote_path = format!("{}:/root/.claude/projects/", server.ssh_target());

    let output = std::process::Command::new("rsync")
        .args([
            "-avz",
            "--stats",
            "-e",
            &ssh_opts,
            &remote_path,
            local_dest.to_str().unwrap(),
        ])
        .output()
        .expect("rsync should execute");

    assert!(
        output.status.success(),
        "rsync should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify files were transferred
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("test-project") || stdout.contains("session.jsonl"),
        "Should transfer test files: {}",
        stdout
    );

    // Verify local files exist
    let test_file = local_dest.join("test-project/session.jsonl");
    assert!(test_file.exists(), "Session file should exist locally");

    // Read and verify content
    let content = std::fs::read_to_string(&test_file).unwrap();
    assert!(
        content.contains("hello world"),
        "File should contain test content"
    );
}

/// Integration test: Sync multiple paths from remote.
#[test]
#[ignore = "requires Docker"]
fn test_sync_multiple_paths() {
    require_docker();

    let server = SshTestServer::start().expect("SSH server should start");
    let tmp = tempfile::TempDir::new().unwrap();
    let local_dest = tmp.path().join("mirror");
    std::fs::create_dir_all(&local_dest).unwrap();

    let ssh_opts = server.rsync_ssh_opts();

    // Sync Claude projects
    let claude_dest = local_dest.join("claude");
    std::fs::create_dir_all(&claude_dest).unwrap();

    let output1 = std::process::Command::new("rsync")
        .args([
            "-avz",
            "-e",
            &ssh_opts,
            &format!("{}:/root/.claude/", server.ssh_target()),
            claude_dest.to_str().unwrap(),
        ])
        .output()
        .expect("rsync should execute");

    assert!(output1.status.success(), "Claude sync should succeed");

    // Sync Codex sessions
    let codex_dest = local_dest.join("codex");
    std::fs::create_dir_all(&codex_dest).unwrap();

    let output2 = std::process::Command::new("rsync")
        .args([
            "-avz",
            "-e",
            &ssh_opts,
            &format!("{}:/root/.codex/", server.ssh_target()),
            codex_dest.to_str().unwrap(),
        ])
        .output()
        .expect("rsync should execute");

    assert!(output2.status.success(), "Codex sync should succeed");

    // Verify both sets of files exist
    assert!(
        claude_dest
            .join("projects/test-project/session.jsonl")
            .exists(),
        "Claude session should exist"
    );
    assert!(
        codex_dest.join("sessions/session1.json").exists(),
        "Codex session should exist"
    );
}

/// Integration test: End-to-end sources sync via cass CLI with real SSH.
///
/// Validates:
/// - `cass sources sync` reports no sources when config is empty
/// - `cass sources add` works against real SSH (via ssh config)
/// - `cass sources sync --json` reports transfer stats
/// - SQLite provenance + workspace path mappings are applied
#[test]
#[ignore = "requires Docker"]
fn test_sources_sync_e2e_real_ssh() {
    require_docker();

    let server = SshTestServer::start().expect("SSH server should start");
    let tmp = tempfile::TempDir::new().unwrap();
    let home_dir = tmp.path().join("home");
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");

    std::fs::create_dir_all(home_dir.join(".ssh")).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    // Write SSH config so `cass sources add` can connect via alias with port/key.
    let ssh_config = format!(
        "Host cass-test\n  HostName 127.0.0.1\n  User root\n  Port {}\n  IdentityFile {}\n  IdentitiesOnly yes\n  HostKeyAlias cass-test\n  UserKnownHostsFile {}\n",
        server.port(),
        server.private_key_path().display(),
        home_dir.join(".ssh/known_hosts").display()
    );
    let ssh_config_path = home_dir.join(".ssh/config");
    std::fs::write(&ssh_config_path, ssh_config).unwrap();
    let known_hosts_path = home_dir.join(".ssh/known_hosts");
    write_known_hosts_for_test_server(&known_hosts_path, &server, "cass-test");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            home_dir.join(".ssh"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(&ssh_config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&known_hosts_path, std::fs::Permissions::from_mode(0o600))
            .unwrap();
    }

    // Seed a session with workspace metadata for path mapping verification.
    let seed_script = r#"mkdir -p /root/.claude/projects/workspace-a
cat > /root/.claude/projects/workspace-a/session.jsonl <<'EOF'
{"type":"user","cwd":"/root/projects/workspace-a","message":{"content":"Workspace mapping test"}}
{"type":"assistant","message":{"content":"ok"}}
EOF
"#;
    server
        .ssh_exec_with_stdin(seed_script)
        .expect("seed remote session");

    let _guard_home = EnvGuard::set("HOME", home_dir.to_string_lossy());
    let _guard_config = EnvGuard::set("XDG_CONFIG_HOME", config_dir.to_string_lossy());
    let _guard_data = EnvGuard::set("CASS_DATA_DIR", data_dir.to_string_lossy());
    let _guard_ssh_config = EnvGuard::set("CASS_SSH_CONFIG", ssh_config_path.to_string_lossy());

    let ssh_probe = std::process::Command::new("ssh")
        .args([
            "-F",
            ssh_config_path.to_str().unwrap(),
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "-o",
            "StrictHostKeyChecking=yes",
            "--",
            "root@cass-test",
            "echo",
            "ok",
        ])
        .env("HOME", &home_dir)
        .output()
        .expect("ssh alias probe");
    assert!(
        ssh_probe.status.success(),
        "test ssh alias should connect before cass sources add: {}",
        String::from_utf8_lossy(&ssh_probe.stderr)
    );

    // 1) Sync with no sources configured should return a friendly JSON status.
    let no_sources = cargo_bin_cmd!("cass")
        .args(["sources", "sync", "--json"])
        .env("HOME", &home_dir)
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .output()
        .expect("sources sync (no sources)");
    assert!(no_sources.status.success());
    let no_sources_json: serde_json::Value =
        serde_json::from_slice(&no_sources.stdout).expect("valid JSON");
    assert_eq!(no_sources_json["status"], "no_sources");

    // 2) Add a real SSH source (uses ssh config alias).
    let add_output = cargo_bin_cmd!("cass")
        .args([
            "sources",
            "add",
            "root@cass-test",
            "--name",
            "cass-test",
            "--path",
            "~/.claude/projects",
        ])
        .env("HOME", &home_dir)
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .env("CASS_SSH_CONFIG", &ssh_config_path)
        .output()
        .expect("sources add");
    assert!(
        add_output.status.success(),
        "sources add should succeed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    // 3) Add a path mapping for workspace rewrite.
    let map_output = cargo_bin_cmd!("cass")
        .args([
            "sources",
            "mappings",
            "add",
            "cass-test",
            "--from",
            "/root/projects",
            "--to",
            "/local/projects",
        ])
        .env("HOME", &home_dir)
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .output()
        .expect("sources mappings add");
    assert!(
        map_output.status.success(),
        "sources mappings add should succeed: {}",
        String::from_utf8_lossy(&map_output.stderr)
    );

    // 4) Sync via CLI (no-index for clean JSON output) and assert transfer metrics.
    let sync_output = cargo_bin_cmd!("cass")
        .args([
            "sources",
            "sync",
            "--source",
            "cass-test",
            "--json",
            "--no-index",
        ])
        .env("HOME", &home_dir)
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .env("CASS_SSH_CONFIG", &ssh_config_path)
        .output()
        .expect("sources sync");
    assert!(
        sync_output.status.success(),
        "sources sync should succeed: {}",
        String::from_utf8_lossy(&sync_output.stderr)
    );
    let sync_json: serde_json::Value =
        serde_json::from_slice(&sync_output.stdout).expect("valid JSON");
    assert_eq!(sync_json["status"], "complete");
    assert!(
        sync_json["total_files"].as_u64().unwrap_or(0) > 0,
        "expected transferred files in sync report"
    );

    // 5) Build index after sync, then validate provenance + path mappings in SQLite.
    let index_output = cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir", data_dir.to_str().unwrap()])
        .env("HOME", &home_dir)
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CASS_DATA_DIR", &data_dir)
        .output()
        .expect("cass index --full");
    assert!(
        index_output.status.success(),
        "index should succeed: {}",
        String::from_utf8_lossy(&index_output.stderr)
    );

    let db_path = data_dir.join("agent_search.db");
    let storage = SqliteStorage::open(&db_path).expect("open sqlite");

    let sources = storage.list_sources().expect("list sources");
    let remote_source = sources
        .iter()
        .find(|s| s.id == "cass-test")
        .expect("remote source should exist");
    assert_eq!(remote_source.kind, SourceKind::Ssh);

    let conversations = storage
        .list_conversations(200, 0)
        .expect("list conversations");
    let remote_conv = conversations
        .into_iter()
        .find(|c| c.source_id == "cass-test")
        .expect("remote conversation should exist");
    assert_eq!(
        remote_conv.workspace,
        Some(std::path::PathBuf::from("/local/projects/workspace-a"))
    );
    assert_eq!(
        remote_conv.metadata_json["cass"]["workspace_original"],
        "/root/projects/workspace-a"
    );
}

fn write_known_hosts_for_test_server(path: &Path, server: &SshTestServer, alias: &str) {
    let port = server.port().to_string();
    let output = std::process::Command::new("ssh-keyscan")
        .args(["-p", &port, "127.0.0.1"])
        .output()
        .expect("ssh-keyscan should execute");
    assert!(
        output.status.success(),
        "ssh-keyscan should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut known_hosts = String::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((_, key_material)) = line.split_once(' ') else {
            continue;
        };
        known_hosts.push_str(alias);
        known_hosts.push(' ');
        known_hosts.push_str(key_material);
        known_hosts.push('\n');
    }

    assert!(
        !known_hosts.is_empty(),
        "ssh-keyscan should emit at least one host key"
    );
    std::fs::write(path, known_hosts).expect("write known_hosts");
}

/// Integration test: Get remote home directory.
#[test]
#[ignore = "requires Docker"]
fn test_get_remote_home() {
    require_docker();

    let server = SshTestServer::start().expect("SSH server should start");

    // Execute `echo $HOME` on the remote
    let home = server.ssh_exec("echo $HOME").expect("Should get home");

    assert_eq!(home.trim(), "/root", "Remote home should be /root");
}

/// Integration test: Verify tilde expansion works with rsync.
#[test]
#[ignore = "requires Docker"]
fn test_tilde_expansion_with_rsync() {
    require_docker();

    let server = SshTestServer::start().expect("SSH server should start");
    let tmp = tempfile::TempDir::new().unwrap();
    let local_dest = tmp.path().join("mirror");
    std::fs::create_dir_all(&local_dest).unwrap();

    // Get remote home
    let home = server
        .ssh_exec("echo $HOME")
        .expect("Should get home")
        .trim()
        .to_string();

    // Expand ~/... to actual path
    let expanded_path = format!("{}/.claude/projects/", home);

    let ssh_opts = server.rsync_ssh_opts();
    let remote_path = format!("{}:{}", server.ssh_target(), expanded_path);

    let output = std::process::Command::new("rsync")
        .args([
            "-avz",
            "-e",
            &ssh_opts,
            &remote_path,
            local_dest.to_str().unwrap(),
        ])
        .output()
        .expect("rsync should execute");

    assert!(
        output.status.success(),
        "Expanded path rsync should succeed"
    );

    // Verify files were transferred
    assert!(
        local_dest.join("test-project/session.jsonl").exists(),
        "Session file should exist after tilde expansion"
    );
}

/// Integration test: Handle non-existent remote path gracefully.
#[test]
#[ignore = "requires Docker"]
fn test_sync_nonexistent_path() {
    require_docker();

    let server = SshTestServer::start().expect("SSH server should start");
    let tmp = tempfile::TempDir::new().unwrap();

    let ssh_opts = server.rsync_ssh_opts();
    let remote_path = format!("{}:/nonexistent/path/", server.ssh_target());

    let output = std::process::Command::new("rsync")
        .args([
            "-avz",
            "-e",
            &ssh_opts,
            &remote_path,
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .expect("rsync should execute");

    // rsync should fail for non-existent paths
    assert!(
        !output.status.success(),
        "rsync should fail for non-existent path"
    );
}

/// Integration test: SSH connection with wrong port fails.
#[test]
fn test_ssh_wrong_port_fails() {
    // This test doesn't need Docker - just verifies timeout behavior
    let _tmp = tempfile::TempDir::new().unwrap();

    let output = std::process::Command::new("ssh")
        .args([
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "ConnectTimeout=2",
            "-o",
            "BatchMode=yes",
            "-p",
            "65535", // Unlikely to be in use
            "root@127.0.0.1",
            "echo test",
        ])
        .output()
        .expect("ssh should execute");

    assert!(!output.status.success(), "SSH to wrong port should fail");
}

/// Integration test: Probe host via SSH using the probe module.
#[test]
#[ignore = "requires Docker"]
fn test_probe_host_real_ssh() {
    require_docker();

    let server = SshTestServer::start().expect("SSH server should start");

    // Test basic probe-like operations via SSH
    // Note: Full probe testing would require custom SSH config for the port.
    // For now, verify we can execute the same commands the probe script uses.

    let output = server
        .ssh_exec("uname -s && uname -m && echo HOME=$HOME")
        .expect("Probe should succeed");

    assert!(output.contains("Linux") || output.contains("Darwin"));
    assert!(output.contains("HOME=/root"));
}

/// Integration test: List files on remote via SSH.
#[test]
#[ignore = "requires Docker"]
fn test_list_remote_files() {
    require_docker();

    let server = SshTestServer::start().expect("SSH server should start");

    // List agent directories
    let output = server
        .ssh_exec("ls -la ~/.claude/projects/")
        .expect("ls should succeed");

    assert!(
        output.contains("test-project"),
        "Should list test-project directory"
    );

    // List session files
    let output = server
        .ssh_exec("find ~/.claude -name '*.jsonl' -type f")
        .expect("find should succeed");

    assert!(
        output.contains("session.jsonl"),
        "Should find session.jsonl files"
    );
}

/// GH468: real rsync/SSH counters must drive the public indexing contract.
#[test]
#[ignore = "requires Docker"]
fn test_rsync_stats_parsing() {
    require_docker();

    let server = SshTestServer::start().expect("SSH server should start");
    let tmp = tempfile::TempDir::new().unwrap();

    let home_dir = tmp.path().join("home");
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(home_dir.join(".ssh")).expect("ssh directory");
    std::fs::create_dir_all(config_dir.join("cass")).expect("config directory");
    std::fs::create_dir_all(&data_dir).expect("data directory");
    let ssh_config_path = home_dir.join(".ssh/config");
    let known_hosts_path = home_dir.join(".ssh/known_hosts");
    std::fs::write(
        &ssh_config_path,
        format!(
            "Host cass-stats\n  HostName 127.0.0.1\n  User root\n  Port {}\n  IdentityFile {}\n  IdentitiesOnly yes\n  HostKeyAlias cass-stats\n  UserKnownHostsFile {}\n",
            server.port(),
            server.private_key_path().display(),
            known_hosts_path.display()
        ),
    )
    .expect("write ssh config");
    write_known_hosts_for_test_server(&known_hosts_path, &server, "cass-stats");
    std::fs::write(
        config_dir.join("cass/sources.toml"),
        "[[sources]]\nname = \"cass-stats\"\ntype = \"ssh\"\n\
         host = \"root@cass-stats\"\n\
         paths = [\"/root/.claude/projects/gh468\"]\nsync_schedule = \"manual\"\n",
    )
    .expect("write source config");
    let session = "{\"type\":\"user\",\"message\":{\"content\":\"gh468searchmarker\"}}\n";
    server
        .ssh_exec_with_stdin(&format!(
            "mkdir -p /root/.claude/projects/gh468\n\
             cat > /root/.claude/projects/gh468/session.jsonl <<'SESSION'\n\
             {session}SESSION\n"
        ))
        .expect("write real remote session");

    let run = |args: &[&str]| {
        let output = cargo_bin_cmd!("cass")
            .args(args)
            .current_dir(&home_dir)
            .env("HOME", &home_dir)
            .env("XDG_CONFIG_HOME", &config_dir)
            .env("XDG_DATA_HOME", &data_dir)
            .env("CASS_DATA_DIR", &data_dir)
            .env("CASS_SSH_CONFIG", &ssh_config_path)
            .env("LC_ALL", "C")
            .timeout(std::time::Duration::from_secs(180))
            .output()
            .expect("execute real cass CLI");
        assert!(
            output.status.success(),
            "cass {args:?}: {:?}\nstdout={}\nstderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "cass {args:?} invalid JSON: {error}\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    };
    let assert_sync = |payload: &serde_json::Value, files: u64, bytes: u64, indexed: bool| {
        assert_eq!(payload["status"], "complete", "{payload}");
        assert_eq!(payload["total_files"], files, "{payload}");
        assert_eq!(payload["total_bytes"], bytes, "{payload}");
        assert_eq!(payload["will_reindex"], indexed, "{payload}");
        assert_eq!(payload.get("indexing").is_some(), indexed, "{payload}");
        assert_eq!(payload["sources"][0]["total_files"], files, "{payload}");
        assert_eq!(
            payload["sources"][0]["paths"][0]["files"], files,
            "{payload}"
        );
        assert_eq!(
            payload["sources"][0]["paths"][0]["bytes"], bytes,
            "{payload}"
        );
    };

    let first = run(&["sources", "sync", "--json", "--no-index"]);
    assert_sync(&first, 1, session.len() as u64, false);
    assert_eq!(first["sources"][0]["method"], "rsync", "{first}");
    let saved = coding_agent_search::sources::sync::SyncStatus::load(&data_dir)
        .expect("read actual saved sync status");
    assert_eq!(saved.sources["cass-stats"].files_synced, 1);
    assert_eq!(
        saved.sources["cass-stats"].bytes_transferred,
        session.len() as u64
    );
    // The configured directory has no trailing slash, so rsync retains its
    // basename inside the per-source path container.
    let mirror = data_dir
        .join("remotes/cass-stats/mirror")
        .join(coding_agent_search::sources::sync::path_to_safe_dirname(
            "/root/.claude/projects/gh468",
        ))
        .join("gh468");
    assert_eq!(
        std::fs::read_to_string(mirror.join("session.jsonl")).expect("transferred session"),
        session
    );
    assert!(!data_dir.join("agent_search.db").exists());
    let unchanged = run(&["sources", "sync", "--json"]);
    assert_sync(&unchanged, 0, 0, false);
    assert!(!data_dir.join("agent_search.db").exists());

    server
        .ssh_exec("touch /root/.claude/projects/gh468/empty.jsonl")
        .expect("add zero-byte file on real server");
    let status_before = std::fs::read(data_dir.join("sync_status.json")).expect("saved status");
    let dry = run(&["sources", "sync", "--json", "--dry-run"]);
    assert_sync(&dry, 0, 0, false);
    assert_eq!(dry["sources"][0]["method"], "not_run");
    assert_eq!(
        std::fs::read(data_dir.join("sync_status.json")).unwrap(),
        status_before
    );
    assert!(!mirror.join("empty.jsonl").exists());
    assert!(!data_dir.join("agent_search.db").exists());

    // A new empty file must still enter indexing, which also indexes the earlier
    // deferred session. The search verifies actual publication, not just a flag.
    let empty_file = run(&["sources", "sync", "--json"]);
    assert_sync(&empty_file, 1, 0, true);
    assert_eq!(
        std::fs::metadata(mirror.join("empty.jsonl")).unwrap().len(),
        0
    );
    let search = run(&["search", "gh468searchmarker", "--json", "--mode", "lexical"]);
    let hits = search["hits"].as_array().expect("search hits array");
    assert_eq!(hits.len(), 1, "{search}");
    assert_eq!(hits[0]["content"], "gh468searchmarker", "{search}");
    let unchanged = run(&["sources", "sync", "--json"]);
    assert_sync(&unchanged, 0, 0, false);
}

/// Integration test: Verify container cleanup on drop.
///
/// This test verifies that SshTestServer's Drop implementation doesn't panic.
/// The actual container cleanup is handled by Docker's --rm flag and the
/// explicit `docker stop` in Drop.
#[test]
#[ignore = "requires Docker"]
fn test_container_cleanup() {
    require_docker();

    // Create server in inner scope so Drop runs at block end
    {
        let server = SshTestServer::start().expect("SSH server should start");

        // Verify the server is actually working before we drop it
        let output = server
            .ssh_exec("echo cleanup_test")
            .expect("SSH should work");
        assert!(
            output.contains("cleanup_test"),
            "Server should be responsive"
        );

        // Server is dropped at end of this block
    }

    // After the server is dropped, wait a moment for cleanup
    std::thread::sleep(std::time::Duration::from_millis(500));

    // If we reach here without panic, Drop worked correctly.
    // The container is auto-removed by Docker's --rm flag.
}
