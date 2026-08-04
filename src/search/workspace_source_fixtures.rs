// Dead-code tolerated module-wide: this workspace/source-mismatch fixture
// suite is consumed by downstream beads .12.5 (report-derived E2E scenario
// scripts) and the view/pack drill-down work (.7.3).
#![allow(dead_code)]

//! Frozen workspace / source-path mismatch fixtures (bead
//! cass-fleet-resilience-20260608-uojcg.7.4).
//!
//! The 2026-06-08 report's workspace-mismatch and stale-source cases came
//! from private corpora that cannot be checked in. This module freezes the
//! named scenarios as deterministic, *redacted* fixtures (synthetic users
//! and project names; no real paths) so search/view changes can be verified
//! without a live corpus.
//!
//! Each [`WorkspaceSourceScenario`] pairs a zero-result workspace filter
//! (consumed by [`crate::search::zero_result_diagnosis`]) with source
//! provenance signals (consumed by [`crate::search::source_provenance`]).
//! The tests prove the canonical workspace suggestion and the
//! `source_exists`/`archive_only` provenance for each, and that both project
//! deterministically. The view-fallback and pack-citation assertions belong
//! to the consuming surfaces (.7.3 / .12.5); this suite freezes the inputs
//! and the diagnosis/provenance contract they build on.

use crate::search::source_provenance::ProvenanceSignals;

/// One frozen workspace/source-mismatch scenario from the report.
pub(crate) struct WorkspaceSourceScenario {
    pub name: &'static str,
    /// The `--workspace` filter the agent used.
    pub requested_workspace: &'static str,
    /// The canonical workspace keys present in the index.
    pub known_workspaces: Vec<String>,
    /// Whether the same query without the filter matched.
    pub global_had_hits: bool,
    /// Source provenance signals for a representative hit in this scenario.
    pub provenance: ProvenanceSignals,
}

