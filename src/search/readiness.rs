// Dead-code tolerated module-wide: the readiness vocabulary lands here
// ahead of wiring into status / health / capabilities / search metadata
// callers. Downstream slices will plug these types into the JSON payload
// builders in src/lib.rs and the TUI status surface.
#![allow(dead_code)]

//! Truthful readiness-state vocabulary for lexical vs. semantic search
//! (bead ibuuh.9).
//!
//! Today cass reports a single "healthy / unhealthy" bit that conflates
//! "lexical index missing" (actually broken — search returns nothing),
//! "lexical index stale but searchable" (slightly old but fully correct),
//! "lexical index rebuilding in background" (search works, new content
//! will land shortly), and "semantic tier still backfilling" (lexical
//! results are complete, hybrid refinement catches up later). Agents and
//! humans keep triggering unnecessary repair rituals because the single
//! health bit cannot distinguish these cases.
//!
//! This module lands the vocabulary that future status/capabilities/
//! search-metadata payloads will project into their JSON. The fields are
//! intentionally orthogonal — lexical readiness and semantic readiness
//! are independent dimensions, and the user-facing `recommended_action`
//! is derived from their combination rather than dropping them behind a
//! single scalar.
//!
//! Invariants the types enforce:
//! - `LexicalReadinessState` covers the five states any agent must be
//!   able to distinguish: `Missing`, `Repairing`, `StaleButSearchable`,
//!   `Ready`, `CorruptQuarantined`. Ordinary search is correct in
//!   `StaleButSearchable` and `Ready` (and degrading-but-serving in
//!   `Repairing`); it is only unavailable in `Missing` and
//!   `CorruptQuarantined`.
//! - `SemanticReadinessState` covers `Absent`, `Backfilling`,
//!   `FastTierReady`, `HybridReady`, `PolicyDisabled`. Absence and
//!   policy-disabled both mean "no semantic refinement" but have
//!   different operator implications.
//! - `SearchRefinementLevel` describes what a PARTICULAR completed
//!   search actually returned (`LexicalOnly`, `FastTierRefined`,
//!   `FullyHybridRefined`). This is independent of the tier
//!   *readiness* above — a search may be `LexicalOnly` either because
//!   the semantic tier was absent or because the planner chose not to
//!   refine.
//! - `ReadinessSnapshot` groups all three plus a
//!   `RecommendedAction` so every downstream consumer (CLI, TUI,
//!   robot) derives its summary from the same canonical source.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LexicalReadinessState {
    /// No usable lexical index exists on disk. Search is unavailable
    /// until a rebuild runs.
    Missing,
    /// A lexical rebuild is actively running; ordinary queries may
    /// return partial results until the rebuild settles.
    Repairing,
    /// The lexical index exists and is byte-consistent but is known to
    /// lag recent DB mutations. Search is fully correct for everything
    /// already indexed; recent ingests may not be visible yet.
    StaleButSearchable,
    /// The lexical index is up to date against the canonical DB.
    Ready,
    /// The lexical index failed validation and has been quarantined
    /// for inspection. Search is unavailable; operator inspection is
    /// required before any auto-recover path is safe.
    CorruptQuarantined,
}

