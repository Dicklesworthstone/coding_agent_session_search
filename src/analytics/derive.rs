//! Derived metric computation for analytics buckets.
//!
//! All division operations use the shared metric-integrity taxonomy so a real
//! zero remains distinct from no data or invalid input. Legacy `Option<f64>`
//! projections are retained for wire compatibility, but are always derived
//! from the corresponding [`MetricOutcome`].

use super::types::{DerivedMetrics, UsageBucket};
use crate::metric_integrity::{MetricOutcome, safe_ratio};

/// Compute all derived metrics from a [`UsageBucket`].
pub fn compute_derived(bucket: &UsageBucket) -> DerivedMetrics {
    let api_coverage_pct = safe_pct(bucket.api_coverage_message_count, bucket.message_count);

    let api_tokens_per_assistant_msg_outcome =
        safe_count_ratio(bucket.api_tokens_total, bucket.assistant_message_count, 1.0);
    let api_tokens_per_assistant_msg = api_tokens_per_assistant_msg_outcome.as_value();

    let content_tokens_per_user_msg_outcome = safe_count_ratio(
        bucket.content_tokens_est_total,
        bucket.user_message_count,
        1.0,
    );
    let content_tokens_per_user_msg = content_tokens_per_user_msg_outcome.as_value();

    let tool_calls_per_1k_api_tokens_outcome =
        safe_count_ratio(bucket.tool_call_count, bucket.api_tokens_total, 1000.0);
    let tool_calls_per_1k_api_tokens = tool_calls_per_1k_api_tokens_outcome.as_value();

    let tool_calls_per_1k_content_tokens_outcome = safe_count_ratio(
        bucket.tool_call_count,
        bucket.content_tokens_est_total,
        1000.0,
    );
    let tool_calls_per_1k_content_tokens = tool_calls_per_1k_content_tokens_outcome.as_value();

    let plan_message_pct_outcome =
        safe_count_ratio(bucket.plan_message_count, bucket.message_count, 100.0);
    let plan_message_pct = plan_message_pct_outcome.as_value();

    let plan_token_share_content_outcome = safe_count_ratio(
        bucket.plan_content_tokens_est_total,
        bucket.content_tokens_est_total,
        1.0,
    );
    let plan_token_share_content = plan_token_share_content_outcome.as_value();
    let plan_token_share_api_outcome =
        safe_count_ratio(bucket.plan_api_tokens_total, bucket.api_tokens_total, 1.0);
    let plan_token_share_api = plan_token_share_api_outcome.as_value();

    DerivedMetrics {
        api_coverage_pct,
        api_tokens_per_assistant_msg,
        api_tokens_per_assistant_msg_outcome,
        content_tokens_per_user_msg,
        content_tokens_per_user_msg_outcome,
        tool_calls_per_1k_api_tokens,
        tool_calls_per_1k_api_tokens_outcome,
        tool_calls_per_1k_content_tokens,
        tool_calls_per_1k_content_tokens_outcome,
        plan_message_pct,
        plan_message_pct_outcome,
        plan_token_share_content,
        plan_token_share_content_outcome,
        plan_token_share_api,
        plan_token_share_api_outcome,
    }
}

/// Safely scale a ratio over non-negative count inputs.
///
/// Negative counters indicate corrupt or incompatible aggregate input and are
/// therefore invalid rather than a successful negative metric. A zero
/// denominator is no-data; a zero numerator over a positive denominator is a
/// genuine true-zero.
fn safe_count_ratio(numerator: i64, denominator: i64, scale: f64) -> MetricOutcome {
    if numerator < 0 || denominator < 0 || !scale.is_finite() || scale <= 0.0 {
        return MetricOutcome::InvalidInput;
    }

    match safe_ratio(numerator as f64, denominator as f64) {
        MetricOutcome::Value(value) => MetricOutcome::finite(value * scale),
        MetricOutcome::TrueZero => MetricOutcome::TrueZero,
        other => other,
    }
}

/// Percentage classified through the shared metric-integrity taxonomy. A zero
/// denominator is `no-data`, not a manufactured `0`; a genuine zero numerator
/// over present rows is `true-zero`. Numeric results are rounded to 2 places.
pub fn safe_pct(numerator: i64, denominator: i64) -> MetricOutcome {
    let outcome = safe_count_ratio(numerator, denominator, 100.0);
    match outcome {
        MetricOutcome::Value(value) => MetricOutcome::finite((value * 100.0).round() / 100.0),
        MetricOutcome::TrueZero => MetricOutcome::TrueZero,
        other => other,
    }
}

/// Safe division returning `None` when the denominator is zero.
pub fn safe_div(numerator: i64, denominator: i64) -> Option<f64> {
    if denominator == 0 {
        None
    } else {
        Some(numerator as f64 / denominator as f64)
    }
}

