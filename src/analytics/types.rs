//! Shared types for the analytics library.
//!
//! These types are used by both CLI commands and the FrankenTUI analytics
//! dashboards, keeping query logic, bucketing, and derived-metric math in
//! one place.

use serde::Serialize;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Analytics-specific error.
#[derive(Debug, Error)]
pub enum AnalyticsError {
    /// The required table does not exist — caller should suggest `cass analytics rebuild`.
    #[error("table '{0}' does not exist — run 'cass analytics rebuild'")]
    MissingTable(String),
    /// A database query failed.
    #[error("analytics db error: {0}")]
    Db(String),
}

/// Convenience alias.
pub type AnalyticsResult<T> = std::result::Result<T, AnalyticsError>;

// ---------------------------------------------------------------------------
// GroupBy
// ---------------------------------------------------------------------------

/// Time-bucket granularity (library-side, no clap dependency).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupBy {
    Hour,
    #[default]
    Day,
    Week,
    Month,
}

impl std::fmt::Display for GroupBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hour => write!(f, "hour"),
            Self::Day => write!(f, "day"),
            Self::Week => write!(f, "week"),
            Self::Month => write!(f, "month"),
        }
    }
}

impl GroupBy {
    /// Human-readable label for display in TUI headers.
    pub fn label(self) -> &'static str {
        match self {
            Self::Hour => "Hourly",
            Self::Day => "Daily",
            Self::Week => "Weekly",
            Self::Month => "Monthly",
        }
    }

    /// Cycle to the next granularity (Hour → Day → Week → Month → Hour).
    pub fn next(self) -> Self {
        match self {
            Self::Hour => Self::Day,
            Self::Day => Self::Week,
            Self::Week => Self::Month,
            Self::Month => Self::Hour,
        }
    }

    /// Cycle to the previous granularity.
    pub fn prev(self) -> Self {
        match self {
            Self::Hour => Self::Month,
            Self::Day => Self::Hour,
            Self::Week => Self::Day,
            Self::Month => Self::Week,
        }
    }
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

/// Source filter for analytics queries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SourceFilter {
    /// No source filtering.
    #[default]
    All,
    /// Only local data.
    Local,
    /// Only remote data (anything that is not "local").
    Remote,
    /// A specific source_id string.
    Specific(String),
}

/// CLI-agnostic analytics filter.
///
/// Callers convert from their own arg structs (e.g. `AnalyticsCommon`) via
/// `From` impls kept in lib.rs.
#[derive(Clone, Debug, Default)]
pub struct AnalyticsFilter {
    /// Inclusive lower bound, epoch milliseconds.
    pub since_ms: Option<i64>,
    /// Inclusive upper bound, epoch milliseconds.
    pub until_ms: Option<i64>,
    /// Agent slug allow-list (empty = all agents).
    pub agents: Vec<String>,
    /// Source filter.
    pub source: SourceFilter,
    /// Workspace id allow-list (empty = all workspaces).
    pub workspace_ids: Vec<i64>,
}

// ---------------------------------------------------------------------------
// Dimension and Metric enums
// ---------------------------------------------------------------------------

/// Dimension for breakdown queries — which column to GROUP BY.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Dim {
    Agent,
    Workspace,
    Source,
    Model,
}

impl std::fmt::Display for Dim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Agent => write!(f, "agent"),
            Self::Workspace => write!(f, "workspace"),
            Self::Source => write!(f, "source"),
            Self::Model => write!(f, "model"),
        }
    }
}

/// Metric selector for breakdown/explorer queries.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// Total API tokens (input + output + cache + thinking).
    #[default]
    ApiTotal,
    ApiInput,
    ApiOutput,
    CacheRead,
    CacheCreation,
    Thinking,
    /// Estimated content tokens (chars / 4).
    ContentEstTotal,
    /// Tool call count.
    ToolCalls,
    /// Plan message count.
    PlanCount,
    /// API coverage percentage.
    CoveragePct,
    /// Message count.
    MessageCount,
    /// Estimated cost in USD from model pricing.
    EstimatedCostUsd,
}

