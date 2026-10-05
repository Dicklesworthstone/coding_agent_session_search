//! Resident-memory feedback, independent of CPU pressure and OS sampling.
//!
//! The policy reduces NEW admission, never kills workers or claims to bound
//! allocations already owned by the database, an embedder or the allocator.
//! A nonzero admission floor lets consumers drain and checkpoint their work.

const HEALTHY_TICKS_TO_GROW: u8 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub resident_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    /// Effective total, including container/cgroup limits, NOT host RAM alone.
    pub total_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    pub capacity_pct: u32,
    pub inflight_byte_limit: u64,
    pub target_resident_bytes: Option<u64>,
    pub healthy_streak: u8,
    pub reason: &'static str,
    pub sample: Sample,
}

#[derive(Debug)]
pub struct Policy {
    capacity_pct: u32,
    healthy_streak: u8,
    last_byte_limit: Option<u64>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            capacity_pct: 100,
            healthy_streak: 0,
            last_byte_limit: None,
        }
    }
}

/// Floor(value * numerator / denominator), without overflowing intermediate
/// products. All callers use small numerators no greater than denominator.
fn fraction(value: u64, numerator: u64, denominator: u64) -> u64 {
    value / denominator * numerator + value % denominator * numerator / denominator
}

fn target_resident_bytes(total: Option<u64>, explicit: Option<u64>) -> Option<u64> {
    let automatic = total.filter(|total| *total > 0).map(|total| {
        // Leave one eighth for the OS, other jobs in the same cgroup and
        // allocations made between samples. This is a setpoint, not a quota.
        fraction(total, 7, 8).max(1)
    });
    match (automatic, explicit.filter(|limit| *limit > 0)) {
        (Some(automatic), Some(explicit)) => Some(automatic.min(explicit)),
        (automatic, explicit) => automatic.or(explicit),
    }
}

