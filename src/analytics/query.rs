//! SQL query builders for analytics.
//!
//! All functions accept a `&rusqlite::Connection` and an [`AnalyticsFilter`],
//! keeping the SQL and bucketing logic in one place for both CLI and ftui.

use std::collections::BTreeMap;

use rusqlite::Connection;

use super::bucketing;
use super::types::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check whether a table exists in the database.
pub fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> bool {
    let mut stmt = match conn.prepare(&format!("PRAGMA table_info({table})")) {
        Ok(stmt) => stmt,
        Err(_) => return false,
    };

    let rows = match stmt.query_map([], |row| row.get::<_, String>(1)) {
        Ok(rows) => rows,
        Err(_) => return false,
    };

    rows.flatten().any(|name| name == column)
}

fn table_has_plan_token_rollups(conn: &Connection, table: &str) -> bool {
    table_has_column(conn, table, "plan_content_tokens_est_total")
        && table_has_column(conn, table, "plan_api_tokens_total")
}

/// Internal stats from a single COUNT/MIN/MAX query on a rollup table.
#[derive(Debug, Default)]
struct RollupStats {
    row_count: i64,
    min_day: Option<i64>,
    max_day: Option<i64>,
    last_updated: Option<i64>,
}

/// Query row counts and range for a single analytics table.
fn query_table_stats(
    conn: &Connection,
    table: &str,
    day_col: &str,
    updated_col: Option<&str>,
) -> RollupStats {
    if !table_exists(conn, table) {
        return RollupStats::default();
    }
    let sql = match updated_col {
        Some(uc) => {
            format!("SELECT COUNT(*), MIN({day_col}), MAX({day_col}), MAX({uc}) FROM {table}")
        }
        None => format!("SELECT COUNT(*), MIN({day_col}), MAX({day_col}), NULL FROM {table}"),
    };
    conn.query_row(&sql, [], |row| {
        Ok(RollupStats {
            row_count: row.get::<_, i64>(0).unwrap_or(0),
            min_day: row.get::<_, Option<i64>>(1).unwrap_or(None),
            max_day: row.get::<_, Option<i64>>(2).unwrap_or(None),
            last_updated: row.get::<_, Option<i64>>(3).unwrap_or(None),
        })
    })
    .unwrap_or_default()
}

/// Build SQL WHERE clause fragments and bind-parameter values from an
/// [`AnalyticsFilter`]'s dimensional (non-time) filters.
///
/// Returns `(clause_fragments, param_values)` where each fragment is like
/// `"agent_slug IN (?1, ?2)"` and `param_values` are the corresponding bind
/// strings.
pub fn build_where_parts(filter: &AnalyticsFilter) -> (Vec<String>, Vec<String>) {
    let mut parts = Vec::new();
    let mut params = Vec::new();

    // Agent filters — multiple agents are OR'd together.
    if !filter.agents.is_empty() {
        let placeholders: Vec<String> = filter
            .agents
            .iter()
            .map(|a| {
                params.push(a.clone());
                format!("?{}", params.len())
            })
            .collect();
        parts.push(format!("agent_slug IN ({})", placeholders.join(", ")));
    }

    // Source filter.
    match &filter.source {
        SourceFilter::All => {}
        SourceFilter::Local => {
            params.push("local".into());
            parts.push(format!("source_id = ?{}", params.len()));
        }
        SourceFilter::Remote => {
            params.push("local".into());
            parts.push(format!("source_id != ?{}", params.len()));
        }
        SourceFilter::Specific(s) => {
            params.push(s.clone());
            parts.push(format!("source_id = ?{}", params.len()));
        }
    }

    (parts, params)
}

// ---------------------------------------------------------------------------
// query_status
// ---------------------------------------------------------------------------

/// Run the analytics status query — returns table health, coverage, and drift.
pub fn query_status(conn: &Connection, _filter: &AnalyticsFilter) -> AnalyticsResult<StatusResult> {
    // 1. Check which analytics tables actually exist.
    let has_message_metrics = table_exists(conn, "message_metrics");
    let has_usage_hourly = table_exists(conn, "usage_hourly");
    let has_usage_daily = table_exists(conn, "usage_daily");
    let has_token_usage = table_exists(conn, "token_usage");
    let has_token_daily_stats = table_exists(conn, "token_daily_stats");

    // 2. Gather per-table row counts and coverage range.
    let mm = query_table_stats(conn, "message_metrics", "day_id", None);
    let uh = query_table_stats(conn, "usage_hourly", "hour_id", Some("last_updated"));
    let ud = query_table_stats(conn, "usage_daily", "day_id", Some("last_updated"));
    let tu = query_table_stats(conn, "token_usage", "day_id", None);
    let tds = query_table_stats(conn, "token_daily_stats", "day_id", Some("last_updated"));

    // 3. Coverage diagnostics.
    let total_messages: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap_or(0);

    let api_coverage_pct = if has_message_metrics && mm.row_count > 0 {
        let api_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_metrics WHERE api_data_source = 'api'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if mm.row_count > 0 {
            (api_count as f64 / mm.row_count as f64) * 100.0
        } else {
            0.0
        }
    } else {
        0.0
    };

    let model_coverage_pct = if has_token_usage && tu.row_count > 0 {
        let with_model: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM token_usage WHERE model_name IS NOT NULL AND model_name != ''",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        (with_model as f64 / tu.row_count as f64) * 100.0
    } else {
        0.0
    };

    let estimate_only_pct = if has_token_usage && tu.row_count > 0 {
        let estimates: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM token_usage WHERE data_source = 'estimated'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        (estimates as f64 / tu.row_count as f64) * 100.0
    } else {
        0.0
    };

    let mm_coverage_pct = if total_messages > 0 {
        (mm.row_count as f64 / total_messages as f64) * 100.0
    } else {
        0.0
    };

    // 4. Drift detection.
    let mut drift_signals: Vec<DriftSignal> = Vec::new();

    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let stale_threshold_secs: i64 = 86400;

    let track_a_fresh = uh
        .last_updated
        .is_some_and(|t| (now_epoch - t).abs() < stale_threshold_secs);
    let track_b_fresh = tds
        .last_updated
        .is_some_and(|t| (now_epoch - t).abs() < stale_threshold_secs);

    if track_a_fresh && !track_b_fresh && has_token_daily_stats {
        drift_signals.push(DriftSignal {
            signal: "track_freshness_mismatch".into(),
            detail:
                "Track A (usage_hourly/daily) is fresh but Track B (token_daily_stats) is stale"
                    .into(),
            severity: "warning".into(),
        });
    }
    if track_b_fresh && !track_a_fresh && has_usage_hourly {
        drift_signals.push(DriftSignal {
            signal: "track_freshness_mismatch".into(),
            detail:
                "Track B (token_daily_stats) is fresh but Track A (usage_hourly/daily) is stale"
                    .into(),
            severity: "warning".into(),
        });
    }

    if mm.row_count > 0 && uh.row_count == 0 && has_usage_hourly {
        drift_signals.push(DriftSignal {
            signal: "missing_rollups".into(),
            detail: "message_metrics has data but usage_hourly is empty — rebuild needed".into(),
            severity: "error".into(),
        });
    }
    if mm.row_count > 0 && ud.row_count == 0 && has_usage_daily {
        drift_signals.push(DriftSignal {
            signal: "missing_rollups".into(),
            detail: "message_metrics has data but usage_daily is empty — rebuild needed".into(),
            severity: "error".into(),
        });
    }
    if tu.row_count > 0 && tds.row_count == 0 && has_token_daily_stats {
        drift_signals.push(DriftSignal {
            signal: "missing_rollups".into(),
            detail: "token_usage has data but token_daily_stats is empty — rebuild needed".into(),
            severity: "error".into(),
        });
    }

    if total_messages > 100 && mm.row_count == 0 && tu.row_count == 0 {
        drift_signals.push(DriftSignal {
            signal: "no_analytics_data".into(),
            detail: format!("{total_messages} messages indexed but no analytics computed"),
            severity: "error".into(),
        });
    }

    // 5. Recommended action.
    let has_error_drift = drift_signals.iter().any(|s| s.severity == "error");
    let has_warning_drift = drift_signals.iter().any(|s| s.severity == "warning");

    let recommended_action = if has_error_drift {
        if mm.row_count == 0 && tu.row_count == 0 {
            "rebuild_all"
        } else if mm.row_count > 0 && (uh.row_count == 0 || ud.row_count == 0) {
            "rebuild_track_a"
        } else if tu.row_count > 0 && tds.row_count == 0 {
            "rebuild_track_b"
        } else {
            "rebuild_all"
        }
    } else if has_warning_drift {
        if track_a_fresh && !track_b_fresh {
            "rebuild_track_b"
        } else if track_b_fresh && !track_a_fresh {
            "rebuild_track_a"
        } else {
            "none"
        }
    } else {
        "none"
    };

    // 6. Assemble result.
    let make_table_info = |name: &str, exists: bool, stats: &RollupStats| TableInfo {
        table: name.into(),
        exists,
        row_count: stats.row_count,
        min_day_id: stats.min_day,
        max_day_id: stats.max_day,
        last_updated: stats.last_updated,
    };

    Ok(StatusResult {
        tables: vec![
            make_table_info("message_metrics", has_message_metrics, &mm),
            make_table_info("usage_hourly", has_usage_hourly, &uh),
            make_table_info("usage_daily", has_usage_daily, &ud),
            make_table_info("token_usage", has_token_usage, &tu),
            make_table_info("token_daily_stats", has_token_daily_stats, &tds),
        ],
        coverage: CoverageInfo {
            total_messages,
            message_metrics_coverage_pct: (mm_coverage_pct * 100.0).round() / 100.0,
            api_token_coverage_pct: (api_coverage_pct * 100.0).round() / 100.0,
            model_name_coverage_pct: (model_coverage_pct * 100.0).round() / 100.0,
            estimate_only_pct: (estimate_only_pct * 100.0).round() / 100.0,
            pricing_coverage_pct: if has_token_usage && tu.row_count > 0 {
                let with_cost: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM token_usage WHERE estimated_cost_usd > 0.0",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                let pct = (with_cost as f64 / tu.row_count as f64) * 100.0;
                (pct * 100.0).round() / 100.0
            } else {
                0.0
            },
        },
        drift: DriftInfo {
            signals: drift_signals,
            track_a_fresh,
            track_b_fresh,
        },
        recommended_action: recommended_action.into(),
    })
}

