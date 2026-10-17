//! Regressions for lost probes and byte-admission recovery. Run together with
//! policy.rs's original tests: rustc --edition=2024 --test policy.rs.

use super::*;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const MAX: u64 = 512 * MIB;

fn ample() -> Sample {
    Sample {
        resident_bytes: Some(4 * GIB),
        available_bytes: Some(10 * GIB),
        total_bytes: Some(16 * GIB),
    }
}

fn starved() -> Sample {
    Sample {
        available_bytes: Some(0),
        ..ample()
    }
}

fn observe(policy: &mut Policy, sample: Sample) -> Decision {
    policy.observe(sample, None, MAX, MIB)
}

#[test]
fn lost_headroom_does_not_recover_workers_or_bytes() {
    let mut policy = Policy::default();
    assert_eq!(observe(&mut policy, starved()).capacity_pct, 1);
    for _ in 0..30 {
        let decision = observe(
            &mut policy,
            Sample {
                available_bytes: None,
                ..ample()
            },
        );
        assert_eq!(decision.capacity_pct, 1);
        assert_eq!(decision.inflight_byte_limit, MIB);
        assert_eq!(decision.healthy_streak, 0);
        assert_eq!(decision.reason, "resident_memory_unknown");
    }
    for expected in [1, 1, 11] {
        assert_eq!(observe(&mut policy, ample()).capacity_pct, expected);
    }
}

#[test]
fn lost_total_cannot_raise_the_last_cgroup_target_to_an_explicit_limit() {
    let mut policy = Policy::default();
    let first = policy.observe(starved(), Some(32 * GIB), MAX, MIB);
    assert_eq!(first.target_resident_bytes, Some(14 * GIB));
    for total in [None, Some(0)] {
        for _ in 0..10 {
            let decision = policy.observe(
                Sample {
                    total_bytes: total,
                    ..ample()
                },
                Some(32 * GIB),
                MAX,
                MIB,
            );
            assert_eq!(decision.target_resident_bytes, Some(14 * GIB));
            assert_eq!(decision.capacity_pct, 1);
            assert_eq!(decision.inflight_byte_limit, MIB);
            assert_eq!(decision.reason, "resident_memory_unknown");
        }
    }
}

#[test]
fn a_surviving_probe_can_still_shrink_during_a_partial_outage() {
    let mut policy = Policy::default();
    observe(&mut policy, ample());
    let decision = observe(
        &mut policy,
        Sample {
            resident_bytes: Some(13 * GIB),
            available_bytes: None,
            ..ample()
        },
    );
    assert_eq!(decision.capacity_pct, 50);
    let decision = observe(
        &mut policy,
        Sample {
            resident_bytes: None,
            available_bytes: Some(0),
            ..ample()
        },
    );
    assert_eq!(decision.capacity_pct, 1);
    assert_eq!(decision.inflight_byte_limit, MIB);
}

#[test]
fn a_never_supported_headroom_probe_does_not_prevent_resident_only_recovery() {
    let mut policy = Policy::default();
    let severe = Sample {
        resident_bytes: Some(16 * GIB),
        available_bytes: None,
        total_bytes: None,
    };
    assert_eq!(
        policy.observe(severe, Some(8 * GIB), MAX, MIB).capacity_pct,
        1
    );
    for expected in [1, 1, 11] {
        let sample = Sample {
            resident_bytes: Some(GIB),
            ..severe
        };
        assert_eq!(
            policy.observe(sample, Some(8 * GIB), MAX, MIB).capacity_pct,
            expected
        );
    }
}

#[test]
fn newly_available_headroom_becomes_required_for_subsequent_recovery() {
    let mut policy = Policy::default();
    observe(
        &mut policy,
        Sample {
            available_bytes: None,
            ..ample()
        },
    );
    observe(&mut policy, starved());
    for _ in 0..6 {
        assert_eq!(
            observe(
                &mut policy,
                Sample {
                    available_bytes: None,
                    ..ample()
                }
            )
            .capacity_pct,
            1
        );
    }
}

#[test]
fn byte_recovery_waits_then_grows_additively_instead_of_reopening_the_ceiling() {
    let mut policy = Policy::default();
    assert_eq!(observe(&mut policy, starved()).inflight_byte_limit, MIB);
    let step = MAX / 10;
    for expected in [MIB, MIB, MIB + step, MIB + step, MIB + step, MIB + 2 * step] {
        assert_eq!(observe(&mut policy, ample()).inflight_byte_limit, expected);
    }
    for _ in 0..60 {
        observe(&mut policy, ample());
    }
    assert_eq!(observe(&mut policy, ample()).inflight_byte_limit, MAX);
}

#[test]
fn failed_headroom_breaks_a_partially_completed_recovery_streak() {
    let mut policy = Policy::default();
    observe(&mut policy, starved());
    observe(&mut policy, ample());
    observe(&mut policy, ample());
    observe(
        &mut policy,
        Sample {
            available_bytes: None,
            ..ample()
        },
    );
    for _ in 0..2 {
        let decision = observe(&mut policy, ample());
        assert_eq!(decision.capacity_pct, 1);
        assert_eq!(decision.inflight_byte_limit, MIB);
    }
    let decision = observe(&mut policy, ample());
    assert_eq!(decision.capacity_pct, 11);
    assert_eq!(decision.inflight_byte_limit, MIB + MAX / 10);
}

#[test]
fn byte_recovery_never_exceeds_current_slack() {
    let mut policy = Policy::default();
    observe(&mut policy, starved());
    let sample = Sample {
        available_bytes: Some(2 * GIB),
        ..ample()
    };
    let ceiling = 16 * GIB;
    for _ in 0..100 {
        let decision = policy.observe(sample, None, ceiling, MIB);
        assert!(decision.inflight_byte_limit <= 768 * MIB);
    }
    assert_eq!(
        policy
            .observe(sample, None, ceiling, MIB)
            .inflight_byte_limit,
        768 * MIB
    );
}

#[test]
fn losing_total_still_uses_the_last_target_for_severe_resident_pressure() {
    let mut policy = Policy::default();
    observe(&mut policy, ample());
    let decision = observe(
        &mut policy,
        Sample {
            resident_bytes: Some(15 * GIB),
            total_bytes: None,
            ..ample()
        },
    );
    assert_eq!(decision.capacity_pct, 1);
    assert_eq!(decision.target_resident_bytes, Some(14 * GIB));
    assert_eq!(decision.reason, "resident_memory_severe");
}

#[test]
fn cold_unsupported_samples_remain_a_noop() {
    let mut policy = Policy::default();
    for _ in 0..100 {
        let decision = observe(&mut policy, Sample::default());
        assert_eq!(decision.capacity_pct, 100);
        assert_eq!(decision.inflight_byte_limit, MAX);
        assert_eq!(decision.target_resident_bytes, None);
    }
}

#[test]
fn lowered_configuration_and_zero_requests_apply_during_recovery() {
    let mut policy = Policy::default();
    observe(&mut policy, starved());
    for _ in 0..3 {
        observe(&mut policy, ample());
    }
    assert_eq!(
        policy.observe(ample(), None, 512, MIB).inflight_byte_limit,
        512
    );
    assert_eq!(policy.observe(ample(), None, 0, MIB).inflight_byte_limit, 0);
}
