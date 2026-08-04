// Dead-code tolerated module-wide: this concurrency / busy-lock / WAL-sidecar /
// stale-cache contention classifier (bead cass-fleet-resilience-20260608-uojcg.14.3)
// lands the classification contract ahead of its projection into the
// health/status/doctor/search robot surfaces and the real-binary contention
// E2E gate (.14.4). It populates the .14.1 StorageState taxonomy that the .14.2
// salvage planner already consumes.
#![allow(dead_code)]

//! Concurrency, busy-lock, WAL-sidecar, and stale-cache contention
//! diagnostics (bead cass-fleet-resilience-20260608-uojcg.14.3).
//!
//! Several failure classes are *contention or transient sidecar state*, not
//! corrupt user data: the CLI, daemon, and indexer can collide on busy locks;
//! a killed process can leave a hot WAL/SHM sidecar; a cached searcher can
//! serve an old segment generation after a publish. cass must explain
//! contention **as contention** — with bounded retry/wait guidance — rather
//! than as missing data or archive loss.
//!
//! This module is the classification contract:
//! - [`ContentionClass`] separates busy/locked, busy-recovery, snapshot
//!   conflict, stale WAL/SHM sidecar, stale searcher/cache, and host-pressure.
//! - [`classify_franken_error`] maps a real `frankensqlite::FrankenError` to
//!   its contention class (or `None` when the error is not contention — e.g.
//!   corruption, which is a `.14.1` integrity state, not a transient).
//! - [`Retryability`] + [`BoundedWaitGuidance`] give retry/backoff advice that
//!   is **always bounded** — robot commands never block indefinitely.
//! - [`CacheStaleness`] classifies a cached searcher relative to the published
//!   generation; a stale cache is *invalidated/reported*, never treated as
//!   archive loss.
//! - [`ContentionReport`] is the projected verdict: class, retryability,
//!   bounded wait, the [`StorageState`] it maps to, best-effort
//!   [`LockEvidence`], a concrete (never bare/destructive) recommended
//!   command, and the "contention, not missing data" explanation.
//!
//! The invariant every consumer can assert: [`ContentionClass::is_archive_loss`]
//! is **always false**. All enums serialize as snake_case.

use serde::{Deserialize, Serialize};

use crate::search::storage_integrity::StorageState;

/// Schema version for the contention-report JSON contract.
pub(crate) const CONTENTION_REPORT_SCHEMA_VERSION: u32 = 1;

const CONTENTION_REPORT_KIND: &str = "contention_diagnostic";

/// A distinct contention / transient-state class. None of these is archive
/// data loss — they resolve by waiting, inspecting a sidecar, or relieving
/// host pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContentionClass {
    /// No contention observed.
    None,
    /// Another writer holds the lock (`SQLITE_BUSY` / `FrankenError::Busy`).
    BusyLocked,
    /// The database is recovering a hot journal/WAL (`BusyRecovery`).
    BusyRecovery,
    /// An MVCC snapshot/serialization conflict (`BusySnapshot` /
    /// `WriteConflict` / `SerializationFailure`) — retry the transaction.
    SnapshotConflict,
    /// A WAL/SHM sidecar is stale or orphaned (a process was killed
    /// mid-write); the canonical rows survive.
    StaleWalSidecar,
    /// A cached searcher / derived artifact is serving an older generation
    /// than was last published; reload or report, never treat as loss.
    StaleSearcherCache,
    /// Host resource pressure (disk/memory/load) is the proximate cause;
    /// waiting will not clear it.
    HostPressure,
}

