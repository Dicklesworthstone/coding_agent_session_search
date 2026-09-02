// Dead-code tolerated module-wide: the truthful search-mode metadata contract
// for bead cass-fleet-resilience-20260608-uojcg.5.4 lands here; the live
// `search --robot-meta` builders in src/lib.rs project it. The pure classifier
// + report are testable without running a search.
#![allow(dead_code)]

//! Truthful hybrid-fallback and semantic-refinement metadata (bead
//! cass-fleet-resilience-20260608-uojcg.5.4).
//!
//! `cass search --robot-meta` already reports `requested_search_mode`,
//! `search_mode` (realized), `fallback_tier`, `fallback_reason` (a free-form
//! string), and `semantic_refinement` (a bare bool). An agent that fails open
//! to lexical must not read that as a command failure or as missing data, and
//! a `--mode semantic` request that cannot run must degrade or fail *visibly*.
//!
//! Two things were missing: a precise [`SearchRefinementLevel`] (was only a
//! bool) and a **typed** fallback reason aligned with the readiness vocabulary
//! in [`crate::search::readiness_projection`] (was a free-form string an agent
//! could not branch on). This module defines [`SemanticFallbackReason`] (the
//! typed reason), classifies the existing free-form fallback strings into it,
//! derives the realized [`SearchRefinementLevel`], and folds everything into a
//! [`SearchModeReport`] via the pure [`project`] classifier — tested against
//! every scenario the .5.4 acceptance names. The live `_meta` builders call the
//! small derivations; nothing here runs a search or touches the network.

use serde::{Deserialize, Serialize};

use crate::search::query::SearchMode;
use crate::search::readiness::SearchRefinementLevel;

/// The typed reason semantic refinement did not contribute to a result, so an
/// agent can branch on a stable code instead of parsing a sentence. Reconciles
/// the free-form strings the live search emits with the readiness vocabulary
/// (`semantic_absent` / `semantic_backfilling` / `semantic_policy_disabled`)
/// used by [`crate::search::readiness_projection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SemanticFallbackReason {
    /// No embedding model has been acquired.
    SemanticAbsent,
    /// Embeddings are still backfilling against the current DB.
    SemanticBackfilling,
    /// Semantic search is disabled by policy (lexical-only).
    SemanticPolicyDisabled,
    /// The acquired model failed checksum/verification, so it is unusable.
    SemanticChecksumMismatch,
    /// A semantic context existed but was rejected at query time.
    SemanticContextRejected,
    /// A semantic context could not be loaded for this query.
    SemanticContextUnavailable,
    /// Hybrid execution itself failed and the query fell open to lexical.
    HybridExecutionError,
    /// Pack/answer evidence enrichment could not use semantic signals.
    PackEnrichmentUnavailable,
    /// Semantic was simply not applied (generic / no specific cause).
    SemanticNotApplied,
}

impl SemanticFallbackReason {
    /// Stable machine code (matches the snake_case serialization).
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::SemanticAbsent => "semantic_absent",
            Self::SemanticBackfilling => "semantic_backfilling",
            Self::SemanticPolicyDisabled => "semantic_policy_disabled",
            Self::SemanticChecksumMismatch => "semantic_checksum_mismatch",
            Self::SemanticContextRejected => "semantic_context_rejected",
            Self::SemanticContextUnavailable => "semantic_context_unavailable",
            Self::HybridExecutionError => "hybrid_execution_error",
            Self::PackEnrichmentUnavailable => "pack_enrichment_unavailable",
            Self::SemanticNotApplied => "semantic_not_applied",
        }
    }
}

