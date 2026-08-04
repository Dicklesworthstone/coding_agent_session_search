// Dead-code tolerated module-wide: the precise semantic-readiness reason
// vocabulary lands here ahead of the status/health/triage/search-metadata
// surfaces that will project it and the model-acquisition layer (.5.5) that
// will populate the richer checksum/model-file signals. Downstream beads
// (.5.4 truthful hybrid fallback, .5.5 model acquisition hardening) consume
// these types.
#![allow(dead_code)]

//! Precise semantic readiness reasons and tier state (bead
//! cass-fleet-resilience-20260608-uojcg.5.1).
//!
//! Every reachable fleet node in the 2026-06-08 report had semantic search
//! unavailable, yet the project contract is that lexical search *fail-opens*
//! and semantic/model acquisition is opt-in. Today status collapses every
//! one of those situations into a vague "semantic unavailable", so an agent
//! cannot tell "operator disabled it" from "model never downloaded" from
//! "vectors are stale against the current DB" from "backfill is mid-flight".
//!
//! This module defines the single precise [`SemanticReadinessReason`] every
//! surface reports, plus the derived [`SemanticReadinessReport`] carrying
//! the JSON fields the report calls for: `available`, `fallback_mode`,
//! `quality_tier` / `fast_tier` readiness, `semantic_only_search_available`,
//! `state_detail`, `next_step`, and the realized search refinement.
//!
//! The classifier operates on an explicit [`SemanticSignals`] input (the
//! contract every surface populates) rather than reaching into the model
//! layer, so it is fully testable without downloading any model. All enums
//! serialize as snake_case, matching the readiness vocabulary in
//! [`crate::search::readiness`].

use serde::{Deserialize, Serialize};

use crate::search::readiness::SearchRefinementLevel;

/// The single precise reason semantic refinement is, or is not, available.
/// Ordered from "intentionally off" through acquisition/build problems to
/// "ready"; the classifier reports the first applicable reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SemanticReadinessReason {
    /// The operator explicitly disabled semantic search via policy.
    PolicyDisabled,
    /// A baseline build with semantic never configured (opt-in not taken);
    /// lexical-only by design, not a failure.
    BaselineNoSemantic,
    /// No embedding model has been acquired (no model dir / embedder).
    ModelNotAcquired,
    /// The model directory exists but required files are missing/incomplete.
    ModelFilesMissing,
    /// Model files are present but failed checksum verification.
    ChecksumMismatch,
    /// Model is ready but no vector index file has been built yet.
    VectorIndexMissing,
    /// A vector index exists but was built against a different DB
    /// generation (fingerprint mismatch) — stale, must not be trusted.
    DbFingerprintMismatch,
    /// Embeddings are actively backfilling; refinement will improve as it
    /// completes.
    BackfillInProgress,
    /// The fast tier is queryable; the quality tier is not yet published.
    FastTierReady,
    /// The quality tier is published and matches the current DB.
    QualityTierReady,
}

impl SemanticReadinessReason {
    /// Whether this reason means semantic refinement can contribute to a
    /// query right now (only the two ready tiers).
    pub(crate) fn is_available(self) -> bool {
        matches!(self, Self::FastTierReady | Self::QualityTierReady)
    }
}

/// Search fallback mode while semantic can't fully refine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FallbackMode {
    /// Full hybrid refinement is available; no fallback in effect.
    None,
    /// Search falls back to lexical-only results.
    Lexical,
}

/// The operator/agent next step to improve semantic readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SemanticNextStep {
    /// Nothing to do; quality tier is ready.
    None,
    /// Re-enable semantic search in policy/config.
    EnableSemanticPolicy,
    /// Acquire the embedding model (opt-in install).
    InstallModel,
    /// Re-acquire / repair the incomplete model files.
    RepairModelFiles,
    /// Re-download the model; checksum verification failed.
    ReacquireModelChecksumFailed,
    /// Build the vector index from the acquired model.
    BuildVectorIndex,
    /// Rebuild embeddings for the current DB generation (stale fingerprint).
    RebuildForCurrentDb,
    /// Wait for the in-progress backfill to converge.
    WaitForBackfill,
}

