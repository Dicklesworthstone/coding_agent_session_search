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

#[test]
fn gh505_tailscale_removes_configured_provider_destinations_before_liveness_checks() {
    for flags in [
        json!({"Online": true}),
        json!({"Online": false}),
        json!({"Online": true, "ShareeNode": true}),
    ] {
        let mut tagged = flags.clone();
        tagged["Tags"] = json!(["tag:mullvad-exit-node"]);
        tagged["TailscaleIPs"] = json!(["fd7a:115c:a1e0::1", "100.90.0.1"]);
        let mut dns_only = flags;
        dns_only["DNSName"] = json!("SE-STO-WG-001.MULLVAD.TS.NET.");
        // DNS evidence must work even when no usable IPv4 address is reported.
        dns_only["TailscaleIPs"] = json!(null);
        let bytes = serde_json::to_vec(&json!({
            "BackendState": "Running",
            "Peer": {"tagged": tagged, "dns-only": dns_only}
        }))
        .unwrap();
        let mut hosts = parse_ssh_config(
            "Host by-ip\n HostName 100.90.0.1\n\
             Host by-ipv6\n HostName fd7a:115c:a1e0:0:0:0:0:1\n\
             Host by-dns\n HostName se-sto-wg-001.mullvad.ts.net\n\
             Host 100.90.0.1 SE-STO-WG-001.MULLVAD.TS.NET.\n\
             Host workstation\n HostName 100.64.0.1\n User developer\n Port 2222\n IdentityFile ~/.ssh/work\n",
        );
        for _ in 0..2 {
            merge_tailscale_hosts(&mut hosts, &bytes).unwrap();
            assert_eq!(hosts.len(), 1, "provider aliases survived: {hosts:?}");
            assert_eq!(hosts[0].connection_string(), "developer@workstation");
            assert_eq!(hosts[0].hostname.as_deref(), Some("100.64.0.1"));
            assert_eq!(hosts[0].port, Some(2222));
            assert_eq!(hosts[0].identity_file.as_deref(), Some("~/.ssh/work"));
        }
    }
}

#[test]
fn gh505_tailscale_provider_names_do_not_override_alias_destinations() {
    let mut hosts = parse_ssh_config(
        "Host se-sto-wg-001.mullvad.ts.net\n HostName 192.0.2.1\n User developer\n\
         Host 100.90.0.1\n HostName 192.0.2.2\n Port 2222\n\
         Host workstation\n HostName work.notmullvad.ts.net\n IdentityFile ~/.ssh/work\n\
         Host lookalike\n HostName se-sto-wg-001.mullvad.ts.net.example.ts.net\n",
    );
    let bytes = serde_json::to_vec(&json!({
        "BackendState": "Running",
        "Peer": {
            "provider": {
                "Online": true,
                "Tags": ["tag:mullvad-exit-node"],
                "DNSName": "se-sto-wg-001.mullvad.ts.net.",
                "TailscaleIPs": ["100.90.0.1"],
                "HostName": "workstation"
            }
        }
    }))
    .unwrap();
    merge_tailscale_hosts(&mut hosts, &bytes).unwrap();
    assert_eq!(hosts.len(), 4);
    assert_eq!(hosts[0].hostname.as_deref(), Some("192.0.2.1"));
    assert_eq!(hosts[0].user.as_deref(), Some("developer"));
    assert_eq!(hosts[1].hostname.as_deref(), Some("192.0.2.2"));
    assert_eq!(hosts[1].port, Some(2222));
    assert_eq!(hosts[2].name, "workstation");
    assert_eq!(hosts[2].identity_file.as_deref(), Some("~/.ssh/work"));
    assert_eq!(hosts[3].name, "lookalike");
}

#[test]
fn gh505_tailscale_invalid_status_never_partially_removes_aliases() {
    let mut hosts = parse_ssh_config(
        "Host provider\n HostName 100.90.0.1\n\
         Host workstation\n HostName 100.64.0.1\n",
    );
    for bytes in [
        br#"{"BackendState":"NeedsLogin","Peer":{"a":{"Tags":["tag:mullvad-exit-node"],"TailscaleIPs":["100.90.0.1"]}}}"#.as_slice(),
        br#"{"BackendState":"Running","Peer":{"a":{"Tags":["tag:mullvad-exit-node"],"TailscaleIPs":["100.90.0.1"]},"z":{"TailscaleIPs":["invalid address"]}}}"#,
    ] {
        assert!(merge_tailscale_hosts(&mut hosts, bytes).is_err());
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].name, "provider");
        assert_eq!(hosts[1].name, "workstation");
    }
}

