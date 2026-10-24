//! Tests for the setup workflow module.
//!
//! These tests cover:
//! - SetupOptions default values and construction
//! - SetupState serialization/deserialization
//! - SetupState::has_progress() logic
//! - SetupResult structure
//! - SetupError display implementations
//!
//! Saved probe fixtures exercise resume without contacting remote machines.
//! Linux retry tests use real OpenSSH with a locally refusing ProxyCommand.

// Allow field reassignment in tests - we deliberately test has_progress() by setting individual fields
#![allow(clippy::field_reassign_with_default)]

mod util;

use coding_agent_search::sources::probe::CassStatus;
use coding_agent_search::sources::setup::{SetupError, SetupOptions, SetupResult, SetupState};
use util::probe_fixtures;

// =============================================================================
// SetupOptions Tests
// =============================================================================

/// Test that SetupOptions::default() produces expected values.
#[test]
fn setup_options_default_values() {
    let opts = SetupOptions::default();

    assert!(!opts.dry_run, "dry_run should default to false");
    assert!(
        !opts.non_interactive,
        "non_interactive should default to false"
    );
    assert!(opts.hosts.is_none(), "hosts should default to None");
    assert!(!opts.skip_install, "skip_install should default to false");
    assert!(!opts.skip_index, "skip_index should default to false");
    assert!(!opts.skip_sync, "skip_sync should default to false");
    assert_eq!(opts.timeout, 10, "timeout should default to 10 seconds");
    assert!(!opts.resume, "resume should default to false");
    assert!(!opts.verbose, "verbose should default to false");
    assert!(!opts.json, "json should default to false");
}

/// Test SetupOptions with various configurations.
#[test]
fn setup_options_custom_values() {
    let opts = SetupOptions {
        dry_run: true,
        non_interactive: true,
        hosts: Some(vec!["host1".to_string(), "host2".to_string()]),
        tailscale: true,
        skip_install: true,
        skip_index: true,
        skip_sync: true,
        timeout: 30,
        resume: true,
        verbose: true,
        json: true,
    };

    assert!(opts.dry_run);
    assert!(opts.non_interactive);
    assert_eq!(
        opts.hosts,
        Some(vec!["host1".to_string(), "host2".to_string()])
    );
    assert!(opts.skip_install);
    assert!(opts.skip_index);
    assert!(opts.skip_sync);
    assert_eq!(opts.timeout, 30);
    assert!(opts.resume);
    assert!(opts.verbose);
    assert!(opts.json);
}

// =============================================================================
// SetupState Tests
// =============================================================================

/// Test that SetupState::default() produces empty state.
#[test]
fn setup_state_default_is_empty() {
    let state = SetupState::default();

    assert!(!state.discovery_complete);
    assert_eq!(state.discovered_hosts, 0);
    assert!(state.discovered_host_names.is_empty());
    assert!(!state.probing_complete);
    assert!(state.probed_hosts.is_empty());
    assert!(!state.selection_complete);
    assert!(state.selected_host_names.is_empty());
    assert!(!state.installation_complete);
    assert!(state.completed_installs.is_empty());
    assert!(!state.indexing_complete);
    assert!(state.completed_indexes.is_empty());
    assert!(!state.configuration_complete);
    assert!(!state.sync_complete);
    assert!(state.current_operation.is_none());
    assert!(state.started_at.is_none());
}

/// Test SetupState::has_progress() returns false for empty state.
#[test]
fn setup_state_has_progress_empty() {
    let state = SetupState::default();
    assert!(!state.has_progress(), "Empty state should have no progress");
}

/// Test SetupState::has_progress() returns true when discovery is complete.
#[test]
fn setup_state_has_progress_discovery() {
    let state = SetupState {
        discovery_complete: true,
        ..Default::default()
    };
    assert!(
        state.has_progress(),
        "State with discovery_complete should have progress"
    );
}

/// Test SetupState::has_progress() returns true when probing is complete.
#[test]
fn setup_state_has_progress_probing() {
    let state = SetupState {
        probing_complete: true,
        ..Default::default()
    };
    assert!(
        state.has_progress(),
        "State with probing_complete should have progress"
    );
}

/// Test SetupState::has_progress() returns true when selection is complete.
#[test]
fn setup_state_has_progress_selection() {
    let state = SetupState {
        selection_complete: true,
        ..Default::default()
    };
    assert!(
        state.has_progress(),
        "State with selection_complete should have progress"
    );
}