impl Metric {
    /// Stable snake_case string used in CLI/JSON display surfaces.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiTotal => "api_total",
            Self::ApiInput => "api_input",
            Self::ApiOutput => "api_output",
            Self::CacheRead => "cache_read",
            Self::CacheCreation => "cache_creation",
            Self::Thinking => "thinking",
            Self::ContentEstTotal => "content_est_total",
            Self::ToolCalls => "tool_calls",
            Self::PlanCount => "plan_count",
            Self::CoveragePct => "coverage_pct",
            Self::MessageCount => "message_count",
            Self::EstimatedCostUsd => "estimated_cost_usd",
        }
    }

    /// Return the SQL column name in the `usage_daily`/`usage_hourly` rollup
    /// tables that corresponds to this metric, or `None` if the metric is
    /// derived and not stored directly.
    pub fn rollup_column(&self) -> Option<&'static str> {
        match self {
            Self::ApiTotal => Some("api_tokens_total"),
            Self::ApiInput => Some("api_input_tokens_total"),
            Self::ApiOutput => Some("api_output_tokens_total"),
            Self::CacheRead => Some("api_cache_read_tokens_total"),
            Self::CacheCreation => Some("api_cache_creation_tokens_total"),
            Self::Thinking => Some("api_thinking_tokens_total"),
            Self::ContentEstTotal => Some("content_tokens_est_total"),
            Self::ToolCalls => Some("tool_call_count"),
            Self::PlanCount => Some("plan_message_count"),
            Self::MessageCount => Some("message_count"),
            Self::CoveragePct => None,      // derived
            Self::EstimatedCostUsd => None, // only in token_daily_stats (Track B)
        }
    }
}

impl std::fmt::Display for Metric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// UsageBucket — the core aggregate row
// ---------------------------------------------------------------------------

/// A single bucket of aggregated token / message metrics.
///
/// Mirrors the columns of `usage_daily` / `usage_hourly`.  Implements additive
/// `merge()` for re-bucketing (day → week, day → month).
///
/// Note: `Eq` is not derived because `estimated_cost_usd` is `f64`.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct UsageBucket {
    pub message_count: i64,
    pub user_message_count: i64,
    pub assistant_message_count: i64,
    pub tool_call_count: i64,
    pub plan_message_count: i64,
    pub api_coverage_message_count: i64,
    pub content_tokens_est_total: i64,
    pub content_tokens_est_user: i64,
    pub content_tokens_est_assistant: i64,
    pub api_tokens_total: i64,
    pub api_input_tokens_total: i64,
    pub api_output_tokens_total: i64,
    pub api_cache_read_tokens_total: i64,
    pub api_cache_creation_tokens_total: i64,
    pub api_thinking_tokens_total: i64,
    pub plan_content_tokens_est_total: i64,
    pub plan_api_tokens_total: i64,
    /// Estimated cost in USD from model pricing tables.
    /// Populated from Track B (token_daily_stats); 0.0 from Track A.
    pub estimated_cost_usd: f64,
}

impl UsageBucket {
    /// Accumulate another bucket into this one (additive merge).
    pub fn merge(&mut self, other: &UsageBucket) {
        self.message_count += other.message_count;
        self.user_message_count += other.user_message_count;
        self.assistant_message_count += other.assistant_message_count;
        self.tool_call_count += other.tool_call_count;
        self.plan_message_count += other.plan_message_count;
        self.api_coverage_message_count += other.api_coverage_message_count;
        self.content_tokens_est_total += other.content_tokens_est_total;
        self.content_tokens_est_user += other.content_tokens_est_user;
        self.content_tokens_est_assistant += other.content_tokens_est_assistant;
        self.api_tokens_total += other.api_tokens_total;
        self.api_input_tokens_total += other.api_input_tokens_total;
        self.api_output_tokens_total += other.api_output_tokens_total;
        self.api_cache_read_tokens_total += other.api_cache_read_tokens_total;
        self.api_cache_creation_tokens_total += other.api_cache_creation_tokens_total;
        self.api_thinking_tokens_total += other.api_thinking_tokens_total;
        self.plan_content_tokens_est_total += other.plan_content_tokens_est_total;
        self.plan_api_tokens_total += other.plan_api_tokens_total;
        self.estimated_cost_usd += other.estimated_cost_usd;
    }

