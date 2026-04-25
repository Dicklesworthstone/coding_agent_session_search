//! SQL query builders for analytics.
//!
//! All functions accept a `&frankensqlite::Connection` and an [`AnalyticsFilter`],
//! keeping the SQL and bucketing logic in one place for both CLI and ftui.

use std::collections::BTreeMap;

use frankensqlite::Connection;
use frankensqlite::Row;
use frankensqlite::compat::{ConnectionExt, ParamValue, RowExt};

use super::bucketing;
use super::types::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check whether a table exists in the database.
pub fn table_exists(conn: &Connection, name: &str) -> bool {
    // Basic validation to prevent SQL injection in table name.
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return false;
    }
    let rows =
        match conn.query_map_collect(&format!("PRAGMA table_info({})", name), &[], |row: &Row| {
            row.get_typed::<String>(1)
        }) {
            Ok(rows) => rows,
            Err(_) => return false,
        };
    !rows.is_empty()
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> bool {
    // Basic validation to prevent SQL injection.
    if !table.chars().all(|c| c.is_alphanumeric() || c == '_')
        || !column.chars().all(|c| c.is_alphanumeric() || c == '_')
    {
        return false;
    }
    let rows =
        match conn.query_map_collect(&format!("PRAGMA table_info({table})"), &[], |row: &Row| {
            row.get_typed::<String>(1)
        }) {
            Ok(rows) => rows,
            Err(_) => return false,
        };

    rows.iter().any(|name| name == column)
}

fn table_has_plan_token_rollups(conn: &Connection, table: &str) -> bool {
    table_has_column(conn, table, "plan_content_tokens_est_total")
        && table_has_column(conn, table, "plan_api_tokens_total")
}

fn normalize_epoch_millis(ts: i64) -> i64 {
    // Support legacy second-based values while preserving millisecond values.
    if (0..100_000_000_000).contains(&ts) {
        ts.saturating_mul(1000)
    } else {
        ts
    }
}
fn normalized_epoch_millis_sql(expr: &str) -> String {
    format!(
        "CASE WHEN ({expr}) >= 0 AND ({expr}) < 100000000000 THEN ({expr}) * 1000 ELSE ({expr}) END"
    )
}

fn is_recently_updated(last_updated: Option<i64>, now_ms: i64, threshold_ms: i64) -> bool {
    last_updated.is_some_and(|ts| (now_ms - normalize_epoch_millis(ts)).abs() < threshold_ms)
}

fn normalized_analytics_source_id_value(source_id: &str) -> String {
    let trimmed = source_id.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case(crate::sources::provenance::LOCAL_SOURCE_ID)
    {
        crate::sources::provenance::LOCAL_SOURCE_ID.to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalized_analytics_source_identity_value(source_id: &str, origin_host: &str) -> String {
    let trimmed_source_id = source_id.trim();
    if trimmed_source_id.is_empty() {
        let trimmed_origin_host = origin_host.trim();
        if trimmed_origin_host.is_empty() {
            crate::sources::provenance::LOCAL_SOURCE_ID.to_string()
        } else {
            trimmed_origin_host.to_string()
        }
    } else if trimmed_source_id.eq_ignore_ascii_case(crate::sources::provenance::LOCAL_SOURCE_ID) {
        crate::sources::provenance::LOCAL_SOURCE_ID.to_string()
    } else {
        trimmed_source_id.to_string()
    }
}

fn breakdown_row_with_value(key: String, bucket: UsageBucket, value: i64) -> BreakdownRow {
    BreakdownRow {
        message_count: bucket.message_count,
        key,
        value,
        bucket,
    }
}

fn analytics_query_error(context: &str, err: impl std::fmt::Display) -> AnalyticsError {
    AnalyticsError::Db(format!("{context}: {err}"))
}

fn normalized_analytics_source_id_sql_expr(column: &str) -> String {
    format!(
        "CASE WHEN TRIM(COALESCE({column}, '')) = '' THEN '{local}'          WHEN LOWER(TRIM(COALESCE({column}, ''))) = '{local}' THEN '{local}'          ELSE TRIM(COALESCE({column}, '')) END",
        local = crate::sources::provenance::LOCAL_SOURCE_ID,
    )
}

fn normalized_analytics_source_identity_sql_expr(
    source_id_column: &str,
    origin_host_column: &str,
) -> String {
    format!(
        "CASE WHEN TRIM(COALESCE({source_id_column}, '')) = '' THEN             CASE WHEN TRIM(COALESCE({origin_host_column}, '')) = '' THEN '{local}'                  ELSE TRIM(COALESCE({origin_host_column}, '')) END          WHEN LOWER(TRIM(COALESCE({source_id_column}, ''))) = '{local}' THEN '{local}'          ELSE TRIM(COALESCE({source_id_column}, '')) END",
        local = crate::sources::provenance::LOCAL_SOURCE_ID,
    )
}

fn normalized_analytics_source_id_with_fallback_sql_expr(
    primary_source_id_column: &str,
    fallback_source_id_column: &str,
) -> String {
    let fallback_sql = normalized_analytics_source_id_sql_expr(fallback_source_id_column);
    format!(
        "CASE WHEN TRIM(COALESCE({primary_source_id_column}, '')) = '' THEN {fallback_sql}          WHEN LOWER(TRIM(COALESCE({primary_source_id_column}, ''))) = '{local}' THEN '{local}'          ELSE TRIM(COALESCE({primary_source_id_column}, '')) END",
        local = crate::sources::provenance::LOCAL_SOURCE_ID,
    )
}

fn normalized_analytics_source_identity_with_fallback_sql_expr(
    primary_source_id_column: &str,
    fallback_source_id_column: &str,
    origin_host_column: &str,
) -> String {
    let fallback_sql = normalized_analytics_source_identity_sql_expr(
        fallback_source_id_column,
        origin_host_column,
    );
    format!(
        "CASE WHEN TRIM(COALESCE({primary_source_id_column}, '')) = '' THEN {fallback_sql}          WHEN LOWER(TRIM(COALESCE({primary_source_id_column}, ''))) = '{local}' THEN '{local}'          ELSE TRIM(COALESCE({primary_source_id_column}, '')) END",
        local = crate::sources::provenance::LOCAL_SOURCE_ID,
    )
}

fn normalized_analytics_agent_value(agent_slug: &str) -> String {
    let trimmed = agent_slug.trim();
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalized_analytics_agent_sql_expr(column: &str) -> String {
    format!(
        "CASE WHEN TRIM(COALESCE({column}, '')) = '' THEN 'unknown' ELSE TRIM(COALESCE({column}, '')) END"
    )
}

fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn canonical_message_metrics_from_sql(conn: &Connection) -> Option<String> {
    if !table_exists(conn, "message_metrics")
        || !table_has_column(conn, "message_metrics", "message_id")
    {
        return None;
    }

    let mut select_parts = vec!["message_id".to_string()];
    for column in [
        "created_at_ms",
        "hour_id",
        "day_id",
        "agent_slug",
        "workspace_id",
        "source_id",
        "role",
        "content_chars",
        "content_tokens_est",
        "api_input_tokens",
        "api_output_tokens",
        "api_cache_read_tokens",
        "api_cache_creation_tokens",
        "api_thinking_tokens",
        "tool_call_count",
        "has_plan",
    ] {
        if table_has_column(conn, "message_metrics", column) {
            select_parts.push(format!("MAX({column}) AS {column}"));
        }
    }
    if table_has_column(conn, "message_metrics", "api_data_source") {
        select_parts.push(
            "CASE
                WHEN MAX(CASE WHEN LOWER(TRIM(COALESCE(api_data_source, ''))) = 'api' THEN 1 ELSE 0 END) != 0 THEN 'api'
                WHEN MAX(CASE WHEN LOWER(TRIM(COALESCE(api_data_source, ''))) = 'estimated' THEN 1 ELSE 0 END) != 0 THEN 'estimated'
                ELSE NULL
             END AS api_data_source"
                .to_string(),
        );
    }

    Some(format!(
        "(SELECT {} FROM message_metrics GROUP BY message_id) mm",
        select_parts.join(", ")
    ))
}

fn analytics_source_filter_matches_key(filter: &SourceFilter, key: &str) -> bool {
    let normalized_key = normalized_analytics_source_id_value(key);
    match filter {
        SourceFilter::All => true,
        SourceFilter::Local => normalized_key == crate::sources::provenance::LOCAL_SOURCE_ID,
        SourceFilter::Remote => normalized_key != crate::sources::provenance::LOCAL_SOURCE_ID,
        SourceFilter::Specific(source_id) => {
            normalized_key == normalized_analytics_source_id_value(source_id)
        }
    }
}

fn push_source_filter_clause(
    parts: &mut Vec<String>,
    _params: &mut Vec<ParamValue>,
    filter: &SourceFilter,
    normalized_source_sql: &str,
) {
    match filter {
        SourceFilter::All => {}
        SourceFilter::Local => {
            parts.push(format!(
                "{normalized_source_sql} = {}",
                sql_string_literal(crate::sources::provenance::LOCAL_SOURCE_ID)
            ));
        }
        SourceFilter::Remote => {
            parts.push(format!(
                "{normalized_source_sql} != {}",
                sql_string_literal(crate::sources::provenance::LOCAL_SOURCE_ID)
            ));
        }
        SourceFilter::Specific(source_id) => {
            let normalized_source_id = normalized_analytics_source_id_value(source_id);
            parts.push(format!(
                "{normalized_source_sql} = {}",
                sql_string_literal(&normalized_source_id)
            ));
        }
    }
}

/// Internal stats from a single COUNT/MIN/MAX query on a rollup table.
#[derive(Debug, Default)]
struct RollupStats {
    row_count: i64,
    min_day: Option<i64>,
    max_day: Option<i64>,
    last_updated: Option<i64>,
}

/// Time-column kind for analytics filter application.
#[derive(Clone, Copy)]
enum AnalyticsTimeColumn<'a> {
    Day(&'a str),
    Hour(&'a str),
    TimestampMs(&'a str),
}

fn normalized_analytics_model_family_sql_expr(column: &str) -> String {
    format!(
        "CASE WHEN TRIM(COALESCE({column}, '')) = '' THEN 'unknown' ELSE TRIM(COALESCE({column}, '')) END"
    )
}

fn build_where_parts_for_columns<'a>(
    filter: &'a AnalyticsFilter,
    agent_column_sql: Option<String>,
    source_column_sql: String,
    workspace_column: Option<&'a str>,
) -> (Vec<String>, Vec<ParamValue>) {
    let mut parts: Vec<String> = Vec::new();
    let mut params: Vec<ParamValue> = Vec::new();

    if !filter.agents.is_empty() {
        if let Some(normalized_agent_sql) = agent_column_sql.as_deref() {
            if filter.agents.len() == 1 {
                parts.push(format!(
                    "{normalized_agent_sql} = {}",
                    sql_string_literal(&normalized_analytics_agent_value(
                        filter.agents[0].as_str()
                    ))
                ));
            } else {
                let agent_literals: Vec<String> = filter
                    .agents
                    .iter()
                    .map(|agent| {
                        sql_string_literal(&normalized_analytics_agent_value(agent.as_str()))
                    })
                    .collect();
                parts.push(format!(
                    "{normalized_agent_sql} IN ({})",
                    agent_literals.join(", ")
                ));
            }
        } else {
            parts.push("1 = 0".to_string());
            return (parts, params);
        }
    }

    push_source_filter_clause(&mut parts, &mut params, &filter.source, &source_column_sql);

    if let Some(workspace_column) = workspace_column
        && !filter.workspace_ids.is_empty()
    {
        if filter.workspace_ids.len() == 1 {
            parts.push(format!("{workspace_column} = {}", filter.workspace_ids[0]));
        } else {
            let workspace_literals: Vec<String> = filter
                .workspace_ids
                .iter()
                .map(|workspace_id| workspace_id.to_string())
                .collect();
            parts.push(format!(
                "{workspace_column} IN ({})",
                workspace_literals.join(", ")
            ));
        }
    }

    (parts, params)
}

fn build_filtered_where_sql<'a>(
    filter: &'a AnalyticsFilter,
    workspace_column: Option<&'a str>,
    agent_column_sql: Option<String>,
    source_column_sql: String,
    time_column: Option<AnalyticsTimeColumn<'a>>,
) -> (String, Vec<ParamValue>) {
    let (mut parts, params) = build_where_parts_for_columns(
        filter,
        agent_column_sql,
        source_column_sql,
        workspace_column,
    );

    match time_column {
        Some(AnalyticsTimeColumn::Day(column)) => {
            let (day_min, day_max) = bucketing::resolve_day_range(filter);
            if let Some(min) = day_min {
                parts.push(format!("{column} >= {min}"));
            }
            if let Some(max) = day_max {
                parts.push(format!("{column} <= {max}"));
            }
        }
        Some(AnalyticsTimeColumn::Hour(column)) => {
            let (hour_min, hour_max) = bucketing::resolve_hour_range(filter);
            if let Some(min) = hour_min {
                parts.push(format!("{column} >= {min}"));
            }
            if let Some(max) = hour_max {
                parts.push(format!("{column} <= {max}"));
            }
        }
        Some(AnalyticsTimeColumn::TimestampMs(column)) => {
            let normalized_column = normalized_epoch_millis_sql(column);
            if let Some(min) = filter.since_ms {
                parts.push(format!("{normalized_column} >= {min}"));
            }
            if let Some(max) = filter.until_ms {
                parts.push(format!("{normalized_column} <= {max}"));
            }
        }
        None => {}
    }

    let where_sql = if parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", parts.join(" AND "))
    };

    (where_sql, params)
}

/// Build SQL WHERE clause fragments and bind-parameter values from an
/// [`AnalyticsFilter`]'s dimensional (non-time) filters.
///
/// Returns `(clause_fragments, param_values)` where each fragment is like
/// `"agent_slug IN (?1, ?2)"` and `param_values` are the corresponding bind
/// strings.
///
/// `workspace_column` should be provided only for tables that contain a
/// workspace id dimension (for example `usage_daily.workspace_id`).
pub fn build_where_parts<'a>(
    filter: &'a AnalyticsFilter,
    workspace_column: Option<&'a str>,
) -> (Vec<String>, Vec<ParamValue>) {
    build_where_parts_for_columns(
        filter,
        Some(normalized_analytics_agent_sql_expr("agent_slug")),
        normalized_analytics_source_id_sql_expr("source_id"),
        workspace_column,
    )
}
fn message_metrics_time_sql(conn: &Connection) -> Option<String> {
    let has_messages = table_exists(conn, "messages");
    let has_conversations = table_exists(conn, "conversations");
    let joins_available = has_messages && has_conversations;
    let has_message_created_at =
        joins_available && table_has_column(conn, "messages", "created_at");
    let has_conversation_started_at =
        joins_available && table_has_column(conn, "conversations", "started_at");
    let has_message_metrics_created_at = table_has_column(conn, "message_metrics", "created_at_ms");

    let mut timestamp_terms: Vec<&str> = Vec::new();
    if has_message_created_at {
        timestamp_terms.push("m.created_at");
    }
    if has_message_metrics_created_at {
        timestamp_terms.push("mm.created_at_ms");
    }
    if has_conversation_started_at {
        timestamp_terms.push("c.started_at");
    }

    if timestamp_terms.is_empty() {
        None
    } else {
        Some(format!("COALESCE({}, 0)", timestamp_terms.join(", ")))
    }
}

fn message_metrics_from_sql_and_source_sql(conn: &Connection) -> (String, String) {
    let message_metrics_sql = canonical_message_metrics_from_sql(conn)
        .unwrap_or_else(|| "message_metrics mm".to_string());
    if table_exists(conn, "messages") && table_exists(conn, "conversations") {
        let source_sql = if table_has_column(conn, "conversations", "origin_host") {
            normalized_analytics_source_identity_with_fallback_sql_expr(
                "mm.source_id",
                "c.source_id",
                "c.origin_host",
            )
        } else {
            normalized_analytics_source_id_with_fallback_sql_expr("mm.source_id", "c.source_id")
        };
        (
            format!(
                "{message_metrics_sql} JOIN messages m ON m.id = mm.message_id JOIN conversations c ON c.id = m.conversation_id"
            ),
            source_sql,
        )
    } else {
        (
            message_metrics_sql,
            normalized_analytics_source_id_sql_expr("mm.source_id"),
        )
    }
}

fn token_usage_from_sql_agent_and_source_sql(
    conn: &Connection,
) -> (String, Option<String>, String) {
    let has_agent_id = table_has_column(conn, "token_usage", "agent_id");
    let has_agents = table_exists(conn, "agents") && has_agent_id;
    let has_conversation_id = table_has_column(conn, "token_usage", "conversation_id");
    let has_conversations = table_exists(conn, "conversations") && has_conversation_id;
    let has_message_id = table_has_column(conn, "token_usage", "message_id");

    let mut from_sql = if has_message_id {
        let mut select_items = vec!["message_id".to_string()];
        for column in [
            "conversation_id",
            "agent_id",
            "workspace_id",
            "source_id",
            "timestamp_ms",
            "day_id",
            "model_name",
            "model_family",
            "total_tokens",
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_creation_tokens",
            "thinking_tokens",
            "estimated_cost_usd",
            "role",
            "content_chars",
            "tool_call_count",
        ] {
            if table_has_column(conn, "token_usage", column) {
                select_items.push(format!("MAX({column}) AS {column}"));
            }
        }
        if table_has_column(conn, "token_usage", "data_source") {
            select_items.push(
                "CASE
                    WHEN MAX(CASE WHEN LOWER(TRIM(COALESCE(data_source, ''))) = 'api' THEN 1 ELSE 0 END) != 0 THEN 'api'
                    WHEN MAX(CASE WHEN LOWER(TRIM(COALESCE(data_source, ''))) = 'estimated' THEN 1 ELSE 0 END) != 0 THEN 'estimated'
                    ELSE NULL
                 END AS data_source"
                    .to_string(),
            );
        }
        format!(
            "(SELECT {} FROM token_usage GROUP BY message_id) tu",
            select_items.join(", ")
        )
    } else {
        "token_usage tu".to_string()
    };
    if has_agents {
        from_sql.push_str(" LEFT JOIN agents a ON a.id = tu.agent_id");
    }

    let source_sql = if has_conversations {
        from_sql.push_str(" LEFT JOIN conversations c ON c.id = tu.conversation_id");
        if table_has_column(conn, "conversations", "origin_host") {
            normalized_analytics_source_identity_with_fallback_sql_expr(
                "tu.source_id",
                "c.source_id",
                "c.origin_host",
            )
        } else {
            normalized_analytics_source_id_with_fallback_sql_expr("tu.source_id", "c.source_id")
        }
    } else {
        normalized_analytics_source_id_sql_expr("tu.source_id")
    };

    (
        from_sql,
        has_agents.then(|| normalized_analytics_agent_sql_expr("a.slug")),
        source_sql,
    )
}
fn token_usage_time_sql(conn: &Connection) -> Option<String> {
    let has_timestamp_ms = table_has_column(conn, "token_usage", "timestamp_ms");
    let has_conversations = table_exists(conn, "conversations");
    let has_conversation_join =
        has_conversations && table_has_column(conn, "token_usage", "conversation_id");
    let has_conversation_started_at =
        has_conversation_join && table_has_column(conn, "conversations", "started_at");

    let mut timestamp_terms: Vec<&str> = Vec::new();
    if has_timestamp_ms {
        timestamp_terms.push("tu.timestamp_ms");
    }
    if has_conversation_started_at {
        timestamp_terms.push("c.started_at");
    }

    if timestamp_terms.is_empty() {
        None
    } else {
        Some(format!("COALESCE({}, 0)", timestamp_terms.join(", ")))
    }
}

#[allow(clippy::too_many_arguments)]
fn query_table_stats_from_source<'a>(
    conn: &Connection,
    required_table: &str,
    from_sql: &str,
    bucket_col: &str,
    updated_col: Option<&str>,
    filter: &'a AnalyticsFilter,
    workspace_column: Option<&'a str>,
    agent_column_sql: Option<String>,
    source_column_sql: String,
    time_column: Option<AnalyticsTimeColumn<'a>>,
) -> RollupStats {
    if !table_exists(conn, required_table) {
        return RollupStats::default();
    }

    let (where_sql, params) = build_filtered_where_sql(
        filter,
        workspace_column,
        agent_column_sql,
        source_column_sql,
        time_column,
    );
    let updated_expr = updated_col.map_or_else(|| "NULL".to_string(), |uc| format!("MAX({uc})"));
    let sql = format!(
        "SELECT COUNT(*), MIN({bucket_col}), MAX({bucket_col}), {updated_expr} FROM {from_sql}{where_sql}"
    );

    conn.query_row_map(&sql, &params, |row: &Row| {
        Ok(RollupStats {
            row_count: row.get_typed::<i64>(0).unwrap_or(0),
            min_day: row.get_typed::<Option<i64>>(1).unwrap_or(None),
            max_day: row.get_typed::<Option<i64>>(2).unwrap_or(None),
            last_updated: row.get_typed::<Option<i64>>(3).unwrap_or(None),
        })
    })
    .unwrap_or_default()
}

fn query_scalar_i64(conn: &Connection, sql: &str, params: &[ParamValue]) -> i64 {
    conn.query_row_map(sql, params, |row: &Row| row.get_typed(0))
        .unwrap_or(0)
}

fn query_total_messages_filtered(conn: &Connection, filter: &AnalyticsFilter) -> i64 {
    if !table_exists(conn, "messages") || !table_exists(conn, "conversations") {
        return 0;
    }

    let has_agents = table_exists(conn, "agents");
    let canonical_message_metrics_sql = canonical_message_metrics_from_sql(conn);
    let has_message_metrics_created_at = canonical_message_metrics_sql.is_some()
        && table_has_column(conn, "message_metrics", "created_at_ms");
    let mut from_sql = if has_agents {
        "messages m JOIN conversations c ON c.id = m.conversation_id LEFT JOIN agents a ON a.id = c.agent_id"
            .to_string()
    } else {
        "messages m JOIN conversations c ON c.id = m.conversation_id".to_string()
    };
    if let Some(message_metrics_sql) = &canonical_message_metrics_sql {
        from_sql.push_str(" LEFT JOIN ");
        from_sql.push_str(message_metrics_sql);
        from_sql.push_str(" ON mm.message_id = m.id");
    }
    let source_sql = if table_has_column(conn, "conversations", "origin_host") {
        normalized_analytics_source_identity_sql_expr("c.source_id", "c.origin_host")
    } else {
        normalized_analytics_source_id_sql_expr("c.source_id")
    };
    let message_time_sql = if has_message_metrics_created_at {
        "COALESCE(m.created_at, mm.created_at_ms, c.started_at, 0)"
    } else {
        "COALESCE(m.created_at, c.started_at, 0)"
    };
    let (where_sql, params) = build_filtered_where_sql(
        filter,
        Some("c.workspace_id"),
        has_agents.then(|| normalized_analytics_agent_sql_expr("a.slug")),
        source_sql,
        Some(AnalyticsTimeColumn::TimestampMs(message_time_sql)),
    );

    query_scalar_i64(
        conn,
        &format!("SELECT COUNT(*) FROM {from_sql}{where_sql}"),
        &params,
    )
}

fn query_message_metrics_filtered_count(
    conn: &Connection,
    filter: &AnalyticsFilter,
    extra_condition: Option<&str>,
) -> i64 {
    if !table_exists(conn, "message_metrics") {
        return 0;
    }

    let (from_sql, source_sql) = message_metrics_from_sql_and_source_sql(conn);
    let message_metrics_time_sql = message_metrics_time_sql(conn);
    let time_column = message_metrics_time_sql
        .as_deref()
        .map(AnalyticsTimeColumn::TimestampMs)
        .unwrap_or(AnalyticsTimeColumn::Day("mm.day_id"));
    let (where_sql, params) = build_filtered_where_sql(
        filter,
        Some("mm.workspace_id"),
        Some(normalized_analytics_agent_sql_expr("mm.agent_slug")),
        source_sql,
        Some(time_column),
    );
    let sql = match extra_condition {
        Some(extra) if where_sql.is_empty() => {
            format!("SELECT COUNT(*) FROM {from_sql} WHERE {extra}")
        }
        Some(extra) => format!("SELECT COUNT(*) FROM {from_sql}{where_sql} AND {extra}"),
        None => format!("SELECT COUNT(*) FROM {from_sql}{where_sql}"),
    };

    query_scalar_i64(conn, &sql, &params)
}

fn query_token_usage_filtered_count(
    conn: &Connection,
    filter: &AnalyticsFilter,
    extra_condition: Option<&str>,
) -> i64 {
    if !table_exists(conn, "token_usage") {
        return 0;
    }

    let (from_sql, agent_sql, source_sql) = token_usage_from_sql_agent_and_source_sql(conn);
    let token_usage_time_sql = token_usage_time_sql(conn);
    let time_column = token_usage_time_sql
        .as_deref()
        .map(AnalyticsTimeColumn::TimestampMs)
        .unwrap_or(AnalyticsTimeColumn::Day("tu.day_id"));
    let (where_sql, params) = build_filtered_where_sql(
        filter,
        Some("tu.workspace_id"),
        agent_sql,
        source_sql,
        Some(time_column),
    );
    let sql = match extra_condition {
        Some(extra) if where_sql.is_empty() => {
            format!("SELECT COUNT(*) FROM {from_sql} WHERE {extra}")
        }
        Some(extra) => format!("SELECT COUNT(*) FROM {from_sql}{where_sql} AND {extra}"),
        None => format!("SELECT COUNT(*) FROM {from_sql}{where_sql}"),
    };

    query_scalar_i64(conn, &sql, &params)
}

fn track_b_requires_token_usage_fallback(filter: &AnalyticsFilter) -> bool {
    !filter.workspace_ids.is_empty() || !matches!(filter.source, SourceFilter::All)
}

fn token_usage_supports_track_b_metric(conn: &Connection, metric: Metric) -> bool {
    if !table_exists(conn, "token_usage") {
        return false;
    }

    match metric {
        Metric::ApiTotal => table_has_column(conn, "token_usage", "total_tokens"),
        Metric::ApiInput => table_has_column(conn, "token_usage", "input_tokens"),
        Metric::ApiOutput => table_has_column(conn, "token_usage", "output_tokens"),
        Metric::CacheRead => table_has_column(conn, "token_usage", "cache_read_tokens"),
        Metric::CacheCreation => table_has_column(conn, "token_usage", "cache_creation_tokens"),
        Metric::Thinking => table_has_column(conn, "token_usage", "thinking_tokens"),
        Metric::ContentEstTotal => table_has_column(conn, "token_usage", "content_chars"),
        Metric::ToolCalls => table_has_column(conn, "token_usage", "tool_call_count"),
        Metric::EstimatedCostUsd => table_has_column(conn, "token_usage", "estimated_cost_usd"),
        Metric::PlanCount | Metric::CoveragePct | Metric::MessageCount => true,
    }
}

