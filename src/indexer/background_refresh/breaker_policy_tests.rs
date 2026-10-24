//! Regression coverage for missing metadata, stale snapshots and clock steps.
use super::*;

const SPAWN: i64 = 1_000_000;
const DETECTED: i64 = 2_000_000;

fn failed_state(failures: u32) -> AutoRefreshState {
    AutoRefreshState {
        last_spawn_ms: SPAWN,
        last_pid: 4242,
        last_reason: "index-stale".into(),
        consecutive_failures: failures,
        failure_counted_for_spawn_ms: SPAWN,
        last_failure_detected_ms: DETECTED,
        last_failure: Some("recorded failure evidence".into()),
    }
}

fn judge(state: &mut AutoRefreshState, watermark: Option<i64>, now: i64) -> BreakerVerdict {
    judge_previous_spawn(state, watermark, now, Path::new("auto-refresh.log"))
}

#[test]
fn missing_watermark_preserves_a_tripped_breaker_and_its_evidence() {
    let mut state = failed_state(FAILURE_TRIP_THRESHOLD);
    let before = state.clone();
    for now in [DETECTED, DETECTED + 86_400_000, i64::MIN, i64::MAX] {
        assert_eq!(judge(&mut state, None, now), BreakerVerdict::Tripped);
        assert_eq!(state, before);
    }
}

#[test]
fn missing_watermark_preserves_backoff_without_counting_another_failure() {
    let mut state = failed_state(1);
    let before = state.clone();
    assert_eq!(
        judge(&mut state, None, DETECTED + 1),
        BreakerVerdict::BackedOff {
            remaining_secs: 3_600
        }
    );
    assert_eq!(state, before);
    assert_eq!(
        judge(&mut state, None, DETECTED + 3_600_000),
        BreakerVerdict::Closed
    );
    assert_eq!(state, before);
}

#[test]
fn sentinel_watermarks_neither_clear_nor_invent_failure_evidence() {
    for watermark in [None, Some(0), Some(-1), Some(i64::MIN)] {
        let mut tripped = failed_state(FAILURE_TRIP_THRESHOLD);
        let before = tripped.clone();
        assert_eq!(
            judge(&mut tripped, watermark, DETECTED),
            BreakerVerdict::Tripped
        );
        assert_eq!(tripped, before);
        let mut unjudged = AutoRefreshState {
            last_spawn_ms: SPAWN,
            ..AutoRefreshState::default()
        };
        let before = unjudged.clone();
        assert_eq!(
            judge(&mut unjudged, watermark, DETECTED),
            BreakerVerdict::Closed
        );
        assert_eq!(unjudged, before);
    }
}

#[test]
fn unknown_outcome_does_not_count_a_new_attempt_against_the_old_streak() {
    let mut state = failed_state(1);
    state.last_spawn_ms += 1;
    let before = state.clone();
    assert!(matches!(
        judge(&mut state, None, DETECTED),
        BreakerVerdict::BackedOff { .. }
    ));
    assert_eq!(state, before);
}

#[test]
fn successful_watermark_resets_and_acknowledges_a_tripped_attempt() {
    let mut state = failed_state(FAILURE_TRIP_THRESHOLD);
    assert_eq!(
        judge(&mut state, Some(SPAWN), DETECTED),
        BreakerVerdict::Closed
    );
    assert_eq!(state.consecutive_failures, 0);
    assert_eq!(state.failure_counted_for_spawn_ms, SPAWN);
    assert_eq!(state.last_failure_detected_ms, 0);
    assert!(state.last_failure.is_none());
}

#[test]
fn success_before_first_failure_cannot_be_undone_by_an_older_snapshot() {
    let mut state = AutoRefreshState {
        last_spawn_ms: SPAWN,
        ..AutoRefreshState::default()
    };
    assert_eq!(
        judge(&mut state, Some(SPAWN + 1), DETECTED),
        BreakerVerdict::Closed
    );
    let acknowledged = state.clone();
    for watermark in [Some(SPAWN - 1), None, Some(1), Some(0)] {
        assert_eq!(
            judge(&mut state, watermark, DETECTED + 1),
            BreakerVerdict::Closed
        );
        assert_eq!(state, acknowledged);
    }
}

#[test]
fn an_acknowledged_success_does_not_immunize_a_different_attempt() {
    let mut state = AutoRefreshState {
        last_spawn_ms: SPAWN,
        ..AutoRefreshState::default()
    };
    assert_eq!(
        judge(&mut state, Some(SPAWN), DETECTED),
        BreakerVerdict::Closed
    );
    state.last_spawn_ms = SPAWN + 1;
    assert_eq!(
        judge(&mut state, Some(SPAWN), DETECTED),
        BreakerVerdict::BackedOff {
            remaining_secs: 3_600
        }
    );
    assert_eq!(state.consecutive_failures, 1);
    assert_eq!(state.failure_counted_for_spawn_ms, SPAWN + 1);
}

#[test]
fn duplicate_failure_observations_and_metadata_outages_do_not_inflate_streak() {
    let mut state = AutoRefreshState {
        last_spawn_ms: SPAWN,
        ..AutoRefreshState::default()
    };
    for watermark in [Some(SPAWN - 1), None, Some(1), Some(0), Some(SPAWN - 1)] {
        assert!(matches!(
            judge(&mut state, watermark, DETECTED),
            BreakerVerdict::BackedOff { .. }
        ));
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(state.last_failure_detected_ms, DETECTED);
    }
}

