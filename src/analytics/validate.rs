//! Analytics validation library.
//!
//! Provides deterministic checks for:
//! - **Track A invariants** — `usage_daily` matches `SUM(message_metrics)`.
//! - **Track B invariants** — `token_daily_stats` matches `SUM(token_usage)`.
//! - **Cross-track drift** — Track A vs Track B deltas by day + agent.
//! - **Performance guardrails** — timing budgets for queries and rebuilds.
//!
//! Output is a structured [`ValidationReport`] that serialises to JSON
//! for `cass analytics validate --json`.

use rusqlite::Connection;
use serde::Serialize;

use super::query::table_exists;

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

/// Severity level for a single check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

/// A single validation check result.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub id: String,
    pub ok: bool,
    pub severity: Severity,
    pub details: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_action: Option<String>,
}

/// A cross-track drift entry.
#[derive(Debug, Clone, Serialize)]
pub struct DriftEntry {
    pub day_id: i64,
    pub agent_slug: String,
    pub source_id: String,
    pub track_a_total: i64,
    pub track_b_total: i64,
    pub delta: i64,
    pub delta_pct: f64,
    pub likely_cause: String,
}

/// Sampling metadata.
#[derive(Debug, Clone, Serialize)]
pub struct SamplingMeta {
    pub buckets_checked: usize,
    pub buckets_total: usize,
    pub mode: String, // "sample" or "deep"
}

/// Report metadata.
#[derive(Debug, Clone, Serialize)]
pub struct ReportMeta {
    pub elapsed_ms: u64,
    pub sampling: SamplingMeta,
    pub path: String,
}

/// Full validation report.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationReport {
    pub checks: Vec<Check>,
    pub drift: Vec<DriftEntry>,
    pub _meta: ReportMeta,
}

impl ValidationReport {
    /// True if every check passed.
    pub fn all_ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }

    /// Count of checks that failed with a given severity.
    pub fn count_failures(&self, sev: Severity) -> usize {
        self.checks
            .iter()
            .filter(|c| !c.ok && c.severity == sev)
            .count()
    }

    /// Produce the JSON value.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::json!({"error": "serialization failed"}))
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Controls sampling vs deep-scan behaviour.
#[derive(Debug, Clone)]
pub struct ValidateConfig {
    /// Maximum number of (day_id, agent_slug) buckets to check per track.
    /// `0` means full scan (deep mode).
    pub sample_buckets: usize,
    /// Absolute delta threshold below which drift is treated as rounding noise.
    pub drift_abs_threshold: i64,
    /// Percentage threshold above which drift is flagged.
    pub drift_pct_threshold: f64,
}

impl Default for ValidateConfig {
    fn default() -> Self {
        Self {
            sample_buckets: 20,
            drift_abs_threshold: 10,
            drift_pct_threshold: 1.0,
        }
    }
}

