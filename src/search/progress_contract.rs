// Dead-code tolerated module-wide: the universal progress/stall contract
// lands here ahead of the operations that will report through it (full
// index, incremental, watch, semantic backfill, historical salvage,
// lexical rebuild, repair) and the status/doctor/fleet surfaces that will
// project it. Downstream beads (.5.3 semantic checkpoints, .14.3 concurrency
// diagnostics, .15.2 daemon stale-searcher recovery) consume these types.
#![allow(dead_code)]

//! Universal progress and stall contract for long-running operations
//! (bead cass-fleet-resilience-20260608-uojcg.4.1).
//!
//! Today each long operation (full/incremental index, watch cycle,
//! semantic backfill, historical salvage, lexical rebuild, repair) reports
//! progress its own way, and "still alive" is conflated with "still making
//! forward progress". An operation can emit heartbeats forever while wedged
//! on a single poison item; agents and humans then either kill healthy work
//! or wait on dead work.
//!
//! The core invariant this module enforces is the **separation of heartbeat
//! from forward progress**:
//! - `heartbeat_at_ms` advances whenever the worker is alive (even mid-stall).
//! - `last_forward_progress_at_ms` advances ONLY when `current` increases.
//!
//! From those two clocks plus `stall_threshold_ms` we derive a single
//! [`OperationState`] — `building`, `repairing`, `stalled`, `stale`,
//! `waiting_on_lock`, `ready`, or `missing` — and a [`ProgressNextStep`], so
//! every status/doctor/fleet surface classifies a long operation the same
//! way instead of hand-rolling stall heuristics.
//!
//! All enum values serialize as snake_case, matching the readiness
//! vocabulary in [`crate::search::readiness`]. All derivations take an
//! explicit `now_ms` (no clock calls) so snapshots are byte-deterministic
//! and unit-testable.

use serde::{Deserialize, Serialize};

/// The kind of long-running operation a progress report describes. Drives
/// whether forward motion is classified as `building` vs `repairing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationKind {
    FullIndex,
    IncrementalIndex,
    WatchCycle,
    SemanticBackfill,
    HistoricalSalvage,
    LexicalRebuild,
    Repair,
}

impl OperationKind {
    /// Whether forward progress on this operation should read as a repair
    /// (vs a fresh build) in the derived [`OperationState`].
    pub(crate) fn is_repair(self) -> bool {
        matches!(self, Self::Repair | Self::HistoricalSalvage)
    }
}

/// The single derived lifecycle state for a long operation. Resolves the
/// seven cases a status surface must distinguish so "alive but wedged"
/// (`Stalled`) never reads as healthy `Building`, and "process gone"
/// (`Stale`) never reads as a live stall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationState {
    /// No operation and no target asset exists.
    Missing,
    /// Forward progress is recent; a fresh build is underway.
    Building,
    /// Forward progress is recent; a repair/salvage is underway.
    Repairing,
    /// Heartbeat is alive but `current` has not advanced past
    /// `stall_threshold_ms` and no lock explains the wait.
    Stalled,
    /// The heartbeat itself has gone silent past the threshold: the worker
    /// likely died and left a partial result.
    Stale,
    /// No forward progress and an exclusive lock is held (by this or another
    /// owner); the operation is waiting on the lock, not wedged.
    WaitingOnLock,
    /// The operation completed (`current >= total`).
    Ready,
}

impl OperationState {
    /// Whether the operation is making, or has made, healthy progress
    /// (`Building`/`Repairing`/`Ready`) as opposed to a degraded state.
    pub(crate) fn is_healthy(self) -> bool {
        matches!(self, Self::Building | Self::Repairing | Self::Ready)
    }
}

