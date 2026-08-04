// Dead-code tolerated module-wide: this liveness regression fixture matrix
// is consumed by downstream beads .12.5 (report-derived E2E scenario
// scripts) and .11.2 (regression corpus for mined issue classes).
#![allow(dead_code)]

//! Deterministic liveness regression fixtures for stalls, salvage, and
//! watch recovery (bead cass-fleet-resilience-20260608-uojcg.4.5).
//!
//! These freeze the liveness failure modes mined from issues #137 (current:0
//! stream), #196 (lock heartbeat without forward progress), #247
//! (zero-new-work / interrupted historical salvage), #248 (OOM-killed watch
//! restart), and #250 (exit-code-9 watch failure with only a drop_close
//! warning) so future indexer changes cannot silently regress liveness.
//!
//! Every fixture is built from in-source literals against a single fixed
//! clock (`NOW_MS`), so the matrix is byte-deterministic and the tests run
//! purely in-memory — a pass can never be a hung timeout (the .4.5
//! "distinguish real pass from timeout" requirement). The progress fixtures
//! resolve through [`crate::search::progress_contract`]; the watch-recovery
//! fixtures use [`crate::search::watch_exit_envelope`].

use crate::search::progress_contract::{ActiveLock, OperationKind, ProgressReport};
use crate::search::watch_exit_envelope::WatchExitEnvelope;

/// Fixed reference clock so every fixture is deterministic.
pub(crate) const NOW_MS: i64 = 1_749_350_000_000; // ~2026-06-08T00:00:00Z
const MIN_MS: i64 = 60_000;
const STALL_THRESHOLD_MS: i64 = 5 * MIN_MS;

fn base(kind: OperationKind, phase: &str, units: &str) -> ProgressReport {
    ProgressReport {
        operation: kind,
        phase: phase.to_string(),
        subphase: None,
        current: 0,
        total: None,
        units: units.to_string(),
        started_at_ms: NOW_MS - 10 * MIN_MS,
        last_forward_progress_at_ms: NOW_MS,
        heartbeat_at_ms: NOW_MS,
        stall_threshold_ms: STALL_THRESHOLD_MS,
        active_lock: None,
    }
}

/// The liveness progress fixtures, in a stable order, as `(name, report)`.
/// Resolve each with `report.resolve(NOW_MS)`.
pub(crate) fn liveness_fixtures() -> Vec<(&'static str, ProgressReport)> {
    vec![
        // #137: a stream reporting current:0 while genuinely making forward
        // progress (heartbeat and forward-progress both now) — must read as
        // building, not stalled.
        ("current_zero_stream_building", {
            let mut r = base(OperationKind::IncrementalIndex, "scanning", "sessions");
            r.current = 0;
            r.total = Some(500);
            r
        }),
        // #196: heartbeat alive but no forward progress for 6 min while an
        // exclusive lock is held — waiting on the lock, not wedged.
        ("lock_heartbeat_without_progress", {
            let mut r = base(OperationKind::LexicalRebuild, "merging", "segments");
            r.current = 12;
            r.total = Some(64);
            r.last_forward_progress_at_ms = NOW_MS - 6 * MIN_MS;
            r.active_lock = Some(ActiveLock {
                owner: "host-b/pid-4242".to_string(),
                acquired_at_ms: NOW_MS - 7 * MIN_MS,
            });
            r
        }),
        // Heartbeat alive, no forward progress for 6 min, no lock — a true
        // wedge that must read as stalled (attach/inspect, do not wait
        // forever).
        ("stalled_no_lock", {
            let mut r = base(OperationKind::FullIndex, "embedding", "vectors");
            r.current = 300;
            r.total = Some(10_000);
            r.last_forward_progress_at_ms = NOW_MS - 6 * MIN_MS;
            r
        }),
        // #247: a historical salvage bundle that found zero new work — it is
        // complete, not stuck.
        ("zero_new_historical_bundle_ready", {
            let mut r = base(OperationKind::HistoricalSalvage, "publishing", "bundles");
            r.current = 1;
            r.total = Some(1);
            r
        }),
        // #247: an interrupted salvage — the heartbeat itself stopped 6 min
        // ago, so the worker died mid-run and must be restarted.
        ("interrupted_salvage_stale", {
            let mut r = base(OperationKind::HistoricalSalvage, "salvaging", "bundles");
            r.current = 3;
            r.total = Some(20);
            r.heartbeat_at_ms = NOW_MS - 6 * MIN_MS;
            r.last_forward_progress_at_ms = NOW_MS - 6 * MIN_MS;
            r
        }),
        // #248: an OOM-killed watch cycle — heartbeat gone, must restart.
        ("oom_killed_watch_stale", {
            let mut r = base(OperationKind::WatchCycle, "ingesting", "sessions");
            r.current = 42;
            r.total = None;
            r.heartbeat_at_ms = NOW_MS - 8 * MIN_MS;
            r.last_forward_progress_at_ms = NOW_MS - 8 * MIN_MS;
            r
        }),
        // Sparse lexical metadata: a rebuild barely underway (few docs) but
        // genuinely progressing — building, not missing/stalled.
        ("sparse_lexical_metadata_building", {
            let mut r = base(OperationKind::LexicalRebuild, "indexing", "docs");
            r.current = 3;
            r.total = Some(250_000);
            r
        }),
    ]
}