/// Test SetupState::has_progress() returns true when installation is complete.
#[test]
fn setup_state_has_progress_installation() {
    let state = SetupState {
        installation_complete: true,
        ..Default::default()
    };
    assert!(
        state.has_progress(),
        "State with installation_complete should have progress"
    );
}

/// Test SetupState::has_progress() returns true when indexing is complete.
#[test]
fn setup_state_has_progress_indexing() {
    let state = SetupState {
        indexing_complete: true,
        ..Default::default()
    };
    assert!(
        state.has_progress(),
        "State with indexing_complete should have progress"
    );
}

/// Test SetupState::has_progress() returns true when configuration is complete.
#[test]
fn setup_state_has_progress_configuration() {
    let state = SetupState {
        configuration_complete: true,
        ..Default::default()
    };
    assert!(
        state.has_progress(),
        "State with configuration_complete should have progress"
    );
}

/// Test SetupState serialization and deserialization roundtrip.
#[test]
fn setup_state_serde_roundtrip() {
    let state = SetupState {
        discovery_complete: true,
        discovered_hosts: 5,
        discovered_host_names: vec!["host1".to_string(), "host2".to_string()],
        probing_complete: true,
        selection_complete: true,
        selected_host_names: vec!["host1".to_string()],
        installation_complete: true,
        completed_installs: vec!["host1".to_string()],
        started_at: Some("2025-01-01T00:00:00Z".to_string()),
        current_operation: Some("Testing".to_string()),
        ..Default::default()
    };

    // Serialize to JSON
    let json = serde_json::to_string(&state).expect("Failed to serialize SetupState");

    // Deserialize back
    let deserialized: SetupState =
        serde_json::from_str(&json).expect("Failed to deserialize SetupState");

    assert_eq!(deserialized.discovery_complete, state.discovery_complete);
    assert_eq!(deserialized.discovered_hosts, state.discovered_hosts);
    assert_eq!(
        deserialized.discovered_host_names,
        state.discovered_host_names
    );
    assert_eq!(deserialized.probing_complete, state.probing_complete);
    assert_eq!(deserialized.selection_complete, state.selection_complete);
    assert_eq!(deserialized.selected_host_names, state.selected_host_names);
    assert_eq!(
        deserialized.installation_complete,
        state.installation_complete
    );
    assert_eq!(deserialized.completed_installs, state.completed_installs);
    assert_eq!(deserialized.started_at, state.started_at);
    assert_eq!(deserialized.current_operation, state.current_operation);
}

/// Test SetupState serialization produces valid JSON.
#[test]
fn setup_state_json_format() {
    let state = SetupState {
        discovery_complete: true,
        discovered_hosts: 3,
        ..Default::default()
    };

    let json = serde_json::to_string_pretty(&state).expect("Failed to serialize SetupState");

    // Verify it's valid JSON by parsing it back
    let value: serde_json::Value = serde_json::from_str(&json).expect("Invalid JSON output");

    assert_eq!(value["discovery_complete"], true);
    assert_eq!(value["discovered_hosts"], 3);
}

/// Test SetupState with HostProbeResult serialization using fixtures.
#[test]
fn setup_state_with_probe_results() {
    // Load a probe result from fixture instead of manual construction
    let probe = probe_fixtures::no_cass_host();

    let state = SetupState {
        probed_hosts: vec![probe],
        probing_complete: true,
        ..Default::default()
    };

    // Serialize and deserialize
    let json = serde_json::to_string(&state).expect("Failed to serialize");
    let deserialized: SetupState = serde_json::from_str(&json).expect("Failed to deserialize");

    assert_eq!(deserialized.probed_hosts.len(), 1);
    // Verify fixture data (no_cass_host.json has host_name: "macbook-air")
    assert!(!deserialized.probed_hosts[0].host_name.is_empty());
    assert!(deserialized.probed_hosts[0].reachable);
}