#[test]
fn failure_streak_escalates_across_distinct_attempts() {
    let mut state = AutoRefreshState::default();
    for attempt in 1..=FAILURE_TRIP_THRESHOLD {
        state.last_spawn_ms = SPAWN + i64::from(attempt);
        let verdict = judge(&mut state, Some(1), DETECTED);
        assert_eq!(state.consecutive_failures, attempt);
        if attempt < FAILURE_TRIP_THRESHOLD {
            let remaining_secs = FAILURE_BACKOFF_SECS[usize::try_from(attempt - 1).unwrap()];
            assert_eq!(verdict, BreakerVerdict::BackedOff { remaining_secs });
        } else {
            assert_eq!(verdict, BreakerVerdict::Tripped);
        }
    }
}

#[test]
fn backoff_rounds_up_and_expires_at_the_exact_millisecond() {
    for (failures, seconds) in [(1, 3_600_u64), (2, 21_600)] {
        let state = failed_state(failures);
        let interval = i64::try_from(seconds).unwrap() * 1000;
        for (elapsed, expected) in [
            (
                -1,
                BreakerVerdict::BackedOff {
                    remaining_secs: seconds + 1,
                },
            ),
            (
                0,
                BreakerVerdict::BackedOff {
                    remaining_secs: seconds,
                },
            ),
            (
                1,
                BreakerVerdict::BackedOff {
                    remaining_secs: seconds,
                },
            ),
            (
                interval - 1,
                BreakerVerdict::BackedOff { remaining_secs: 1 },
            ),
            (interval, BreakerVerdict::Closed),
            (interval + 1, BreakerVerdict::Closed),
        ] {
            assert_eq!(
                recorded_breaker_verdict(&state, DETECTED + elapsed),
                expected
            );
        }
    }
}

#[test]
fn backwards_clock_steps_do_not_overflow_or_open_the_breaker() {
    let mut state = failed_state(2);
    state.last_failure_detected_ms = i64::MAX;
    let expected_ms = 21_600_000_i128 + i128::from(i64::MAX) - i128::from(i64::MIN);
    let expected_seconds = u64::try_from((expected_ms + 999) / 1000).unwrap();
    assert_eq!(
        recorded_breaker_verdict(&state, i64::MIN),
        BreakerVerdict::BackedOff {
            remaining_secs: expected_seconds
        }
    );
}

#[test]
fn arithmetic_matches_widened_reference_across_timestamp_extremes() {
    let times = [i64::MIN, -1, 0, 1, DETECTED, i64::MAX];
    for failures in [0, 1, 2, 3, u32::MAX] {
        for detected in times {
            for now in times {
                let mut state = failed_state(failures);
                state.last_failure_detected_ms = detected;
                let expected = if failures >= FAILURE_TRIP_THRESHOLD {
                    BreakerVerdict::Tripped
                } else if failures == 0 {
                    BreakerVerdict::Closed
                } else {
                    let backoff = if failures == 1 { 3_600_000 } else { 21_600_000 };
                    let remaining = i128::from(detected) + backoff - i128::from(now);
                    if remaining <= 0 {
                        BreakerVerdict::Closed
                    } else {
                        BreakerVerdict::BackedOff {
                            remaining_secs: u64::try_from((remaining + 999) / 1000).unwrap(),
                        }
                    }
                };
                assert_eq!(recorded_breaker_verdict(&state, now), expected);
            }
        }
    }
}

#[test]
fn scheduler_full_pass_exemption_is_explicit_and_does_not_erase_evidence() {
    let mut state = failed_state(FAILURE_TRIP_THRESHOLD);
    let before = state.clone();
    let log = Path::new("auto-refresh.log");
    assert_eq!(
        spawn_breaker_verdict(&mut state, true, None, DETECTED, log),
        BreakerVerdict::Closed
    );
    assert_eq!(state, before);
    assert_eq!(
        spawn_breaker_verdict(&mut state, false, None, DETECTED, log),
        BreakerVerdict::Tripped
    );
    assert_eq!(
        spawn_breaker_verdict(&mut state, true, Some(1), DETECTED, log),
        BreakerVerdict::Tripped
    );
    assert_eq!(state, before);
}

#[test]
fn acknowledged_success_survives_state_serialization() {
    let mut state = AutoRefreshState {
        last_spawn_ms: SPAWN,
        ..AutoRefreshState::default()
    };
    assert_eq!(
        judge(&mut state, Some(SPAWN), DETECTED),
        BreakerVerdict::Closed
    );
    let json = serde_json::to_vec(&state).unwrap();
    let mut reloaded: AutoRefreshState = serde_json::from_slice(&json).unwrap();
    assert_eq!(
        judge(&mut reloaded, Some(SPAWN - 1), DETECTED),
        BreakerVerdict::Closed
    );
    assert_eq!(reloaded.consecutive_failures, 0);
}

#[test]
fn incremental_spawn_path_cannot_bypass_trip_when_metadata_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let state = failed_state(FAILURE_TRIP_THRESHOLD);
    save_state(dir.path(), &state).unwrap();
    let outcome = maybe_spawn_with_policy(
        dir.path(),
        &dir.path().join("agent_search.db"),
        "index-stale",
        false,
        None,
        AutoRefreshPolicy {
            enabled: true,
            cooldown: Duration::ZERO,
        },
    );
    assert!(matches!(outcome, AutoRefreshOutcome::Tripped { .. }));
    assert_eq!(load_state(dir.path()).unwrap(), state);
    assert!(!log_path(dir.path()).exists(), "must not spawn a child");
    assert!(!dir.path().join("agent_search.db").exists());
}
