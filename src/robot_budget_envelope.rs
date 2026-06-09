//! Bounded execution budgets and partial/error envelopes for slow robot surfaces.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.2.2
//! ("Add bounded execution budgets and partial/error envelopes for slow robot
//! surfaces").
//!
//! This module is the shared **foundation** for that bead: a reusable execution
//! budget ([`RobotBudget`]) and a generic partial/error envelope
//! ([`BudgetEnvelope`]) that any robot surface (status, doctor check, view,
//! search, triage, pack, fleet probes) can wrap its payload in. When rich work
//! would exceed the budget, the surface returns a *partial* JSON document that
//! still carries `elapsed_ms`, `timed_out`, `budget_ms`, the `skipped_sections`,
//! a `recommended_next_probe`, and whatever facts completed — enough for an agent
//! to act safely instead of hanging or getting truncated garbage.
//!
//! The decision logic is pure ([`budget_status`]) so it is deterministic and unit
//! testable without wall-clock dependence; [`RobotBudget`] wraps a real
//! [`Instant`] for production use but delegates its phase decision to that pure
//! function. Per the bead's safety requirement, this type is inert data — it
//! never launches background work; a read-only probe stays read-only.
//!
//! Wiring each surface to populate and emit this envelope (plus slow-path
//! fixtures and per-surface E2E) is the remaining work tracked under 2.2; this
//! commit lands the contract the wiring builds on.

use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Stable schema version for the budget-envelope wire format.
pub const BUDGET_ENVELOPE_SCHEMA_VERSION: u32 = 1;

/// Fraction of the budget at which a surface should start shedding optional
/// sections rather than risk blowing the deadline. 80%.
pub const NEAR_LIMIT_FRACTION: f64 = 0.8;

/// The phase of budget consumption, derived purely from elapsed vs. total.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum BudgetPhase {
    /// Comfortably within budget; do the full work.
    Healthy,
    /// Past [`NEAR_LIMIT_FRACTION`]; shed optional/expensive sections.
    NearLimit,
    /// Budget reached or exceeded; stop and return what you have.
    Exhausted,
}

/// Pure budget-phase decision. `budget_ms == 0` means "no time" → always
/// [`BudgetPhase::Exhausted`]. Deterministic and the single source of truth for
/// [`RobotBudget`].
pub fn budget_status(elapsed_ms: u64, budget_ms: u64) -> BudgetPhase {
    if budget_ms == 0 || elapsed_ms >= budget_ms {
        return BudgetPhase::Exhausted;
    }
    // elapsed/budget >= NEAR_LIMIT_FRACTION, computed without floats losing
    // precision on large values: elapsed * 100 >= budget * (fraction*100).
    let near_threshold = (budget_ms as u128 * (NEAR_LIMIT_FRACTION * 100.0) as u128) / 100;
    if (elapsed_ms as u128) >= near_threshold {
        BudgetPhase::NearLimit
    } else {
        BudgetPhase::Healthy
    }
}

/// A wall-clock execution budget for a single robot invocation. Construct at the
/// start of the surface's work; query [`phase`](Self::phase) /
/// [`is_exhausted`](Self::is_exhausted) before each expensive section.
#[derive(Debug, Clone, Copy)]
pub struct RobotBudget {
    total_ms: u64,
    start: Instant,
}

impl RobotBudget {
    /// Start a budget of `total_ms` milliseconds from now.
    pub fn new(total_ms: u64) -> Self {
        Self {
            total_ms,
            start: Instant::now(),
        }
    }

    /// Start a budget with an explicit start instant (for composing with an
    /// already-running timer).
    pub fn with_start(total_ms: u64, start: Instant) -> Self {
        Self { total_ms, start }
    }

    /// The configured total budget.
    pub fn total_ms(&self) -> u64 {
        self.total_ms
    }

    /// Milliseconds elapsed since the budget started.
    pub fn elapsed_ms(&self) -> u64 {
        // Saturate at u64::MAX; millis as u128 -> u64 is safe for any realistic run.
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Milliseconds remaining (saturating at 0).
    pub fn remaining_ms(&self) -> u64 {
        self.total_ms.saturating_sub(self.elapsed_ms())
    }

    /// Current budget phase.
    pub fn phase(&self) -> BudgetPhase {
        budget_status(self.elapsed_ms(), self.total_ms)
    }

    /// `true` once the budget is reached or exceeded.
    pub fn is_exhausted(&self) -> bool {
        self.phase() == BudgetPhase::Exhausted
    }

    /// `true` if there is still healthy headroom for an expensive section.
    pub fn is_healthy(&self) -> bool {
        self.phase() == BudgetPhase::Healthy
    }
}

/// Outcome of a budgeted robot operation.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum BudgetOutcome {
    /// All requested work completed within budget.
    Complete,
    /// Some sections were skipped/shed but the surface returned proactively
    /// (it did not hit the hard deadline).
    Partial,
    /// The hard deadline was reached; remaining work was abandoned.
    TimedOut,
}

