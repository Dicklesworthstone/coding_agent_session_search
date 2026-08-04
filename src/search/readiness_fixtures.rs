// Dead-code tolerated module-wide: the fleet-host fixture matrix lands
// ahead of the downstream consumers (.12.5 report-derived E2E scenario
// scripts and .11.2 regression corpus) that will load these fixtures to
// prove behavior without reaching live machines.
#![allow(dead_code)]

//! Deterministic readiness fixture matrix for the named host states from
//! the 2026-06-08 CASS fleet/session analysis (bead
//! cass-fleet-resilience-20260608-uojcg.1.5).
//!
//! The truth-table values live once in
//! [`crate::search::readiness::fleet_fixtures`]; this module wraps each
//! canonical [`DerivedAssetTruthTable`] with the *redacted* host identity
//! and the running `cass` version reported for that node, so future work
//! can replay each named fleet state — `local`, `ts1`, `ts2`, `css`,
//! `csd`, `mac-mini-max`, `mac-mini-old` — without touching live machines
//! or leaking real paths / hostnames.
//!
//! Determinism / redaction / size invariants:
//! - Every fixture is built from in-source literals (no clock, no env, no
//!   filesystem), so the loader is byte-deterministic across runs.
//! - Host names are logical labels (`local`, `ts1`, …), never real
//!   hostnames; no absolute paths or user data appear in any field.
//! - The set is intentionally small (seven rows) and serializes to compact
//!   JSON via serde for cross-crate (E2E / golden) consumers.

use serde::{Deserialize, Serialize};

use crate::search::readiness::{DerivedAssetTruthTable, fleet_fixtures};

/// One named fleet host from the report, pairing the canonical derived-asset
/// truth table with the redacted host identity and reported `cass` version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FleetHostFixture {
    /// Redacted logical host label (e.g. `local`, `ts1`, `mac-mini-old`).
    pub host: String,
    /// The `cass` version running on that node per the report. Empty when
    /// the node was unreachable and the version could not be observed.
    pub cass_version: String,
    /// Short redacted note describing the operational situation the fixture
    /// reproduces. No paths / hostnames / user data.
    pub notes: String,
    /// The canonical derived-asset truth table for the host.
    pub table: DerivedAssetTruthTable,
}

/// Static host metadata keyed by the canonical fixture name from
/// [`fleet_fixtures`]. Kept in the report's narrative order.
struct HostMeta {
    fixture_name: &'static str,
    host: &'static str,
    cass_version: &'static str,
    notes: &'static str,
}

const HOSTS: &[HostMeta] = &[
    HostMeta {
        fixture_name: "local_stale_quarantine",
        host: "local",
        cass_version: "0.6.13",
        notes: "stale lexical index with 133 ingest-OOM quarantines; last query fell back to lexical-only refinement",
    },
    HostMeta {
        fixture_name: "ts1_high_archive_risk",
        host: "ts1",
        cass_version: "0.6.13",
        notes: "stale (last_scan_ts newer than last_indexed_at) with high archive risk; a full rebuild would be data-loss risky",
    },
    HostMeta {
        fixture_name: "ts2_fast_health_slow_status",
        host: "ts2",
        cass_version: "0.6.10",
        notes: "health fast/cached while the heavier status/doctor probe is slow during a semantic backfill",
    },
    HostMeta {
        fixture_name: "csd_missing_lexical_metadata",
        host: "csd",
        cass_version: "0.4.1",
        notes: "lexical metadata absent with an older doctor gap; index never built or lost",
    },
    HostMeta {
        fixture_name: "css_stale_existing_index",
        host: "css",
        cass_version: "0.6.13",
        notes: "stale existing index against a dependency-noisy corpus; search still correct for indexed content",
    },
    HostMeta {
        fixture_name: "mac_mini_max_stale_old_binary",
        host: "mac-mini-max",
        cass_version: "0.4.1",
        notes: "stale macOS data dir and workspace mismatch on an old binary; upgrade before trusting/rebuilding assets",
    },
    HostMeta {
        fixture_name: "mac_mini_old_unreachable",
        host: "mac-mini-old",
        cass_version: "",
        notes: "unreachable via the fleet probe; no local fields are trustworthy",
    },
];