impl LexicalReadinessState {
    /// Whether ordinary search can run against this state. True for
    /// Ready, StaleButSearchable, and Repairing (degraded); false for
    /// Missing and CorruptQuarantined.
    pub(crate) fn is_searchable(self) -> bool {
        matches!(
            self,
            Self::Ready | Self::StaleButSearchable | Self::Repairing
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SemanticReadinessState {
    /// No semantic assets exist. Hybrid refinement is impossible until
    /// an acquisition run lands the required models and embeddings.
    Absent,
    /// Semantic assets are being acquired or backfilled; fast-tier
    /// refinement may become available mid-flight.
    Backfilling,
    /// Fast-tier semantic assets are ready; the quality tier is not
    /// yet available.
    FastTierReady,
    /// Both tiers ready; fully hybrid refinement is possible.
    HybridReady,
    /// The operator explicitly disabled semantic search via policy;
    /// absence is intentional, not a failure condition.
    PolicyDisabled,
}

impl SemanticReadinessState {
    /// Whether the semantic tier can contribute to query refinement at
    /// this state. True only for `FastTierReady` and `HybridReady`.
    pub(crate) fn can_refine(self) -> bool {
        matches!(self, Self::FastTierReady | Self::HybridReady)
    }
}

/// What a completed search actually produced. Independent of tier
/// *readiness* — a search can be `LexicalOnly` either because the
/// semantic tier was absent or because the planner chose not to refine
/// (e.g., a pinned-lexical flag or a fail-open demotion).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SearchRefinementLevel {
    LexicalOnly,
    FastTierRefined,
    FullyHybridRefined,
}

/// Operator / agent-facing remediation recommendation. Derived from a
/// `ReadinessSnapshot` rather than stored; kept as an enum so
/// downstream consumers can pattern-match consistently across CLI,
/// TUI, and robot payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecommendedAction {
    /// Everything is converged or acceptably degraded; no user action
    /// needed.
    NothingRequired,
    /// The lexical index is missing or quarantined and must be
    /// rebuilt before search can resume.
    RepairLexicalNow,
    /// A lexical repair is already running. Foreground callers should
    /// attach or wait boundedly instead of starting another rebuild or
    /// reporting the semantic tier as the active wait reason.
    WaitForLexicalRepair,
    /// Lexical search is working; semantic assets are still
    /// converging. Waiting is sufficient.
    WaitForSemanticCatchUp,
    /// Lexical index is stale; a rebuild is recommended to pick up
    /// recent ingests but search continues to work in the meantime.
    RefreshLexicalSoon,
    /// Policy explicitly disabled semantic refinement; nothing to do
    /// beyond acknowledging the degraded search quality.
    SemanticDisabledByPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReadinessSnapshot {
    pub lexical: LexicalReadinessState,
    pub semantic: SemanticReadinessState,
    /// Optional: the refinement level the most recent completed
    /// search actually achieved. `None` when no search has run since
    /// startup.
    pub last_search_refinement: Option<SearchRefinementLevel>,
}

impl ReadinessSnapshot {
    pub(crate) fn new(lexical: LexicalReadinessState, semantic: SemanticReadinessState) -> Self {
        Self {
            lexical,
            semantic,
            last_search_refinement: None,
        }
    }

    pub(crate) fn with_last_search_refinement(mut self, level: SearchRefinementLevel) -> Self {
        self.last_search_refinement = Some(level);
        self
    }

    /// Derive the recommended operator action from the current
    /// readiness state. Deliberately simple and conservative: the
    /// lexical axis dominates (a broken lexical index is a real
    /// outage; semantic issues are degraded-service at worst).
    pub(crate) fn recommended_action(&self) -> RecommendedAction {
        match self.lexical {
            LexicalReadinessState::Missing | LexicalReadinessState::CorruptQuarantined => {
                RecommendedAction::RepairLexicalNow
            }
            LexicalReadinessState::Repairing => {
                // Lexical repair dominates every semantic state: the
                // foreground contract is attach/wait/fail-open for the
                // active repair, not a second rebuild or a semantic wait.
                RecommendedAction::WaitForLexicalRepair
            }
            LexicalReadinessState::StaleButSearchable => RecommendedAction::RefreshLexicalSoon,
            LexicalReadinessState::Ready => match self.semantic {
                SemanticReadinessState::Absent | SemanticReadinessState::Backfilling => {
                    RecommendedAction::WaitForSemanticCatchUp
                }
                SemanticReadinessState::PolicyDisabled => {
                    RecommendedAction::SemanticDisabledByPolicy
                }
                SemanticReadinessState::FastTierReady | SemanticReadinessState::HybridReady => {
                    RecommendedAction::NothingRequired
                }
            },
        }
    }

    /// Whether ordinary search queries can run at all. Collapses the
    /// two lexical-axis failure modes into a single predicate for
    /// callers that only care about availability.
    pub(crate) fn is_searchable(&self) -> bool {
        self.lexical.is_searchable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_states_serialize_as_snake_case() {
        let pairs: &[(LexicalReadinessState, &str)] = &[
            (LexicalReadinessState::Missing, "missing"),
            (LexicalReadinessState::Repairing, "repairing"),
            (
                LexicalReadinessState::StaleButSearchable,
                "stale_but_searchable",
            ),
            (LexicalReadinessState::Ready, "ready"),
            (
                LexicalReadinessState::CorruptQuarantined,
                "corrupt_quarantined",
            ),
        ];
        for (state, expected) in pairs {
            assert_eq!(
                serde_json::to_string(state).unwrap(),
                format!("\"{expected}\"")
            );
        }
    }

    #[test]
    fn semantic_states_serialize_as_snake_case() {
        let pairs: &[(SemanticReadinessState, &str)] = &[
            (SemanticReadinessState::Absent, "absent"),
            (SemanticReadinessState::Backfilling, "backfilling"),
            (SemanticReadinessState::FastTierReady, "fast_tier_ready"),
            (SemanticReadinessState::HybridReady, "hybrid_ready"),
            (SemanticReadinessState::PolicyDisabled, "policy_disabled"),
        ];
        for (state, expected) in pairs {
            assert_eq!(
                serde_json::to_string(state).unwrap(),
                format!("\"{expected}\"")
            );
        }
    }

    #[test]
    fn refinement_levels_serialize_as_snake_case() {
        let pairs: &[(SearchRefinementLevel, &str)] = &[
            (SearchRefinementLevel::LexicalOnly, "lexical_only"),
            (SearchRefinementLevel::FastTierRefined, "fast_tier_refined"),
            (
                SearchRefinementLevel::FullyHybridRefined,
                "fully_hybrid_refined",
            ),
        ];
        for (level, expected) in pairs {
            assert_eq!(
                serde_json::to_string(level).unwrap(),
                format!("\"{expected}\"")
            );
        }
    }

    #[test]
    fn is_searchable_distinguishes_lexical_failure_modes() {
        assert!(!LexicalReadinessState::Missing.is_searchable());
        assert!(!LexicalReadinessState::CorruptQuarantined.is_searchable());
        assert!(LexicalReadinessState::Repairing.is_searchable());
        assert!(LexicalReadinessState::StaleButSearchable.is_searchable());
        assert!(LexicalReadinessState::Ready.is_searchable());
    }

    #[test]
    fn semantic_can_refine_only_when_at_least_fast_tier_ready() {
        assert!(!SemanticReadinessState::Absent.can_refine());
        assert!(!SemanticReadinessState::Backfilling.can_refine());
        assert!(!SemanticReadinessState::PolicyDisabled.can_refine());
        assert!(SemanticReadinessState::FastTierReady.can_refine());
        assert!(SemanticReadinessState::HybridReady.can_refine());
    }

    #[test]
    fn recommended_actions_serialize_as_snake_case() {
        let pairs: &[(RecommendedAction, &str)] = &[
            (RecommendedAction::NothingRequired, "nothing_required"),
            (RecommendedAction::RepairLexicalNow, "repair_lexical_now"),
            (
                RecommendedAction::WaitForLexicalRepair,
                "wait_for_lexical_repair",
            ),
            (
                RecommendedAction::WaitForSemanticCatchUp,
                "wait_for_semantic_catch_up",
            ),
            (
                RecommendedAction::RefreshLexicalSoon,
                "refresh_lexical_soon",
            ),
            (
                RecommendedAction::SemanticDisabledByPolicy,
                "semantic_disabled_by_policy",
            ),
        ];
        for (action, expected) in pairs {
            let expected_json = format!("\"{expected}\"");
            assert!(
                matches!(
                    serde_json::to_string(action).as_deref(),
                    Ok(actual) if actual == expected_json.as_str()
                ),
                "action should serialize as {expected_json}"
            );
        }
    }

    #[test]
    fn recommended_action_missing_lexical_always_repair_now() {
        for sem in [
            SemanticReadinessState::Absent,
            SemanticReadinessState::Backfilling,
            SemanticReadinessState::FastTierReady,
            SemanticReadinessState::HybridReady,
            SemanticReadinessState::PolicyDisabled,
        ] {
            let snap = ReadinessSnapshot::new(LexicalReadinessState::Missing, sem);
            assert_eq!(
                snap.recommended_action(),
                RecommendedAction::RepairLexicalNow
            );
        }
    }

    #[test]
    fn recommended_action_corrupt_lexical_always_repair_now() {
        let snap = ReadinessSnapshot::new(
            LexicalReadinessState::CorruptQuarantined,
            SemanticReadinessState::HybridReady,
        );
        assert_eq!(
            snap.recommended_action(),
            RecommendedAction::RepairLexicalNow
        );
    }

    #[test]
    fn recommended_action_active_lexical_repair_dominates_semantic_state() {
        for sem in [
            SemanticReadinessState::Absent,
            SemanticReadinessState::Backfilling,
            SemanticReadinessState::FastTierReady,
            SemanticReadinessState::HybridReady,
            SemanticReadinessState::PolicyDisabled,
        ] {
            let snap = ReadinessSnapshot::new(LexicalReadinessState::Repairing, sem);
            assert_eq!(
                snap.recommended_action(),
                RecommendedAction::WaitForLexicalRepair
            );
            assert!(snap.is_searchable());
        }
    }

    #[test]
    fn recommended_action_stale_lexical_requests_refresh() {
        for sem in [
            SemanticReadinessState::Absent,
            SemanticReadinessState::HybridReady,
        ] {
            let snap = ReadinessSnapshot::new(LexicalReadinessState::StaleButSearchable, sem);
            assert_eq!(
                snap.recommended_action(),
                RecommendedAction::RefreshLexicalSoon
            );
        }
    }

    #[test]
    fn recommended_action_ready_plus_hybrid_is_nothing_required() {
        let snap = ReadinessSnapshot::new(
            LexicalReadinessState::Ready,
            SemanticReadinessState::HybridReady,
        );
        assert_eq!(
            snap.recommended_action(),
            RecommendedAction::NothingRequired
        );
    }

    #[test]
    fn recommended_action_ready_plus_policy_disabled_acknowledges_policy() {
        let snap = ReadinessSnapshot::new(
            LexicalReadinessState::Ready,
            SemanticReadinessState::PolicyDisabled,
        );
        assert_eq!(
            snap.recommended_action(),
            RecommendedAction::SemanticDisabledByPolicy
        );
    }

    #[test]
    fn recommended_action_ready_plus_semantic_converging_waits() {
        for sem in [
            SemanticReadinessState::Absent,
            SemanticReadinessState::Backfilling,
        ] {
            let snap = ReadinessSnapshot::new(LexicalReadinessState::Ready, sem);
            assert_eq!(
                snap.recommended_action(),
                RecommendedAction::WaitForSemanticCatchUp
            );
        }
    }

    #[test]
    fn snapshot_with_last_search_refinement_round_trips_through_json() {
        let snap = ReadinessSnapshot::new(
            LexicalReadinessState::Ready,
            SemanticReadinessState::FastTierReady,
        )
        .with_last_search_refinement(SearchRefinementLevel::FastTierRefined);

        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"lexical\":\"ready\""));
        assert!(json.contains("\"semantic\":\"fast_tier_ready\""));
        assert!(json.contains("\"last_search_refinement\":\"fast_tier_refined\""));

        let parsed: ReadinessSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, snap);
    }

    #[test]
    fn snapshot_defaults_last_search_refinement_to_none() {
        let snap = ReadinessSnapshot::new(
            LexicalReadinessState::Ready,
            SemanticReadinessState::HybridReady,
        );
        assert!(snap.last_search_refinement.is_none());
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"last_search_refinement\":null"));
    }
}