/// Test SetupState with multiple fixture-based probe results.
#[test]
fn setup_state_with_multiple_probe_results() {
    // Load different host scenarios from fixtures
    let indexed = probe_fixtures::indexed_host();
    let no_cass = probe_fixtures::no_cass_host();
    let unreachable = probe_fixtures::unreachable_host();

    let state = SetupState {
        probed_hosts: vec![indexed.clone(), no_cass.clone(), unreachable.clone()],
        probing_complete: true,
        ..Default::default()
    };

    // Serialize and deserialize roundtrip
    let json = serde_json::to_string(&state).expect("Failed to serialize");
    let deserialized: SetupState = serde_json::from_str(&json).expect("Failed to deserialize");

    assert_eq!(deserialized.probed_hosts.len(), 3);

    // Verify indexed host has cass installed
    assert!(deserialized.probed_hosts[0].cass_status.is_installed());
    // Verify no_cass host doesn't have cass
    assert!(!deserialized.probed_hosts[1].cass_status.is_installed());
    // Verify unreachable host is not reachable
    assert!(!deserialized.probed_hosts[2].reachable);
}

// =============================================================================
// SetupError Tests
// =============================================================================

/// Test SetupError::Cancelled display.
#[test]
fn setup_error_cancelled_display() {
    let err = SetupError::Cancelled;
    assert_eq!(format!("{err}"), "Setup cancelled by user");
}

/// Test SetupError::NoHosts display.
#[test]
fn setup_error_no_hosts_display() {
    let err = SetupError::NoHosts;
    assert_eq!(format!("{err}"), "No SSH hosts found or selected");
}

/// Test SetupError::Interrupted display.
#[test]
fn setup_error_interrupted_display() {
    let err = SetupError::Interrupted;
    assert_eq!(format!("{err}"), "Setup interrupted");
}

/// Test SetupError::Io display.
#[test]
fn setup_error_io_display() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
    let err = SetupError::Io(io_err);
    assert!(format!("{err}").contains("IO error"));
}

/// Test SetupError::Json display.
#[test]
fn setup_error_json_display() {
    // Create a JSON error by parsing invalid JSON
    let json_err = serde_json::from_str::<SetupState>("invalid json").unwrap_err();
    let err = SetupError::Json(json_err);
    assert!(format!("{err}").contains("JSON error"));
}

// =============================================================================
// SetupResult Tests
// =============================================================================

/// Test SetupResult structure.
#[test]
fn setup_result_structure() {
    let result = SetupResult {
        sources_added: 3,
        hosts_installed: 1,
        hosts_indexed: 2,
        total_sessions: 150,
        dry_run: false,
        sync_pending: false,
    };

    assert_eq!(result.sources_added, 3);
    assert_eq!(result.hosts_installed, 1);
    assert_eq!(result.hosts_indexed, 2);
    assert_eq!(result.total_sessions, 150);
    assert!(!result.dry_run);
}

/// Test SetupResult for dry run.
#[test]
fn setup_result_dry_run() {
    let result = SetupResult {
        sources_added: 5,
        hosts_installed: 2,
        hosts_indexed: 3,
        total_sessions: 0,
        dry_run: true,
        sync_pending: false,
    };

    assert!(result.dry_run);
    assert_eq!(result.sources_added, 5);
}

// =============================================================================
// CassStatus Helper Tests (used in setup workflow)
// =============================================================================

/// Test CassStatus::is_installed() for NotFound.
#[test]
fn cass_status_not_found_not_installed() {
    let status = CassStatus::NotFound;
    assert!(!status.is_installed());
}

/// Test CassStatus::is_installed() for Unknown.
#[test]
fn cass_status_unknown_not_installed() {
    let status = CassStatus::Unknown;
    assert!(!status.is_installed());
}

/// Test CassStatus::is_installed() for InstalledNotIndexed.
#[test]
fn cass_status_installed_not_indexed_is_installed() {
    let status = CassStatus::InstalledNotIndexed {
        version: "0.1.50".to_string(),
    };
    assert!(status.is_installed());
}

/// Test CassStatus::is_installed() for Indexed.
#[test]
fn cass_status_indexed_is_installed() {
    let status = CassStatus::Indexed {
        version: "0.1.50".to_string(),
        session_count: 100,
        last_indexed: Some("2025-01-01T00:00:00Z".to_string()),
    };
    assert!(status.is_installed());
}

// =============================================================================
// State Workflow Tests
// =============================================================================