// ---------------------------------------------------------------------------
// query_tokens_timeseries
// ---------------------------------------------------------------------------

/// Run the token/usage timeseries query with the given bucketing granularity.
pub fn query_tokens_timeseries(
    conn: &Connection,
    filter: &AnalyticsFilter,
    group_by: GroupBy,
) -> AnalyticsResult<TimeseriesResult> {
    let query_start = std::time::Instant::now();

    // Choose source table and bucket column.
    let (table, bucket_col) = match group_by {
        GroupBy::Hour => ("usage_hourly", "hour_id"),
        _ => ("usage_daily", "day_id"),
    };

    // Check that the source table exists.
    if !table_exists(conn, table) {
        return Ok(TimeseriesResult {
            buckets: vec![],
            totals: UsageBucket::default(),
            source_table: table.into(),
            group_by,
            elapsed_ms: query_start.elapsed().as_millis() as u64,
            path: "none".into(),
        });
    }

    // Build WHERE clause.
    let (day_min, day_max) = bucketing::resolve_day_range(filter);
    let (hour_min, hour_max) = bucketing::resolve_hour_range(filter);

    let (dim_parts, dim_params) = build_where_parts(filter);
    let mut where_parts = dim_parts;
    let mut bind_values = dim_params;

    match group_by {
        GroupBy::Hour => {
            if let Some(min) = hour_min {
                bind_values.push(min.to_string());
                where_parts.push(format!("{bucket_col} >= ?{}", bind_values.len()));
            }
            if let Some(max) = hour_max {
                bind_values.push(max.to_string());
                where_parts.push(format!("{bucket_col} <= ?{}", bind_values.len()));
            }
        }
        _ => {
            if let Some(min) = day_min {
                bind_values.push(min.to_string());
                where_parts.push(format!("{bucket_col} >= ?{}", bind_values.len()));
            }
            if let Some(max) = day_max {
                bind_values.push(max.to_string());
                where_parts.push(format!("{bucket_col} <= ?{}", bind_values.len()));
            }
        }
    }

    let where_clause = if where_parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_parts.join(" AND "))
    };

    let has_plan_token_rollups = table_has_plan_token_rollups(conn, table);
    let plan_content_expr = if has_plan_token_rollups {
        "SUM(plan_content_tokens_est_total)"
    } else {
        "0"
    };
    let plan_api_expr = if has_plan_token_rollups {
        "SUM(plan_api_tokens_total)"
    } else {
        "0"
    };

    let sql = format!(
        "SELECT {bucket_col},
                SUM(message_count),
                SUM(user_message_count),
                SUM(assistant_message_count),
                SUM(tool_call_count),
                SUM(plan_message_count),
                {plan_content_expr},
                {plan_api_expr},
                SUM(api_coverage_message_count),
                SUM(content_tokens_est_total),
                SUM(content_tokens_est_user),
                SUM(content_tokens_est_assistant),
                SUM(api_tokens_total),
                SUM(api_input_tokens_total),
                SUM(api_output_tokens_total),
                SUM(api_cache_read_tokens_total),
                SUM(api_cache_creation_tokens_total),
                SUM(api_thinking_tokens_total)
         FROM {table}
         {where_clause}
         GROUP BY {bucket_col}
         ORDER BY {bucket_col}"
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| AnalyticsError::Db(format!("Failed to prepare analytics query: {e}")))?;

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = bind_values
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();

    let rows_result = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, i64>(12)?,
                row.get::<_, i64>(13)?,
                row.get::<_, i64>(14)?,
                row.get::<_, i64>(15)?,
                row.get::<_, i64>(16)?,
                row.get::<_, i64>(17)?,
            ))
        })
        .map_err(|e| AnalyticsError::Db(format!("Analytics query failed: {e}")))?;

    let mut raw_buckets: Vec<(i64, UsageBucket)> = Vec::new();
    for row in rows_result {
        let r = row.map_err(|e| AnalyticsError::Db(format!("Row read error: {e}")))?;
        raw_buckets.push((
            r.0,
            UsageBucket {
                message_count: r.1,
                user_message_count: r.2,
                assistant_message_count: r.3,
                tool_call_count: r.4,
                plan_message_count: r.5,
                plan_content_tokens_est_total: r.6,
                plan_api_tokens_total: r.7,
                api_coverage_message_count: r.8,
                content_tokens_est_total: r.9,
                content_tokens_est_user: r.10,
                content_tokens_est_assistant: r.11,
                api_tokens_total: r.12,
                api_input_tokens_total: r.13,
                api_output_tokens_total: r.14,
                api_cache_read_tokens_total: r.15,
                api_cache_creation_tokens_total: r.16,
                api_thinking_tokens_total: r.17,
                ..Default::default()
            },
        ));
    }

    // Re-bucket by week or month if needed.
    let final_buckets: Vec<(String, UsageBucket)> = match group_by {
        GroupBy::Hour => raw_buckets
            .into_iter()
            .map(|(id, row)| (bucketing::hour_id_to_iso(id), row))
            .collect(),
        GroupBy::Day => raw_buckets
            .into_iter()
            .map(|(id, row)| (bucketing::day_id_to_iso(id), row))
            .collect(),
        GroupBy::Week => {
            let mut merged: BTreeMap<String, UsageBucket> = BTreeMap::new();
            for (day_id, row) in raw_buckets {
                let key = bucketing::day_id_to_iso_week(day_id);
                merged.entry(key).or_default().merge(&row);
            }
            merged.into_iter().collect()
        }
        GroupBy::Month => {
            let mut merged: BTreeMap<String, UsageBucket> = BTreeMap::new();
            for (day_id, row) in raw_buckets {
                let key = bucketing::day_id_to_month(day_id);
                merged.entry(key).or_default().merge(&row);
            }
            merged.into_iter().collect()
        }
    };

    // Compute totals.
    let mut totals = UsageBucket::default();
    for (_, row) in &final_buckets {
        totals.merge(row);
    }

    let elapsed_ms = query_start.elapsed().as_millis() as u64;

    Ok(TimeseriesResult {
        buckets: final_buckets,
        totals,
        source_table: table.into(),
        group_by,
        elapsed_ms,
        path: "rollup".into(),
    })
}