/// A generic partial/error envelope around a robot payload. `data` always
/// carries whatever facts completed, even on timeout, so consumers can act on a
/// partial result instead of nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetEnvelope<T> {
    /// Mirrors [`BUDGET_ENVELOPE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Outcome classification.
    pub outcome: BudgetOutcome,
    /// `true` iff `outcome == TimedOut` (mirrors it for boolean-only consumers).
    pub timed_out: bool,
    /// Wall-clock spent on the operation.
    pub elapsed_ms: u64,
    /// The budget the operation was given.
    pub budget_ms: u64,
    /// Named sections that were skipped/shed (e.g. `"semantic"`, `"remote"`),
    /// making a partial result honest rather than indistinguishable from full.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_sections: Vec<String>,
    /// The cheaper/bounded probe to run next to obtain the skipped facts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_next_probe: Option<String>,
    /// The payload — complete or partial.
    pub data: T,
}

impl<T> BudgetEnvelope<T> {
    /// A complete result: no sections skipped, not timed out.
    pub fn complete(data: T, elapsed_ms: u64, budget_ms: u64) -> Self {
        Self {
            schema_version: BUDGET_ENVELOPE_SCHEMA_VERSION,
            outcome: BudgetOutcome::Complete,
            timed_out: false,
            elapsed_ms,
            budget_ms,
            skipped_sections: Vec::new(),
            recommended_next_probe: None,
            data,
        }
    }

    /// A partial result returned proactively (sections shed before the deadline).
    pub fn partial(data: T, elapsed_ms: u64, budget_ms: u64, skipped_sections: Vec<String>) -> Self {
        Self {
            schema_version: BUDGET_ENVELOPE_SCHEMA_VERSION,
            outcome: BudgetOutcome::Partial,
            timed_out: false,
            elapsed_ms,
            budget_ms,
            skipped_sections,
            recommended_next_probe: None,
            data,
        }
    }

    /// A result abandoned at the hard deadline. `data` is whatever completed.
    pub fn timed_out(
        data: T,
        elapsed_ms: u64,
        budget_ms: u64,
        skipped_sections: Vec<String>,
    ) -> Self {
        Self {
            schema_version: BUDGET_ENVELOPE_SCHEMA_VERSION,
            outcome: BudgetOutcome::TimedOut,
            timed_out: true,
            elapsed_ms,
            budget_ms,
            skipped_sections,
            recommended_next_probe: None,
            data,
        }
    }

    /// Build directly from a [`RobotBudget`], choosing complete/partial/timed-out
    /// from the budget phase and the skipped set: any skipped sections with an
    /// exhausted budget is a timeout; skipped without exhaustion is partial;
    /// nothing skipped is complete.
    pub fn from_budget(data: T, budget: &RobotBudget, skipped_sections: Vec<String>) -> Self {
        let elapsed_ms = budget.elapsed_ms();
        let budget_ms = budget.total_ms();
        let exhausted = budget.is_exhausted();
        match (skipped_sections.is_empty(), exhausted) {
            (true, false) => Self::complete(data, elapsed_ms, budget_ms),
            (_, true) => Self::timed_out(data, elapsed_ms, budget_ms, skipped_sections),
            (false, false) => Self::partial(data, elapsed_ms, budget_ms, skipped_sections),
        }
    }

    /// Record a skipped section (chainable).
    pub fn skip_section(mut self, name: impl Into<String>) -> Self {
        self.skipped_sections.push(name.into());
        self
    }

    /// Attach the recommended next probe (chainable).
    pub fn with_next_probe(mut self, probe: impl Into<String>) -> Self {
        self.recommended_next_probe = Some(probe.into());
        self
    }

    /// `true` if the result is anything less than fully complete.
    pub fn is_degraded(&self) -> bool {
        self.outcome != BudgetOutcome::Complete
    }

