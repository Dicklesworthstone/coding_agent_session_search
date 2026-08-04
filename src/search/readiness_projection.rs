// Dead-code tolerated module-wide: this shared readiness projection lands
// ahead of its call-site wiring into the health/status/triage/search-meta
// JSON builders in src/lib.rs. Once those surfaces call `project`, they all
// emit the identical readiness vocabulary.
#![allow(dead_code)]

//! Shared readiness projection for health / status / triage / search-meta
//! (bead cass-fleet-resilience-20260608-uojcg.1.2).
//!
//! Today each surface reconstructs a readiness summary with ad-hoc logic, so
//! `cass health --json` can contradict `cass status --json`. This module is
//! the single projection every surface derives from the canonical
//! [`DerivedAssetTruthTable`]: [`project`] yields a [`ReadinessSummary`] with
//! one overall class, one recommended action, the detailed component facts
//! (lexical/semantic/archive-risk/quarantine), and — for cheap surfaces that
//! skip rich probes — an explicit `deferred_fields` list so the JSON says
//! what is unknown rather than guessing.
//!
//! The invariant the tests enforce: the **readiness vocabulary is identical
//! across surfaces**. A cheap `health` projection may defer fields, but it
//! must never report a different class or recommended action than `status`
//! for the same truth table. The `lib.rs` call-site wiring is the remaining
//! integration; this is the shared core. All enums serialize as snake_case.

use serde::{Deserialize, Serialize};

use crate::search::readiness::{
    ArchiveRiskLevel, CanonicalDbAvailability, DerivedAssetTruthTable, LexicalReadinessState,
    RecommendedAction, SafeNextAction, SearchRefinementLevel, SemanticReadinessState,
};

/// Which surface is projecting. Controls only which fields are *deferred*,
/// never the readiness vocabulary itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SurfaceKind {
    /// Cheap, fast: may defer rich probes (quarantine scan, projection age).
    Health,
    /// Full readiness with all component facts.
    Status,
    /// Full readiness, agent-oriented (same vocabulary as status).
    Triage,
    /// Search `--robot-meta`: adds realized refinement + lexical fallback.
    SearchMeta,
}

impl SurfaceKind {
    /// Whether this surface defers rich (expensive) probe fields for speed.
    fn defers_rich_probes(self) -> bool {
        matches!(self, Self::Health)
    }
}

/// The single overall readiness class (the golden-covered cases).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReadinessClass {
    /// Ready and converged.
    Ready,
    /// Stale but fully searchable for indexed content.
    StaleSearchable,
    /// A rebuild/repair is actively running.
    Repairing,
    /// No usable index / fresh install — search unavailable.
    Missing,
    /// Index corrupt/quarantined — search unavailable, inspect first.
    CorruptQuarantined,
    /// Canonical DB present but unusable (open failed / corrupt).
    DbUnusable,
    /// Host unreachable — nothing local is trustworthy.
    Unreachable,
}

/// The canonical readiness summary every surface emits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReadinessSummary {
    pub surface: SurfaceKind,
    /// The single overall class.
    pub class: ReadinessClass,
    /// Whether ordinary search can run.
    pub is_searchable: bool,
    /// The single recommended action (detailed facts preserved below).
    pub recommended_action: RecommendedAction,
    /// The safe next command's action (machine-matchable).
    pub safe_next_action: SafeNextAction,
    // --- detailed component facts (preserved for agents) ---
    pub lexical: LexicalReadinessState,
    pub semantic: SemanticReadinessState,
    pub archive_risk: ArchiveRiskLevel,
    /// Whether quarantined artifacts make results incomplete (advisory).
    pub quarantine_incomplete: bool,
    /// The refinement a recent search realized — search-meta only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realized_refinement: Option<SearchRefinementLevel>,
    /// Why search fell back to lexical, when it did — search-meta only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lexical_fallback_reason: Option<String>,
    /// Fields deferred for speed on cheap surfaces (so JSON says "unknown"
    /// rather than guessing). Empty for full surfaces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred_fields: Vec<String>,
}

