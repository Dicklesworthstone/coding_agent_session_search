//! GH505 CLI boundary regressions. Only the external Tailscale status command
//! supplies a deterministic fixture (see the no-mock policy allowance). The
//! production discovery/setup code and real OpenSSH run unchanged. ProxyCommand
//! records transport attempts and fails locally, so no tailnet is contacted.
#![cfg(target_os = "linux")]

use coding_agent_search::sources::setup::{SetupDiscoveryMode, SetupState};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;

struct FleetFixture {
    root: tempfile::TempDir,
    bin: PathBuf,
    ssh_config: PathBuf,
    status: PathBuf,
    status_calls: PathBuf,
    transports: PathBuf,
}

fn fake_tailscale_status() -> &'static str {
    r#"#!/bin/sh
set -eu
[ "$#" -eq 2 ] && [ "$1" = status ] && [ "$2" = --json ] || exit 64
printf 'status --json\n' >> "$CASS_TEST_STATUS_CALLS"
exec /bin/cat "$CASS_TEST_STATUS_JSON"
"#
}

impl FleetFixture {
    fn new(peers: Value, aliases: &str) -> Self {
        // A missing real SSH binary is a test failure, not a silent pass.
        assert!(
            std::process::Command::new("/usr/bin/ssh")
                .arg("-V")
                .output()
                .unwrap()
                .status
                .success()
        );
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(root.path().join("home")).unwrap();
        fs::create_dir_all(root.path().join("cache/cass")).unwrap();
        let status = root.path().join("status.json");
        fs::write(
            &status,
            serde_json::to_vec(&json!({"BackendState": "Running", "Peer": peers})).unwrap(),
        )
        .unwrap();
        let tailscale = bin.join("tailscale");
        fs::write(&tailscale, fake_tailscale_status()).unwrap();
        fs::set_permissions(&tailscale, fs::Permissions::from_mode(0o755)).unwrap();
        let transport = root.path().join("record-transport.sh");
        fs::write(
            &transport,
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$CASS_TEST_TRANSPORTS\"\nexit 1\n",
        )
        .unwrap();
        let ssh_config = root.path().join("ssh_config");
        fs::write(&ssh_config, format!(
            "Host *\n  ProxyCommand /bin/sh \"{}\" %h\n  BatchMode yes\n  ConnectionAttempts 1\n{aliases}",
            transport.display(),
        )).unwrap();
        let status_calls = root.path().join("status-calls");
        let transports = root.path().join("transports");
        Self {
            root,
            bin,
            ssh_config,
            status,
            status_calls,
            transports,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = assert_cmd::Command::new(env!("CARGO_BIN_EXE_cass"));
        command
            .env_clear()
            .current_dir(self.root.path())
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("HOME", self.root.path().join("home"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("CASS_DATA_DIR", self.root.path().join("cass-data"))
            .env("CASS_SSH_CONFIG", &self.ssh_config)
            .env("CASS_TEST_STATUS_JSON", &self.status)
            .env("CASS_TEST_STATUS_CALLS", &self.status_calls)
            .env("CASS_TEST_TRANSPORTS", &self.transports)
            .env("CASS_TEST_SETUP_STATE", self.state_path())
            .env("CASS_TEST_SNAPSHOTS", self.root.path().join("snapshots"))
            .args(args)
            .timeout(Duration::from_secs(30))
            .output()
            .unwrap()
    }

    fn attempted_transports(&self) -> BTreeSet<String> {
        fs::read_to_string(&self.transports)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn state_path(&self) -> PathBuf {
        self.root.path().join("cache/cass/setup_state.json")
    }

    fn save_discovery(&self, mode: Option<SetupDiscoveryMode>, names: &[&str]) {
        let state = SetupState {
            discovery_complete: true,
            discovered_hosts: names.len(),
            discovered_host_names: names.iter().map(|name| (*name).to_owned()).collect(),
            discovery_mode: mode,
            ..Default::default()
        };
        let mut json = serde_json::to_value(state).unwrap();
        if mode.is_none() {
            // Exercise the actual old format, not just a new struct's default.
            json.as_object_mut().unwrap().remove("discovery_mode");
        }
        fs::write(self.state_path(), serde_json::to_vec(&json).unwrap()).unwrap();
    }

    fn save_legacy_discovery(&self) {
        self.save_discovery(None, &["100.90.0.1", "100.90.0.2"]);
    }

    fn capture_state_at_transport(&self) {
        fs::create_dir_all(self.root.path().join("snapshots")).unwrap();
        fs::write(
            self.root.path().join("record-transport.sh"),
            r#"#!/bin/sh
set -eu
printf '%s\n' "$1" >> "$CASS_TEST_TRANSPORTS"
/bin/cp "$CASS_TEST_SETUP_STATE" "$CASS_TEST_SNAPSHOTS/$1.json"
exit 1
"#,
        )
        .unwrap();
    }

    fn setup(&self, resume: bool) -> Output {
        self.setup_with_options(resume, &["--tailscale"])
    }

    fn setup_with_options(&self, resume: bool, discovery_args: &[&str]) -> Output {
        let mut args = vec![
            "sources",
            "setup",
            "--non-interactive",
            "--skip-install",
            "--skip-index",
            "--skip-sync",
            "--json",
        ];
        if resume {
            args.push("--resume");
        }
        args.extend_from_slice(discovery_args);
        self.run(&args)
    }
}

fn mullvad_peers() -> Value {
    json!({
        "tag-only": {"Online": true, "Tags": ["tag:mullvad-exit-node"], "TailscaleIPs": ["100.90.0.1"]},
        "dns-only": {"Online": true, "DNSName": "SE-STO-WG-001.MULLVAD.TS.NET.", "Tags": null, "TailscaleIPs": ["100.90.0.2"]}
    })
}

fn mixed_peers() -> Value {
    let mut peers = mullvad_peers();
    peers["alias"] = json!({"Online": true, "DNSName": "workstation.example.ts.net.", "TailscaleIPs": ["100.64.0.1"]});
    peers["ordinary-exit"] = json!({"Online": true, "ExitNode": true, "ExitNodeOption": true, "Location": {"Country": "Sweden"}, "Tags": [], "TailscaleIPs": ["100.64.0.2"]});
    peers["offline"] = json!({"Online": false, "TailscaleIPs": ["100.64.0.3"]});
    peers["sharee"] = json!({"Online": true, "ShareeNode": true, "TailscaleIPs": ["100.64.0.4"]});
    peers
}

const ALIAS: &str = "Host workstation\n  HostName 100.64.0.1\n  User developer\n";

/// `sources setup --json` reports "nothing to configure" as exit 0 with
/// `status: no_hosts`, both when discovery is empty and when every probe
/// fails. The ProxyCommand refuses every transport, so each setup here ends
/// that way; which transports it attempted is what distinguishes the cases.
fn assert_setup_no_hosts(output: &Output) {
    assert!(output.status.success(), "{output:?}");
    // Stdout that is not JSON reads as Null and fails the status check below.
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    assert_eq!(report["status"], "no_hosts", "{output:?}");
}

#[test]
fn gh505_discovery_and_setup_never_probe_mullvad_peers() {
    let fixture = FleetFixture::new(mixed_peers(), ALIAS);
    let output = fixture.run(&["sources", "discover", "--tailscale", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let discovery: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        discovery["hosts"].as_array().unwrap().len(),
        2,
        "{discovery}"
    );
    assert!(fixture.attempted_transports().is_empty());

    let output = fixture.setup(false);
    assert_setup_no_hosts(&output);
    assert_eq!(
        fixture.attempted_transports(),
        BTreeSet::from(["100.64.0.1".into(), "100.64.0.2".into()])
    );
    assert_eq!(
        fs::read_to_string(&fixture.status_calls)
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn gh505_all_mullvad_setup_does_not_start_ssh_transport() {
    let fixture = FleetFixture::new(mullvad_peers(), "");
    let output = fixture.run(&["sources", "discover", "--tailscale", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let discovery: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(discovery["status"], "no_hosts", "{discovery}");
    let output = fixture.setup(false);
    assert_setup_no_hosts(&output);
    assert!(fixture.attempted_transports().is_empty());
    assert_eq!(
        fs::read_to_string(&fixture.status_calls)
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn gh505_resumed_setup_refreshes_legacy_unfiltered_discovery() {
    let fixture = FleetFixture::new(mixed_peers(), ALIAS);
    fixture.save_legacy_discovery();
    let output = fixture.setup(true);
    assert_setup_no_hosts(&output);
    assert_eq!(
        fixture.attempted_transports(),
        BTreeSet::from(["100.64.0.1".into(), "100.64.0.2".into()])
    );
    assert_eq!(
        fs::read_to_string(&fixture.status_calls)
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn gh505_resumed_setup_with_bad_status_keeps_aliases_not_cached_peers() {
    let fixture = FleetFixture::new(mullvad_peers(), ALIAS);
    fixture.save_legacy_discovery();
    fs::write(&fixture.status, "invalid status JSON").unwrap();
    let output = fixture.setup(true);
    assert_setup_no_hosts(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Tailscale discovery unavailable"),
        "{output:?}"
    );
    assert_eq!(
        fixture.attempted_transports(),
        BTreeSet::from(["100.64.0.1".into()])
    );
    assert_eq!(
        fs::read_to_string(&fixture.status_calls)
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn gh505_legacy_resume_without_tailscale_never_replays_cached_peers() {
    let fixture = FleetFixture::new(mixed_peers(), ALIAS);
    fixture.save_legacy_discovery();
    let output = fixture.setup_with_options(true, &[]);
    assert_setup_no_hosts(&output);
    assert_eq!(
        fixture.attempted_transports(),
        BTreeSet::from(["100.64.0.1".into()])
    );
    // Missing provenance must not silently opt an SSH-only setup into Tailscale.
    assert!(!fixture.status_calls.exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no provider metadata"), "{output:?}");
    assert!(stderr.contains("--tailscale or --hosts"), "{output:?}");
}

#[test]
fn gh505_legacy_resume_without_flags_or_aliases_never_starts_transport() {
    let fixture = FleetFixture::new(mullvad_peers(), "");
    fixture.save_legacy_discovery();
    let output = fixture.setup_with_options(true, &[]);
    assert_setup_no_hosts(&output);
    assert!(fixture.attempted_transports().is_empty());
    assert!(!fixture.status_calls.exists());
}

#[test]
fn gh505_saved_tailscale_mode_refreshes_without_repeating_flag() {
    let fixture = FleetFixture::new(mixed_peers(), ALIAS);
    fixture.save_discovery(
        Some(SetupDiscoveryMode::Tailscale),
        &["100.90.0.1", "100.90.0.2"],
    );
    let output = fixture.setup_with_options(true, &[]);
    assert_setup_no_hosts(&output);
    assert_eq!(
        fixture.attempted_transports(),
        BTreeSet::from(["100.64.0.1".into(), "100.64.0.2".into()])
    );
    assert_eq!(
        fs::read_to_string(&fixture.status_calls).unwrap(),
        "status --json\n"
    );
}

#[test]
fn gh505_saved_tailscale_mode_bad_status_never_falls_back_to_cache() {
    let fixture = FleetFixture::new(mullvad_peers(), ALIAS);
    fixture.save_discovery(
        Some(SetupDiscoveryMode::Tailscale),
        &["100.90.0.1", "100.90.0.2"],
    );
    fs::write(&fixture.status, "invalid status JSON").unwrap();
    let output = fixture.setup_with_options(true, &[]);
    assert_setup_no_hosts(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Tailscale discovery unavailable"),
        "{output:?}"
    );
    assert_eq!(
        fixture.attempted_transports(),
        BTreeSet::from(["100.64.0.1".into()])
    );
    assert_eq!(
        fs::read_to_string(&fixture.status_calls).unwrap(),
        "status --json\n"
    );
}

#[test]
fn gh505_saved_ssh_mode_refreshes_aliases_without_enabling_tailscale() {
    let fixture = FleetFixture::new(mixed_peers(), ALIAS);
    fixture.save_discovery(Some(SetupDiscoveryMode::Ssh), &["100.64.0.9"]);
    let output = fixture.setup_with_options(true, &[]);
    assert_setup_no_hosts(&output);
    assert_eq!(
        fixture.attempted_transports(),
        BTreeSet::from(["100.64.0.1".into()])
    );
    assert!(!fixture.status_calls.exists());
}

#[test]
fn gh505_saved_explicit_hosts_are_preserved_without_discovery() {
    let fixture = FleetFixture::new(mixed_peers(), ALIAS);
    fixture.save_discovery(Some(SetupDiscoveryMode::Explicit), &["100.64.0.9"]);
    let output = fixture.setup_with_options(true, &[]);
    assert_setup_no_hosts(&output);
    // An explicitly requested machine need not be in SSH config or Tailscale.
    assert_eq!(
        fixture.attempted_transports(),
        BTreeSet::from(["100.64.0.9".into()])
    );
    assert!(!fixture.status_calls.exists());
}

#[test]
fn gh505_current_explicit_hosts_override_unfinished_legacy_candidates() {
    let fixture = FleetFixture::new(mixed_peers(), ALIAS);
    fixture.save_legacy_discovery();
    let output = fixture.setup_with_options(true, &["--hosts", "workstation", "--tailscale"]);
    assert_setup_no_hosts(&output);
    assert_eq!(
        fixture.attempted_transports(),
        BTreeSet::from(["100.64.0.1".into()])
    );
    assert!(!fixture.status_calls.exists());
}

#[test]
fn gh505_completed_probing_does_not_restart_discovery_or_transport() {
    let fixture = FleetFixture::new(mixed_peers(), ALIAS);
    fixture.save_legacy_discovery();
    let mut state: Value = serde_json::from_slice(&fs::read(fixture.state_path()).unwrap()).unwrap();
    // The completed probe phase found no reachable hosts; no later phase is due.
    state["probing_complete"] = json!(true);
    fs::write(fixture.state_path(), serde_json::to_vec(&state).unwrap()).unwrap();
    let output = fixture.setup(true);
    assert_setup_no_hosts(&output);
    assert!(fixture.attempted_transports().is_empty());
    assert!(!fixture.status_calls.exists());
}

#[test]
fn gh505_real_saved_provider_mode_survives_resume_without_flags() {
    let peers = json!({
        "ordinary-exit": {"Online": true, "ExitNodeOption": true, "TailscaleIPs": ["100.64.0.2"]}
    });
    let fixture = FleetFixture::new(peers, "");
    fixture.capture_state_at_transport();
    let output = fixture.setup(false);
    assert_setup_no_hosts(&output);
    assert_eq!(
        fixture.attempted_transports(),
        BTreeSet::from(["100.64.0.2".into()])
    );
    // Capture actual production state just after discovery and before probing
    // completes. No killed children, timing races, or synthesized provenance.
    let snapshot = fs::read(fixture.root.path().join("snapshots/100.64.0.2.json")).unwrap();
    let state: Value = serde_json::from_slice(&snapshot).unwrap();
    assert_eq!(state["discovery_mode"], "tailscale");
    assert_eq!(state["discovery_complete"], true);
    assert_eq!(state["probing_complete"], false);
    assert_eq!(state["discovered_host_names"], json!(["100.64.0.2"]));

    // Resume that exact saved state after the old peer goes offline and a new
    // real machine appears alongside provider endpoints. Only current eligible
    // peers may reach the real OpenSSH transport, even without --tailscale.
    let mut peers = mullvad_peers();
    peers["previous"] = json!({"Online": false, "TailscaleIPs": ["100.64.0.2"]});
    peers["new-machine"] = json!({"Online": true, "TailscaleIPs": ["100.64.0.9"]});
    fs::write(
        &fixture.status,
        serde_json::to_vec(&json!({"BackendState": "Running", "Peer": peers})).unwrap(),
    )
    .unwrap();
    fs::write(fixture.state_path(), &snapshot).unwrap();
    fs::write(&fixture.transports, "").unwrap();
    let output = fixture.setup_with_options(true, &[]);
    assert_setup_no_hosts(&output);
    assert_eq!(
        fixture.attempted_transports(),
        BTreeSet::from(["100.64.0.9".into()])
    );
    assert_eq!(
        fs::read_to_string(&fixture.status_calls).unwrap(),
        "status --json\nstatus --json\n"
    );
}

const PROVIDER_ALIASES: &str = "Host provider-by-tag\n HostName 100.90.0.1\n\
    Host provider-by-dns\n HostName se-sto-wg-001.mullvad.ts.net\n\
    Host 100.90.0.2\n\
    Host workstation.example.ts.net\n HostName 100.90.0.1\n";

#[test]
fn gh505_configured_provider_aliases_never_reach_setup_transport() {
    // A provider-looking alias label can still be a legitimate SSH machine.
    let aliases = format!(
        "{ALIAS}{PROVIDER_ALIASES}Host se-sto-wg-001.mullvad.ts.net\n HostName 100.64.0.8\n"
    );
    let fixture = FleetFixture::new(mixed_peers(), &aliases);
    let output = fixture.run(&["sources", "discover", "--tailscale", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let discovery: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(discovery["hosts"].as_array().unwrap().len(), 3, "{discovery}");
    assert!(fixture.attempted_transports().is_empty());

    let output = fixture.setup(false);
    assert_setup_no_hosts(&output);
    assert_eq!(
        fixture.attempted_transports(),
        BTreeSet::from([
            "100.64.0.1".into(),
            "100.64.0.2".into(),
            "100.64.0.8".into(),
        ])
    );
}

#[test]
fn gh505_only_provider_aliases_do_not_start_transport_even_when_offline() {
    let mut peers = mullvad_peers();
    peers["tag-only"]["Online"] = json!(false);
    peers["dns-only"]["ShareeNode"] = json!(true);
    let fixture = FleetFixture::new(peers, PROVIDER_ALIASES);
    let output = fixture.run(&["sources", "discover", "--tailscale", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let discovery: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(discovery["status"], "no_hosts", "{discovery}");
    let output = fixture.setup(false);
    assert_setup_no_hosts(&output);
    assert!(fixture.attempted_transports().is_empty());
}

#[test]
fn gh505_bare_resume_filters_provider_aliases_before_peer_deduplication() {
    let fixture = FleetFixture::new(mixed_peers(), PROVIDER_ALIASES);
    fixture.save_discovery(
        Some(SetupDiscoveryMode::Tailscale),
        &["provider-by-tag", "100.90.0.2"],
    );
    let output = fixture.setup_with_options(true, &[]);
    assert_setup_no_hosts(&output);
    // The alias named workstation.example.ts.net points at a provider. It must
    // be removed before considering the real workstation's DNS name, even
    // though that real peer sorts ahead of the provider in the status map.
    assert_eq!(
        fixture.attempted_transports(),
        BTreeSet::from(["100.64.0.1".into(), "100.64.0.2".into()])
    );
    assert_eq!(
        fs::read_to_string(&fixture.status_calls).unwrap(),
        "status --json\n"
    );
}
