//! Metric-integrity taxonomy: never silently smooth missing data into a zero,
//! and never ship a NaN/Inf as a success.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.15.4
//! ("Protect bookmarks analytics bakeoff pages update and TUI surfaces from
//! silent drift").
//!
//! The report found auxiliary-but-user-visible surfaces silently smoothing over
//! data, schema, or arithmetic problems: analytics aggregates returning zero
//! token counts indistinguishably from *no data*, bakeoff metrics dividing by a
//! zero baseline into `NaN`/`Inf` and reporting it as a passing score, and
//! charts masking intent with saturating-zero behaviour. A user who trusts a
//! "0" that actually means "we have no idea" makes the wrong call.
//!
//! This module is the unifying guard. [`MetricOutcome`] is the one taxonomy that
//! keeps a genuine zero distinct from missing data, a failed grouped aggregate,
//! a schema-incompatible rollup, a rebuild-required state, and invalid
//! (`NaN`/`Inf`/negative-denominator) input. The safe arithmetic helpers
//! ([`safe_ratio`], [`classify_aggregate`], [`bakeoff_quality_ratio`],
//! [`improvement_pct`]) compute through that taxonomy so a zero denominator or a
//! non-finite input becomes a structured no-data / invalid-input outcome —
//! never a `NaN`/`Inf` that reads as success. [`chart_cell`] renders the
//! distinction a chart/TUI must show: a missing value is `—`, a true zero is
//! `0`.
//!
//! Pure, side-effect-free logic. The analytics / bakeoff / chart / TUI surfaces
//! adopt these helpers (follow-on); here we pin the taxonomy and its tests.

use serde::{Deserialize, Serialize};

/// Stable schema version for the metric-outcome wire format.
pub const METRIC_INTEGRITY_SCHEMA_VERSION: u32 = 1;

/// The outcome of computing or rolling up a metric. The whole point is that a
/// genuine zero, an absence of data, a failed aggregate, a schema mismatch, a
/// rebuild-required state, and invalid input are **distinct** — never collapsed
/// into a bare `0` or a silent `NaN`.
///
/// Not `Eq`/`Ord` because it carries an `f64`; that float is always finite (the
/// constructors route `NaN`/`Inf` to [`MetricOutcome::InvalidInput`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetricOutcome {
    /// A real, finite value (which may legitimately be non-zero).
    Value(f64),
    /// Rows were present and the value is genuinely zero (e.g. zero tokens used).
    TrueZero,
    /// No rows / no data to compute from — distinct from a zero value.
    #[default]
    NoData,
    /// A grouped aggregate could not be computed (e.g. an engine limitation on
    /// mixed aggregates) — distinct from "no data".
    AggregateFailed,
    /// The underlying rollup schema is incompatible (drift) and must be migrated.
    SchemaIncompatible,
    /// Derived rollups are stale/absent and must be rebuilt before this metric
    /// is trustworthy.
    RebuildRequired,
    /// The inputs were invalid (`NaN`/`Inf`, or a negative denominator) — never
    /// reported as a passing value.
    InvalidInput,
}

impl MetricOutcome {
    /// Build a [`MetricOutcome::Value`] from a float, routing any non-finite
    /// value to [`MetricOutcome::InvalidInput`] so a `NaN`/`Inf` can never enter
    /// the `Value` channel.
    pub fn finite(value: f64) -> Self {
        if value.is_finite() {
            MetricOutcome::Value(value)
        } else {
            MetricOutcome::InvalidInput
        }
    }

