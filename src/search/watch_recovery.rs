// Dead-code tolerated module-wide: this OOM-recovery decision model lands
// ahead of the watch/index restart path that will consult it. Downstream
// bead .11.5 (integrated golden + E2E gate) exercises it.
#![allow(dead_code)]

//! Checkpointed watch/index OOM recovery decisions (bead
//! cass-fleet-resilience-20260608-uojcg.4.3).
//!
//! Issue #248 showed `cass index --watch`, after an OOM-kill, repeatedly
//! selecting a full `deferred_authoritative_db_rebuild` — triggered by a
//! *sparse Tantivy detection* — even when a checkpoint or a trusted lexical
//! index already existed. That turned a transient OOM into an expensive
//! restart loop with no evidence that a full rebuild was actually required.
//!
//! This module makes the restart choose a **bounded** recovery path from
//! explicit signals, and only falls back to a full rebuild when there is
//! genuinely nothing trusted to resume from. It also guards against the
//! restart loop: after repeated restarts with no forward progress it halts
//! and asks for an operator instead of rebuilding again.
//!
//! [`RecoverySignals::plan`] returns a [`RecoveryPlan`] recording the
//! decision, *why* a rebuild is or isn't needed, which derived assets are
//! trusted, and what work remains — the clear status the report asks for.
//! Pure inputs, so every case is unit-testable without a watch loop. Enums
//! serialize as snake_case.

use serde::{Deserialize, Serialize};

/// The default number of consecutive no-progress restarts after which
/// recovery halts instead of rebuilding again.
pub(crate) const DEFAULT_MAX_RESTART_ATTEMPTS: u32 = 3;

/// The chosen recovery path after an OOM/killed watch or index job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecoveryDecision {
    /// A valid checkpoint matches the current DB; resume from it (bounded).
    ResumeFromCheckpoint,
    /// The canonical DB and prior derived assets are trusted but the lexical
    /// stage is incomplete (or only sparse); do a bounded lexical repair,
    /// NOT a full rebuild.
    BoundedLexicalRepair,
    /// Nothing trusted to resume from (no checkpoint, no trusted assets); a
    /// full rebuild is genuinely justified — with recorded evidence.
    FullRebuild,
    /// Repeated restarts made no forward progress; halt and require operator
    /// inspection instead of looping into another rebuild.
    HaltRestartLoop,
}

impl RecoveryDecision {
    /// Whether this decision performs bounded work (vs a full rebuild or a
    /// halt).
    pub(crate) fn is_bounded(self) -> bool {
        matches!(
            self,
            Self::ResumeFromCheckpoint | Self::BoundedLexicalRepair
        )
    }
}

/// Signals the restart path supplies about what survived the OOM/kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecoverySignals {
    /// A checkpoint record exists.
    pub checkpoint_present: bool,
    /// The checkpoint matches the current DB generation. `None` = unknown
    /// (treated conservatively as not-matching).
    pub checkpoint_db_matches: Option<bool>,
    /// The lexical stage had completed before the kill.
    pub lexical_stage_completed: bool,
    /// An existing lexical index is byte-trusted against the current DB.
    pub derived_assets_trusted: bool,
    /// A sparse-metadata (sparse Tantivy) condition was detected (#248). On
    /// its own this must NOT force a full rebuild.
    pub sparse_metadata_detected: bool,
    /// Consecutive prior restart attempts in this recovery loop.
    pub restart_attempts: u32,
    /// Whether any forward progress happened since the last restart.
    pub forward_progress_since_last_restart: bool,
    /// The loop-guard threshold (see [`DEFAULT_MAX_RESTART_ATTEMPTS`]).
    pub max_restart_attempts: u32,
}

impl Default for RecoverySignals {
    fn default() -> Self {
        Self {
            checkpoint_present: false,
            checkpoint_db_matches: None,
            lexical_stage_completed: false,
            derived_assets_trusted: false,
            sparse_metadata_detected: false,
            restart_attempts: 0,
            forward_progress_since_last_restart: false,
            max_restart_attempts: DEFAULT_MAX_RESTART_ATTEMPTS,
        }
    }
}