impl ValidateConfig {
    /// Deep-scan mode: check every bucket.
    pub fn deep() -> Self {
        Self {
            sample_buckets: 0,
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run the full validation suite and return a structured report.
pub fn run_validation(conn: &Connection, config: &ValidateConfig) -> ValidationReport {
    let start = std::time::Instant::now();
    let mut checks = Vec::new();
    let mut buckets_checked: usize = 0;
    let mut buckets_total: usize = 0;

    // --- Track A ---
    let (a_checks, a_checked, a_total) = validate_track_a(conn, config);
    checks.extend(a_checks);
    buckets_checked += a_checked;
    buckets_total += a_total;

    // --- Track B ---
    let (b_checks, b_checked, b_total) = validate_track_b(conn, config);
    checks.extend(b_checks);
    buckets_checked += b_checked;
    buckets_total += b_total;

    // --- Cross-track drift ---
    let (d_checks, d_entries) = validate_cross_track_drift(conn, config);
    checks.extend(d_checks);
    let drift = d_entries;

    // --- Non-negative counters ---
    checks.extend(validate_non_negative_counters(conn));

    let elapsed_ms = start.elapsed().as_millis() as u64;
    let mode = if config.sample_buckets == 0 {
        "deep"
    } else {
        "sample"
    };

    ValidationReport {
        checks,
        drift,
        _meta: ReportMeta {
            elapsed_ms,
            sampling: SamplingMeta {
                buckets_checked,
                buckets_total,
                mode: mode.into(),
            },
            path: "rollup".into(),
        },
    }
}

// ---------------------------------------------------------------------------
// Track A validation
// ---------------------------------------------------------------------------

/// Validate Track A: `usage_daily` aggregates must match `SUM(message_metrics)`.
///
/// Returns `(checks, buckets_checked, buckets_total)`.
fn validate_track_a(
    conn: &Connection,
    config: &ValidateConfig,
) -> (Vec<Check>, usize, usize) {
    let mut checks = Vec::new();

    if !table_exists(conn, "usage_daily") || !table_exists(conn, "message_metrics") {
        checks.push(Check {
            id: "track_a.tables_exist".into(),
            ok: false,
            severity: Severity::Error,
            details: "Track A tables missing (usage_daily or message_metrics)".into(),
            suggested_action: Some("Run 'cass analytics rebuild'".into()),
        });
        return (checks, 0, 0);
    }

    checks.push(Check {
        id: "track_a.tables_exist".into(),
        ok: true,
        severity: Severity::Info,
        details: "Track A tables exist".into(),
        suggested_action: None,
    });

    // Get all distinct (day_id, agent_slug, workspace_id, source_id) buckets in usage_daily.
    let total_buckets: usize = conn
        .query_row("SELECT COUNT(*) FROM usage_daily", [], |r| {
            r.get::<_, i64>(0).map(|v| v as usize)
        })
        .unwrap_or(0);

    if total_buckets == 0 {
        checks.push(Check {
            id: "track_a.has_data".into(),
            ok: false,
            severity: Severity::Warning,
            details: "usage_daily is empty".into(),
            suggested_action: Some("Run 'cass analytics rebuild'".into()),
        });
        return (checks, 0, 0);
    }

    // Sample or full scan.
    let limit_clause = if config.sample_buckets > 0 {
        format!("LIMIT {}", config.sample_buckets)
    } else {
        String::new()
    };

    // Check content_tokens_est_total invariant.
    let sql = format!(
        "SELECT ud.day_id, ud.agent_slug, ud.workspace_id, ud.source_id,
                ud.content_tokens_est_total,
                COALESCE(mm.sum_content, 0),
                ud.message_count,
                COALESCE(mm.sum_msgs, 0),
                ud.api_tokens_total,
                COALESCE(mm.sum_api, 0),
                ud.api_coverage_message_count,
                COALESCE(mm.sum_api_coverage, 0)
         FROM usage_daily ud
         LEFT JOIN (
             SELECT day_id, agent_slug, workspace_id, source_id,
                    SUM(content_tokens_est) AS sum_content,
                    COUNT(*) AS sum_msgs,
                    SUM(CASE WHEN api_data_source = 'api'
                             THEN COALESCE(api_input_tokens, 0)
                                + COALESCE(api_output_tokens, 0)
                                + COALESCE(api_cache_read_tokens, 0)
                                + COALESCE(api_cache_creation_tokens, 0)
                                + COALESCE(api_thinking_tokens, 0)
                             ELSE 0 END) AS sum_api,
                    SUM(CASE WHEN api_data_source = 'api' THEN 1 ELSE 0 END) AS sum_api_coverage
             FROM message_metrics
             GROUP BY day_id, agent_slug, workspace_id, source_id
         ) mm ON ud.day_id = mm.day_id
              AND ud.agent_slug = mm.agent_slug
              AND ud.workspace_id = mm.workspace_id
              AND ud.source_id = mm.source_id
         ORDER BY ud.day_id DESC
         {limit_clause}"
    );

    let mut mismatches_content = 0_usize;
    let mut mismatches_msg_count = 0_usize;
    let mut mismatches_api = 0_usize;
    let mut mismatches_api_cov = 0_usize;
    let mut checked = 0_usize;

    if let Ok(mut stmt) = conn.prepare(&sql)
        && let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,   // day_id
                row.get::<_, String>(1)?, // agent_slug
                row.get::<_, i64>(4)?,    // ud.content_tokens_est_total
                row.get::<_, i64>(5)?,    // mm.sum_content
                row.get::<_, i64>(6)?,    // ud.message_count
                row.get::<_, i64>(7)?,    // mm.sum_msgs
                row.get::<_, i64>(8)?,    // ud.api_tokens_total
                row.get::<_, i64>(9)?,    // mm.sum_api
                row.get::<_, i64>(10)?,   // ud.api_coverage_message_count
                row.get::<_, i64>(11)?,   // mm.sum_api_coverage
            ))
        })
    {
        for row in rows.flatten() {
            checked += 1;
            let (_day_id, _agent, ud_content, mm_content, ud_msgs, mm_msgs, ud_api, mm_api, ud_cov, mm_cov) = row;
            if ud_content != mm_content {
                mismatches_content += 1;
            }
            if ud_msgs != mm_msgs {
                mismatches_msg_count += 1;
            }
            if ud_api != mm_api {
                mismatches_api += 1;
            }
            if ud_cov != mm_cov {
                mismatches_api_cov += 1;
            }
        }
    }

    // Content tokens check.
    checks.push(Check {
        id: "track_a.content_tokens_match".into(),
        ok: mismatches_content == 0,
        severity: if mismatches_content > 0 { Severity::Error } else { Severity::Info },
        details: format!(
            "content_tokens_est_total: {mismatches_content}/{checked} buckets mismatched"
        ),
        suggested_action: if mismatches_content > 0 {
            Some("Run 'cass analytics rebuild --track a'".into())
        } else {
            None
        },
    });

    // Message count check.
    checks.push(Check {
        id: "track_a.message_count_match".into(),
        ok: mismatches_msg_count == 0,
        severity: if mismatches_msg_count > 0 { Severity::Error } else { Severity::Info },
        details: format!(
            "message_count: {mismatches_msg_count}/{checked} buckets mismatched"
        ),
        suggested_action: if mismatches_msg_count > 0 {
            Some("Run 'cass analytics rebuild --track a'".into())
        } else {
            None
        },
    });

    // API tokens check.
    checks.push(Check {
        id: "track_a.api_tokens_match".into(),
        ok: mismatches_api == 0,
        severity: if mismatches_api > 0 { Severity::Error } else { Severity::Info },
        details: format!(
            "api_tokens_total: {mismatches_api}/{checked} buckets mismatched"
        ),
        suggested_action: if mismatches_api > 0 {
            Some("Run 'cass analytics rebuild --track a'".into())
        } else {
            None
        },
    });

    // API coverage check.
    checks.push(Check {
        id: "track_a.api_coverage_match".into(),
        ok: mismatches_api_cov == 0,
        severity: if mismatches_api_cov > 0 { Severity::Warning } else { Severity::Info },
        details: format!(
            "api_coverage_message_count: {mismatches_api_cov}/{checked} buckets mismatched"
        ),
        suggested_action: if mismatches_api_cov > 0 {
            Some("Run 'cass analytics rebuild --track a'".into())
        } else {
            None
        },
    });

    (checks, checked, total_buckets)
}