fn message_metrics_supports_track_a_metric(conn: &Connection, metric: Metric) -> bool {
    if !table_exists(conn, "message_metrics")
        || !table_has_column(conn, "message_metrics", "message_id")
    {
        return false;
    }

    match metric {
        Metric::ApiTotal => {
            table_has_column(conn, "message_metrics", "api_input_tokens")
                && table_has_column(conn, "message_metrics", "api_output_tokens")
                && table_has_column(conn, "message_metrics", "api_cache_read_tokens")
                && table_has_column(conn, "message_metrics", "api_cache_creation_tokens")
                && table_has_column(conn, "message_metrics", "api_thinking_tokens")
        }
        Metric::ApiInput => table_has_column(conn, "message_metrics", "api_input_tokens"),
        Metric::ApiOutput => table_has_column(conn, "message_metrics", "api_output_tokens"),
        Metric::CacheRead => table_has_column(conn, "message_metrics", "api_cache_read_tokens"),
        Metric::CacheCreation => {
            table_has_column(conn, "message_metrics", "api_cache_creation_tokens")
        }
        Metric::Thinking => table_has_column(conn, "message_metrics", "api_thinking_tokens"),
        Metric::ContentEstTotal => table_has_column(conn, "message_metrics", "content_tokens_est"),
        Metric::CoveragePct => table_has_column(conn, "message_metrics", "api_data_source"),
        Metric::MessageCount => true,
        Metric::ToolCalls => table_has_column(conn, "message_metrics", "tool_call_count"),
        Metric::PlanCount => table_has_column(conn, "message_metrics", "has_plan"),
        Metric::EstimatedCostUsd => false,
    }
}

fn track_a_breakdown_supports_raw_metric(conn: &Connection, metric: Metric) -> bool {
    if !table_exists(conn, "messages") || !table_exists(conn, "conversations") {
        return false;
    }

    match metric {
        Metric::MessageCount => true,
        Metric::EstimatedCostUsd => false,
        _ => message_metrics_supports_track_a_metric(conn, metric),
    }
}

fn track_a_breakdown_requires_raw_fallback(filter: &AnalyticsFilter, dim: Dim) -> bool {
    matches!(dim, Dim::Source)
        || (matches!(dim, Dim::Agent | Dim::Workspace)
            && (track_a_timeseries_requires_source_fallback(filter)
                || analytics_requires_exact_raw_time_filter(filter)))
}

fn track_b_breakdown_requires_token_usage_fallback(filter: &AnalyticsFilter, dim: Dim) -> bool {
    matches!(dim, Dim::Source)
        || track_b_requires_token_usage_fallback(filter)
        || analytics_requires_exact_raw_time_filter(filter)
}

fn track_a_timeseries_requires_source_fallback(filter: &AnalyticsFilter) -> bool {
    match &filter.source {
        SourceFilter::Remote => true,
        SourceFilter::Specific(source_id) => {
            normalized_analytics_source_id_value(source_id.as_str())
                != crate::sources::provenance::LOCAL_SOURCE_ID
        }
        SourceFilter::All | SourceFilter::Local => false,
    }
}

fn track_a_tools_supports_raw_source_fallback(conn: &Connection) -> bool {
    table_exists(conn, "messages")
        && table_exists(conn, "conversations")
        && table_exists(conn, "message_metrics")
        && table_has_column(conn, "message_metrics", "message_id")
        && table_has_column(conn, "message_metrics", "tool_call_count")
}

fn analytics_requires_exact_raw_time_filter(filter: &AnalyticsFilter) -> bool {
    filter.since_ms.is_some() || filter.until_ms.is_some()
}

fn track_a_timeseries_requires_raw_fallback(filter: &AnalyticsFilter) -> bool {
    track_a_timeseries_requires_source_fallback(filter)
        || analytics_requires_exact_raw_time_filter(filter)
}

fn track_b_cost_timeseries_requires_token_usage_fallback(
    filter: &AnalyticsFilter,
    group_by: GroupBy,
) -> bool {
    matches!(group_by, GroupBy::Hour)
        || track_b_requires_token_usage_fallback(filter)
        || analytics_requires_exact_raw_time_filter(filter)
}

fn query_track_a_rollup_status_with_message_metrics_fallback(
    conn: &Connection,
    table: &str,
    bucket_col: &str,
    filter: &AnalyticsFilter,
) -> RollupStats {
    if !table_exists(conn, table) {
        return RollupStats::default();
    }

    let default_stats = || {
        query_table_stats_from_source(
            conn,
            table,
            table,
            bucket_col,
            Some("last_updated"),
            filter,
            Some("workspace_id"),
            Some(normalized_analytics_agent_sql_expr("agent_slug")),
            normalized_analytics_source_id_sql_expr("source_id"),
            Some(match bucket_col {
                "hour_id" => AnalyticsTimeColumn::Hour("hour_id"),
                "day_id" => AnalyticsTimeColumn::Day("day_id"),
                _ => return RollupStats::default(),
            }),
        )
    };

    if !(track_a_timeseries_requires_source_fallback(filter)
        || analytics_requires_exact_raw_time_filter(filter))
        || !table_exists(conn, "message_metrics")
        || !table_exists(conn, "messages")
        || !table_exists(conn, "conversations")
        || !table_has_column(conn, "message_metrics", "message_id")
        || !table_has_column(conn, "message_metrics", bucket_col)
    {
        return default_stats();
    }

    let (message_metrics_from_sql, message_metrics_source_sql) =
        message_metrics_from_sql_and_source_sql(conn);
    let message_metrics_bucket_col = match bucket_col {
        "hour_id" => "mm.hour_id",
        "day_id" => "mm.day_id",
        _ => return default_stats(),
    };
    let message_metrics_agent_sql = normalized_analytics_agent_sql_expr("mm.agent_slug");
    let message_metrics_time_sql = message_metrics_time_sql(conn);
    let (where_sql, params) = build_filtered_where_sql(
        filter,
        Some("mm.workspace_id"),
        Some(message_metrics_agent_sql.clone()),
        message_metrics_source_sql.clone(),
        Some(
            message_metrics_time_sql
                .as_deref()
                .map(AnalyticsTimeColumn::TimestampMs)
                .unwrap_or(match bucket_col {
                    "hour_id" => AnalyticsTimeColumn::Hour("mm.hour_id"),
                    "day_id" => AnalyticsTimeColumn::Day("mm.day_id"),
                    _ => return default_stats(),
                }),
        ),
    );
    let rollup_agent_sql = normalized_analytics_agent_sql_expr("rollup.agent_slug");
    let rollup_source_sql = normalized_analytics_source_id_with_fallback_sql_expr(
        "rollup.source_id",
        "filtered_keys.source_id",
    );
    let sql = format!(
        "SELECT COUNT(*), MIN(bucket_id), MAX(bucket_id), MAX(last_updated)\n         FROM (\n             SELECT DISTINCT\n                    rollup.{bucket_col} AS bucket_id,\n                    rollup.last_updated AS last_updated\n               FROM {table} rollup\n               JOIN (\n                  SELECT {message_metrics_bucket_col} AS bucket_id,\n                         {message_metrics_agent_sql} AS agent_slug,\n                         mm.workspace_id AS workspace_id,\n                         {message_metrics_source_sql} AS source_id\n                    FROM {message_metrics_from_sql}\n                  {where_sql}\n                   GROUP BY {message_metrics_bucket_col},\n                            {message_metrics_agent_sql},\n                            mm.workspace_id,\n                            {message_metrics_source_sql}\n              ) filtered_keys\n                 ON rollup.{bucket_col} = filtered_keys.bucket_id\n                AND {rollup_agent_sql} = filtered_keys.agent_slug\n                AND rollup.workspace_id = filtered_keys.workspace_id\n                AND {rollup_source_sql} = filtered_keys.source_id\n         ) matched_track_a_rollups"
    );

    conn.query_row_map(&sql, &params, |row: &Row| {
        Ok(RollupStats {
            row_count: row.get_typed::<i64>(0).unwrap_or(0),
            min_day: row.get_typed::<Option<i64>>(1).unwrap_or(None),
            max_day: row.get_typed::<Option<i64>>(2).unwrap_or(None),
            last_updated: row.get_typed::<Option<i64>>(3).unwrap_or(None),
        })
    })
    .unwrap_or_default()
}