impl RecoverySignals {
    fn checkpoint_usable(&self) -> bool {
        self.checkpoint_present && self.checkpoint_db_matches == Some(true)
    }

    /// True when the loop guard should trip: too many restarts and no
    /// forward progress to show for them.
    fn loop_guard_tripped(&self) -> bool {
        self.restart_attempts >= self.max_restart_attempts
            && !self.forward_progress_since_last_restart
    }

    /// Decide the recovery path and explain it.
    pub(crate) fn plan(&self) -> RecoveryPlan {
        // 1. Loop guard dominates: never rebuild again on a stuck loop.
        if self.loop_guard_tripped() {
            return RecoveryPlan {
                decision: RecoveryDecision::HaltRestartLoop,
                reason: format!(
                    "{} restarts with no forward progress; halting to avoid a rebuild loop (operator inspection required)",
                    self.restart_attempts
                ),
                derived_assets_trusted: self.derived_assets_trusted,
                rebuild_required: false,
                work_remaining: WorkRemaining::OperatorInspection,
            };
        }

        // 2. A checkpoint that matches the current DB is the cheapest, safest
        //    resume.
        if self.checkpoint_usable() {
            return RecoveryPlan {
                decision: RecoveryDecision::ResumeFromCheckpoint,
                reason:
                    "a checkpoint matching the current database exists; resume bounded work from it"
                        .to_string(),
                derived_assets_trusted: self.derived_assets_trusted,
                rebuild_required: false,
                work_remaining: WorkRemaining::ResumeCheckpoint,
            };
        }

        // 3. Trusted derived assets: a sparse-metadata detection or an
        //    incomplete lexical stage warrants a BOUNDED repair, not a full
        //    rebuild (the #248 fix — sparse detection alone is not evidence
        //    that the whole index is gone).
        if self.derived_assets_trusted
            && (self.sparse_metadata_detected || !self.lexical_stage_completed)
        {
            let why = if self.sparse_metadata_detected {
                "sparse lexical metadata detected but derived assets are trusted against the current DB; bounded lexical repair, not a full rebuild"
            } else {
                "lexical stage incomplete but derived assets are trusted; bounded lexical repair"
            };
            return RecoveryPlan {
                decision: RecoveryDecision::BoundedLexicalRepair,
                reason: why.to_string(),
                derived_assets_trusted: true,
                rebuild_required: false,
                work_remaining: WorkRemaining::BoundedLexicalRepair,
            };
        }

        // 4. Trusted assets and a completed lexical stage with no sparse
        //    flag: nothing to recover — already converged.
        if self.derived_assets_trusted && self.lexical_stage_completed {
            return RecoveryPlan {
                decision: RecoveryDecision::ResumeFromCheckpoint,
                reason:
                    "derived assets are trusted and the lexical stage completed; no rebuild needed"
                        .to_string(),
                derived_assets_trusted: true,
                rebuild_required: false,
                work_remaining: WorkRemaining::None,
            };
        }

        // 5. Nothing trusted and no usable checkpoint: a full rebuild is
        //    genuinely justified, with recorded evidence.
        RecoveryPlan {
            decision: RecoveryDecision::FullRebuild,
            reason:
                "no usable checkpoint and no trusted derived assets; a full rebuild is required"
                    .to_string(),
            derived_assets_trusted: false,
            rebuild_required: true,
            work_remaining: WorkRemaining::FullRebuild,
        }
    }
}

/// What work remains after the recovery decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkRemaining {
    None,
    ResumeCheckpoint,
    BoundedLexicalRepair,
    FullRebuild,
    OperatorInspection,
}