    /// Map the payload while preserving all budget metadata.
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> BudgetEnvelope<U> {
        BudgetEnvelope {
            schema_version: self.schema_version,
            outcome: self.outcome,
            timed_out: self.timed_out,
            elapsed_ms: self.elapsed_ms,
            budget_ms: self.budget_ms,
            skipped_sections: self.skipped_sections,
            recommended_next_probe: self.recommended_next_probe,
            data: f(self.data),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn budget_status_is_pure_and_covers_phases() {
        assert_eq!(budget_status(0, 1000), BudgetPhase::Healthy);
        assert_eq!(budget_status(799, 1000), BudgetPhase::Healthy);
        assert_eq!(budget_status(800, 1000), BudgetPhase::NearLimit);
        assert_eq!(budget_status(999, 1000), BudgetPhase::NearLimit);
        assert_eq!(budget_status(1000, 1000), BudgetPhase::Exhausted);
        assert_eq!(budget_status(5000, 1000), BudgetPhase::Exhausted);
        // Zero budget is always exhausted.
        assert_eq!(budget_status(0, 0), BudgetPhase::Exhausted);
    }

    #[test]
    fn zero_budget_is_immediately_exhausted() {
        let b = RobotBudget::new(0);
        assert!(b.is_exhausted());
        assert!(!b.is_healthy());
        assert_eq!(b.remaining_ms(), 0);
    }

    #[test]
    fn large_budget_starts_healthy_with_headroom() {
        let b = RobotBudget::new(60_000);
        // Immediately after construction we are well within budget.
        assert!(!b.is_exhausted());
        assert!(b.remaining_ms() <= 60_000);
        assert!(b.remaining_ms() > 50_000, "should have most of the budget left");
        assert_eq!(b.total_ms(), 60_000);
    }

    #[test]
    fn complete_envelope_has_no_skips_and_is_not_degraded() {
        let env = BudgetEnvelope::complete(json!({"ready": true}), 12, 8000);
        assert_eq!(env.outcome, BudgetOutcome::Complete);
        assert!(!env.timed_out);
        assert!(!env.is_degraded());
        assert!(env.skipped_sections.is_empty());
    }

    #[test]
    fn timed_out_envelope_keeps_partial_data_and_flags() {
        let env = BudgetEnvelope::timed_out(
            json!({"index": "ok"}),
            8001,
            8000,
            vec!["semantic".to_string(), "remote".to_string()],
        )
        .with_next_probe("cass health --json");
        assert_eq!(env.outcome, BudgetOutcome::TimedOut);
        assert!(env.timed_out);
        assert!(env.is_degraded());
        let value = serde_json::to_value(&env).unwrap();
        assert_eq!(value["outcome"], "timed-out");
        assert_eq!(value["timed_out"], true);
        assert_eq!(value["elapsed_ms"], 8001);
        assert_eq!(value["budget_ms"], 8000);
        assert_eq!(value["skipped_sections"][0], "semantic");
        assert_eq!(value["recommended_next_probe"], "cass health --json");
        // Partial data survives the timeout.
        assert_eq!(value["data"]["index"], "ok");
    }

    #[test]
    fn partial_envelope_is_proactive_not_timed_out() {
        let env = BudgetEnvelope::partial(json!({}), 6500, 8000, vec!["pack".to_string()]);
        assert_eq!(env.outcome, BudgetOutcome::Partial);
        assert!(!env.timed_out);
        assert!(env.is_degraded());
    }

    #[test]
    fn from_budget_chooses_outcome() {
        // Complete: nothing skipped, budget not exhausted.
        let healthy = RobotBudget::new(60_000);
        let c = BudgetEnvelope::from_budget(json!({}), &healthy, vec![]);
        assert_eq!(c.outcome, BudgetOutcome::Complete);
        // Partial: skipped but not exhausted.
        let p = BudgetEnvelope::from_budget(json!({}), &healthy, vec!["x".to_string()]);
        assert_eq!(p.outcome, BudgetOutcome::Partial);
        // Timed out: exhausted budget.
        let dead = RobotBudget::new(0);
        let t = BudgetEnvelope::from_budget(json!({}), &dead, vec!["x".to_string()]);
        assert_eq!(t.outcome, BudgetOutcome::TimedOut);
        assert!(t.timed_out);
    }

    #[test]
    fn skip_section_chains_and_accumulates() {
        let env = BudgetEnvelope::partial(json!({}), 100, 8000, vec![])
            .skip_section("a")
            .skip_section("b");
        assert_eq!(env.skipped_sections, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn map_preserves_budget_metadata() {
        let env = BudgetEnvelope::timed_out(5u32, 9000, 8000, vec!["s".to_string()]);
        let mapped = env.map(|n| n.to_string());
        assert_eq!(mapped.data, "5");
        assert_eq!(mapped.outcome, BudgetOutcome::TimedOut);
        assert!(mapped.timed_out);
        assert_eq!(mapped.elapsed_ms, 9000);
        assert_eq!(mapped.skipped_sections, vec!["s".to_string()]);
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let env = BudgetEnvelope::partial(
            json!({"k": 1}),
            7000,
            8000,
            vec!["semantic".to_string()],
        )
        .with_next_probe("cass triage --json");
        let value = serde_json::to_value(&env).unwrap();
        let back: BudgetEnvelope<serde_json::Value> = serde_json::from_value(value).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn outcome_and_phase_wire_values_are_kebab() {
        assert_eq!(serde_json::to_string(&BudgetOutcome::TimedOut).unwrap(), "\"timed-out\"");
        assert_eq!(serde_json::to_string(&BudgetOutcome::Partial).unwrap(), "\"partial\"");
        assert_eq!(serde_json::to_string(&BudgetOutcome::Complete).unwrap(), "\"complete\"");
        assert_eq!(serde_json::to_string(&BudgetPhase::NearLimit).unwrap(), "\"near-limit\"");
        assert_eq!(serde_json::to_string(&BudgetPhase::Exhausted).unwrap(), "\"exhausted\"");
    }
}
