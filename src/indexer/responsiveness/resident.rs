//! Apply resident-memory feedback to the existing responsiveness sampler.
//! No extra thread, model load or foreground OS probe is introduced here.

use super::{
    GovernorConfig, available_memory_bytes, process_resident_memory_bytes, total_memory_bytes,
};
use std::sync::{LazyLock, Mutex};

mod policy;
use policy::{Decision, Policy, Sample};

#[derive(Default)]
struct State {
    policy: Policy,
    last: Option<Decision>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));
static EXPLICIT_LIMIT: LazyLock<Option<u64>> = LazyLock::new(|| {
    dotenvy::var("CASS_RESPONSIVENESS_MAX_RSS_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|limit| *limit > 0)
});

/// Called only by the existing background sampler. Sample outside the lock:
/// Darwin's bounded footprint probe must never block a foreground admission.
pub(super) fn sample(cfg: &GovernorConfig) {
    if cfg.disabled || super::disabled_via_env() {
        let mut state = STATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = State::default();
        drop(state);
        crate::indexer::redact_secrets::update_redaction_retention_limit(usize::MAX);
        return;
    }
    let observation = Sample {
        resident_bytes: process_resident_memory_bytes(),
        available_bytes: available_memory_bytes(),
        total_bytes: total_memory_bytes(),
    };
    let mut state = STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let prior = state.last;
    let decision = state.policy.observe(
        observation,
        *EXPLICIT_LIMIT,
        u64::try_from(cfg.max_inflight_bytes).unwrap_or(u64::MAX),
        u64::try_from(cfg.min_inflight_bytes).unwrap_or(u64::MAX),
    );
    state.last = Some(decision);
    drop(state);
    // Redaction workers share one retained-value allowance. Shrink it from
    // memory feedback, not CPU pressure (evicting useful results merely for
    // CPU load would make their expensive redaction work run more often).
    // Workers release their own entries; the sampler never locks a cache or
    // touches its values, and leases continue accounting for idle owners.
    crate::indexer::redact_secrets::update_redaction_retention_limit(
        usize::try_from(decision.inflight_byte_limit).unwrap_or(usize::MAX),
    );
    // Do not emit the same pressure warning on every tick. A change of state
    // or capacity is useful evidence; continuously varying RSS is not a log.
    if prior.map_or(100, |prior| prior.capacity_pct) != decision.capacity_pct {
        tracing::info!(
            memory_capacity_pct = decision.capacity_pct,
            memory_inflight_byte_limit = decision.inflight_byte_limit,
            target_resident_bytes = ?decision.target_resident_bytes,
            resident_bytes = ?observation.resident_bytes,
            available_bytes = ?observation.available_bytes,
            effective_total_bytes = ?observation.total_bytes,
            reason = decision.reason,
            "resident-memory admission updated"
        );
    }
}

pub(super) fn capacity_pct() -> u32 {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .last
        .map_or(100, |last| last.capacity_pct)
}

pub(super) fn limit_inflight_bytes(desired: usize) -> usize {
    if super::disabled_via_env() {
        return desired;
    }
    let limit = STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .last
        .map_or(u64::MAX, |last| last.inflight_byte_limit);
    desired.min(usize::try_from(limit).unwrap_or(usize::MAX))
}

/// Passive inspection only: no filesystem access or thread initialization.
/// `None` means this process has not taken a memory observation, not healthy.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub(crate) struct MemoryTelemetry {
    pub capacity_pct: u32,
    pub inflight_byte_limit: u64,
    pub target_resident_bytes: Option<u64>,
    pub resident_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    pub effective_total_bytes: Option<u64>,
    pub healthy_streak: u8,
    pub reason: &'static str,
}

pub(super) fn telemetry() -> Option<MemoryTelemetry> {
    if super::disabled_via_env() {
        return None;
    }
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .last
        .map(|last| MemoryTelemetry {
            capacity_pct: last.capacity_pct,
            inflight_byte_limit: last.inflight_byte_limit,
            target_resident_bytes: last.target_resident_bytes,
            resident_bytes: last.sample.resident_bytes,
            available_bytes: last.sample.available_bytes,
            effective_total_bytes: last.sample.total_bytes,
            healthy_streak: last.healthy_streak,
            reason: last.reason,
        })
}
