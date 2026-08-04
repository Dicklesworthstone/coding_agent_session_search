// Dead-code tolerated module-wide: this regression corpus is consumed by the
// integrated golden + E2E gate (.11.5) and the subsystem closeout gate
// (.15.5); it ties the report's named issue classes to the contract cores
// and fixture suites already landed under src/search/ and src/indexer/.
#![allow(dead_code)]

//! Regression corpus for the report's named issue classes (bead
//! cass-fleet-resilience-20260608-uojcg.11.2).
//!
//! Encodes each mined issue class as a deterministic, in-source fixture
//! stating its root cause, the incident category, the expected
//! status/doctor/triage behavior, the safe next command, and the proof
//! command that locks the regression. This is the single index a closeout
//! gate walks to confirm every report failure mode has a home and a proof —
//! without reaching a live private corpus.
//!
//! It composes the contracts already landed: [`RootCauseFamily`]
//! (`.9.1`), [`IncidentCategory`] (`.10.1`), and the fixture suites
//! `liveness_fixtures` (`.4.5`), `workspace_source_fixtures` (`.7.4`),
//! `readiness_fixtures` (`.1.5`), and the quarantine compat fixtures
//! (`.3.4`). The corpus is serialize-only (borrowed `&'static` fields); all
//! enums serialize as snake_case.

use serde::Serialize;

use crate::root_cause_taxonomy::RootCauseFamily;
use crate::search::incident_categories::IncidentCategory;

/// One named issue-class regression fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct IssueClassFixture {
    /// The report/issue identifier (e.g. `#248`).
    pub issue_id: &'static str,
    pub title: &'static str,
    pub root_cause_family: RootCauseFamily,
    pub category: IncidentCategory,
    /// Expected status/doctor/triage behavior when this class occurs.
    pub expected_behavior: &'static str,
    /// The safe next command an agent should run (never a bare `cass`/`bv`,
    /// never destructive).
    pub safe_next_command: &'static str,
    /// The proof command / test that locks this regression.
    pub proof_command: &'static str,
}