/// The safe next step for a given [`OperationState`]. Kept as an enum (with
/// a snake_case wire form) so consumers pattern-match a stable vocabulary
/// rather than sniffing prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProgressNextStep {
    /// Operation is done; nothing to do.
    None,
    /// Healthy progress; wait boundedly for completion.
    WaitBounded,
    /// Alive but not progressing; attach to or inspect the run before
    /// killing it, or wait a bounded interval.
    AttachOrWait,
    /// Blocked on a lock; wait for the current owner to release it instead
    /// of starting a duplicate.
    WaitForLockOwner,
    /// Heartbeat gone; the run likely died — restart it.
    RestartOperation,
    /// No operation running and the asset is absent — start it.
    StartOperation,
}

/// Who holds the operation's exclusive lock and since when. Surfaced so a
/// `waiting_on_lock` state names the owner instead of a generic "busy".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ActiveLock {
    /// Stable owner identity (pid / host / agent label). Redacted-friendly.
    pub owner: String,
    /// Wall-clock ms when the lock was acquired.
    pub acquired_at_ms: i64,
}

/// The raw progress a long operation reports. These are the *inputs*; the
/// derived stall/state fields live on [`ProgressSnapshot`] via
/// [`ProgressReport::resolve`]. Keeping inputs and derivations separate
/// means the worker only has to advance two clocks honestly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProgressReport {
    pub operation: OperationKind,
    /// Coarse phase, e.g. `scanning`, `embedding`, `merging`, `publishing`.
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subphase: Option<String>,
    /// Units of forward progress completed so far.
    pub current: u64,
    /// Total expected units; `None` when indeterminate (e.g. a watch tail).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// What `current`/`total` count, e.g. `sessions`, `files`, `vectors`.
    pub units: String,
    /// Wall-clock ms when the operation started.
    pub started_at_ms: i64,
    /// Wall-clock ms when `current` last increased (FORWARD progress).
    pub last_forward_progress_at_ms: i64,
    /// Wall-clock ms of the last liveness heartbeat (advances even mid-stall).
    pub heartbeat_at_ms: i64,
    /// How long without forward progress (heartbeat still alive) before the
    /// operation is considered stalled, and how long without a heartbeat
    /// before it is considered stale.
    pub stall_threshold_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_lock: Option<ActiveLock>,
}

impl ProgressReport {
    /// Whether the operation has completed all known work.
    fn is_complete(&self) -> bool {
        matches!(self.total, Some(t) if t > 0 && self.current >= t)
    }

    /// Resolve the raw report against `now_ms` into a fully-derived
    /// snapshot. `now_ms` is explicit (no clock call) for determinism.
    pub(crate) fn resolve(&self, now_ms: i64) -> ProgressSnapshot {
        let elapsed_ms = (now_ms - self.started_at_ms).max(0);
        let since_forward_ms = (now_ms - self.last_forward_progress_at_ms).max(0);
        let since_heartbeat_ms = (now_ms - self.heartbeat_at_ms).max(0);
        let threshold = self.stall_threshold_ms.max(0);

        let complete = self.is_complete();
        let heartbeat_dead = since_heartbeat_ms > threshold;
        let no_forward = since_forward_ms >= threshold;

        let (state, stall_reason) = if complete {
            (OperationState::Ready, None)
        } else if heartbeat_dead {
            (
                OperationState::Stale,
                Some(format!(
                    "no heartbeat for {since_heartbeat_ms}ms (> {threshold}ms); operation likely died mid-run"
                )),
            )
        } else if no_forward {
            // Heartbeat alive but no forward progress: distinguish a lock
            // wait from a true wedge.
            match &self.active_lock {
                Some(lock) => (
                    OperationState::WaitingOnLock,
                    Some(format!(
                        "no forward progress for {since_forward_ms}ms; blocked on lock held by {}",
                        lock.owner
                    )),
                ),
                None => (
                    OperationState::Stalled,
                    Some(format!(
                        "heartbeat alive but no forward progress for {since_forward_ms}ms (> {threshold}ms)"
                    )),
                ),
            }
        } else if self.operation.is_repair() {
            (OperationState::Repairing, None)
        } else {
            (OperationState::Building, None)
        };

        let next_step = match state {
            OperationState::Ready => ProgressNextStep::None,
            OperationState::Building | OperationState::Repairing => ProgressNextStep::WaitBounded,
            OperationState::WaitingOnLock => ProgressNextStep::WaitForLockOwner,
            OperationState::Stalled => ProgressNextStep::AttachOrWait,
            OperationState::Stale => ProgressNextStep::RestartOperation,
            OperationState::Missing => ProgressNextStep::StartOperation,
        };

        ProgressSnapshot {
            operation: self.operation,
            phase: self.phase.clone(),
            subphase: self.subphase.clone(),
            current: self.current,
            total: self.total,
            units: self.units.clone(),
            elapsed_ms,
            since_forward_progress_ms: since_forward_ms,
            since_heartbeat_ms,
            stall_threshold_ms: threshold,
            stalled: matches!(state, OperationState::Stalled),
            stall_reason,
            active_lock: self.active_lock.clone(),
            state,
            next_step,
        }
    }
}