#[test]
fn gh505_tailscale_alias_label_does_not_hide_a_different_destination() {
    let bytes = serde_json::to_vec(&json!({
        "BackendState": "Running",
        "Peer": {
            "real": {
                "Online": true,
                "DNSName": "workstation.example.ts.net.",
                "TailscaleIPs": ["100.64.0.1"]
            }
        }
    }))
    .unwrap();
    for name in ["workstation.example.ts.net", "WORKSTATION.example.ts.net."] {
        let mut hosts = parse_ssh_config(&format!(
            "Host {name}\n HostName 100.64.0.8\n User developer\n Port 2222\n IdentityFile ~/.ssh/work\n"
        ));
        for _ in 0..2 {
            assert_eq!(merge_tailscale_hosts(&mut hosts, &bytes).unwrap(), 0);
            assert_eq!(hosts.len(), 2);
            assert_eq!(hosts[0].name, name);
            assert_eq!(hosts[0].hostname.as_deref(), Some("100.64.0.8"));
            assert_eq!(hosts[0].user.as_deref(), Some("developer"));
            assert_eq!(hosts[0].port, Some(2222));
            assert_eq!(hosts[0].identity_file.as_deref(), Some("~/.ssh/work"));
            assert_eq!(hosts[1].connection_string(), "100.64.0.1");
        }
        // Without a HostName override, the alias really does name the peer.
        let mut hosts = parse_ssh_config(&format!("Host {name}\n User developer\n"));
        assert_eq!(merge_tailscale_hosts(&mut hosts, &bytes).unwrap(), 0);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].connection_string(), format!("developer@{name}"));
    }
}

#[test]
fn gh505_tailscale_ip_targets_do_not_recreate_removed_or_remapped_aliases() {
    let mut hosts = parse_ssh_config(
        "Host 100.64.0.1\n HostName 100.90.0.1\n\
         Host 100.64.0.2\n HostName 100.64.0.8\n User developer\n Port 2222\n",
    );
    let bytes = serde_json::to_vec(&json!({
        "BackendState": "Running",
        "Peer": {
            "a-real": {"Online": true, "TailscaleIPs": ["100.64.0.1"]},
            "b-real": {"Online": true, "TailscaleIPs": ["100.64.0.2"]},
            "z-provider": {
                "Online": false,
                "Tags": ["tag:mullvad-exit-node"],
                "TailscaleIPs": ["100.90.0.1"]
            }
        }
    }))
    .unwrap();
    assert_eq!(merge_tailscale_hosts(&mut hosts, &bytes).unwrap(), 2);
    // The first alias is excluded; adding a raw .1 would invoke that same
    // still-configured alias. The second alias remains usable for .8, but
    // must not also be added as a second (misdirected) candidate for .2.
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].name, "100.64.0.2");
    assert_eq!(hosts[0].hostname.as_deref(), Some("100.64.0.8"));
    assert_eq!(hosts[0].user.as_deref(), Some("developer"));
    assert_eq!(hosts[0].port, Some(2222));
}

#[test]
fn gh505_tailscale_safe_alias_keeps_peer_with_a_conflicting_numeric_name() {
    let mut hosts = parse_ssh_config(
        "Host 100.64.0.1\n HostName 100.90.0.1\n\
         Host workstation\n HostName 100.64.0.1\n User developer\n Port 2222\n IdentityFile ~/.ssh/work\n",
    );
    let bytes = serde_json::to_vec(&json!({
        "BackendState": "Running",
        "Peer": {
            "a-real": {"Online": true, "TailscaleIPs": ["100.64.0.1"]},
            "z-provider": {
                "Online": true,
                "Tags": ["tag:mullvad-exit-node"],
                "TailscaleIPs": ["100.90.0.1"]
            }
        }
    }))
    .unwrap();
    // Prefer the actual destination match before rejecting an ambiguous IP
    // target: this peer is still reachable through its unambiguous alias.
    assert_eq!(merge_tailscale_hosts(&mut hosts, &bytes).unwrap(), 0);
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].connection_string(), "developer@workstation");
    assert_eq!(hosts[0].hostname.as_deref(), Some("100.64.0.1"));
    assert_eq!(hosts[0].port, Some(2222));
    assert_eq!(hosts[0].identity_file.as_deref(), Some("~/.ssh/work"));
}