    /// Produce the nested JSON shape expected by CLI consumers.
    ///
    /// The shape is backwards-compatible with the original `TokenBucketRow::to_json`.
    pub fn to_json(&self, bucket_key: &str) -> serde_json::Value {
        let derived = super::derive::compute_derived(self);

        serde_json::json!({
            "bucket": bucket_key,
            "counts": {
                "message_count": self.message_count,
                "user_message_count": self.user_message_count,
                "assistant_message_count": self.assistant_message_count,
                "tool_call_count": self.tool_call_count,
                "plan_message_count": self.plan_message_count,
            },
            "content_tokens": {
                "est_total": self.content_tokens_est_total,
                "est_user": self.content_tokens_est_user,
                "est_assistant": self.content_tokens_est_assistant,
            },
            "api_tokens": {
                "total": self.api_tokens_total,
                "input": self.api_input_tokens_total,
                "output": self.api_output_tokens_total,
                "cache_read": self.api_cache_read_tokens_total,
                "cache_creation": self.api_cache_creation_tokens_total,
                "thinking": self.api_thinking_tokens_total,
            },
            "plan_tokens": {
                "content_est_total": self.plan_content_tokens_est_total,
                "api_total": self.plan_api_tokens_total,
            },
            "coverage": {
                "api_coverage_message_count": self.api_coverage_message_count,
                "api_coverage_pct": derived.api_coverage_pct,
            },
            "derived": {
                "api_tokens_per_assistant_msg": derived.api_tokens_per_assistant_msg,
                "content_tokens_per_user_msg": derived.content_tokens_per_user_msg,
                "tool_calls_per_1k_api_tokens": derived.tool_calls_per_1k_api_tokens,
                "tool_calls_per_1k_content_tokens": derived.tool_calls_per_1k_content_tokens,
                "plan_message_pct": derived.plan_message_pct,
                "plan_token_share_content": derived.plan_token_share_content,
                "plan_token_share_api": derived.plan_token_share_api,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Timeseries result
// ---------------------------------------------------------------------------

/// Result of a token/usage timeseries query.
pub struct TimeseriesResult {
    /// Ordered (label, bucket) pairs.
    pub buckets: Vec<(String, UsageBucket)>,
    /// Grand totals across all buckets.
    pub totals: UsageBucket,
    /// Which rollup or raw table backed the query.
    pub source_table: String,
    /// Granularity that was used.
    pub group_by: GroupBy,
    /// Query wall-time in milliseconds.
    pub elapsed_ms: u64,
    /// "rollup" or "slow".
    pub path: String,
}

impl TimeseriesResult {
    /// Produce the CLI-compatible JSON envelope.
    pub fn to_cli_json(&self) -> serde_json::Value {
        let bucket_json: Vec<serde_json::Value> = self
            .buckets
            .iter()
            .map(|(key, row)| row.to_json(key))
            .collect();

        serde_json::json!({
            "buckets": bucket_json,
            "totals": self.totals.to_json("all"),
            "bucket_count": self.buckets.len(),
            "_meta": {
                "elapsed_ms": self.elapsed_ms,
                "path": self.path,
                "group_by": self.group_by.to_string(),
                "source_table": self.source_table,
                "rows_read": self.buckets.len(),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Breakdown result
// ---------------------------------------------------------------------------

/// A single row in a breakdown query result (one value of the chosen dimension).
#[derive(Debug, Clone, Serialize)]
pub struct BreakdownRow {
    /// The dimension value (agent slug, workspace id, source id, or model family).
    pub key: String,
    /// The metric value (SUM of the selected metric column).
    pub value: i64,
    /// Message count for this slice (useful for context).
    pub message_count: i64,
    /// Full bucket for this slice (available for derived metric computation).
    pub bucket: UsageBucket,
}

impl BreakdownRow {
    /// Produce JSON for CLI output.
    pub fn to_json(&self) -> serde_json::Value {
        let derived = super::derive::compute_derived(&self.bucket);
        serde_json::json!({
            "key": self.key,
            "value": self.value,
            "message_count": self.message_count,
            "derived": {
                "api_coverage_pct": derived.api_coverage_pct,
                "tool_calls_per_1k_api_tokens": derived.tool_calls_per_1k_api_tokens,
                "plan_message_pct": derived.plan_message_pct,
            },
        })
    }
}

/// Result of a breakdown query.
pub struct BreakdownResult {
    /// Rows ordered by the metric value descending.
    pub rows: Vec<BreakdownRow>,
    /// Which dimension was grouped by.
    pub dim: Dim,
    /// Which metric was selected.
    pub metric: Metric,
    /// Which rollup table was queried.
    pub source_table: String,
    /// Query wall-time in milliseconds.
    pub elapsed_ms: u64,
}

impl BreakdownResult {
    /// Produce the CLI-compatible JSON envelope.
    pub fn to_cli_json(&self) -> serde_json::Value {
        let rows_json: Vec<serde_json::Value> = self.rows.iter().map(|r| r.to_json()).collect();
        serde_json::json!({
            "dim": self.dim.to_string(),
            "metric": self.metric.to_string(),
            "rows": rows_json,
            "row_count": self.rows.len(),
            "_meta": {
                "elapsed_ms": self.elapsed_ms,
                "source_table": self.source_table,
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Tool report
// ---------------------------------------------------------------------------

/// A single row in a tool usage report.
#[derive(Debug, Clone, Serialize)]
pub struct ToolRow {
    /// The agent slug or workspace — dimension key.
    pub key: String,
    /// Total tool call count.
    pub tool_call_count: i64,
    /// Total message count.
    pub message_count: i64,
    /// Total API tokens.
    pub api_tokens_total: i64,
    /// Tool calls per 1k API tokens (derived).
    pub tool_calls_per_1k_api_tokens: Option<f64>,
    /// Tool calls per 1k content tokens (derived).
    pub tool_calls_per_1k_content_tokens: Option<f64>,
}

impl ToolRow {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "key": self.key,
            "tool_call_count": self.tool_call_count,
            "message_count": self.message_count,
            "api_tokens_total": self.api_tokens_total,
            "tool_calls_per_1k_api_tokens": self.tool_calls_per_1k_api_tokens,
            "tool_calls_per_1k_content_tokens": self.tool_calls_per_1k_content_tokens,
        })
    }
}

/// Result of a tool usage report query.
pub struct ToolReport {
    /// Rows ordered by tool_call_count descending.
    pub rows: Vec<ToolRow>,
    /// Totals across all rows.
    pub total_tool_calls: i64,
    pub total_messages: i64,
    pub total_api_tokens: i64,
    /// Which rollup table was queried.
    pub source_table: String,
    /// Query wall-time in milliseconds.
    pub elapsed_ms: u64,
}

impl ToolReport {
    /// Produce the CLI-compatible JSON envelope.
    pub fn to_cli_json(&self) -> serde_json::Value {
        let rows_json: Vec<serde_json::Value> = self.rows.iter().map(|r| r.to_json()).collect();
        let overall_per_1k = if self.total_api_tokens > 0 {
            Some(self.total_tool_calls as f64 / (self.total_api_tokens as f64 / 1000.0))
        } else {
            None
        };
        serde_json::json!({
            "rows": rows_json,
            "row_count": self.rows.len(),
            "totals": {
                "tool_call_count": self.total_tool_calls,
                "message_count": self.total_messages,
                "api_tokens_total": self.total_api_tokens,
                "tool_calls_per_1k_api_tokens": overall_per_1k,
            },
            "_meta": {
                "elapsed_ms": self.elapsed_ms,
                "source_table": self.source_table,
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Session scatter result
// ---------------------------------------------------------------------------

/// A single per-session point for Explorer scatter plots.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionScatterPoint {
    /// Source identifier (`local`, remote source id, etc.).
    pub source_id: String,
    /// Session path used as the stable per-session key.
    pub source_path: String,
    /// Total messages in the session (x-axis).
    pub message_count: i64,
    /// Total API tokens in the session (y-axis).
    pub api_tokens_total: i64,
}

// ---------------------------------------------------------------------------
// Status result types
// ---------------------------------------------------------------------------

/// Per-table statistics.
#[derive(Debug, Default, Clone, Serialize)]
pub struct TableInfo {
    pub table: String,
    pub exists: bool,
    pub row_count: i64,
    pub min_day_id: Option<i64>,
    pub max_day_id: Option<i64>,
    pub last_updated: Option<i64>,
}

impl TableInfo {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "table": self.table,
            "exists": self.exists,
            "row_count": self.row_count,
            "min_day_id": self.min_day_id,
            "max_day_id": self.max_day_id,
            "last_updated": self.last_updated,
        })
    }
}

/// Coverage statistics.
#[derive(Debug, Default, Clone, Serialize)]
pub struct CoverageInfo {
    pub total_messages: i64,
    pub message_metrics_coverage_pct: f64,
    pub api_token_coverage_pct: f64,
    pub model_name_coverage_pct: f64,
    pub estimate_only_pct: f64,
}

/// Drift detection output.
#[derive(Debug, Default, Clone, Serialize)]
pub struct DriftInfo {
    pub signals: Vec<DriftSignal>,
    pub track_a_fresh: bool,
    pub track_b_fresh: bool,
}

/// A single drift detection signal.
#[derive(Debug, Clone, Serialize)]
pub struct DriftSignal {
    pub signal: String,
    pub detail: String,
    pub severity: String,
}

impl DriftSignal {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "signal": self.signal,
            "detail": self.detail,
            "severity": self.severity,
        })
    }
}

/// Full status result.
pub struct StatusResult {
    pub tables: Vec<TableInfo>,
    pub coverage: CoverageInfo,
    pub drift: DriftInfo,
    pub recommended_action: String,
}

impl StatusResult {
    /// Produce the CLI-compatible JSON output.
    pub fn to_json(&self) -> serde_json::Value {
        let tables_json: Vec<serde_json::Value> = self.tables.iter().map(|t| t.to_json()).collect();
        let signals_json: Vec<serde_json::Value> = self
            .drift
            .signals
            .iter()
            .map(DriftSignal::to_json)
            .collect();

        serde_json::json!({
            "tables": tables_json,
            "coverage": {
                "total_messages": self.coverage.total_messages,
                "message_metrics_coverage_pct": self.coverage.message_metrics_coverage_pct,
                "api_token_coverage_pct": self.coverage.api_token_coverage_pct,
                "model_name_coverage_pct": self.coverage.model_name_coverage_pct,
                "estimate_only_pct": self.coverage.estimate_only_pct,
            },
            "drift": {
                "signals": signals_json,
                "track_a_fresh": self.drift.track_a_fresh,
                "track_b_fresh": self.drift.track_b_fresh,
            },
            "recommended_action": self.recommended_action,
        })
    }
}

// ---------------------------------------------------------------------------
// Unpriced model report
// ---------------------------------------------------------------------------

/// A model name with no matching pricing entry, and its token volume.
#[derive(Debug, Clone, Serialize)]
pub struct UnpricedModel {
    /// The model name (or "(none)" if no model name was recorded).
    pub model_name: String,
    /// Total tokens across all unpriced usages of this model.
    pub total_tokens: i64,
    /// Number of token_usage rows with this model.
    pub row_count: i64,
}

/// Result of the unpriced-models query.
#[derive(Debug, Clone, Serialize)]
pub struct UnpricedModelsReport {
    /// Models with no pricing match, sorted by total_tokens descending.
    pub models: Vec<UnpricedModel>,
    /// Total unpriced tokens across all models.
    pub total_unpriced_tokens: i64,
    /// Total priced tokens for context.
    pub total_priced_tokens: i64,
}

// ---------------------------------------------------------------------------
// Derived metrics
// ---------------------------------------------------------------------------

/// Computed ratios from a [`UsageBucket`].
#[derive(Debug, Clone, Serialize)]
pub struct DerivedMetrics {
    pub api_coverage_pct: f64,
    pub api_tokens_per_assistant_msg: Option<f64>,
    pub content_tokens_per_user_msg: Option<f64>,
    pub tool_calls_per_1k_api_tokens: Option<f64>,
    pub tool_calls_per_1k_content_tokens: Option<f64>,
    pub plan_message_pct: Option<f64>,
    pub plan_token_share_content: Option<f64>,
    pub plan_token_share_api: Option<f64>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analytics_error_display_and_sources_are_preserved() {
        let missing = AnalyticsError::MissingTable("usage_daily".to_string());
        assert_eq!(
            missing.to_string(),
            "table 'usage_daily' does not exist — run 'cass analytics rebuild'"
        );
        assert!(std::error::Error::source(&missing).is_none());

        let db = AnalyticsError::Db("query failed".to_string());
        assert_eq!(db.to_string(), "analytics db error: query failed");
        assert!(std::error::Error::source(&db).is_none());
    }

    #[test]
    fn usage_bucket_merge_is_additive() {
        let mut a = UsageBucket {
            message_count: 10,
            user_message_count: 5,
            assistant_message_count: 5,
            tool_call_count: 3,
            api_tokens_total: 1000,
            api_input_tokens_total: 600,
            api_output_tokens_total: 400,
            estimated_cost_usd: 0.50,
            ..Default::default()
        };
        let b = UsageBucket {
            message_count: 20,
            user_message_count: 10,
            assistant_message_count: 10,
            tool_call_count: 7,
            api_tokens_total: 2000,
            api_input_tokens_total: 1200,
            api_output_tokens_total: 800,
            estimated_cost_usd: 1.25,
            ..Default::default()
        };
        a.merge(&b);
        assert_eq!(a.message_count, 30);
        assert_eq!(a.user_message_count, 15);
        assert_eq!(a.assistant_message_count, 15);
        assert_eq!(a.tool_call_count, 10);
        assert_eq!(a.api_tokens_total, 3000);
        assert_eq!(a.api_input_tokens_total, 1800);
        assert_eq!(a.api_output_tokens_total, 1200);
        assert!((a.estimated_cost_usd - 1.75).abs() < 0.001);
    }

    #[test]
    fn usage_bucket_to_json_shape() {
        let bucket = UsageBucket {
            message_count: 100,
            assistant_message_count: 50,
            plan_message_count: 10,
            plan_content_tokens_est_total: 1_000,
            plan_api_tokens_total: 2_000,
            content_tokens_est_total: 10_000,
            api_tokens_total: 5000,
            api_coverage_message_count: 80,
            estimated_cost_usd: 2.50,
            ..Default::default()
        };
        let json = bucket.to_json("2025-01-15");
        assert_eq!(json["bucket"], "2025-01-15");
        assert!(json["counts"]["message_count"].is_number());
        assert!(json["plan_tokens"]["content_est_total"].is_number());
        assert!(json["content_tokens"]["est_total"].is_number());
        assert!(json["api_tokens"]["total"].is_number());
        assert!(json["coverage"]["api_coverage_pct"].is_number());
        assert!(json["derived"].is_object());
        assert!(json["derived"]["plan_token_share_content"].is_number());
        assert!(json["derived"]["plan_token_share_api"].is_number());
    }

    #[test]
    fn group_by_display() {
        assert_eq!(GroupBy::Hour.to_string(), "hour");
        assert_eq!(GroupBy::Day.to_string(), "day");
        assert_eq!(GroupBy::Week.to_string(), "week");
        assert_eq!(GroupBy::Month.to_string(), "month");
    }

    #[test]
    fn group_by_next_cycles_through_all() {
        assert_eq!(GroupBy::Hour.next(), GroupBy::Day);
        assert_eq!(GroupBy::Day.next(), GroupBy::Week);
        assert_eq!(GroupBy::Week.next(), GroupBy::Month);
        assert_eq!(GroupBy::Month.next(), GroupBy::Hour);
    }

    #[test]
    fn group_by_prev_cycles_through_all() {
        assert_eq!(GroupBy::Hour.prev(), GroupBy::Month);
        assert_eq!(GroupBy::Day.prev(), GroupBy::Hour);
        assert_eq!(GroupBy::Week.prev(), GroupBy::Day);
        assert_eq!(GroupBy::Month.prev(), GroupBy::Week);
    }

    #[test]
    fn group_by_label() {
        assert_eq!(GroupBy::Hour.label(), "Hourly");
        assert_eq!(GroupBy::Day.label(), "Daily");
        assert_eq!(GroupBy::Week.label(), "Weekly");
        assert_eq!(GroupBy::Month.label(), "Monthly");
    }

    #[test]
    fn metric_as_str_matches_display_for_all_variants() {
        let cases = [
            (Metric::ApiTotal, "api_total"),
            (Metric::ApiInput, "api_input"),
            (Metric::ApiOutput, "api_output"),
            (Metric::CacheRead, "cache_read"),
            (Metric::CacheCreation, "cache_creation"),
            (Metric::Thinking, "thinking"),
            (Metric::ContentEstTotal, "content_est_total"),
            (Metric::ToolCalls, "tool_calls"),
            (Metric::PlanCount, "plan_count"),
            (Metric::CoveragePct, "coverage_pct"),
            (Metric::MessageCount, "message_count"),
            (Metric::EstimatedCostUsd, "estimated_cost_usd"),
        ];

        for (metric, expected) in cases {
            assert_eq!(metric.as_str(), expected);
            assert_eq!(metric.to_string(), expected);
        }
    }

    #[test]
    fn drift_signal_to_json_shape() {
        let signal = DriftSignal {
            signal: "track-a-stale".to_string(),
            detail: "usage_daily is older than token_usage".to_string(),
            severity: "warning".to_string(),
        };

        let json = signal.to_json();
        assert_eq!(json["signal"], "track-a-stale");
        assert_eq!(json["detail"], "usage_daily is older than token_usage");
        assert_eq!(json["severity"], "warning");
        assert_eq!(json.as_object().expect("object").len(), 3);
    }

    #[test]
    fn default_filter_is_unfiltered() {
        let f = AnalyticsFilter::default();
        assert!(f.since_ms.is_none());
        assert!(f.until_ms.is_none());
        assert!(f.agents.is_empty());
        assert_eq!(f.source, SourceFilter::All);
        assert!(f.workspace_ids.is_empty());
    }
}