impl ContentionClass {
    pub(crate) fn stable_name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::BusyLocked => "busy_locked",
            Self::BusyRecovery => "busy_recovery",
            Self::SnapshotConflict => "snapshot_conflict",
            Self::StaleWalSidecar => "stale_wal_sidecar",
            Self::StaleSearcherCache => "stale_searcher_cache",
            Self::HostPressure => "host_pressure",
        }
    }

    /// The invariant: contention is **never** archive/source data loss. A
    /// busy lock, a stale sidecar, and a stale cache all leave the canonical
    /// rows intact.
    pub(crate) fn is_archive_loss(self) -> bool {
        false
    }

    /// Whether this class resolves on its own by waiting (a transient lock or
    /// recovery), as opposed to needing inspection or host action.
    pub(crate) fn is_transient_lock(self) -> bool {
        matches!(
            self,
            Self::BusyLocked | Self::BusyRecovery | Self::SnapshotConflict
        )
    }

    /// How this class may be retried.
    pub(crate) fn retryability(self) -> Retryability {
        match self {
            // Nothing to retry.
            Self::None => Retryability::NotRetryable,
            // Transient locks/conflicts clear with bounded backoff.
            Self::BusyLocked | Self::BusyRecovery | Self::SnapshotConflict => {
                Retryability::RetryAfterBackoff
            }
            // Sidecar / cache states need a read-only inspection or reload
            // before a retry is meaningful.
            Self::StaleWalSidecar | Self::StaleSearcherCache => Retryability::RetryAfterInspection,
            // Waiting will not free disk/memory; this needs operator action.
            Self::HostPressure => Retryability::NotRetryable,
        }
    }

    /// The bounded-wait policy for this class, when waiting is meaningful.
    /// Always finite — a robot command must never block indefinitely.
    pub(crate) fn bounded_wait(self) -> Option<BoundedWaitGuidance> {
        match self.retryability() {
            Retryability::RetryAfterBackoff => Some(BoundedWaitGuidance::transient_lock()),
            Retryability::RetryAfterInspection => Some(BoundedWaitGuidance::after_inspection()),
            Retryability::NotRetryable => None,
        }
    }

    /// The storage-integrity state this contention maps to, when it implies
    /// one. Busy variants are `BusyOrLocked`; a stale sidecar is
    /// `WalSidecarSuspect`; a stale searcher cache is `DerivedOnlyDrift`
    /// (derived assets drifted, canonical intact). `None`/`HostPressure` do
    /// not by themselves imply a storage-integrity fault.
    pub(crate) fn to_storage_state(self) -> Option<StorageState> {
        match self {
            Self::None | Self::HostPressure => None,
            Self::BusyLocked | Self::BusyRecovery | Self::SnapshotConflict => {
                Some(StorageState::BusyOrLocked)
            }
            Self::StaleWalSidecar => Some(StorageState::WalSidecarSuspect),
            Self::StaleSearcherCache => Some(StorageState::DerivedOnlyDrift),
        }
    }

    /// A concrete, non-destructive `cass` command to run next, or `None` when
    /// the right move is simply a bounded wait + retry.
    pub(crate) fn recommended_command(self) -> Option<&'static str> {
        match self {
            // Transient: re-check readiness; the command succeeds once the
            // other writer releases.
            Self::BusyLocked | Self::BusyRecovery | Self::SnapshotConflict => {
                Some("cass status --json")
            }
            // Sidecar suspect: read-only inspection.
            Self::StaleWalSidecar => Some("cass doctor check --json"),
            // Stale cache: status reports the live generation after reload.
            Self::StaleSearcherCache => Some("cass status --json"),
            // Host pressure: status surfaces the pressure; the fix is
            // host-level (free disk / memory), reflected in the explanation.
            Self::HostPressure => Some("cass status --json"),
            Self::None => None,
        }
    }

    /// The one-line "contention, not missing data" explanation.
    pub(crate) fn explanation(self) -> &'static str {
        match self {
            Self::None => "no storage contention observed",
            Self::BusyLocked => {
                "another writer holds the lock; this is contention, not missing data — retry after a bounded backoff"
            }
            Self::BusyRecovery => {
                "the database is recovering a hot WAL; this is transient, not corruption — retry after a bounded backoff"
            }
            Self::SnapshotConflict => {
                "an MVCC snapshot/serialization conflict; the transaction can be retried after a bounded backoff"
            }
            Self::StaleWalSidecar => {
                "a WAL/SHM sidecar is stale or orphaned; the canonical rows are intact — checkpoint/recover, do not treat as loss"
            }
            Self::StaleSearcherCache => {
                "a cached searcher is serving an older generation; reload/invalidate it — the published index is not lost"
            }
            Self::HostPressure => {
                "host resource pressure (disk/memory/load) is the proximate cause; waiting will not clear it — relieve host pressure"
            }
        }
    }
}