/// A fully-resolved progress view: the raw report fields plus the derived
/// `state`, `stalled`, `stall_reason`, elapsed/since clocks, and
/// `next_step`. This is the single shape every status/doctor/fleet surface
/// serializes for a long operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProgressSnapshot {
    pub operation: OperationKind,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subphase: Option<String>,
    pub current: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    pub units: String,
    pub elapsed_ms: i64,
    pub since_forward_progress_ms: i64,
    pub since_heartbeat_ms: i64,
    pub stall_threshold_ms: i64,
    pub stalled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_lock: Option<ActiveLock>,
    pub state: OperationState,
    pub next_step: ProgressNextStep,
}

impl ProgressSnapshot {
    /// The snapshot for an operation that is not running and whose target
    /// asset is absent. `Missing` has no clocks; callers map `None` reports
    /// to this so the `missing` case shares the same shape.
    pub(crate) fn missing(operation: OperationKind, units: impl Into<String>) -> Self {
        Self {
            operation,
            phase: "absent".to_string(),
            subphase: None,
            current: 0,
            total: None,
            units: units.into(),
            elapsed_ms: 0,
            since_forward_progress_ms: 0,
            since_heartbeat_ms: 0,
            stall_threshold_ms: 0,
            stalled: false,
            stall_reason: None,
            active_lock: None,
            state: OperationState::Missing,
            next_step: ProgressNextStep::StartOperation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    /// A baseline report: started 10 min ago, just made forward progress
    /// and heartbeat now, 5-min stall threshold.
    fn report(now: i64, kind: OperationKind) -> ProgressReport {
        ProgressReport {
            operation: kind,
            phase: "scanning".to_string(),
            subphase: Some("connectors".to_string()),
            current: 100,
            total: Some(1000),
            units: "sessions".to_string(),
            started_at_ms: now - 10 * MIN,
            last_forward_progress_at_ms: now,
            heartbeat_at_ms: now,
            stall_threshold_ms: 5 * MIN,
            active_lock: None,
        }
    }

    #[test]
    fn enums_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&OperationKind::SemanticBackfill).unwrap(),
            "\"semantic_backfill\""
        );
        assert_eq!(
            serde_json::to_string(&OperationState::WaitingOnLock).unwrap(),
            "\"waiting_on_lock\""
        );
        assert_eq!(
            serde_json::to_string(&ProgressNextStep::AttachOrWait).unwrap(),
            "\"attach_or_wait\""
        );
    }

    #[test]
    fn true_forward_progress_reads_as_building() {
        let now = 1_000 * MIN;
        let snap = report(now, OperationKind::FullIndex).resolve(now);
        assert_eq!(snap.state, OperationState::Building);
        assert!(!snap.stalled);
        assert!(snap.stall_reason.is_none());
        assert_eq!(snap.next_step, ProgressNextStep::WaitBounded);
        assert_eq!(snap.elapsed_ms, 10 * MIN);
    }

    #[test]
    fn repair_progress_reads_as_repairing() {
        let now = 1_000 * MIN;
        for kind in [OperationKind::Repair, OperationKind::HistoricalSalvage] {
            let snap = report(now, kind).resolve(now);
            assert_eq!(snap.state, OperationState::Repairing, "{kind:?}");
            assert!(snap.state.is_healthy());
        }
    }

    #[test]
    fn heartbeat_without_forward_progress_is_stalled_not_building() {
        let now = 1_000 * MIN;
        let mut r = report(now, OperationKind::LexicalRebuild);
        // Heartbeat is fresh (now) but forward progress is 6 min stale,
        // past the 5-min threshold.
        r.heartbeat_at_ms = now;
        r.last_forward_progress_at_ms = now - 6 * MIN;
        let snap = r.resolve(now);
        assert_eq!(snap.state, OperationState::Stalled);
        assert!(snap.stalled);
        assert!(
            snap.stall_reason
                .as_deref()
                .unwrap()
                .contains("no forward progress")
        );
        assert_eq!(snap.next_step, ProgressNextStep::AttachOrWait);
    }

    #[test]
    fn dead_heartbeat_past_threshold_is_stale_with_restart() {
        let now = 1_000 * MIN;
        let mut r = report(now, OperationKind::SemanticBackfill);
        // Both clocks 6 min stale: the worker stopped emitting heartbeats.
        r.heartbeat_at_ms = now - 6 * MIN;
        r.last_forward_progress_at_ms = now - 6 * MIN;
        let snap = r.resolve(now);
        assert_eq!(snap.state, OperationState::Stale);
        assert!(!snap.stalled, "stale is distinct from a live stall");
        assert_eq!(snap.next_step, ProgressNextStep::RestartOperation);
    }

    #[test]
    fn no_progress_with_lock_is_waiting_on_lock_not_stalled() {
        let now = 1_000 * MIN;
        let mut r = report(now, OperationKind::FullIndex);
        r.last_forward_progress_at_ms = now - 6 * MIN;
        r.heartbeat_at_ms = now;
        r.active_lock = Some(ActiveLock {
            owner: "host-b/pid-4242".to_string(),
            acquired_at_ms: now - 7 * MIN,
        });
        let snap = r.resolve(now);
        assert_eq!(snap.state, OperationState::WaitingOnLock);
        assert!(!snap.stalled);
        assert_eq!(snap.next_step, ProgressNextStep::WaitForLockOwner);
        assert!(
            snap.stall_reason
                .as_deref()
                .unwrap()
                .contains("host-b/pid-4242")
        );
    }

    #[test]
    fn completed_publish_is_ready() {
        let now = 1_000 * MIN;
        let mut r = report(now, OperationKind::FullIndex);
        r.current = 1000;
        r.total = Some(1000);
        // Even if forward progress is old, completion dominates.
        r.last_forward_progress_at_ms = now - 30 * MIN;
        r.heartbeat_at_ms = now - 30 * MIN;
        let snap = r.resolve(now);
        assert_eq!(snap.state, OperationState::Ready);
        assert!(!snap.stalled);
        assert_eq!(snap.next_step, ProgressNextStep::None);
    }

    #[test]
    fn missing_operation_snapshot_recommends_start() {
        let snap = ProgressSnapshot::missing(OperationKind::FullIndex, "sessions");
        assert_eq!(snap.state, OperationState::Missing);
        assert_eq!(snap.next_step, ProgressNextStep::StartOperation);
        assert!(!snap.state.is_healthy());
    }

    #[test]
    fn indeterminate_total_never_reports_ready() {
        let now = 1_000 * MIN;
        let mut r = report(now, OperationKind::WatchCycle);
        r.total = None; // a watch tail has no known total
        r.current = 9999;
        let snap = r.resolve(now);
        assert_ne!(snap.state, OperationState::Ready);
        assert_eq!(snap.state, OperationState::Building);
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let now = 1_000 * MIN;
        let snap = report(now, OperationKind::IncrementalIndex).resolve(now);
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"operation\":\"incremental_index\""));
        assert!(json.contains("\"state\":\"building\""));
        assert!(json.contains("\"next_step\":\"wait_bounded\""));
        let parsed: ProgressSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, snap);
    }
}