// ---------------------------------------------------------------------------
// query_cost_timeseries (Track B)
// ---------------------------------------------------------------------------

/// Run a cost-focused timeseries query from `token_daily_stats` (Track B).
///
/// Unlike `query_tokens_timeseries` which reads Track A (`usage_daily`), this
/// function reads Track B which carries the `estimated_cost_usd` column
/// populated from model-pricing tables.  Returns the same `TimeseriesResult`
/// so callers can use it interchangeably.
pub fn query_cost_timeseries(
    conn: &Connection,
    filter: &AnalyticsFilter,
    group_by: GroupBy,
) -> AnalyticsResult<TimeseriesResult> {
    let query_start = std::time::Instant::now();

    let table = "token_daily_stats";

    if !table_exists(conn, table) {
        return Ok(TimeseriesResult {
            buckets: vec![],
            totals: UsageBucket::default(),
            source_table: table.into(),
            group_by,
            elapsed_ms: query_start.elapsed().as_millis() as u64,
            path: "none".into(),
        });
    }

    // Build WHERE clause — Track B only has day_id (no hourly equivalent).
    let (day_min, day_max) = bucketing::resolve_day_range(filter);
    let (dim_parts, dim_params) = build_where_parts(filter);
    let mut where_parts = dim_parts;
    let mut bind_values = dim_params;

    if let Some(min) = day_min {
        bind_values.push(min.to_string());
        where_parts.push(format!("day_id >= ?{}", bind_values.len()));
    }
    if let Some(max) = day_max {
        bind_values.push(max.to_string());
        where_parts.push(format!("day_id <= ?{}", bind_values.len()));
    }

    let where_clause = if where_parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_parts.join(" AND "))
    };

    let sql = format!(
        "SELECT day_id,
                SUM(api_call_count),
                SUM(user_message_count),
                SUM(assistant_message_count),
                SUM(total_tool_calls),
                SUM(total_input_tokens),
                SUM(total_output_tokens),
                SUM(total_cache_read_tokens),
                SUM(total_cache_creation_tokens),
                SUM(total_thinking_tokens),
                SUM(grand_total_tokens),
                SUM(total_content_chars),
                SUM(estimated_cost_usd)
         FROM {table}
         {where_clause}
         GROUP BY day_id
         ORDER BY day_id"
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| AnalyticsError::Db(format!("Failed to prepare cost timeseries query: {e}")))?;

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = bind_values
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();

    let rows_result = stmt
        .query_map(param_refs.as_slice(), |row| {
            let day_id: i64 = row.get(0)?;
            let api_call_count: i64 = row.get(1)?;
            let user_msg: i64 = row.get(2)?;
            let asst_msg: i64 = row.get(3)?;
            let tool_calls: i64 = row.get(4)?;
            let input_tok: i64 = row.get(5)?;
            let output_tok: i64 = row.get(6)?;
            let cache_read: i64 = row.get(7)?;
            let cache_create: i64 = row.get(8)?;
            let thinking: i64 = row.get(9)?;
            let grand_total: i64 = row.get(10)?;
            let content_chars: i64 = row.get(11)?;
            let cost: f64 = row.get(12)?;

            Ok((
                day_id,
                UsageBucket {
                    message_count: api_call_count,
                    user_message_count: user_msg,
                    assistant_message_count: asst_msg,
                    tool_call_count: tool_calls,
                    api_coverage_message_count: api_call_count, // all Track B = API
                    content_tokens_est_total: content_chars / 4,
                    api_tokens_total: grand_total,
                    api_input_tokens_total: input_tok,
                    api_output_tokens_total: output_tok,
                    api_cache_read_tokens_total: cache_read,
                    api_cache_creation_tokens_total: cache_create,
                    api_thinking_tokens_total: thinking,
                    estimated_cost_usd: cost,
                    ..Default::default()
                },
            ))
        })
        .map_err(|e| AnalyticsError::Db(format!("Cost timeseries query failed: {e}")))?;

    let mut raw_buckets: Vec<(i64, UsageBucket)> = Vec::new();
    for row in rows_result {
        let r = row.map_err(|e| AnalyticsError::Db(format!("Row read error: {e}")))?;
        raw_buckets.push(r);
    }

    // Re-bucket by day/week/month (Track B has no hourly, so Hour falls back to Day).
    let final_buckets: Vec<(String, UsageBucket)> = match group_by {
        GroupBy::Hour | GroupBy::Day => raw_buckets
            .into_iter()
            .map(|(id, row)| (bucketing::day_id_to_iso(id), row))
            .collect(),
        GroupBy::Week => {
            let mut merged: BTreeMap<String, UsageBucket> = BTreeMap::new();
            for (day_id, row) in raw_buckets {
                let key = bucketing::day_id_to_iso_week(day_id);
                merged.entry(key).or_default().merge(&row);
            }
            merged.into_iter().collect()
        }
        GroupBy::Month => {
            let mut merged: BTreeMap<String, UsageBucket> = BTreeMap::new();
            for (day_id, row) in raw_buckets {
                let key = bucketing::day_id_to_month(day_id);
                merged.entry(key).or_default().merge(&row);
            }
            merged.into_iter().collect()
        }
    };

    // Compute totals.
    let mut totals = UsageBucket::default();
    for (_, row) in &final_buckets {
        totals.merge(row);
    }

    let elapsed_ms = query_start.elapsed().as_millis() as u64;

    Ok(TimeseriesResult {
        buckets: final_buckets,
        totals,
        source_table: table.into(),
        group_by,
        elapsed_ms,
        path: "rollup".into(),
    })
}