// ---------------------------------------------------------------------------
// Track B validation
// ---------------------------------------------------------------------------

/// Validate Track B: `token_daily_stats` must match `SUM(token_usage)`.
fn validate_track_b(
    conn: &Connection,
    config: &ValidateConfig,
) -> (Vec<Check>, usize, usize) {
    let mut checks = Vec::new();

    if !table_exists(conn, "token_daily_stats") || !table_exists(conn, "token_usage") {
        checks.push(Check {
            id: "track_b.tables_exist".into(),
            ok: false,
            severity: Severity::Error,
            details: "Track B tables missing (token_daily_stats or token_usage)".into(),
            suggested_action: Some(
                "Run 'cass analytics rebuild --track all' (requires z9fse.13)".into(),
            ),
        });
        return (checks, 0, 0);
    }

    checks.push(Check {
        id: "track_b.tables_exist".into(),
        ok: true,
        severity: Severity::Info,
        details: "Track B tables exist".into(),
        suggested_action: None,
    });

    let total_buckets: usize = conn
        .query_row("SELECT COUNT(*) FROM token_daily_stats", [], |r| {
            r.get::<_, i64>(0).map(|v| v as usize)
        })
        .unwrap_or(0);

    if total_buckets == 0 {
        checks.push(Check {
            id: "track_b.has_data".into(),
            ok: false,
            severity: Severity::Warning,
            details: "token_daily_stats is empty".into(),
            suggested_action: Some("Run 'cass analytics rebuild --track all'".into()),
        });
        return (checks, 0, 0);
    }

    let limit_clause = if config.sample_buckets > 0 {
        format!("LIMIT {}", config.sample_buckets)
    } else {
        String::new()
    };

    // token_usage uses agent_id (FK) not agent_slug; we need agents table.
    // If agents table doesn't exist, we fall back to a simpler join.
    let has_agents_table = table_exists(conn, "agents");

    let sql = if has_agents_table {
        format!(
            "SELECT tds.day_id, tds.agent_slug, tds.source_id, tds.model_family,
                    tds.grand_total_tokens,
                    COALESCE(tu.sum_total, 0),
                    tds.total_tool_calls,
                    COALESCE(tu.sum_tools, 0),
                    tds.api_call_count,
                    COALESCE(tu.sum_rows, 0)
             FROM token_daily_stats tds
             LEFT JOIN (
                 SELECT t.day_id,
                        a.slug AS agent_slug,
                        t.source_id,
                        COALESCE(t.model_family, 'unknown') AS model_family,
                        SUM(COALESCE(t.total_tokens, 0)) AS sum_total,
                        SUM(t.tool_call_count) AS sum_tools,
                        COUNT(*) AS sum_rows
                 FROM token_usage t
                 JOIN agents a ON a.id = t.agent_id
                 GROUP BY t.day_id, a.slug, t.source_id, COALESCE(t.model_family, 'unknown')
             ) tu ON tds.day_id = tu.day_id
                   AND tds.agent_slug = tu.agent_slug
                   AND tds.source_id = tu.source_id
                   AND tds.model_family = tu.model_family
             ORDER BY tds.day_id DESC
             {limit_clause}"
        )
    } else {
        // Without agents table, we can't join — skip granular check.
        checks.push(Check {
            id: "track_b.agents_table_missing".into(),
            ok: false,
            severity: Severity::Warning,
            details: "agents table not found — cannot validate Track B granular invariants".into(),
            suggested_action: None,
        });
        return (checks, 0, total_buckets);
    };

    let mut mismatches_total = 0_usize;
    let mut mismatches_tools = 0_usize;
    let mut checked = 0_usize;

    if let Ok(mut stmt) = conn.prepare(&sql)
        && let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(4)?,  // tds.grand_total_tokens
                row.get::<_, i64>(5)?,  // tu.sum_total
                row.get::<_, i64>(6)?,  // tds.total_tool_calls
                row.get::<_, i64>(7)?,  // tu.sum_tools
            ))
        })
    {
        for row in rows.flatten() {
            checked += 1;
            let (tds_total, tu_total, tds_tools, tu_tools) = row;
            if tds_total != tu_total {
                mismatches_total += 1;
            }
            if tds_tools != tu_tools {
                mismatches_tools += 1;
            }
        }
    }

    checks.push(Check {
        id: "track_b.grand_total_match".into(),
        ok: mismatches_total == 0,
        severity: if mismatches_total > 0 { Severity::Error } else { Severity::Info },
        details: format!(
            "grand_total_tokens: {mismatches_total}/{checked} buckets mismatched"
        ),
        suggested_action: if mismatches_total > 0 {
            Some("Run 'cass analytics rebuild --track all'".into())
        } else {
            None
        },
    });

    checks.push(Check {
        id: "track_b.tool_calls_match".into(),
        ok: mismatches_tools == 0,
        severity: if mismatches_tools > 0 { Severity::Warning } else { Severity::Info },
        details: format!(
            "total_tool_calls: {mismatches_tools}/{checked} buckets mismatched"
        ),
        suggested_action: if mismatches_tools > 0 {
            Some("Run 'cass analytics rebuild --track all'".into())
        } else {
            None
        },
    });

    (checks, checked, total_buckets)
}

