//! GH505 CLI boundary regressions. Only the external Tailscale status command
//! supplies a deterministic fixture (see the no-mock policy allowance). The
//! production discovery/setup code and real OpenSSH run unchanged. ProxyCommand
//! records transport attempts and fails locally, so no tailnet is contacted.
#![cfg(target_os = "linux")]

use coding_agent_search::sources::setup::SetupState;
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

    fn save_legacy_discovery(&self) {
        let state = SetupState {
            discovery_complete: true,
            discovered_hosts: 2,
            discovered_host_names: vec!["100.90.0.1".into(), "100.90.0.2".into()],
            ..Default::default()
        };
        fs::write(
            self.root.path().join("cache/cass/setup_state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
    }

    fn setup(&self, resume: bool) -> Output {
        let mut args = vec![
            "sources",
            "setup",
            "--tailscale",
            "--non-interactive",
            "--skip-install",
            "--skip-index",
            "--skip-sync",
            "--json",
        ];
        if resume {
            args.push("--resume");
        }
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
