// Dead-code tolerated module-wide: these recovery-journey definitions are
// inputs to the human/robot parity work (.13.2), the redacted evidence
// bundle UX (.13.3), the robot-docs update (.11.3), and the integrated
// resilience gate (.11.5).
#![allow(dead_code)]

//! End-to-end user recovery journeys (bead
//! cass-fleet-resilience-20260608-uojcg.13.1).
//!
//! Binds the resilience graph to actual user outcomes: each journey names
//! the user question, the expected robot state, the human-facing summary,
//! the safe next commands, the unsafe commands, the required proof, the
//! fixture provenance, and the structured-log artifact. The journeys become
//! inputs to docs (`.11.3`), E2E scripts (`.12.5`), and the integrated gate
//! (`.11.5`).
//!
//! Machine-consumable and deterministic (in-source, serialize-only), it
//! composes the landed contracts: readiness / archive-risk (`.1.*`), the
//! storage taxonomy (`.14.1`), the readiness fixtures (`.1.5`), the
//! quarantine compat fixtures (`.3.4`), and the proof-log schema (`.12.3`).
//! All commands are concrete `cass` invocations (never a bare `cass`/`bv`,
//! never destructive); the single unsafe-command list per journey is exactly
//! the backup-first gate from the archive-risk envelope.

use serde::Serialize;

/// One end-to-end recovery journey with its acceptance fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RecoveryJourney {
    pub id: &'static str,
    /// The question the user/agent is actually asking.
    pub user_question: &'static str,
    /// The expected robot state label (matches the readiness/storage
    /// vocabulary an agent reads from `--robot`).
    pub expected_robot_state: &'static str,
    /// The bounded human-facing one-line summary.
    pub human_summary: &'static str,
    /// Safe next commands, in order of preference. Concrete `cass` commands.
    pub safe_next_commands: &'static [&'static str],
    /// Commands that are unsafe in this state (e.g. a rebuild before a
    /// backup under high archive risk). Empty when nothing is gated.
    pub unsafe_commands: &'static [&'static str],
    /// The required proof (unit/integration/E2E test) for the journey.
    pub required_proof: &'static str,
    /// Where the deterministic fixture for this journey comes from.
    pub fixture_provenance: &'static str,
    /// The structured-log artifact (per the .12.3 proof-log schema).
    pub log_artifact: &'static str,
}

static JOURNEYS: &[RecoveryJourney] = &[
    RecoveryJourney {
        id: "first_run_no_index",
        user_question: "I just installed cass — why does search return nothing?",
        expected_robot_state: "db=missing, lexical=missing, recommended_action=index_full",
        human_summary: "No index yet. Build the initial index; nothing to lose on a fresh install.",
        safe_next_commands: &["cass index --full"],
        unsafe_commands: &[],
        required_proof: "cargo test --lib search::readiness (fresh_install_recommends_index_full)",
        fixture_provenance: "readiness::fleet_fixtures csd_missing_lexical_metadata",
        log_artifact: "proof_log scenario=first_run_no_index phase=verify",
    },
    RecoveryJourney {
        id: "stale_searchable_semantic_unavailable",
        user_question: "Search works but feels old and semantic is off — is it broken?",
        expected_robot_state: "lexical=stale_but_searchable, semantic=absent, action=refresh_lexical_soon",
        human_summary: "Search is correct for indexed content; refresh to pick up recent sessions. Semantic is opt-in (install a model for hybrid).",
        safe_next_commands: &["cass index", "cass models install"],
        unsafe_commands: &[],
        required_proof: "cargo test --lib search::readiness (stale + semantic reason) + search::semantic_readiness",
        fixture_provenance: "readiness::fleet_fixtures css_stale_existing_index",
        log_artifact: "proof_log scenario=stale_searchable_semantic_unavailable phase=verify",
    },
    RecoveryJourney {
        id: "high_archive_risk_backup_first",
        user_question: "Search looks off on a host where the archive is the only copy — should I rebuild?",
        expected_robot_state: "archive_risk=high, recommended_action=backup_then_repair",
        human_summary: "High archive risk: back up the canonical archive (or produce a fingerprinted plan) BEFORE any rebuild or repair.",
        safe_next_commands: &["cass doctor --json", "cass health --json"],
        // The backup-first gate: these mutating repairs are unsafe until a backup exists.
        unsafe_commands: &["cass index --full", "cass index"],
        required_proof: "cargo test --lib search::readiness (ts1_high_archive_risk_is_backup_first + archive_safety_envelope)",
        fixture_provenance: "readiness::fleet_fixtures ts1_high_archive_risk",
        log_artifact: "proof_log scenario=high_archive_risk_backup_first phase=verify",
    },
    RecoveryJourney {
        id: "quarantine_excluded_incomplete_results",
        user_question: "Are my search results complete, or is something being excluded?",
        expected_robot_state: "lexical=ready, quarantine.quarantined_count>0 (advisory)",
        human_summary: "Results are correct but a few conversations are quarantined and excluded; inspect them (search itself is unaffected).",
        safe_next_commands: &["cass diag --json --quarantine"],
        unsafe_commands: &[],
        required_proof: "cargo test --lib indexer::quarantine + search::readiness (healthy_node_with_only_quarantine)",
        fixture_provenance: "indexer/fixtures/quarantine/*.json + readiness local_stale_quarantine",
        log_artifact: "proof_log scenario=quarantine_excluded_incomplete_results phase=verify",
    },
    RecoveryJourney {
        id: "unreachable_fleet_host",
        user_question: "A fleet host won't respond — what do I do locally?",
        expected_robot_state: "db=unreachable, recommended_action=host_unreachable",
        human_summary: "Host unreachable; nothing local is trustworthy. Retry from a reachable node before acting.",
        safe_next_commands: &["cass status --json"],
        unsafe_commands: &[],
        required_proof: "cargo test --lib search::readiness (mac_mini_old_unreachable_yields_host_unreachable)",
        fixture_provenance: "readiness::fleet_fixtures mac_mini_old_unreachable",
        log_artifact: "proof_log scenario=unreachable_fleet_host phase=verify",
    },
    RecoveryJourney {
        id: "watch_crash_recovery",
        user_question: "cass index --watch keeps exiting — is it looping on a rebuild?",
        expected_robot_state: "watch_exit kind=storage_close_failure/timeout; recovery=bounded (not full rebuild)",
        human_summary: "Watch emitted a parseable exit envelope; recovery is bounded (resume/repair), not a full-rebuild loop. Check health, then re-run watch.",
        safe_next_commands: &["cass health --json", "cass index --watch"],
        unsafe_commands: &[],
        required_proof: "cargo test --lib search::watch_exit_envelope + search::watch_recovery + search::liveness_fixtures",
        fixture_provenance: "search::liveness_fixtures watch_recovery_fixtures",
        log_artifact: "proof_log scenario=watch_crash_recovery phase=verify",
    },
];