/// Test state progression through phases.
#[test]
fn setup_state_phase_progression() {
    let mut state = SetupState::default();

    // Phase 1: Discovery
    assert!(!state.has_progress());
    state.discovery_complete = true;
    state.discovered_hosts = 5;
    state.discovered_host_names = vec![
        "host1".to_string(),
        "host2".to_string(),
        "host3".to_string(),
        "host4".to_string(),
        "host5".to_string(),
    ];
    assert!(state.has_progress());

    // Phase 2: Probing
    state.probing_complete = true;

    // Phase 3: Selection
    state.selection_complete = true;
    state.selected_host_names = vec!["host1".to_string(), "host2".to_string()];

    // Phase 4: Installation
    state.installation_complete = true;
    state.completed_installs = vec!["host2".to_string()];

    // Phase 5: Indexing
    state.indexing_complete = true;
    state.completed_indexes = vec!["host1".to_string(), "host2".to_string()];

    // Phase 6: Configuration
    state.configuration_complete = true;

    // Phase 7: Sync
    state.sync_complete = true;

    // Verify all phases recorded
    assert!(state.discovery_complete);
    assert!(state.probing_complete);
    assert!(state.selection_complete);
    assert!(state.installation_complete);
    assert!(state.indexing_complete);
    assert!(state.configuration_complete);
    assert!(state.sync_complete);

    // Verify state can be serialized
    let json = serde_json::to_string(&state).unwrap();
    let restored: SetupState = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.discovered_hosts, 5);
    assert_eq!(restored.selected_host_names.len(), 2);
    assert_eq!(restored.completed_installs.len(), 1);
    assert_eq!(restored.completed_indexes.len(), 2);
}

/// Test that sync_complete doesn't affect has_progress().
/// has_progress() is used to determine if there's a resumable session,
/// and sync_complete being true means the setup is done, not resumable.
#[test]
fn setup_state_sync_complete_not_in_has_progress() {
    let mut state = SetupState::default();
    state.sync_complete = true;

    // sync_complete alone doesn't trigger has_progress (correct behavior)
    // because has_progress checks only the phases that represent actual work
    assert!(
        !state.has_progress(),
        "sync_complete alone should not indicate resumable progress"
    );
}

#[cfg(target_os = "linux")]
mod retry {
    use super::*;
    use coding_agent_search::sources::config::SourcesConfig;
    use coding_agent_search::sources::probe::{
        DetectedAgent, HostProbeResult, ResourceInfo, SystemInfo,
    };
    use coding_agent_search::sources::setup::SetupDiscoveryMode;
    use std::collections::BTreeSet;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;
    use std::process::Output;
    use std::time::Duration;

    const HOSTS: [&str; 3] = ["100.64.0.1", "100.64.0.2", "100.64.0.3"];
    const CONFIG: &str = "disabled_agents = [\"openclaw\"]\n\
        [[sources]]\nname = \"existing\"\ntype = \"ssh\"\nhost = \"operator@existing\"\n\
        paths = [\"~/.codex/sessions\"]\nsync_schedule = \"daily\"\n";

    struct RetryFixture {
        root: tempfile::TempDir,
        initial: SetupState,
        config_inode: u64,
    }