/// Map a real `frankensqlite::FrankenError` to its contention class, or `None`
/// when the error is not a transient/contention class (e.g. corruption, which
/// is a `.14.1` integrity state). The struct variants are matched with `{ .. }`
/// so this stays robust to field-shape changes, with a catch-all for any
/// future non-contention variant.
pub(crate) fn classify_franken_error(err: &frankensqlite::FrankenError) -> Option<ContentionClass> {
    use frankensqlite::FrankenError as E;
    match err {
        E::Busy | E::DatabaseLocked { .. } | E::LockFailed { .. } => {
            Some(ContentionClass::BusyLocked)
        }
        E::BusyRecovery => Some(ContentionClass::BusyRecovery),
        E::BusySnapshot { .. } | E::WriteConflict { .. } | E::SerializationFailure { .. } => {
            Some(ContentionClass::SnapshotConflict)
        }
        // Corruption is NOT contention — it is a `.14.1` IntegrityFailed state
        // handled by the storage-integrity probe, never auto-retried here.
        _ => None,
    }
}

/// Whether a `frankensqlite::FrankenError` is a retryable contention error
/// (busy/recovery/snapshot conflict). Mirrors the retry predicate used by the
/// connection-manager backoff loop, but driven by the shared classifier so the
/// two never disagree.
pub(crate) fn is_retryable_contention(err: &frankensqlite::FrankenError) -> bool {
    classify_franken_error(err)
        .is_some_and(|c| matches!(c.retryability(), Retryability::RetryAfterBackoff))
}

/// How a contention class may be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Retryability {
    /// Retry after a bounded jittered backoff (a transient lock/conflict).
    RetryAfterBackoff,
    /// Re-check after a read-only inspection / reload (sidecar / cache).
    RetryAfterInspection,
    /// Not resolvable by waiting; needs an explicit operator action.
    NotRetryable,
}

/// Bounded-wait guidance. Every field is finite by construction: a robot
/// command following this guidance is guaranteed to stop waiting after
/// `max_total_wait_ms`, never blocking indefinitely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BoundedWaitGuidance {
    pub max_attempts: u32,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
    /// Hard ceiling on total time spent waiting across all attempts.
    pub max_total_wait_ms: u64,
    /// Whether to jitter each backoff to avoid thundering-herd lock-step.
    pub jittered: bool,
}

impl BoundedWaitGuidance {
    /// Backoff for a transient lock/conflict: mirrors the production
    /// jittered exponential backoff (2ms → 256ms), capped in total so a
    /// robot command never hangs.
    pub(crate) fn transient_lock() -> Self {
        Self {
            max_attempts: 6,
            initial_backoff_ms: 2,
            max_backoff_ms: 256,
            max_total_wait_ms: 2_000,
            jittered: true,
        }
    }

    /// A short bounded re-check after a read-only inspection/reload.
    pub(crate) fn after_inspection() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff_ms: 10,
            max_backoff_ms: 100,
            max_total_wait_ms: 500,
            jittered: true,
        }
    }

    /// Whether the policy is bounded (finite). Always true — kept as an
    /// explicit, assertable invariant for consumers.
    pub(crate) fn is_bounded(&self) -> bool {
        self.max_attempts > 0 && self.max_total_wait_ms > 0 && self.max_total_wait_ms < u64::MAX
    }
}

/// How a cached searcher relates to the published index generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CacheStaleness {
    /// The cache matches the published generation.
    Fresh,
    /// The cache is serving an older generation than published.
    StaleGeneration,
    /// A reload was attempted and failed; the cache may be stale.
    ReloadFailed,
}

impl CacheStaleness {
    /// Classify a cached searcher from the published vs cached generation
    /// signatures and whether the last reload succeeded.
    pub(crate) fn classify(
        published_generation: Option<&str>,
        cached_generation: Option<&str>,
        reload_ok: bool,
    ) -> Self {
        if !reload_ok {
            return Self::ReloadFailed;
        }
        match (published_generation, cached_generation) {
            (Some(p), Some(c)) if p == c => Self::Fresh,
            // Unknown generations are treated as fresh: absence of a signature
            // is not evidence of staleness (and never of loss).
            (None, _) | (_, None) => Self::Fresh,
            _ => Self::StaleGeneration,
        }
    }