fn ws(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// The six named scenarios, in a stable order. All paths are synthetic
/// (user `dev`, project `myproj`) — redacted by construction.
pub(crate) fn scenarios() -> Vec<WorkspaceSourceScenario> {
    vec![
        // macOS /Users path moved to /dp.
        WorkspaceSourceScenario {
            name: "macos_users_moved_to_dp",
            requested_workspace: "/Users/dev/myproj",
            known_workspaces: ws(&["/dp/myproj"]),
            global_had_hits: true,
            provenance: ProvenanceSignals {
                source_path: Some("/dp/myproj/session.jsonl".to_string()),
                file_exists: true,
                source_id: Some(1),
                origin_host: None,
                is_local_source: true,
                archive_row_present: true,
                path_mapping_applied: true,
            },
        },
        // Linux /data/projects checkout moved to a sibling path.
        WorkspaceSourceScenario {
            name: "linux_data_projects_checkout_moved",
            requested_workspace: "/data/projects/myproj",
            known_workspaces: ws(&["/data/projects2/myproj"]),
            global_had_hits: true,
            provenance: ProvenanceSignals {
                source_path: Some("/data/projects2/myproj/session.jsonl".to_string()),
                file_exists: true,
                source_id: Some(2),
                origin_host: None,
                is_local_source: true,
                archive_row_present: true,
                path_mapping_applied: false,
            },
        },
        // Remote source whose original workspace is mapped locally.
        WorkspaceSourceScenario {
            name: "remote_source_mapped_locally",
            requested_workspace: "/data/mirror/myproj",
            known_workspaces: ws(&["/data/mirror/myproj"]),
            global_had_hits: true,
            provenance: ProvenanceSignals {
                source_path: Some("/data/mirror/myproj/session.jsonl".to_string()),
                file_exists: true,
                source_id: Some(3),
                origin_host: Some("mac-mini-old".to_string()),
                is_local_source: false,
                archive_row_present: true,
                path_mapping_applied: true,
            },
        },
        // Source file pruned but the archive row survives.
        WorkspaceSourceScenario {
            name: "source_pruned_archive_row_present",
            requested_workspace: "/dp/myproj",
            known_workspaces: ws(&["/dp/myproj"]),
            global_had_hits: true,
            provenance: ProvenanceSignals {
                source_path: None,
                file_exists: false,
                source_id: None,
                origin_host: None,
                is_local_source: true,
                archive_row_present: true,
                path_mapping_applied: false,
            },
        },
        // Same project indexed under a release checkout name.
        WorkspaceSourceScenario {
            name: "same_project_release_checkout_name",
            requested_workspace: "/dp/myproj",
            known_workspaces: ws(&["/dp/myproj-2.0.0"]),
            global_had_hits: true,
            provenance: ProvenanceSignals {
                source_path: Some("/dp/myproj-2.0.0/session.jsonl".to_string()),
                file_exists: true,
                source_id: Some(5),
                origin_host: None,
                is_local_source: true,
                archive_row_present: true,
                path_mapping_applied: false,
            },
        },
        // Ambiguous basename collision: two known workspaces share the
        // requested basename.
        WorkspaceSourceScenario {
            name: "ambiguous_basename_collision",
            requested_workspace: "/old/checkout/myproj",
            known_workspaces: ws(&["/dp/a/myproj", "/dp/b/myproj"]),
            global_had_hits: true,
            provenance: ProvenanceSignals {
                source_path: Some("/dp/a/myproj/session.jsonl".to_string()),
                file_exists: true,
                source_id: Some(6),
                origin_host: None,
                is_local_source: true,
                archive_row_present: true,
                path_mapping_applied: false,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::source_provenance::ProvenanceKind;
    use crate::search::zero_result_diagnosis::{ZeroResultDiagnosis, diagnose_zero_result};

    fn scenario(name: &str) -> WorkspaceSourceScenario {
        scenarios()
            .into_iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("missing scenario {name}"))
    }

    #[test]
    fn suite_covers_all_six_named_scenarios_in_stable_order() {
        let names: Vec<&str> = scenarios().iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "macos_users_moved_to_dp",
                "linux_data_projects_checkout_moved",
                "remote_source_mapped_locally",
                "source_pruned_archive_row_present",
                "same_project_release_checkout_name",
                "ambiguous_basename_collision",
            ]
        );
    }

    #[test]
    fn each_scenario_yields_the_intended_canonical_suggestion() {
        // (name, expected diagnosis, expected number of candidates)
        let cases = [
            (
                "macos_users_moved_to_dp",
                ZeroResultDiagnosis::WorkspaceFilterLikelyWrong,
                1,
            ),
            (
                "linux_data_projects_checkout_moved",
                ZeroResultDiagnosis::WorkspaceFilterLikelyWrong,
                1,
            ),
            (
                "remote_source_mapped_locally",
                ZeroResultDiagnosis::WorkspaceHasNoMatch,
                0,
            ),
            (
                "source_pruned_archive_row_present",
                ZeroResultDiagnosis::WorkspaceHasNoMatch,
                0,
            ),
            (
                "same_project_release_checkout_name",
                ZeroResultDiagnosis::WorkspaceNotIndexed,
                0,
            ),
            (
                "ambiguous_basename_collision",
                ZeroResultDiagnosis::WorkspaceFilterLikelyWrong,
                2,
            ),
        ];
        for (name, diagnosis, n_candidates) in cases {
            let s = scenario(name);
            let report = diagnose_zero_result(
                s.requested_workspace,
                &s.known_workspaces,
                s.global_had_hits,
            );
            assert_eq!(report.diagnosis, diagnosis, "{name} diagnosis");
            assert_eq!(
                report.candidate_workspaces.len(),
                n_candidates,
                "{name} candidate count"
            );
        }
    }

    #[test]
    fn each_scenario_yields_the_intended_provenance() {
        // (name, expected kind, source_exists, archive_only)
        let cases = [
            (
                "macos_users_moved_to_dp",
                ProvenanceKind::PathMapped,
                true,
                false,
            ),
            (
                "linux_data_projects_checkout_moved",
                ProvenanceKind::LocalPresent,
                true,
                false,
            ),
            (
                "remote_source_mapped_locally",
                ProvenanceKind::PathMapped, // mapping applied dominates remote here
                true,
                false,
            ),
            (
                "source_pruned_archive_row_present",
                ProvenanceKind::ArchiveOnlyPruned,
                false,
                true,
            ),
            (
                "same_project_release_checkout_name",
                ProvenanceKind::LocalPresent,
                true,
                false,
            ),
            (
                "ambiguous_basename_collision",
                ProvenanceKind::LocalPresent,
                true,
                false,
            ),
        ];
        for (name, kind, source_exists, archive_only) in cases {
            let p = scenario(name).provenance.provenance();
            assert_eq!(p.kind, kind, "{name} provenance kind");
            assert_eq!(p.source_exists, source_exists, "{name} source_exists");
            assert_eq!(p.archive_only, archive_only, "{name} archive_only");
        }
    }

    #[test]
    fn pruned_scenario_is_not_openable_and_others_are() {
        assert!(
            !scenario("source_pruned_archive_row_present")
                .provenance
                .provenance()
                .is_openable_file()
        );
        assert!(
            scenario("macos_users_moved_to_dp")
                .provenance
                .provenance()
                .is_openable_file()
        );
    }

    #[test]
    fn diagnosis_and_provenance_project_deterministically() {
        for s in scenarios() {
            let d1 = diagnose_zero_result(
                s.requested_workspace,
                &s.known_workspaces,
                s.global_had_hits,
            );
            let d2 = diagnose_zero_result(
                s.requested_workspace,
                &s.known_workspaces,
                s.global_had_hits,
            );
            assert_eq!(d1, d2, "{} diagnosis deterministic", s.name);
            assert_eq!(
                s.provenance.provenance(),
                s.provenance.provenance(),
                "{} provenance deterministic",
                s.name
            );
        }
    }

    #[test]
    fn fixtures_are_redacted_synthetic_only() {
        // No real home directories or usernames — every path is synthetic.
        for s in scenarios() {
            assert!(
                !s.requested_workspace.contains("/Users/jeff")
                    && !s.requested_workspace.contains("/home/")
            );
            if let Some(p) = &s.provenance.source_path {
                assert!(
                    !p.contains("/home/"),
                    "{}: source_path must be synthetic",
                    s.name
                );
            }
        }
    }
}