/// The signals a surface supplies to classify semantic readiness. This is
/// the stable contract; the model-acquisition layer and `SemanticAssetState`
/// adapter populate it. Kept minimal and `Copy` so fixtures are cheap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SemanticSignals {
    /// Semantic search is enabled by policy/config.
    pub policy_enabled: bool,
    /// This is a baseline build with semantic never configured.
    pub baseline_only: bool,
    /// An embedding model has been acquired (model dir + embedder present).
    pub model_present: bool,
    /// The acquired model's files are complete.
    pub model_files_complete: bool,
    /// The acquired model passed checksum verification.
    pub checksum_ok: bool,
    /// A vector index file exists on disk.
    pub vector_index_present: bool,
    /// Whether the vector index matches the current DB fingerprint.
    /// `None` when not yet evaluable.
    pub db_fingerprint_matches: Option<bool>,
    /// Embeddings are actively backfilling.
    pub backfill_in_progress: bool,
    /// The fast tier is queryable against the current DB.
    pub fast_tier_ready: bool,
    /// The quality tier is published and queryable against the current DB.
    pub quality_tier_ready: bool,
}

impl SemanticSignals {
    /// Classify the single precise reason from these signals, evaluated in
    /// priority order (intentional-off → acquisition → build → stale →
    /// backfill → ready).
    pub(crate) fn reason(&self) -> SemanticReadinessReason {
        use SemanticReadinessReason as R;
        if !self.policy_enabled {
            return R::PolicyDisabled;
        }
        if self.baseline_only {
            return R::BaselineNoSemantic;
        }
        if !self.model_present {
            return R::ModelNotAcquired;
        }
        if !self.model_files_complete {
            return R::ModelFilesMissing;
        }
        if !self.checksum_ok {
            return R::ChecksumMismatch;
        }
        // A queryable quality tier dominates: even if a fingerprint check is
        // pending elsewhere, a published+matching quality tier is ready.
        if self.quality_tier_ready {
            return R::QualityTierReady;
        }
        if !self.vector_index_present {
            return R::VectorIndexMissing;
        }
        if self.db_fingerprint_matches == Some(false) {
            return R::DbFingerprintMismatch;
        }
        if self.backfill_in_progress {
            return R::BackfillInProgress;
        }
        if self.fast_tier_ready {
            return R::FastTierReady;
        }
        // Index present, fingerprint not disproven, nothing ready yet and no
        // active backfill flag: treat as backfill-pending rather than ready.
        R::BackfillInProgress
    }

    /// Derive the full readiness report for these signals.
    pub(crate) fn report(&self) -> SemanticReadinessReport {
        use SemanticReadinessReason as R;
        let reason = self.reason();
        let available = reason.is_available();
        let quality_tier_ready = matches!(reason, R::QualityTierReady);

        let realized_refinement = if quality_tier_ready {
            SearchRefinementLevel::FullyHybridRefined
        } else if matches!(reason, R::FastTierReady) {
            SearchRefinementLevel::FastTierRefined
        } else {
            SearchRefinementLevel::LexicalOnly
        };

        // Full hybrid only when the quality tier is ready; otherwise search
        // serves correct lexical results while semantic catches up / is off.
        let fallback_mode = if quality_tier_ready {
            FallbackMode::None
        } else {
            FallbackMode::Lexical
        };

        let next_step = match reason {
            R::QualityTierReady => SemanticNextStep::None,
            R::PolicyDisabled => SemanticNextStep::EnableSemanticPolicy,
            R::BaselineNoSemantic | R::ModelNotAcquired => SemanticNextStep::InstallModel,
            R::ModelFilesMissing => SemanticNextStep::RepairModelFiles,
            R::ChecksumMismatch => SemanticNextStep::ReacquireModelChecksumFailed,
            R::VectorIndexMissing => SemanticNextStep::BuildVectorIndex,
            R::DbFingerprintMismatch => SemanticNextStep::RebuildForCurrentDb,
            R::BackfillInProgress => SemanticNextStep::WaitForBackfill,
            // Fast tier ready: quality refinement still improving via backfill.
            R::FastTierReady => SemanticNextStep::WaitForBackfill,
        };

        SemanticReadinessReport {
            reason,
            available,
            // `semantic --mode` can run whenever either tier is queryable.
            semantic_only_search_available: self.fast_tier_ready || self.quality_tier_ready,
            fallback_mode,
            fast_tier_ready: self.fast_tier_ready,
            quality_tier_ready,
            state_detail: reason.state_detail().to_string(),
            next_step,
            realized_refinement,
        }
    }
}

