// Dead-code tolerated module-wide: this publish-safety validator lands
// ahead of the semantic backfill/publish path that will gate on it.
// Downstream beads (.5.4 truthful hybrid fallback, .5.5 model-acquisition
// hardening) and the backfill loop consume these verdicts.
#![allow(dead_code)]

//! Semantic checkpoint resume + partial-publish safety (bead
//! cass-fleet-resilience-20260608-uojcg.5.3).
//!
//! The hard correctness rule: a vector tier must **never** be published if
//! it would lie about its DB coverage. A tier built against an older DB
//! generation, or one whose backfill was interrupted, or whose index/HNSW
//! files are orphaned, must not be advertised as queryable — otherwise
//! `--mode semantic` silently returns stale or partial results that an agent
//! trusts as complete.
//!
//! This module derives two pure things from checkpoint/tier signals:
//! 1. [`PublishSafety`] — whether a partial tier is safe to publish now, and
//!    if not, exactly why (so the caller fails closed instead of guessing).
//! 2. [`CheckpointResume`] — where a backfill should resume, preserving
//!    whole-conversation boundaries so resume never splits a conversation's
//!    embeddings.
//!
//! Both operate on an explicit [`CheckpointSignals`] input so every case —
//! interrupted backfill, DB mutation mid-backfill, checkpoint resume,
//! fingerprint mismatch, partial tier availability, publish failure, and
//! orphaned vectors — is unit-testable without a model or DB. Enums
//! serialize as snake_case.

use serde::{Deserialize, Serialize};

/// Whether a partial vector tier is safe to publish, and why not when it is
/// not. The caller must fail closed on any `Unsafe*` verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublishSafety {
    /// The tier is complete, matches the current DB, has no orphans, and the
    /// publish step succeeded: safe to advertise as queryable.
    SafeToPublish,
    /// The vector tier was built against a different DB generation; it would
    /// lie about coverage. Rebuild before publishing.
    UnsafeFingerprintMismatch,
    /// The backfill did not finish; publishing would advertise partial
    /// coverage as complete.
    UnsafeIncompleteCoverage,
    /// Index/HNSW files are present without a matching completed checkpoint
    /// (or vice versa) — orphaned artifacts; do not publish.
    UnsafeOrphanArtifacts,
    /// The publish/commit step itself failed; the prior tier must remain
    /// authoritative.
    UnsafePublishFailed,
}

impl PublishSafety {
    pub(crate) fn is_safe(self) -> bool {
        matches!(self, Self::SafeToPublish)
    }
}

/// Where a backfill should resume. `resume_from_offset` is the last
/// conversation-safe offset; `restart_from_zero` is set when no usable
/// checkpoint exists (or the checkpoint is for a stale DB) so resuming would
/// be unsafe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CheckpointResume {
    /// Conversation-safe offset to resume from (0 when restarting).
    pub resume_from_offset: i64,
    /// True when the backfill must restart from scratch rather than resume.
    pub restart_from_zero: bool,
    /// Whether the resume offset preserves a whole-conversation boundary.
    pub conversation_boundary_preserved: bool,
}

/// The signals a backfill/publish step supplies. Mirrors the fields on
/// `SemanticCheckpointProgressState`/`SemanticTierAssetState` but is a small,
/// explicit `Copy` contract so cases are cheap to test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CheckpointSignals {
    /// A checkpoint record exists for this tier.
    pub checkpoint_present: bool,
    /// The checkpoint's coverage is complete (`conversations_processed >=
    /// total`).
    pub checkpoint_completed: bool,
    /// The checkpoint matches the current DB generation. `None` when not yet
    /// evaluated (treated conservatively as a mismatch for publish).
    pub checkpoint_db_matches: Option<bool>,
    /// The DB was mutated after the checkpoint was taken (invalidates it).
    pub db_mutated_since_checkpoint: bool,
    /// The last conversation-safe offset recorded by the checkpoint.
    pub last_safe_offset: Option<i64>,
    /// Vector/HNSW index files exist on disk.
    pub index_files_present: bool,
    /// The publish/commit step completed successfully (only meaningful when
    /// attempting a publish).
    pub publish_succeeded: bool,
}

impl CheckpointSignals {
    /// True only when the checkpoint definitively matches the current DB and
    /// the DB has not mutated since.
    fn db_coverage_current(&self) -> bool {
        self.checkpoint_db_matches == Some(true) && !self.db_mutated_since_checkpoint
    }

    /// Decide whether the tier is safe to publish. Fails closed: any
    /// uncertainty (unknown fingerprint, mid-mutation, incomplete, orphaned,
    /// failed publish) yields a specific `Unsafe*` verdict.
    pub(crate) fn publish_safety(&self) -> PublishSafety {
        // A failed publish step is authoritative: never advertise the new
        // tier.
        if !self.publish_succeeded {
            return PublishSafety::UnsafePublishFailed;
        }
        // Orphan handling: index files without a completed checkpoint, or a
        // completed checkpoint without index files, are inconsistent.
        if self.index_files_present != self.checkpoint_completed {
            return PublishSafety::UnsafeOrphanArtifacts;
        }
        // Coverage must be complete to publish as a ready tier.
        if !self.checkpoint_present || !self.checkpoint_completed {
            return PublishSafety::UnsafeIncompleteCoverage;
        }
        // And it must match the current DB generation, with no mutation since.
        if !self.db_coverage_current() {
            return PublishSafety::UnsafeFingerprintMismatch;
        }
        PublishSafety::SafeToPublish
    }

