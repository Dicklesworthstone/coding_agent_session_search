//! GH505: provider metadata, not exit-node capability, determines SSH eligibility.
use super::{merge_tailscale_hosts, parse_ssh_config};
use serde_json::json;

#[test]
fn gh505_tailscale_excludes_mullvad_tags_and_provider_dns() {
    // Reduced status records use the Tags/DNSName shape reported in GH505 and
    // defined by tailscale/ipn/ipnstate.PeerStatus. Each signal works alone.
    for mut peer in [
        json!({"Tags": ["tag:mullvad-exit-node"]}),
        json!({"Tags": ["tag:other", "tag:mullvad-exit-node"], "DNSName": ""}),
        json!({"Tags": ["tag:mullvad-exit-node"], "Location": null, "ExitNode": false}),
        json!({"DNSName": "se-sto-wg-001.mullvad.ts.net."}),
        json!({"DNSName": "se-sto-wg-001.mullvad.ts.net", "Tags": null}),
        json!({"DNSName": "SE-STO-WG-001.MULLVAD.TS.NET.", "Tags": []}),
        json!({"DNSName": "se-sto-wg-001.mullvad.ts.net.", "Tags": ["tag:other"], "Location": {"Country": "Sweden"}, "ExitNodeOption": true}),
    ] {
        peer["Online"] = json!(true);
        peer["TailscaleIPs"] = json!(["fd7a:115c:a1e0::1", "100.64.0.1"]);
        let bytes =
            serde_json::to_vec(&json!({"BackendState": "Running", "Peer": {"a": peer}})).unwrap();
        let mut hosts = Vec::new();
        merge_tailscale_hosts(&mut hosts, &bytes).unwrap();
        assert!(hosts.is_empty(), "Mullvad peer was discovered: {peer}");
    }
}

#[test]
fn gh505_tailscale_retains_normal_exit_nodes_and_lookalikes() {
    for mut peer in [
        json!({}),
        json!({"Tags": null}),
        json!({"Tags": []}),
        json!({"Tags": ["tag:server", "tag:mullvad-client"]}),
        json!({"Tags": ["tag:mullvad-exit-node-backup"]}),
        json!({"Location": {"Country": "Sweden", "City": "Stockholm"}}),
        json!({"ExitNode": true, "ExitNodeOption": true}),
        json!({"ExitNodeOption": true, "Location": {"Country": "Sweden"}}),
        json!({"HostName": "se-sto-wg-001.mullvad.ts.net."}),
        json!({"DNSName": "mullvad-workstation.example.ts.net."}),
        json!({"DNSName": "work.notmullvad.ts.net."}),
        json!({"DNSName": "work.mullvad.ts.net.example.ts.net."}),
        json!({"DNSName": "mullvad.ts.net."}),
    ] {
        peer["Online"] = json!(true);
        peer["TailscaleIPs"] = json!(["100.64.0.2"]);
        let bytes =
            serde_json::to_vec(&json!({"BackendState": "Running", "Peer": {"a": peer}})).unwrap();
        let mut hosts = Vec::new();
        merge_tailscale_hosts(&mut hosts, &bytes).unwrap();
        assert_eq!(hosts.len(), 1, "legitimate peer was excluded: {peer}");
        assert_eq!(hosts[0].connection_string(), "100.64.0.2");
    }
}

#[test]
fn gh505_tailscale_mixed_fleet_preserves_ssh_alias_metadata() {
    let mut hosts = parse_ssh_config(
        "Host workstation\n HostName 100.64.0.1\n User developer\n Port 2222\n IdentityFile ~/.ssh/work\n\
         Host server\n HostName SERVER.example.ts.net.\n User operator\n IdentityFile ~/.ssh/server\n",
    );
    let bytes = serde_json::to_vec(&json!({
        "BackendState": "Running",
        "Peer": {
            "a": {"Online": true, "DNSName": "workstation.example.ts.net.", "TailscaleIPs": ["100.64.0.1"], "Tags": null},
            "b": {"Online": true, "DNSName": "server.example.ts.net.", "TailscaleIPs": ["100.64.0.2"], "ExitNodeOption": true},
            "c": {"Online": true, "DNSName": "other.example.ts.net.", "TailscaleIPs": ["100.64.0.3"], "Location": {"Country": "Sweden"}},
            "d": {"Online": true, "HostName": "workstation", "TailscaleIPs": ["100.64.0.4"], "Tags": ["tag:mullvad-exit-node"]},
            "e": {"Online": true, "DNSName": "se-sto-wg-001.mullvad.ts.net.", "TailscaleIPs": ["100.64.0.5"]},
            "f": {"Online": false, "TailscaleIPs": ["100.64.0.6"]},
            "g": {"Online": true, "ShareeNode": true, "TailscaleIPs": ["100.64.0.7"]}
        }
    })).unwrap();
    for _ in 0..2 {
        merge_tailscale_hosts(&mut hosts, &bytes).unwrap();
        assert_eq!(hosts.len(), 3);
        assert_eq!(hosts[0].connection_string(), "developer@workstation");
        assert_eq!(hosts[0].hostname.as_deref(), Some("100.64.0.1"));
        assert_eq!(hosts[0].port, Some(2222));
        assert_eq!(hosts[0].identity_file.as_deref(), Some("~/.ssh/work"));
        assert_eq!(hosts[1].connection_string(), "operator@server");
        assert_eq!(hosts[1].hostname.as_deref(), Some("SERVER.example.ts.net."));
        assert_eq!(hosts[1].identity_file.as_deref(), Some("~/.ssh/server"));
        assert_eq!(hosts[2].connection_string(), "100.64.0.3");
    }
}

#[test]
fn gh505_tailscale_large_mullvad_inventory_only_adds_real_machines() {
    let mut peers = serde_json::Map::new();
    // Reproduce the reporter's 536 provider nodes and five real machines.
    for index in 0..536 {
        peers.insert(
            format!("mullvad-{index}"),
            json!({
                "Online": true,
                "Tags": ["tag:mullvad-exit-node"],
                "DNSName": format!("provider-{index}.mullvad.ts.net."),
                "TailscaleIPs": [format!("100.90.{}.{}", index / 250, index % 250 + 1)]
            }),
        );
    }
    for index in 1..=5 {
        peers.insert(
            format!("machine-{index}"),
            json!({
                "Online": true,
                "TailscaleIPs": [format!("100.64.0.{index}")]
            }),
        );
    }
    let bytes = serde_json::to_vec(&json!({"BackendState": "Running", "Peer": peers})).unwrap();
    let mut hosts = Vec::new();
    merge_tailscale_hosts(&mut hosts, &bytes).unwrap();
    assert_eq!(
        hosts
            .iter()
            .map(|host| host.name.as_str())
            .collect::<Vec<_>>(),
        [
            "100.64.0.1",
            "100.64.0.2",
            "100.64.0.3",
            "100.64.0.4",
            "100.64.0.5"
        ]
    );
}
