// Dead-code tolerated module-wide: this archive-first drill-down resolver
// lands ahead of the view/expand/pack code paths in src/lib.rs that will
// consult it to read canonical archive rows. Downstream bead .12.5
// (report-derived E2E scenarios) exercises the wired behavior.
#![allow(dead_code)]

//! Archive-first drill-down resolution for view / expand / pack evidence
//! (bead cass-fleet-resilience-20260608-uojcg.7.3).
//!
//! The report's lesson is that a hit's `source_path` is not the only
//! authority: when the path is stale or absent, view/expand/pack must still
//! drill into the conversation by its **stable archive identity** (the
//! canonical SQLite conversation/message rows) instead of failing. A live
//! `source_path` + line stays supported when it exists.
//!
//! This module is the pure decision layer: given what a search-hit bundle
//! carries (whether a live file backs it, the archive identifiers, the
//! source id, and how many candidate sources matched), it decides whether to
//! read from the live file or the archive row — and, when neither is
//! resolvable, returns a stable `err.kind` (`not_found` / `ambiguous_source`)
//! with an actionable hint instead of an opaque failure. The actual
//! canonical-row read and view rendering are the lib.rs integration (no new
//! rusqlite; privacy/redaction enforced there). All enums serialize as
//! snake_case.

use serde::{Deserialize, Serialize};

/// Where view/expand/pack should read the drilled-down content from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DrillDownSource {
    /// The live `source_path` (+ line) still exists; read it directly.
    LiveFile,
    /// Read from the canonical SQLite archive row by stable identity.
    ArchiveRow,
}

/// Stable `err.kind` when a drill-down cannot be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum DrillDownError {
    /// No live file and no usable archive identity — nothing to drill into.
    NotFound,
    /// More than one candidate source matched and no `source_id`/archive id
    /// disambiguates them.
    AmbiguousSource,
}

/// The resolution: either a concrete source to read, or a stable error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub(crate) enum DrillDownResolution {
    Resolve { source: DrillDownSource },
    Error { kind: DrillDownError },
}

/// What a search-hit bundle carries for drill-down. The view/pack caller
/// populates this from the hit + its provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DrillDownRequest {
    /// The live `source_path` exists and is openable (from
    /// `SourceProvenance::is_openable_file`).
    pub has_live_file: bool,
    /// A stable archive conversation identity is present.
    pub archive_conversation_id: Option<i64>,
    /// A stable archive message identity is present (optional, narrows to a
    /// message within the conversation).
    pub archive_message_id: Option<i64>,
    /// The source id, when known (disambiguates colliding sources).
    pub source_id: Option<i64>,
    /// Number of candidate sources that matched the bundle's path/identity.
    /// `>1` with no `source_id` is ambiguous.
    pub candidate_source_count: usize,
}

/// The drill-down plan: the resolution plus whether it preferred the archive,
/// a human reason, and a hint for the error cases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DrillDownPlan {
    pub resolution: DrillDownResolution,
    /// True when content is read from the archive rather than a live file.
    pub archive_first: bool,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl DrillDownRequest {
    fn has_archive_identity(&self) -> bool {
        self.archive_conversation_id.is_some()
    }

    /// Decide how to drill down, archive-first when the live file is gone.
    pub(crate) fn plan(&self) -> DrillDownPlan {
        // Ambiguity is resolved before anything else: multiple candidate
        // sources with nothing to disambiguate must not silently pick one.
        if self.candidate_source_count > 1 && self.source_id.is_none() {
            return DrillDownPlan {
                resolution: DrillDownResolution::Error {
                    kind: DrillDownError::AmbiguousSource,
                },
                archive_first: false,
                reason: format!(
                    "{} candidate sources matched and no source_id disambiguates them",
                    self.candidate_source_count
                ),
                hint: Some("re-run with --source-id to select the intended source".to_string()),
            };
        }

        // A live file (+ line) remains the primary authority when present.
        if self.has_live_file {
            return DrillDownPlan {
                resolution: DrillDownResolution::Resolve {
                    source: DrillDownSource::LiveFile,
                },
                archive_first: false,
                reason: "source_path exists; read the live file (line remains authoritative)"
                    .to_string(),
                hint: None,
            };
        }

        // No live file: drill into the canonical archive row by identity.
        if self.has_archive_identity() {
            return DrillDownPlan {
                resolution: DrillDownResolution::Resolve {
                    source: DrillDownSource::ArchiveRow,
                },
                archive_first: true,
                reason: "source_path is stale or absent; drilling into the canonical archive row by stable identity".to_string(),
                hint: None,
            };
        }

        // Neither a live file nor an archive identity: a clear not-found.
        DrillDownPlan {
            resolution: DrillDownResolution::Error {
                kind: DrillDownError::NotFound,
            },
            archive_first: false,
            reason: "no live file and no archive identity for this hit".to_string(),
            hint: Some(
                "re-run the search with --robot-meta to capture archive identifiers".to_string(),
            ),
        }
    }
}