// ---------------------------------------------------------------------------
// query_breakdown
// ---------------------------------------------------------------------------

/// Run a breakdown query: aggregate one metric by a chosen dimension.
///
/// Returns rows ordered by the metric value descending, capped at `limit`.
/// This answers questions like "which agent uses the most tokens?" or
/// "which workspace has the most tool calls?".
pub fn query_breakdown(
    conn: &Connection,
    filter: &AnalyticsFilter,
    dim: Dim,
    metric: Metric,
    limit: usize,
) -> AnalyticsResult<BreakdownResult> {
    let query_start = std::time::Instant::now();

    // For the Model dimension, use token_daily_stats (Track B) which has model_family.
    // For Agent/Workspace/Source, use usage_daily (Track A) — EXCEPT when the
    // metric is EstimatedCostUsd, which only exists in Track B.  Track B has
    // agent_slug and source_id but NOT workspace_id, so Workspace always uses
    // Track A (cost will be 0.0 — acceptable limitation).
    let use_track_b = dim == Dim::Model
        || (metric == Metric::EstimatedCostUsd && matches!(dim, Dim::Agent | Dim::Source));

    let (table, dim_col) = if use_track_b {
        match dim {
            Dim::Agent => ("token_daily_stats", "agent_slug"),
            Dim::Source => ("token_daily_stats", "source_id"),
            Dim::Model => ("token_daily_stats", "model_family"),
            Dim::Workspace => unreachable!(), // guarded by use_track_b logic
        }
    } else {
        match dim {
            Dim::Agent => ("usage_daily", "agent_slug"),
            Dim::Workspace => ("usage_daily", "workspace_id"),
            Dim::Source => ("usage_daily", "source_id"),
            Dim::Model => unreachable!(), // Model always routes to Track B
        }
    };

    if !table_exists(conn, table) {
        return Ok(BreakdownResult {
            rows: vec![],
            dim,
            metric,
            source_table: table.into(),
            elapsed_ms: query_start.elapsed().as_millis() as u64,
        });
    }

    // Build WHERE clause.
    let (day_min, day_max) = bucketing::resolve_day_range(filter);
    let (dim_parts, dim_params) = build_where_parts(filter);
    let mut where_parts = dim_parts;
    let mut bind_values = dim_params;

    if let Some(min) = day_min {
        bind_values.push(min.to_string());
        where_parts.push(format!("day_id >= ?{}", bind_values.len()));
    }
    if let Some(max) = day_max {
        bind_values.push(max.to_string());
        where_parts.push(format!("day_id <= ?{}", bind_values.len()));
    }

    let where_clause = if where_parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_parts.join(" AND "))
    };

    // For Track A (usage_daily), we can select the full bucket.
    // For Track B (token_daily_stats), column names differ — map accordingly.
    let sql = if use_track_b {
        // Track B: token_daily_stats columns map to different names.
        build_breakdown_sql_track_b(dim_col, &metric, &where_clause, limit)
    } else {
        // Track A: usage_daily — full UsageBucket columns available.
        let has_plan_token_rollups = table_has_plan_token_rollups(conn, "usage_daily");
        build_breakdown_sql_track_a(
            dim_col,
            &metric,
            &where_clause,
            limit,
            has_plan_token_rollups,
        )
    };

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| AnalyticsError::Db(format!("Failed to prepare breakdown query: {e}")))?;

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = bind_values
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();

    let rows = if use_track_b {
        read_breakdown_rows_track_b(&mut stmt, &param_refs, &metric)?
    } else {
        read_breakdown_rows_track_a(&mut stmt, &param_refs, &metric)?
    };

    let elapsed_ms = query_start.elapsed().as_millis() as u64;

    Ok(BreakdownResult {
        rows,
        dim,
        metric,
        source_table: table.into(),
        elapsed_ms,
    })
}

/// Build SQL for breakdown from usage_daily (Track A).
fn build_breakdown_sql_track_a(
    dim_col: &str,
    metric: &Metric,
    where_clause: &str,
    limit: usize,
    has_plan_token_rollups: bool,
) -> String {
    let order_col = metric.rollup_column().unwrap_or("api_tokens_total");
    let plan_content_expr = if has_plan_token_rollups {
        "SUM(plan_content_tokens_est_total)"
    } else {
        "0"
    };
    let plan_api_expr = if has_plan_token_rollups {
        "SUM(plan_api_tokens_total)"
    } else {
        "0"
    };
    format!(
        "SELECT CAST({dim_col} AS TEXT),
                SUM(message_count),
                SUM(user_message_count),
                SUM(assistant_message_count),
                SUM(tool_call_count),
                SUM(plan_message_count),
                {plan_content_expr},
                {plan_api_expr},
                SUM(api_coverage_message_count),
                SUM(content_tokens_est_total),
                SUM(content_tokens_est_user),
                SUM(content_tokens_est_assistant),
                SUM(api_tokens_total),
                SUM(api_input_tokens_total),
                SUM(api_output_tokens_total),
                SUM(api_cache_read_tokens_total),
                SUM(api_cache_creation_tokens_total),
                SUM(api_thinking_tokens_total),
                SUM({order_col})
         FROM usage_daily
         {where_clause}
         GROUP BY {dim_col}
         ORDER BY SUM({order_col}) DESC
         LIMIT {limit}"
    )
}

/// Build SQL for breakdown from token_daily_stats (Track B).
fn build_breakdown_sql_track_b(
    dim_col: &str,
    metric: &Metric,
    where_clause: &str,
    limit: usize,
) -> String {
    // Map Metric to the Track B column name.
    let order_col = match metric {
        Metric::ApiTotal => "grand_total_tokens",
        Metric::ApiInput => "total_input_tokens",
        Metric::ApiOutput => "total_output_tokens",
        Metric::CacheRead => "total_cache_read_tokens",
        Metric::CacheCreation => "total_cache_creation_tokens",
        Metric::Thinking => "total_thinking_tokens",
        Metric::ToolCalls => "total_tool_calls",
        Metric::MessageCount => "api_call_count",
        Metric::EstimatedCostUsd => "estimated_cost_usd",
        _ => "grand_total_tokens",
    };
    format!(
        "SELECT {dim_col},
                SUM(api_call_count),
                SUM(user_message_count),
                SUM(assistant_message_count),
                SUM(total_tool_calls),
                SUM(total_input_tokens),
                SUM(total_output_tokens),
                SUM(total_cache_read_tokens),
                SUM(total_cache_creation_tokens),
                SUM(total_thinking_tokens),
                SUM(grand_total_tokens),
                SUM(total_content_chars),
                SUM(estimated_cost_usd),
                SUM({order_col})
         FROM token_daily_stats
         {where_clause}
         GROUP BY {dim_col}
         ORDER BY SUM({order_col}) DESC
         LIMIT {limit}"
    )
}