/// Load the deterministic fleet-host fixture matrix in the report's stable
/// order. Each entry wraps the canonical [`DerivedAssetTruthTable`] from
/// [`fleet_fixtures`] with the redacted host identity and reported version.
pub(crate) fn fleet_host_fixtures() -> Vec<FleetHostFixture> {
    // `fleet_fixtures()` and `HOSTS` are maintained in the same stable
    // report order, so pair them positionally (no fallible name lookup).
    // `debug_assert` catches order/name drift in debug + test builds; the
    // `fixtures_and_host_metadata_are_aligned` test is the hard gate.
    let canonical = fleet_fixtures();
    debug_assert_eq!(
        canonical.len(),
        HOSTS.len(),
        "fleet fixture count and host metadata count diverged"
    );
    canonical
        .into_iter()
        .zip(HOSTS.iter())
        .map(|((name, table), m)| {
            debug_assert_eq!(name, m.fixture_name, "fleet fixture order/name drift");
            FleetHostFixture {
                host: m.host.to_string(),
                cass_version: m.cass_version.to_string(),
                notes: m.notes.to_string(),
                table,
            }
        })
        .collect()
}

/// Look up a single fleet-host fixture by its redacted host label.
pub(crate) fn fleet_host_fixture(host: &str) -> Option<FleetHostFixture> {
    fleet_host_fixtures().into_iter().find(|f| f.host == host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::readiness::{
        ArchiveRiskLevel, BinaryCompatibility, LexicalReadinessState, MaintenanceActivity,
        SafeNextAction, SemanticReadinessState,
    };

    /// The intended readiness/action/safety mapping for each named host,
    /// asserted against the loaded fixture so the matrix is self-checking.
    struct Expect {
        host: &'static str,
        lexical: LexicalReadinessState,
        semantic: SemanticReadinessState,
        action: SafeNextAction,
        searchable: bool,
        // Whether the recommended next command would mutate derived assets
        // or the archive (the "safety envelope"). High-risk / unreachable /
        // broken-DB states must recommend a non-mutating action.
        action_mutating: bool,
    }

    const EXPECTATIONS: &[Expect] = &[
        Expect {
            host: "local",
            lexical: LexicalReadinessState::StaleButSearchable,
            semantic: SemanticReadinessState::HybridReady,
            action: SafeNextAction::RefreshLexical,
            searchable: true,
            action_mutating: true,
        },
        Expect {
            host: "ts1",
            lexical: LexicalReadinessState::Ready,
            semantic: SemanticReadinessState::HybridReady,
            // High archive risk forces a backup-first, non-mutating action.
            action: SafeNextAction::BackupThenRepair,
            searchable: true,
            action_mutating: false,
        },
        Expect {
            host: "ts2",
            lexical: LexicalReadinessState::Ready,
            semantic: SemanticReadinessState::Backfilling,
            action: SafeNextAction::WaitForSemantic,
            searchable: true,
            action_mutating: false,
        },
        Expect {
            host: "csd",
            lexical: LexicalReadinessState::Missing,
            semantic: SemanticReadinessState::Absent,
            action: SafeNextAction::RepairLexical,
            searchable: false,
            action_mutating: true,
        },
        Expect {
            host: "css",
            lexical: LexicalReadinessState::StaleButSearchable,
            semantic: SemanticReadinessState::HybridReady,
            action: SafeNextAction::RefreshLexical,
            searchable: true,
            action_mutating: true,
        },
        Expect {
            host: "mac-mini-max",
            lexical: LexicalReadinessState::StaleButSearchable,
            semantic: SemanticReadinessState::FastTierReady,
            // Binary skew is surfaced ahead of the stale-lexical refresh;
            // upgrading the binary is non-mutating w.r.t. derived assets.
            action: SafeNextAction::UpgradeBinary,
            searchable: true,
            action_mutating: false,
        },
        Expect {
            host: "mac-mini-old",
            lexical: LexicalReadinessState::Missing,
            semantic: SemanticReadinessState::Absent,
            action: SafeNextAction::HostUnreachable,
            searchable: false,
            action_mutating: false,
        },
    ];

    fn fixture_for(host: &str) -> FleetHostFixture {
        fleet_host_fixture(host).unwrap_or_else(|| panic!("missing fleet host fixture {host}"))
    }

    #[test]
    fn matrix_covers_all_seven_named_hosts_in_report_order() {
        let hosts: Vec<String> = fleet_host_fixtures().into_iter().map(|f| f.host).collect();
        assert_eq!(
            hosts,
            vec![
                "local",
                "ts1",
                "ts2",
                "csd",
                "css",
                "mac-mini-max",
                "mac-mini-old",
            ]
        );
    }

    #[test]
    fn fixtures_and_host_metadata_are_aligned() {
        // Hard gate behind the loader's debug_assert: the canonical truth
        // tables and the host metadata must stay 1:1 and in the same order.
        let canonical = crate::search::readiness::fleet_fixtures();
        assert_eq!(
            canonical.len(),
            super::HOSTS.len(),
            "fixture count vs host metadata count"
        );
        for ((name, _), m) in canonical.iter().zip(super::HOSTS.iter()) {
            assert_eq!(*name, m.fixture_name, "fixture order/name alignment");
        }
    }

    #[test]
    fn every_expectation_has_a_fixture_and_vice_versa() {
        let expected: std::collections::BTreeSet<&str> =
            EXPECTATIONS.iter().map(|e| e.host).collect();
        let actual: std::collections::BTreeSet<String> =
            fleet_host_fixtures().into_iter().map(|f| f.host).collect();
        let actual_refs: std::collections::BTreeSet<&str> =
            actual.iter().map(String::as_str).collect();
        assert_eq!(expected, actual_refs);
    }

    #[test]
    fn each_fixture_maps_to_intended_readiness_action_and_safety_envelope() {
        for e in EXPECTATIONS {
            let f = fixture_for(e.host);
            assert_eq!(
                f.table.readiness.lexical, e.lexical,
                "{} lexical readiness",
                e.host
            );
            assert_eq!(
                f.table.readiness.semantic, e.semantic,
                "{} semantic readiness",
                e.host
            );
            let cmd = f.table.safe_next_command();
            assert_eq!(cmd.action, e.action, "{} safe-next action", e.host);
            assert_eq!(
                cmd.action.is_mutating(),
                e.action_mutating,
                "{} safety envelope (action mutating?)",
                e.host
            );
            assert_eq!(
                f.table.is_searchable(),
                e.searchable,
                "{} searchability",
                e.host
            );
        }
    }

    #[test]
    fn high_archive_risk_host_never_recommends_a_mutating_repair() {
        // ts1 is the report's high-archive-risk node: it must not emit
        // casual rebuild/repair advice.
        let ts1 = fixture_for("ts1");
        assert_eq!(ts1.table.archive_risk, ArchiveRiskLevel::High);
        let cmd = ts1.table.safe_next_command();
        assert_eq!(cmd.action, SafeNextAction::BackupThenRepair);
        assert!(
            !cmd.action.is_mutating(),
            "high archive risk must yield a non-mutating, backup-first action"
        );
    }

    #[test]
    fn low_risk_stale_hosts_still_get_normal_refresh_guidance() {
        for host in ["local", "css"] {
            let f = fixture_for(host);
            assert!(matches!(f.table.archive_risk, ArchiveRiskLevel::Low));
            assert_eq!(
                f.table.safe_next_command().action,
                SafeNextAction::RefreshLexical,
                "{host} low-risk stale should refresh normally"
            );
        }
    }

    #[test]
    fn local_fixture_reflects_133_ingest_oom_quarantines() {
        let local = fixture_for("local");
        assert_eq!(local.cass_version, "0.6.13");
        assert_eq!(local.table.quarantine.quarantined_count, 133);
        assert_eq!(
            local.table.quarantine.causes,
            vec!["ingest_oom".to_string()]
        );
        assert!(local.table.quarantine.has_exclusions());
    }

    #[test]
    fn unreachable_host_has_empty_version_and_unknown_axes() {
        let old = fixture_for("mac-mini-old");
        assert!(old.cass_version.is_empty());
        assert_eq!(old.table.maintenance, MaintenanceActivity::Unknown);
        assert_eq!(old.table.binary, BinaryCompatibility::Unknown);
        assert!(!old.table.is_searchable());
    }

    #[test]
    fn fixtures_are_redacted_no_absolute_paths_or_real_hosts() {
        // Cheap redaction guard: serialized fixtures must not embed
        // filesystem paths or obvious real-host markers.
        let json = serde_json::to_string(&fleet_host_fixtures()).unwrap();
        assert!(!json.contains("/Users/"), "no macOS home paths");
        assert!(!json.contains("/home/"), "no linux home paths");
        assert!(!json.contains(".local/share"), "no XDG data paths");
    }

    #[test]
    fn matrix_round_trips_through_json_deterministically() {
        let matrix = fleet_host_fixtures();
        let json = serde_json::to_string(&matrix).unwrap();
        let parsed: Vec<FleetHostFixture> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, matrix);
        // Determinism: re-serializing the parsed copy yields identical bytes.
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }
}