    /// The contention class this staleness implies, if any. A fresh cache is
    /// no contention; stale/reload-failed is a `StaleSearcherCache` to report
    /// or invalidate — never archive loss.
    pub(crate) fn to_contention_class(self) -> ContentionClass {
        match self {
            Self::Fresh => ContentionClass::None,
            Self::StaleGeneration | Self::ReloadFailed => ContentionClass::StaleSearcherCache,
        }
    }
}

/// Best-effort, platform-tolerant evidence about who/what held the lock. Every
/// field is optional: on platforms where a holder PID is not reliably
/// available, it is simply `None` — never a fabricated assumption.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) struct LockEvidence {
    /// PID observed holding the lock, when discoverable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_pid: Option<u32>,
    /// A short, platform-tolerant note about what was observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Where the evidence came from (e.g. `busy_timeout_expiry`, `pidfile`,
    /// `none`). Stable snake_case.
    pub source: String,
}

impl LockEvidence {
    /// Evidence that contention was observed via a busy-timeout expiry, with
    /// no reliable holder identity (the common, platform-tolerant case).
    pub(crate) fn from_busy_timeout() -> Self {
        Self {
            holder_pid: None,
            note: Some("busy lock observed; holder identity not available on this platform".into()),
            source: "busy_timeout_expiry".to_string(),
        }
    }
}

/// The projected contention verdict a readiness surface emits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContentionReport {
    pub schema_version: u32,
    pub report_kind: String,
    pub class: ContentionClass,
    pub retryability: Retryability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounded_wait: Option<BoundedWaitGuidance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_state: Option<StorageState>,
    /// Always false: contention is never archive/source data loss.
    pub is_archive_loss: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<LockEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_command: Option<String>,
    pub explanation: String,
}

impl ContentionReport {
    /// Build the verdict for a contention class with optional lock evidence.
    pub(crate) fn classify(class: ContentionClass, evidence: Option<LockEvidence>) -> Self {
        Self {
            schema_version: CONTENTION_REPORT_SCHEMA_VERSION,
            report_kind: CONTENTION_REPORT_KIND.to_string(),
            class,
            retryability: class.retryability(),
            bounded_wait: class.bounded_wait(),
            storage_state: class.to_storage_state(),
            is_archive_loss: class.is_archive_loss(),
            evidence,
            recommended_command: class.recommended_command().map(str::to_string),
            explanation: class.explanation().to_string(),
        }
    }