impl DrillDownPlan {
    pub(crate) fn is_error(&self) -> bool {
        matches!(self.resolution, DrillDownResolution::Error { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live() -> DrillDownRequest {
        DrillDownRequest {
            has_live_file: true,
            archive_conversation_id: Some(100),
            archive_message_id: Some(5),
            source_id: Some(1),
            candidate_source_count: 1,
        }
    }

    #[test]
    fn enums_serialize_with_stable_wire_forms() {
        assert_eq!(
            serde_json::to_string(&DrillDownSource::ArchiveRow).unwrap(),
            "\"archive_row\""
        );
        // err.kind uses kebab-case per the robot contract.
        assert_eq!(
            serde_json::to_string(&DrillDownError::AmbiguousSource).unwrap(),
            "\"ambiguous-source\""
        );
        assert_eq!(
            serde_json::to_string(&DrillDownError::NotFound).unwrap(),
            "\"not-found\""
        );
    }

    #[test]
    fn live_file_present_reads_the_file_not_the_archive() {
        let plan = live().plan();
        assert_eq!(
            plan.resolution,
            DrillDownResolution::Resolve {
                source: DrillDownSource::LiveFile
            }
        );
        assert!(!plan.archive_first);
        assert!(!plan.is_error());
    }

    #[test]
    fn stale_path_with_archive_identity_drills_archive_first() {
        let mut r = live();
        r.has_live_file = false; // source_path stale/absent
        let plan = r.plan();
        assert_eq!(
            plan.resolution,
            DrillDownResolution::Resolve {
                source: DrillDownSource::ArchiveRow
            }
        );
        assert!(plan.archive_first);
    }

    #[test]
    fn archive_only_fixture_resolves_to_a_useful_archive_row() {
        // No file, but a conversation identity exists (the source-pruned /
        // archive-only case from .7.2/.7.4).
        let r = DrillDownRequest {
            has_live_file: false,
            archive_conversation_id: Some(4242),
            archive_message_id: None,
            source_id: None,
            candidate_source_count: 1,
        };
        let plan = r.plan();
        assert!(!plan.is_error());
        assert!(plan.archive_first);
    }

    #[test]
    fn missing_row_returns_not_found_with_hint() {
        let r = DrillDownRequest {
            has_live_file: false,
            archive_conversation_id: None,
            archive_message_id: None,
            source_id: None,
            candidate_source_count: 1,
        };
        let plan = r.plan();
        assert_eq!(
            plan.resolution,
            DrillDownResolution::Error {
                kind: DrillDownError::NotFound
            }
        );
        assert!(plan.hint.is_some(), "errors must carry an actionable hint");
    }

    #[test]
    fn ambiguous_source_without_source_id_is_an_error_with_hint() {
        let r = DrillDownRequest {
            has_live_file: false,
            archive_conversation_id: Some(1),
            archive_message_id: None,
            source_id: None,
            candidate_source_count: 3,
        };
        let plan = r.plan();
        assert_eq!(
            plan.resolution,
            DrillDownResolution::Error {
                kind: DrillDownError::AmbiguousSource
            }
        );
        assert!(plan.hint.unwrap().contains("--source-id"));
    }

    #[test]
    fn source_id_disambiguates_multiple_candidates() {
        let mut r = live();
        r.candidate_source_count = 3;
        r.source_id = Some(2); // explicit selection resolves ambiguity
        assert!(!r.plan().is_error());
    }

    #[test]
    fn plan_round_trips_through_json() {
        let mut r = live();
        r.has_live_file = false;
        let plan = r.plan();
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"outcome\":\"resolve\""));
        assert!(json.contains("\"source\":\"archive_row\""));
        assert!(json.contains("\"archive_first\":true"));
        let parsed: DrillDownPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, plan);
    }
}