    impl RetryFixture {
        fn new(indexing: bool) -> Self {
            assert!(
                std::process::Command::new("/usr/bin/ssh")
                    .arg("-V")
                    .output()
                    .expect("real OpenSSH is required")
                    .status
                    .success()
            );
            let root = tempfile::tempdir().unwrap();
            for path in ["home", "cache/cass", "config/cass"] {
                fs::create_dir_all(root.path().join(path)).unwrap();
            }
            let config = root.path().join("config/cass/sources.toml");
            fs::write(&config, CONFIG).unwrap();
            let config_inode = fs::symlink_metadata(&config).unwrap().ino();
            let recorder = root.path().join("refuse-transport.sh");
            fs::write(
                &recorder,
                "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$1\" >> \"$CASS_TEST_TRANSPORTS\"\nexit 1\n",
            )
            .unwrap();
            fs::write(
                root.path().join("ssh_config"),
                format!(
                    "Host *\n ProxyCommand /bin/sh \"{}\" %h\n BatchMode yes\n ConnectionAttempts 1\n",
                    recorder.display()
                ),
            )
            .unwrap();
            // These are persisted resume inputs, not replacement implementations
            // of the installer or indexer. All attempted SSH uses real OpenSSH.
            let probes = HOSTS
                .iter()
                .map(|name| HostProbeResult {
                    host_name: (*name).into(),
                    reachable: true,
                    connection_time_ms: 1,
                    cass_status: if indexing {
                        CassStatus::InstalledNotIndexed {
                            version: "0.10.0".into(),
                        }
                    } else {
                        CassStatus::NotFound
                    },
                    detected_agents: vec![DetectedAgent {
                        agent_type: "claude_code".into(),
                        path: "~/.claude/projects".into(),
                        estimated_sessions: Some(1),
                        estimated_size_mb: Some(1),
                    }],
                    system_info: Some(SystemInfo {
                        os: "linux".into(),
                        arch: "x86_64".into(),
                        // glibc 2.27 gets no pre-built binary (no checksum
                        // request) and wget without curl gets no bootstrap: the
                        // only install candidate is the source build over the
                        // refusing SSH.
                        distro: Some("Ubuntu 18.04".into()),
                        has_cargo: true,
                        has_cargo_binstall: false,
                        has_curl: false,
                        has_wget: true,
                        remote_home: "/home/developer".into(),
                        machine_id: None,
                    }),
                    resources: Some(ResourceInfo {
                        disk_available_mb: 16384,
                        memory_total_mb: 16384,
                        memory_available_mb: 8192,
                        can_compile: true,
                    }),
                    error: None,
                })
                .collect();
            let names: Vec<String> = HOSTS.iter().map(|name| (*name).into()).collect();
            let initial = SetupState {
                discovery_complete: true,
                discovered_hosts: names.len(),
                discovered_host_names: names.clone(),
                discovery_mode: Some(SetupDiscoveryMode::Tailscale),
                probing_complete: true,
                probed_hosts: probes,
                selection_complete: true,
                selected_host_names: names.clone(),
                installation_complete: indexing,
                completed_installs: if indexing {
                    names
                } else {
                    vec![HOSTS[0].into()]
                },
                completed_indexes: vec![HOSTS[0].into()],
                started_at: Some("2026-10-01T00:00:00Z".into()),
                ..Default::default()
            };
            let fixture = Self {
                root,
                initial,
                config_inode,
            };
            fixture.save_initial();
            fixture
        }

        fn state_path(&self) -> PathBuf {
            self.root.path().join("cache/cass/setup_state.json")
        }

        fn save_initial(&self) {
            fs::write(
                self.state_path(),
                serde_json::to_vec(&self.initial).unwrap(),
            )
            .unwrap();
        }

        fn run(&self, extra: &[&str]) -> Output {
            let mut command = assert_cmd::Command::new(env!("CARGO_BIN_EXE_cass"));
            command
                .env_clear()
                .current_dir(self.root.path())
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", self.root.path().join("home"))
                .env("XDG_CONFIG_HOME", self.root.path().join("config"))
                .env("XDG_CACHE_HOME", self.root.path().join("cache"))
                .env("XDG_DATA_HOME", self.root.path().join("data"))
                .env("CASS_DATA_DIR", self.root.path().join("cass-data"))
                .env("CASS_SSH_CONFIG", self.root.path().join("ssh_config"))
                .env("CASS_TEST_TRANSPORTS", self.root.path().join("transports"))
                .args(["sources", "setup", "--resume", "--non-interactive"])
                .args(extra)
                .timeout(Duration::from_secs(30))
                .output()
                .unwrap()
        }