fn query_token_daily_stats_status(conn: &Connection, filter: &AnalyticsFilter) -> RollupStats {
    if !table_exists(conn, "token_daily_stats") {
        return RollupStats::default();
    }

    if !(track_b_requires_token_usage_fallback(filter)
        || analytics_requires_exact_raw_time_filter(filter))
        || !table_exists(conn, "token_usage")
    {
        return query_table_stats_from_source(
            conn,
            "token_daily_stats",
            "token_daily_stats",
            "day_id",
            Some("last_updated"),
            filter,
            None,
            Some(normalized_analytics_agent_sql_expr("agent_slug")),
            normalized_analytics_source_id_sql_expr("source_id"),
            Some(AnalyticsTimeColumn::Day("day_id")),
        );
    }

    let (token_usage_from_sql, token_usage_agent_sql, token_usage_source_sql) =
        token_usage_from_sql_agent_and_source_sql(conn);
    let token_usage_agent_sql = token_usage_agent_sql.unwrap_or_else(|| "'unknown'".to_string());
    let token_usage_model_sql = normalized_analytics_model_family_sql_expr("tu.model_family");
    let token_usage_time_sql = token_usage_time_sql(conn);
    let (where_sql, params) = build_filtered_where_sql(
        filter,
        Some("tu.workspace_id"),
        Some(token_usage_agent_sql.clone()),
        token_usage_source_sql.clone(),
        Some(
            token_usage_time_sql
                .as_deref()
                .map(AnalyticsTimeColumn::TimestampMs)
                .unwrap_or(AnalyticsTimeColumn::Day("tu.day_id")),
        ),
    );
    let tds_agent_sql = normalized_analytics_agent_sql_expr("tds.agent_slug");
    let tds_source_sql = normalized_analytics_source_id_with_fallback_sql_expr(
        "tds.source_id",
        "filtered_keys.source_id",
    );
    let tds_model_sql = normalized_analytics_model_family_sql_expr("tds.model_family");
    let sql = format!(
        "SELECT COUNT(*), MIN(day_id), MAX(day_id), MAX(last_updated)          FROM (             SELECT DISTINCT                    tds.day_id AS day_id,                    tds.agent_slug AS agent_slug,                    tds.source_id AS source_id,                    tds.model_family AS model_family,                    tds.last_updated AS last_updated               FROM token_daily_stats tds               JOIN (                  SELECT tu.day_id AS day_id,                         {token_usage_agent_sql} AS agent_slug,                         {token_usage_source_sql} AS source_id,                         {token_usage_model_sql} AS model_family                   FROM {token_usage_from_sql}                  {where_sql}                   GROUP BY tu.day_id, {token_usage_agent_sql}, {token_usage_source_sql}, {token_usage_model_sql}              ) filtered_keys                 ON tds.day_id = filtered_keys.day_id                AND {tds_agent_sql} = filtered_keys.agent_slug                AND {tds_source_sql} = filtered_keys.source_id                AND {tds_model_sql} = filtered_keys.model_family         ) matched_token_daily_stats"
    );

    conn.query_row_map(&sql, &params, |row: &Row| {
        Ok(RollupStats {
            row_count: row.get_typed::<i64>(0).unwrap_or(0),
            min_day: row.get_typed::<Option<i64>>(1).unwrap_or(None),
            max_day: row.get_typed::<Option<i64>>(2).unwrap_or(None),
            last_updated: row.get_typed::<Option<i64>>(3).unwrap_or(None),
        })
    })
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// query_status
// ---------------------------------------------------------------------------

/// Run the analytics status query — returns table health, coverage, and drift.
pub fn query_status(conn: &Connection, filter: &AnalyticsFilter) -> AnalyticsResult<StatusResult> {
    let has_message_metrics = table_exists(conn, "message_metrics");
    let has_usage_hourly = table_exists(conn, "usage_hourly");
    let has_usage_daily = table_exists(conn, "usage_daily");
    let has_token_usage = table_exists(conn, "token_usage");
    let has_token_daily_stats = table_exists(conn, "token_daily_stats");

    let (message_metrics_from_sql, message_metrics_source_sql) =
        message_metrics_from_sql_and_source_sql(conn);
    let message_metrics_time_sql = message_metrics_time_sql(conn);
    let mm = query_table_stats_from_source(
        conn,
        "message_metrics",
        &message_metrics_from_sql,
        "mm.day_id",
        None,
        filter,
        Some("mm.workspace_id"),
        Some(normalized_analytics_agent_sql_expr("mm.agent_slug")),
        message_metrics_source_sql,
        Some(
            message_metrics_time_sql
                .as_deref()
                .map(AnalyticsTimeColumn::TimestampMs)
                .unwrap_or(AnalyticsTimeColumn::Day("mm.day_id")),
        ),
    );
    let uh = query_track_a_rollup_status_with_message_metrics_fallback(
        conn,
        "usage_hourly",
        "hour_id",
        filter,
    );
    let ud = query_track_a_rollup_status_with_message_metrics_fallback(
        conn,
        "usage_daily",
        "day_id",
        filter,
    );
    let (token_usage_from_sql, token_usage_agent_sql, token_usage_source_sql) =
        token_usage_from_sql_agent_and_source_sql(conn);
    let token_usage_time_sql = token_usage_time_sql(conn);
    let tu = query_table_stats_from_source(
        conn,
        "token_usage",
        &token_usage_from_sql,
        "tu.day_id",
        None,
        filter,
        Some("tu.workspace_id"),
        token_usage_agent_sql,
        token_usage_source_sql,
        Some(
            token_usage_time_sql
                .as_deref()
                .map(AnalyticsTimeColumn::TimestampMs)
                .unwrap_or(AnalyticsTimeColumn::Day("tu.day_id")),
        ),
    );
    let tds = query_token_daily_stats_status(conn, filter);

    let total_messages = query_total_messages_filtered(conn, filter);

    let api_coverage_pct = if has_message_metrics && mm.row_count > 0 {
        let api_count =
            query_message_metrics_filtered_count(conn, filter, Some("api_data_source = 'api'"));
        (api_count as f64 / mm.row_count as f64) * 100.0
    } else {
        0.0
    };

    let model_coverage_pct = if has_token_usage && tu.row_count > 0 {
        let with_model = query_token_usage_filtered_count(
            conn,
            filter,
            Some("model_name IS NOT NULL AND TRIM(model_name) != ''"),
        );
        (with_model as f64 / tu.row_count as f64) * 100.0
    } else {
        0.0
    };

    let estimate_only_pct = if has_token_usage && tu.row_count > 0 {
        let estimates =
            query_token_usage_filtered_count(conn, filter, Some("data_source = 'estimated'"));
        (estimates as f64 / tu.row_count as f64) * 100.0
    } else {
        0.0
    };

    let mm_coverage_pct = if total_messages > 0 {
        (mm.row_count as f64 / total_messages as f64) * 100.0
    } else {
        0.0
    };

    let mut drift_signals: Vec<DriftSignal> = Vec::new();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let stale_threshold_ms: i64 = 86_400_000;

    let track_a_fresh = is_recently_updated(uh.last_updated, now_ms, stale_threshold_ms);
    let track_b_fresh = is_recently_updated(tds.last_updated, now_ms, stale_threshold_ms);

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

    if track_a_timeseries_requires_raw_fallback(filter)
        && table_exists(conn, "messages")
        && table_exists(conn, "conversations")
    {
        return query_track_a_timeseries_from_raw(conn, filter, group_by, query_start);
    }

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

    let (dim_parts, dim_params) = build_where_parts(filter, Some("workspace_id"));
    let mut where_parts = dim_parts;
    let mut bind_values = dim_params;

    match group_by {
        GroupBy::Hour => {
            if let Some(min) = hour_min {
                bind_values.push(ParamValue::from(min));
                where_parts.push(format!("{bucket_col} >= ?{}", bind_values.len()));
            }
            if let Some(max) = hour_max {
                bind_values.push(ParamValue::from(max));
                where_parts.push(format!("{bucket_col} <= ?{}", bind_values.len()));
            }
        }
        _ => {
            if let Some(min) = day_min {
                bind_values.push(ParamValue::from(min));
                where_parts.push(format!("{bucket_col} >= ?{}", bind_values.len()));
            }
            if let Some(max) = day_max {
                bind_values.push(ParamValue::from(max));
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
         FROM (
             SELECT * FROM {table}
             {where_clause}
         ) filtered_tokens_timeseries
         GROUP BY {bucket_col}
         ORDER BY {bucket_col}"
    );

    let param_values: Vec<ParamValue> = bind_values.clone();

    let raw_buckets: Vec<(i64, UsageBucket)> = conn
        .query_map_collect(&sql, &param_values, |row: &Row| {
            Ok((
                row.get_typed::<i64>(0)?,
                UsageBucket {
                    message_count: row.get_typed(1)?,
                    user_message_count: row.get_typed(2)?,
                    assistant_message_count: row.get_typed(3)?,
                    tool_call_count: row.get_typed(4)?,
                    plan_message_count: row.get_typed(5)?,
                    plan_content_tokens_est_total: row.get_typed(6)?,
                    plan_api_tokens_total: row.get_typed(7)?,
                    api_coverage_message_count: row.get_typed(8)?,
                    content_tokens_est_total: row.get_typed(9)?,
                    content_tokens_est_user: row.get_typed(10)?,
                    content_tokens_est_assistant: row.get_typed(11)?,
                    api_tokens_total: row.get_typed(12)?,
                    api_input_tokens_total: row.get_typed(13)?,
                    api_output_tokens_total: row.get_typed(14)?,
                    api_cache_read_tokens_total: row.get_typed(15)?,
                    api_cache_creation_tokens_total: row.get_typed(16)?,
                    api_thinking_tokens_total: row.get_typed(17)?,
                    ..Default::default()
                },
            ))
        })
        .map_err(|e| analytics_query_error("Analytics query failed", e))?;

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

fn query_track_a_timeseries_from_raw(
    conn: &Connection,
    filter: &AnalyticsFilter,
    group_by: GroupBy,
    query_start: std::time::Instant,
) -> AnalyticsResult<TimeseriesResult> {
    let has_agents = table_exists(conn, "agents");
    let has_origin_host = table_has_column(conn, "conversations", "origin_host");
    let canonical_message_metrics_sql = canonical_message_metrics_from_sql(conn);
    let join_message_metrics = canonical_message_metrics_sql.is_some();
    let has_message_metrics_created_at =
        join_message_metrics && table_has_column(conn, "message_metrics", "created_at_ms");
    let has_content_tokens_est =
        join_message_metrics && table_has_column(conn, "message_metrics", "content_tokens_est");
    let has_api_input_tokens =
        join_message_metrics && table_has_column(conn, "message_metrics", "api_input_tokens");
    let has_api_output_tokens =
        join_message_metrics && table_has_column(conn, "message_metrics", "api_output_tokens");
    let has_api_cache_read_tokens =
        join_message_metrics && table_has_column(conn, "message_metrics", "api_cache_read_tokens");
    let has_api_cache_creation_tokens = join_message_metrics
        && table_has_column(conn, "message_metrics", "api_cache_creation_tokens");
    let has_api_thinking_tokens =
        join_message_metrics && table_has_column(conn, "message_metrics", "api_thinking_tokens");
    let has_api_data_source =
        join_message_metrics && table_has_column(conn, "message_metrics", "api_data_source");
    let has_tool_call_count =
        join_message_metrics && table_has_column(conn, "message_metrics", "tool_call_count");
    let has_has_plan =
        join_message_metrics && table_has_column(conn, "message_metrics", "has_plan");

    let conversation_sql = if has_origin_host {
        "SELECT id, TRIM(COALESCE(source_id, '')), TRIM(COALESCE(origin_host, '')) FROM conversations"
    } else {
        "SELECT id, TRIM(COALESCE(source_id, '')), '' FROM conversations"
    };
    let conversation_sources: BTreeMap<i64, (String, String)> = conn
        .query_map_collect(conversation_sql, &[], |row: &Row| {
            Ok((
                row.get_typed::<i64>(0)?,
                (row.get_typed::<String>(1)?, row.get_typed::<String>(2)?),
            ))
        })
        .map_err(|e| analytics_query_error("Analytics query failed", e))?
        .into_iter()
        .collect();

    let mut from_sql = String::from("messages m JOIN conversations c ON c.id = m.conversation_id");
    if has_agents {
        from_sql.push_str(" LEFT JOIN agents a ON a.id = c.agent_id");
    }
    if let Some(message_metrics_sql) = &canonical_message_metrics_sql {
        from_sql.push_str(" LEFT JOIN ");
        from_sql.push_str(message_metrics_sql);
        from_sql.push_str(" ON mm.message_id = m.id");
    }

    let filter_for_sql = AnalyticsFilter {
        source: SourceFilter::All,
        ..filter.clone()
    };
    let message_time_sql = if has_message_metrics_created_at {
        if join_message_metrics {
            "COALESCE(m.created_at, mm.created_at_ms, c.started_at, 0)"
        } else {
            "COALESCE(m.created_at, (SELECT MAX(message_metrics.created_at_ms) FROM message_metrics WHERE message_metrics.message_id = m.id), c.started_at, 0)"
        }
    } else {
        "COALESCE(m.created_at, c.started_at, 0)"
    };
    let (where_sql, params) = build_filtered_where_sql(
        &filter_for_sql,
        Some("c.workspace_id"),
        has_agents.then(|| normalized_analytics_agent_sql_expr("a.slug")),
        sql_string_literal("all"),
        Some(AnalyticsTimeColumn::TimestampMs(message_time_sql)),
    );

    let content_tokens_expr = if has_content_tokens_est {
        "COALESCE(mm.content_tokens_est, 0)"
    } else {
        "0"
    };
    let api_input_expr = if has_api_input_tokens {
        "COALESCE(mm.api_input_tokens, 0)"
    } else {
        "0"
    };
    let api_output_expr = if has_api_output_tokens {
        "COALESCE(mm.api_output_tokens, 0)"
    } else {
        "0"
    };
    let api_cache_read_expr = if has_api_cache_read_tokens {
        "COALESCE(mm.api_cache_read_tokens, 0)"
    } else {
        "0"
    };
    let api_cache_creation_expr = if has_api_cache_creation_tokens {
        "COALESCE(mm.api_cache_creation_tokens, 0)"
    } else {
        "0"
    };
    let api_thinking_expr = if has_api_thinking_tokens {
        "COALESCE(mm.api_thinking_tokens, 0)"
    } else {
        "0"
    };
    let api_covered_expr = if has_api_data_source {
        "CASE WHEN mm.api_data_source = 'api' THEN 1 ELSE 0 END"
    } else {
        "0"
    };
    let tool_call_expr = if has_tool_call_count {
        "COALESCE(mm.tool_call_count, 0)"
    } else {
        "0"
    };
    let has_plan_expr = if has_has_plan {
        "CASE WHEN COALESCE(mm.has_plan, 0) != 0 THEN 1 ELSE 0 END"
    } else {
        "0"
    };

    let sql = format!(
        "SELECT m.conversation_id,
                m.role,
                {message_time_sql},
                {content_tokens_expr},
                {api_input_expr},
                {api_output_expr},
                {api_cache_read_expr},
                {api_cache_creation_expr},
                {api_thinking_expr},
                {api_covered_expr},
                {tool_call_expr},
                {has_plan_expr}
         FROM {from_sql}{where_sql}"
    );

    let row_buckets: Vec<(i64, i64, UsageBucket)> = conn
        .query_map_collect(&sql, &params, |row: &Row| {
            let conversation_id: i64 = row.get_typed(0)?;
            let role: String = row.get_typed(1)?;
            let created_at_ms: i64 = row.get_typed(2)?;
            let content_tokens_est: i64 = row.get_typed(3)?;
            let api_input_tokens: i64 = row.get_typed(4)?;
            let api_output_tokens: i64 = row.get_typed(5)?;
            let api_cache_read_tokens: i64 = row.get_typed(6)?;
            let api_cache_creation_tokens: i64 = row.get_typed(7)?;
            let api_thinking_tokens: i64 = row.get_typed(8)?;
            let api_covered: i64 = row.get_typed(9)?;
            let tool_call_count: i64 = row.get_typed(10)?;
            let has_plan: i64 = row.get_typed(11)?;
            let normalized_created_at_ms = normalize_epoch_millis(created_at_ms);
            let bucket_id = match group_by {
                GroupBy::Hour => crate::storage::sqlite::FrankenStorage::hour_id_from_millis(
                    normalized_created_at_ms,
                ),
                GroupBy::Day | GroupBy::Week | GroupBy::Month => {
                    crate::storage::sqlite::FrankenStorage::day_id_from_millis(
                        normalized_created_at_ms,
                    )
                }
            };

            let mut bucket = UsageBucket {
                message_count: 1,
                user_message_count: i64::from(role == "user"),
                assistant_message_count: i64::from(role == "assistant"),
                tool_call_count,
                plan_message_count: has_plan,
                api_coverage_message_count: api_covered,
                content_tokens_est_total: content_tokens_est,
                content_tokens_est_user: if role == "user" {
                    content_tokens_est
                } else {
                    0
                },
                content_tokens_est_assistant: if role == "assistant" {
                    content_tokens_est
                } else {
                    0
                },
                api_input_tokens_total: api_input_tokens,
                api_output_tokens_total: api_output_tokens,
                api_cache_read_tokens_total: api_cache_read_tokens,
                api_cache_creation_tokens_total: api_cache_creation_tokens,
                api_thinking_tokens_total: api_thinking_tokens,
                plan_content_tokens_est_total: if has_plan != 0 { content_tokens_est } else { 0 },
                ..Default::default()
            };
            bucket.api_tokens_total = api_input_tokens
                + api_output_tokens
                + api_cache_read_tokens
                + api_cache_creation_tokens
                + api_thinking_tokens;
            if has_plan != 0 && api_covered != 0 {
                bucket.plan_api_tokens_total = bucket.api_tokens_total;
            }

            Ok((conversation_id, bucket_id, bucket))
        })
        .map_err(|e| analytics_query_error("Analytics query failed", e))?;

    let mut grouped_buckets: BTreeMap<i64, UsageBucket> = BTreeMap::new();
    for (conversation_id, bucket_id, bucket) in row_buckets {
        let (source_id, origin_host) = conversation_sources
            .get(&conversation_id)
            .cloned()
            .unwrap_or_else(|| {
                (
                    crate::sources::provenance::LOCAL_SOURCE_ID.to_string(),
                    String::new(),
                )
            });
        let normalized_key = normalized_analytics_source_identity_value(&source_id, &origin_host);
        if !analytics_source_filter_matches_key(&filter.source, &normalized_key) {
            continue;
        }
        grouped_buckets.entry(bucket_id).or_default().merge(&bucket);
    }

    let raw_buckets: Vec<(i64, UsageBucket)> = grouped_buckets.into_iter().collect();
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

    let mut totals = UsageBucket::default();
    for (_, row) in &final_buckets {
        totals.merge(row);
    }

    Ok(TimeseriesResult {
        buckets: final_buckets,
        totals,
        source_table: if join_message_metrics {
            "message_metrics".into()
        } else {
            "messages".into()
        },
        group_by,
        elapsed_ms: query_start.elapsed().as_millis() as u64,
        path: "raw".into(),
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
fn query_cost_timeseries_from_token_usage(
    conn: &Connection,
    filter: &AnalyticsFilter,
    group_by: GroupBy,
    query_start: std::time::Instant,
) -> AnalyticsResult<TimeseriesResult> {
    if !table_exists(conn, "token_usage") {
        return Ok(TimeseriesResult {
            buckets: vec![],
            totals: UsageBucket::default(),
            source_table: "token_usage".into(),
            group_by,
            elapsed_ms: query_start.elapsed().as_millis() as u64,
            path: "none".into(),
        });
    }

    let has_input_tokens = table_has_column(conn, "token_usage", "input_tokens");
    let has_output_tokens = table_has_column(conn, "token_usage", "output_tokens");
    let has_cache_read_tokens = table_has_column(conn, "token_usage", "cache_read_tokens");
    let has_cache_creation_tokens = table_has_column(conn, "token_usage", "cache_creation_tokens");
    let has_thinking_tokens = table_has_column(conn, "token_usage", "thinking_tokens");
    let has_estimated_cost = table_has_column(conn, "token_usage", "estimated_cost_usd");
    let has_role = table_has_column(conn, "token_usage", "role");
    let has_content_chars = table_has_column(conn, "token_usage", "content_chars");
    let has_tool_call_count = table_has_column(conn, "token_usage", "tool_call_count");

    let (token_usage_from_sql, token_usage_agent_sql, token_usage_source_sql) =
        token_usage_from_sql_agent_and_source_sql(conn);
    let token_usage_time_sql = token_usage_time_sql(conn);
    let has_exact_time = token_usage_time_sql.is_some();
    let time_column = token_usage_time_sql
        .as_deref()
        .map(AnalyticsTimeColumn::TimestampMs)
        .unwrap_or(AnalyticsTimeColumn::Day("tu.day_id"));
    let (where_sql, params) = build_filtered_where_sql(
        filter,
        Some("tu.workspace_id"),
        token_usage_agent_sql,
        token_usage_source_sql,
        Some(time_column),
    );

    let sql = format!(
        "SELECT {},
                {} AS role,
                {},
                {},
                {},
                {},
                {},
                {},
                {},
                {},
                {}
         FROM {token_usage_from_sql}
         {where_sql}",
        token_usage_time_sql.as_deref().unwrap_or("tu.day_id"),
        if has_role { "tu.role" } else { "''" },
        if has_tool_call_count {
            "COALESCE(tu.tool_call_count, 0)"
        } else {
            "0"
        },
        if has_input_tokens {
            "COALESCE(tu.input_tokens, 0)"
        } else {
            "0"
        },
        if has_output_tokens {
            "COALESCE(tu.output_tokens, 0)"
        } else {
            "0"
        },
        if has_cache_read_tokens {
            "COALESCE(tu.cache_read_tokens, 0)"
        } else {
            "0"
        },
        if has_cache_creation_tokens {
            "COALESCE(tu.cache_creation_tokens, 0)"
        } else {
            "0"
        },
        if has_thinking_tokens {
            "COALESCE(tu.thinking_tokens, 0)"
        } else {
            "0"
        },
        "COALESCE(tu.total_tokens, 0)",
        if has_content_chars {
            "COALESCE(tu.content_chars, 0)"
        } else {
            "0"
        },
        if has_estimated_cost {
            "COALESCE(tu.estimated_cost_usd, 0.0)"
        } else {
            "0.0"
        },
    );

    let raw_rows: Vec<(i64, UsageBucket)> = conn
        .query_map_collect(&sql, &params, |row: &Row| {
            let raw_time_value: i64 = row.get_typed(0)?;
            let role: String = row.get_typed(1)?;
            let tool_calls: i64 = row.get_typed(2)?;
            let input_tok: i64 = row.get_typed(3)?;
            let output_tok: i64 = row.get_typed(4)?;
            let cache_read: i64 = row.get_typed(5)?;
            let cache_create: i64 = row.get_typed(6)?;
            let thinking: i64 = row.get_typed(7)?;
            let grand_total: i64 = row.get_typed(8)?;
            let content_chars: i64 = row.get_typed(9)?;
            let cost: f64 = row.get_typed(10)?;

            let bucket_id = if has_exact_time {
                let normalized_created_at_ms = normalize_epoch_millis(raw_time_value);
                match group_by {
                    GroupBy::Hour => crate::storage::sqlite::FrankenStorage::hour_id_from_millis(
                        normalized_created_at_ms,
                    ),
                    GroupBy::Day | GroupBy::Week | GroupBy::Month => {
                        crate::storage::sqlite::FrankenStorage::day_id_from_millis(
                            normalized_created_at_ms,
                        )
                    }
                }
            } else {
                raw_time_value
            };

            Ok((
                bucket_id,
                UsageBucket {
                    message_count: 1,
                    user_message_count: i64::from(role == "user"),
                    assistant_message_count: i64::from(role == "assistant"),
                    tool_call_count: tool_calls,
                    api_coverage_message_count: 1,
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
        .map_err(|e| analytics_query_error("Cost timeseries query failed", e))?;

    let mut grouped_buckets: BTreeMap<i64, UsageBucket> = BTreeMap::new();
    for (bucket_id, bucket) in raw_rows {
        grouped_buckets.entry(bucket_id).or_default().merge(&bucket);
    }

    let raw_buckets: Vec<(i64, UsageBucket)> = grouped_buckets.into_iter().collect();
    let final_buckets: Vec<(String, UsageBucket)> = match group_by {
        GroupBy::Hour if has_exact_time => raw_buckets
            .into_iter()
            .map(|(id, row)| (bucketing::hour_id_to_iso(id), row))
            .collect(),
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

    let mut totals = UsageBucket::default();
    for (_, row) in &final_buckets {
        totals.merge(row);
    }

    Ok(TimeseriesResult {
        buckets: final_buckets,
        totals,
        source_table: "token_usage".into(),
        group_by,
        elapsed_ms: query_start.elapsed().as_millis() as u64,
        path: "raw".into(),
    })
}

pub fn query_cost_timeseries(
    conn: &Connection,
    filter: &AnalyticsFilter,
    group_by: GroupBy,
) -> AnalyticsResult<TimeseriesResult> {
    let query_start = std::time::Instant::now();

    let table = "token_daily_stats";

    if track_b_cost_timeseries_requires_token_usage_fallback(filter, group_by)
        && token_usage_supports_track_b_metric(conn, Metric::ApiTotal)
        && token_usage_supports_track_b_metric(conn, Metric::EstimatedCostUsd)
    {
        return query_cost_timeseries_from_token_usage(conn, filter, group_by, query_start);
    }

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
    let (dim_parts, dim_params) = build_where_parts(filter, None);
    let mut where_parts = dim_parts;
    let mut bind_values = dim_params;

    if let Some(min) = day_min {
        bind_values.push(ParamValue::from(min));
        where_parts.push(format!("day_id >= ?{}", bind_values.len()));
    }
    if let Some(max) = day_max {
        bind_values.push(ParamValue::from(max));
        where_parts.push(format!("day_id <= ?{}", bind_values.len()));
    }

    // Exclude pre-aggregated "all" permutation rows from token_daily_stats
    // to avoid double/multi-counting. The SUM in the query does aggregation.
    if table == "token_daily_stats" {
        where_parts.push("model_family != 'all'".into());
        where_parts.push("agent_slug != 'all'".into());
        where_parts.push("LOWER(TRIM(COALESCE(source_id, ''))) != 'all'".into());
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
         FROM (
             SELECT * FROM {table}
             {where_clause}
         ) filtered_cost_timeseries
         GROUP BY day_id
         ORDER BY day_id"
    );

    let param_values: Vec<ParamValue> = bind_values.clone();

    let raw_buckets: Vec<(i64, UsageBucket)> = conn
        .query_map_collect(&sql, &param_values, |row: &Row| {
            let day_id: i64 = row.get_typed(0)?;
            let api_call_count: i64 = row.get_typed(1)?;
            let user_msg: i64 = row.get_typed(2)?;
            let asst_msg: i64 = row.get_typed(3)?;
            let tool_calls: i64 = row.get_typed(4)?;
            let input_tok: i64 = row.get_typed(5)?;
            let output_tok: i64 = row.get_typed(6)?;
            let cache_read: i64 = row.get_typed(7)?;
            let cache_create: i64 = row.get_typed(8)?;
            let thinking: i64 = row.get_typed(9)?;
            let grand_total: i64 = row.get_typed(10)?;
            let content_chars: i64 = row.get_typed(11)?;
            let cost: f64 = row.get_typed(12)?;

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
        .map_err(|e| analytics_query_error("Cost timeseries query failed", e))?;

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

fn breakdown_route(dim: Dim, metric: Metric) -> (&'static str, &'static str, bool) {
    match (dim, metric) {
        (Dim::Model, _) => ("token_daily_stats", "model_family", true),
        (Dim::Agent, Metric::EstimatedCostUsd) => ("token_daily_stats", "agent_slug", true),
        (Dim::Source, Metric::EstimatedCostUsd) => ("token_daily_stats", "source_id", true),
        (Dim::Agent, _) => ("usage_daily", "agent_slug", false),
        (Dim::Workspace, _) => ("usage_daily", "workspace_id", false),
        (Dim::Source, _) => ("usage_daily", "source_id", false),
    }
}

fn query_track_b_breakdown_from_token_usage(
    conn: &Connection,
    filter: &AnalyticsFilter,
    dim: Dim,
    metric: Metric,
    limit: usize,
    query_start: std::time::Instant,
) -> AnalyticsResult<BreakdownResult> {
    if !table_exists(conn, "token_usage") {
        return Ok(BreakdownResult {
            rows: vec![],
            dim,
            metric,
            source_table: "token_usage".into(),
            elapsed_ms: query_start.elapsed().as_millis() as u64,
        });
    }

    let has_input_tokens = table_has_column(conn, "token_usage", "input_tokens");
    let has_output_tokens = table_has_column(conn, "token_usage", "output_tokens");
    let has_cache_read_tokens = table_has_column(conn, "token_usage", "cache_read_tokens");
    let has_cache_creation_tokens = table_has_column(conn, "token_usage", "cache_creation_tokens");
    let has_thinking_tokens = table_has_column(conn, "token_usage", "thinking_tokens");
    let has_estimated_cost = table_has_column(conn, "token_usage", "estimated_cost_usd");
    let has_role = table_has_column(conn, "token_usage", "role");
    let has_content_chars = table_has_column(conn, "token_usage", "content_chars");
    let has_tool_call_count = table_has_column(conn, "token_usage", "tool_call_count");

    let sum_or_zero = |expr: &str, present: bool| {
        if present {
            format!("SUM(COALESCE({expr}, 0))")
        } else {
            // Must be SUM(0), not bare 0: these expressions feed into ORDER BY
            // where a bare integer literal is treated as a column position.
            "SUM(0)".to_string()
        }
    };
    let count_role = |role: &str| {
        if has_role {
            format!("SUM(CASE WHEN tu.role = '{}' THEN 1 ELSE 0 END)", role)
        } else {
            "SUM(0)".to_string()
        }
    };

    let (token_usage_from_sql, token_usage_agent_sql, token_usage_source_sql) =
        token_usage_from_sql_agent_and_source_sql(conn);
    let token_usage_agent_sql = token_usage_agent_sql.unwrap_or_else(|| "'unknown'".to_string());
    let key_sql = match dim {
        Dim::Model => normalized_analytics_model_family_sql_expr("tu.model_family"),
        Dim::Agent => token_usage_agent_sql.clone(),
        Dim::Source => token_usage_source_sql.clone(),
        _ => unreachable!(
            "track-b token_usage fallback only supports model, agent, and source breakdowns"
        ),
    };
    let input_sql = sum_or_zero("tu.input_tokens", has_input_tokens);
    let output_sql = sum_or_zero("tu.output_tokens", has_output_tokens);
    let cache_read_sql = sum_or_zero("tu.cache_read_tokens", has_cache_read_tokens);
    let cache_creation_sql = sum_or_zero("tu.cache_creation_tokens", has_cache_creation_tokens);
    let thinking_sql = sum_or_zero("tu.thinking_tokens", has_thinking_tokens);
    let total_sql = sum_or_zero("tu.total_tokens", true);
    let content_chars_sql = sum_or_zero("tu.content_chars", has_content_chars);
    let estimated_cost_sql = if has_estimated_cost {
        "SUM(COALESCE(tu.estimated_cost_usd, 0.0))".to_string()
    } else {
        "0.0".to_string()
    };
    let tool_calls_sql = sum_or_zero("tu.tool_call_count", has_tool_call_count);
    let user_count_sql = count_role("user");
    let assistant_count_sql = count_role("assistant");

    let order_sql = match metric {
        Metric::ApiTotal => total_sql.clone(),
        Metric::ApiInput => input_sql.clone(),
        Metric::ApiOutput => output_sql.clone(),
        Metric::CacheRead => cache_read_sql.clone(),
        Metric::CacheCreation => cache_creation_sql.clone(),
        Metric::Thinking => thinking_sql.clone(),
        Metric::ContentEstTotal => content_chars_sql.clone(),
        Metric::ToolCalls => tool_calls_sql.clone(),
        Metric::PlanCount | Metric::CoveragePct | Metric::MessageCount => "COUNT(*)".to_string(),
        Metric::EstimatedCostUsd => estimated_cost_sql.clone(),
    };

    let token_usage_time_sql = token_usage_time_sql(conn);
    let time_column = token_usage_time_sql
        .as_deref()
        .map(AnalyticsTimeColumn::TimestampMs)
        .unwrap_or(AnalyticsTimeColumn::Day("tu.day_id"));
    let (where_sql, params) = build_filtered_where_sql(
        filter,
        Some("tu.workspace_id"),
        Some(token_usage_agent_sql.clone()),
        token_usage_source_sql,
        Some(time_column),
    );

    let sql = format!(
        "SELECT {key_sql},
                COUNT(*),
                {user_count_sql},
                {assistant_count_sql},
                {tool_calls_sql},
                {input_sql},
                {output_sql},
                {cache_read_sql},
                {cache_creation_sql},
                {thinking_sql},
                {total_sql},
                {content_chars_sql},
                {estimated_cost_sql}
         FROM {token_usage_from_sql}
         {where_sql}
         GROUP BY {key_sql}
         ORDER BY {order_sql} DESC
         LIMIT {limit}"
    );

    let raw_rows = conn
        .query_map_collect(&sql, &params, |row: &Row| {
            let get_i64 = |idx| {
                row.get_typed::<i64>(idx)
                    .or_else(|_| row.get_typed::<f64>(idx).map(|value| value.round() as i64))
            };
            let get_f64 = |idx| {
                row.get_typed::<f64>(idx)
                    .or_else(|_| row.get_typed::<i64>(idx).map(|value| value as f64))
            };

            let key: String = row.get_typed(0)?;
            let message_count: i64 = get_i64(1)?;
            let user_message_count: i64 = get_i64(2)?;
            let assistant_message_count: i64 = get_i64(3)?;
            let tool_call_count: i64 = get_i64(4)?;
            let api_input_tokens_total: i64 = get_i64(5)?;
            let api_output_tokens_total: i64 = get_i64(6)?;
            let api_cache_read_tokens_total: i64 = get_i64(7)?;
            let api_cache_creation_tokens_total: i64 = get_i64(8)?;
            let api_thinking_tokens_total: i64 = get_i64(9)?;
            let api_tokens_total: i64 = get_i64(10)?;
            let total_content_chars: i64 = get_i64(11)?;
            let estimated_cost_usd: f64 = get_f64(12)?;
            let bucket = UsageBucket {
                message_count,
                user_message_count,
                assistant_message_count,
                tool_call_count,
                api_coverage_message_count: message_count,
                content_tokens_est_total: total_content_chars / 4,
                api_tokens_total,
                api_input_tokens_total,
                api_output_tokens_total,
                api_cache_read_tokens_total,
                api_cache_creation_tokens_total,
                api_thinking_tokens_total,
                estimated_cost_usd,
                ..Default::default()
            };
            Ok((key, bucket))
        })
        .map_err(|e| analytics_query_error("Breakdown query failed", e))?;

    let rows = raw_rows
        .into_iter()
        .map(|(key, bucket)| {
            let value = match metric {
                Metric::ApiTotal => bucket.api_tokens_total,
                Metric::ApiInput => bucket.api_input_tokens_total,
                Metric::ApiOutput => bucket.api_output_tokens_total,
                Metric::CacheRead => bucket.api_cache_read_tokens_total,
                Metric::CacheCreation => bucket.api_cache_creation_tokens_total,
                Metric::Thinking => bucket.api_thinking_tokens_total,
                Metric::ContentEstTotal => bucket.content_tokens_est_total,
                Metric::ToolCalls => bucket.tool_call_count,
                Metric::PlanCount => 0,
                Metric::CoveragePct => {
                    super::derive::safe_pct(bucket.api_coverage_message_count, bucket.message_count)
                        .round() as i64
                }
                Metric::MessageCount => bucket.message_count,
                Metric::EstimatedCostUsd => bucket.estimated_cost_usd.round() as i64,
            };
            breakdown_row_with_value(key, bucket, value)
        })
        .collect();

    Ok(BreakdownResult {
        rows,
        dim,
        metric,
        source_table: "token_usage".into(),
        elapsed_ms: query_start.elapsed().as_millis() as u64,
    })
}

fn query_track_a_breakdown_from_raw(
    conn: &Connection,
    filter: &AnalyticsFilter,
    dim: Dim,
    metric: Metric,
    limit: usize,
    query_start: std::time::Instant,
) -> AnalyticsResult<BreakdownResult> {
    let has_agents = table_exists(conn, "agents");
    let has_origin_host = table_has_column(conn, "conversations", "origin_host");
    let canonical_message_metrics_sql = canonical_message_metrics_from_sql(conn);
    let join_message_metrics =
        !matches!(metric, Metric::MessageCount) && canonical_message_metrics_sql.is_some();
    let message_metrics_has_message_id = table_exists(conn, "message_metrics")
        && table_has_column(conn, "message_metrics", "message_id");
    let has_api_data_source =
        join_message_metrics && table_has_column(conn, "message_metrics", "api_data_source");
    let has_tool_call_count =
        join_message_metrics && table_has_column(conn, "message_metrics", "tool_call_count");
    let has_has_plan =
        join_message_metrics && table_has_column(conn, "message_metrics", "has_plan");
    let has_message_metrics_created_at = message_metrics_has_message_id
        && table_has_column(conn, "message_metrics", "created_at_ms");

    let conversation_sql = if has_origin_host {
        "SELECT id, TRIM(COALESCE(source_id, '')), TRIM(COALESCE(origin_host, '')) FROM conversations"
    } else {
        "SELECT id, TRIM(COALESCE(source_id, '')), '' FROM conversations"
    };
    let conversation_sources: BTreeMap<i64, (String, String)> = conn
        .query_map_collect(conversation_sql, &[], |row: &Row| {
            Ok((
                row.get_typed::<i64>(0)?,
                (row.get_typed::<String>(1)?, row.get_typed::<String>(2)?),
            ))
        })
        .map_err(|e| analytics_query_error("Breakdown query failed", e))?
        .into_iter()
        .collect();

    let mut from_sql = String::from("messages m JOIN conversations c ON c.id = m.conversation_id");
    if has_agents {
        from_sql.push_str(" LEFT JOIN agents a ON a.id = c.agent_id");
    }
    if let Some(message_metrics_sql) = &canonical_message_metrics_sql {
        from_sql.push_str(" LEFT JOIN ");
        from_sql.push_str(message_metrics_sql);
        from_sql.push_str(" ON mm.message_id = m.id");
    }

    let filter_for_sql = AnalyticsFilter {
        source: SourceFilter::All,
        ..filter.clone()
    };
    let message_time_sql = if has_message_metrics_created_at {
        if join_message_metrics {
            "COALESCE(m.created_at, mm.created_at_ms, c.started_at, 0)"
        } else {
            "COALESCE(m.created_at, (SELECT MAX(message_metrics.created_at_ms) FROM message_metrics WHERE message_metrics.message_id = m.id), c.started_at, 0)"
        }
    } else {
        "COALESCE(m.created_at, c.started_at, 0)"
    };
    let (where_sql, params) = build_filtered_where_sql(
        &filter_for_sql,
        Some("c.workspace_id"),
        has_agents.then(|| normalized_analytics_agent_sql_expr("a.slug")),
        sql_string_literal("all"),
        Some(AnalyticsTimeColumn::TimestampMs(message_time_sql)),
    );

    let dim_key_expr = match dim {
        Dim::Source => "''".to_string(),
        Dim::Agent => {
            if has_agents {
                normalized_analytics_agent_sql_expr("a.slug")
            } else {
                "'unknown'".to_string()
            }
        }
        Dim::Workspace => "CAST(COALESCE(c.workspace_id, 0) AS TEXT)".to_string(),
        Dim::Model => unreachable!("track A raw breakdown does not support model dimension"),
    };
    let content_tokens_expr = if join_message_metrics {
        "COALESCE(mm.content_tokens_est, 0)"
    } else {
        "0"
    };
    let api_input_expr = if join_message_metrics {
        "COALESCE(mm.api_input_tokens, 0)"
    } else {
        "0"
    };
    let api_output_expr = if join_message_metrics {
        "COALESCE(mm.api_output_tokens, 0)"
    } else {
        "0"
    };
    let api_cache_read_expr = if join_message_metrics {
        "COALESCE(mm.api_cache_read_tokens, 0)"
    } else {
        "0"
    };
    let api_cache_creation_expr = if join_message_metrics {
        "COALESCE(mm.api_cache_creation_tokens, 0)"
    } else {
        "0"
    };
    let api_thinking_expr = if join_message_metrics {
        "COALESCE(mm.api_thinking_tokens, 0)"
    } else {
        "0"
    };
    let api_covered_expr = if has_api_data_source {
        "CASE WHEN mm.api_data_source = 'api' THEN 1 ELSE 0 END"
    } else {
        "0"
    };
    let tool_call_expr = if has_tool_call_count {
        "COALESCE(mm.tool_call_count, 0)"
    } else {
        "0"
    };
    let has_plan_expr = if has_has_plan {
        "CASE WHEN COALESCE(mm.has_plan, 0) != 0 THEN 1 ELSE 0 END"
    } else {
        "0"
    };

    let sql = format!(
        "SELECT m.conversation_id,
                {dim_key_expr},
                m.role,
                {content_tokens_expr},
                {api_input_expr},
                {api_output_expr},
                {api_cache_read_expr},
                {api_cache_creation_expr},
                {api_thinking_expr},
                {api_covered_expr},
                {tool_call_expr},
                {has_plan_expr}
         FROM {from_sql}{where_sql}"
    );

    let row_buckets = conn
        .query_map_collect(&sql, &params, |row: &Row| {
            let conversation_id: i64 = row.get_typed(0)?;
            let dim_key: String = row.get_typed(1)?;
            let role: String = row.get_typed(2)?;
            let content_tokens_est: i64 = row.get_typed(3)?;
            let api_input_tokens: i64 = row.get_typed(4)?;
            let api_output_tokens: i64 = row.get_typed(5)?;
            let api_cache_read_tokens: i64 = row.get_typed(6)?;
            let api_cache_creation_tokens: i64 = row.get_typed(7)?;
            let api_thinking_tokens: i64 = row.get_typed(8)?;
            let api_covered: i64 = row.get_typed(9)?;
            let tool_call_count: i64 = row.get_typed(10)?;
            let has_plan: i64 = row.get_typed(11)?;

            let mut bucket = UsageBucket {
                message_count: 1,
                user_message_count: i64::from(role == "user"),
                assistant_message_count: i64::from(role == "assistant"),
                tool_call_count,
                plan_message_count: has_plan,
                api_coverage_message_count: api_covered,
                content_tokens_est_total: content_tokens_est,
                content_tokens_est_user: if role == "user" {
                    content_tokens_est
                } else {
                    0
                },
                content_tokens_est_assistant: if role == "assistant" {
                    content_tokens_est
                } else {
                    0
                },
                api_input_tokens_total: api_input_tokens,
                api_output_tokens_total: api_output_tokens,
                api_cache_read_tokens_total: api_cache_read_tokens,
                api_cache_creation_tokens_total: api_cache_creation_tokens,
                api_thinking_tokens_total: api_thinking_tokens,
                ..Default::default()
            };
            bucket.api_tokens_total = api_input_tokens
                + api_output_tokens
                + api_cache_read_tokens
                + api_cache_creation_tokens
                + api_thinking_tokens;
            Ok((conversation_id, dim_key, bucket))
        })
        .map_err(|e| analytics_query_error("Breakdown query failed", e))?;

    let mut grouped_buckets: BTreeMap<String, UsageBucket> = BTreeMap::new();
    for (conversation_id, dim_key, bucket) in row_buckets {
        let (source_id, origin_host) = conversation_sources
            .get(&conversation_id)
            .cloned()
            .unwrap_or_else(|| {
                (
                    crate::sources::provenance::LOCAL_SOURCE_ID.to_string(),
                    String::new(),
                )
            });
        let normalized_source_key =
            normalized_analytics_source_identity_value(&source_id, &origin_host);
        if !analytics_source_filter_matches_key(&filter.source, &normalized_source_key) {
            continue;
        }
        let group_key = match dim {
            Dim::Source => normalized_source_key,
            Dim::Agent | Dim::Workspace => dim_key,
            Dim::Model => unreachable!("track A raw breakdown does not support model dimension"),
        };
        grouped_buckets.entry(group_key).or_default().merge(&bucket);
    }

    let mut rows: Vec<BreakdownRow> = grouped_buckets
        .into_iter()
        .map(|(key, bucket)| {
            let value = match metric {
                Metric::ApiTotal => bucket.api_tokens_total,
                Metric::ApiInput => bucket.api_input_tokens_total,
                Metric::ApiOutput => bucket.api_output_tokens_total,
                Metric::CacheRead => bucket.api_cache_read_tokens_total,
                Metric::CacheCreation => bucket.api_cache_creation_tokens_total,
                Metric::Thinking => bucket.api_thinking_tokens_total,
                Metric::ContentEstTotal => bucket.content_tokens_est_total,
                Metric::ToolCalls => bucket.tool_call_count,
                Metric::PlanCount => bucket.plan_message_count,
                Metric::CoveragePct => {
                    super::derive::safe_pct(bucket.api_coverage_message_count, bucket.message_count)
                        .round() as i64
                }
                Metric::MessageCount => bucket.message_count,
                Metric::EstimatedCostUsd => bucket.estimated_cost_usd.round() as i64,
            };
            breakdown_row_with_value(key, bucket, value)
        })
        .collect();

    rows.sort_by(|a, b| b.value.cmp(&a.value).then_with(|| a.key.cmp(&b.key)));
    rows.truncate(limit);

    Ok(BreakdownResult {
        rows,
        dim,
        metric,
        source_table: if join_message_metrics {
            "message_metrics".into()
        } else {
            "messages".into()
        },
        elapsed_ms: query_start.elapsed().as_millis() as u64,
    })
}

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

    // Track B has model_family and estimated_cost_usd.
    // Workspace is Track A-only (usage_daily) because Track B has no workspace_id.
    let (table, dim_col, use_track_b) = breakdown_route(dim, metric);
    if use_track_b
        && token_usage_supports_track_b_metric(conn, metric)
        && track_b_breakdown_requires_token_usage_fallback(filter, dim)
    {
        return query_track_b_breakdown_from_token_usage(
            conn,
            filter,
            dim,
            metric,
            limit,
            query_start,
        );
    }
    if !use_track_b
        && track_a_breakdown_supports_raw_metric(conn, metric)
        && track_a_breakdown_requires_raw_fallback(filter, dim)
    {
        return query_track_a_breakdown_from_raw(conn, filter, dim, metric, limit, query_start);
    }
    let dim_col_sql = match dim {
        Dim::Source => normalized_analytics_source_id_sql_expr(dim_col),
        Dim::Agent => normalized_analytics_agent_sql_expr(dim_col),
        _ => dim_col.to_string(),
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
    let filter_for_sql = if matches!(dim, Dim::Source) {
        AnalyticsFilter {
            source: SourceFilter::All,
            ..filter.clone()
        }
    } else {
        filter.clone()
    };
    let (day_min, day_max) = bucketing::resolve_day_range(filter);
    let (dim_parts, dim_params) = build_where_parts(
        &filter_for_sql,
        if use_track_b {
            None
        } else {
            Some("workspace_id")
        },
    );
    let mut where_parts = dim_parts;
    let mut bind_values = dim_params;

    if let Some(min) = day_min {
        bind_values.push(ParamValue::from(min));
        where_parts.push(format!("day_id >= ?{}", bind_values.len()));
    }
    if let Some(max) = day_max {
        bind_values.push(ParamValue::from(max));
        where_parts.push(format!("day_id <= ?{}", bind_values.len()));
    }

    // Exclude pre-aggregated "all" permutation rows from token_daily_stats
    // to avoid double-counting. The SUM in the query handles aggregation.
    if use_track_b {
        where_parts.push("model_family != 'all'".into());
        where_parts.push("agent_slug != 'all'".into());
        where_parts.push("source_id != 'all'".into());
    }

    let where_clause = if where_parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_parts.join(" AND "))
    };

    // For Track A (usage_daily), we can select the full bucket.
    // For Track B (token_daily_stats), column names differ — map accordingly.
    // Source breakdowns normalize/filter source ids in Rust after the SQL rows are read,
    // so applying LIMIT in SQL first can drop the matching source entirely.
    let sql_limit = if matches!(dim, Dim::Source) && !matches!(filter.source, SourceFilter::All) {
        None
    } else {
        Some(limit)
    };
    let sql = if use_track_b {
        // Track B: token_daily_stats columns map to different names.
        build_breakdown_sql_track_b(&dim_col_sql, &metric, &where_clause, sql_limit)
    } else {
        // Track A: usage_daily — full UsageBucket columns available.
        let has_plan_token_rollups = table_has_plan_token_rollups(conn, "usage_daily");
        build_breakdown_sql_track_a(
            &dim_col_sql,
            &metric,
            &where_clause,
            sql_limit,
            has_plan_token_rollups,
        )
    };

    let param_values: Vec<ParamValue> = bind_values.clone();

    let mut rows = if use_track_b {
        read_breakdown_rows_track_b(conn, &sql, &param_values, &metric)?
    } else {
        read_breakdown_rows_track_a(conn, &sql, &param_values, &metric)?
    };

    if matches!(dim, Dim::Source) {
        rows.retain(|row| analytics_source_filter_matches_key(&filter.source, &row.key));
        rows.truncate(limit);
    }

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
    limit: Option<usize>,
    has_plan_token_rollups: bool,
) -> String {
    let (sort_value_sql, order_by_expr) = match metric {
        Metric::CoveragePct => (
            "SUM(api_coverage_message_count)".to_string(),
            "CASE
                WHEN SUM(message_count) = 0 THEN 0.0
                ELSE CAST(SUM(api_coverage_message_count) AS REAL) / CAST(SUM(message_count) AS REAL)
             END"
                .to_string(),
        ),
        _ => {
            let order_col = metric.rollup_column().unwrap_or("api_tokens_total");
            (format!("SUM({order_col})"), format!("SUM({order_col})"))
        }
    };
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
    let limit_clause = limit
        .map(|limit| {
            format!(
                "
         LIMIT {limit}"
            )
        })
        .unwrap_or_default();
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
                {sort_value_sql}
         FROM (
             SELECT * FROM usage_daily
             {where_clause}
         ) filtered_usage_daily
         GROUP BY CAST({dim_col} AS TEXT)
         ORDER BY {order_by_expr} DESC, CAST({dim_col} AS TEXT) ASC{limit_clause}"
    )
}

/// Build SQL for breakdown from token_daily_stats (Track B).
fn build_breakdown_sql_track_b(
    dim_col: &str,
    metric: &Metric,
    where_clause: &str,
    limit: Option<usize>,
) -> String {
    // Map Metric to the Track B column name.
    let order_col = match metric {
        Metric::ApiTotal => "grand_total_tokens",
        Metric::ApiInput => "total_input_tokens",
        Metric::ApiOutput => "total_output_tokens",
        Metric::CacheRead => "total_cache_read_tokens",
        Metric::CacheCreation => "total_cache_creation_tokens",
        Metric::Thinking => "total_thinking_tokens",
        Metric::ContentEstTotal => "total_content_chars",
        Metric::ToolCalls => "total_tool_calls",
        // token_daily_stats does not carry plan-message rollups.
        // Keep ordering deterministic/useful by call volume.
        Metric::PlanCount => "api_call_count",
        // Coverage on Track B is derived and generally 100%; rank by call volume.
        Metric::CoveragePct => "api_call_count",
        Metric::MessageCount => "api_call_count",
        Metric::EstimatedCostUsd => "estimated_cost_usd",
    };
    let limit_clause = limit
        .map(|limit| {
            format!(
                "
         LIMIT {limit}"
            )
        })
        .unwrap_or_default();
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
         FROM (
             SELECT * FROM token_daily_stats
             {where_clause}
         ) filtered_token_daily_stats
         GROUP BY {dim_col}
         ORDER BY SUM({order_col}) DESC{limit_clause}"
    )
}

/// Read breakdown rows from a Track A (usage_daily) query result.
fn read_breakdown_rows_track_a(
    conn: &Connection,
    sql: &str,
    params: &[ParamValue],
    metric: &Metric,
) -> AnalyticsResult<Vec<BreakdownRow>> {
    let raw_rows = conn
        .query_map_collect(sql, params, |row: &Row| {
            let key: String = row.get_typed(0)?;
            let bucket = UsageBucket {
                message_count: row.get_typed(1)?,
                user_message_count: row.get_typed(2)?,
                assistant_message_count: row.get_typed(3)?,
                tool_call_count: row.get_typed(4)?,
                plan_message_count: row.get_typed(5)?,
                plan_content_tokens_est_total: row.get_typed(6)?,
                plan_api_tokens_total: row.get_typed(7)?,
                api_coverage_message_count: row.get_typed(8)?,
                content_tokens_est_total: row.get_typed(9)?,
                content_tokens_est_user: row.get_typed(10)?,
                content_tokens_est_assistant: row.get_typed(11)?,
                api_tokens_total: row.get_typed(12)?,
                api_input_tokens_total: row.get_typed(13)?,
                api_output_tokens_total: row.get_typed(14)?,
                api_cache_read_tokens_total: row.get_typed(15)?,
                api_cache_creation_tokens_total: row.get_typed(16)?,
                api_thinking_tokens_total: row.get_typed(17)?,
                ..Default::default()
            };
            let sort_value: i64 = row.get_typed(18)?;
            Ok((key, bucket, sort_value))
        })
        .map_err(|e| analytics_query_error("Breakdown query failed", e))?;

    let mut result = Vec::new();
    for (key, bucket, sort_value) in raw_rows {
        // Some metrics are derived when reading Track A rows.
        let value = match metric {
            Metric::CoveragePct => {
                let pct = super::derive::safe_pct(
                    bucket.api_coverage_message_count,
                    bucket.message_count,
                );
                pct.round() as i64
            }
            // Track A has no cost column; expose stable zero values.
            Metric::EstimatedCostUsd => 0,
            _ => sort_value,
        };
        result.push(breakdown_row_with_value(key, bucket, value));
    }
    Ok(result)
}

/// Read breakdown rows from a Track B (token_daily_stats) query result.
fn read_breakdown_rows_track_b(
    conn: &Connection,
    sql: &str,
    params: &[ParamValue],
    metric: &Metric,
) -> AnalyticsResult<Vec<BreakdownRow>> {
    let raw_rows = conn
        .query_map_collect(sql, params, |row: &Row| {
            let key: String = row.get_typed(0)?;
            let api_call_count: i64 = row.get_typed(1)?;
            let user_message_count: i64 = row.get_typed(2)?;
            let assistant_message_count: i64 = row.get_typed(3)?;
            let total_tool_calls: i64 = row.get_typed(4)?;
            let total_input: i64 = row.get_typed(5)?;
            let total_output: i64 = row.get_typed(6)?;
            let total_cache_read: i64 = row.get_typed(7)?;
            let total_cache_creation: i64 = row.get_typed(8)?;
            let total_thinking: i64 = row.get_typed(9)?;
            let grand_total: i64 = row.get_typed(10)?;
            let total_content_chars: i64 = row.get_typed(11)?;
            let estimated_cost: f64 = row.get_typed(12)?;
            // When the sort metric is a Real column (e.g. estimated_cost_usd),
            // SQLite returns a float.  Round before converting to i64 to avoid
            // truncation (e.g. $0.99 → 1 instead of 0).
            let sort_value: i64 = match row.get_typed::<f64>(13) {
                Ok(v) => v.round() as i64,
                Err(_) => row.get_typed(13)?,
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
        .map_err(|e| analytics_query_error("Breakdown query failed", e))?;

    let mut result = Vec::new();
    for (key, bucket, sort_value) in raw_rows {
        let value = match metric {
            Metric::CoveragePct => {
                super::derive::safe_pct(bucket.api_coverage_message_count, bucket.message_count)
                    .round() as i64
            }
            Metric::ContentEstTotal => bucket.content_tokens_est_total,
            Metric::PlanCount => 0,
            _ => sort_value,
        };
        result.push(breakdown_row_with_value(key, bucket, value));
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
fn query_tools_from_raw(
    conn: &Connection,
    filter: &AnalyticsFilter,
    query_start: std::time::Instant,
    limit: usize,
) -> AnalyticsResult<ToolReport> {
    let has_agents = table_exists(conn, "agents");
    let has_origin_host = table_has_column(conn, "conversations", "origin_host");
    let canonical_message_metrics_sql = canonical_message_metrics_from_sql(conn);
    let has_message_metrics_created_at = canonical_message_metrics_sql.is_some()
        && table_has_column(conn, "message_metrics", "created_at_ms");
    let has_content_tokens_est = table_has_column(conn, "message_metrics", "content_tokens_est");
    let has_api_input_tokens = table_has_column(conn, "message_metrics", "api_input_tokens");
    let has_api_output_tokens = table_has_column(conn, "message_metrics", "api_output_tokens");
    let has_api_cache_read_tokens =
        table_has_column(conn, "message_metrics", "api_cache_read_tokens");
    let has_api_cache_creation_tokens =
        table_has_column(conn, "message_metrics", "api_cache_creation_tokens");
    let has_api_thinking_tokens = table_has_column(conn, "message_metrics", "api_thinking_tokens");

    let conversation_sql = if has_origin_host {
        "SELECT id, TRIM(COALESCE(source_id, '')), TRIM(COALESCE(origin_host, '')) FROM conversations"
    } else {
        "SELECT id, TRIM(COALESCE(source_id, '')), '' FROM conversations"
    };
    let conversation_sources: BTreeMap<i64, (String, String)> = conn
        .query_map_collect(conversation_sql, &[], |row: &Row| {
            Ok((
                row.get_typed::<i64>(0)?,
                (row.get_typed::<String>(1)?, row.get_typed::<String>(2)?),
            ))
        })
        .map_err(|e| analytics_query_error("Tool report query failed", e))?
        .into_iter()
        .collect();

    let mut from_sql = String::from("messages m JOIN conversations c ON c.id = m.conversation_id");
    if has_agents {
        from_sql.push_str(" LEFT JOIN agents a ON a.id = c.agent_id");
    }
    if let Some(message_metrics_sql) = &canonical_message_metrics_sql {
        from_sql.push_str(" LEFT JOIN ");
        from_sql.push_str(message_metrics_sql);
        from_sql.push_str(" ON mm.message_id = m.id");
    }

    let filter_for_sql = AnalyticsFilter {
        source: SourceFilter::All,
        ..filter.clone()
    };
    let message_time_sql = if has_message_metrics_created_at {
        "COALESCE(m.created_at, mm.created_at_ms, c.started_at, 0)"
    } else {
        "COALESCE(m.created_at, c.started_at, 0)"
    };
    let (where_sql, params) = build_filtered_where_sql(
        &filter_for_sql,
        Some("c.workspace_id"),
        has_agents.then(|| normalized_analytics_agent_sql_expr("a.slug")),
        sql_string_literal("all"),
        Some(AnalyticsTimeColumn::TimestampMs(message_time_sql)),
    );

    let agent_sql = if has_agents {
        normalized_analytics_agent_sql_expr("a.slug")
    } else {
        "'unknown'".to_string()
    };
    let tool_call_expr = "COALESCE(mm.tool_call_count, 0)";
    let content_tokens_expr = if has_content_tokens_est {
        "COALESCE(mm.content_tokens_est, 0)"
    } else {
        "0"
    };
    let api_input_expr = if has_api_input_tokens {
        "COALESCE(mm.api_input_tokens, 0)"
    } else {
        "0"
    };
    let api_output_expr = if has_api_output_tokens {
        "COALESCE(mm.api_output_tokens, 0)"
    } else {
        "0"
    };
    let api_cache_read_expr = if has_api_cache_read_tokens {
        "COALESCE(mm.api_cache_read_tokens, 0)"
    } else {
        "0"
    };
    let api_cache_creation_expr = if has_api_cache_creation_tokens {
        "COALESCE(mm.api_cache_creation_tokens, 0)"
    } else {
        "0"
    };
    let api_thinking_expr = if has_api_thinking_tokens {
        "COALESCE(mm.api_thinking_tokens, 0)"
    } else {
        "0"
    };
    let api_tokens_expr = format!(
        "({api_input_expr} + {api_output_expr} + {api_cache_read_expr} + {api_cache_creation_expr} + {api_thinking_expr})"
    );

    let sql = format!(
        "SELECT m.conversation_id,
                {agent_sql},
                {tool_call_expr},
                {content_tokens_expr},
                {api_tokens_expr}
         FROM {from_sql}{where_sql}"
    );

    let raw_rows: Vec<(i64, String, i64, i64, i64)> = conn
        .query_map_collect(&sql, &params, |row: &Row| {
            Ok((
                row.get_typed::<i64>(0)?,
                row.get_typed::<String>(1)?,
                row.get_typed::<i64>(2)?,
                row.get_typed::<i64>(3)?,
                row.get_typed::<i64>(4)?,
            ))
        })
        .map_err(|e| analytics_query_error("Tool report query failed", e))?;

    let mut grouped_rows: BTreeMap<String, (i64, i64, i64, i64)> = BTreeMap::new();
    for (conversation_id, key, tool_call_count, content_tokens_est_total, api_tokens_total) in
        raw_rows
    {
        let (source_id, origin_host) = conversation_sources
            .get(&conversation_id)
            .cloned()
            .unwrap_or_else(|| {
                (
                    crate::sources::provenance::LOCAL_SOURCE_ID.to_string(),
                    String::new(),
                )
            });
        let normalized_source_key =
            normalized_analytics_source_identity_value(&source_id, &origin_host);
        if !analytics_source_filter_matches_key(&filter.source, &normalized_source_key) {
            continue;
        }

        let entry = grouped_rows.entry(key).or_default();
        entry.0 += tool_call_count;
        entry.1 += 1;
        entry.2 += api_tokens_total;
        entry.3 += content_tokens_est_total;
    }

    let mut rows: Vec<ToolRow> = grouped_rows
        .into_iter()
        .map(
            |(
                key,
                (tool_call_count, message_count, api_tokens_total, content_tokens_est_total),
            )| {
                let tool_calls_per_1k_api_tokens = if api_tokens_total > 0 {
                    Some(tool_call_count as f64 / (api_tokens_total as f64 / 1000.0))
                } else {
                    None
                };
                let tool_calls_per_1k_content_tokens = if content_tokens_est_total > 0 {
                    Some(tool_call_count as f64 / (content_tokens_est_total as f64 / 1000.0))
                } else {
                    None
                };
                ToolRow {
                    key,
                    tool_call_count,
                    message_count,
                    api_tokens_total,
                    tool_calls_per_1k_api_tokens,
                    tool_calls_per_1k_content_tokens,
                }
            },
        )
        .collect();

    rows.sort_by(|a, b| {
        b.tool_call_count
            .cmp(&a.tool_call_count)
            .then_with(|| a.key.cmp(&b.key))
    });

    let total_tool_calls = rows.iter().map(|row| row.tool_call_count).sum();
    let total_messages = rows.iter().map(|row| row.message_count).sum();
    let total_api_tokens = rows.iter().map(|row| row.api_tokens_total).sum();

    rows.truncate(limit);

    Ok(ToolReport {
        rows,
        total_tool_calls,
        total_messages,
        total_api_tokens,
        source_table: "message_metrics".into(),
        elapsed_ms: query_start.elapsed().as_millis() as u64,
    })
}

pub fn query_tools(
    conn: &Connection,
    filter: &AnalyticsFilter,
    group_by: GroupBy,
    limit: usize,
) -> AnalyticsResult<ToolReport> {
    let query_start = std::time::Instant::now();

    if track_a_timeseries_requires_raw_fallback(filter)
        && track_a_tools_supports_raw_source_fallback(conn)
    {
        return query_tools_from_raw(conn, filter, query_start, limit);
    }

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
    let (dim_parts, dim_params) = build_where_parts(filter, Some("workspace_id"));
    let mut where_parts = dim_parts;
    let mut bind_values = dim_params;

    match group_by {
        GroupBy::Hour => {
            if let Some(min) = hour_min {
                bind_values.push(ParamValue::from(min));
                where_parts.push(format!("{bucket_col} >= ?{}", bind_values.len()));
            }
            if let Some(max) = hour_max {
                bind_values.push(ParamValue::from(max));
                where_parts.push(format!("{bucket_col} <= ?{}", bind_values.len()));
            }
        }
        _ => {
            if let Some(min) = day_min {
                bind_values.push(ParamValue::from(min));
                where_parts.push(format!("{bucket_col} >= ?{}", bind_values.len()));
            }
            if let Some(max) = day_max {
                bind_values.push(ParamValue::from(max));
                where_parts.push(format!("{bucket_col} <= ?{}", bind_values.len()));
            }
        }
    }

    let where_clause = if where_parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_parts.join(" AND "))
    };

    // Group by normalized agent slug so analytics matches displayed metadata.
    let tool_agent_sql = normalized_analytics_agent_sql_expr("agent_slug");
    let sql = format!(
        "SELECT {tool_agent_sql},
                SUM(tool_call_count),
                SUM(message_count),
                SUM(api_tokens_total),
                SUM(content_tokens_est_total)
         FROM (
             SELECT * FROM {table}
             {where_clause}
         ) filtered_tool_usage
         GROUP BY {tool_agent_sql}
         ORDER BY SUM(tool_call_count) DESC"
    );

    let param_values: Vec<ParamValue> = bind_values.clone();

    let tool_rows = conn
        .query_map_collect(&sql, &param_values, |row: &Row| {
            let key: String = row.get_typed(0)?;
            let tool_call_count: i64 = row.get_typed(1)?;
            let message_count: i64 = row.get_typed(2)?;
            let api_tokens_total: i64 = row.get_typed(3)?;
            let content_tokens_est_total: i64 = row.get_typed(4)?;

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
        .map_err(|e| analytics_query_error("Tool report query failed", e))?;

    let mut rows = Vec::new();
    let mut total_tool_calls: i64 = 0;
    let mut total_messages: i64 = 0;
    let mut total_api_tokens: i64 = 0;

    for r in tool_rows {
        total_tool_calls += r.tool_call_count;
        total_messages += r.message_count;
        total_api_tokens += r.api_tokens_total;
        rows.push(r);
    }
    rows.truncate(limit);

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
// query_session_scatter
// ---------------------------------------------------------------------------

/// Query per-session `(message_count, api_tokens_total)` points for Explorer
/// scatter plots.
///
/// Uses `conversations` + `messages` as the primary source and prefers
/// `message_metrics` API-token columns when available. Falls back to
/// `token_usage.total_tokens`, then conversation rollups.
pub fn query_session_scatter(
    conn: &Connection,
    filter: &AnalyticsFilter,
    limit: usize,
) -> AnalyticsResult<Vec<SessionScatterPoint>> {
    if !table_exists(conn, "conversations") || !table_exists(conn, "messages") {
        return Ok(Vec::new());
    }

    let has_agents = table_exists(conn, "agents");
    if !has_agents && !filter.agents.is_empty() {
        return Ok(Vec::new());
    }

    let mut where_parts: Vec<String> = Vec::new();
    let mut bind_values: Vec<ParamValue> = Vec::new();

    let canonical_message_metrics_sql = canonical_message_metrics_from_sql(conn);
    let has_message_metrics = canonical_message_metrics_sql.is_some();
    let has_token_usage = table_exists(conn, "token_usage");
    let has_mm_created_at =
        has_message_metrics && table_has_column(conn, "message_metrics", "created_at_ms");
    let has_tu_timestamp = has_token_usage && table_has_column(conn, "token_usage", "timestamp_ms");

    // Agent filters.
    if !filter.agents.is_empty() {
        let normalized_agent_sql = normalized_analytics_agent_sql_expr("a.slug");
        let agent_literals: Vec<String> = filter
            .agents
            .iter()
            .map(|agent| sql_string_literal(&normalized_analytics_agent_value(agent.as_str())))
            .collect();
        where_parts.push(format!(
            "{normalized_agent_sql} IN ({})",
            agent_literals.join(", ")
        ));
    }

    // Source filter.
    let normalized_source_sql = if table_has_column(conn, "conversations", "origin_host") {
        normalized_analytics_source_identity_sql_expr("c.source_id", "c.origin_host")
    } else {
        normalized_analytics_source_id_sql_expr("c.source_id")
    };
    push_source_filter_clause(
        &mut where_parts,
        &mut bind_values,
        &filter.source,
        &normalized_source_sql,
    );

    // Workspace filters.
    if !filter.workspace_ids.is_empty() {
        let placeholders: Vec<String> = filter
            .workspace_ids
            .iter()
            .map(|workspace_id| {
                bind_values.push(ParamValue::from(*workspace_id));
                format!("?{}", bind_values.len())
            })
            .collect();
        where_parts.push(format!(
            "COALESCE(c.workspace_id, 0) IN ({})",
            placeholders.join(", ")
        ));
    }

    let has_conv_rollup = table_has_column(conn, "conversations", "grand_total_tokens");
    let has_mm_api_source =
        has_message_metrics && table_has_column(conn, "message_metrics", "api_data_source");

    let message_metrics_join = canonical_message_metrics_sql
        .as_deref()
        .map(|message_metrics_sql| {
            format!(" LEFT JOIN {message_metrics_sql} ON mm.message_id = m.id")
        })
        .unwrap_or_default();
    let token_usage_join = if has_token_usage {
        if has_tu_timestamp {
            " LEFT JOIN (SELECT message_id, MAX(COALESCE(total_tokens, 0)) AS total_tokens, MAX(timestamp_ms) AS timestamp_ms FROM token_usage GROUP BY message_id) tu ON tu.message_id = m.id"
        } else {
            " LEFT JOIN (SELECT message_id, MAX(COALESCE(total_tokens, 0)) AS total_tokens FROM token_usage GROUP BY message_id) tu ON tu.message_id = m.id"
        }
    } else {
        ""
    };

    let mm_api_sum = "COALESCE(mm.api_input_tokens, 0)
            + COALESCE(mm.api_output_tokens, 0)
            + COALESCE(mm.api_cache_read_tokens, 0)
            + COALESCE(mm.api_cache_creation_tokens, 0)
            + COALESCE(mm.api_thinking_tokens, 0)";
    let mm_has_api_values = "COALESCE(
            mm.api_input_tokens,
            mm.api_output_tokens,
            mm.api_cache_read_tokens,
            mm.api_cache_creation_tokens,
            mm.api_thinking_tokens
        ) IS NOT NULL";
    let message_token_expr = if has_message_metrics && has_token_usage {
        if has_mm_api_source {
            format!(
                "CASE
                    WHEN mm.message_id IS NULL THEN COALESCE(tu.total_tokens, 0)
                    WHEN LOWER(TRIM(COALESCE(mm.api_data_source, 'api'))) = 'estimated'
                        THEN COALESCE(tu.total_tokens, 0)
                    WHEN {mm_has_api_values} THEN {mm_api_sum}
                    ELSE COALESCE(tu.total_tokens, 0)
                 END"
            )
        } else {
            format!(
                "CASE
                    WHEN mm.message_id IS NULL THEN COALESCE(tu.total_tokens, 0)
                    WHEN {mm_has_api_values} THEN {mm_api_sum}
                    ELSE COALESCE(tu.total_tokens, 0)
                 END"
            )
        }
    } else if has_message_metrics {
        format!(
            "CASE
                WHEN mm.message_id IS NOT NULL THEN {mm_api_sum}
                ELSE 0
             END"
        )
    } else if has_token_usage {
        "COALESCE(tu.total_tokens, 0)".to_string()
    } else {
        "0".to_string()
    };
    let normalize_sql = |expr: &str| {
        format!("CASE WHEN {expr} BETWEEN 0 AND 100000000000 THEN {expr} * 1000 ELSE {expr} END")
    };
    let normalized_created_at = normalize_sql("m.created_at");
    let normalized_mm_created_at = normalize_sql("mm.created_at_ms");
    let normalized_tu_timestamp = normalize_sql("tu.timestamp_ms");
    let normalized_started_at = normalize_sql("c_msg.started_at");
    let message_timestamp_expr = match (has_mm_created_at, has_tu_timestamp) {
        (true, true) => format!(
            "CASE
                WHEN m.created_at IS NOT NULL THEN {normalized_created_at}
                WHEN mm.created_at_ms IS NOT NULL THEN {normalized_mm_created_at}
                WHEN tu.timestamp_ms IS NOT NULL THEN {normalized_tu_timestamp}
                WHEN c_msg.started_at IS NOT NULL THEN {normalized_started_at}
                ELSE 0
             END"
        ),
        (true, false) => format!(
            "CASE
                WHEN m.created_at IS NOT NULL THEN {normalized_created_at}
                WHEN mm.created_at_ms IS NOT NULL THEN {normalized_mm_created_at}
                WHEN c_msg.started_at IS NOT NULL THEN {normalized_started_at}
                ELSE 0
             END"
        ),
        (false, true) => format!(
            "CASE
                WHEN m.created_at IS NOT NULL THEN {normalized_created_at}
                WHEN tu.timestamp_ms IS NOT NULL THEN {normalized_tu_timestamp}
                WHEN c_msg.started_at IS NOT NULL THEN {normalized_started_at}
                ELSE 0
             END"
        ),
        (false, false) => format!(
            "CASE
                WHEN m.created_at IS NOT NULL THEN {normalized_created_at}
                WHEN c_msg.started_at IS NOT NULL THEN {normalized_started_at}
                ELSE 0
             END"
        ),
    };
    let per_message_sql = format!(
        "(SELECT m.id AS message_id,
                 m.conversation_id AS conversation_id,
                 {message_token_expr} AS message_api_tokens,
                 {message_timestamp_expr} AS event_ts_ms
          FROM messages m
          JOIN conversations c_msg ON c_msg.id = m.conversation_id
          {message_metrics_join}
          {token_usage_join}) msg"
    );
    if let Some(min) = filter.since_ms {
        bind_values.push(ParamValue::from(min));
        where_parts.push(format!("msg.event_ts_ms >= ?{}", bind_values.len()));
    }
    if let Some(max) = filter.until_ms {
        bind_values.push(ParamValue::from(max));
        where_parts.push(format!("msg.event_ts_ms <= ?{}", bind_values.len()));
    }

    let where_clause = if where_parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_parts.join(" AND "))
    };

    let detailed_token_expr = "SUM(COALESCE(msg.message_api_tokens, 0))";
    let token_expr = if has_conv_rollup {
        format!(
            "CASE
                WHEN MAX(COALESCE(c.grand_total_tokens, 0)) > ({detailed_token_expr})
                THEN MAX(COALESCE(c.grand_total_tokens, 0))
                ELSE ({detailed_token_expr})
             END"
        )
    } else {
        detailed_token_expr.to_string()
    };

    let agents_join = if has_agents {
        "LEFT JOIN agents a ON a.id = c.agent_id"
    } else {
        ""
    };

    let sql = format!(
        "SELECT {normalized_source_sql},
                c.source_path,
                COUNT(msg.message_id) AS message_count,
                {token_expr} AS api_tokens_total
         FROM conversations c
         JOIN {per_message_sql} ON msg.conversation_id = c.id
         {agents_join}
         {where_clause}
         GROUP BY c.id, {normalized_source_sql}, c.source_path
         HAVING COUNT(msg.message_id) > 0
         ORDER BY api_tokens_total DESC, message_count DESC
         LIMIT {limit}"
    );

    let param_values: Vec<ParamValue> = bind_values.clone();

    let points = conn
        .query_map_collect(&sql, &param_values, |row: &Row| {
            Ok(SessionScatterPoint {
                source_id: row.get_typed(0)?,
                source_path: row.get_typed(1)?,
                message_count: row.get_typed(2)?,
                api_tokens_total: row.get_typed::<Option<i64>>(3)?.unwrap_or(0),
            })
        })
        .map_err(|e| analytics_query_error("Session scatter query failed", e))?;

    Ok(points)
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
    if !table_exists(conn, "token_usage")
        || !table_has_column(conn, "token_usage", "total_tokens")
        || !table_has_column(conn, "token_usage", "estimated_cost_usd")
    {
        return Ok(UnpricedModelsReport {
            models: Vec::new(),
            total_unpriced_tokens: 0,
            total_priced_tokens: 0,
        });
    }

    let has_model_name = table_has_column(conn, "token_usage", "model_name");
    let (from_sql, _, _) = token_usage_from_sql_agent_and_source_sql(conn);
    let models_sql = if has_model_name {
        format!(
            "SELECT CASE
                        WHEN TRIM(COALESCE(tu.model_name, '')) = '' THEN '(none)'
                        ELSE TRIM(COALESCE(tu.model_name, ''))
                    END AS model,
                    SUM(COALESCE(tu.total_tokens, 0)) AS tot,
                    COUNT(*) AS cnt
             FROM {from_sql}
             WHERE tu.estimated_cost_usd IS NULL
             GROUP BY model
             ORDER BY tot DESC
             LIMIT ?1"
        )
    } else {
        format!(
            "SELECT '(none)' AS model,
                    SUM(COALESCE(tu.total_tokens, 0)) AS tot,
                    COUNT(*) AS cnt
             FROM {from_sql}
             WHERE tu.estimated_cost_usd IS NULL
             HAVING COUNT(*) > 0
             LIMIT ?1"
        )
    };

    let models: Vec<UnpricedModel> = conn
        .query_map_collect(
            &models_sql,
            &[ParamValue::from(limit as i64)],
            |row: &Row| {
                Ok(UnpricedModel {
                    model_name: row.get_typed(0)?,
                    total_tokens: row.get_typed(1)?,
                    row_count: row.get_typed(2)?,
                })
            },
        )
        .map_err(|e| AnalyticsError::Db(e.to_string()))?;

    let total_unpriced_tokens: i64 = conn
        .query_row_map(
            &format!(
                "SELECT SUM(COALESCE(tu.total_tokens, 0))
                 FROM {from_sql}
                 WHERE tu.estimated_cost_usd IS NULL"
            ),
            &[],
            |r: &Row| Ok(r.get_typed::<Option<i64>>(0)?.unwrap_or(0)),
        )
        .unwrap_or(0);

    let total_priced_tokens: i64 = conn
        .query_row_map(
            &format!(
                "SELECT SUM(COALESCE(tu.total_tokens, 0))
                 FROM {from_sql}
                 WHERE tu.estimated_cost_usd IS NOT NULL"
            ),
            &[],
            |r: &Row| Ok(r.get_typed::<Option<i64>>(0)?.unwrap_or(0)),
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
        let (parts, params) = build_where_parts(&f, None);
        assert!(parts.is_empty());
        assert!(params.is_empty());
    }

    #[test]
    fn build_where_parts_single_agent() {
        let f = AnalyticsFilter {
            agents: vec!["claude_code".into()],
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f, None);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains("TRIM(COALESCE(agent_slug, ''))"));
        assert!(parts[0].contains("'claude_code'"));
        assert!(params.is_empty());
    }

    #[test]
    fn build_where_parts_single_agent_normalizes_trimmed_unknown_alias() {
        let f = AnalyticsFilter {
            agents: vec!["   ".into()],
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f, None);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains("TRIM(COALESCE(agent_slug, ''))"));
        assert!(parts[0].contains("'unknown'"));
        assert!(params.is_empty());
    }

    #[test]
    fn build_where_parts_multiple_agents() {
        let f = AnalyticsFilter {
            agents: vec!["claude_code".into(), "codex".into(), "aider".into()],
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f, None);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains("TRIM(COALESCE(agent_slug, ''))"));
        assert!(parts[0].contains("'claude_code'"));
        assert!(parts[0].contains("'codex'"));
        assert!(parts[0].contains("'aider'"));
        assert!(params.is_empty());
    }

    #[test]
    fn build_where_parts_source_local() {
        let f = AnalyticsFilter {
            source: SourceFilter::Local,
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f, None);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains("CASE WHEN TRIM(COALESCE(source_id, '')) = ''"));
        assert!(parts[0].contains("= 'local'"));
        assert!(params.is_empty());
    }

    #[test]
    fn build_where_parts_source_remote() {
        let f = AnalyticsFilter {
            source: SourceFilter::Remote,
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f, None);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains("CASE WHEN TRIM(COALESCE(source_id, '')) = ''"));
        assert!(parts[0].contains("!= 'local'"));
        assert!(params.is_empty());
    }

    #[test]
    fn build_where_parts_source_specific() {
        let f = AnalyticsFilter {
            source: SourceFilter::Specific("myhost.local".into()),
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f, None);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains("CASE WHEN TRIM(COALESCE(source_id, '')) = ''"));
        assert!(parts[0].contains("= 'myhost.local'"));
        assert!(params.is_empty());
    }

    #[test]
    fn build_where_parts_source_specific_normalizes_trimmed_local_alias() {
        let f = AnalyticsFilter {
            source: SourceFilter::Specific("  LOCAL  ".into()),
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f, None);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains("CASE WHEN TRIM(COALESCE(source_id, '')) = ''"));
        assert!(parts[0].contains("= 'local'"));
        assert!(params.is_empty());
    }

    #[test]
    fn build_where_parts_combined() {
        let f = AnalyticsFilter {
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f, None);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].contains("TRIM(COALESCE(agent_slug, ''))"));
        assert!(parts[0].contains("'codex'"));
        assert!(parts[1].contains("= 'local'"));
        assert!(params.is_empty());
    }

    #[test]
    fn build_where_parts_workspace_filter_enabled() {
        let f = AnalyticsFilter {
            workspace_ids: vec![7, 42],
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f, Some("workspace_id"));
        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains("workspace_id IN (7, 42)"));
        assert!(params.is_empty());
    }

    #[test]
    fn build_where_parts_workspace_filter_disabled() {
        let f = AnalyticsFilter {
            workspace_ids: vec![7, 42],
            ..Default::default()
        };
        let (parts, params) = build_where_parts(&f, None);
        assert!(parts.is_empty());
        assert!(params.is_empty());
    }

    // -----------------------------------------------------------------------
    // Integration tests with in-memory SQLite
    // -----------------------------------------------------------------------

    /// Create an in-memory database with the usage_daily schema and seed data.
    fn setup_usage_daily_db() -> Connection {
        let conn = Connection::open(":memory:").unwrap();
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
            conn.execute_compat(
                "INSERT INTO usage_daily (day_id, agent_slug, workspace_id, source_id,
                    message_count, user_message_count, assistant_message_count,
                    tool_call_count, plan_message_count, api_coverage_message_count,
                    content_tokens_est_total, content_tokens_est_user, content_tokens_est_assistant,
                    api_tokens_total, api_input_tokens_total, api_output_tokens_total,
                    api_cache_read_tokens_total, api_cache_creation_tokens_total,
                    api_thinking_tokens_total)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
                frankensqlite::params![
                    r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8, r.9, r.10, r.11, r.12, r.13, r.14,
                    r.15, r.16, r.17, r.18
                ],
            )
            .unwrap();
        }

        conn
    }

    #[allow(dead_code)]
    /// Legacy Track A schema fixture (pre plan-token rollup columns).
    fn setup_usage_daily_legacy_db() -> Connection {
        let conn = Connection::open(":memory:").unwrap();
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

    fn setup_usage_hourly_db() -> Connection {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_hourly (
                hour_id INTEGER NOT NULL,
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
                PRIMARY KEY (hour_id, agent_slug, workspace_id, source_id)
            );",
        )
        .unwrap();

        conn.execute_compat(
            "INSERT INTO usage_hourly (
                hour_id, agent_slug, workspace_id, source_id,
                message_count, user_message_count, assistant_message_count,
                tool_call_count, plan_message_count,
                plan_content_tokens_est_total, plan_api_tokens_total,
                api_coverage_message_count,
                content_tokens_est_total, content_tokens_est_user, content_tokens_est_assistant,
                api_tokens_total, api_input_tokens_total, api_output_tokens_total,
                api_cache_read_tokens_total, api_cache_creation_tokens_total, api_thinking_tokens_total,
                last_updated
             ) VALUES
                (?1, 'codex', 1, 'local',
                 10, 4, 6, 3, 1,
                 200, 400,
                 8,
                 1200, 500, 700,
                 1400, 700, 550, 100, 25, 25,
                 ?2)",
            frankensqlite::params![1000_i64, 1_i64],
        )
        .unwrap();

        conn.execute_compat(
            "INSERT INTO usage_hourly (
                hour_id, agent_slug, workspace_id, source_id,
                message_count, user_message_count, assistant_message_count,
                tool_call_count, plan_message_count,
                plan_content_tokens_est_total, plan_api_tokens_total,
                api_coverage_message_count,
                content_tokens_est_total, content_tokens_est_user, content_tokens_est_assistant,
                api_tokens_total, api_input_tokens_total, api_output_tokens_total,
                api_cache_read_tokens_total, api_cache_creation_tokens_total, api_thinking_tokens_total,
                last_updated
             ) VALUES
                (?1, 'codex', 1, 'local',
                 20, 9, 11, 5, 2,
                 400, 700,
                 17,
                 2200, 900, 1300,
                 2600, 1300, 1000, 200, 50, 50,
                 ?2)",
            frankensqlite::params![1001_i64, 2_i64],
        )
        .unwrap();
        conn
    }

    fn setup_tools_remote_source_fallback_db() -> Connection {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute_batch(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL
            );
             CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent_id INTEGER NOT NULL,
                workspace_id INTEGER,
                source_id TEXT NOT NULL,
                origin_host TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER
            );
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                created_at INTEGER,
                content TEXT NOT NULL
            );
             CREATE TABLE message_metrics (
                message_id INTEGER PRIMARY KEY,
                created_at_ms INTEGER,
                tool_call_count INTEGER NOT NULL DEFAULT 0,
                content_tokens_est INTEGER,
                api_input_tokens INTEGER,
                api_output_tokens INTEGER,
                api_cache_read_tokens INTEGER,
                api_cache_creation_tokens INTEGER,
                api_thinking_tokens INTEGER
            );
             CREATE TABLE usage_daily (
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
            );",
        )
        .unwrap();

        conn.execute("INSERT INTO agents (id, slug) VALUES (1, 'codex')")
            .unwrap();
        conn.execute("INSERT INTO agents (id, slug) VALUES (2, 'claude_code')")
            .unwrap();

        conn.execute(
            "INSERT INTO conversations
             (id, agent_id, workspace_id, source_id, origin_host, source_path, started_at)
             VALUES (1, 1, 1, 'local', '', '/sessions/local.jsonl', 1700000000000)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations
             (id, agent_id, workspace_id, source_id, origin_host, source_path, started_at)
             VALUES (2, 2, 2, '   ', 'remote-ci', '/sessions/remote.jsonl', 1700000001000)",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, created_at, content)
             VALUES (11, 1, 0, 'assistant', 1700000000000, 'local tool')",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, created_at, content)
             VALUES (21, 2, 0, 'assistant', 1700000001000, 'remote tool')",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO message_metrics
             (message_id, created_at_ms, tool_call_count, content_tokens_est,
              api_input_tokens, api_output_tokens, api_cache_read_tokens,
              api_cache_creation_tokens, api_thinking_tokens)
             VALUES (11, 1700000000000, 2, 30, 10, 20, 0, 0, 0)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_metrics
             (message_id, created_at_ms, tool_call_count, content_tokens_est,
              api_input_tokens, api_output_tokens, api_cache_read_tokens,
              api_cache_creation_tokens, api_thinking_tokens)
             VALUES (21, 1700000001000, 7, 90, 30, 70, 0, 0, 0)",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO usage_daily
             (day_id, agent_slug, workspace_id, source_id, message_count,
              assistant_message_count, tool_call_count, content_tokens_est_total,
              content_tokens_est_assistant, api_tokens_total, api_input_tokens_total,
              api_output_tokens_total, last_updated)
             VALUES (20250, 'codex', 1, 'local', 1, 1, 2, 30, 30, 30, 10, 20, 1)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage_daily
             (day_id, agent_slug, workspace_id, source_id, message_count,
              assistant_message_count, tool_call_count, content_tokens_est_total,
              content_tokens_est_assistant, api_tokens_total, api_input_tokens_total,
              api_output_tokens_total, last_updated)
             VALUES (20250, 'claude_code', 2, '   ', 1, 1, 7, 90, 90, 100, 30, 70, 1)",
        )
        .unwrap();

        conn
    }

    /// Create an in-memory database with the token_daily_stats schema and seed data.
    fn setup_token_daily_stats_db() -> Connection {
        let conn = Connection::open(":memory:").unwrap();
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
        conn.execute_compat(
            "INSERT INTO token_daily_stats VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            frankensqlite::params![20250, "claude_code", "local", "opus", 80, 40, 40, 5, 30000, 25000, 3000, 1500, 500, 60000, 160000, 20, 1.50, 3, now],
        ).unwrap();
        conn.execute_compat(
            "INSERT INTO token_daily_stats VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            frankensqlite::params![20250, "claude_code", "local", "sonnet", 40, 20, 20, 2, 10000, 8000, 1000, 500, 200, 19700, 80000, 8, 0.40, 2, now],
        ).unwrap();
        conn.execute_compat(
            "INSERT INTO token_daily_stats VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            frankensqlite::params![20250, "codex", "local", "gpt-4o", 50, 25, 25, 3, 15000, 12000, 2000, 800, 0, 29800, 100000, 10, 0.80, 1, now],
        ).unwrap();

        conn
    }

    fn setup_status_freshness_db(
        hourly_last_updated: i64,
        track_b_last_updated: i64,
    ) -> Connection {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_hourly (
                hour_id INTEGER NOT NULL,
                last_updated INTEGER NOT NULL
            );
            CREATE TABLE token_daily_stats (
                day_id INTEGER NOT NULL,
                last_updated INTEGER NOT NULL
            );",
        )
        .unwrap();

        conn.execute_compat(
            "INSERT INTO usage_hourly (hour_id, last_updated) VALUES (?1, ?2)",
            frankensqlite::params![123_i64, hourly_last_updated],
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO token_daily_stats (day_id, last_updated) VALUES (?1, ?2)",
            frankensqlite::params![456_i64, track_b_last_updated],
        )
        .unwrap();

        conn
    }

    fn setup_session_scatter_db() -> Connection {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute_batch(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL
            );
             CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent_id INTEGER NOT NULL,
                workspace_id INTEGER,
                source_id TEXT NOT NULL,
                origin_host TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER,
                grand_total_tokens INTEGER
            );
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                created_at INTEGER,
                content TEXT NOT NULL
            );
             CREATE TABLE message_metrics (
                message_id INTEGER PRIMARY KEY,
                api_input_tokens INTEGER,
                api_output_tokens INTEGER,
                api_cache_read_tokens INTEGER,
                api_cache_creation_tokens INTEGER,
                api_thinking_tokens INTEGER
            );",
        )
        .unwrap();

        conn.execute("INSERT INTO agents (id, slug) VALUES (1, 'codex')")
            .unwrap();
        conn.execute("INSERT INTO agents (id, slug) VALUES (2, 'claude_code')")
            .unwrap();

        conn.execute(
            "INSERT INTO conversations
             (id, agent_id, workspace_id, source_id, source_path, started_at, grand_total_tokens)
             VALUES (1, 1, 10, 'local', '/sessions/a.jsonl', 1700000000000, 1000)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations
             (id, agent_id, workspace_id, source_id, source_path, started_at, grand_total_tokens)
             VALUES (2, 2, 20, 'remote-ci', '/sessions/b.jsonl', 1700000000000, 2300)",
        )
        .unwrap();

        // Session A: 2 messages, total api tokens = 1000.
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, created_at, content)
             VALUES (11, 1, 0, 'user', 1700000001000, 'a1')",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, created_at, content)
             VALUES (12, 1, 1, 'assistant', 1700000002000, 'a2')",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_metrics
             (message_id, api_input_tokens, api_output_tokens, api_cache_read_tokens, api_cache_creation_tokens, api_thinking_tokens)
             VALUES (11, 200, 250, 0, 0, 50)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_metrics
             (message_id, api_input_tokens, api_output_tokens, api_cache_read_tokens, api_cache_creation_tokens, api_thinking_tokens)
             VALUES (12, 200, 300, 0, 0, 0)",
        )
        .unwrap();

        // Session B: 3 messages, total api tokens = 2300.
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, created_at, content)
             VALUES (21, 2, 0, 'user', 1700000001000, 'b1')",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, created_at, content)
             VALUES (22, 2, 1, 'assistant', 1700000002000, 'b2')",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, created_at, content)
             VALUES (23, 2, 2, 'assistant', 1700000003000, 'b3')",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_metrics
             (message_id, api_input_tokens, api_output_tokens, api_cache_read_tokens, api_cache_creation_tokens, api_thinking_tokens)
             VALUES (21, 300, 500, 0, 0, 0)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_metrics
             (message_id, api_input_tokens, api_output_tokens, api_cache_read_tokens, api_cache_creation_tokens, api_thinking_tokens)
             VALUES (22, 500, 500, 0, 0, 0)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_metrics
             (message_id, api_input_tokens, api_output_tokens, api_cache_read_tokens, api_cache_creation_tokens, api_thinking_tokens)
             VALUES (23, 200, 300, 0, 0, 0)",
        )
        .unwrap();

        conn
    }

    fn setup_session_scatter_with_token_usage_fallback_db() -> Connection {
        let conn = setup_session_scatter_db();
        conn.execute_batch(
            "CREATE TABLE token_usage (
                message_id INTEGER PRIMARY KEY,
                total_tokens INTEGER
            );",
        )
        .unwrap();

        // Keep message 11 with concrete API split from message_metrics.
        conn.execute("INSERT INTO token_usage (message_id, total_tokens) VALUES (11, 999)")
            .unwrap();
        // Message 12 has message_metrics row but no API split; token_usage should be used.
        conn.execute(
            "UPDATE message_metrics
             SET api_input_tokens = NULL,
                 api_output_tokens = NULL,
                 api_cache_read_tokens = NULL,
                 api_cache_creation_tokens = NULL,
                 api_thinking_tokens = NULL
             WHERE message_id = 12",
        )
        .unwrap();
        conn.execute("INSERT INTO token_usage (message_id, total_tokens) VALUES (12, 900)")
            .unwrap();

        conn
    }

    fn setup_session_scatter_with_api_source_column_db() -> Connection {
        let conn = setup_session_scatter_with_token_usage_fallback_db();
        conn.execute("ALTER TABLE message_metrics ADD COLUMN api_data_source TEXT")
            .unwrap();
        // Mark only session A rows as explicit API rows; keep session B rows NULL
        // to simulate legacy records after schema migration.
        conn.execute(
            "UPDATE message_metrics
             SET api_data_source = 'api'
             WHERE message_id IN (11, 12)",
        )
        .unwrap();
        conn
    }

    fn setup_duplicate_message_metrics_raw_db() -> Connection {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute_batch(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL
            );
             CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent_id INTEGER NOT NULL,
                workspace_id INTEGER,
                source_id TEXT NOT NULL,
                origin_host TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER,
                grand_total_tokens INTEGER
            );
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                created_at INTEGER,
                content TEXT NOT NULL
            );
             CREATE TABLE message_metrics (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                created_at_ms INTEGER,
                tool_call_count INTEGER,
                content_tokens_est INTEGER,
                api_input_tokens INTEGER,
                api_output_tokens INTEGER,
                api_cache_read_tokens INTEGER,
                api_cache_creation_tokens INTEGER,
                api_thinking_tokens INTEGER,
                api_data_source TEXT,
                has_plan INTEGER
            );",
        )
        .unwrap();

        conn.execute("INSERT INTO agents (id, slug) VALUES (1, 'codex')")
            .unwrap();
        conn.execute(
            "INSERT INTO conversations
             (id, agent_id, workspace_id, source_id, source_path, started_at, grand_total_tokens)
             VALUES (1, 1, 10, 'local', '/sessions/dup.jsonl', 1700000000000, 1200)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, created_at, content)
             VALUES (11, 1, 0, 'user', 1700000001000, 'dup-a')",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, idx, role, created_at, content)
             VALUES (12, 1, 1, 'assistant', 1700000002000, 'dup-b')",
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO message_metrics
                 (id, message_id, created_at_ms, tool_call_count, content_tokens_est,
                  api_input_tokens, api_output_tokens, api_cache_read_tokens,
                  api_cache_creation_tokens, api_thinking_tokens, api_data_source, has_plan)
             VALUES
                 (1, 11, 1700000001000, 3, 100, 200, 300, 0, 0, 0, 'api', 1),
                 (2, 11, 1700000001000, 3, 100, 200, 300, 0, 0, 0, 'api', 1),
                 (3, 12, 1700000002000, 4, 120, 250, 450, 0, 0, 0, 'api', 0);",
        )
        .unwrap();

        conn
    }

    fn setup_status_filter_db() -> Connection {
        let conn = Connection::open(":memory:").unwrap();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        let day11_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(11);
        conn.execute_batch(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL
            );
             CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent_id INTEGER NOT NULL,
                workspace_id INTEGER,
                source_id TEXT NOT NULL,
                source_path TEXT NOT NULL,
                started_at INTEGER
            );
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                created_at INTEGER,
                content TEXT NOT NULL
            );
             CREATE TABLE message_metrics (
                message_id INTEGER PRIMARY KEY,
                created_at_ms INTEGER NOT NULL,
                hour_id INTEGER NOT NULL,
                day_id INTEGER NOT NULL,
                agent_slug TEXT NOT NULL,
                workspace_id INTEGER NOT NULL DEFAULT 0,
                source_id TEXT NOT NULL DEFAULT 'local',
                role TEXT NOT NULL,
                content_chars INTEGER NOT NULL,
                content_tokens_est INTEGER NOT NULL,
                api_input_tokens INTEGER,
                api_output_tokens INTEGER,
                api_cache_read_tokens INTEGER,
                api_cache_creation_tokens INTEGER,
                api_thinking_tokens INTEGER,
                api_data_source TEXT NOT NULL DEFAULT 'estimated'
            );
             CREATE TABLE usage_hourly (
                hour_id INTEGER NOT NULL,
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
                PRIMARY KEY (hour_id, agent_slug, workspace_id, source_id)
            );
             CREATE TABLE usage_daily (
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
             CREATE TABLE token_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                message_id INTEGER NOT NULL,
                conversation_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                workspace_id INTEGER,
                source_id TEXT NOT NULL DEFAULT 'local',
                timestamp_ms INTEGER NOT NULL,
                day_id INTEGER NOT NULL,
                model_name TEXT,
                model_family TEXT,
                total_tokens INTEGER,
                data_source TEXT NOT NULL DEFAULT 'api'
            );
             CREATE TABLE token_daily_stats (
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

        conn.execute("INSERT INTO agents (id, slug) VALUES (1, 'codex')")
            .unwrap();
        conn.execute("INSERT INTO agents (id, slug) VALUES (2, 'claude_code')")
            .unwrap();

        conn.execute(&format!(
            "INSERT INTO conversations (id, agent_id, workspace_id, source_id, source_path, started_at)
             VALUES (1, 1, 1, 'local', '/sessions/a.jsonl', {day10_ms})"
        ))
        .unwrap();
        conn.execute(&format!(
            "INSERT INTO conversations (id, agent_id, workspace_id, source_id, source_path, started_at)
             VALUES (2, 2, 2, 'remote-ci', '/sessions/b.jsonl', {day11_ms})"
        ))
        .unwrap();

        conn.execute(&format!(
            "INSERT INTO messages (id, conversation_id, idx, role, created_at, content)
             VALUES (11, 1, 0, 'user', {}, 'a1')",
            day10_ms + 100,
        ))
        .unwrap();
        conn.execute(&format!(
            "INSERT INTO messages (id, conversation_id, idx, role, created_at, content)
             VALUES (12, 1, 1, 'assistant', {}, 'a2')",
            day10_ms + 200,
        ))
        .unwrap();
        conn.execute(&format!(
            "INSERT INTO messages (id, conversation_id, idx, role, created_at, content)
             VALUES (21, 2, 0, 'assistant', {}, 'b1')",
            day11_ms + 100,
        ))
        .unwrap();

        conn.execute_batch(
            &format!(
                "INSERT INTO message_metrics
                    (message_id, created_at_ms, hour_id, day_id, agent_slug, workspace_id, source_id,
                     role, content_chars, content_tokens_est, api_input_tokens, api_output_tokens,
                     api_cache_read_tokens, api_cache_creation_tokens, api_thinking_tokens, api_data_source)
                 VALUES
                    (11, {day10_ms} + 100, 240, 10, 'codex', 1, 'local', 'user', 10, 3, 5, 7, 0, 0, 0, 'api'),
                    (12, {day10_ms} + 200, 240, 10, 'codex', 1, 'local', 'assistant', 12, 4, 8, 9, 0, 0, 0, 'api'),
                    (21, {day11_ms} + 100, 264, 11, 'claude_code', 2, 'remote-ci', 'assistant', 14, 5, NULL, NULL, NULL, NULL, NULL, 'estimated');
                 INSERT INTO usage_hourly
                    (hour_id, agent_slug, workspace_id, source_id, message_count, user_message_count,
                     assistant_message_count, api_coverage_message_count, api_tokens_total, last_updated)
                 VALUES
                    (240, 'codex', 1, 'local', 2, 1, 1, 2, 29, {now_ms}),
                    (264, 'claude_code', 2, 'remote-ci', 1, 0, 1, 0, 0, {now_ms});
                 INSERT INTO usage_daily
                    (day_id, agent_slug, workspace_id, source_id, message_count, user_message_count,
                     assistant_message_count, api_coverage_message_count, api_tokens_total, last_updated)
                 VALUES
                    (10, 'codex', 1, 'local', 2, 1, 1, 2, 29, {now_ms}),
                    (11, 'claude_code', 2, 'remote-ci', 1, 0, 1, 0, 0, {now_ms});
                 INSERT INTO token_usage
                    (message_id, conversation_id, agent_id, workspace_id, source_id, timestamp_ms, day_id,
                     model_name, model_family, total_tokens, data_source)
                 VALUES
                    (11, 1, 1, 1, 'local', {day10_ms} + 100, 10, 'gpt-4o-mini', 'gpt-4o', 12, 'api'),
                    (12, 1, 1, 1, 'local', {day10_ms} + 200, 10, 'gpt-4o-mini', 'gpt-4o', 17, 'api'),
                    (21, 2, 2, 2, 'remote-ci', {day11_ms} + 100, 11, NULL, 'claude', 11, 'estimated');
                 INSERT INTO token_daily_stats
                    (day_id, agent_slug, source_id, model_family, api_call_count, user_message_count,
                     assistant_message_count, grand_total_tokens, session_count, last_updated)
                 VALUES
                    (10, 'codex', 'local', 'gpt-4o', 2, 1, 1, 29, 1, {now_ms}),
                    (11, 'claude_code', 'remote-ci', 'claude', 0, 0, 1, 11, 1, {now_ms});"
            ),
        )
        .unwrap();

        conn
    }

    #[test]
    fn normalize_epoch_millis_preserves_negative_millisecond_values() {
        assert_eq!(normalize_epoch_millis(-1_000), -1_000);
        assert_eq!(normalize_epoch_millis(-86_400_000), -86_400_000);
        assert_eq!(normalize_epoch_millis(1_700_000_000), 1_700_000_000_000);
    }

    fn setup_legacy_status_filter_db_without_message_metrics_created_at() -> Connection {
        let conn = Connection::open(":memory:").unwrap();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        conn.execute_batch(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL
            );
             CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent_id INTEGER NOT NULL,
                workspace_id INTEGER,
                source_id TEXT NOT NULL,
                source_path TEXT NOT NULL,
                started_at INTEGER
            );
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                created_at INTEGER,
                content TEXT NOT NULL
            );
             CREATE TABLE message_metrics (
                message_id INTEGER PRIMARY KEY,
                hour_id INTEGER NOT NULL,
                day_id INTEGER NOT NULL,
                agent_slug TEXT NOT NULL,
                workspace_id INTEGER NOT NULL DEFAULT 0,
                source_id TEXT NOT NULL DEFAULT 'local',
                role TEXT NOT NULL,
                content_chars INTEGER NOT NULL,
                content_tokens_est INTEGER NOT NULL,
                api_input_tokens INTEGER,
                api_output_tokens INTEGER,
                api_cache_read_tokens INTEGER,
                api_cache_creation_tokens INTEGER,
                api_thinking_tokens INTEGER,
                api_data_source TEXT NOT NULL DEFAULT 'estimated'
            );
             CREATE TABLE usage_hourly (
                hour_id INTEGER NOT NULL,
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
                PRIMARY KEY (hour_id, agent_slug, workspace_id, source_id)
            );
             CREATE TABLE usage_daily (
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
            );",
        )
        .unwrap();

        conn.execute("INSERT INTO agents (id, slug) VALUES (1, 'codex')")
            .unwrap();
        conn.execute(&format!(
            "INSERT INTO conversations (id, agent_id, workspace_id, source_id, source_path, started_at)
             VALUES (1, 1, 1, 'local', '/sessions/legacy-a.jsonl', {day10_ms})"
        ))
        .unwrap();
        conn.execute(&format!(
            "INSERT INTO messages (id, conversation_id, idx, role, created_at, content)
             VALUES (11, 1, 0, 'user', {}, 'legacy-a1')",
            day10_ms + 100,
        ))
        .unwrap();
        conn.execute(&format!(
            "INSERT INTO messages (id, conversation_id, idx, role, created_at, content)
             VALUES (12, 1, 1, 'assistant', {}, 'legacy-a2')",
            day10_ms + 200,
        ))
        .unwrap();

        conn.execute_batch(
            &format!(
                "INSERT INTO message_metrics
                    (message_id, hour_id, day_id, agent_slug, workspace_id, source_id,
                     role, content_chars, content_tokens_est, api_input_tokens, api_output_tokens,
                     api_cache_read_tokens, api_cache_creation_tokens, api_thinking_tokens, api_data_source)
                 VALUES
                    (11, 240, 10, 'codex', 1, 'local', 'user', 10, 3, 5, 7, 0, 0, 0, 'api'),
                    (12, 240, 10, 'codex', 1, 'local', 'assistant', 12, 4, 8, 9, 0, 0, 0, 'api');
                 INSERT INTO usage_hourly
                    (hour_id, agent_slug, workspace_id, source_id, message_count, user_message_count,
                     assistant_message_count, api_coverage_message_count, api_tokens_total, last_updated)
                 VALUES
                    (240, 'codex', 1, 'local', 2, 1, 1, 2, 29, {now_ms});
                 INSERT INTO usage_daily
                    (day_id, agent_slug, workspace_id, source_id, message_count, user_message_count,
                     assistant_message_count, api_coverage_message_count, api_tokens_total, last_updated)
                 VALUES
                    (10, 'codex', 1, 'local', 2, 1, 1, 2, 29, {now_ms});"
            ),
        )
        .unwrap();

        conn
    }

    fn setup_legacy_track_b_filter_db_without_token_usage_timestamp() -> Connection {
        let conn = setup_legacy_status_filter_db_without_message_metrics_created_at();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);

        conn.execute_batch(
            "CREATE TABLE token_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                message_id INTEGER NOT NULL,
                conversation_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                workspace_id INTEGER,
                source_id TEXT NOT NULL DEFAULT 'local',
                day_id INTEGER NOT NULL,
                model_name TEXT,
                model_family TEXT,
                total_tokens INTEGER,
                estimated_cost_usd REAL NOT NULL DEFAULT 0.0,
                data_source TEXT NOT NULL DEFAULT 'api'
            );
             CREATE TABLE token_daily_stats (
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

        conn.execute_batch(
            &format!(
                "INSERT INTO token_usage
                    (message_id, conversation_id, agent_id, workspace_id, source_id, day_id,
                     model_name, model_family, total_tokens, estimated_cost_usd, data_source)
                 VALUES
                    (11, 1, 1, 1, 'local', 10, 'gpt-4o-mini', 'gpt-4o', 12, 0.12, 'api'),
                    (12, 1, 1, 1, 'local', 10, 'gpt-4o-mini', 'gpt-4o', 17, 0.17, 'api');
                 INSERT INTO token_daily_stats
                    (day_id, agent_slug, source_id, model_family, api_call_count, user_message_count,
                     assistant_message_count, grand_total_tokens, estimated_cost_usd, session_count, last_updated)
                 VALUES
                    (10, 'codex', 'local', 'gpt-4o', 2, 1, 1, 29, 0.29, 1, {now_ms});
                 UPDATE conversations SET started_at = {day10_ms} + 100 WHERE id = 1;"
            ),
        )
        .unwrap();

        conn
    }

    #[test]
    fn query_status_treats_millisecond_timestamps_as_fresh() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let conn = setup_status_freshness_db(now_ms - 1_000, now_ms - 2_000);

        let result = query_status(&conn, &AnalyticsFilter::default()).unwrap();

        assert!(result.drift.track_a_fresh);
        assert!(result.drift.track_b_fresh);
        assert_eq!(result.recommended_action, "none");
    }

    #[test]
    fn query_status_supports_legacy_second_timestamps() {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let conn = setup_status_freshness_db(now_secs - 5, now_secs - 10);

        let result = query_status(&conn, &AnalyticsFilter::default()).unwrap();

        assert!(result.drift.track_a_fresh);
        assert!(result.drift.track_b_fresh);
    }

    #[test]
    fn query_status_detects_millisecond_freshness_mismatch() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let stale_ms = now_ms - (3 * 86_400_000);
        let conn = setup_status_freshness_db(now_ms - 1_000, stale_ms);

        let result = query_status(&conn, &AnalyticsFilter::default()).unwrap();

        assert!(result.drift.track_a_fresh);
        assert!(!result.drift.track_b_fresh);
        assert_eq!(result.recommended_action, "rebuild_track_b");
        assert!(
            result
                .drift
                .signals
                .iter()
                .any(|signal| signal.signal == "track_freshness_mismatch")
        );
    }

    #[test]
    fn query_status_deduplicates_duplicate_token_usage_rows_in_coverage() {
        let conn = setup_status_filter_db();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        conn.execute(&format!(
            "INSERT INTO token_usage
                (message_id, conversation_id, agent_id, workspace_id, source_id, timestamp_ms, day_id,
                 model_name, model_family, total_tokens, data_source)
             VALUES
                (11, 1, 1, 1, 'local', {}, 10, NULL, 'gpt-4o', 12, 'estimated')",
            day10_ms + 100
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            since_ms: Some(day10_ms),
            until_ms: Some(day10_ms + 1_000),
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            workspace_ids: vec![1],
        };

        let result = query_status(&conn, &filter).unwrap();

        assert_eq!(status_table_row_count(&result, "token_usage"), 2);
        assert_eq!(result.coverage.model_name_coverage_pct, 100.0);
        assert_eq!(result.coverage.estimate_only_pct, 0.0);
    }

    #[test]
    fn query_status_blank_duplicate_token_usage_data_source_does_not_override_estimated() {
        let conn = setup_status_filter_db();
        let day11_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(11);
        conn.execute("UPDATE token_usage SET data_source = 'estimated' WHERE message_id = 21")
            .unwrap();
        conn.execute(&format!(
            "INSERT INTO token_usage
                (message_id, conversation_id, agent_id, workspace_id, source_id, timestamp_ms, day_id,
                 model_name, model_family, total_tokens, data_source)
             VALUES
                (21, 2, 2, 2, 'remote-ci', {}, 11, NULL, 'claude', 11, '   ')",
            day11_ms + 100
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            agents: vec!["claude_code".into()],
            source: SourceFilter::Specific("remote-ci".into()),
            workspace_ids: vec![2],
            ..Default::default()
        };

        let result = query_status(&conn, &filter).unwrap();

        assert_eq!(status_table_row_count(&result, "token_usage"), 1);
        assert_eq!(result.coverage.total_messages, 1);
        assert_eq!(result.coverage.estimate_only_pct, 100.0);
    }

    fn status_table_row_count(result: &StatusResult, table: &str) -> i64 {
        result
            .tables
            .iter()
            .find(|info| info.table == table)
            .map(|info| info.row_count)
            .unwrap_or(-1)
    }

    #[test]
    fn query_status_applies_dimensional_filters_to_tables_and_coverage() {
        let conn = setup_status_filter_db();
        let filter = AnalyticsFilter {
            since_ms: Some(crate::storage::sqlite::FrankenStorage::millis_from_day_id(
                10,
            )),
            until_ms: Some(crate::storage::sqlite::FrankenStorage::millis_from_day_id(10) + 1_000),
            agents: vec!["  codex  ".into()],
            source: SourceFilter::Specific("  LOCAL  ".into()),
            workspace_ids: vec![1],
        };

        let result = query_status(&conn, &filter).unwrap();

        assert_eq!(result.coverage.total_messages, 2);
        assert_eq!(status_table_row_count(&result, "message_metrics"), 2);
        assert_eq!(status_table_row_count(&result, "usage_hourly"), 1);
        assert_eq!(status_table_row_count(&result, "usage_daily"), 1);
        assert_eq!(status_table_row_count(&result, "token_usage"), 2);
        assert_eq!(status_table_row_count(&result, "token_daily_stats"), 1);
        assert_eq!(result.coverage.message_metrics_coverage_pct, 100.0);
        assert_eq!(result.coverage.api_token_coverage_pct, 100.0);
        assert_eq!(result.coverage.model_name_coverage_pct, 100.0);
        assert_eq!(result.coverage.estimate_only_pct, 0.0);
        assert_eq!(result.recommended_action, "none");
    }

    #[test]
    fn query_status_subday_filter_excludes_same_day_rollup_rows_without_raw_matches() {
        let conn = setup_status_filter_db();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        conn.execute(&format!(
            "UPDATE messages SET created_at = {} WHERE conversation_id = 1",
            day10_ms + 10_000
        ))
        .unwrap();
        conn.execute(&format!(
            "UPDATE message_metrics SET created_at_ms = {} WHERE agent_slug = 'codex'",
            day10_ms + 10_000
        ))
        .unwrap();
        conn.execute(&format!(
            "UPDATE token_usage SET timestamp_ms = {} WHERE agent_id = 1",
            day10_ms + 10_000
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            since_ms: Some(day10_ms),
            until_ms: Some(day10_ms + 500),
            agents: vec!["codex".into()],
            ..Default::default()
        };

        let result = query_status(&conn, &filter).unwrap();

        assert_eq!(result.coverage.total_messages, 0);
        assert_eq!(status_table_row_count(&result, "message_metrics"), 0);
        assert_eq!(status_table_row_count(&result, "usage_hourly"), 0);
        assert_eq!(status_table_row_count(&result, "usage_daily"), 0);
        assert_eq!(status_table_row_count(&result, "token_usage"), 0);
        assert_eq!(status_table_row_count(&result, "token_daily_stats"), 0);
        assert_eq!(result.recommended_action, "none");
    }

    #[test]
    fn query_status_uses_exact_raw_timestamps_for_subday_coverage_counts() {
        let conn = setup_status_filter_db();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        conn.execute(&format!(
            "UPDATE messages SET created_at = {} WHERE id = 12",
            day10_ms + 10_000
        ))
        .unwrap();
        conn.execute(&format!(
            "UPDATE message_metrics SET created_at_ms = {} WHERE message_id = 12",
            day10_ms + 10_000
        ))
        .unwrap();
        conn.execute(&format!(
            "UPDATE token_usage SET timestamp_ms = {}, model_name = NULL, data_source = 'estimated' WHERE message_id = 12",
            day10_ms + 10_000
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            since_ms: Some(day10_ms),
            until_ms: Some(day10_ms + 500),
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            workspace_ids: vec![1],
        };

        let result = query_status(&conn, &filter).unwrap();

        assert_eq!(result.coverage.total_messages, 1);
        assert_eq!(status_table_row_count(&result, "message_metrics"), 1);
        assert_eq!(status_table_row_count(&result, "token_usage"), 1);
        assert_eq!(result.coverage.message_metrics_coverage_pct, 100.0);
        assert_eq!(result.coverage.api_token_coverage_pct, 100.0);
        assert_eq!(result.coverage.model_name_coverage_pct, 100.0);
        assert_eq!(result.coverage.estimate_only_pct, 0.0);
    }

    #[test]
    fn query_status_uses_message_metrics_timestamp_when_message_created_at_missing() {
        let conn = setup_status_filter_db();
        conn.execute("UPDATE messages SET created_at = NULL WHERE conversation_id = 1")
            .unwrap();

        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        let filter = AnalyticsFilter {
            since_ms: Some(day10_ms),
            until_ms: Some(day10_ms + 86_399_999),
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            workspace_ids: vec![1],
        };

        let result = query_status(&conn, &filter).unwrap();

        assert_eq!(result.coverage.total_messages, 2);
        assert_eq!(status_table_row_count(&result, "message_metrics"), 2);
        assert_eq!(result.coverage.message_metrics_coverage_pct, 100.0);
    }

    #[test]
    fn query_status_uses_message_created_at_when_message_metrics_timestamp_column_is_missing() {
        let conn = setup_legacy_status_filter_db_without_message_metrics_created_at();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        conn.execute(&format!(
            "UPDATE messages SET created_at = {} WHERE conversation_id = 1",
            day10_ms + 10_000
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            since_ms: Some(day10_ms),
            until_ms: Some(day10_ms + 500),
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            workspace_ids: vec![1],
        };

        let result = query_status(&conn, &filter).unwrap();

        assert_eq!(result.coverage.total_messages, 0);
        assert_eq!(status_table_row_count(&result, "message_metrics"), 0);
        assert_eq!(status_table_row_count(&result, "usage_hourly"), 0);
        assert_eq!(status_table_row_count(&result, "usage_daily"), 0);
        assert_eq!(result.recommended_action, "none");
    }

    #[test]
    fn query_status_uses_conversation_started_at_when_message_metrics_timestamp_column_is_missing()
    {
        let conn = setup_legacy_status_filter_db_without_message_metrics_created_at();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        conn.execute("UPDATE messages SET created_at = NULL WHERE conversation_id = 1")
            .unwrap();
        conn.execute(&format!(
            "UPDATE conversations SET started_at = {} WHERE id = 1",
            day10_ms + 10_000
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            since_ms: Some(day10_ms),
            until_ms: Some(day10_ms + 500),
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            workspace_ids: vec![1],
        };

        let result = query_status(&conn, &filter).unwrap();

        assert_eq!(result.coverage.total_messages, 0);
        assert_eq!(status_table_row_count(&result, "message_metrics"), 0);
        assert_eq!(status_table_row_count(&result, "usage_hourly"), 0);
        assert_eq!(status_table_row_count(&result, "usage_daily"), 0);
        assert_eq!(result.recommended_action, "none");
    }

    #[test]
    fn query_status_uses_conversation_started_at_when_token_usage_timestamp_column_is_missing() {
        let conn = setup_legacy_track_b_filter_db_without_token_usage_timestamp();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        conn.execute(&format!(
            "UPDATE conversations SET started_at = {} WHERE id = 1",
            day10_ms + 10_000
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            since_ms: Some(day10_ms),
            until_ms: Some(day10_ms + 500),
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            workspace_ids: vec![1],
        };

        let result = query_status(&conn, &filter).unwrap();

        assert_eq!(status_table_row_count(&result, "token_usage"), 0);
        assert_eq!(status_table_row_count(&result, "token_daily_stats"), 0);
        assert_eq!(result.coverage.model_name_coverage_pct, 0.0);
        assert_eq!(result.coverage.estimate_only_pct, 0.0);
        assert_eq!(result.recommended_action, "rebuild_track_b");
    }

    #[test]
    fn query_cost_timeseries_uses_conversation_started_at_when_token_usage_timestamp_column_is_missing()
     {
        let conn = setup_legacy_track_b_filter_db_without_token_usage_timestamp();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        conn.execute(&format!(
            "UPDATE conversations SET started_at = {} WHERE id = 1",
            day10_ms + 10_000
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            since_ms: Some(day10_ms),
            until_ms: Some(day10_ms + 500),
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            workspace_ids: vec![1],
        };

        let result = query_cost_timeseries(&conn, &filter, GroupBy::Hour).unwrap();

        assert_eq!(result.source_table, "token_usage");
        assert!(result.buckets.is_empty());
        assert_eq!(result.totals.api_tokens_total, 0);
        assert_eq!(result.totals.estimated_cost_usd, 0.0);
    }

    #[test]
    fn query_breakdown_model_api_total_uses_conversation_started_at_when_token_usage_timestamp_column_is_missing()
     {
        let conn = setup_legacy_track_b_filter_db_without_token_usage_timestamp();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        conn.execute(&format!(
            "UPDATE conversations SET started_at = {} WHERE id = 1",
            day10_ms + 10_000
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            since_ms: Some(day10_ms),
            until_ms: Some(day10_ms + 500),
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            workspace_ids: vec![1],
        };

        let result = query_breakdown(&conn, &filter, Dim::Model, Metric::ApiTotal, 10).unwrap();

        assert_eq!(result.source_table, "token_usage");
        assert!(result.rows.is_empty());
    }

    #[test]
    fn query_status_unknown_workspace_filter_returns_empty_subset() {
        let conn = setup_status_filter_db();
        let filter = AnalyticsFilter {
            workspace_ids: vec![999],
            ..Default::default()
        };

        let result = query_status(&conn, &filter).unwrap();

        assert_eq!(result.coverage.total_messages, 0);
        assert_eq!(status_table_row_count(&result, "message_metrics"), 0);
        assert_eq!(status_table_row_count(&result, "usage_hourly"), 0);
        assert_eq!(status_table_row_count(&result, "usage_daily"), 0);
        assert_eq!(status_table_row_count(&result, "token_usage"), 0);
        assert_eq!(status_table_row_count(&result, "token_daily_stats"), 0);
        assert!(result.drift.signals.is_empty());
        assert_eq!(result.recommended_action, "none");
    }

    #[test]
    fn query_status_source_filter_matches_blank_remote_raw_source_ids_via_origin_host() {
        let conn = setup_status_filter_db();
        conn.execute("ALTER TABLE conversations ADD COLUMN origin_host TEXT")
            .unwrap();
        conn.execute(
            "UPDATE conversations SET source_id = '   ', origin_host = 'remote-ci' WHERE id = 2",
        )
        .unwrap();
        conn.execute(
            "UPDATE message_metrics SET source_id = '   ' WHERE agent_slug = 'claude_code'",
        )
        .unwrap();
        conn.execute("UPDATE usage_hourly SET source_id = '   ' WHERE agent_slug = 'claude_code'")
            .unwrap();
        conn.execute("UPDATE usage_daily SET source_id = '   ' WHERE agent_slug = 'claude_code'")
            .unwrap();
        conn.execute("UPDATE token_usage SET source_id = '   ' WHERE conversation_id = 2")
            .unwrap();
        conn.execute(
            "UPDATE token_daily_stats SET source_id = '   ' WHERE agent_slug = 'claude_code'",
        )
        .unwrap();

        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            ..Default::default()
        };
        let result = query_status(&conn, &filter).unwrap();

        assert_eq!(result.coverage.total_messages, 1);
        assert_eq!(status_table_row_count(&result, "message_metrics"), 1);
        assert_eq!(status_table_row_count(&result, "usage_hourly"), 1);
        assert_eq!(status_table_row_count(&result, "usage_daily"), 1);
        assert_eq!(status_table_row_count(&result, "token_usage"), 1);
        assert_eq!(status_table_row_count(&result, "token_daily_stats"), 1);
        assert_eq!(result.recommended_action, "none");
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
    fn query_breakdown_by_agent_coalesces_trimmed_and_blank_agent_slugs() {
        let conn = setup_usage_daily_db();
        conn.execute("UPDATE usage_daily SET agent_slug = '  codex  ' WHERE agent_slug = 'codex'")
            .unwrap();
        conn.execute("UPDATE usage_daily SET agent_slug = '   ' WHERE agent_slug = 'aider'")
            .unwrap();

        let result = query_breakdown(
            &conn,
            &AnalyticsFilter::default(),
            Dim::Agent,
            Metric::ToolCalls,
            10,
        )
        .unwrap();

        let codex = result.rows.iter().find(|row| row.key == "codex").unwrap();
        assert_eq!(codex.bucket.tool_call_count, 25);

        let unknown = result.rows.iter().find(|row| row.key == "unknown").unwrap();
        assert_eq!(unknown.bucket.tool_call_count, 5);
    }

    #[test]
    fn query_breakdown_by_agent_coverage_pct_orders_by_coverage_before_limit() {
        let conn = setup_usage_daily_db();
        conn.execute(
            "UPDATE usage_daily
             SET api_coverage_message_count = CASE agent_slug
                 WHEN 'claude_code' THEN 10
                 WHEN 'codex' THEN message_count
                 ELSE api_coverage_message_count
             END",
        )
        .unwrap();

        let result = query_breakdown(
            &conn,
            &AnalyticsFilter::default(),
            Dim::Agent,
            Metric::CoveragePct,
            1,
        )
        .unwrap();

        assert_eq!(result.source_table, "usage_daily");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "codex");
        assert_eq!(result.rows[0].value, 100);
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
    fn query_breakdown_by_source_specific_filter_applies_before_limit_on_track_a_rollup() {
        let conn = setup_usage_daily_db();
        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote".into()),
            ..Default::default()
        };
        let result = query_breakdown(&conn, &filter, Dim::Source, Metric::MessageCount, 1).unwrap();

        assert_eq!(result.source_table, "usage_daily");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "remote");
        assert_eq!(result.rows[0].value, 30);
    }

    #[test]
    fn query_breakdown_by_source_coalesces_trimmed_local_ids() {
        let conn = setup_usage_daily_db();
        conn.execute_compat(
            "INSERT INTO usage_daily (day_id, agent_slug, workspace_id, source_id,
                message_count, user_message_count, assistant_message_count,
                tool_call_count, plan_message_count, api_coverage_message_count,
                content_tokens_est_total, content_tokens_est_user, content_tokens_est_assistant,
                api_tokens_total, api_input_tokens_total, api_output_tokens_total,
                api_cache_read_tokens_total, api_cache_creation_tokens_total,
                api_thinking_tokens_total)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            frankensqlite::params![
                20252,
                "cursor",
                3,
                "  LOCAL  ",
                5,
                2,
                3,
                1,
                0,
                1,
                500,
                250,
                250,
                700,
                300,
                300,
                50,
                30,
                20
            ],
        )
        .unwrap();

        let result = query_breakdown(
            &conn,
            &AnalyticsFilter::default(),
            Dim::Source,
            Metric::MessageCount,
            10,
        )
        .unwrap();
        let local_rows: Vec<_> = result
            .rows
            .iter()
            .filter(|row| row.key == "local")
            .collect();

        assert_eq!(local_rows.len(), 1);
        assert_eq!(local_rows[0].value, 335);
    }

    #[test]
    fn query_breakdown_by_source_with_cost_metric_coalesces_trimmed_local_ids() {
        let conn = setup_token_daily_stats_db();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute_compat(
            "INSERT INTO token_daily_stats VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            frankensqlite::params![20251, "cursor", "  LOCAL  ", "sonnet", 10, 5, 5, 1, 1500, 1200, 0, 0, 0, 2700, 9000, 1, 0.25, 1, now],
        )
        .unwrap();

        let result = query_breakdown(
            &conn,
            &AnalyticsFilter::default(),
            Dim::Source,
            Metric::EstimatedCostUsd,
            10,
        )
        .unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "local");
        assert_eq!(result.rows[0].value, 3);
    }

    #[test]
    fn query_breakdown_by_source_specific_filter_applies_before_limit_on_track_b_rollup() {
        let conn = setup_token_daily_stats_db();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute_compat(
            "INSERT INTO token_daily_stats VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            frankensqlite::params![20250, "claude_code", "remote-ci", "sonnet", 5, 2, 3, 1, 1200, 900, 0, 0, 0, 2100, 6000, 1, 0.6, 1, now],
        )
        .unwrap();

        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            ..Default::default()
        };
        let result =
            query_breakdown(&conn, &filter, Dim::Source, Metric::EstimatedCostUsd, 1).unwrap();

        assert_eq!(result.source_table, "token_daily_stats");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "remote-ci");
        assert_eq!(result.rows[0].value, 1);
    }

    #[test]
    fn query_breakdown_by_source_message_count_recovers_blank_remote_usage_daily_source_via_origin_host()
     {
        let conn = setup_status_filter_db();
        conn.execute("ALTER TABLE conversations ADD COLUMN origin_host TEXT")
            .unwrap();
        conn.execute(
            "UPDATE conversations SET source_id = '   ', origin_host = 'remote-ci' WHERE id = 2",
        )
        .unwrap();
        conn.execute("UPDATE usage_daily SET source_id = '   ' WHERE agent_slug = 'claude_code'")
            .unwrap();

        let result = query_breakdown(
            &conn,
            &AnalyticsFilter::default(),
            Dim::Source,
            Metric::MessageCount,
            10,
        )
        .unwrap();

        assert_eq!(result.source_table, "messages");
        let remote = result
            .rows
            .iter()
            .find(|row| row.key == "remote-ci")
            .expect("remote source row should exist");
        assert_eq!(remote.value, 1);
        assert_eq!(remote.message_count, 1);
        let local = result
            .rows
            .iter()
            .find(|row| row.key == "local")
            .expect("local source row should exist");
        assert_eq!(local.value, 2);
    }

    #[test]
    fn query_breakdown_by_source_api_total_matches_blank_remote_usage_daily_source_via_origin_host()
    {
        let conn = setup_status_filter_db();
        conn.execute("ALTER TABLE conversations ADD COLUMN origin_host TEXT")
            .unwrap();
        conn.execute(
            "UPDATE conversations SET source_id = '   ', origin_host = 'remote-ci' WHERE id = 2",
        )
        .unwrap();
        conn.execute("UPDATE usage_daily SET source_id = '   ' WHERE agent_slug = 'claude_code'")
            .unwrap();
        conn.execute(
            "UPDATE message_metrics SET api_input_tokens = 13, api_output_tokens = 7, api_data_source = 'api' WHERE message_id = 21",
        )
        .unwrap();

        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            ..Default::default()
        };
        let result = query_breakdown(&conn, &filter, Dim::Source, Metric::ApiTotal, 10).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "remote-ci");
        assert_eq!(result.rows[0].value, 20);
        assert_eq!(result.rows[0].bucket.api_tokens_total, 20);
        assert_eq!(result.rows[0].bucket.api_coverage_message_count, 1);
    }

    #[test]
    fn query_breakdown_source_with_cost_metric_source_filter_matches_blank_remote_token_usage_source_via_origin_host()
     {
        let conn = setup_status_filter_db();
        conn.execute("ALTER TABLE conversations ADD COLUMN origin_host TEXT")
            .unwrap();
        conn.execute("ALTER TABLE token_usage ADD COLUMN estimated_cost_usd REAL")
            .unwrap();
        conn.execute(
            "UPDATE conversations SET source_id = '   ', origin_host = 'remote-ci' WHERE id = 2",
        )
        .unwrap();
        conn.execute(
        "UPDATE token_usage SET source_id = '   ', estimated_cost_usd = 0.4 WHERE conversation_id = 2",
    )
    .unwrap();
        conn.execute(
            "UPDATE token_daily_stats SET source_id = '   ' WHERE agent_slug = 'claude_code'",
        )
        .unwrap();

        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            ..Default::default()
        };
        let result =
            query_breakdown(&conn, &filter, Dim::Source, Metric::EstimatedCostUsd, 10).unwrap();

        assert_eq!(result.source_table, "token_usage");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "remote-ci");
        assert_eq!(result.rows[0].message_count, 1);
        assert!((result.rows[0].bucket.estimated_cost_usd - 0.4).abs() < 0.001);
    }

    #[test]
    fn query_breakdown_agent_with_cost_metric_source_filter_matches_blank_remote_token_usage_source_via_origin_host()
     {
        let conn = setup_status_filter_db();
        conn.execute("ALTER TABLE conversations ADD COLUMN origin_host TEXT")
            .unwrap();
        conn.execute("ALTER TABLE token_usage ADD COLUMN estimated_cost_usd REAL")
            .unwrap();
        conn.execute(
            "UPDATE conversations SET source_id = '   ', origin_host = 'remote-ci' WHERE id = 2",
        )
        .unwrap();
        conn.execute(
        "UPDATE token_usage SET source_id = '   ', estimated_cost_usd = 0.4 WHERE conversation_id = 2",
    )
    .unwrap();
        conn.execute(
            "UPDATE token_daily_stats SET source_id = '   ' WHERE agent_slug = 'claude_code'",
        )
        .unwrap();

        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            ..Default::default()
        };
        let result =
            query_breakdown(&conn, &filter, Dim::Agent, Metric::EstimatedCostUsd, 10).unwrap();

        assert_eq!(result.source_table, "token_usage");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "claude_code");
        assert_eq!(result.rows[0].message_count, 1);
        assert!((result.rows[0].bucket.estimated_cost_usd - 0.4).abs() < 0.001);
    }

    #[test]
    fn query_breakdown_source_with_cost_metric_default_uses_token_usage_to_recover_blank_remote_source_via_origin_host()
     {
        let conn = setup_status_filter_db();
        conn.execute("ALTER TABLE conversations ADD COLUMN origin_host TEXT")
            .unwrap();
        conn.execute("ALTER TABLE token_usage ADD COLUMN estimated_cost_usd REAL")
            .unwrap();
        conn.execute(
            "UPDATE conversations SET source_id = '   ', origin_host = 'remote-ci' WHERE id = 2",
        )
        .unwrap();
        conn.execute(
        "UPDATE token_usage SET source_id = '   ', estimated_cost_usd = 0.4 WHERE conversation_id = 2",
    )
    .unwrap();
        conn.execute(
        "UPDATE token_daily_stats SET source_id = '   ', estimated_cost_usd = 0.4 WHERE agent_slug = 'claude_code'",
    )
    .unwrap();

        let result = query_breakdown(
            &conn,
            &AnalyticsFilter::default(),
            Dim::Source,
            Metric::EstimatedCostUsd,
            10,
        )
        .unwrap();

        assert_eq!(result.source_table, "token_usage");
        let remote = result
            .rows
            .iter()
            .find(|row| row.key == "remote-ci")
            .expect("remote source row should exist");
        assert_eq!(remote.message_count, 1);
        assert!((remote.bucket.estimated_cost_usd - 0.4).abs() < 0.001);
    }

    #[test]
    fn query_breakdown_workspace_filter_applies_on_track_a() {
        let conn = setup_usage_daily_db();
        let filter = AnalyticsFilter {
            workspace_ids: vec![2],
            ..Default::default()
        };
        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::MessageCount, 10).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "aider");
        assert_eq!(result.rows[0].value, 30);
    }

    #[test]
    fn query_breakdown_by_agent_tool_calls_matches_blank_remote_usage_daily_source_via_origin_host()
    {
        let conn = setup_tools_remote_source_fallback_db();
        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            ..Default::default()
        };

        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::ToolCalls, 10).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "claude_code");
        assert_eq!(result.rows[0].message_count, 1);
        assert_eq!(result.rows[0].value, 7);
        assert_eq!(result.rows[0].bucket.tool_call_count, 7);
    }

    #[test]
    fn query_breakdown_by_agent_plan_count_matches_blank_remote_usage_daily_source_via_origin_host()
    {
        let conn = setup_tools_remote_source_fallback_db();
        conn.execute("ALTER TABLE message_metrics ADD COLUMN has_plan INTEGER NOT NULL DEFAULT 0")
            .unwrap();
        conn.execute("UPDATE message_metrics SET has_plan = 1 WHERE message_id = 21")
            .unwrap();
        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            ..Default::default()
        };

        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::PlanCount, 10).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "claude_code");
        assert_eq!(result.rows[0].message_count, 1);
        assert_eq!(result.rows[0].value, 1);
        assert_eq!(result.rows[0].bucket.plan_message_count, 1);
    }

    #[test]
    fn query_breakdown_by_agent_message_count_uses_message_metrics_timestamp_when_message_created_at_missing()
     {
        let conn = setup_tools_remote_source_fallback_db();
        conn.execute("UPDATE messages SET created_at = NULL WHERE id = 21")
            .unwrap();
        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            since_ms: Some(1_700_000_000_500),
            until_ms: Some(1_700_000_001_500),
            ..Default::default()
        };

        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::MessageCount, 10).unwrap();

        assert_eq!(result.source_table, "messages");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "claude_code");
        assert_eq!(result.rows[0].value, 1);
        assert_eq!(result.rows[0].message_count, 1);
    }

    #[test]
    fn query_breakdown_by_agent_api_total_uses_message_metrics_timestamp_when_message_created_at_missing()
     {
        let conn = setup_tools_remote_source_fallback_db();
        conn.execute("UPDATE messages SET created_at = NULL WHERE id = 21")
            .unwrap();
        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            since_ms: Some(1_700_000_000_500),
            until_ms: Some(1_700_000_001_500),
            ..Default::default()
        };

        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::ApiTotal, 10).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "claude_code");
        assert_eq!(result.rows[0].value, 100);
        assert_eq!(result.rows[0].bucket.api_tokens_total, 100);
    }

    #[test]
    fn query_breakdown_by_agent_api_total_subday_filter_excludes_same_day_rollup_rows_without_raw_matches()
     {
        let conn = setup_status_filter_db();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        let later_ms = day10_ms + (12 * 60 * 60 * 1000);

        conn.execute(&format!(
            "UPDATE messages SET created_at = {later_ms} WHERE conversation_id = 1"
        ))
        .unwrap();
        conn.execute(&format!(
            "UPDATE message_metrics SET created_at_ms = {later_ms} WHERE agent_slug = 'codex'"
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            since_ms: Some(day10_ms),
            until_ms: Some(day10_ms + 500),
            agents: vec!["codex".into()],
            ..Default::default()
        };
        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::ApiTotal, 10).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert!(result.rows.is_empty());
    }

    #[test]
    fn query_breakdown_model_api_total_deduplicates_duplicate_token_usage_rows() {
        let conn = setup_status_filter_db();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        conn.execute(&format!(
            "INSERT INTO token_usage
                (message_id, conversation_id, agent_id, workspace_id, source_id, timestamp_ms, day_id,
                 model_name, model_family, total_tokens, data_source)
             VALUES
                (11, 1, 1, 1, 'local', {}, 10, 'gpt-4o-mini', 'gpt-4o', 12, 'api')",
            day10_ms + 100
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            workspace_ids: vec![1],
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            ..Default::default()
        };
        let result = query_breakdown(&conn, &filter, Dim::Model, Metric::ApiTotal, 10).unwrap();

        assert_eq!(result.source_table, "token_usage");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].message_count, 2);
        assert_eq!(result.rows[0].value, 29);
        assert_eq!(result.rows[0].bucket.api_tokens_total, 29);
    }

    #[test]
    fn query_breakdown_by_agent_api_total_matches_blank_remote_usage_daily_source_via_origin_host()
    {
        let conn = setup_tools_remote_source_fallback_db();
        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            ..Default::default()
        };

        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::ApiTotal, 10).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "claude_code");
        assert_eq!(result.rows[0].message_count, 1);
        assert_eq!(result.rows[0].value, 100);
        assert_eq!(result.rows[0].bucket.api_tokens_total, 100);
    }

    #[test]
    fn query_breakdown_by_workspace_message_count_matches_blank_remote_usage_daily_source_via_origin_host()
     {
        let conn = setup_tools_remote_source_fallback_db();
        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            ..Default::default()
        };

        let result =
            query_breakdown(&conn, &filter, Dim::Workspace, Metric::MessageCount, 10).unwrap();

        assert_eq!(result.source_table, "messages");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "2");
        assert_eq!(result.rows[0].message_count, 1);
        assert_eq!(result.rows[0].value, 1);
    }

    #[test]
    fn query_breakdown_model_workspace_filter_uses_token_usage_and_normalizes_filters() {
        let conn = setup_status_filter_db();
        let filter = AnalyticsFilter {
            workspace_ids: vec![1],
            agents: vec!["  codex  ".into()],
            source: SourceFilter::Specific("  LOCAL  ".into()),
            ..Default::default()
        };
        let result = query_breakdown(&conn, &filter, Dim::Model, Metric::ApiTotal, 10).unwrap();

        assert_eq!(result.source_table, "token_usage");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "gpt-4o");
        assert_eq!(result.rows[0].message_count, 2);
        assert_eq!(result.rows[0].value, 29);
        assert_eq!(result.rows[0].bucket.api_tokens_total, 29);
    }

    #[test]
    fn query_breakdown_model_api_total_subday_filter_excludes_same_day_rollup_rows_without_raw_matches()
     {
        let conn = setup_status_filter_db();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        let later_ms = day10_ms + (12 * 60 * 60 * 1000);

        conn.execute(&format!(
            "UPDATE token_usage SET timestamp_ms = {later_ms} WHERE conversation_id = 1"
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            since_ms: Some(day10_ms),
            until_ms: Some(day10_ms + 500),
            agents: vec!["codex".into()],
            ..Default::default()
        };
        let result = query_breakdown(&conn, &filter, Dim::Model, Metric::ApiTotal, 10).unwrap();

        assert_eq!(result.source_table, "token_usage");
        assert!(result.rows.is_empty());
    }

    #[test]
    fn query_breakdown_model_unknown_workspace_filter_returns_empty() {
        let conn = setup_status_filter_db();
        let filter = AnalyticsFilter {
            workspace_ids: vec![999],
            ..Default::default()
        };
        let result = query_breakdown(&conn, &filter, Dim::Model, Metric::ApiTotal, 10).unwrap();

        assert_eq!(result.source_table, "token_usage");
        assert!(result.rows.is_empty());
    }

    #[test]
    fn query_breakdown_model_workspace_filter_matches_blank_remote_token_usage_source_via_origin_host()
     {
        let conn = setup_status_filter_db();
        conn.execute("ALTER TABLE conversations ADD COLUMN origin_host TEXT")
            .unwrap();
        conn.execute(
            "UPDATE conversations SET source_id = '   ', origin_host = 'remote-ci' WHERE id = 2",
        )
        .unwrap();
        conn.execute("UPDATE token_usage SET source_id = '   ' WHERE conversation_id = 2")
            .unwrap();
        conn.execute(
            "UPDATE token_daily_stats SET source_id = '   ' WHERE agent_slug = 'claude_code'",
        )
        .unwrap();

        let filter = AnalyticsFilter {
            workspace_ids: vec![2],
            agents: vec!["claude_code".into()],
            source: SourceFilter::Specific("remote-ci".into()),
            ..Default::default()
        };
        let result = query_breakdown(&conn, &filter, Dim::Model, Metric::ApiTotal, 10).unwrap();

        assert_eq!(result.source_table, "token_usage");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "claude");
        assert_eq!(result.rows[0].message_count, 1);
        assert_eq!(result.rows[0].value, 11);
        assert_eq!(result.rows[0].bucket.api_tokens_total, 11);
    }

    #[test]
    fn query_breakdown_model_source_filter_matches_blank_remote_token_daily_stats_source_via_origin_host()
     {
        let conn = setup_status_filter_db();
        conn.execute("ALTER TABLE conversations ADD COLUMN origin_host TEXT")
            .unwrap();
        conn.execute(
            "UPDATE conversations SET source_id = '   ', origin_host = 'remote-ci' WHERE id = 2",
        )
        .unwrap();
        conn.execute("UPDATE token_usage SET source_id = '   ' WHERE conversation_id = 2")
            .unwrap();
        conn.execute(
            "UPDATE token_daily_stats SET source_id = '   ' WHERE agent_slug = 'claude_code'",
        )
        .unwrap();

        let filter = AnalyticsFilter {
            agents: vec!["claude_code".into()],
            source: SourceFilter::Specific("remote-ci".into()),
            ..Default::default()
        };
        let result = query_breakdown(&conn, &filter, Dim::Model, Metric::ApiTotal, 10).unwrap();

        assert_eq!(result.source_table, "token_usage");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "claude");
        assert_eq!(result.rows[0].message_count, 1);
        assert_eq!(result.rows[0].value, 11);
        assert_eq!(result.rows[0].bucket.api_tokens_total, 11);
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
        let conn = Connection::open(":memory:").unwrap();
        let filter = AnalyticsFilter::default();
        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::ApiTotal, 10).unwrap();
        assert!(result.rows.is_empty());
    }

    #[test]
    fn query_breakdown_result_to_json_shape() {
        let conn = setup_usage_daily_db();
        let filter = AnalyticsFilter::default();
        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::ApiTotal, 10).unwrap();

        let json = result.to_cli_json();
        assert_eq!(json["dim"], "agent");
        assert_eq!(json["metric"], "api_total");
        assert!(json["rows"].is_array());
        assert!(json["row_count"].is_number());
        assert!(json["_meta"]["elapsed_ms"].is_number());
    }

    #[test]
    fn query_unpriced_models_totals_include_hidden_models_beyond_limit() {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute_batch(
            "CREATE TABLE token_usage (
                model_name TEXT,
                total_tokens INTEGER,
                estimated_cost_usd REAL
            );
             INSERT INTO token_usage (model_name, total_tokens, estimated_cost_usd) VALUES
                ('model-a', 100, NULL),
                ('model-b', 40, NULL),
                ('model-c', 10, NULL),
                ('model-priced', 25, 0.5);",
        )
        .unwrap();

        let result = query_unpriced_models(&conn, 1).unwrap();

        assert_eq!(result.models.len(), 1);
        assert_eq!(result.models[0].model_name, "model-a");
        assert_eq!(result.models[0].total_tokens, 100);
        assert_eq!(result.total_unpriced_tokens, 150);
        assert_eq!(result.total_priced_tokens, 25);
    }

    #[test]
    fn query_unpriced_models_deduplicates_duplicate_token_usage_rows() {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute_batch(
            "CREATE TABLE token_usage (
                message_id INTEGER,
                model_name TEXT,
                total_tokens INTEGER,
                estimated_cost_usd REAL
            );
             INSERT INTO token_usage (message_id, model_name, total_tokens, estimated_cost_usd) VALUES
                (1, 'model-a', 100, NULL),
                (1, 'model-a', 100, NULL),
                (2, 'model-b', 40, NULL),
                (3, 'model-priced', 25, 0.5),
                (3, 'model-priced', 25, 0.5);",
        )
        .unwrap();

        let result = query_unpriced_models(&conn, 10).unwrap();

        assert_eq!(result.models.len(), 2);
        assert_eq!(result.models[0].model_name, "model-a");
        assert_eq!(result.models[0].total_tokens, 100);
        assert_eq!(result.models[0].row_count, 1);
        assert_eq!(result.models[1].model_name, "model-b");
        assert_eq!(result.models[1].total_tokens, 40);
        assert_eq!(result.models[1].row_count, 1);
        assert_eq!(result.total_unpriced_tokens, 140);
        assert_eq!(result.total_priced_tokens, 25);
    }

    #[test]
    fn query_unpriced_models_coalesces_blank_model_names_into_none_bucket() {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute_batch(
            "CREATE TABLE token_usage (
                model_name TEXT,
                total_tokens INTEGER,
                estimated_cost_usd REAL
            );
             INSERT INTO token_usage (model_name, total_tokens, estimated_cost_usd) VALUES
                (NULL, 100, NULL),
                ('   ', 40, NULL),
                (' model-a ', 10, NULL);",
        )
        .unwrap();

        let result = query_unpriced_models(&conn, 10).unwrap();

        assert_eq!(result.models.len(), 2);
        assert_eq!(result.models[0].model_name, "(none)");
        assert_eq!(result.models[0].total_tokens, 140);
        assert_eq!(result.models[0].row_count, 2);
        assert_eq!(result.models[1].model_name, "model-a");
        assert_eq!(result.models[1].total_tokens, 10);
        assert_eq!(result.total_unpriced_tokens, 150);
    }

    #[test]
    fn query_unpriced_models_missing_estimated_cost_column_returns_empty_report() {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute_batch(
            "CREATE TABLE token_usage (
                model_name TEXT,
                total_tokens INTEGER
            );
             INSERT INTO token_usage (model_name, total_tokens) VALUES
                ('model-a', 100),
                ('model-b', 40);",
        )
        .unwrap();

        let result = query_unpriced_models(&conn, 10).unwrap();

        assert!(result.models.is_empty());
        assert_eq!(result.total_unpriced_tokens, 0);
        assert_eq!(result.total_priced_tokens, 0);
    }

    #[test]
    fn query_unpriced_models_without_model_name_column_uses_none_bucket() {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute_batch(
            "CREATE TABLE token_usage (
                total_tokens INTEGER,
                estimated_cost_usd REAL
            );
             INSERT INTO token_usage (total_tokens, estimated_cost_usd) VALUES
                (100, NULL),
                (40, NULL),
                (25, 0.5);",
        )
        .unwrap();

        let result = query_unpriced_models(&conn, 10).unwrap();

        assert_eq!(result.models.len(), 1);
        assert_eq!(result.models[0].model_name, "(none)");
        assert_eq!(result.models[0].total_tokens, 140);
        assert_eq!(result.models[0].row_count, 2);
        assert_eq!(result.total_unpriced_tokens, 140);
        assert_eq!(result.total_priced_tokens, 25);
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
    fn query_tools_normalizes_grouped_agent_slugs() {
        let conn = setup_usage_daily_db();
        conn.execute("UPDATE usage_daily SET agent_slug = '  codex  ' WHERE agent_slug = 'codex'")
            .unwrap();
        conn.execute("UPDATE usage_daily SET agent_slug = '' WHERE agent_slug = 'aider'")
            .unwrap();

        let result = query_tools(&conn, &AnalyticsFilter::default(), GroupBy::Day, 10).unwrap();

        let codex = result.rows.iter().find(|row| row.key == "codex").unwrap();
        assert_eq!(codex.tool_call_count, 25);

        let unknown = result.rows.iter().find(|row| row.key == "unknown").unwrap();
        assert_eq!(unknown.tool_call_count, 5);
    }

    #[test]
    fn query_tools_workspace_filter_applies() {
        let conn = setup_usage_daily_db();
        let filter = AnalyticsFilter {
            workspace_ids: vec![2],
            ..Default::default()
        };
        let result = query_tools(&conn, &filter, GroupBy::Day, 10).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "aider");
        assert_eq!(result.rows[0].tool_call_count, 5);
    }

    #[test]
    fn query_tools_totals_include_hidden_rows_beyond_limit() {
        let conn = setup_usage_daily_db();
        let result = query_tools(&conn, &AnalyticsFilter::default(), GroupBy::Day, 1).unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "claude_code");
        assert_eq!(result.rows[0].tool_call_count, 45);
        assert_eq!(result.total_tool_calls, 75);
        assert_eq!(result.total_messages, 360);
        assert_eq!(result.total_api_tokens, 210_000);
    }

    #[test]
    fn query_tools_raw_totals_include_hidden_rows_beyond_limit() {
        let conn = setup_tools_remote_source_fallback_db();
        let filter = AnalyticsFilter {
            since_ms: Some(1_700_000_000_000),
            until_ms: Some(1_700_000_002_000),
            ..Default::default()
        };
        let result = query_tools(&conn, &filter, GroupBy::Day, 1).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "claude_code");
        assert_eq!(result.rows[0].tool_call_count, 7);
        assert_eq!(result.total_tool_calls, 9);
        assert_eq!(result.total_messages, 2);
        assert_eq!(result.total_api_tokens, 130);
    }

    #[test]
    fn query_tools_source_filter_matches_blank_remote_usage_daily_source_via_origin_host() {
        let conn = setup_tools_remote_source_fallback_db();
        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            ..Default::default()
        };

        let result = query_tools(&conn, &filter, GroupBy::Day, 10).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "claude_code");
        assert_eq!(result.rows[0].tool_call_count, 7);
        assert_eq!(result.rows[0].message_count, 1);
        assert_eq!(result.rows[0].api_tokens_total, 100);
        assert_eq!(result.total_tool_calls, 7);
        assert_eq!(result.total_messages, 1);
        assert_eq!(result.total_api_tokens, 100);
    }

    #[test]
    fn query_tools_subday_filter_excludes_same_day_rollup_rows_without_raw_matches() {
        let conn = setup_tools_remote_source_fallback_db();
        let day_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(20250);
        let later_ms = day_ms + (12 * 60 * 60 * 1000);
        conn.execute(&format!(
            "UPDATE messages SET created_at = {later_ms} WHERE conversation_id = 1"
        ))
        .unwrap();
        conn.execute(&format!(
            "UPDATE message_metrics SET created_at_ms = {later_ms} WHERE message_id = 11"
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            since_ms: Some(day_ms),
            until_ms: Some(day_ms + 500),
            agents: vec!["codex".into()],
            ..Default::default()
        };
        let result = query_tools(&conn, &filter, GroupBy::Day, 10).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert!(result.rows.is_empty());
        assert_eq!(result.total_tool_calls, 0);
        assert_eq!(result.total_messages, 0);
        assert_eq!(result.total_api_tokens, 0);
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
        let conn = Connection::open(":memory:").unwrap();
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
    fn query_tools_hour_group_uses_usage_hourly() {
        let conn = setup_usage_hourly_db();
        let filter = AnalyticsFilter::default();
        let result = query_tools(&conn, &filter, GroupBy::Hour, 10).unwrap();

        assert_eq!(result.source_table, "usage_hourly");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "codex");
        assert_eq!(result.rows[0].tool_call_count, 8);
        assert_eq!(result.rows[0].message_count, 30);
        assert_eq!(result.rows[0].api_tokens_total, 4000);
    }

    #[test]
    fn query_total_messages_filtered_deduplicates_duplicate_message_metrics_rows() {
        let conn = setup_duplicate_message_metrics_raw_db();
        let filter = AnalyticsFilter {
            since_ms: Some(1_700_000_000_000),
            until_ms: Some(1_700_000_003_000),
            ..Default::default()
        };

        assert_eq!(query_total_messages_filtered(&conn, &filter), 2);
    }

    #[test]
    fn query_total_messages_filtered_uses_conversation_started_at_when_message_timestamps_missing()
    {
        let conn = setup_duplicate_message_metrics_raw_db();
        conn.execute("UPDATE messages SET created_at = NULL")
            .unwrap();
        conn.execute("UPDATE message_metrics SET created_at_ms = NULL")
            .unwrap();
        let filter = AnalyticsFilter {
            since_ms: Some(1_700_000_000_000),
            until_ms: Some(1_700_000_000_500),
            ..Default::default()
        };

        assert_eq!(query_total_messages_filtered(&conn, &filter), 2);
    }

    #[test]
    fn query_tokens_timeseries_deduplicates_duplicate_message_metrics_rows() {
        let conn = setup_duplicate_message_metrics_raw_db();
        let filter = AnalyticsFilter {
            since_ms: Some(1_700_000_000_000),
            until_ms: Some(1_700_000_003_000),
            ..Default::default()
        };
        let result = query_tokens_timeseries(&conn, &filter, GroupBy::Day).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert_eq!(result.path, "raw");
        assert_eq!(result.totals.message_count, 2);
        assert_eq!(result.totals.tool_call_count, 7);
        assert_eq!(result.totals.api_tokens_total, 1_200);
    }

    #[test]
    fn query_tokens_timeseries_uses_conversation_started_at_when_message_timestamps_missing() {
        let conn = setup_duplicate_message_metrics_raw_db();
        conn.execute("UPDATE messages SET created_at = NULL")
            .unwrap();
        conn.execute("UPDATE message_metrics SET created_at_ms = NULL")
            .unwrap();
        let filter = AnalyticsFilter {
            since_ms: Some(1_700_000_000_000),
            until_ms: Some(1_700_000_000_500),
            ..Default::default()
        };
        let result = query_tokens_timeseries(&conn, &filter, GroupBy::Day).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert_eq!(result.path, "raw");
        assert_eq!(result.totals.message_count, 2);
        assert_eq!(result.totals.api_tokens_total, 1_200);
    }

    #[test]
    fn query_breakdown_by_agent_api_total_deduplicates_duplicate_message_metrics_rows() {
        let conn = setup_duplicate_message_metrics_raw_db();
        let filter = AnalyticsFilter {
            since_ms: Some(1_700_000_000_000),
            until_ms: Some(1_700_000_003_000),
            ..Default::default()
        };
        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::ApiTotal, 10).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "codex");
        assert_eq!(result.rows[0].message_count, 2);
        assert_eq!(result.rows[0].value, 1_200);
    }

    #[test]
    fn query_breakdown_by_agent_message_count_uses_conversation_started_at_when_message_timestamps_missing()
     {
        let conn = setup_duplicate_message_metrics_raw_db();
        conn.execute("UPDATE messages SET created_at = NULL")
            .unwrap();
        conn.execute("UPDATE message_metrics SET created_at_ms = NULL")
            .unwrap();
        let filter = AnalyticsFilter {
            since_ms: Some(1_700_000_000_000),
            until_ms: Some(1_700_000_000_500),
            ..Default::default()
        };
        let result = query_breakdown(&conn, &filter, Dim::Agent, Metric::MessageCount, 10).unwrap();

        assert_eq!(result.source_table, "messages");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "codex");
        assert_eq!(result.rows[0].value, 2);
    }

    #[test]
    fn query_tools_deduplicates_duplicate_message_metrics_rows() {
        let conn = setup_duplicate_message_metrics_raw_db();
        let filter = AnalyticsFilter {
            since_ms: Some(1_700_000_000_000),
            until_ms: Some(1_700_000_003_000),
            ..Default::default()
        };
        let result = query_tools(&conn, &filter, GroupBy::Day, 10).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "codex");
        assert_eq!(result.rows[0].tool_call_count, 7);
        assert_eq!(result.rows[0].message_count, 2);
        assert_eq!(result.rows[0].api_tokens_total, 1_200);
    }

    #[test]
    fn query_tools_uses_conversation_started_at_when_message_timestamps_missing() {
        let conn = setup_duplicate_message_metrics_raw_db();
        conn.execute("UPDATE messages SET created_at = NULL")
            .unwrap();
        conn.execute("UPDATE message_metrics SET created_at_ms = NULL")
            .unwrap();
        let filter = AnalyticsFilter {
            since_ms: Some(1_700_000_000_000),
            until_ms: Some(1_700_000_000_500),
            ..Default::default()
        };
        let result = query_tools(&conn, &filter, GroupBy::Day, 10).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "codex");
        assert_eq!(result.rows[0].message_count, 2);
        assert_eq!(result.rows[0].tool_call_count, 7);
    }

    #[test]
    fn query_session_scatter_deduplicates_duplicate_message_metrics_rows() {
        let conn = setup_duplicate_message_metrics_raw_db();
        let points = query_session_scatter(&conn, &AnalyticsFilter::default(), 10).unwrap();

        assert_eq!(points.len(), 1);
        assert_eq!(points[0].source_path, "/sessions/dup.jsonl");
        assert_eq!(points[0].message_count, 2);
        assert_eq!(points[0].api_tokens_total, 1_200);
    }

    #[test]
    fn query_session_scatter_returns_sorted_points() {
        let conn = setup_session_scatter_db();
        let points = query_session_scatter(&conn, &AnalyticsFilter::default(), 10).unwrap();

        assert_eq!(points.len(), 2);
        assert_eq!(points[0].source_path, "/sessions/b.jsonl");
        assert_eq!(points[0].message_count, 3);
        assert_eq!(points[0].api_tokens_total, 2300);

        assert_eq!(points[1].source_path, "/sessions/a.jsonl");
        assert_eq!(points[1].message_count, 2);
        assert_eq!(points[1].api_tokens_total, 1000);
    }

    #[test]
    fn query_session_scatter_applies_agent_and_source_filters() {
        let conn = setup_session_scatter_db();
        let filter = AnalyticsFilter {
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            ..Default::default()
        };

        let points = query_session_scatter(&conn, &filter, 10).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].source_id, "local");
        assert_eq!(points[0].source_path, "/sessions/a.jsonl");
        assert_eq!(points[0].message_count, 2);
        assert_eq!(points[0].api_tokens_total, 1000);
    }

    #[test]
    fn query_session_scatter_without_agents_table_still_returns_points() {
        let conn = setup_session_scatter_db();
        conn.execute_batch("DROP TABLE agents;").unwrap();

        let points = query_session_scatter(&conn, &AnalyticsFilter::default(), 10).unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].source_path, "/sessions/b.jsonl");
        assert_eq!(points[1].source_path, "/sessions/a.jsonl");
    }

    #[test]
    fn query_session_scatter_with_missing_agent_row_keeps_session_without_filter() {
        let conn = setup_session_scatter_db();
        conn.execute("DELETE FROM agents WHERE id = 2").unwrap();

        let points = query_session_scatter(&conn, &AnalyticsFilter::default(), 10).unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].source_path, "/sessions/b.jsonl");
        assert_eq!(points[0].api_tokens_total, 2300);
    }

    #[test]
    fn query_session_scatter_normalizes_trimmed_agent_filter_and_agent_slug() {
        let conn = setup_session_scatter_db();
        conn.execute("UPDATE agents SET slug = '  codex  ' WHERE id = 1")
            .unwrap();

        let filter = AnalyticsFilter {
            agents: vec![" codex ".into()],
            ..Default::default()
        };

        let points = query_session_scatter(&conn, &filter, 10).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].source_path, "/sessions/a.jsonl");
        assert_eq!(points[0].api_tokens_total, 1000);
    }

    #[test]
    fn query_session_scatter_normalizes_trimmed_local_source_ids() {
        let conn = setup_session_scatter_db();
        conn.execute("UPDATE conversations SET source_id = '  LOCAL  ' WHERE id = 1")
            .unwrap();

        let filter = AnalyticsFilter {
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            ..Default::default()
        };

        let points = query_session_scatter(&conn, &filter, 10).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].source_id, "local");
        assert_eq!(points[0].source_path, "/sessions/a.jsonl");
    }

    #[test]
    fn query_session_scatter_matches_blank_remote_source_id_via_origin_host() {
        let conn = setup_session_scatter_db();
        conn.execute(
            "UPDATE conversations SET source_id = '   ', origin_host = 'remote-ci' WHERE id = 2",
        )
        .unwrap();

        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            ..Default::default()
        };

        let points = query_session_scatter(&conn, &filter, 10).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].source_id, "remote-ci");
        assert_eq!(points[0].source_path, "/sessions/b.jsonl");
        assert_eq!(points[0].message_count, 3);
        assert_eq!(points[0].api_tokens_total, 2300);
    }

    #[test]
    fn query_session_scatter_falls_back_to_token_usage_when_mm_tokens_missing() {
        let conn = setup_session_scatter_with_token_usage_fallback_db();
        let filter = AnalyticsFilter {
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            ..Default::default()
        };

        let points = query_session_scatter(&conn, &filter, 10).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].source_path, "/sessions/a.jsonl");
        assert_eq!(points[0].message_count, 2);
        // Message 11: 500 from message_metrics (preferred over token_usage=999).
        // Message 12: 900 from token_usage (message_metrics fields are NULL).
        assert_eq!(points[0].api_tokens_total, 1400);
    }

    #[test]
    fn query_session_scatter_aggregates_duplicate_token_usage_rows_per_message() {
        let conn = setup_session_scatter_db();
        conn.execute_batch(
            "CREATE TABLE token_usage (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                total_tokens INTEGER
            );",
        )
        .unwrap();
        conn.execute("INSERT INTO token_usage (id, message_id, total_tokens) VALUES (1, 11, 600)")
            .unwrap();
        conn.execute("INSERT INTO token_usage (id, message_id, total_tokens) VALUES (2, 11, 700)")
            .unwrap();

        let filter = AnalyticsFilter {
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            ..Default::default()
        };

        let points = query_session_scatter(&conn, &filter, 10).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].source_path, "/sessions/a.jsonl");
        assert_eq!(points[0].message_count, 2);
        // Message 11 still prefers its single message_metrics row (500) even if
        // dirty token_usage data contains multiple rows for the same message.
        assert_eq!(points[0].api_tokens_total, 1000);
    }

    #[test]
    fn query_session_scatter_uses_max_token_usage_total_for_duplicate_rows_per_message() {
        let conn = setup_session_scatter_db();
        conn.execute_batch(
            "CREATE TABLE token_usage (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                total_tokens INTEGER
            );",
        )
        .unwrap();
        conn.execute(
            "UPDATE message_metrics
             SET api_input_tokens = NULL,
                 api_output_tokens = NULL,
                 api_cache_read_tokens = NULL,
                 api_cache_creation_tokens = NULL,
                 api_thinking_tokens = NULL
             WHERE message_id = 12",
        )
        .unwrap();
        conn.execute("INSERT INTO token_usage (id, message_id, total_tokens) VALUES (1, 12, 400)")
            .unwrap();
        conn.execute("INSERT INTO token_usage (id, message_id, total_tokens) VALUES (2, 12, 900)")
            .unwrap();

        let filter = AnalyticsFilter {
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            ..Default::default()
        };

        let points = query_session_scatter(&conn, &filter, 10).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].source_path, "/sessions/a.jsonl");
        assert_eq!(points[0].message_count, 2);
        // Message 12 uses token_usage fallback, but duplicate dirty rows must
        // still resolve to one per-message total rather than summing both.
        assert_eq!(points[0].api_tokens_total, 1400);
    }

    #[test]
    fn query_session_scatter_falls_back_to_conversation_rollup_when_detailed_tokens_are_sparse() {
        let conn = setup_session_scatter_db();
        conn.execute(
            "UPDATE message_metrics
             SET api_input_tokens = NULL,
                 api_output_tokens = NULL,
                 api_cache_read_tokens = NULL,
                 api_cache_creation_tokens = NULL,
                 api_thinking_tokens = NULL
             WHERE message_id = 12",
        )
        .unwrap();

        let filter = AnalyticsFilter {
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            ..Default::default()
        };

        let points = query_session_scatter(&conn, &filter, 10).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].source_path, "/sessions/a.jsonl");
        assert_eq!(points[0].message_count, 2);
        // Detailed per-message API rows only account for message 11 (500), so
        // scatter must fall back to the conversation rollup total (1000).
        assert_eq!(points[0].api_tokens_total, 1000);
    }

    #[test]
    fn query_session_scatter_uses_message_metrics_timestamp_when_message_created_at_missing() {
        let conn = setup_session_scatter_db();
        conn.execute("ALTER TABLE message_metrics ADD COLUMN created_at_ms INTEGER")
            .unwrap();
        conn.execute(
            "UPDATE message_metrics
             SET created_at_ms = CASE message_id
                 WHEN 11 THEN 1700000001000
                 WHEN 12 THEN 1700000002000
                 WHEN 21 THEN 1700000001000
                 WHEN 22 THEN 1700000002000
                 WHEN 23 THEN 1700000003000
                 ELSE 0
             END",
        )
        .unwrap();
        conn.execute("UPDATE messages SET created_at = NULL WHERE conversation_id = 2")
            .unwrap();

        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            since_ms: Some(1_700_000_000_500),
            until_ms: Some(1_700_000_003_500),
            ..Default::default()
        };

        let points = query_session_scatter(&conn, &filter, 10).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].source_path, "/sessions/b.jsonl");
        assert_eq!(points[0].api_tokens_total, 2300);
    }

    #[test]
    fn query_session_scatter_uses_token_usage_timestamp_when_message_created_at_missing() {
        let conn = setup_session_scatter_with_token_usage_fallback_db();
        conn.execute("ALTER TABLE token_usage ADD COLUMN timestamp_ms INTEGER")
            .unwrap();
        conn.execute(
            "UPDATE token_usage
             SET timestamp_ms = CASE message_id
                 WHEN 11 THEN 1700000001000
                 WHEN 12 THEN 1700000002000
                 ELSE 0
             END",
        )
        .unwrap();
        conn.execute("UPDATE messages SET created_at = NULL WHERE conversation_id = 1")
            .unwrap();

        let filter = AnalyticsFilter {
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            since_ms: Some(1_700_000_000_500),
            until_ms: Some(1_700_000_002_500),
            ..Default::default()
        };

        let points = query_session_scatter(&conn, &filter, 10).unwrap();

        assert_eq!(points.len(), 1);
        assert_eq!(points[0].source_path, "/sessions/a.jsonl");
        assert_eq!(points[0].api_tokens_total, 1400);
    }

    #[test]
    fn query_cost_timeseries_deduplicates_duplicate_token_usage_rows() {
        let conn = setup_status_filter_db();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        conn.execute("ALTER TABLE token_usage ADD COLUMN estimated_cost_usd REAL")
            .unwrap();
        conn.execute(
            "UPDATE token_usage
             SET estimated_cost_usd = CASE message_id
                 WHEN 11 THEN 0.2
                 WHEN 12 THEN 0.3
                 WHEN 21 THEN 0.4
                 ELSE 0.0
             END",
        )
        .unwrap();
        conn.execute(&format!(
            "INSERT INTO token_usage
                (message_id, conversation_id, agent_id, workspace_id, source_id, timestamp_ms, day_id,
                 model_name, model_family, total_tokens, data_source, estimated_cost_usd)
             VALUES
                (11, 1, 1, 1, 'local', {}, 10, 'gpt-4o-mini', 'gpt-4o', 12, 'api', 0.2)",
            day10_ms + 100
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            workspace_ids: vec![1],
            agents: vec!["codex".into()],
            source: SourceFilter::Local,
            ..Default::default()
        };
        let result = query_cost_timeseries(&conn, &filter, GroupBy::Day).unwrap();

        assert_eq!(result.source_table, "token_usage");
        assert_eq!(result.path, "raw");
        assert_eq!(result.buckets.len(), 1);
        assert_eq!(result.buckets[0].1.message_count, 2);
        assert_eq!(result.buckets[0].1.api_tokens_total, 29);
        assert!((result.buckets[0].1.estimated_cost_usd - 0.5).abs() < 0.001);
        assert_eq!(result.totals.api_tokens_total, 29);
        assert!((result.totals.estimated_cost_usd - 0.5).abs() < 0.001);
    }

    #[test]
    fn query_session_scatter_with_api_source_column_preserves_legacy_mm_rows() {
        let conn = setup_session_scatter_with_api_source_column_db();
        let points = query_session_scatter(&conn, &AnalyticsFilter::default(), 10).unwrap();

        assert_eq!(points.len(), 2);
        let session_a = points
            .iter()
            .find(|p| p.source_path == "/sessions/a.jsonl")
            .expect("session A should exist");
        let session_b = points
            .iter()
            .find(|p| p.source_path == "/sessions/b.jsonl")
            .expect("session B should exist");

        // Session A still uses mixed mm/token_usage fallback correctly.
        assert_eq!(session_a.api_tokens_total, 1400);
        // Session B rows have NULL api_data_source but valid API columns and
        // must continue using message_metrics values.
        assert_eq!(session_b.api_tokens_total, 2300);
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
    fn query_tokens_timeseries_source_filter_matches_blank_remote_usage_daily_source_via_origin_host()
     {
        let conn = setup_status_filter_db();
        conn.execute("ALTER TABLE conversations ADD COLUMN origin_host TEXT")
            .unwrap();
        conn.execute(
            "UPDATE conversations SET source_id = '   ', origin_host = 'remote-ci' WHERE id = 2",
        )
        .unwrap();
        conn.execute(
            "UPDATE message_metrics SET source_id = '   ' WHERE agent_slug = 'claude_code'",
        )
        .unwrap();
        conn.execute("UPDATE usage_hourly SET source_id = '   ' WHERE agent_slug = 'claude_code'")
            .unwrap();
        conn.execute("UPDATE usage_daily SET source_id = '   ' WHERE agent_slug = 'claude_code'")
            .unwrap();

        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            ..Default::default()
        };
        let result = query_tokens_timeseries(&conn, &filter, GroupBy::Day).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert_eq!(result.buckets.len(), 1);
        assert_eq!(result.buckets[0].1.message_count, 1);
        assert_eq!(result.buckets[0].1.assistant_message_count, 1);
        assert_eq!(result.buckets[0].1.content_tokens_est_total, 5);
        assert_eq!(result.totals.message_count, 1);
        assert_eq!(result.totals.content_tokens_est_total, 5);
    }

    #[test]
    fn query_tokens_timeseries_subday_filter_excludes_same_day_rollup_rows_without_raw_matches() {
        let conn = setup_status_filter_db();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        let later_ms = day10_ms + (12 * 60 * 60 * 1000);

        conn.execute(&format!(
            "UPDATE messages SET created_at = {later_ms} WHERE conversation_id = 1"
        ))
        .unwrap();
        conn.execute(&format!(
            "UPDATE message_metrics SET created_at_ms = {later_ms} WHERE agent_slug = 'codex'"
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            since_ms: Some(day10_ms),
            until_ms: Some(day10_ms + 500),
            agents: vec!["codex".into()],
            ..Default::default()
        };
        let result = query_tokens_timeseries(&conn, &filter, GroupBy::Day).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert_eq!(result.path, "raw");
        assert!(result.buckets.is_empty());
        assert_eq!(result.totals.message_count, 0);
        assert_eq!(result.totals.api_tokens_total, 0);
    }

    #[test]
    fn query_tokens_timeseries_uses_legacy_second_message_metrics_timestamps_for_exact_filters() {
        let conn = setup_status_filter_db();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        let second_ts = (day10_ms + 100) / 1000;

        conn.execute("UPDATE messages SET created_at = NULL WHERE conversation_id = 1")
            .unwrap();
        conn.execute(&format!(
            "UPDATE message_metrics SET created_at_ms = {second_ts} WHERE agent_slug = 'codex'"
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            since_ms: Some(day10_ms),
            until_ms: Some(day10_ms + 500),
            agents: vec!["codex".into()],
            ..Default::default()
        };
        let result = query_tokens_timeseries(&conn, &filter, GroupBy::Day).unwrap();

        assert_eq!(result.source_table, "message_metrics");
        assert_eq!(result.path, "raw");
        assert_eq!(result.buckets.len(), 1);
        assert_eq!(result.buckets[0].1.message_count, 2);
        assert_eq!(result.buckets[0].1.api_tokens_total, 29);
        assert_eq!(result.totals.message_count, 2);
        assert_eq!(result.totals.api_tokens_total, 29);
    }

    #[test]
    fn query_cost_timeseries_source_filter_matches_blank_remote_token_daily_stats_source_via_origin_host()
     {
        let conn = setup_status_filter_db();
        conn.execute("ALTER TABLE conversations ADD COLUMN origin_host TEXT")
            .unwrap();
        conn.execute("ALTER TABLE token_usage ADD COLUMN estimated_cost_usd REAL")
            .unwrap();
        conn.execute(
            "UPDATE conversations SET source_id = '   ', origin_host = 'remote-ci' WHERE id = 2",
        )
        .unwrap();
        conn.execute("UPDATE token_usage SET source_id = '   ', estimated_cost_usd = 0.4 WHERE conversation_id = 2")
            .unwrap();
        conn.execute(
            "UPDATE token_daily_stats SET source_id = '   ' WHERE agent_slug = 'claude_code'",
        )
        .unwrap();

        let filter = AnalyticsFilter {
            source: SourceFilter::Specific("remote-ci".into()),
            ..Default::default()
        };
        let result = query_cost_timeseries(&conn, &filter, GroupBy::Day).unwrap();

        assert_eq!(result.source_table, "token_usage");
        assert_eq!(result.buckets.len(), 1);
        assert_eq!(result.buckets[0].1.api_tokens_total, 11);
        assert!((result.buckets[0].1.estimated_cost_usd - 0.4).abs() < 0.001);
        assert_eq!(result.totals.api_tokens_total, 11);
        assert!((result.totals.estimated_cost_usd - 0.4).abs() < 0.001);
    }

    #[test]
    fn query_cost_timeseries_subday_filter_excludes_same_day_rollup_rows_without_raw_matches() {
        let conn = setup_status_filter_db();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        let later_ms = day10_ms + (12 * 60 * 60 * 1000);
        conn.execute("ALTER TABLE token_usage ADD COLUMN estimated_cost_usd REAL")
            .unwrap();
        conn.execute(
            "UPDATE token_usage
             SET estimated_cost_usd = CASE message_id
                 WHEN 11 THEN 0.2
                 WHEN 12 THEN 0.3
                 WHEN 21 THEN 0.4
                 ELSE 0.0
             END",
        )
        .unwrap();
        conn.execute(&format!(
            "UPDATE token_usage SET timestamp_ms = {later_ms} WHERE conversation_id = 1"
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            since_ms: Some(day10_ms),
            until_ms: Some(day10_ms + 500),
            agents: vec!["codex".into()],
            ..Default::default()
        };
        let result = query_cost_timeseries(&conn, &filter, GroupBy::Day).unwrap();

        assert_eq!(result.source_table, "token_usage");
        assert_eq!(result.path, "raw");
        assert!(result.buckets.is_empty());
        assert_eq!(result.totals.api_tokens_total, 0);
        assert!((result.totals.estimated_cost_usd - 0.0).abs() < 0.001);
    }

    #[test]
    fn query_cost_timeseries_uses_legacy_second_token_usage_timestamps_for_exact_filters() {
        let conn = setup_status_filter_db();
        let day10_ms = crate::storage::sqlite::FrankenStorage::millis_from_day_id(10);
        let second_ts = (day10_ms + 100) / 1000;
        conn.execute("ALTER TABLE token_usage ADD COLUMN estimated_cost_usd REAL")
            .unwrap();
        conn.execute(
            "UPDATE token_usage
             SET estimated_cost_usd = CASE message_id
                 WHEN 11 THEN 0.2
                 WHEN 12 THEN 0.3
                 WHEN 21 THEN 0.4
                 ELSE 0.0
             END",
        )
        .unwrap();
        conn.execute(&format!(
            "UPDATE token_usage SET timestamp_ms = {second_ts} WHERE conversation_id = 1"
        ))
        .unwrap();

        let filter = AnalyticsFilter {
            since_ms: Some(day10_ms),
            until_ms: Some(day10_ms + 500),
            agents: vec!["codex".into()],
            ..Default::default()
        };
        let result = query_cost_timeseries(&conn, &filter, GroupBy::Day).unwrap();

        assert_eq!(result.source_table, "token_usage");
        assert_eq!(result.path, "raw");
        assert_eq!(result.buckets.len(), 1);
        assert_eq!(result.buckets[0].1.api_tokens_total, 29);
        assert!((result.buckets[0].1.estimated_cost_usd - 0.5).abs() < 0.001);
        assert_eq!(result.totals.api_tokens_total, 29);
        assert!((result.totals.estimated_cost_usd - 0.5).abs() < 0.001);
    }

    #[test]
    fn query_cost_timeseries_hour_group_uses_token_usage_hour_buckets() {
        let conn = setup_status_filter_db();
        conn.execute("ALTER TABLE token_usage ADD COLUMN estimated_cost_usd REAL")
            .unwrap();
        conn.execute(
            "UPDATE token_usage
             SET estimated_cost_usd = CASE message_id
                 WHEN 11 THEN 0.2
                 WHEN 12 THEN 0.3
                 WHEN 21 THEN 0.4
                 ELSE 0.0
             END",
        )
        .unwrap();

        let result =
            query_cost_timeseries(&conn, &AnalyticsFilter::default(), GroupBy::Hour).unwrap();

        assert_eq!(result.source_table, "token_usage");
        assert_eq!(result.path, "raw");
        assert_eq!(result.buckets.len(), 2);
        assert!(
            result
                .buckets
                .iter()
                .all(|(bucket, _)| bucket.contains('T'))
        );
        assert_eq!(result.totals.api_tokens_total, 40);
        assert!((result.totals.estimated_cost_usd - 0.9).abs() < 0.001);
    }

    #[test]
    fn query_cost_timeseries_missing_table_returns_empty() {
        let conn = Connection::open(":memory:").unwrap();
        let filter = AnalyticsFilter::default();
        let result = query_cost_timeseries(&conn, &filter, GroupBy::Day).unwrap();

        assert!(result.buckets.is_empty());
        assert_eq!(result.totals.estimated_cost_usd, 0.0);
        assert_eq!(result.path, "none");
    }

    #[test]
    fn query_breakdown_agent_with_cost_metric_normalizes_trimmed_agent_slug() {
        let conn = setup_token_daily_stats_db();
        conn.execute(
            "UPDATE token_daily_stats SET agent_slug = '  codex  ' WHERE agent_slug = 'codex'",
        )
        .unwrap();

        let filter = AnalyticsFilter {
            agents: vec![" codex ".into()],
            ..Default::default()
        };
        let result =
            query_breakdown(&conn, &filter, Dim::Agent, Metric::EstimatedCostUsd, 10).unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].key, "codex");
        assert!((result.rows[0].bucket.estimated_cost_usd - 0.80).abs() < f64::EPSILON);
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
    fn query_breakdown_workspace_with_cost_metric_uses_track_a_zero_values() {
        let conn = setup_usage_daily_db();
        let filter = AnalyticsFilter::default();
        let result =
            query_breakdown(&conn, &filter, Dim::Workspace, Metric::EstimatedCostUsd, 10).unwrap();

        assert_eq!(result.source_table, "usage_daily");
        assert!(!result.rows.is_empty());
        assert!(result.rows.iter().all(|r| r.value == 0));
        assert!(
            result
                .rows
                .iter()
                .all(|r| r.bucket.estimated_cost_usd == 0.0)
        );
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

    #[test]
    fn query_breakdown_model_content_est_total_uses_content_chars() {
        let conn = setup_token_daily_stats_db();
        let filter = AnalyticsFilter::default();
        let result =
            query_breakdown(&conn, &filter, Dim::Model, Metric::ContentEstTotal, 10).unwrap();

        // content_est_total is total_content_chars / 4 on Track B.
        assert_eq!(result.rows[0].key, "opus");
        assert_eq!(result.rows[0].value, 40_000);
        assert_eq!(result.rows[1].key, "gpt-4o");
        assert_eq!(result.rows[1].value, 25_000);
    }

    #[test]
    fn query_breakdown_model_coverage_pct_is_derived() {
        let conn = setup_token_daily_stats_db();
        let filter = AnalyticsFilter::default();
        let result = query_breakdown(&conn, &filter, Dim::Model, Metric::CoveragePct, 10).unwrap();

        assert!(!result.rows.is_empty());
        assert!(result.rows.iter().all(|r| r.value == 100));
    }

    #[test]
    fn query_breakdown_model_plan_count_is_zero_on_track_b() {
        let conn = setup_token_daily_stats_db();
        let filter = AnalyticsFilter::default();
        let result = query_breakdown(&conn, &filter, Dim::Model, Metric::PlanCount, 10).unwrap();

        assert!(!result.rows.is_empty());
        assert!(result.rows.iter().all(|r| r.value == 0));
    }
}