/// Derive the overall class from the canonical DB + lexical axis.
fn classify(table: &DerivedAssetTruthTable) -> ReadinessClass {
    match table.db {
        CanonicalDbAvailability::Unreachable => return ReadinessClass::Unreachable,
        CanonicalDbAvailability::OpenFailed | CanonicalDbAvailability::Corrupt => {
            return ReadinessClass::DbUnusable;
        }
        CanonicalDbAvailability::Missing => return ReadinessClass::Missing,
        CanonicalDbAvailability::Available => {}
    }
    match table.readiness.lexical {
        LexicalReadinessState::Ready => ReadinessClass::Ready,
        LexicalReadinessState::StaleButSearchable => ReadinessClass::StaleSearchable,
        LexicalReadinessState::Repairing => ReadinessClass::Repairing,
        LexicalReadinessState::Missing => ReadinessClass::Missing,
        LexicalReadinessState::CorruptQuarantined => ReadinessClass::CorruptQuarantined,
    }
}

/// Project the canonical truth table into the shared readiness summary for
/// `surface`. The class, recommended action, and component facts are
/// identical across surfaces; only `deferred_fields` and the search-meta
/// extras vary.
pub(crate) fn project(table: &DerivedAssetTruthTable, surface: SurfaceKind) -> ReadinessSummary {
    let class = classify(table);
    let recommended_action = table.readiness.recommended_action();
    let safe_next_action = table.safe_next_command().action;

    // Search-meta surfaces the realized refinement and a lexical fallback
    // reason; other surfaces omit them.
    let (realized_refinement, lexical_fallback_reason) = if surface == SurfaceKind::SearchMeta {
        let refinement = table.readiness.last_search_refinement;
        let fallback = match refinement {
            Some(SearchRefinementLevel::LexicalOnly) => Some(match table.readiness.semantic {
                SemanticReadinessState::Absent => "semantic_absent".to_string(),
                SemanticReadinessState::Backfilling => "semantic_backfilling".to_string(),
                SemanticReadinessState::PolicyDisabled => "semantic_policy_disabled".to_string(),
                _ => "semantic_not_applied".to_string(),
            }),
            _ => None,
        };
        (refinement, fallback)
    } else {
        (None, None)
    };

    // Cheap surfaces defer rich probes but keep the core readiness vocabulary
    // so they never contradict status.
    let deferred_fields = if surface.defers_rich_probes() {
        vec![
            "quarantine_detail".to_string(),
            "last_projection_ms".to_string(),
            "scan_watermark_ms".to_string(),
        ]
    } else {
        Vec::new()
    };

    ReadinessSummary {
        surface,
        class,
        is_searchable: table.is_searchable(),
        recommended_action,
        safe_next_action,
        lexical: table.readiness.lexical,
        semantic: table.readiness.semantic,
        archive_risk: table.archive_risk,
        quarantine_incomplete: table.quarantine.has_exclusions(),
        realized_refinement,
        lexical_fallback_reason,
        deferred_fields,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::readiness::fleet_fixtures;

    fn fixture(name: &str) -> DerivedAssetTruthTable {
        fleet_fixtures()
            .into_iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("missing fixture {name}"))
            .1
    }

    #[test]
    fn enums_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&ReadinessClass::StaleSearchable).unwrap(),
            "\"stale_searchable\""
        );
        assert_eq!(
            serde_json::to_string(&SurfaceKind::SearchMeta).unwrap(),
            "\"search_meta\""
        );
    }

    #[test]
    fn health_and_status_never_contradict_for_any_fleet_state() {
        // The core .1.2 invariant: the cheap surface must not report a
        // different class / recommended action than the full one.
        for (name, table) in fleet_fixtures() {
            let health = project(&table, SurfaceKind::Health);
            let status = project(&table, SurfaceKind::Status);
            let triage = project(&table, SurfaceKind::Triage);
            assert_eq!(health.class, status.class, "{name} class health vs status");
            assert_eq!(
                health.recommended_action, status.recommended_action,
                "{name} recommended_action health vs status"
            );
            assert_eq!(
                health.is_searchable, status.is_searchable,
                "{name} searchable"
            );
            assert_eq!(health.lexical, status.lexical, "{name} lexical");
            assert_eq!(health.semantic, status.semantic, "{name} semantic");
            // Triage uses the identical vocabulary as status.
            assert_eq!(triage.class, status.class, "{name} triage vs status");
            assert_eq!(triage.recommended_action, status.recommended_action);
        }
    }

    #[test]
    fn cheap_health_marks_deferred_fields_full_status_does_not() {
        let t = fixture("local_stale_quarantine");
        let health = project(&t, SurfaceKind::Health);
        let status = project(&t, SurfaceKind::Status);
        assert!(
            !health.deferred_fields.is_empty(),
            "health must mark deferrals"
        );
        assert!(status.deferred_fields.is_empty(), "status defers nothing");
    }

    #[test]
    fn golden_classes_cover_stale_missing_repairing_corrupt() {
        // stale-but-searchable
        assert_eq!(
            project(&fixture("css_stale_existing_index"), SurfaceKind::Status).class,
            ReadinessClass::StaleSearchable
        );
        // missing
        assert_eq!(
            project(
                &fixture("csd_missing_lexical_metadata"),
                SurfaceKind::Status
            )
            .class,
            ReadinessClass::Missing
        );
        // unreachable
        assert_eq!(
            project(&fixture("mac_mini_old_unreachable"), SurfaceKind::Status).class,
            ReadinessClass::Unreachable
        );

        // repairing + corrupt-quarantined: construct directly (no fleet
        // fixture seeds them).
        let mut repairing = fixture("css_stale_existing_index");
        repairing.readiness = crate::search::readiness::ReadinessSnapshot::new(
            LexicalReadinessState::Repairing,
            SemanticReadinessState::Absent,
        );
        assert_eq!(
            project(&repairing, SurfaceKind::Status).class,
            ReadinessClass::Repairing
        );

        let mut corrupt = fixture("css_stale_existing_index");
        corrupt.readiness = crate::search::readiness::ReadinessSnapshot::new(
            LexicalReadinessState::CorruptQuarantined,
            SemanticReadinessState::Absent,
        );
        let c = project(&corrupt, SurfaceKind::Status);
        assert_eq!(c.class, ReadinessClass::CorruptQuarantined);
        assert!(!c.is_searchable);
    }

    #[test]
    fn search_meta_reports_realized_refinement_and_fallback_reason() {
        // local_stale_quarantine carries a lexical-only realized refinement.
        let t = fixture("local_stale_quarantine");
        let meta = project(&t, SurfaceKind::SearchMeta);
        assert_eq!(
            meta.realized_refinement,
            Some(SearchRefinementLevel::LexicalOnly)
        );
        assert!(meta.lexical_fallback_reason.is_some());
        assert!(
            meta.quarantine_incomplete,
            "quarantine incompleteness reported"
        );
        // Non-search surfaces omit the search-only extras.
        let status = project(&t, SurfaceKind::Status);
        assert!(status.realized_refinement.is_none());
        assert!(status.lexical_fallback_reason.is_none());
    }

    #[test]
    fn one_recommended_action_with_component_facts_preserved() {
        let t = fixture("ts1_high_archive_risk");
        let s = project(&t, SurfaceKind::Status);
        // One recommended action...
        assert_eq!(s.recommended_action, RecommendedAction::NothingRequired);
        // ...but the component facts (archive risk) are preserved for agents.
        assert_eq!(s.archive_risk, ArchiveRiskLevel::High);
        assert_eq!(s.safe_next_action, SafeNextAction::BackupThenRepair);
    }

    #[test]
    fn summary_round_trips_through_json() {
        let t = fixture("local_stale_quarantine");
        let meta = project(&t, SurfaceKind::SearchMeta);
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"surface\":\"search_meta\""));
        assert!(json.contains("\"class\":\"stale_searchable\""));
        let parsed: ReadinessSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, meta);
    }
}
