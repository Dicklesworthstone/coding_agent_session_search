// Dead-code tolerated module-wide: these E2E scenario definitions are
// consumed by the bounded runner (.12.2 / e2e_runner) and the CI/local proof
// recipe (.12.6); the integrated gate (.11.5) walks them.
#![allow(dead_code)]

//! Report-derived E2E scenario scripts for fleet and archive states (bead
//! cass-fleet-resilience-20260608-uojcg.12.5).
//!
//! Deterministic scenario *definitions* that simulate the 2026-06-08
//! report's named fleet states. Each scenario states its fixture setup, the
//! `cass` command sequence, the expected JSON assertions, the expected
//! structured-log artifacts, a privacy note, and the owning implementation
//! bead a failure points to. Live-host execution is opt-in
//! (`requires_live_host`) and never required for default CI — the default is
//! to replay against the deterministic fixtures.
//!
//! Machine-consumable and serialize-only; composes the landed contracts:
//! the readiness fixtures (`.1.5`), liveness fixtures (`.4.5`),
//! workspace/source fixtures (`.7.4`), quarantine compat fixtures (`.3.4`),
//! the recovery journeys (`.13.1`), and the proof-log schema (`.12.3`). The
//! `.12.2` runner executes them; this module is the deterministic spec.
//! Commands are concrete `cass` invocations (never bare/destructive).

use serde::Serialize;

/// One report-derived E2E scenario definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct E2eScenario {
    pub id: &'static str,
    pub description: &'static str,
    /// The implementation bead a failure of this scenario points to.
    pub owning_bead: &'static str,
    /// How the deterministic fixture state is established.
    pub fixture_setup: &'static str,
    /// The `cass` command sequence to run, in order.
    pub command_sequence: &'static [&'static str],
    /// Expected JSON assertions (field/value expectations on robot output).
    pub expected_assertions: &'static [&'static str],
    /// Expected structured-log artifacts (per the `.12.3` proof-log schema).
    pub expected_log_artifacts: &'static [&'static str],
    /// Privacy note: what is/ isn't surfaced and how it is redacted.
    pub privacy_note: &'static str,
    /// Opt-in live-host replay. False = deterministic fixture mode (CI).
    pub requires_live_host: bool,
}

static SCENARIOS: &[E2eScenario] = &[
    E2eScenario {
        id: "local_healthy_lexical_semantic_unavailable",
        description: "healthy lexical index with semantic unavailable; search succeeds with truthful lexical fallback metadata",
        owning_bead: "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.5.1",
        fixture_setup: "readiness::fleet_fixtures css_stale_existing_index with semantic absent",
        command_sequence: &["cass search \"query\" --json --robot-meta"],
        expected_assertions: &[
            "_meta.search_mode reflects lexical fallback",
            "_meta.fallback_reason is present and truthful",
            "hits are returned (lexical works)",
        ],
        expected_log_artifacts: &["proof_log scenario=local_healthy_lexical_semantic_unavailable"],
        privacy_note: "result snippets follow existing redaction; no raw model/vector paths surfaced",
        requires_live_host: false,
    },
    E2eScenario {
        id: "local_stale_quarantine",
        description: "stale derived assets plus ingest-OOM quarantine; health/status/doctor explain rebuild+quarantine without data-loss advice",
        owning_bead: "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.3.4",
        fixture_setup: "readiness local_stale_quarantine + indexer/fixtures/quarantine/*.json",
        command_sequence: &["cass status --json", "cass diag --json --quarantine"],
        expected_assertions: &[
            "recommended_action=refresh_lexical_soon (not a destructive rebuild)",
            "quarantine.quarantined_count>0 surfaced as advisory",
            "no data-loss / blind-rebuild advice",
        ],
        expected_log_artifacts: &["proof_log scenario=local_stale_quarantine"],
        privacy_note: "quarantine entries reported by count/cause; no raw conversation text",
        requires_live_host: false,
    },
    E2eScenario {
        id: "ts1_high_archive_risk_backup_first",
        description: "high archive-risk host; status/doctor require backup/inspection before any repair",
        owning_bead: "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.1.3",
        fixture_setup: "readiness::fleet_fixtures ts1_high_archive_risk",
        command_sequence: &["cass doctor --json"],
        expected_assertions: &[
            "archive_risk=high",
            "recommended_action=backup_then_repair",
            "rebuild/repair commands marked unsafe_until backup",
        ],
        expected_log_artifacts: &["proof_log scenario=ts1_high_archive_risk_backup_first"],
        privacy_note: "reports risk level + data_dir only; no archive contents",
        requires_live_host: false,
    },
    E2eScenario {
        id: "csd_missing_lexical_metadata",
        description: "missing lexical metadata; status reports missing (repair lexical), distinct from stale",
        owning_bead: "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.1.1",
        fixture_setup: "readiness::fleet_fixtures csd_missing_lexical_metadata",
        command_sequence: &["cass status --json"],
        expected_assertions: &[
            "lexical=missing",
            "recommended_action=repair_lexical/index_full",
            "is_searchable=false",
        ],
        expected_log_artifacts: &["proof_log scenario=csd_missing_lexical_metadata"],
        privacy_note: "operational state only",
        requires_live_host: false,
    },
    E2eScenario {
        id: "mac_mini_max_stale_old_binary",
        description: "stale assets on an old binary; upgrade is recommended before trusting/rebuilding assets",
        owning_bead: "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.6.6",
        fixture_setup: "readiness::fleet_fixtures mac_mini_max_stale_old_binary",
        command_sequence: &["cass status --json"],
        expected_assertions: &[
            "binary=outdated",
            "recommended_action=upgrade_binary (ahead of stale-lexical refresh)",
        ],
        expected_log_artifacts: &["proof_log scenario=mac_mini_max_stale_old_binary"],
        privacy_note: "version/operational state only",
        requires_live_host: false,
    },
    E2eScenario {
        id: "mac_mini_old_unreachable_live",
        description: "unreachable fleet host (live replay only): host_unreachable, nothing local to do",
        owning_bead: "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.6.1",
        fixture_setup: "readiness::fleet_fixtures mac_mini_old_unreachable (deterministic) or a real offline host (live)",
        command_sequence: &["cass status --json"],
        expected_assertions: &["db=unreachable", "recommended_action=host_unreachable"],
        expected_log_artifacts: &["proof_log scenario=mac_mini_old_unreachable_live"],
        privacy_note: "no host credentials or paths surfaced; reachability only",
        // The only opt-in live scenario; deterministic fixture covers CI.
        requires_live_host: true,
    },
];