impl SemanticReadinessReason {
    /// A stable one-line human explanation for this reason.
    fn state_detail(self) -> &'static str {
        match self {
            Self::PolicyDisabled => "semantic search disabled by policy; lexical search only",
            Self::BaselineNoSemantic => {
                "baseline build without semantic configured; install a model to enable hybrid refinement"
            }
            Self::ModelNotAcquired => {
                "no embedding model acquired; lexical search works, hybrid refinement is opt-in"
            }
            Self::ModelFilesMissing => "embedding model files are incomplete; re-acquire the model",
            Self::ChecksumMismatch => "embedding model failed checksum verification; re-acquire it",
            Self::VectorIndexMissing => "model ready but no vector index built yet",
            Self::DbFingerprintMismatch => {
                "vector index is stale against the current database; rebuild embeddings"
            }
            Self::BackfillInProgress => {
                "semantic backfill in progress; hybrid refinement improves as it completes"
            }
            Self::FastTierReady => "fast semantic tier ready; quality tier still backfilling",
            Self::QualityTierReady => {
                "quality semantic tier ready; full hybrid refinement available"
            }
        }
    }
}

/// The derived semantic readiness report every status/health/triage/search
/// surface projects. Carries exactly the fields the .5.1 acceptance names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SemanticReadinessReport {
    pub reason: SemanticReadinessReason,
    /// `semantic.available`: hybrid refinement can contribute now.
    pub available: bool,
    pub semantic_only_search_available: bool,
    pub fallback_mode: FallbackMode,
    pub fast_tier_ready: bool,
    pub quality_tier_ready: bool,
    pub state_detail: String,
    pub next_step: SemanticNextStep,
    /// The search refinement a query would realize at this readiness.
    pub realized_refinement: SearchRefinementLevel,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully-ready quality-tier baseline; individual tests flip the one
    /// signal under test.
    fn ready() -> SemanticSignals {
        SemanticSignals {
            policy_enabled: true,
            baseline_only: false,
            model_present: true,
            model_files_complete: true,
            checksum_ok: true,
            vector_index_present: true,
            db_fingerprint_matches: Some(true),
            backfill_in_progress: false,
            fast_tier_ready: true,
            quality_tier_ready: true,
        }
    }

    #[test]
    fn enums_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&SemanticReadinessReason::DbFingerprintMismatch).unwrap(),
            "\"db_fingerprint_mismatch\""
        );
        assert_eq!(
            serde_json::to_string(&FallbackMode::Lexical).unwrap(),
            "\"lexical\""
        );
        assert_eq!(
            serde_json::to_string(&SemanticNextStep::ReacquireModelChecksumFailed).unwrap(),
            "\"reacquire_model_checksum_failed\""
        );
    }

    #[test]
    fn quality_tier_ready_is_fully_hybrid_with_no_fallback() {
        let r = ready().report();
        assert_eq!(r.reason, SemanticReadinessReason::QualityTierReady);
        assert!(r.available);
        assert!(r.quality_tier_ready);
        assert_eq!(r.fallback_mode, FallbackMode::None);
        assert_eq!(
            r.realized_refinement,
            SearchRefinementLevel::FullyHybridRefined
        );
        assert_eq!(r.next_step, SemanticNextStep::None);
        assert!(r.semantic_only_search_available);
    }

    #[test]
    fn policy_disabled_dominates_every_other_signal() {
        let mut s = ready();
        s.policy_enabled = false;
        let r = s.report();
        assert_eq!(r.reason, SemanticReadinessReason::PolicyDisabled);
        assert!(!r.available);
        assert_eq!(r.fallback_mode, FallbackMode::Lexical);
        assert_eq!(r.next_step, SemanticNextStep::EnableSemanticPolicy);
    }

    #[test]
    fn baseline_without_semantic_suggests_install() {
        let mut s = ready();
        s.baseline_only = true;
        let r = s.report();
        assert_eq!(r.reason, SemanticReadinessReason::BaselineNoSemantic);
        assert_eq!(r.next_step, SemanticNextStep::InstallModel);
        assert_eq!(r.realized_refinement, SearchRefinementLevel::LexicalOnly);
    }

    #[test]
    fn acquisition_chain_reasons_are_reported_in_priority_order() {
        // model not acquired
        let mut s = ready();
        s.model_present = false;
        assert_eq!(s.reason(), SemanticReadinessReason::ModelNotAcquired);
        assert_eq!(s.report().next_step, SemanticNextStep::InstallModel);

        // model present but files incomplete
        let mut s = ready();
        s.model_files_complete = false;
        assert_eq!(s.reason(), SemanticReadinessReason::ModelFilesMissing);
        assert_eq!(s.report().next_step, SemanticNextStep::RepairModelFiles);

        // files complete but checksum failed
        let mut s = ready();
        s.checksum_ok = false;
        assert_eq!(s.reason(), SemanticReadinessReason::ChecksumMismatch);
        assert_eq!(
            s.report().next_step,
            SemanticNextStep::ReacquireModelChecksumFailed
        );
    }

    #[test]
    fn vector_index_missing_when_model_ok_but_no_index() {
        let mut s = ready();
        s.quality_tier_ready = false;
        s.fast_tier_ready = false;
        s.vector_index_present = false;
        let r = s.report();
        assert_eq!(r.reason, SemanticReadinessReason::VectorIndexMissing);
        assert_eq!(r.next_step, SemanticNextStep::BuildVectorIndex);
        assert!(!r.available);
        assert!(!r.semantic_only_search_available);
    }

    #[test]
    fn stale_fingerprint_is_flagged_and_must_rebuild() {
        let mut s = ready();
        s.quality_tier_ready = false;
        s.fast_tier_ready = false;
        s.db_fingerprint_matches = Some(false);
        let r = s.report();
        assert_eq!(r.reason, SemanticReadinessReason::DbFingerprintMismatch);
        assert_eq!(r.next_step, SemanticNextStep::RebuildForCurrentDb);
    }

    #[test]
    fn backfill_in_progress_waits() {
        let mut s = ready();
        s.quality_tier_ready = false;
        s.fast_tier_ready = false;
        s.backfill_in_progress = true;
        let r = s.report();
        assert_eq!(r.reason, SemanticReadinessReason::BackfillInProgress);
        assert_eq!(r.next_step, SemanticNextStep::WaitForBackfill);
    }

    #[test]
    fn fast_tier_ready_serves_search_while_quality_backfills() {
        let mut s = ready();
        s.quality_tier_ready = false;
        let r = s.report();
        assert_eq!(r.reason, SemanticReadinessReason::FastTierReady);
        assert!(r.available);
        assert!(r.semantic_only_search_available);
        assert_eq!(r.fallback_mode, FallbackMode::Lexical);
        assert_eq!(
            r.realized_refinement,
            SearchRefinementLevel::FastTierRefined
        );
        assert_eq!(r.next_step, SemanticNextStep::WaitForBackfill);
    }

    #[test]
    fn report_round_trips_through_json_with_expected_fields() {
        let r = ready().report();
        let json = serde_json::to_string(&r).unwrap();
        for needle in [
            "\"reason\":\"quality_tier_ready\"",
            "\"available\":true",
            "\"semantic_only_search_available\":true",
            "\"fallback_mode\":\"none\"",
            "\"next_step\":\"none\"",
            "\"realized_refinement\":\"fully_hybrid_refined\"",
        ] {
            assert!(json.contains(needle), "missing {needle} in {json}");
        }
        let parsed: SemanticReadinessReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn every_reason_is_reachable_from_some_signal_set() {
        use SemanticReadinessReason as R;
        let reached: std::collections::BTreeSet<R> = [
            {
                let mut s = ready();
                s.policy_enabled = false;
                s.reason()
            },
            {
                let mut s = ready();
                s.baseline_only = true;
                s.reason()
            },
            {
                let mut s = ready();
                s.model_present = false;
                s.reason()
            },
            {
                let mut s = ready();
                s.model_files_complete = false;
                s.reason()
            },
            {
                let mut s = ready();
                s.checksum_ok = false;
                s.reason()
            },
            {
                let mut s = ready();
                s.quality_tier_ready = false;
                s.fast_tier_ready = false;
                s.vector_index_present = false;
                s.reason()
            },
            {
                let mut s = ready();
                s.quality_tier_ready = false;
                s.fast_tier_ready = false;
                s.db_fingerprint_matches = Some(false);
                s.reason()
            },
            {
                let mut s = ready();
                s.quality_tier_ready = false;
                s.fast_tier_ready = false;
                s.backfill_in_progress = true;
                s.reason()
            },
            {
                let mut s = ready();
                s.quality_tier_ready = false;
                s.reason()
            },
            ready().reason(),
        ]
        .into_iter()
        .collect();
        assert_eq!(reached.len(), 10, "all ten reasons must be reachable");
    }
}