/// The recovery plan: decision plus the evidence the report requires (why a
/// rebuild is needed, trusted assets, work remaining).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecoveryPlan {
    pub decision: RecoveryDecision,
    pub reason: String,
    pub derived_assets_trusted: bool,
    pub rebuild_required: bool,
    pub work_remaining: WorkRemaining,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trusted, converged baseline; tests flip one signal at a time.
    fn trusted_converged() -> RecoverySignals {
        RecoverySignals {
            derived_assets_trusted: true,
            lexical_stage_completed: true,
            ..RecoverySignals::default()
        }
    }

    #[test]
    fn enums_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&RecoveryDecision::ResumeFromCheckpoint).unwrap(),
            "\"resume_from_checkpoint\""
        );
        assert_eq!(
            serde_json::to_string(&WorkRemaining::OperatorInspection).unwrap(),
            "\"operator_inspection\""
        );
    }

    #[test]
    fn oom_before_publish_with_matching_checkpoint_resumes() {
        // OOM before the publish stage, but a checkpoint matching the current
        // DB survived: resume from it, no rebuild.
        let s = RecoverySignals {
            checkpoint_present: true,
            checkpoint_db_matches: Some(true),
            lexical_stage_completed: false,
            ..RecoverySignals::default()
        };
        let plan = s.plan();
        assert_eq!(plan.decision, RecoveryDecision::ResumeFromCheckpoint);
        assert!(!plan.rebuild_required);
        assert!(plan.decision.is_bounded());
    }

    #[test]
    fn oom_after_lexical_stage_with_trusted_assets_needs_no_rebuild() {
        let plan = trusted_converged().plan();
        assert!(!plan.rebuild_required);
        assert_eq!(plan.work_remaining, WorkRemaining::None);
    }

    #[test]
    fn sparse_metadata_with_trusted_assets_does_bounded_repair_not_full_rebuild() {
        // The #248 regression: sparse Tantivy detection must NOT trigger a
        // full rebuild when derived assets are actually trusted.
        let mut s = trusted_converged();
        s.sparse_metadata_detected = true;
        let plan = s.plan();
        assert_eq!(plan.decision, RecoveryDecision::BoundedLexicalRepair);
        assert!(
            !plan.rebuild_required,
            "sparse detection must not force a full rebuild"
        );
        assert!(plan.decision.is_bounded());
    }

    #[test]
    fn no_checkpoint_and_no_trusted_assets_justifies_full_rebuild_with_evidence() {
        let s = RecoverySignals::default();
        let plan = s.plan();
        assert_eq!(plan.decision, RecoveryDecision::FullRebuild);
        assert!(plan.rebuild_required);
        assert!(!plan.reason.is_empty(), "a full rebuild must record why");
        assert_eq!(plan.work_remaining, WorkRemaining::FullRebuild);
    }

    #[test]
    fn restart_loop_is_halted_instead_of_rebuilding_again() {
        // Even with nothing trusted (which would otherwise full-rebuild), a
        // stuck loop must halt rather than rebuild a fourth time.
        let s = RecoverySignals {
            restart_attempts: 3,
            forward_progress_since_last_restart: false,
            max_restart_attempts: 3,
            ..RecoverySignals::default()
        };
        let plan = s.plan();
        assert_eq!(plan.decision, RecoveryDecision::HaltRestartLoop);
        assert!(!plan.rebuild_required, "must not rebuild on a stuck loop");
        assert_eq!(plan.work_remaining, WorkRemaining::OperatorInspection);
    }

    #[test]
    fn restart_attempts_with_forward_progress_do_not_trip_the_guard() {
        let s = RecoverySignals {
            checkpoint_present: true,
            checkpoint_db_matches: Some(true),
            restart_attempts: 5,
            forward_progress_since_last_restart: true,
            ..RecoverySignals::default()
        };
        // Progress is being made, so resume rather than halt.
        assert_eq!(s.plan().decision, RecoveryDecision::ResumeFromCheckpoint);
    }

    #[test]
    fn unknown_checkpoint_match_is_not_trusted_for_resume() {
        let s = RecoverySignals {
            checkpoint_present: true,
            checkpoint_db_matches: None,
            ..RecoverySignals::default()
        };
        // Unknown match + nothing trusted -> full rebuild, not a blind resume.
        assert_eq!(s.plan().decision, RecoveryDecision::FullRebuild);
    }

    #[test]
    fn plan_round_trips_through_json_with_clear_status_fields() {
        let mut s = trusted_converged();
        s.sparse_metadata_detected = true;
        let plan = s.plan();
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"decision\":\"bounded_lexical_repair\""));
        assert!(json.contains("\"rebuild_required\":false"));
        assert!(json.contains("\"work_remaining\":\"bounded_lexical_repair\""));
        let parsed: RecoveryPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, plan);
    }
}