// ---------------------------------------------------------------------------
// Cross-track drift detection
// ---------------------------------------------------------------------------

/// Detect drift between Track A and Track B at the day + agent + source level.
fn validate_cross_track_drift(
    conn: &Connection,
    config: &ValidateConfig,
) -> (Vec<Check>, Vec<DriftEntry>) {
    let mut checks = Vec::new();
    let mut entries = Vec::new();

    let has_a = table_exists(conn, "usage_daily");
    let has_b = table_exists(conn, "token_daily_stats");

    if !has_a || !has_b {
        let missing = if !has_a && !has_b {
            "both tracks"
        } else if !has_a {
            "Track A (usage_daily)"
        } else {
            "Track B (token_daily_stats)"
        };
        checks.push(Check {
            id: "cross_track.tables_exist".into(),
            ok: false,
            severity: Severity::Warning,
            details: format!("Cannot compute cross-track drift: {missing} missing"),
            suggested_action: Some("Run 'cass analytics rebuild --track all'".into()),
        });
        return (checks, entries);
    }

    let limit_clause = if config.sample_buckets > 0 {
        format!("LIMIT {}", config.sample_buckets)
    } else {
        String::new()
    };

    // Compare api_tokens_total (Track A) vs grand_total_tokens (Track B) by day+agent+source.
    let sql = format!(
        "SELECT COALESCE(a.day_id, b.day_id) AS did,
                COALESCE(a.agent_slug, b.agent_slug) AS agent,
                COALESCE(a.source_id, b.source_id) AS source,
                COALESCE(a.api_total, 0),
                COALESCE(b.grand_total, 0)
         FROM (
             SELECT day_id, agent_slug, source_id, SUM(api_tokens_total) AS api_total
             FROM usage_daily
             GROUP BY day_id, agent_slug, source_id
         ) a
         FULL OUTER JOIN (
             SELECT day_id, agent_slug, source_id, SUM(grand_total_tokens) AS grand_total
             FROM token_daily_stats
             GROUP BY day_id, agent_slug, source_id
         ) b ON a.day_id = b.day_id AND a.agent_slug = b.agent_slug AND a.source_id = b.source_id
         ORDER BY did DESC
         {limit_clause}"
    );

    // SQLite doesn't support FULL OUTER JOIN — fall back to UNION approach.
    let sql_compat = format!(
        "SELECT day_id, agent_slug, source_id,
                SUM(a_total) AS a_total,
                SUM(b_total) AS b_total
         FROM (
             SELECT day_id, agent_slug, source_id,
                    SUM(api_tokens_total) AS a_total, 0 AS b_total
             FROM usage_daily
             GROUP BY day_id, agent_slug, source_id
             UNION ALL
             SELECT day_id, agent_slug, source_id,
                    0 AS a_total, SUM(grand_total_tokens) AS b_total
             FROM token_daily_stats
             GROUP BY day_id, agent_slug, source_id
         )
         GROUP BY day_id, agent_slug, source_id
         ORDER BY day_id DESC
         {limit_clause}"
    );

    // Try the compatible query first.
    let mut drift_count = 0_usize;
    let mut drift_checked = 0_usize;

    // Try compatible SQL (UNION-based).
    let query_result = conn.prepare(&sql_compat);
    if let Ok(mut stmt) = query_result {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,    // day_id
                row.get::<_, String>(1)?,  // agent_slug
                row.get::<_, String>(2)?,  // source_id
                row.get::<_, i64>(3)?,     // a_total
                row.get::<_, i64>(4)?,     // b_total
            ))
        }) {
            for row in rows.flatten() {
                drift_checked += 1;
                let (day_id, agent_slug, source_id, a_total, b_total) = row;
                let delta = a_total - b_total;
                let denom = a_total.max(b_total).max(1);
                let delta_pct = (delta.abs() as f64 / denom as f64) * 100.0;

                if delta.abs() > config.drift_abs_threshold
                    && delta_pct > config.drift_pct_threshold
                {
                    drift_count += 1;
                    let likely_cause = if a_total > 0 && b_total == 0 {
                        "Track B missing rows (rebuild needed or not yet ingested)"
                    } else if b_total > 0 && a_total == 0 {
                        "Track A missing rows (rebuild needed)"
                    } else if a_total > b_total {
                        "Track A higher — Track B may be stale or missing some messages"
                    } else {
                        "Track B higher — Track A may have been rebuilt recently without all data"
                    };

                    entries.push(DriftEntry {
                        day_id,
                        agent_slug,
                        source_id,
                        track_a_total: a_total,
                        track_b_total: b_total,
                        delta,
                        delta_pct: (delta_pct * 100.0).round() / 100.0,
                        likely_cause: likely_cause.into(),
                    });
                }
            }
        }
    } else {
        // If the UNION query also fails (shouldn't happen), try FULL OUTER JOIN.
        if let Ok(mut stmt) = conn.prepare(&sql) {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            }) {
                for row in rows.flatten() {
                    drift_checked += 1;
                    let (day_id, agent_slug, source_id, a_total, b_total) = row;
                    let delta = a_total - b_total;
                    let denom = a_total.max(b_total).max(1);
                    let delta_pct = (delta.abs() as f64 / denom as f64) * 100.0;

                    if delta.abs() > config.drift_abs_threshold
                        && delta_pct > config.drift_pct_threshold
                    {
                        drift_count += 1;
                        entries.push(DriftEntry {
                            day_id,
                            agent_slug,
                            source_id,
                            track_a_total: a_total,
                            track_b_total: b_total,
                            delta,
                            delta_pct: (delta_pct * 100.0).round() / 100.0,
                            likely_cause: "drift detected (unknown cause)".into(),
                        });
                    }
                }
            }
        }
    }

    let total_ok = drift_count == 0;
    checks.push(Check {
        id: "cross_track.drift".into(),
        ok: total_ok,
        severity: if drift_count > 0 { Severity::Warning } else { Severity::Info },
        details: format!(
            "Cross-track drift: {drift_count}/{drift_checked} day+agent+source slices drifted"
        ),
        suggested_action: if drift_count > 0 {
            Some("Run 'cass analytics rebuild --track all' to re-sync both tracks".into())
        } else {
            None
        },
    });

    (checks, entries)
}