/// Read breakdown rows from a Track A (usage_daily) query result.
fn read_breakdown_rows_track_a(
    stmt: &mut rusqlite::Statement<'_>,
    params: &[&dyn rusqlite::types::ToSql],
    metric: &Metric,
) -> AnalyticsResult<Vec<BreakdownRow>> {
    let rows_result = stmt
        .query_map(params, |row| {
            let key: String = row.get(0)?;
            let bucket = UsageBucket {
                message_count: row.get(1)?,
                user_message_count: row.get(2)?,
                assistant_message_count: row.get(3)?,
                tool_call_count: row.get(4)?,
                plan_message_count: row.get(5)?,
                plan_content_tokens_est_total: row.get(6)?,
                plan_api_tokens_total: row.get(7)?,
                api_coverage_message_count: row.get(8)?,
                content_tokens_est_total: row.get(9)?,
                content_tokens_est_user: row.get(10)?,
                content_tokens_est_assistant: row.get(11)?,
                api_tokens_total: row.get(12)?,
                api_input_tokens_total: row.get(13)?,
                api_output_tokens_total: row.get(14)?,
                api_cache_read_tokens_total: row.get(15)?,
                api_cache_creation_tokens_total: row.get(16)?,
                api_thinking_tokens_total: row.get(17)?,
                ..Default::default()
            };
            let sort_value: i64 = row.get(18)?;
            Ok((key, bucket, sort_value))
        })
        .map_err(|e| AnalyticsError::Db(format!("Breakdown query failed: {e}")))?;

    let mut result = Vec::new();
    for row in rows_result {
        let (key, bucket, sort_value) =
            row.map_err(|e| AnalyticsError::Db(format!("Row read error: {e}")))?;
        // For CoveragePct, compute derived value instead of using raw column.
        let value = if *metric == Metric::CoveragePct {
            let pct =
                super::derive::safe_pct(bucket.api_coverage_message_count, bucket.message_count);
            pct as i64
        } else {
            sort_value
        };
        result.push(BreakdownRow {
            message_count: bucket.message_count,
            key,
            value,
            bucket,
        });
    }
    Ok(result)
}

/// Read breakdown rows from a Track B (token_daily_stats) query result.
fn read_breakdown_rows_track_b(
    stmt: &mut rusqlite::Statement<'_>,
    params: &[&dyn rusqlite::types::ToSql],
    _metric: &Metric,
) -> AnalyticsResult<Vec<BreakdownRow>> {
    let rows_result = stmt
        .query_map(params, |row| {
            let key: String = row.get(0)?;
            let api_call_count: i64 = row.get(1)?;
            let user_message_count: i64 = row.get(2)?;
            let assistant_message_count: i64 = row.get(3)?;
            let total_tool_calls: i64 = row.get(4)?;
            let total_input: i64 = row.get(5)?;
            let total_output: i64 = row.get(6)?;
            let total_cache_read: i64 = row.get(7)?;
            let total_cache_creation: i64 = row.get(8)?;
            let total_thinking: i64 = row.get(9)?;
            let grand_total: i64 = row.get(10)?;
            let total_content_chars: i64 = row.get(11)?;
            let estimated_cost: f64 = row.get(12)?;
            // When the sort metric is a Real column (e.g. estimated_cost_usd),
            // SQLite returns a float.  Read as f64 and truncate to i64.
            let sort_value: i64 = match row.get::<_, f64>(13) {
                Ok(v) => v as i64,
                Err(_) => row.get(13)?,
            };

            // Map Track B columns to UsageBucket.
            let bucket = UsageBucket {
                message_count: api_call_count,
                user_message_count,
                assistant_message_count,
                tool_call_count: total_tool_calls,
                api_coverage_message_count: api_call_count, // all are API-sourced in Track B
                content_tokens_est_total: total_content_chars / 4, // chars → tokens estimate
                api_tokens_total: grand_total,
                api_input_tokens_total: total_input,
                api_output_tokens_total: total_output,
                api_cache_read_tokens_total: total_cache_read,
                api_cache_creation_tokens_total: total_cache_creation,
                api_thinking_tokens_total: total_thinking,
                estimated_cost_usd: estimated_cost,
                ..Default::default()
            };

            Ok((key, bucket, sort_value))
        })
        .map_err(|e| AnalyticsError::Db(format!("Breakdown query failed: {e}")))?;

    let mut result = Vec::new();
    for row in rows_result {
        let (key, bucket, sort_value) =
            row.map_err(|e| AnalyticsError::Db(format!("Row read error: {e}")))?;
        result.push(BreakdownRow {
            message_count: bucket.message_count,
            key,
            value: sort_value,
            bucket,
        });
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// query_tools
// ---------------------------------------------------------------------------

/// Run a tool usage report — tool calls broken down by a dimension.
///
/// Uses `usage_daily` (Track A) which has reliable `tool_call_count`.
/// Returns rows ordered by tool_call_count descending, capped at `limit`.
pub fn query_tools(
    conn: &Connection,
    filter: &AnalyticsFilter,
    group_by: GroupBy,
    limit: usize,
) -> AnalyticsResult<ToolReport> {
    let query_start = std::time::Instant::now();

    let (table, bucket_col) = match group_by {
        GroupBy::Hour => ("usage_hourly", "hour_id"),
        _ => ("usage_daily", "day_id"),
    };

    if !table_exists(conn, table) {
        return Ok(ToolReport {
            rows: vec![],
            total_tool_calls: 0,
            total_messages: 0,
            total_api_tokens: 0,
            source_table: table.into(),
            elapsed_ms: query_start.elapsed().as_millis() as u64,
        });
    }

    // Build WHERE clause.
    let (day_min, day_max) = bucketing::resolve_day_range(filter);
    let (hour_min, hour_max) = bucketing::resolve_hour_range(filter);
    let (dim_parts, dim_params) = build_where_parts(filter);
    let mut where_parts = dim_parts;
    let mut bind_values = dim_params;

    match group_by {
        GroupBy::Hour => {
            if let Some(min) = hour_min {
                bind_values.push(min.to_string());
                where_parts.push(format!("{bucket_col} >= ?{}", bind_values.len()));
            }
            if let Some(max) = hour_max {
                bind_values.push(max.to_string());
                where_parts.push(format!("{bucket_col} <= ?{}", bind_values.len()));
            }
        }
        _ => {
            if let Some(min) = day_min {
                bind_values.push(min.to_string());
                where_parts.push(format!("{bucket_col} >= ?{}", bind_values.len()));
            }
            if let Some(max) = day_max {
                bind_values.push(max.to_string());
                where_parts.push(format!("{bucket_col} <= ?{}", bind_values.len()));
            }
        }
    }

    let where_clause = if where_parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_parts.join(" AND "))
    };

    // Group by agent_slug for tool breakdown (most useful default).
    let sql = format!(
        "SELECT agent_slug,
                SUM(tool_call_count),
                SUM(message_count),
                SUM(api_tokens_total),
                SUM(content_tokens_est_total)
         FROM {table}
         {where_clause}
         GROUP BY agent_slug
         ORDER BY SUM(tool_call_count) DESC
         LIMIT {limit}"
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| AnalyticsError::Db(format!("Failed to prepare tool report query: {e}")))?;

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = bind_values
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();

    let rows_result = stmt
        .query_map(param_refs.as_slice(), |row| {
            let key: String = row.get(0)?;
            let tool_call_count: i64 = row.get(1)?;
            let message_count: i64 = row.get(2)?;
            let api_tokens_total: i64 = row.get(3)?;
            let content_tokens_est_total: i64 = row.get(4)?;

            let tool_calls_per_1k_api = if api_tokens_total > 0 {
                Some(tool_call_count as f64 / (api_tokens_total as f64 / 1000.0))
            } else {
                None
            };
            let tool_calls_per_1k_content = if content_tokens_est_total > 0 {
                Some(tool_call_count as f64 / (content_tokens_est_total as f64 / 1000.0))
            } else {
                None
            };

            Ok(ToolRow {
                key,
                tool_call_count,
                message_count,
                api_tokens_total,
                tool_calls_per_1k_api_tokens: tool_calls_per_1k_api,
                tool_calls_per_1k_content_tokens: tool_calls_per_1k_content,
            })
        })
        .map_err(|e| AnalyticsError::Db(format!("Tool report query failed: {e}")))?;

    let mut rows = Vec::new();
    let mut total_tool_calls: i64 = 0;
    let mut total_messages: i64 = 0;
    let mut total_api_tokens: i64 = 0;

    for row in rows_result {
        let r = row.map_err(|e| AnalyticsError::Db(format!("Row read error: {e}")))?;
        total_tool_calls += r.tool_call_count;
        total_messages += r.message_count;
        total_api_tokens += r.api_tokens_total;
        rows.push(r);
    }

    let elapsed_ms = query_start.elapsed().as_millis() as u64;

    Ok(ToolReport {
        rows,
        total_tool_calls,
        total_messages,
        total_api_tokens,
        source_table: table.into(),
        elapsed_ms,
    })
}