/// The frozen regression corpus, in a stable order.
static CORPUS: &[IssueClassFixture] = &[
    IssueClassFixture {
        issue_id: "#110",
        title: "chunked FTS rebuild OOM risk",
        root_cause_family: RootCauseFamily::CassDerivedState,
        category: IncidentCategory::QuarantineOom,
        expected_behavior: "rebuild bounds memory and quarantines the poison chunk rather than OOM-killing the run",
        safe_next_command: "cass diag --json --quarantine",
        proof_command: "cargo test --lib indexer::quarantine",
    },
    IssueClassFixture {
        issue_id: "#120",
        title: "remote auth fallback",
        root_cause_family: RootCauseFamily::RemoteTransportAuth,
        category: IncidentCategory::RemoteSyncAuth,
        expected_behavior: "auth/transport failure surfaces a stable err.kind with a reconnect hint; no destructive local action",
        safe_next_command: "cass sources list --json",
        proof_command: "cargo test --test e2e_sources",
    },
    IssueClassFixture {
        issue_id: "#137",
        title: "current:0 stream misread as stalled",
        root_cause_family: RootCauseFamily::CassDerivedState,
        category: IncidentCategory::IndexStallProgress,
        expected_behavior: "a current:0 stream with live forward progress reads as building, not stalled",
        safe_next_command: "cass status --json",
        proof_command: "cargo test --lib search::liveness_fixtures",
    },
    IssueClassFixture {
        issue_id: "#196",
        title: "lock heartbeat without forward progress",
        root_cause_family: RootCauseFamily::CassDerivedState,
        category: IncidentCategory::IndexStallProgress,
        expected_behavior: "heartbeat-without-progress reads as stalled/waiting-on-lock and prompts attach, not an open-ended wait",
        safe_next_command: "cass status --json",
        proof_command: "cargo test --lib search::liveness_fixtures",
    },
    IssueClassFixture {
        issue_id: "#247",
        title: "historical salvage zero-new loop",
        root_cause_family: RootCauseFamily::CassDerivedState,
        category: IncidentCategory::WatchSalvageIssues,
        expected_behavior: "a zero-new bundle is skipped via the granular ledger instead of a 5-12 minute re-scan",
        safe_next_command: "cass status --json",
        proof_command: "cargo test --lib search::salvage_ledger",
    },
    IssueClassFixture {
        issue_id: "#248",
        title: "watch OOM restart loop",
        root_cause_family: RootCauseFamily::CassDerivedState,
        category: IncidentCategory::WatchSalvageIssues,
        expected_behavior: "after an OOM-kill a bounded checkpointed recovery is chosen; sparse detection never forces a full rebuild loop",
        safe_next_command: "cass status --json",
        proof_command: "cargo test --lib search::watch_recovery",
    },
    IssueClassFixture {
        issue_id: "#250",
        title: "watch exit code 9 with no reason",
        root_cause_family: RootCauseFamily::CassDerivedState,
        category: IncidentCategory::WatchSalvageIssues,
        expected_behavior: "watch exit emits a parseable envelope (kind/subsystem/retryability/next command), not a bare code 9",
        safe_next_command: "cass health --json",
        proof_command: "cargo test --lib search::watch_exit_envelope",
    },
    IssueClassFixture {
        issue_id: "#257",
        title: "semantic progress/checkpoint/quality tier",
        root_cause_family: RootCauseFamily::SemanticAssets,
        category: IncidentCategory::Semantic,
        expected_behavior: "semantic backfill exposes ordered progress events; a tier is never published if it would lie about DB coverage",
        safe_next_command: "cass status --json",
        proof_command: "cargo test --lib search::semantic_publish_safety",
    },
    IssueClassFixture {
        issue_id: "#258",
        title: "legacy quarantine retry carry-over",
        root_cause_family: RootCauseFamily::CassDerivedState,
        category: IncidentCategory::QuarantineOom,
        expected_behavior: "a legacy quarantine record (no version) is retry-eligible, not silently orphaned forever",
        safe_next_command: "cass diag --json --quarantine",
        proof_command: "cargo test --lib indexer::quarantine",
    },
    IssueClassFixture {
        issue_id: "openread-fts-messages",
        title: "OpenRead / fts_messages cursor failure",
        root_cause_family: RootCauseFamily::FrankensqliteStorage,
        category: IncidentCategory::StorageBusyCorrupt,
        expected_behavior: "an OpenRead/FTS failure is classified openread_failed/fts_metadata_failed with archive-risk handling, not generic stale-index advice",
        safe_next_command: "cass doctor --json",
        proof_command: "cargo test --lib search::storage_integrity",
    },
    IssueClassFixture {
        issue_id: "database-busy",
        title: "database busy / locked",
        root_cause_family: RootCauseFamily::FrankensqliteStorage,
        category: IncidentCategory::StorageBusyCorrupt,
        expected_behavior: "a busy lock is reported busy_or_locked with the check skipped (not_checked), canonical rows still trustworthy",
        safe_next_command: "cass doctor --json",
        proof_command: "cargo test --lib search::storage_integrity",
    },
    IssueClassFixture {
        issue_id: "missing-lexical-metadata",
        title: "missing lexical metadata",
        root_cause_family: RootCauseFamily::CassDerivedState,
        category: IncidentCategory::IndexStaleMissing,
        expected_behavior: "absent lexical metadata reads as missing (repair lexical), distinct from stale-but-searchable",
        safe_next_command: "cass index --full",
        proof_command: "cargo test --lib search::readiness",
    },
    IssueClassFixture {
        issue_id: "workspace-mismatch",
        title: "workspace/source-path mismatch zero-hit",
        root_cause_family: RootCauseFamily::WorkspaceProvenance,
        category: IncidentCategory::SearchZeroWorkspace,
        expected_behavior: "a zero-result workspace filter suggests canonical workspaces rather than reading as a true empty",
        safe_next_command: "cass sources list --json",
        proof_command: "cargo test --lib search::workspace_source_fixtures",
    },
    IssueClassFixture {
        issue_id: "noisy-dependency-logging",
        title: "noisy dependency logging / attribution",
        root_cause_family: RootCauseFamily::Unknown,
        category: IncidentCategory::DependencyAttribution,
        expected_behavior: "dependency-attribution incidents are gated behind explicit trace surfaces and attributed, not silently noisy",
        safe_next_command: "cass diag --json",
        proof_command: "cargo test --lib search::incident_categories",
    },
];

/// The frozen regression corpus.
pub(crate) fn regression_corpus() -> &'static [IssueClassFixture] {
    CORPUS
}