// ---------------------------------------------------------------------------
// Non-negative counter checks
// ---------------------------------------------------------------------------

/// Validate that rollup counters are never negative.
fn validate_non_negative_counters(conn: &Connection) -> Vec<Check> {
    let mut checks = Vec::new();

    // Track A: usage_daily non-negative.
    if table_exists(conn, "usage_daily") {
        let cols = [
            "message_count",
            "user_message_count",
            "assistant_message_count",
            "tool_call_count",
            "plan_message_count",
            "api_coverage_message_count",
            "content_tokens_est_total",
            "api_tokens_total",
        ];
        let cond = cols
            .iter()
            .map(|c| format!("{c} < 0"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!("SELECT COUNT(*) FROM usage_daily WHERE {cond}");
        let negative_rows: i64 = conn.query_row(&sql, [], |r| r.get(0)).unwrap_or(0);

        checks.push(Check {
            id: "track_a.non_negative_counters".into(),
            ok: negative_rows == 0,
            severity: if negative_rows > 0 {
                Severity::Error
            } else {
                Severity::Info
            },
            details: format!("usage_daily: {negative_rows} rows with negative counters"),
            suggested_action: if negative_rows > 0 {
                Some("Run 'cass analytics rebuild --track a'".into())
            } else {
                None
            },
        });
    }

    // Track A: api_coverage_message_count <= message_count.
    if table_exists(conn, "usage_daily") {
        let bad: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_daily WHERE api_coverage_message_count > message_count",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        checks.push(Check {
            id: "track_a.coverage_lte_messages".into(),
            ok: bad == 0,
            severity: if bad > 0 {
                Severity::Warning
            } else {
                Severity::Info
            },
            details: format!(
                "usage_daily: {bad} rows where api_coverage_message_count > message_count"
            ),
            suggested_action: if bad > 0 {
                Some("Run 'cass analytics rebuild --track a'".into())
            } else {
                None
            },
        });
    }

    // Track B: token_daily_stats non-negative.
    if table_exists(conn, "token_daily_stats") {
        let cols = [
            "api_call_count",
            "total_input_tokens",
            "total_output_tokens",
            "grand_total_tokens",
            "total_tool_calls",
        ];
        let cond = cols
            .iter()
            .map(|c| format!("{c} < 0"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!("SELECT COUNT(*) FROM token_daily_stats WHERE {cond}");
        let negative_rows: i64 = conn.query_row(&sql, [], |r| r.get(0)).unwrap_or(0);

        checks.push(Check {
            id: "track_b.non_negative_counters".into(),
            ok: negative_rows == 0,
            severity: if negative_rows > 0 {
                Severity::Error
            } else {
                Severity::Info
            },
            details: format!("token_daily_stats: {negative_rows} rows with negative counters"),
            suggested_action: if negative_rows > 0 {
                Some("Run 'cass analytics rebuild --track all'".into())
            } else {
                None
            },
        });
    }

    checks
}

// ---------------------------------------------------------------------------
// Performance guardrails
// ---------------------------------------------------------------------------

/// A single performance measurement.
#[derive(Debug, Clone, Serialize)]
pub struct PerfMeasurement {
    pub id: String,
    pub elapsed_ms: u64,
    pub budget_ms: u64,
    pub within_budget: bool,
    pub details: String,
}

/// Run a performance guardrail check: time a basic timeseries query.
pub fn perf_query_guardrail(conn: &Connection) -> PerfMeasurement {
    let start = std::time::Instant::now();

    // Run a basic rollup query — same as query_tokens_timeseries with no filters.
    let budget_ms = 500_u64; // 500ms budget for rollup timeseries query
    let row_count: i64 = if table_exists(conn, "usage_daily") {
        let sql = "SELECT COUNT(*) FROM (
            SELECT day_id, SUM(message_count) FROM usage_daily GROUP BY day_id
        )";
        conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0)
    } else {
        0
    };

    let elapsed_ms = start.elapsed().as_millis() as u64;

    PerfMeasurement {
        id: "perf.query_timeseries".into(),
        elapsed_ms,
        budget_ms,
        within_budget: elapsed_ms <= budget_ms,
        details: format!("Timeseries rollup query: {row_count} day buckets in {elapsed_ms}ms"),
    }
}