/// Classify a free-form lexical-fallback reason string (the form the live
/// `SearchModeMeta::fall_back_to_lexical` records) into a typed reason. The
/// match is substring-based and case-insensitive so it survives the trailing
/// detail the call sites append (e.g. `"hybrid execution unavailable: <err>"`).
pub(crate) fn classify_fallback_reason(reason: &str) -> SemanticFallbackReason {
    let r = reason.to_ascii_lowercase();
    // Most specific phrases first.
    if r.contains("policy") || r.contains("disabled") || r.contains("lexical-only") {
        SemanticFallbackReason::SemanticPolicyDisabled
    } else if r.contains("checksum") || r.contains("verification failed") {
        SemanticFallbackReason::SemanticChecksumMismatch
    } else if r.contains("backfill") || r.contains("catching up") {
        SemanticFallbackReason::SemanticBackfilling
    } else if r.contains("not acquired") || r.contains("no model") || r.contains("model absent") {
        SemanticFallbackReason::SemanticAbsent
    } else if r.contains("pack semantic enrichment") {
        SemanticFallbackReason::PackEnrichmentUnavailable
    } else if r.contains("context rejected") {
        SemanticFallbackReason::SemanticContextRejected
    } else if r.contains("context unavailable") {
        SemanticFallbackReason::SemanticContextUnavailable
    } else if r.contains("hybrid execution") {
        SemanticFallbackReason::HybridExecutionError
    } else {
        SemanticFallbackReason::SemanticNotApplied
    }
}

/// Derive the realized refinement level from the realized mode and whether the
/// quality tier actually contributed. A lexical realized mode (including a
/// fail-open) is always [`SearchRefinementLevel::LexicalOnly`]; a completed
/// semantic/hybrid query is [`SearchRefinementLevel::FullyHybridRefined`]; the
/// [`SearchRefinementLevel::FastTierRefined`] case is for progressive (TUI)
/// searches where only the fast tier has landed so far.
pub(crate) fn refinement_level(
    realized: SearchMode,
    quality_tier_refined: bool,
) -> SearchRefinementLevel {
    match realized {
        SearchMode::Lexical => SearchRefinementLevel::LexicalOnly,
        SearchMode::Semantic | SearchMode::Hybrid => {
            if quality_tier_refined {
                SearchRefinementLevel::FullyHybridRefined
            } else {
                SearchRefinementLevel::FastTierRefined
            }
        }
    }
}

/// What happened to an explicit `--mode semantic` request. Semantic-only never
/// silently degrades to lexical: it is either satisfied or it fails visibly
/// (exit 15), and this records which so robot output is unambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SemanticOnlyOutcome {
    /// The request was not `--mode semantic`.
    NotRequested,
    /// Semantic-only ran and returned semantic results.
    Satisfied,
    /// Semantic-only could not run; the query failed rather than degrading.
    FailedUnavailable,
}

/// The inputs a search surface supplies to project truthful mode metadata.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SearchModeSignals<'a> {
    /// The mode the caller asked for.
    pub requested: SearchMode,
    /// The mode that actually ran (after any fail-open).
    pub realized: SearchMode,
    /// Whether the realized mode was the default (no explicit `--mode`).
    pub defaulted: bool,
    /// Whether the query failed open from semantic/hybrid to lexical.
    pub fell_back_to_lexical: bool,
    /// The free-form fallback reason recorded at the fail-open site, if any.
    pub fallback_reason_text: Option<&'a str>,
    /// Whether the quality (not just fast) semantic tier contributed. For a
    /// one-shot CLI search this is true whenever semantic/hybrid completed.
    pub quality_tier_refined: bool,
    /// Whether a `--mode semantic` request could actually run.
    pub semantic_only_satisfied: bool,
}

/// The truthful, fully typed search-mode metadata report. Output-only
/// (`SearchMode` is serialize-only), so this does not derive `Deserialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SearchModeReport {
    pub requested_mode: SearchMode,
    pub realized_mode: SearchMode,
    pub mode_defaulted: bool,
    pub refinement_level: SearchRefinementLevel,
    /// Whether semantic refinement actually contributed.
    pub semantic_refinement: bool,
    pub fallback_tier: Option<&'static str>,
    pub fallback_reason: Option<SemanticFallbackReason>,
    pub semantic_only_outcome: SemanticOnlyOutcome,
}