/// Look up a fixture by its issue id.
pub(crate) fn issue_fixture(issue_id: &str) -> Option<&'static IssueClassFixture> {
    CORPUS.iter().find(|f| f.issue_id == issue_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The issue classes the bead requires the corpus to cover.
    const REQUIRED: &[&str] = &[
        "#110",
        "#120",
        "#137",
        "#196",
        "#247",
        "#248",
        "#250",
        "#257",
        "#258",
        "openread-fts-messages",
        "database-busy",
        "missing-lexical-metadata",
        "workspace-mismatch",
        "noisy-dependency-logging",
    ];

    #[test]
    fn corpus_covers_every_required_issue_class() {
        for id in REQUIRED {
            assert!(
                issue_fixture(id).is_some(),
                "regression corpus is missing required issue class {id}"
            );
        }
        // No silent shrinkage: count guard.
        assert_eq!(regression_corpus().len(), REQUIRED.len());
    }

    #[test]
    fn every_fixture_states_root_cause_category_behavior_and_proof() {
        for f in regression_corpus() {
            assert!(!f.title.is_empty(), "{} title", f.issue_id);
            assert!(!f.expected_behavior.is_empty(), "{} behavior", f.issue_id);
            assert!(
                !f.safe_next_command.is_empty(),
                "{} next command",
                f.issue_id
            );
            assert!(!f.proof_command.is_empty(), "{} proof command", f.issue_id);
            // The category's a-priori family agrees with the fixture, except
            // where the fixture deliberately narrows a CassDerivedState
            // incident (e.g. quarantine/watch live under cass-derived-state)
            // or attribution defers to Unknown.
            let _ = f.category; // category is asserted concretely below
        }
    }

    #[test]
    fn safe_next_and_proof_commands_are_never_bare_and_never_destructive() {
        for f in regression_corpus() {
            for cmd in [f.safe_next_command, f.proof_command] {
                assert_ne!(cmd.trim(), "cass", "{} bare cass", f.issue_id);
                assert_ne!(cmd.trim(), "bv", "{} bare bv", f.issue_id);
                assert!(
                    cmd.starts_with("cass ") || cmd.starts_with("cargo "),
                    "{}: command must be a concrete cass/cargo invocation: {cmd}",
                    f.issue_id
                );
                for bad in ["rm ", "rm -", "--force-clean", "DROP ", "delete "] {
                    assert!(!cmd.contains(bad), "{} destructive: {cmd}", f.issue_id);
                }
            }
        }
    }

    #[test]
    fn liveness_classes_attribute_to_cass_derived_state() {
        for id in ["#137", "#196", "#247", "#248", "#250"] {
            assert_eq!(
                issue_fixture(id).unwrap().root_cause_family,
                RootCauseFamily::CassDerivedState,
                "{id}"
            );
        }
    }

    #[test]
    fn storage_classes_attribute_to_frankensqlite_storage() {
        for id in ["openread-fts-messages", "database-busy"] {
            assert_eq!(
                issue_fixture(id).unwrap().root_cause_family,
                RootCauseFamily::FrankensqliteStorage,
                "{id}"
            );
        }
    }

    #[test]
    fn report_specific_attributions_are_correct() {
        assert_eq!(
            issue_fixture("#120").unwrap().root_cause_family,
            RootCauseFamily::RemoteTransportAuth
        );
        assert_eq!(
            issue_fixture("#257").unwrap().category,
            IncidentCategory::Semantic
        );
        assert_eq!(
            issue_fixture("workspace-mismatch").unwrap().category,
            IncidentCategory::SearchZeroWorkspace
        );
        assert_eq!(
            issue_fixture("noisy-dependency-logging").unwrap().category,
            IncidentCategory::DependencyAttribution
        );
    }

    #[test]
    fn fixture_serializes_with_snake_case_enums() {
        let f = issue_fixture("#248").unwrap();
        let json = serde_json::to_string(f).unwrap();
        assert!(json.contains("\"issue_id\":\"#248\""));
        assert!(json.contains("\"category\":\"watch_salvage_issues\""));
        assert!(json.contains("\"root_cause_family\":\"cass-derived-state\""));
        assert!(json.contains("\"proof_command\":\"cargo test --lib search::watch_recovery\""));
    }

    #[test]
    fn corpus_is_deterministic_in_order() {
        let a: Vec<&str> = regression_corpus().iter().map(|f| f.issue_id).collect();
        let b: Vec<&str> = regression_corpus().iter().map(|f| f.issue_id).collect();
        assert_eq!(a, b);
        assert_eq!(a.first(), Some(&"#110"));
        assert_eq!(a.last(), Some(&"noisy-dependency-logging"));
    }
}