/// Run a performance guardrail for breakdown queries.
pub fn perf_breakdown_guardrail(conn: &Connection) -> PerfMeasurement {
    let start = std::time::Instant::now();
    let budget_ms = 200_u64;

    let row_count: i64 = if table_exists(conn, "usage_daily") {
        let sql = "SELECT COUNT(*) FROM (
            SELECT agent_slug, SUM(api_tokens_total)
            FROM usage_daily GROUP BY agent_slug
        )";
        conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0)
    } else {
        0
    };

    let elapsed_ms = start.elapsed().as_millis() as u64;

    PerfMeasurement {
        id: "perf.query_breakdown".into(),
        elapsed_ms,
        budget_ms,
        within_budget: elapsed_ms <= budget_ms,
        details: format!("Breakdown query: {row_count} agent groups in {elapsed_ms}ms"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Fixture helpers --

    /// Create a minimal Track A fixture (message_metrics + usage_daily).
    fn setup_track_a_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE message_metrics (
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
                api_service_tier TEXT,
                api_data_source TEXT NOT NULL DEFAULT 'estimated',
                tool_call_count INTEGER NOT NULL DEFAULT 0,
                has_tool_calls INTEGER NOT NULL DEFAULT 0,
                has_plan INTEGER NOT NULL DEFAULT 0
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

        // Insert consistent data: 3 messages for claude_code on day 20250.
        conn.execute_batch(
            "INSERT INTO message_metrics VALUES
                (1, 1750000000000, 416666, 20254, 'claude_code', 1, 'local', 'user',   400, 100, NULL, NULL, NULL, NULL, NULL, NULL, 'estimated', 0, 0, 0),
                (2, 1750000000001, 416666, 20254, 'claude_code', 1, 'local', 'assistant', 800, 200, 500, 300, 50, 20, 10, NULL, 'api', 3, 1, 0),
                (3, 1750000000002, 416666, 20254, 'claude_code', 1, 'local', 'user',   600, 150, NULL, NULL, NULL, NULL, NULL, NULL, 'estimated', 0, 0, 0);
            INSERT INTO usage_daily VALUES
                (20254, 'claude_code', 1, 'local',
                 3, 2, 1, 3, 0, 1,
                 450, 250, 200,
                 880, 500, 300, 50, 20, 10,
                 0);",
        )
        .unwrap();

        conn
    }

    /// Create a consistent fixture with both Track A and Track B.
    fn setup_both_tracks_fixture() -> Connection {
        let conn = setup_track_a_fixture();

        conn.execute_batch(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL UNIQUE
            );
            INSERT INTO agents VALUES (1, 'claude_code');

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
                model_tier TEXT,
                service_tier TEXT,
                provider TEXT,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_creation_tokens INTEGER,
                thinking_tokens INTEGER,
                total_tokens INTEGER,
                estimated_cost_usd REAL,
                role TEXT NOT NULL,
                content_chars INTEGER NOT NULL,
                has_tool_calls INTEGER NOT NULL DEFAULT 0,
                tool_call_count INTEGER NOT NULL DEFAULT 0,
                data_source TEXT NOT NULL DEFAULT 'api',
                UNIQUE(message_id)
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
            );

            -- Insert matching token_usage for message 2 (the only api-sourced message).
            INSERT INTO token_usage VALUES
                (1, 2, 100, 1, 1, 'local', 1750000000001, 20254,
                 'claude-opus-4', 'opus', 'opus', NULL, 'anthropic',
                 500, 300, 50, 20, 10, 880, 0.05, 'assistant', 800, 1, 3, 'api');

            -- Token daily stats matching the token_usage.
            INSERT INTO token_daily_stats VALUES
                (20254, 'claude_code', 'local', 'opus',
                 1, 0, 1, 0,
                 500, 300, 50, 20, 10, 880,
                 800, 3, 0.05, 1, 0);",
        )
        .unwrap();

        conn
    }

    // -- Tests --

    #[test]
    fn consistent_track_a_passes() {
        let conn = setup_track_a_fixture();
        let config = ValidateConfig::deep();
        let report = run_validation(&conn, &config);

        // Track A checks should all pass.
        let track_a_checks: Vec<_> = report
            .checks
            .iter()
            .filter(|c| c.id.starts_with("track_a."))
            .collect();
        assert!(!track_a_checks.is_empty());
        for c in &track_a_checks {
            assert!(c.ok, "Check {} failed: {}", c.id, c.details);
        }
    }

    #[test]
    fn drifted_track_a_detects_mismatch() {
        let conn = setup_track_a_fixture();

        // Inject drift: change usage_daily content_tokens_est_total.
        conn.execute(
            "UPDATE usage_daily SET content_tokens_est_total = 9999 WHERE day_id = 20254",
            [],
        )
        .unwrap();

        let config = ValidateConfig::deep();
        let report = run_validation(&conn, &config);

        let content_check = report
            .checks
            .iter()
            .find(|c| c.id == "track_a.content_tokens_match")
            .expect("should have content tokens check");
        assert!(!content_check.ok, "Should detect content tokens mismatch");
        assert!(content_check.suggested_action.is_some());
    }

    #[test]
    fn drifted_track_a_message_count_detected() {
        let conn = setup_track_a_fixture();

        // Inject drift: change message_count.
        conn.execute(
            "UPDATE usage_daily SET message_count = 999 WHERE day_id = 20254",
            [],
        )
        .unwrap();

        let config = ValidateConfig::deep();
        let report = run_validation(&conn, &config);

        let msg_check = report
            .checks
            .iter()
            .find(|c| c.id == "track_a.message_count_match")
            .expect("should have message count check");
        assert!(!msg_check.ok);
    }

    #[test]
    fn consistent_both_tracks_passes() {
        let conn = setup_both_tracks_fixture();
        let config = ValidateConfig::deep();
        let report = run_validation(&conn, &config);

        assert!(
            report.all_ok(),
            "All checks should pass on consistent fixture: {:#?}",
            report
                .checks
                .iter()
                .filter(|c| !c.ok)
                .collect::<Vec<_>>()
        );
        assert!(report.drift.is_empty());
    }

    #[test]
    fn cross_track_drift_detected() {
        let conn = setup_both_tracks_fixture();

        // Inject drift: delete token_usage row (Track B ledger).
        conn.execute("DELETE FROM token_usage WHERE id = 1", [])
            .unwrap();
        // Also zero out token_daily_stats to be consistent with the deletion.
        conn.execute(
            "UPDATE token_daily_stats SET grand_total_tokens = 0 WHERE day_id = 20254",
            [],
        )
        .unwrap();

        let config = ValidateConfig::deep();
        let report = run_validation(&conn, &config);

        let drift_check = report
            .checks
            .iter()
            .find(|c| c.id == "cross_track.drift")
            .expect("should have cross-track drift check");
        // Track A has api_tokens_total=880 but Track B now has 0.
        assert!(!drift_check.ok, "Should detect cross-track drift");
        assert!(!report.drift.is_empty());
        assert_eq!(report.drift[0].track_a_total, 880);
        assert_eq!(report.drift[0].track_b_total, 0);
    }

    #[test]
    fn negative_counters_detected() {
        let conn = setup_track_a_fixture();

        // Inject negative counter.
        conn.execute(
            "UPDATE usage_daily SET tool_call_count = -5 WHERE day_id = 20254",
            [],
        )
        .unwrap();

        let config = ValidateConfig::deep();
        let report = run_validation(&conn, &config);

        let neg_check = report
            .checks
            .iter()
            .find(|c| c.id == "track_a.non_negative_counters")
            .expect("should have non-negative check");
        assert!(!neg_check.ok, "Should detect negative counters");
    }

    #[test]
    fn coverage_exceeding_message_count_detected() {
        let conn = setup_track_a_fixture();

        // Inject bad data: coverage > message count.
        conn.execute(
            "UPDATE usage_daily SET api_coverage_message_count = 999 WHERE day_id = 20254",
            [],
        )
        .unwrap();

        let config = ValidateConfig::deep();
        let report = run_validation(&conn, &config);

        let cov_check = report
            .checks
            .iter()
            .find(|c| c.id == "track_a.coverage_lte_messages")
            .expect("should have coverage <= messages check");
        assert!(!cov_check.ok);
    }

    #[test]
    fn empty_database_reports_missing_tables() {
        let conn = Connection::open_in_memory().unwrap();
        let config = ValidateConfig::default();
        let report = run_validation(&conn, &config);

        // Should have error-level checks about missing tables.
        let errors: Vec<_> = report
            .checks
            .iter()
            .filter(|c| !c.ok && c.severity == Severity::Error)
            .collect();
        assert!(!errors.is_empty());
    }

    #[test]
    fn sample_mode_limits_buckets() {
        let conn = setup_track_a_fixture();
        let config = ValidateConfig {
            sample_buckets: 1,
            ..Default::default()
        };
        let report = run_validation(&conn, &config);

        assert_eq!(report._meta.sampling.mode, "sample");
        // We only have 1 bucket anyway, but the mode should reflect sampling.
        assert!(report._meta.sampling.buckets_checked <= 1);
    }

    #[test]
    fn deep_mode_scans_all() {
        let conn = setup_track_a_fixture();
        let config = ValidateConfig::deep();
        let report = run_validation(&conn, &config);

        assert_eq!(report._meta.sampling.mode, "deep");
    }

    #[test]
    fn report_json_shape() {
        let conn = setup_track_a_fixture();
        let config = ValidateConfig::deep();
        let report = run_validation(&conn, &config);
        let json = report.to_json();

        assert!(json["checks"].is_array());
        assert!(json["drift"].is_array());
        assert!(json["_meta"]["elapsed_ms"].is_number());
        assert!(json["_meta"]["sampling"]["mode"].is_string());
    }

    #[test]
    fn perf_query_guardrail_completes() {
        let conn = setup_track_a_fixture();
        let m = perf_query_guardrail(&conn);
        assert!(m.within_budget, "Query should be within 500ms budget on fixture");
    }

    #[test]
    fn perf_breakdown_guardrail_completes() {
        let conn = setup_track_a_fixture();
        let m = perf_breakdown_guardrail(&conn);
        assert!(m.within_budget, "Breakdown should be within 200ms budget on fixture");
    }
}
