//! Shared types for the analytics library.
//!
//! These types are used by both CLI commands and the FrankenTUI analytics
//! dashboards, keeping query logic, bucketing, and derived-metric math in
//! one place.

use serde::Serialize;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Analytics-specific error.
#[derive(Debug)]
pub enum AnalyticsError {
    /// The required table does not exist — caller should suggest `cass analytics rebuild`.
    MissingTable(String),
    /// A database query failed.
    Db(String),
}

impl std::fmt::Display for AnalyticsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTable(t) => {
                write!(
                    f,
                    "table '{t}' does not exist — run 'cass analytics rebuild'"
                )
            }
            Self::Db(msg) => write!(f, "analytics db error: {msg}"),
        }
    }
}

impl std::error::Error for AnalyticsError {}

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
}

impl Metric {
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
            Self::CoveragePct => None, // derived from api_coverage_message_count / message_count
        }
    }
}

impl std::fmt::Display for Metric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiTotal => write!(f, "api_total"),
            Self::ApiInput => write!(f, "api_input"),
            Self::ApiOutput => write!(f, "api_output"),
            Self::CacheRead => write!(f, "cache_read"),
            Self::CacheCreation => write!(f, "cache_creation"),
            Self::Thinking => write!(f, "thinking"),
            Self::ContentEstTotal => write!(f, "content_est_total"),
            Self::ToolCalls => write!(f, "tool_calls"),
            Self::PlanCount => write!(f, "plan_count"),
            Self::CoveragePct => write!(f, "coverage_pct"),
            Self::MessageCount => write!(f, "message_count"),
        }
    }
}

// ---------------------------------------------------------------------------
// UsageBucket — the core aggregate row
// ---------------------------------------------------------------------------

/// A single bucket of aggregated token / message metrics.
///
/// Mirrors the columns of `usage_daily` / `usage_hourly`.  Implements additive
/// `merge()` for re-bucketing (day → week, day → month).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
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
    /// Which rollup table was queried.
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
            .map(|s| {
                serde_json::json!({
                    "signal": s.signal,
                    "detail": s.detail,
                    "severity": s.severity,
                })
            })
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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn usage_bucket_to_json_shape() {
        let bucket = UsageBucket {
            message_count: 100,
            assistant_message_count: 50,
            api_tokens_total: 5000,
            api_coverage_message_count: 80,
            ..Default::default()
        };
        let json = bucket.to_json("2025-01-15");
        assert_eq!(json["bucket"], "2025-01-15");
        assert!(json["counts"]["message_count"].is_number());
        assert!(json["content_tokens"]["est_total"].is_number());
        assert!(json["api_tokens"]["total"].is_number());
        assert!(json["coverage"]["api_coverage_pct"].is_number());
        assert!(json["derived"].is_object());
    }

    #[test]
    fn group_by_display() {
        assert_eq!(GroupBy::Hour.to_string(), "hour");
        assert_eq!(GroupBy::Day.to_string(), "day");
        assert_eq!(GroupBy::Week.to_string(), "week");
        assert_eq!(GroupBy::Month.to_string(), "month");
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