    /// Compute the safe resume point for an interrupted backfill. Resuming is
    /// only safe when a checkpoint exists, still matches the current DB, and
    /// recorded a conversation-safe offset; otherwise restart from zero.
    pub(crate) fn resume_point(&self) -> CheckpointResume {
        if !self.checkpoint_present || !self.db_coverage_current() {
            // No usable checkpoint, or it is stale against the current DB:
            // resuming would embed against shifted data. Restart cleanly.
            return CheckpointResume {
                resume_from_offset: 0,
                restart_from_zero: true,
                conversation_boundary_preserved: true,
            };
        }
        match self.last_safe_offset {
            Some(offset) if offset >= 0 => CheckpointResume {
                resume_from_offset: offset,
                restart_from_zero: false,
                conversation_boundary_preserved: true,
            },
            // Checkpoint present and DB-current but no recorded safe offset:
            // we cannot prove a conversation boundary, so restart.
            _ => CheckpointResume {
                resume_from_offset: 0,
                restart_from_zero: true,
                conversation_boundary_preserved: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully-ready, DB-current, completed checkpoint with index files and a
    /// successful publish. Tests flip one signal at a time.
    fn ready() -> CheckpointSignals {
        CheckpointSignals {
            checkpoint_present: true,
            checkpoint_completed: true,
            checkpoint_db_matches: Some(true),
            db_mutated_since_checkpoint: false,
            last_safe_offset: Some(4_096),
            index_files_present: true,
            publish_succeeded: true,
        }
    }

    #[test]
    fn enums_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&PublishSafety::UnsafeFingerprintMismatch).unwrap(),
            "\"unsafe_fingerprint_mismatch\""
        );
    }

    #[test]
    fn ready_tier_is_safe_to_publish() {
        assert_eq!(ready().publish_safety(), PublishSafety::SafeToPublish);
        assert!(ready().publish_safety().is_safe());
    }

    #[test]
    fn interrupted_backfill_is_incomplete_coverage_and_not_publishable() {
        let mut s = ready();
        s.checkpoint_completed = false;
        s.index_files_present = false; // consistent: no completed index yet
        assert_eq!(s.publish_safety(), PublishSafety::UnsafeIncompleteCoverage);
    }

    #[test]
    fn db_mutation_during_backfill_blocks_publish_as_fingerprint_mismatch() {
        let mut s = ready();
        s.db_mutated_since_checkpoint = true;
        assert_eq!(s.publish_safety(), PublishSafety::UnsafeFingerprintMismatch);
    }

    #[test]
    fn fingerprint_mismatch_blocks_publish() {
        let mut s = ready();
        s.checkpoint_db_matches = Some(false);
        assert_eq!(s.publish_safety(), PublishSafety::UnsafeFingerprintMismatch);
        // Unknown fingerprint is treated conservatively as not-current too.
        let mut s = ready();
        s.checkpoint_db_matches = None;
        assert_eq!(s.publish_safety(), PublishSafety::UnsafeFingerprintMismatch);
    }

    #[test]
    fn orphaned_artifacts_are_detected_both_directions() {
        // Index files present but checkpoint not completed.
        let mut s = ready();
        s.checkpoint_completed = false;
        s.index_files_present = true;
        assert_eq!(s.publish_safety(), PublishSafety::UnsafeOrphanArtifacts);

        // Completed checkpoint but index files missing.
        let mut s = ready();
        s.checkpoint_completed = true;
        s.index_files_present = false;
        assert_eq!(s.publish_safety(), PublishSafety::UnsafeOrphanArtifacts);
    }

    #[test]
    fn publish_failure_keeps_prior_tier_authoritative() {
        let mut s = ready();
        s.publish_succeeded = false;
        assert_eq!(s.publish_safety(), PublishSafety::UnsafePublishFailed);
    }

    #[test]
    fn checkpoint_resume_uses_last_safe_offset_when_db_current() {
        let r = ready().resume_point();
        assert!(!r.restart_from_zero);
        assert_eq!(r.resume_from_offset, 4_096);
        assert!(r.conversation_boundary_preserved);
    }

    #[test]
    fn resume_restarts_from_zero_when_checkpoint_is_stale() {
        let mut s = ready();
        s.checkpoint_completed = false; // mid-backfill
        s.db_mutated_since_checkpoint = true; // ...and DB shifted under it
        let r = s.resume_point();
        assert!(
            r.restart_from_zero,
            "stale checkpoint must restart, not resume"
        );
        assert_eq!(r.resume_from_offset, 0);
    }

    #[test]
    fn resume_restarts_when_no_checkpoint_or_no_safe_offset() {
        let mut s = ready();
        s.checkpoint_present = false;
        assert!(s.resume_point().restart_from_zero);

        let mut s = ready();
        s.last_safe_offset = None;
        assert!(
            s.resume_point().restart_from_zero,
            "no recorded conversation-safe offset means we cannot resume safely"
        );
    }

    #[test]
    fn partial_tier_mid_backfill_resumes_from_its_offset_without_publishing() {
        // Fast tier still catching up: not publishable, but a clean DB-current
        // checkpoint can resume from its offset.
        let mut s = ready();
        s.checkpoint_completed = false;
        s.index_files_present = false;
        s.last_safe_offset = Some(1_000);
        assert_eq!(s.publish_safety(), PublishSafety::UnsafeIncompleteCoverage);
        let r = s.resume_point();
        assert!(!r.restart_from_zero);
        assert_eq!(r.resume_from_offset, 1_000);
    }

    #[test]
    fn resume_round_trips_through_json() {
        let r = ready().resume_point();
        let json = serde_json::to_string(&r).unwrap();
        let parsed: CheckpointResume = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, r);
    }
}