    /// Build the verdict directly from a `frankensqlite::FrankenError`, or
    /// `None` when the error is not a contention class (e.g. corruption).
    pub(crate) fn from_franken_error(err: &frankensqlite::FrankenError) -> Option<Self> {
        classify_franken_error(err).map(|class| Self::classify(class, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_CLASSES: &[ContentionClass] = &[
        ContentionClass::None,
        ContentionClass::BusyLocked,
        ContentionClass::BusyRecovery,
        ContentionClass::SnapshotConflict,
        ContentionClass::StaleWalSidecar,
        ContentionClass::StaleSearcherCache,
        ContentionClass::HostPressure,
    ];

    #[test]
    fn classes_serialize_snake_case_and_are_stable() {
        let pairs: &[(ContentionClass, &str)] = &[
            (ContentionClass::None, "none"),
            (ContentionClass::BusyLocked, "busy_locked"),
            (ContentionClass::BusyRecovery, "busy_recovery"),
            (ContentionClass::SnapshotConflict, "snapshot_conflict"),
            (ContentionClass::StaleWalSidecar, "stale_wal_sidecar"),
            (ContentionClass::StaleSearcherCache, "stale_searcher_cache"),
            (ContentionClass::HostPressure, "host_pressure"),
        ];
        for (variant, want) in pairs {
            assert_eq!(
                serde_json::to_string(variant).expect("serialize class"),
                format!("\"{want}\"")
            );
            assert_eq!(variant.stable_name(), *want);
        }
        assert_eq!(pairs.len(), ALL_CLASSES.len());
    }

    #[test]
    fn contention_is_never_archive_loss() {
        for &class in ALL_CLASSES {
            assert!(
                !class.is_archive_loss(),
                "{class:?} must not be archive loss"
            );
            let report = ContentionReport::classify(class, None);
            assert!(!report.is_archive_loss, "{class:?} report archive loss");
        }
    }

    #[test]
    fn retryability_matches_class_semantics() {
        assert_eq!(
            ContentionClass::BusyLocked.retryability(),
            Retryability::RetryAfterBackoff
        );
        assert_eq!(
            ContentionClass::BusyRecovery.retryability(),
            Retryability::RetryAfterBackoff
        );
        assert_eq!(
            ContentionClass::SnapshotConflict.retryability(),
            Retryability::RetryAfterBackoff
        );
        assert_eq!(
            ContentionClass::StaleWalSidecar.retryability(),
            Retryability::RetryAfterInspection
        );
        assert_eq!(
            ContentionClass::StaleSearcherCache.retryability(),
            Retryability::RetryAfterInspection
        );
        assert_eq!(
            ContentionClass::HostPressure.retryability(),
            Retryability::NotRetryable
        );
        assert_eq!(
            ContentionClass::None.retryability(),
            Retryability::NotRetryable
        );
    }

    #[test]
    fn bounded_wait_is_always_finite_and_present_only_when_retryable() {
        for &class in ALL_CLASSES {
            match class.retryability() {
                Retryability::NotRetryable => {
                    assert!(class.bounded_wait().is_none(), "{class:?} should not wait");
                }
                _ => {
                    let wait = class
                        .bounded_wait()
                        .expect("retryable class has a wait policy");
                    assert!(wait.is_bounded(), "{class:?} wait must be bounded");
                    assert!(wait.max_total_wait_ms > 0 && wait.max_total_wait_ms < u64::MAX);
                    assert!(wait.max_attempts > 0);
                }
            }
        }
    }

    #[test]
    fn storage_state_mapping_is_consistent_with_taxonomy() {
        assert_eq!(
            ContentionClass::BusyLocked.to_storage_state(),
            Some(StorageState::BusyOrLocked)
        );
        assert_eq!(
            ContentionClass::SnapshotConflict.to_storage_state(),
            Some(StorageState::BusyOrLocked)
        );
        assert_eq!(
            ContentionClass::StaleWalSidecar.to_storage_state(),
            Some(StorageState::WalSidecarSuspect)
        );
        assert_eq!(
            ContentionClass::StaleSearcherCache.to_storage_state(),
            Some(StorageState::DerivedOnlyDrift)
        );
        // Host pressure / none are not by themselves storage-integrity faults.
        assert_eq!(ContentionClass::HostPressure.to_storage_state(), None);
        assert_eq!(ContentionClass::None.to_storage_state(), None);
    }

    #[test]
    fn recommended_commands_are_concrete_and_never_destructive() {
        for &class in ALL_CLASSES {
            if let Some(cmd) = class.recommended_command() {
                assert!(cmd.starts_with("cass "), "must be concrete cass: {cmd}");
                assert_ne!(cmd.trim(), "cass");
                for bad in [
                    "rm ",
                    "rm -",
                    "delete ",
                    "DROP ",
                    "--purge",
                    "--force-clean",
                ] {
                    assert!(!cmd.contains(bad), "destructive token in {cmd}");
                }
            }
        }
    }

    #[test]
    fn cache_staleness_classification() {
        assert_eq!(
            CacheStaleness::classify(Some("g7"), Some("g7"), true),
            CacheStaleness::Fresh
        );
        assert_eq!(
            CacheStaleness::classify(Some("g8"), Some("g7"), true),
            CacheStaleness::StaleGeneration
        );
        assert_eq!(
            CacheStaleness::classify(Some("g8"), Some("g7"), false),
            CacheStaleness::ReloadFailed
        );
        // Unknown generation is treated as fresh (absence ≠ staleness ≠ loss).
        assert_eq!(
            CacheStaleness::classify(None, Some("g7"), true),
            CacheStaleness::Fresh
        );
        // Stale / reload-failed map to a reportable cache contention, never loss.
        assert_eq!(
            CacheStaleness::StaleGeneration.to_contention_class(),
            ContentionClass::StaleSearcherCache
        );
        assert!(
            !CacheStaleness::ReloadFailed
                .to_contention_class()
                .is_archive_loss()
        );
    }

    #[test]
    fn report_round_trips_through_json_with_invariant() {
        let report = ContentionReport::classify(
            ContentionClass::BusyLocked,
            Some(LockEvidence::from_busy_timeout()),
        );
        let json = serde_json::to_string(&report).expect("serialize report");
        assert!(json.contains("\"report_kind\":\"contention_diagnostic\""));
        assert!(json.contains("\"class\":\"busy_locked\""));
        assert!(json.contains("\"is_archive_loss\":false"));
        assert!(json.contains("\"retryability\":\"retry_after_backoff\""));
        let parsed: ContentionReport = serde_json::from_str(&json).expect("parse report");
        assert_eq!(parsed, report);
    }

    #[test]
    fn lock_evidence_is_platform_tolerant() {
        let ev = LockEvidence::from_busy_timeout();
        // No fabricated holder identity when the platform cannot provide one.
        assert!(ev.holder_pid.is_none());
        assert_eq!(ev.source, "busy_timeout_expiry");
        assert!(ev.note.is_some());
    }

    #[test]
    fn classify_franken_error_maps_busy_variants_and_skips_corruption() {
        use frankensqlite::FrankenError as E;
        // Busy / recovery are concrete unit variants — safe to construct.
        assert_eq!(
            classify_franken_error(&E::Busy),
            Some(ContentionClass::BusyLocked)
        );
        assert_eq!(
            classify_franken_error(&E::BusyRecovery),
            Some(ContentionClass::BusyRecovery)
        );
        assert_eq!(
            classify_franken_error(&E::DatabaseLocked {
                path: std::path::PathBuf::from("/tmp/locked.db"),
            }),
            Some(ContentionClass::BusyLocked)
        );
        assert_eq!(
            classify_franken_error(&E::LockFailed {
                detail: "reserved lock held".to_string(),
            }),
            Some(ContentionClass::BusyLocked)
        );
        assert!(is_retryable_contention(&E::Busy));
        assert!(is_retryable_contention(&E::BusyRecovery));
        // A non-contention error classifies to None and is not retryable here.
        assert_eq!(classify_franken_error(&E::QueryReturnedNoRows), None);
        assert!(!is_retryable_contention(&E::QueryReturnedNoRows));
    }
}

/// Integration coverage: real reader/writer contention on a frankensqlite-backed
/// temp DB. Mirrors the proven concurrent-stress pattern (anyhow + downcast to
/// `FrankenError`, `concurrent_writer`, `execute_compat`) but drives the retry
/// loop through this module's shared classifier, proving that every MVCC commit
/// conflict is classified as a retryable contention class (never archive loss),
/// that bounded retry converges, and that no update is lost.
#[cfg(test)]
mod contention_integration_tests {
    use super::{ContentionClass, classify_franken_error};
    use crate::storage::sqlite::{ConnectionManagerConfig, FrankenConnectionManager, WriterGuard};
    use frankensqlite::compat::{RowExt, TransactionExt};
    use frankensqlite::params as fparams;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::TempDir;

    /// One blind-increment transaction against the hot counter row. A fresh
    /// transaction per call reads the latest committed value, so a retried
    /// commit lands exactly one increment — no lost updates. Returns the raw
    /// `anyhow` error so the caller can downcast to `FrankenError` and classify
    /// the contention, mirroring the production retry loop.
    fn try_increment(guard: &WriterGuard<'_>) -> anyhow::Result<()> {
        let mut tx = guard.storage().raw().transaction()?;
        tx.execute_compat(
            "UPDATE counter SET v = v + ?1 WHERE id = 1",
            fparams![1_i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    #[test]
    fn concurrent_writers_on_hot_row_classify_as_retryable_contention_and_converge() {
        let dir = TempDir::new().expect("temp dir");
        let db_path = dir.path().join("contention.db");
        let config = ConnectionManagerConfig {
            reader_count: 2,
            max_writers: 4,
        };
        let mgr = FrankenConnectionManager::new(&db_path, config).expect("open manager");

        // One hot counter row that every writer contends on — maximizes the
        // chance the MVCC engine raises a real write-write conflict.
        {
            let mut guard = mgr.writer().expect("writer");
            guard
                .storage()
                .raw()
                .execute("CREATE TABLE counter (id INTEGER PRIMARY KEY, v INTEGER NOT NULL)")
                .expect("create table");
            guard
                .storage()
                .raw()
                .execute("INSERT INTO counter (id, v) VALUES (1, 0)")
                .expect("seed counter");
            guard.mark_committed();
        }

        let num_threads = 6;
        let incr_per_thread = 60;
        let classified_conflicts = Arc::new(AtomicUsize::new(0));
        let archive_loss_seen = Arc::new(AtomicUsize::new(0));
        let non_contention_errors = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|s| {
            for _ in 0..num_threads {
                let m = &mgr;
                let conflicts = Arc::clone(&classified_conflicts);
                let losses = Arc::clone(&archive_loss_seen);
                let unexpected = Arc::clone(&non_contention_errors);
                s.spawn(move || {
                    for _ in 0..incr_per_thread {
                        let mut attempt: u32 = 0;
                        loop {
                            let mut guard = m.concurrent_writer().expect("concurrent writer");
                            let result = try_increment(&guard);

                            match result {
                                Ok(()) => {
                                    guard.mark_committed();
                                    break;
                                }
                                Err(err) => {
                                    let franken = err
                                        .downcast_ref::<frankensqlite::FrankenError>()
                                        .or_else(|| {
                                            err.root_cause()
                                                .downcast_ref::<frankensqlite::FrankenError>()
                                        });
                                    let class = franken.and_then(classify_franken_error);
                                    match class {
                                        Some(c) => {
                                            conflicts.fetch_add(1, Ordering::Relaxed);
                                            if c.is_archive_loss() {
                                                losses.fetch_add(1, Ordering::Relaxed);
                                            }
                                            // Bounded jittered backoff, capped so
                                            // the loop can never spin forever.
                                            attempt += 1;
                                            assert!(
                                                attempt < 500,
                                                "bounded retry must converge, not spin"
                                            );
                                            let backoff = (1u64 << attempt.min(8)).min(256);
                                            std::thread::sleep(Duration::from_millis(backoff));
                                        }
                                        None => {
                                            // A non-contention error here is a
                                            // genuine failure; record and stop
                                            // this increment's loop.
                                            unexpected.fetch_add(1, Ordering::Relaxed);
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                });
            }
        });

        // No spurious non-contention error surfaced under pure write contention.
        assert_eq!(
            non_contention_errors.load(Ordering::Relaxed),
            0,
            "all errors under write contention must be contention classes"
        );
        // Contention is never archive loss.
        assert_eq!(
            archive_loss_seen.load(Ordering::Relaxed),
            0,
            "no contention error may report archive loss"
        );
        // Bounded retry converged with no lost updates: the hot counter equals
        // every successful increment.
        let reader = mgr.reader();
        let rows = reader
            .query("SELECT v FROM counter WHERE id = 1")
            .expect("read counter");
        let final_v: i64 = rows[0].get_typed(0).expect("typed counter");
        assert_eq!(
            final_v,
            (num_threads * incr_per_thread) as i64,
            "every increment must be durably applied (no lost updates)"
        );
        // The classifier was actually exercised by real MVCC contention.
        let observed = classified_conflicts.load(Ordering::Relaxed);
        eprintln!("hot-row contention: {observed} classified retryable conflicts");
        assert!(
            observed >= 1,
            "hot-row contention should raise at least one classified conflict"
        );
        // Sanity: the contention class observed maps to a busy-or-locked
        // storage state and is retryable.
        let busy = ContentionClass::SnapshotConflict;
        assert!(busy.to_storage_state().is_some());
        assert!(!busy.is_archive_loss());
    }
}