impl Policy {
    pub fn observe(
        &mut self,
        sample: Sample,
        explicit_resident_limit: Option<u64>,
        max_inflight_bytes: u64,
        min_inflight_bytes: u64,
    ) -> Decision {
        let target = target_resident_bytes(sample.total_bytes, explicit_resident_limit);
        let resident = sample.resident_bytes.zip(target);
        let headroom = sample.available_bytes.zip(sample.total_bytes.filter(|total| *total > 0));
        let severe = resident.is_some_and(|(used, target)| used >= target)
            || headroom.is_some_and(|(free, total)| free <= fraction(total, 1, 32));
        let pressured = resident.is_some_and(|(used, target)| used >= fraction(target, 9, 10))
            || headroom.is_some_and(|(free, total)| free <= fraction(total, 1, 16));
        // Missing resident telemetry is not a healthy sample. A supported
        // headroom probe can still shrink admission, but cannot restore it
        // while the process' footprint is unknown. Initially unknown is a
        // no-op, preserving operation on unsupported platforms.
        let healthy = resident.is_some_and(|(used, target)| used <= fraction(target, 4, 5))
            && headroom.is_none_or(|(free, total)| free >= fraction(total, 1, 8));

        let reason = if severe {
            self.capacity_pct = 1;
            self.healthy_streak = 0;
            "resident_memory_severe"
        } else if pressured {
            self.capacity_pct = (self.capacity_pct / 2).max(1);
            self.healthy_streak = 0;
            "resident_memory_pressure"
        } else if healthy {
            self.healthy_streak = self.healthy_streak.saturating_add(1);
            if self.healthy_streak >= HEALTHY_TICKS_TO_GROW {
                self.healthy_streak = 0;
                self.capacity_pct = self.capacity_pct.saturating_add(10).min(100);
                "resident_memory_recovery"
            } else {
                "resident_memory_healthy_hold"
            }
        } else {
            self.healthy_streak = 0;
            if resident.is_none() {
                "resident_memory_unknown"
            } else {
                "resident_memory_hysteresis_hold"
            }
        };

        // Reserve half the observed slack for ungoverned allocations. The
        // floor is bounded by the configured ceiling, not added to it. It is
        // intentional that this can be positive with zero headroom: one small
        // drain operation must remain possible, and this is NOT an RSS quota.
        let mut byte_limit = max_inflight_bytes;
        if let Some((used, target)) = resident {
            byte_limit = byte_limit.min(target.saturating_sub(used) / 2);
        }
        if let Some((free, total)) = headroom {
            byte_limit = byte_limit.min(free.saturating_sub(fraction(total, 1, 32)) / 2);
        }
        if !healthy {
            // Losing telemetry or entering the deadband cannot reopen byte
            // admission while worker admission is deliberately held down.
            byte_limit = byte_limit.min(self.last_byte_limit.unwrap_or(max_inflight_bytes));
        }
        byte_limit = byte_limit.max(min_inflight_bytes.max(1).min(max_inflight_bytes));
        self.last_byte_limit = Some(byte_limit);
        Decision {
            capacity_pct: self.capacity_pct,
            inflight_byte_limit: byte_limit,
            target_resident_bytes: target,
            healthy_streak: self.healthy_streak,
            reason,
            sample,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    const CEILING: u64 = 512 * MIB;

    fn healthy() -> Sample {
        Sample {
            resident_bytes: Some(4 * GIB),
            available_bytes: Some(10 * GIB),
            total_bytes: Some(16 * GIB),
        }
    }

    #[test]
    fn ample_memory_preserves_requested_capacity_and_byte_ceiling() {
        let decision = Policy::default().observe(healthy(), None, CEILING, MIB);
        assert_eq!(decision.capacity_pct, 100);
        assert_eq!(decision.inflight_byte_limit, CEILING);
        assert_eq!(decision.target_resident_bytes, Some(14 * GIB));
    }

    #[test]
    fn effective_cgroup_total_controls_the_target_not_ample_host_memory() {
        let sample = Sample { available_bytes: Some(97 * GIB), resident_bytes: Some(14 * GIB), ..healthy() };
        let decision = Policy::default().observe(sample, None, CEILING, MIB);
        assert_eq!(decision.capacity_pct, 1);
        assert_eq!(decision.inflight_byte_limit, MIB);
        assert_eq!(decision.reason, "resident_memory_severe");
    }

    #[test]
    fn process_pressure_halves_repeatedly_without_a_cpu_signal() {
        let sample = Sample { resident_bytes: Some(13 * GIB), ..healthy() };
        let mut policy = Policy::default();
        for expected in [50, 25, 12, 6, 3, 1, 1] {
            assert_eq!(policy.observe(sample, None, CEILING, MIB).capacity_pct, expected);
        }
    }

    #[test]
    fn severe_headroom_pressure_engages_even_with_a_small_process() {
        let sample = Sample { available_bytes: Some(256 * MIB), ..healthy() };
        let decision = Policy::default().observe(sample, None, CEILING, MIB);
        assert_eq!(decision.capacity_pct, 1);
        assert_eq!(decision.inflight_byte_limit, MIB);
    }

    #[test]
    fn pressure_from_other_processes_also_reduces_admission() {
        let sample = Sample { available_bytes: Some(GIB), ..healthy() };
        let decision = Policy::default().observe(sample, None, CEILING, MIB);
        assert_eq!(decision.capacity_pct, 50);
        assert_eq!(decision.inflight_byte_limit, 256 * MIB);
    }

    #[test]
    fn byte_budget_reserves_slack_even_before_severe_pressure() {
        let sample = Sample { resident_bytes: Some(14 * GIB - 128 * MIB), ..healthy() };
        let decision = Policy::default().observe(sample, None, CEILING, MIB);
        assert_eq!(decision.capacity_pct, 50);
        assert_eq!(decision.inflight_byte_limit, 64 * MIB);
    }

    #[test]
    fn recovery_needs_three_new_healthy_samples_for_each_additive_step() {
        let mut policy = Policy::default();
        policy.observe(Sample { resident_bytes: Some(16 * GIB), ..healthy() }, None, CEILING, MIB);
        for expected in [1, 1, 11, 11, 11, 21] {
            assert_eq!(policy.observe(healthy(), None, CEILING, MIB).capacity_pct, expected);
        }
        for _ in 0..100 {
            policy.observe(healthy(), None, CEILING, MIB);
        }
        assert_eq!(policy.observe(healthy(), None, CEILING, MIB).capacity_pct, 100);
    }

    #[test]
    fn hysteresis_band_prevents_bounce_and_breaks_a_recovery_streak() {
        let mut policy = Policy::default();
        policy.observe(Sample { resident_bytes: Some(14 * GIB), ..healthy() }, None, CEILING, MIB);
        policy.observe(healthy(), None, CEILING, MIB);
        policy.observe(healthy(), None, CEILING, MIB);
        let band = Sample { resident_bytes: Some(12 * GIB), ..healthy() };
        for _ in 0..20 {
            let decision = policy.observe(band, None, CEILING, MIB);
            assert_eq!(decision.capacity_pct, 1);
            assert_eq!(decision.healthy_streak, 0);
            assert_eq!(decision.reason, "resident_memory_hysteresis_hold");
        }
        assert_eq!(policy.observe(healthy(), None, CEILING, MIB).capacity_pct, 1);
    }

    #[test]
    fn unavailable_telemetry_never_asserts_healthy_recovery() {
        let mut policy = Policy::default();
        let unknown = Policy::default().observe(Sample::default(), None, CEILING, MIB);
        assert_eq!(unknown.capacity_pct, 100);
        assert_eq!(unknown.inflight_byte_limit, CEILING);
        policy.observe(Sample { resident_bytes: Some(14 * GIB), ..healthy() }, None, CEILING, MIB);
        for _ in 0..20 {
            let decision = policy.observe(Sample { resident_bytes: None, ..healthy() }, None, CEILING, MIB);
            assert_eq!(decision.capacity_pct, 1);
            assert_eq!(decision.reason, "resident_memory_unknown");
            assert_eq!(decision.inflight_byte_limit, MIB);
        }
    }

    #[test]
    fn missing_resident_sample_does_not_mask_real_headroom_pressure() {
        let sample = Sample { resident_bytes: None, available_bytes: Some(0), ..healthy() };
        assert_eq!(Policy::default().observe(sample, None, CEILING, MIB).capacity_pct, 1);
    }

    #[test]
    fn explicit_limit_can_lower_but_not_raise_the_effective_memory_target() {
        assert_eq!(target_resident_bytes(Some(16 * GIB), Some(8 * GIB)), Some(8 * GIB));
        assert_eq!(target_resident_bytes(Some(16 * GIB), Some(32 * GIB)), Some(14 * GIB));
        assert_eq!(target_resident_bytes(Some(16 * GIB), Some(0)), Some(14 * GIB));
    }

    #[test]
    fn explicit_limit_works_without_a_total_memory_probe() {
        let sample = Sample { resident_bytes: Some(8 * GIB), total_bytes: None, available_bytes: None };
        let decision = Policy::default().observe(sample, Some(8 * GIB), CEILING, MIB);
        assert_eq!(decision.capacity_pct, 1);
        assert_eq!(decision.target_resident_bytes, Some(8 * GIB));
    }

    #[test]
    fn smaller_limits_are_not_raised_by_the_drain_floor() {
        let sample = Sample { resident_bytes: Some(16 * GIB), available_bytes: Some(0), ..healthy() };
        for ceiling in [0, 1, 512, MIB - 1, MIB, CEILING] {
            let decision = Policy::default().observe(sample, None, ceiling, MIB);
            assert_eq!(decision.inflight_byte_limit, MIB.min(ceiling));
        }
    }

    #[test]
    fn configured_drain_floor_is_respected_and_capped() {
        let sample = Sample { resident_bytes: Some(16 * GIB), ..healthy() };
        let decision = Policy::default().observe(sample, None, CEILING, 8 * MIB);
        assert_eq!(decision.inflight_byte_limit, 8 * MIB);
        let decision = Policy::default().observe(sample, None, MIB, 8 * MIB);
        assert_eq!(decision.inflight_byte_limit, MIB);
        assert_eq!(Policy::default().observe(sample, None, CEILING, 0).inflight_byte_limit, 1);
    }

    #[test]
    fn unknown_or_zero_totals_do_not_manufacture_a_zero_quota() {
        for total in [None, Some(0)] {
            let sample = Sample { total_bytes: total, ..healthy() };
            let decision = Policy::default().observe(sample, None, CEILING, MIB);
            assert_eq!(decision.target_resident_bytes, None);
            assert_eq!(decision.capacity_pct, 100);
        }
    }

    #[test]
    fn target_and_available_boundaries_are_inclusive() {
        let target = 14 * GIB;
        let pressure = fraction(target, 9, 10);
        assert_eq!(Policy::default().observe(Sample { resident_bytes: Some(pressure - 1), ..healthy() }, None, CEILING, MIB).capacity_pct, 100);
        assert_eq!(Policy::default().observe(Sample { resident_bytes: Some(pressure), ..healthy() }, None, CEILING, MIB).capacity_pct, 50);
        assert_eq!(Policy::default().observe(Sample { resident_bytes: Some(target), ..healthy() }, None, CEILING, MIB).capacity_pct, 1);
        assert_eq!(Policy::default().observe(Sample { available_bytes: Some(GIB / 2), ..healthy() }, None, CEILING, MIB).capacity_pct, 1);
    }

    #[test]
    fn arithmetic_is_bounded_at_u64_extremes() {
        for total in [1, 7, 8, 31, 32, u64::MAX - 1, u64::MAX] {
            for resident in [0, total / 2, total, u64::MAX] {
                let sample = Sample { resident_bytes: Some(resident), available_bytes: Some(total), total_bytes: Some(total) };
                let decision = Policy::default().observe(sample, None, u64::MAX, MIB);
                assert!((1..=100).contains(&decision.capacity_pct));
                assert!(decision.target_resident_bytes.unwrap() <= total);
                assert!(decision.inflight_byte_limit >= MIB);
            }
        }
        assert_eq!(fraction(u64::MAX, 7, 8), ((u64::MAX as u128 * 7) / 8) as u64);
    }

    #[test]
    fn increasing_resident_pressure_never_increases_fresh_admission() {
        let mut prior_capacity = 100;
        let mut prior_limit = CEILING;
        for step in 0..=4096 {
            let sample = Sample { resident_bytes: Some(step * 4 * MIB), ..healthy() };
            let decision = Policy::default().observe(sample, None, CEILING, MIB);
            assert!(decision.capacity_pct <= prior_capacity);
            assert!(decision.inflight_byte_limit <= prior_limit);
            prior_capacity = decision.capacity_pct;
            prior_limit = decision.inflight_byte_limit;
        }
    }
}