    /// Stable kebab-case label for the *kind* (the `Value` payload is separate).
    pub const fn kind_str(self) -> &'static str {
        match self {
            MetricOutcome::Value(_) => "value",
            MetricOutcome::TrueZero => "true-zero",
            MetricOutcome::NoData => "no-data",
            MetricOutcome::AggregateFailed => "aggregate-failed",
            MetricOutcome::SchemaIncompatible => "schema-incompatible",
            MetricOutcome::RebuildRequired => "rebuild-required",
            MetricOutcome::InvalidInput => "invalid-input",
        }
    }

    /// The numeric value, when this is a real value or a true zero (a true zero
    /// reads as `0.0`). `None` for every not-a-number outcome (no-data, failed,
    /// schema, rebuild, invalid) — so a caller can never accidentally treat
    /// "missing" as `0`.
    pub fn as_value(self) -> Option<f64> {
        match self {
            MetricOutcome::Value(v) => Some(v),
            MetricOutcome::TrueZero => Some(0.0),
            _ => None,
        }
    }

    /// Whether this outcome carries a trustworthy number (a real value or a true
    /// zero).
    pub fn is_numeric(self) -> bool {
        matches!(self, MetricOutcome::Value(_) | MetricOutcome::TrueZero)
    }

    /// Whether this outcome means "we have no usable data here" (as opposed to a
    /// genuine zero) — the distinction a chart/TUI must not smooth over.
    pub fn is_no_data(self) -> bool {
        matches!(
            self,
            MetricOutcome::NoData
                | MetricOutcome::AggregateFailed
                | MetricOutcome::SchemaIncompatible
                | MetricOutcome::RebuildRequired
        )
    }
}

/// Divide safely through the taxonomy: a non-finite input is
/// [`MetricOutcome::InvalidInput`], a negative denominator is invalid, a zero
/// denominator is [`MetricOutcome::NoData`] (a ratio of "nothing" is not zero),
/// and a finite result is a [`MetricOutcome::Value`] (or [`MetricOutcome::TrueZero`]).
pub fn safe_ratio(numerator: f64, denominator: f64) -> MetricOutcome {
    if !numerator.is_finite() || !denominator.is_finite() || denominator < 0.0 {
        return MetricOutcome::InvalidInput;
    }
    if denominator == 0.0 {
        return MetricOutcome::NoData;
    }
    let result = numerator / denominator;
    if !result.is_finite() {
        return MetricOutcome::InvalidInput;
    }
    if result == 0.0 {
        MetricOutcome::TrueZero
    } else {
        MetricOutcome::Value(result)
    }
}

/// Classify an aggregate over `row_count` rows whose summed value is `sum`. Zero
/// rows is [`MetricOutcome::NoData`] (not zero); a non-finite sum is invalid; a
/// finite zero sum over present rows is a genuine [`MetricOutcome::TrueZero`].
pub fn classify_aggregate(row_count: u64, sum: f64) -> MetricOutcome {
    if row_count == 0 {
        return MetricOutcome::NoData;
    }
    if !sum.is_finite() {
        return MetricOutcome::InvalidInput;
    }
    if sum == 0.0 {
        MetricOutcome::TrueZero
    } else {
        MetricOutcome::Value(sum)
    }
}

/// The bakeoff quality ratio (candidate relative to baseline). An empty/zero or
/// non-positive baseline is [`MetricOutcome::NoData`] — you cannot express a
/// fraction "of nothing", and dividing by it would be the `NaN`/`Inf` that read
/// as a passing score. Non-finite inputs are invalid.
pub fn bakeoff_quality_ratio(baseline: f64, candidate: f64) -> MetricOutcome {
    if !baseline.is_finite() || !candidate.is_finite() {
        return MetricOutcome::InvalidInput;
    }
    if baseline <= 0.0 {
        return MetricOutcome::NoData;
    }
    safe_ratio(candidate, baseline)
}

/// Percentage improvement of `candidate` over `baseline`, through the same
/// guard: a non-positive baseline is no-data rather than an infinite percentage.
pub fn improvement_pct(baseline: f64, candidate: f64) -> MetricOutcome {
    if !baseline.is_finite() || !candidate.is_finite() {
        return MetricOutcome::InvalidInput;
    }
    if baseline <= 0.0 {
        return MetricOutcome::NoData;
    }
    MetricOutcome::finite((candidate - baseline) / baseline * 100.0)
}

/// How a chart/TUI cell must render an outcome so a missing value is never shown
/// as `0`. A real value/true-zero shows its number; every no-data/invalid
/// outcome shows the explicit `—` placeholder.
pub fn chart_cell(outcome: MetricOutcome) -> String {
    match outcome {
        MetricOutcome::Value(v) => format_value(v),
        MetricOutcome::TrueZero => "0".to_string(),
        _ => "—".to_string(),
    }
}