// ---------------------------------------------------------------------------
// Unpriced models — discover unknown/unmatched pricing
// ---------------------------------------------------------------------------

/// Query `token_usage` for model names that have `estimated_cost_usd IS NULL`,
/// grouped by model_name with total token counts.  Returns the top `limit`
/// unpriced models sorted by total_tokens descending.
pub fn query_unpriced_models(
    conn: &Connection,
    limit: usize,
) -> AnalyticsResult<UnpricedModelsReport> {
    if !table_exists(conn, "token_usage") {
        return Ok(UnpricedModelsReport {
            models: Vec::new(),
            total_unpriced_tokens: 0,
            total_priced_tokens: 0,
        });
    }

    // Unpriced models
    let mut stmt = conn
        .prepare(
            "SELECT COALESCE(model_name, '(none)') AS model,
                    SUM(COALESCE(total_tokens, 0)) AS tot,
                    COUNT(*) AS cnt
             FROM token_usage
             WHERE estimated_cost_usd IS NULL
             GROUP BY model
             ORDER BY tot DESC
             LIMIT ?1",
        )
        .map_err(|e| AnalyticsError::Db(e.to_string()))?;

    let models: Vec<UnpricedModel> = stmt
        .query_map([limit as i64], |row| {
            Ok(UnpricedModel {
                model_name: row.get(0)?,
                total_tokens: row.get(1)?,
                row_count: row.get(2)?,
            })
        })
        .map_err(|e| AnalyticsError::Db(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();

    let total_unpriced_tokens: i64 = models.iter().map(|m| m.total_tokens).sum();

    // Total priced tokens for context
    let total_priced_tokens: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(COALESCE(total_tokens, 0)), 0)
             FROM token_usage
             WHERE estimated_cost_usd IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    Ok(UnpricedModelsReport {
        models,
        total_unpriced_tokens,
        total_priced_tokens,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_where_parts_empty_filter() {
        let f = AnalyticsFilter::default();
        let (parts, params) = build_where_parts(&f);
        assert!(parts.is_empty());
        assert!(params.is_empty());
    }

    #[test]
    fn build_where_parts_single_agent() {
        let f = AnalyticsFilter {
            agents: vec!["claude_code".into()],
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains("agent_slug IN"));
        assert_eq!(params, vec!["claude_code"]);
    }

    #[test]
    fn build_where_parts_multiple_agents() {
        let f = AnalyticsFilter {
            agents: vec!["claude_code".into(), "codex".into(), "aider".into()],
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains("?1"));
        assert!(parts[0].contains("?2"));
        assert!(parts[0].contains("?3"));
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn build_where_parts_source_local() {
        let f = AnalyticsFilter {
            source: SourceFilter::Local,
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains("source_id = ?1"));
        assert_eq!(params, vec!["local"]);
    }

    #[test]
    fn build_where_parts_source_remote() {
        let f = AnalyticsFilter {
            source: SourceFilter::Remote,
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains("source_id != ?1"));
        assert_eq!(params, vec!["local"]);
    }

    #[test]
    fn build_where_parts_source_specific() {
        let f = AnalyticsFilter {
            source: SourceFilter::Specific("myhost.local".into()),
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains("source_id = ?1"));
        assert_eq!(params, vec!["myhost.local"]);
    }

    #[test]
    fn build_where_parts_combined() {
        let f = AnalyticsFilter {
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f);
        assert_eq!(parts.len(), 2);
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], "codex");
        assert_eq!(params[1], "local");
    }

    // -----------------------------------------------------------------------
    // Integration tests with in-memory SQLite
    // -----------------------------------------------------------------------

    /// Create an in-memory database with the usage_daily schema and seed data.
    fn setup_usage_daily_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_daily (
                day_id INTEGER NOT NULL,
                agent_slug TEXT NOT NULL,
                workspace_id INTEGER NOT NULL DEFAULT 0,
                source_id TEXT NOT NULL DEFAULT 'local',
                message_count INTEGER NOT NULL DEFAULT 0,
                user_message_count INTEGER NOT NULL DEFAULT 0,
                assistant_message_count INTEGER NOT NULL DEFAULT 0,
                tool_call_count INTEGER NOT NULL DEFAULT 0,
                plan_message_count INTEGER NOT NULL DEFAULT 0,
                plan_content_tokens_est_total INTEGER NOT NULL DEFAULT 0,
                plan_api_tokens_total INTEGER NOT NULL DEFAULT 0,
                api_coverage_message_count INTEGER NOT NULL DEFAULT 0,
                content_tokens_est_total INTEGER NOT NULL DEFAULT 0,
                content_tokens_est_user INTEGER NOT NULL DEFAULT 0,
                content_tokens_est_assistant INTEGER NOT NULL DEFAULT 0,
                api_tokens_total INTEGER NOT NULL DEFAULT 0,
                api_input_tokens_total INTEGER NOT NULL DEFAULT 0,
                api_output_tokens_total INTEGER NOT NULL DEFAULT 0,
                api_cache_read_tokens_total INTEGER NOT NULL DEFAULT 0,
                api_cache_creation_tokens_total INTEGER NOT NULL DEFAULT 0,
                api_thinking_tokens_total INTEGER NOT NULL DEFAULT 0,
                last_updated INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (day_id, agent_slug, workspace_id, source_id)
            );",
        )
        .unwrap();

        // Seed: 3 agents across 2 days
        let rows = [
            (
                20250,
                "claude_code",
                1,
                "local",
                100,
                50,
                50,
                20,
                5,
                80,
                40000,
                20000,
                20000,
                60000,
                30000,
                25000,
                3000,
                1500,
                500,
            ),
            (
                20250, "codex", 1, "local", 50, 25, 25, 10, 2, 40, 20000, 10000, 10000, 30000,
                15000, 12000, 2000, 800, 200,
            ),
            (
                20250, "aider", 2, "remote", 30, 15, 15, 5, 0, 0, 12000, 6000, 6000, 0, 0, 0, 0, 0,
                0,
            ),
            (
                20251,
                "claude_code",
                1,
                "local",
                120,
                60,
                60,
                25,
                8,
                100,
                50000,
                25000,
                25000,
                80000,
                40000,
                32000,
                5000,
                2000,
                1000,
            ),
            (
                20251, "codex", 1, "local", 60, 30, 30, 15, 3, 50, 25000, 12500, 12500, 40000,
                20000, 16000, 2500, 1000, 500,
            ),
        ];

        for r in &rows {
            conn.execute(
                "INSERT INTO usage_daily (day_id, agent_slug, workspace_id, source_id,
                    message_count, user_message_count, assistant_message_count,
                    tool_call_count, plan_message_count, api_coverage_message_count,
                    content_tokens_est_total, content_tokens_est_user, content_tokens_est_assistant,
                    api_tokens_total, api_input_tokens_total, api_output_tokens_total,
                    api_cache_read_tokens_total, api_cache_creation_tokens_total,
                    api_thinking_tokens_total)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
                rusqlite::params![
                    r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8, r.9, r.10, r.11, r.12, r.13, r.14,
                    r.15, r.16, r.17, r.18
                ],
            )
            .unwrap();
        }

        conn
    }

    /// Legacy Track A schema fixture (pre plan-token rollup columns).
    fn setup_usage_daily_legacy_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_daily (
                day_id INTEGER NOT NULL,
                agent_slug TEXT NOT NULL,
                workspace_id INTEGER NOT NULL DEFAULT 0,
                source_id TEXT NOT NULL DEFAULT 'local',
                message_count INTEGER NOT NULL DEFAULT 0,
                user_message_count INTEGER NOT NULL DEFAULT 0,
                assistant_message_count INTEGER NOT NULL DEFAULT 0,
                tool_call_count INTEGER NOT NULL DEFAULT 0,
                plan_message_count INTEGER NOT NULL DEFAULT 0,
                api_coverage_message_count INTEGER NOT NULL DEFAULT 0,
                content_tokens_est_total INTEGER NOT NULL DEFAULT 0,
                content_tokens_est_user INTEGER NOT NULL DEFAULT 0,
                content_tokens_est_assistant INTEGER NOT NULL DEFAULT 0,
                api_tokens_total INTEGER NOT NULL DEFAULT 0,
                api_input_tokens_total INTEGER NOT NULL DEFAULT 0,
                api_output_tokens_total INTEGER NOT NULL DEFAULT 0,
                api_cache_read_tokens_total INTEGER NOT NULL DEFAULT 0,
                api_cache_creation_tokens_total INTEGER NOT NULL DEFAULT 0,
                api_thinking_tokens_total INTEGER NOT NULL DEFAULT 0,
                last_updated INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (day_id, agent_slug, workspace_id, source_id)
            );
            INSERT INTO usage_daily VALUES
                (20254, 'codex', 1, 'local',
                 3, 1, 2, 4, 1, 2,
                 900, 300, 600,
                 1200, 600, 500, 50, 30, 20,
                 0);",
        )
        .unwrap();
        conn
    }

    /// Create an in-memory database with the token_daily_stats schema and seed data.
    fn setup_token_daily_stats_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE token_daily_stats (
                day_id INTEGER NOT NULL,
                agent_slug TEXT NOT NULL,
                source_id TEXT NOT NULL DEFAULT 'all',
                model_family TEXT NOT NULL DEFAULT 'all',
                api_call_count INTEGER NOT NULL DEFAULT 0,
                user_message_count INTEGER NOT NULL DEFAULT 0,
                assistant_message_count INTEGER NOT NULL DEFAULT 0,
                tool_message_count INTEGER NOT NULL DEFAULT 0,
                total_input_tokens INTEGER NOT NULL DEFAULT 0,
                total_output_tokens INTEGER NOT NULL DEFAULT 0,
                total_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                total_cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                total_thinking_tokens INTEGER NOT NULL DEFAULT 0,
                grand_total_tokens INTEGER NOT NULL DEFAULT 0,
                total_content_chars INTEGER NOT NULL DEFAULT 0,
                total_tool_calls INTEGER NOT NULL DEFAULT 0,
                estimated_cost_usd REAL NOT NULL DEFAULT 0.0,
                session_count INTEGER NOT NULL DEFAULT 0,
                last_updated INTEGER NOT NULL,
                PRIMARY KEY (day_id, agent_slug, source_id, model_family)
            );",
        )
        .unwrap();

        // Seed: 2 models across 1 day
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO token_daily_stats VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            rusqlite::params![20250, "claude_code", "local", "opus", 80, 40, 40, 5, 30000, 25000, 3000, 1500, 500, 60000, 160000, 20, 1.50, 3, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO token_daily_stats VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            rusqlite::params![20250, "claude_code", "local", "sonnet", 40, 20, 20, 2, 10000, 8000, 1000, 500, 200, 19700, 80000, 8, 0.40, 2, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO token_daily_stats VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            rusqlite::params![20250, "codex", "local", "gpt-4o", 50, 25, 25, 3, 15000, 12000, 2000, 800, 0, 29800, 100000, 10, 0.80, 1, now],
        ).unwrap();

        conn
    }

    #[test]
    fn query_breakdown_by_agent_returns_ordered_rows() {
        let conn = setup_usage_daily_db();
        let filter = AnalyticsFilter::default();
        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::ApiTotal, 10).unwrap();

        assert_eq!(result.dim, Dim::Agent);
        assert_eq!(result.metric, Metric::ApiTotal);
        assert!(!result.rows.is_empty());
        // claude_code should be first (highest api_tokens_total)
        assert_eq!(result.rows[0].key, "claude_code");
        // Verify descending order
        for i in 1..result.rows.len() {
            assert!(result.rows[i - 1].value >= result.rows[i].value);
        }
    }

    #[test]
    fn query_breakdown_by_source_filters_correctly() {
        let conn = setup_usage_daily_db();
        let filter = AnalyticsFilter {
            source: SourceFilter::Local,
            ..Default::default()
        };
        let result =
            query_breakdown(&conn, &filter, Dim::Source, Metric::MessageCount, 10).unwrap();

        // Only "local" source should appear
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "local");
    }

    #[test]
    fn query_breakdown_by_model_uses_track_b() {
        let conn = setup_token_daily_stats_db();
        let filter = AnalyticsFilter::default();
        let result = query_breakdown(&conn, &filter, Dim::Model, Metric::ApiTotal, 10).unwrap();

        assert_eq!(result.source_table, "token_daily_stats");
        assert_eq!(result.rows.len(), 3); // opus, gpt-4o, sonnet
        // opus has highest grand_total (60000)
        assert_eq!(result.rows[0].key, "opus");
    }

    #[test]
    fn query_breakdown_limit_caps_rows() {
        let conn = setup_usage_daily_db();
        let filter = AnalyticsFilter::default();
        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::ApiTotal, 2).unwrap();

        assert_eq!(result.rows.len(), 2);
    }

    #[test]
    fn query_breakdown_missing_table_returns_empty() {
        let conn = Connection::open_in_memory().unwrap();
        let filter = AnalyticsFilter::default();
        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::ApiTotal, 10).unwrap();
        assert!(result.rows.is_empty());
    }

    #[test]
    fn query_tokens_timeseries_legacy_rollup_schema_defaults_plan_token_rollups_to_zero() {
        let conn = setup_usage_daily_legacy_db();
        let filter = AnalyticsFilter::default();
        let result = query_tokens_timeseries(&conn, &filter, GroupBy::Day).unwrap();

        assert_eq!(result.buckets.len(), 1);
        let bucket = &result.buckets[0].1;
        assert_eq!(bucket.plan_content_tokens_est_total, 0);
        assert_eq!(bucket.plan_api_tokens_total, 0);
    }

    #[test]
    fn query_breakdown_result_to_json_shape() {
        let conn = setup_usage_daily_db();
        let filter = AnalyticsFilter::default();
        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::ToolCalls, 10).unwrap();

        let json = result.to_cli_json();
        assert_eq!(json["dim"], "agent");
        assert_eq!(json["metric"], "tool_calls");
        assert!(json["rows"].is_array());
        assert!(json["row_count"].is_number());
        assert!(json["_meta"]["elapsed_ms"].is_number());
    }

    #[test]
    fn query_tools_returns_agent_breakdown() {
        let conn = setup_usage_daily_db();
        let filter = AnalyticsFilter::default();
        let result = query_tools(&conn, &filter, GroupBy::Day, 10).unwrap();

        assert!(!result.rows.is_empty());
        // claude_code should have the most tool calls (20+25=45)
        assert_eq!(result.rows[0].key, "claude_code");
        assert_eq!(result.rows[0].tool_call_count, 45);

        // Totals should sum correctly
        let sum: i64 = result.rows.iter().map(|r| r.tool_call_count).sum();
        assert_eq!(result.total_tool_calls, sum);
    }

    #[test]
    fn query_tools_derived_metrics_correct() {
        let conn = setup_usage_daily_db();
        let filter = AnalyticsFilter::default();
        let result = query_tools(&conn, &filter, GroupBy::Day, 10).unwrap();

        for row in &result.rows {
            if row.api_tokens_total > 0 {
                let expected = row.tool_call_count as f64 / (row.api_tokens_total as f64 / 1000.0);
                assert!((row.tool_calls_per_1k_api_tokens.unwrap() - expected).abs() < 0.001);
            }
        }
    }

    #[test]
    fn query_tools_missing_table_returns_empty() {
        let conn = Connection::open_in_memory().unwrap();
        let filter = AnalyticsFilter::default();
        let result = query_tools(&conn, &filter, GroupBy::Day, 10).unwrap();
        assert!(result.rows.is_empty());
        assert_eq!(result.total_tool_calls, 0);
    }

    #[test]
    fn query_tools_report_to_json_shape() {
        let conn = setup_usage_daily_db();
        let filter = AnalyticsFilter::default();
        let result = query_tools(&conn, &filter, GroupBy::Day, 10).unwrap();

        let json = result.to_cli_json();
        assert!(json["rows"].is_array());
        assert!(json["totals"]["tool_call_count"].is_number());
        assert!(json["_meta"]["elapsed_ms"].is_number());
    }

    #[test]
    fn query_breakdown_with_agent_filter() {
        let conn = setup_usage_daily_db();
        let filter = AnalyticsFilter {
            agents: vec!["codex".into()],
            ..Default::default()
        };
        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::ApiTotal, 10).unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "codex");
        // Total should be 30000 + 40000 = 70000
        assert_eq!(result.rows[0].value, 70000);
    }

    #[test]
    fn metric_display_roundtrip() {
        assert_eq!(Metric::ApiTotal.to_string(), "api_total");
        assert_eq!(Metric::ToolCalls.to_string(), "tool_calls");
        assert_eq!(Metric::CoveragePct.to_string(), "coverage_pct");
    }

    #[test]
    fn dim_display_roundtrip() {
        assert_eq!(Dim::Agent.to_string(), "agent");
        assert_eq!(Dim::Model.to_string(), "model");
        assert_eq!(Dim::Workspace.to_string(), "workspace");
        assert_eq!(Dim::Source.to_string(), "source");
    }

    #[test]
    fn metric_rollup_column_coverage_pct_is_none() {
        assert!(Metric::CoveragePct.rollup_column().is_none());
    }

    #[test]
    fn metric_rollup_column_api_total_is_some() {
        assert_eq!(Metric::ApiTotal.rollup_column(), Some("api_tokens_total"));
    }

    // -----------------------------------------------------------------------
    // query_cost_timeseries tests
    // -----------------------------------------------------------------------

    #[test]
    fn query_cost_timeseries_returns_cost_data() {
        let conn = setup_token_daily_stats_db();
        let filter = AnalyticsFilter::default();
        let result = query_cost_timeseries(&conn, &filter, GroupBy::Day).unwrap();

        assert_eq!(result.source_table, "token_daily_stats");
        assert_eq!(result.buckets.len(), 1); // all seeded on day 20250
        let (_, bucket) = &result.buckets[0];
        // Total cost: opus 1.50 + sonnet 0.40 + gpt-4o 0.80 = 2.70
        assert!((bucket.estimated_cost_usd - 2.70).abs() < 0.01);
        // Total api_tokens: 60000 + 19700 + 29800 = 109500
        assert_eq!(bucket.api_tokens_total, 109_500);
        // Total messages: 80 + 40 + 50 = 170
        assert_eq!(bucket.message_count, 170);
    }

    #[test]
    fn query_cost_timeseries_totals_match_bucket_sums() {
        let conn = setup_token_daily_stats_db();
        let filter = AnalyticsFilter::default();
        let result = query_cost_timeseries(&conn, &filter, GroupBy::Day).unwrap();

        let sum_cost: f64 = result
            .buckets
            .iter()
            .map(|(_, b)| b.estimated_cost_usd)
            .sum();
        assert!((result.totals.estimated_cost_usd - sum_cost).abs() < 0.001);
    }

    #[test]
    fn query_cost_timeseries_missing_table_returns_empty() {
        let conn = Connection::open_in_memory().unwrap();
        let filter = AnalyticsFilter::default();
        let result = query_cost_timeseries(&conn, &filter, GroupBy::Day).unwrap();

        assert!(result.buckets.is_empty());
        assert_eq!(result.totals.estimated_cost_usd, 0.0);
        assert_eq!(result.path, "none");
    }

    #[test]
    fn query_breakdown_agent_with_cost_metric_uses_track_b() {
        let conn = setup_token_daily_stats_db();
        let filter = AnalyticsFilter::default();
        let result =
            query_breakdown(&conn, &filter, Dim::Agent, Metric::EstimatedCostUsd, 10).unwrap();

        // Should route to token_daily_stats (Track B), not usage_daily.
        assert_eq!(result.source_table, "token_daily_stats");
        assert!(!result.rows.is_empty());
        // claude_code has cost 1.50 + 0.40 = 1.90, codex has 0.80
        assert_eq!(result.rows[0].key, "claude_code");
        assert!((result.rows[0].bucket.estimated_cost_usd - 1.90).abs() < 0.01);
        assert!((result.rows[1].bucket.estimated_cost_usd - 0.80).abs() < 0.01);
    }

    #[test]
    fn query_breakdown_model_with_cost_metric_orders_by_cost() {
        let conn = setup_token_daily_stats_db();
        let filter = AnalyticsFilter::default();
        let result =
            query_breakdown(&conn, &filter, Dim::Model, Metric::EstimatedCostUsd, 10).unwrap();

        // Should order by estimated_cost_usd DESC: opus (1.50) > codex/gpt-4o (0.80) > sonnet (0.40)
        assert_eq!(result.rows[0].key, "opus");
        assert!((result.rows[0].bucket.estimated_cost_usd - 1.50).abs() < 0.01);
    }
}