        fn transports(&self) -> BTreeSet<String> {
            match fs::read_to_string(self.root.path().join("transports")) {
                Ok(log) => log.lines().map(str::to_owned).collect(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
                Err(error) => panic!("cannot read transport observations: {error}"),
            }
        }

        fn assert_paused(&self, output: &Output, phase: &str, transport: bool) {
            assert!(!output.status.success(), "{output:?}");
            let diagnostics = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let error_kind = if phase == "Installing" {
                "Install error:"
            } else {
                "Index error:"
            };
            assert!(diagnostics.contains(error_kind), "{output:?}");
            assert!(diagnostics.contains(HOSTS[1]), "{output:?}");
            assert!(diagnostics.contains("--resume"), "{output:?}");
            assert!(!diagnostics.contains("Setup Complete"), "{output:?}");
            let saved: SetupState =
                serde_json::from_slice(&fs::read(self.state_path()).unwrap()).unwrap();
            let mut expected = self.initial.clone();
            expected.current_operation = Some(format!("{phase} on {}", HOSTS[1]));
            assert_eq!(
                serde_json::to_value(saved).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
            let config = self.root.path().join("config/cass/sources.toml");
            assert_eq!(fs::read(&config).unwrap(), CONFIG.as_bytes());
            assert_eq!(
                fs::symlink_metadata(&config).unwrap().ino(),
                self.config_inode
            );
            assert_eq!(fs::read_dir(config.parent().unwrap()).unwrap().count(), 1);
            assert_eq!(
                self.transports(),
                if transport {
                    BTreeSet::from([HOSTS[1].into()])
                } else {
                    BTreeSet::new()
                }
            );
        }

        fn assert_configured(&self, output: &Output) {
            assert!(output.status.success(), "{output:?}");
            assert!(!self.state_path().exists());
            assert!(self.transports().is_empty());
            let path = self.root.path().join("config/cass/sources.toml");
            let config = SourcesConfig::load_from(&path).unwrap();
            assert_eq!(config.sources.len(), 4);
            assert_eq!(config.disabled_agents, vec!["openclaw"]);
            assert_eq!(
                config.find_source("existing").unwrap().host.as_deref(),
                Some("operator@existing")
            );
            for host in HOSTS {
                let source = config.find_source(host).unwrap();
                assert_eq!(source.host.as_deref(), Some(host));
                assert_eq!(source.paths, vec!["~/.claude/projects"]);
            }
            let backups: Vec<_> = fs::read_dir(path.parent().unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|entry| {
                    entry
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("sources.toml.backup.")
                })
                .collect();
            assert_eq!(backups.len(), 1);
            assert_eq!(fs::read(&backups[0]).unwrap(), CONFIG.as_bytes());
        }

        fn assert_scope_refused(&self, output: &Output, before: &[u8], inode: u64) {
            assert!(!output.status.success(), "{output:?}");
            let diagnostics = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            for expected in [
                "Cannot change --hosts",
                "without --resume",
                "No remote operations were started",
                "checkpoint is unchanged",
            ] {
                assert!(diagnostics.contains(expected), "{output:?}");
            }
            assert!(!diagnostics.contains("Setup Complete"), "{output:?}");
            assert!(self.transports().is_empty());
            assert_eq!(fs::read(self.state_path()).unwrap(), before);
            assert_eq!(
                fs::symlink_metadata(self.state_path()).unwrap().ino(),
                inode
            );
            assert_eq!(
                fs::read_dir(self.state_path().parent().unwrap())
                    .unwrap()
                    .count(),
                1
            );
            let config = self.root.path().join("config/cass/sources.toml");
            assert_eq!(fs::read(&config).unwrap(), CONFIG.as_bytes());
            assert_eq!(
                fs::symlink_metadata(&config).unwrap().ino(),
                self.config_inode
            );
            assert_eq!(fs::read_dir(config.parent().unwrap()).unwrap().count(), 1);
        }
    }

    #[test]
    fn setup_install_failure_retains_completed_work_and_can_resume() {
        for json in [false, true] {
            for skip_sync in [false, true] {
                let fixture = RetryFixture::new(false);
                let mut flags = Vec::new();
                if json {
                    flags.push("--json");
                }
                if skip_sync {
                    flags.push("--skip-sync");
                }
                for _ in 0..2 {
                    let output = fixture.run(&flags);
                    fixture.assert_paused(&output, "Installing", true);
                }
                // An explicit skip is different from silently swallowing a
                // failure. The retained checkpoint permits this caller choice.
                fs::write(fixture.root.path().join("transports"), "").unwrap();
                let output =
                    fixture.run(&["--json", "--skip-install", "--skip-index", "--skip-sync"]);
                fixture.assert_configured(&output);
            }
        }
    }

    #[test]
    fn setup_index_failure_retains_completed_work_and_can_resume() {
        for json in [false, true] {
            for skip_sync in [false, true] {
                let fixture = RetryFixture::new(true);
                let mut flags = vec!["--skip-install"];
                if json {
                    flags.push("--json");
                }
                if skip_sync {
                    flags.push("--skip-sync");
                }
                for _ in 0..2 {
                    let output = fixture.run(&flags);
                    fixture.assert_paused(&output, "Indexing", true);
                }
                fs::write(fixture.root.path().join("transports"), "").unwrap();
                let output = fixture.run(&["--json", "--skip-index", "--skip-sync"]);
                fixture.assert_configured(&output);
            }
        }
    }

    #[test]
    fn setup_install_preconditions_keep_checkpoint_without_starting_ssh() {
        for missing in ["system", "resources", "disk", "method"] {
            let mut fixture = RetryFixture::new(false);
            let pending = &mut fixture.initial.probed_hosts[1];
            match missing {
                "system" => pending.system_info = None,
                "resources" => pending.resources = None,
                "disk" => pending.resources.as_mut().unwrap().disk_available_mb = 0,
                "method" => pending.system_info.as_mut().unwrap().has_cargo = false,
                _ => unreachable!(),
            }
            fixture.save_initial();
            for _ in 0..2 {
                let output = fixture.run(&["--json", "--skip-sync"]);
                fixture.assert_paused(&output, "Installing", false);
            }
        }
    }

    #[test]
    fn setup_completed_remote_work_finishes_without_repeating_it() {
        let mut fixture = RetryFixture::new(false);
        fixture.initial.completed_installs = HOSTS.iter().map(|host| (*host).into()).collect();
        fixture.initial.completed_indexes = fixture.initial.completed_installs.clone();
        // The process exited after the last per-host checkpoint but before
        // either phase flag. Reconcile those flags without reinstalling or
        // reindexing hosts whose completed work is already on disk.
        fixture.save_initial();
        let output = fixture.run(&["--json", "--skip-sync"]);
        fixture.assert_configured(&output);
    }

    #[test]
    fn setup_resume_changed_host_scope_never_reaches_old_targets() {
        for (indexing, selection_complete) in [(false, false), (false, true), (true, true)] {
            for mode in [None, Some("--json"), Some("--dry-run")] {
                let mut fixture = RetryFixture::new(indexing);
                if !selection_complete {
                    fixture.initial.selection_complete = false;
                    fixture.initial.selected_host_names.clear();
                    fixture.initial.completed_installs.clear();
                    fixture.initial.completed_indexes.clear();
                }
                fixture.save_initial();
                let before = fs::read(fixture.state_path()).unwrap();
                let inode = fs::symlink_metadata(fixture.state_path()).unwrap().ino();
                let mut flags = vec!["--hosts", HOSTS[2]];
                flags.extend(mode);
                for _ in 0..2 {
                    let output = fixture.run(&flags);
                    // Previously the new list was ignored after probing; the
                    // old selection could install/index on .1 or .2 instead.
                    fixture.assert_scope_refused(&output, &before, inode);
                }
            }
        }
    }

    #[test]
    fn setup_resume_equivalent_host_scope_preserves_selection_and_successes() {
        for requested in [
            vec![HOSTS[2], HOSTS[1], HOSTS[0], HOSTS[2]],
            vec![HOSTS[1], HOSTS[0], HOSTS[1]],
        ] {
            let mut fixture = RetryFixture::new(false);
            fixture.initial.selected_host_names = vec![HOSTS[0].into(), HOSTS[1].into()];
            fixture.save_initial();
            let argument = requested.join(",");
            let output = fixture.run(&["--json", "--hosts", &argument]);
            // Accept both the original discovered set and the saved selection.
            // Neither may expand that selection or repeat completed host .1.
            fixture.assert_paused(&output, "Installing", true);
        }
    }

    #[test]
    fn setup_resume_scope_refusal_preserves_configuration_retry() {
        let mut fixture = RetryFixture::new(false);
        fixture.initial.completed_installs = HOSTS.iter().map(|host| (*host).into()).collect();
        fixture.initial.completed_indexes = fixture.initial.completed_installs.clone();
        fixture.save_initial();
        let before = fs::read(fixture.state_path()).unwrap();
        let inode = fs::symlink_metadata(fixture.state_path()).unwrap().ino();
        for requested in [
            "100.64.0.99",
            "100.64.0.1,100.64.0.2,100.64.0.3,100.64.0.99",
            "100.64.0.1,operator@100.64.0.2,100.64.0.3",
        ] {
            let output = fixture.run(&["--json", "--hosts", requested, "--skip-sync"]);
            // Even when no remote install/index is due, do not configure the
            // old targets, return success or clear their resumable progress.
            fixture.assert_scope_refused(&output, &before, inode);
        }
        let argument = format!("{},{},{},{}", HOSTS[2], HOSTS[0], HOSTS[1], HOSTS[0]);
        let output = fixture.run(&["--json", "--hosts", &argument, "--skip-sync"]);
        fixture.assert_configured(&output);
    }
}