/// Project the signals into the truthful report. Pure; no I/O.
pub(crate) fn project(signals: SearchModeSignals<'_>) -> SearchModeReport {
    let refinement_level = if signals.fell_back_to_lexical {
        SearchRefinementLevel::LexicalOnly
    } else {
        refinement_level(signals.realized, signals.quality_tier_refined)
    };

    let semantic_refinement = matches!(
        refinement_level,
        SearchRefinementLevel::FastTierRefined | SearchRefinementLevel::FullyHybridRefined
    );

    let fallback_tier = if signals.fell_back_to_lexical {
        Some("lexical")
    } else {
        None
    };

    let fallback_reason = if signals.fell_back_to_lexical {
        Some(
            signals
                .fallback_reason_text
                .map(classify_fallback_reason)
                .unwrap_or(SemanticFallbackReason::SemanticNotApplied),
        )
    } else {
        None
    };

    let semantic_only_outcome = match signals.requested {
        SearchMode::Semantic => {
            if signals.semantic_only_satisfied {
                SemanticOnlyOutcome::Satisfied
            } else {
                SemanticOnlyOutcome::FailedUnavailable
            }
        }
        _ => SemanticOnlyOutcome::NotRequested,
    };

    SearchModeReport {
        requested_mode: signals.requested,
        realized_mode: signals.realized,
        mode_defaulted: signals.defaulted,
        refinement_level,
        semantic_refinement,
        fallback_tier,
        fallback_reason,
        semantic_only_outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SearchModeSignals<'static> {
        SearchModeSignals {
            requested: SearchMode::Hybrid,
            realized: SearchMode::Hybrid,
            defaulted: true,
            fell_back_to_lexical: false,
            fallback_reason_text: None,
            quality_tier_refined: true,
            semantic_only_satisfied: false,
        }
    }

    // --- the seven acceptance scenarios ------------------------------------

    #[test]
    fn scenario_full_hybrid() {
        let r = project(base());
        assert_eq!(r.realized_mode, SearchMode::Hybrid);
        assert_eq!(
            r.refinement_level,
            SearchRefinementLevel::FullyHybridRefined
        );
        assert!(r.semantic_refinement);
        assert!(r.fallback_tier.is_none());
        assert!(r.fallback_reason.is_none());
        assert_eq!(r.semantic_only_outcome, SemanticOnlyOutcome::NotRequested);
    }

    #[test]
    fn scenario_hybrid_with_absent_model_falls_open_to_lexical() {
        let mut s = base();
        s.realized = SearchMode::Lexical;
        s.fell_back_to_lexical = true;
        s.fallback_reason_text = Some("semantic context unavailable: no model acquired");
        let r = project(s);
        assert_eq!(r.realized_mode, SearchMode::Lexical);
        assert_eq!(r.refinement_level, SearchRefinementLevel::LexicalOnly);
        assert!(!r.semantic_refinement);
        assert_eq!(r.fallback_tier, Some("lexical"));
        // "no model acquired" classifies as absent, not the generic context msg.
        assert_eq!(
            r.fallback_reason,
            Some(SemanticFallbackReason::SemanticAbsent)
        );
    }

    #[test]
    fn scenario_lexical_only_policy() {
        let mut s = base();
        s.requested = SearchMode::Lexical;
        s.realized = SearchMode::Lexical;
        s.defaulted = false;
        let r = project(s);
        assert_eq!(r.refinement_level, SearchRefinementLevel::LexicalOnly);
        assert!(!r.semantic_refinement);
        // Requested lexical is not a fallback — no fallback tier/reason.
        assert!(r.fallback_tier.is_none());
        assert!(r.fallback_reason.is_none());
        assert_eq!(r.semantic_only_outcome, SemanticOnlyOutcome::NotRequested);
    }

    #[test]
    fn scenario_fast_tier_refinement() {
        let mut s = base();
        s.quality_tier_refined = false; // only the fast tier landed
        let r = project(s);
        assert_eq!(r.refinement_level, SearchRefinementLevel::FastTierRefined);
        assert!(r.semantic_refinement);
        assert!(r.fallback_tier.is_none());
    }

    #[test]
    fn scenario_semantic_only_unavailable_fails_visibly() {
        let mut s = base();
        s.requested = SearchMode::Semantic;
        s.realized = SearchMode::Semantic;
        s.semantic_only_satisfied = false;
        let r = project(s);
        // Semantic-only never silently degrades; the outcome says it failed.
        assert_eq!(
            r.semantic_only_outcome,
            SemanticOnlyOutcome::FailedUnavailable
        );
        assert!(
            r.fallback_tier.is_none(),
            "semantic-only does not fail open"
        );
    }

    #[test]
    fn scenario_semantic_only_satisfied() {
        let mut s = base();
        s.requested = SearchMode::Semantic;
        s.realized = SearchMode::Semantic;
        s.semantic_only_satisfied = true;
        let r = project(s);
        assert_eq!(r.semantic_only_outcome, SemanticOnlyOutcome::Satisfied);
        assert_eq!(
            r.refinement_level,
            SearchRefinementLevel::FullyHybridRefined
        );
        assert!(r.semantic_refinement);
    }

    #[test]
    fn scenario_stale_lexical_but_searchable() {
        // Stale lexical still serves results; with no semantic it is a clean
        // lexical realization, not a failure.
        let mut s = base();
        s.requested = SearchMode::Lexical;
        s.realized = SearchMode::Lexical;
        let r = project(s);
        assert_eq!(r.refinement_level, SearchRefinementLevel::LexicalOnly);
        assert!(r.fallback_reason.is_none());
    }

    #[test]
    fn scenario_hybrid_execution_error_falls_open() {
        let mut s = base();
        s.realized = SearchMode::Lexical;
        s.fell_back_to_lexical = true;
        s.fallback_reason_text = Some("hybrid execution unavailable: index io error");
        let r = project(s);
        assert_eq!(
            r.fallback_reason,
            Some(SemanticFallbackReason::HybridExecutionError)
        );
        assert_eq!(r.refinement_level, SearchRefinementLevel::LexicalOnly);
    }

    // --- reason classification ---------------------------------------------

    #[test]
    fn classify_covers_live_fallback_strings() {
        let cases = [
            (
                "semantic context rejected: bad dim",
                SemanticFallbackReason::SemanticContextRejected,
            ),
            (
                "semantic context unavailable: missing vectors",
                SemanticFallbackReason::SemanticContextUnavailable,
            ),
            (
                "hybrid execution unavailable: e",
                SemanticFallbackReason::HybridExecutionError,
            ),
            (
                "pack semantic enrichment unavailable; using lexical evidence",
                SemanticFallbackReason::PackEnrichmentUnavailable,
            ),
            (
                "semantic disabled by policy",
                SemanticFallbackReason::SemanticPolicyDisabled,
            ),
            (
                "model checksum mismatch",
                SemanticFallbackReason::SemanticChecksumMismatch,
            ),
            (
                "semantic backfill in progress",
                SemanticFallbackReason::SemanticBackfilling,
            ),
            ("no model acquired", SemanticFallbackReason::SemanticAbsent),
            (
                "something else entirely",
                SemanticFallbackReason::SemanticNotApplied,
            ),
        ];
        for (text, want) in cases {
            assert_eq!(classify_fallback_reason(text), want, "for {text:?}");
        }
    }

    #[test]
    fn refinement_level_maps_modes() {
        assert_eq!(
            refinement_level(SearchMode::Lexical, true),
            SearchRefinementLevel::LexicalOnly
        );
        assert_eq!(
            refinement_level(SearchMode::Hybrid, true),
            SearchRefinementLevel::FullyHybridRefined
        );
        assert_eq!(
            refinement_level(SearchMode::Hybrid, false),
            SearchRefinementLevel::FastTierRefined
        );
        assert_eq!(
            refinement_level(SearchMode::Semantic, true),
            SearchRefinementLevel::FullyHybridRefined
        );
    }

    // --- serialization ------------------------------------------------------

    #[test]
    fn report_serializes_with_snake_case() {
        let r = project(base());
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"refinement_level\":\"fully_hybrid_refined\""));
        assert!(json.contains("\"semantic_only_outcome\":\"not_requested\""));
        assert!(json.contains("\"semantic_refinement\":true"));
    }

    #[test]
    fn fallback_reason_code_matches_serialization() {
        for reason in [
            SemanticFallbackReason::SemanticAbsent,
            SemanticFallbackReason::HybridExecutionError,
            SemanticFallbackReason::PackEnrichmentUnavailable,
        ] {
            assert_eq!(
                serde_json::to_string(&reason).unwrap(),
                format!("\"{}\"", reason.code())
            );
        }
    }
}