/// Format a finite value compactly (integers without a trailing `.0`).
fn format_value(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v:.4}")
    }
}

/// Clock-rollback-safe elapsed milliseconds: if `last_ms` is in the future of
/// `now_ms` (a clock rollback), return `0` rather than panicking or wrapping.
pub fn saturating_age_ms(now_ms: u64, last_ms: u64) -> u64 {
    now_ms.saturating_sub(last_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- the central distinction: zero vs missing -------------------------

    #[test]
    fn aggregate_over_zero_rows_is_no_data_not_zero() {
        let outcome = classify_aggregate(0, 0.0);
        assert_eq!(outcome, MetricOutcome::NoData);
        assert!(outcome.is_no_data());
        assert!(!outcome.is_numeric());
        assert_eq!(outcome.as_value(), None, "missing must never read as 0");
    }

    #[test]
    fn aggregate_over_present_rows_summing_zero_is_true_zero() {
        let outcome = classify_aggregate(5, 0.0);
        assert_eq!(outcome, MetricOutcome::TrueZero);
        assert!(outcome.is_numeric());
        assert!(!outcome.is_no_data());
        assert_eq!(outcome.as_value(), Some(0.0));
    }

    #[test]
    fn aggregate_with_a_real_sum_is_a_value() {
        assert_eq!(classify_aggregate(3, 1234.0), MetricOutcome::Value(1234.0));
    }

    #[test]
    fn aggregate_with_non_finite_sum_is_invalid() {
        assert_eq!(classify_aggregate(3, f64::NAN), MetricOutcome::InvalidInput);
        assert_eq!(
            classify_aggregate(3, f64::INFINITY),
            MetricOutcome::InvalidInput
        );
    }

    // --- safe ratio: never NaN/Inf ----------------------------------------

    #[test]
    fn ratio_by_zero_is_no_data_never_nan_or_inf() {
        let outcome = safe_ratio(5.0, 0.0);
        assert_eq!(outcome, MetricOutcome::NoData);
        assert_eq!(outcome.as_value(), None);
    }

    #[test]
    fn ratio_zero_over_positive_is_true_zero() {
        assert_eq!(safe_ratio(0.0, 4.0), MetricOutcome::TrueZero);
    }

    #[test]
    fn ratio_negative_denominator_and_non_finite_are_invalid() {
        assert_eq!(safe_ratio(1.0, -2.0), MetricOutcome::InvalidInput);
        assert_eq!(safe_ratio(f64::NAN, 2.0), MetricOutcome::InvalidInput);
        assert_eq!(safe_ratio(1.0, f64::INFINITY), MetricOutcome::InvalidInput);
    }

    #[test]
    fn ratio_finite_value_is_a_value() {
        assert_eq!(safe_ratio(3.0, 4.0), MetricOutcome::Value(0.75));
    }

    // --- bakeoff: zero baseline never NaN/Inf "pass" ----------------------

    #[test]
    fn bakeoff_zero_or_negative_baseline_is_no_data_not_a_passing_score() {
        // The exact reported bug: NDCG/quality baseline of 0.0 must not divide
        // into a NaN/Inf that reads as a pass.
        assert_eq!(bakeoff_quality_ratio(0.0, 0.9), MetricOutcome::NoData);
        assert_eq!(bakeoff_quality_ratio(-1.0, 0.9), MetricOutcome::NoData);
        assert_eq!(improvement_pct(0.0, 0.9), MetricOutcome::NoData);
        // None of these may surface a number.
        assert_eq!(bakeoff_quality_ratio(0.0, 0.9).as_value(), None);
    }

    #[test]
    fn bakeoff_real_baseline_computes_ratio_and_improvement() {
        // 3/4 = 0.75 is exact in f64, so an equality assertion is safe here.
        assert_eq!(bakeoff_quality_ratio(4.0, 3.0), MetricOutcome::Value(0.75));
        // 0.8 -> 0.88 is +10% (compared approximately for float safety).
        let improved = improvement_pct(0.8, 0.88)
            .as_value()
            .expect("a real-baseline improvement is a value");
        assert!((improved - 10.0).abs() < 1e-9, "got {improved}");
    }

    #[test]
    fn bakeoff_non_finite_inputs_are_invalid() {
        assert_eq!(
            bakeoff_quality_ratio(f64::NAN, 1.0),
            MetricOutcome::InvalidInput
        );
        assert_eq!(
            improvement_pct(1.0, f64::INFINITY),
            MetricOutcome::InvalidInput
        );
    }

    #[test]
    fn finite_constructor_routes_non_finite_to_invalid() {
        assert_eq!(MetricOutcome::finite(3.5), MetricOutcome::Value(3.5));
        assert_eq!(MetricOutcome::finite(f64::NAN), MetricOutcome::InvalidInput);
        assert_eq!(
            MetricOutcome::finite(f64::INFINITY),
            MetricOutcome::InvalidInput
        );
    }

    // --- chart rendering: missing is — not 0 ------------------------------

    #[test]
    fn chart_cell_shows_dash_for_missing_and_zero_for_true_zero() {
        assert_eq!(chart_cell(MetricOutcome::NoData), "—");
        assert_eq!(chart_cell(MetricOutcome::AggregateFailed), "—");
        assert_eq!(chart_cell(MetricOutcome::SchemaIncompatible), "—");
        assert_eq!(chart_cell(MetricOutcome::RebuildRequired), "—");
        assert_eq!(chart_cell(MetricOutcome::InvalidInput), "—");
        assert_eq!(chart_cell(MetricOutcome::TrueZero), "0");
        assert_eq!(chart_cell(MetricOutcome::Value(42.0)), "42");
        assert_eq!(chart_cell(MetricOutcome::Value(0.75)), "0.7500");
    }

    #[test]
    fn no_data_variants_are_classified_as_no_data() {
        for o in [
            MetricOutcome::NoData,
            MetricOutcome::AggregateFailed,
            MetricOutcome::SchemaIncompatible,
            MetricOutcome::RebuildRequired,
        ] {
            assert!(o.is_no_data(), "{} should be no-data", o.kind_str());
            assert!(!o.is_numeric());
            assert_eq!(o.as_value(), None);
        }
        // InvalidInput is not "no data" — it is bad input — but is still not numeric.
        assert!(!MetricOutcome::InvalidInput.is_no_data());
        assert!(!MetricOutcome::InvalidInput.is_numeric());
    }

    // --- clock-rollback safe time math ------------------------------------

    #[test]
    fn saturating_age_handles_clock_rollback() {
        assert_eq!(saturating_age_ms(1_000, 400), 600);
        // last_ms in the future of now_ms (clock rolled back) -> 0, no wrap/panic.
        assert_eq!(saturating_age_ms(400, 1_000), 0);
        assert_eq!(saturating_age_ms(0, u64::MAX), 0);
    }

    // --- serialization stability ------------------------------------------

    #[test]
    fn outcome_serializes_with_stable_kebab_kinds_and_round_trips() {
        // Unit variants serialize as their kebab label.
        assert_eq!(
            serde_json::to_string(&MetricOutcome::NoData).expect("ser"),
            "\"no-data\""
        );
        assert_eq!(
            serde_json::to_string(&MetricOutcome::SchemaIncompatible).expect("ser"),
            "\"schema-incompatible\""
        );
        // The value variant carries its float.
        let v = serde_json::to_value(MetricOutcome::Value(12.5)).expect("to_value");
        assert_eq!(v["value"], 12.5);
        let back: MetricOutcome = serde_json::from_value(v).expect("round-trip");
        assert_eq!(back, MetricOutcome::Value(12.5));
        // kind_str matches the wire kind for unit variants.
        assert_eq!(MetricOutcome::TrueZero.kind_str(), "true-zero");
        assert_eq!(
            MetricOutcome::RebuildRequired.kind_str(),
            "rebuild-required"
        );
    }
}