/// All report-derived E2E scenarios, in a stable order.
pub(crate) fn e2e_scenarios() -> &'static [E2eScenario] {
    SCENARIOS
}

/// The scenarios that run in default CI (deterministic; no live host).
pub(crate) fn ci_scenarios() -> Vec<&'static E2eScenario> {
    SCENARIOS.iter().filter(|s| !s.requires_live_host).collect()
}

/// Look up a scenario by id.
pub(crate) fn scenario(id: &str) -> Option<&'static E2eScenario> {
    SCENARIOS.iter().find(|s| s.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_report_scenarios_are_present() {
        for id in [
            "local_healthy_lexical_semantic_unavailable",
            "local_stale_quarantine",
            "ts1_high_archive_risk_backup_first",
        ] {
            assert!(scenario(id).is_some(), "missing required scenario {id}");
        }
    }

    #[test]
    fn every_scenario_is_fully_specified() {
        for s in e2e_scenarios() {
            assert!(!s.description.is_empty(), "{} description", s.id);
            assert!(!s.fixture_setup.is_empty(), "{} fixture_setup", s.id);
            assert!(!s.command_sequence.is_empty(), "{} commands", s.id);
            assert!(!s.expected_assertions.is_empty(), "{} assertions", s.id);
            assert!(
                !s.expected_log_artifacts.is_empty(),
                "{} log artifacts",
                s.id
            );
            assert!(!s.privacy_note.is_empty(), "{} privacy note", s.id);
            // Failures must point at an owning implementation bead.
            assert!(
                s.owning_bead.contains("uojcg."),
                "{} owning_bead must reference a bead: {}",
                s.id,
                s.owning_bead
            );
        }
    }

    #[test]
    fn commands_are_concrete_cass_and_never_destructive() {
        for s in e2e_scenarios() {
            for cmd in s.command_sequence {
                assert!(
                    cmd.starts_with("cass "),
                    "{}: command must be a concrete cass invocation: {cmd}",
                    s.id
                );
                for bad in ["rm ", "rm -", "--force-clean", "DROP ", "delete "] {
                    assert!(!cmd.contains(bad), "{} destructive command: {cmd}", s.id);
                }
            }
        }
    }

    #[test]
    fn default_ci_requires_no_live_host() {
        // The .12.5 acceptance: live-host execution is opt-in, never required
        // for default CI.
        let ci = ci_scenarios();
        assert!(!ci.is_empty(), "there must be deterministic CI scenarios");
        assert!(ci.iter().all(|s| !s.requires_live_host));
        // The named fleet/archive states are all CI-runnable without a host.
        for id in [
            "local_healthy_lexical_semantic_unavailable",
            "local_stale_quarantine",
            "ts1_high_archive_risk_backup_first",
            "csd_missing_lexical_metadata",
            "mac_mini_max_stale_old_binary",
        ] {
            assert!(
                !scenario(id).unwrap().requires_live_host,
                "{id} must be CI-runnable"
            );
        }
    }

    #[test]
    fn only_the_unreachable_host_scenario_is_live_opt_in() {
        let live: Vec<&str> = e2e_scenarios()
            .iter()
            .filter(|s| s.requires_live_host)
            .map(|s| s.id)
            .collect();
        assert_eq!(live, vec!["mac_mini_old_unreachable_live"]);
    }

    #[test]
    fn scenario_serializes_with_expected_fields() {
        let s = scenario("ts1_high_archive_risk_backup_first").unwrap();
        let json = serde_json::to_string(s).unwrap();
        assert!(json.contains("\"id\":\"ts1_high_archive_risk_backup_first\""));
        assert!(json.contains("\"requires_live_host\":false"));
        assert!(json.contains("\"owning_bead\""));
        assert!(json.contains("\"expected_log_artifacts\""));
    }

    #[test]
    fn scenarios_are_deterministic_in_order() {
        let a: Vec<&str> = e2e_scenarios().iter().map(|s| s.id).collect();
        assert_eq!(
            a.first(),
            Some(&"local_healthy_lexical_semantic_unavailable")
        );
        assert_eq!(a, e2e_scenarios().iter().map(|s| s.id).collect::<Vec<_>>());
    }
}