/// The watch-recovery exit fixtures, in a stable order, as `(name,
/// envelope)`.
pub(crate) fn watch_recovery_fixtures() -> Vec<(&'static str, WatchExitEnvelope)> {
    vec![
        // #250: exit code 9 with only a drop_close warning — a storage close
        // failure, now a parseable envelope.
        (
            "exit_code_9_storage_close",
            WatchExitEnvelope::storage_close_failure(9, "drop_close: flush returned EIO"),
        ),
        // #248: an OOM kill (SIGKILL -> 137) that an agent should restart
        // after checking health.
        (
            "oom_killed_watch_restart",
            WatchExitEnvelope::unknown(
                137,
                "process OOM-killed (SIGKILL); restart watch after checking health",
            ),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::progress_contract::{OperationState, ProgressNextStep};
    use crate::search::watch_exit_envelope::{Retryability, WatchExitKind};

    fn progress(name: &str) -> ProgressReport {
        liveness_fixtures()
            .into_iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("missing liveness fixture {name}"))
            .1
    }

    /// The intended progress-contract resolution per fixture: the .4.5
    /// "exercise progress contract, status projection, safe next command"
    /// requirement, as a self-checking table.
    #[test]
    fn each_progress_fixture_resolves_to_intended_state_and_next_step() {
        let cases = [
            (
                "current_zero_stream_building",
                OperationState::Building,
                ProgressNextStep::WaitBounded,
            ),
            (
                "lock_heartbeat_without_progress",
                OperationState::WaitingOnLock,
                ProgressNextStep::WaitForLockOwner,
            ),
            (
                "stalled_no_lock",
                OperationState::Stalled,
                ProgressNextStep::AttachOrWait,
            ),
            (
                "zero_new_historical_bundle_ready",
                OperationState::Ready,
                ProgressNextStep::None,
            ),
            (
                "interrupted_salvage_stale",
                OperationState::Stale,
                ProgressNextStep::RestartOperation,
            ),
            (
                "oom_killed_watch_stale",
                OperationState::Stale,
                ProgressNextStep::RestartOperation,
            ),
            (
                "sparse_lexical_metadata_building",
                OperationState::Building,
                ProgressNextStep::WaitBounded,
            ),
        ];
        for (name, state, next) in cases {
            let snap = progress(name).resolve(NOW_MS);
            assert_eq!(snap.state, state, "{name} state");
            assert_eq!(snap.next_step, next, "{name} next_step");
        }
    }

    #[test]
    fn no_degraded_fixture_recommends_unbounded_waiting() {
        // The #137/#196/#247/#248 regressions are "told to wait forever on
        // wedged/dead work". Assert that only genuinely-progressing or
        // lock-blocked states ever yield a wait, and stalled/stale never do.
        for (name, report) in liveness_fixtures() {
            let snap = report.resolve(NOW_MS);
            match snap.state {
                OperationState::Stalled => {
                    assert_eq!(
                        snap.next_step,
                        ProgressNextStep::AttachOrWait,
                        "{name}: a stall must prompt attach/inspect, not an open-ended wait"
                    );
                }
                OperationState::Stale => {
                    assert_eq!(
                        snap.next_step,
                        ProgressNextStep::RestartOperation,
                        "{name}: dead heartbeat must prompt restart, never wait"
                    );
                }
                _ => {}
            }
            // A stalled/stale op must never be classified healthy.
            if matches!(snap.state, OperationState::Stalled | OperationState::Stale) {
                assert!(!snap.state.is_healthy(), "{name} must not read as healthy");
            }
        }
    }

    #[test]
    fn fixtures_are_deterministic_across_resolutions() {
        // Resolving the same fixture twice at the same NOW yields identical
        // snapshots — a pass is a real pass, not a timing artifact.
        for (name, report) in liveness_fixtures() {
            assert_eq!(
                report.resolve(NOW_MS),
                report.resolve(NOW_MS),
                "{name} must resolve deterministically"
            );
        }
    }

    #[test]
    fn liveness_matrix_covers_all_named_issue_classes_in_stable_order() {
        let names: Vec<&str> = liveness_fixtures().into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            vec![
                "current_zero_stream_building",
                "lock_heartbeat_without_progress",
                "stalled_no_lock",
                "zero_new_historical_bundle_ready",
                "interrupted_salvage_stale",
                "oom_killed_watch_stale",
                "sparse_lexical_metadata_building",
            ]
        );
    }

    #[test]
    fn watch_recovery_fixtures_classify_exit_code_9_and_oom() {
        let map: std::collections::HashMap<&str, WatchExitEnvelope> =
            watch_recovery_fixtures().into_iter().collect();

        let storage = &map["exit_code_9_storage_close"];
        assert_eq!(storage.kind, WatchExitKind::StorageCloseFailure);
        assert_eq!(storage.exit_code, 9);
        assert!(storage.next_command.is_some());

        let oom = &map["oom_killed_watch_restart"];
        assert_eq!(oom.kind, WatchExitKind::Unknown);
        assert_eq!(oom.exit_code, 137);
        // OOM is an operator-attention exit, not a tight auto-retry loop.
        assert_eq!(oom.retryability, Retryability::RetryAfterFix);
        assert!(!oom.is_auto_retryable());
    }

    #[test]
    fn watch_recovery_fixtures_round_trip_through_json() {
        for (name, env) in watch_recovery_fixtures() {
            let json = serde_json::to_string(&env).unwrap();
            let parsed: WatchExitEnvelope = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, env, "{name} must round-trip");
        }
    }
}