/// The end-to-end recovery journeys, in a stable order.
pub(crate) fn recovery_journeys() -> &'static [RecoveryJourney] {
    JOURNEYS
}

/// Look up a journey by id.
pub(crate) fn journey(id: &str) -> Option<&'static RecoveryJourney> {
    JOURNEYS.iter().find(|j| j.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The journeys the bead explicitly requires.
    const REQUIRED: &[&str] = &[
        "first_run_no_index",
        "stale_searchable_semantic_unavailable",
        "high_archive_risk_backup_first",
        "quarantine_excluded_incomplete_results",
    ];

    #[test]
    fn required_journeys_are_all_present() {
        for id in REQUIRED {
            assert!(journey(id).is_some(), "missing required journey {id}");
        }
    }

    #[test]
    fn every_journey_names_all_acceptance_fields() {
        for j in recovery_journeys() {
            assert!(!j.user_question.is_empty(), "{} user_question", j.id);
            assert!(!j.expected_robot_state.is_empty(), "{} robot_state", j.id);
            assert!(!j.human_summary.is_empty(), "{} human_summary", j.id);
            assert!(!j.safe_next_commands.is_empty(), "{} safe commands", j.id);
            assert!(!j.required_proof.is_empty(), "{} proof", j.id);
            assert!(!j.fixture_provenance.is_empty(), "{} provenance", j.id);
            assert!(!j.log_artifact.is_empty(), "{} log artifact", j.id);
        }
    }

    #[test]
    fn safe_commands_are_concrete_cass_and_never_destructive() {
        for j in recovery_journeys() {
            for cmd in j.safe_next_commands {
                assert!(
                    cmd.starts_with("cass "),
                    "{}: safe command must be a concrete cass invocation: {cmd}",
                    j.id
                );
                for bad in ["rm ", "rm -", "--force-clean", "DROP ", "delete "] {
                    assert!(
                        !cmd.contains(bad),
                        "{} destructive safe command: {cmd}",
                        j.id
                    );
                }
            }
        }
    }

    #[test]
    fn only_high_archive_risk_journey_gates_unsafe_commands() {
        // The backup-first gate is the report's central archive-safety rule:
        // exactly the high-archive-risk journey lists unsafe (rebuild)
        // commands; the others gate nothing.
        let high = journey("high_archive_risk_backup_first").unwrap();
        assert!(!high.unsafe_commands.is_empty());
        assert!(
            high.unsafe_commands
                .iter()
                .any(|c| c.contains("index --full"))
        );
        // Its safe commands are inspection-only (no rebuild).
        assert!(high.safe_next_commands.iter().all(|c| !c.contains("index")));

        for j in recovery_journeys() {
            if j.id != "high_archive_risk_backup_first" {
                assert!(
                    j.unsafe_commands.is_empty(),
                    "{} should not gate unsafe commands",
                    j.id
                );
            }
        }
    }

    #[test]
    fn first_run_never_marks_index_unsafe() {
        // On a fresh install there is nothing to lose, so index --full is
        // safe (and the recommended action), never gated.
        let fr = journey("first_run_no_index").unwrap();
        assert!(fr.safe_next_commands.contains(&"cass index --full"));
        assert!(fr.unsafe_commands.is_empty());
    }

    #[test]
    fn journey_serializes_with_expected_fields() {
        let j = journey("high_archive_risk_backup_first").unwrap();
        let json = serde_json::to_string(j).unwrap();
        assert!(json.contains("\"id\":\"high_archive_risk_backup_first\""));
        assert!(json.contains("\"unsafe_commands\":[\"cass index --full\""));
        assert!(json.contains("\"required_proof\""));
        assert!(json.contains("\"log_artifact\""));
    }

    #[test]
    fn journeys_are_deterministic_in_order() {
        let a: Vec<&str> = recovery_journeys().iter().map(|j| j.id).collect();
        assert_eq!(a.first(), Some(&"first_run_no_index"));
        assert_eq!(
            a,
            recovery_journeys().iter().map(|j| j.id).collect::<Vec<_>>()
        );
    }
}