/// Safe division for f64 numerator with i64 denominator.
/// Returns `None` when the denominator is zero.
pub fn safe_div_f64(numerator: f64, denominator: i64) -> Option<f64> {
    if denominator == 0 {
        None
    } else {
        Some(numerator / denominator as f64)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_div_zero_denominator() {
        assert_eq!(safe_div(100, 0), None);
    }

    #[test]
    fn safe_div_normal() {
        assert_eq!(safe_div(100, 50), Some(2.0));
    }

    #[test]
    fn safe_div_f64_zero_denominator() {
        assert_eq!(safe_div_f64(1.50, 0), None);
    }

    #[test]
    fn safe_div_f64_normal() {
        let result = safe_div_f64(3.0, 2);
        assert_eq!(result, Some(1.5));
    }

    #[test]
    fn safe_pct_zero_denominator() {
        assert_eq!(safe_pct(50, 0), MetricOutcome::NoData);
    }

    #[test]
    fn safe_pct_normal() {
        let pct = safe_pct(75, 100).as_value().expect("numeric percentage");
        assert!((pct - 75.0).abs() < 0.01);
    }

    #[test]
    fn safe_pct_rounding() {
        // 1/3 = 33.333...% → should round to 33.33
        let pct = safe_pct(1, 3).as_value().expect("numeric percentage");
        assert!((pct - 33.33).abs() < 0.01);
    }

    #[test]
    fn safe_pct_rejects_negative_counters() {
        assert_eq!(safe_pct(-1, 10), MetricOutcome::InvalidInput);
        assert_eq!(safe_pct(1, -10), MetricOutcome::InvalidInput);
    }

    #[test]
    fn compute_derived_empty_bucket() {
        let bucket = UsageBucket::default();
        let d = compute_derived(&bucket);
        assert_eq!(d.api_coverage_pct, MetricOutcome::NoData);
        assert_eq!(d.api_tokens_per_assistant_msg, None);
        assert_eq!(d.content_tokens_per_user_msg, None);
        assert_eq!(d.tool_calls_per_1k_api_tokens, None);
        assert_eq!(d.tool_calls_per_1k_content_tokens, None);
        assert_eq!(d.plan_message_pct, None);
        assert_eq!(d.plan_token_share_content, None);
        assert_eq!(d.plan_token_share_api, None);
        assert_eq!(
            d.api_tokens_per_assistant_msg_outcome,
            MetricOutcome::NoData
        );
        assert_eq!(d.content_tokens_per_user_msg_outcome, MetricOutcome::NoData);
        assert_eq!(
            d.tool_calls_per_1k_api_tokens_outcome,
            MetricOutcome::NoData
        );
        assert_eq!(
            d.tool_calls_per_1k_content_tokens_outcome,
            MetricOutcome::NoData
        );
        assert_eq!(d.plan_message_pct_outcome, MetricOutcome::NoData);
        assert_eq!(d.plan_token_share_content_outcome, MetricOutcome::NoData);
        assert_eq!(d.plan_token_share_api_outcome, MetricOutcome::NoData);
    }

    #[test]
    fn compute_derived_realistic_bucket() {
        let bucket = UsageBucket {
            message_count: 100,
            user_message_count: 50,
            assistant_message_count: 50,
            tool_call_count: 10,
            plan_message_count: 5,
            plan_content_tokens_est_total: 2_500,
            plan_api_tokens_total: 3_000,
            api_coverage_message_count: 80,
            content_tokens_est_total: 50_000,
            api_tokens_total: 60_000,
            estimated_cost_usd: 3.00,
            ..Default::default()
        };
        let d = compute_derived(&bucket);
        assert_eq!(d.api_coverage_pct, MetricOutcome::Value(80.0));
        assert_eq!(d.api_tokens_per_assistant_msg, Some(1200.0));
        assert_eq!(d.content_tokens_per_user_msg, Some(1000.0));
        assert!(d.tool_calls_per_1k_api_tokens.is_some());
        assert!(d.tool_calls_per_1k_content_tokens.is_some());
        assert!((d.plan_message_pct.unwrap() - 5.0).abs() < 0.01);
        assert_eq!(d.plan_token_share_content, Some(0.05));
        assert_eq!(d.plan_token_share_api, Some(0.05));
        assert_eq!(
            d.api_tokens_per_assistant_msg_outcome,
            MetricOutcome::Value(1200.0)
        );
        assert_eq!(
            d.content_tokens_per_user_msg_outcome,
            MetricOutcome::Value(1000.0)
        );
        assert_eq!(d.plan_message_pct_outcome, MetricOutcome::Value(5.0));
        assert_eq!(
            d.plan_token_share_content_outcome,
            MetricOutcome::Value(0.05)
        );
        assert_eq!(d.plan_token_share_api_outcome, MetricOutcome::Value(0.05));
    }

    #[test]
    fn no_nan_or_infinity() {
        // Even with weird values, we should never get NaN or Infinity
        let bucket = UsageBucket {
            message_count: 0,
            api_tokens_total: 0,
            content_tokens_est_total: 0,
            ..Default::default()
        };
        let d = compute_derived(&bucket);
        assert_eq!(d.api_coverage_pct, MetricOutcome::NoData);
    }

    #[test]
    fn compute_derived_distinguishes_true_zero_from_missing_and_invalid() {
        let true_zero = compute_derived(&UsageBucket {
            message_count: 4,
            user_message_count: 2,
            assistant_message_count: 2,
            api_tokens_total: 10,
            content_tokens_est_total: 20,
            ..Default::default()
        });
        assert_eq!(
            true_zero.tool_calls_per_1k_api_tokens_outcome,
            MetricOutcome::TrueZero
        );
        assert_eq!(true_zero.plan_message_pct_outcome, MetricOutcome::TrueZero);

        let invalid = compute_derived(&UsageBucket {
            message_count: 1,
            assistant_message_count: 1,
            api_tokens_total: -1,
            ..Default::default()
        });
        assert_eq!(
            invalid.api_tokens_per_assistant_msg_outcome,
            MetricOutcome::InvalidInput
        );
        assert_eq!(invalid.api_tokens_per_assistant_msg, None);
    }
}
