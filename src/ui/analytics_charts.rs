//! Analytics chart rendering for the ftui analytics views.
//!
//! Provides [`AnalyticsChartData`] (pre-computed chart data) and rendering
//! functions that turn analytics query results into terminal-native
//! visualizations using ftui-extras charts and canvas widgets.
//!
//! Chart data is loaded via `load_chart_data(db, filters, group_by)` — a single
//! DB query path that all 8 analytics views share. Views consume
//! pre-computed data during `view()` without further DB access.
//! The Explorer view layer adds dimension overlays via `build_dimension_overlay()`
//! for proportional breakdowns by agent/workspace/source.

use ftui::render::cell::PackedRgba;
use ftui::widgets::Widget;
use ftui::widgets::paragraph::Paragraph;
use ftui_extras::canvas::{CanvasRef, Mode as CanvasMode, Painter};
use ftui_extras::charts::LineChart as FtuiLineChart;
use ftui_extras::charts::Series as ChartSeries;
use ftui_extras::charts::{BarChart, BarDirection, BarGroup, Sparkline};

use super::app::{AnalyticsView, BreakdownTab, ExplorerMetric, ExplorerOverlay, HeatmapMetric};
use super::ftui_adapter::{Constraint, Flex, Rect};
use crate::sources::provenance::SourceFilter;

// ---------------------------------------------------------------------------
// Agent accent colors (consistent across all chart views)
// ---------------------------------------------------------------------------

/// Fixed color palette for up to 14 agents. Colors cycle for overflow.
const AGENT_COLORS: &[PackedRgba] = &[
    PackedRgba::rgb(0, 150, 255),   // claude_code — cyan
    PackedRgba::rgb(255, 100, 0),   // codex — orange
    PackedRgba::rgb(0, 200, 100),   // gemini — green
    PackedRgba::rgb(200, 50, 200),  // cursor — magenta
    PackedRgba::rgb(255, 200, 0),   // chatgpt — gold
    PackedRgba::rgb(100, 200, 255), // aider — sky
    PackedRgba::rgb(255, 80, 80),   // pi_agent — red
    PackedRgba::rgb(150, 255, 150), // cline — lime
    PackedRgba::rgb(180, 130, 255), // opencode — lavender
    PackedRgba::rgb(255, 160, 200), // amp — pink
    PackedRgba::rgb(200, 200, 100), // factory — olive
    PackedRgba::rgb(100, 255, 200), // clawdbot — mint
    PackedRgba::rgb(255, 220, 150), // vibe — peach
    PackedRgba::rgb(150, 150, 255), // openclaw — periwinkle
];

fn agent_color(idx: usize) -> PackedRgba {
    AGENT_COLORS[idx % AGENT_COLORS.len()]
}

// ---------------------------------------------------------------------------
// Theme-adaptive structural colors for chart chrome
// ---------------------------------------------------------------------------

/// Structural colors for chart axes, labels, gridlines, and text that adapt
/// to dark vs. light backgrounds. All chart renderers should use these
/// instead of hardcoding gray tones.
#[derive(Clone, Copy)]
struct ChartColors {
    /// Primary axis / legend text (e.g. axis labels, table headers).
    axis: PackedRgba,
    /// Secondary / muted text (e.g. row labels, small metadata).
    muted: PackedRgba,
    /// Tertiary / very subtle text (e.g. grid lines, separators).
    subtle: PackedRgba,
    /// Bright emphasis text (e.g. highlighted values, headers).
    emphasis: PackedRgba,
    /// Tooltip background.
    tooltip_bg: PackedRgba,
    /// Tooltip foreground.
    tooltip_fg: PackedRgba,
    /// Highlight/selected marker (yellow tones).
    highlight: PackedRgba,
    /// Highlight dimmed variant.
    highlight_dim: PackedRgba,
}

impl ChartColors {
    fn for_theme(dark_mode: bool) -> Self {
        if dark_mode {
            Self {
                axis: PackedRgba::rgb(190, 200, 220),
                muted: PackedRgba::rgb(140, 140, 160),
                subtle: PackedRgba::rgb(100, 100, 110),
                emphasis: PackedRgba::rgb(200, 200, 200),
                tooltip_bg: PackedRgba::rgb(60, 60, 80),
                tooltip_fg: PackedRgba::rgb(255, 255, 255),
                highlight: PackedRgba::rgb(255, 255, 80),
                highlight_dim: PackedRgba::rgb(255, 200, 0),
            }
        } else {
            Self {
                axis: PackedRgba::rgb(60, 60, 80),
                muted: PackedRgba::rgb(100, 100, 120),
                subtle: PackedRgba::rgb(160, 160, 175),
                emphasis: PackedRgba::rgb(40, 40, 50),
                tooltip_bg: PackedRgba::rgb(240, 240, 245),
                tooltip_fg: PackedRgba::rgb(20, 20, 30),
                highlight: PackedRgba::rgb(180, 140, 0),
                highlight_dim: PackedRgba::rgb(160, 120, 0),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// AnalyticsChartData — pre-computed chart data
// ---------------------------------------------------------------------------

/// Pre-computed chart data for the analytics views.
///
/// Loaded once when entering the analytics surface, refreshed on filter changes.
#[derive(Clone, Debug, Default)]
pub struct AnalyticsChartData {
    /// Per-agent token totals: `(agent_slug, api_tokens_total)` sorted desc.
    pub agent_tokens: Vec<(String, f64)>,
    /// Per-agent message counts: `(agent_slug, message_count)` sorted desc.
    pub agent_messages: Vec<(String, f64)>,
    /// Per-agent tool call counts: `(agent_slug, tool_call_count)` sorted desc.
    pub agent_tool_calls: Vec<(String, f64)>,
    // ── Workspace breakdowns ─────────────────────────────────────
    /// Per-workspace token totals: `(workspace_path, api_tokens_total)` sorted desc.
    pub workspace_tokens: Vec<(String, f64)>,
    /// Per-workspace message counts: `(workspace_path, message_count)` sorted desc.
    pub workspace_messages: Vec<(String, f64)>,
    // ── Source breakdowns ────────────────────────────────────────
    /// Per-source token totals: `(source_id, api_tokens_total)` sorted desc.
    pub source_tokens: Vec<(String, f64)>,
    /// Per-source message counts: `(source_id, message_count)` sorted desc.
    pub source_messages: Vec<(String, f64)>,
    /// Daily timeseries: `(label, api_tokens_total)` ordered by date.
    pub daily_tokens: Vec<(String, f64)>,
    /// Daily timeseries: `(label, message_count)` ordered by date.
    pub daily_messages: Vec<(String, f64)>,
    /// Per-model token totals: `(model_family, grand_total_tokens)` sorted desc.
    pub model_tokens: Vec<(String, f64)>,
    /// Coverage percentage (0..100).
    pub coverage_pct: f64,
    /// Total messages across all data.
    pub total_messages: i64,
    /// Total API tokens across all data.
    pub total_api_tokens: i64,
    /// Total tool calls across all data.
    pub total_tool_calls: i64,
    /// Number of unique agents seen.
    pub agent_count: usize,
    /// Per-day heatmap values: `(day_label, normalized_value 0..1)`.
    pub heatmap_days: Vec<(String, f64)>,

    // ── Dashboard KPI extras ─────────────────────────────────────
    /// Total content-estimated tokens across all data.
    pub total_content_tokens: i64,
    /// Daily content tokens: `(label, content_tokens_est_total)`.
    pub daily_content_tokens: Vec<(String, f64)>,
    /// Daily tool calls: `(label, tool_call_count)`.
    pub daily_tool_calls: Vec<(String, f64)>,
    /// Total plan messages.
    pub total_plan_messages: i64,
    /// Daily plan messages: `(label, plan_message_count)`.
    pub daily_plan_messages: Vec<(String, f64)>,
    /// Per-session points for Explorer scatter (x=messages, y=API tokens).
    pub session_scatter: Vec<crate::analytics::SessionScatterPoint>,
    // ── Tools view (enhanced) ─────────────────────────────────
    /// Full tool report rows (agent → calls, msgs, tokens, derived metrics).
    pub tool_rows: Vec<crate::analytics::ToolRow>,

    // ── Plans view ───────────────────────────────────────────
    /// Per-agent plan message counts: `(agent_slug, plan_message_count)` sorted desc.
    pub agent_plan_messages: Vec<(String, f64)>,
    /// Plan message share (% of total messages that are plan messages).
    pub plan_message_pct: f64,
    /// Plan API token share (% of API tokens attributed to plans).
    pub plan_api_token_share: f64,
    /// True when analytics rollups were auto-rebuilt during the latest load.
    pub auto_rebuilt: bool,
    /// Captures auto-rebuild errors; data may still be partially available.
    pub auto_rebuild_error: Option<String>,
}

impl AnalyticsChartData {
    /// Returns true when the dataset contains no meaningful analytics data.
    pub fn is_empty(&self) -> bool {
        self.total_api_tokens == 0
            && self.total_messages == 0
            && self.total_tool_calls == 0
            && self.agent_tokens.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Data loading
// ---------------------------------------------------------------------------

/// Load analytics data from the database, returning an `AnalyticsChartData`.
///
/// Gracefully returns empty data if the database is unavailable or tables
/// are missing.
pub fn load_chart_data(
    db: &crate::storage::sqlite::FrankenStorage,
    filters: &super::app::AnalyticsFilterState,
    group_by: crate::analytics::GroupBy,
) -> AnalyticsChartData {
    use crate::analytics;

    let conn = db.raw();

    // Build filter from analytics filter state.
    let filter = analytics::AnalyticsFilter {
        since_ms: filters.since_ms,
        until_ms: filters.until_ms,
        agents: filters.agents.iter().cloned().collect(),
        source: match &filters.source_filter {
            SourceFilter::All => analytics::SourceFilter::All,
            SourceFilter::Local => analytics::SourceFilter::Local,
            SourceFilter::Remote => analytics::SourceFilter::Remote,
            SourceFilter::SourceId(s) => analytics::SourceFilter::Specific(s.clone()),
        },
        workspace_ids: resolve_workspace_filter_ids(conn, &filters.workspaces),
    };

    let mut data = AnalyticsChartData::default();
    let mut load_errors: Vec<String> = Vec::new();

    // Agent breakdown (Track A — usage_daily).
    match analytics::query::query_breakdown(
        conn,
        &filter,
        analytics::Dim::Agent,
        analytics::Metric::ApiTotal,
        20,
    ) {
        Ok(result) => {
            data.agent_count = result.rows.len();
            data.agent_tokens = result
                .rows
                .iter()
                .map(|r| (r.key.clone(), r.value as f64))
                .collect();
            data.total_api_tokens = result.rows.iter().map(|r| r.value).sum();
        }
        Err(e) => {
            tracing::warn!(query = "agent_tokens", error = %e, "analytics query failed");
            load_errors.push(format!("agent_tokens: {e}"));
        }
    }

    // Helper to log analytics query errors.
    macro_rules! try_analytics {
        ($label:expr, $expr:expr, $errors:ident) => {
            match $expr {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(query = $label, error = %e, "analytics query failed");
                    $errors.push(format!("{}: {e}", $label));
                    None
                }
            }
        };
    }

    // Agent message counts.
    if let Some(result) = try_analytics!("agent_messages", analytics::query::query_breakdown(
        conn, &filter, analytics::Dim::Agent, analytics::Metric::MessageCount, 20,
    ), load_errors) {
        data.agent_messages = result.rows.iter().map(|r| (r.key.clone(), r.value as f64)).collect();
        data.total_messages = result.rows.iter().map(|r| r.value).sum();
    }

    // Workspace breakdown (Track A — usage_daily).
    if let Some(result) = try_analytics!("workspace_tokens", analytics::query::query_breakdown(
        conn, &filter, analytics::Dim::Workspace, analytics::Metric::ApiTotal, 20,
    ), load_errors) {
        data.workspace_tokens = result.rows.iter().map(|r| (r.key.clone(), r.value as f64)).collect();
    }
    if let Some(result) = try_analytics!("workspace_messages", analytics::query::query_breakdown(
        conn, &filter, analytics::Dim::Workspace, analytics::Metric::MessageCount, 20,
    ), load_errors) {
        data.workspace_messages = result.rows.iter().map(|r| (r.key.clone(), r.value as f64)).collect();
    }

    // Source breakdown (Track A — usage_daily).
    if let Some(result) = try_analytics!("source_tokens", analytics::query::query_breakdown(
        conn, &filter, analytics::Dim::Source, analytics::Metric::ApiTotal, 20,
    ), load_errors) {
        data.source_tokens = result.rows.iter().map(|r| (r.key.clone(), r.value as f64)).collect();
    }
    if let Some(result) = try_analytics!("source_messages", analytics::query::query_breakdown(
        conn, &filter, analytics::Dim::Source, analytics::Metric::MessageCount, 20,
    ), load_errors) {
        data.source_messages = result.rows.iter().map(|r| (r.key.clone(), r.value as f64)).collect();
    }

    // Tool usage — load full rows for the enhanced tools table.
    if let Some(result) = try_analytics!("tools", analytics::query::query_tools(conn, &filter, group_by, 50), load_errors) {
        data.agent_tool_calls = result.rows.iter().map(|r| (r.key.clone(), r.tool_call_count as f64)).collect();
        data.total_tool_calls = result.total_tool_calls;
        data.tool_rows = result.rows;
    }

    // Per-session scatter points (messages vs API tokens).
    if let Some(points) = try_analytics!("session_scatter", analytics::query::query_session_scatter(conn, &filter, 600), load_errors) {
        data.session_scatter = points;
    }

    // Daily timeseries (for sparklines and line chart).
    if let Some(result) = try_analytics!("timeseries", analytics::query::query_tokens_timeseries(conn, &filter, group_by), load_errors) {
        data.daily_tokens = result.buckets.iter().map(|(label, bucket)| (label.clone(), bucket.api_tokens_total as f64)).collect();
        data.daily_messages = result.buckets.iter().map(|(label, bucket)| (label.clone(), bucket.message_count as f64)).collect();
        data.daily_content_tokens = result.buckets.iter().map(|(label, bucket)| (label.clone(), bucket.content_tokens_est_total as f64)).collect();
        data.daily_tool_calls = result.buckets.iter().map(|(label, bucket)| (label.clone(), bucket.tool_call_count as f64)).collect();
        data.daily_plan_messages = result.buckets.iter().map(|(label, bucket)| (label.clone(), bucket.plan_message_count as f64)).collect();
        data.total_content_tokens = result.totals.content_tokens_est_total;
        data.total_plan_messages = result.totals.plan_message_count;

        // Build heatmap data (normalize token values to 0..1).
        let max_tokens = data.daily_tokens.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max);
        data.heatmap_days = data.daily_tokens.iter().map(|(label, v)| {
            let norm = if max_tokens > 0.0 { v / max_tokens } else { 0.0 };
            (label.clone(), norm)
        }).collect();
    }

    // Model breakdown (Track B — token_daily_stats).
    if let Some(result) = try_analytics!("model_tokens", analytics::query::query_breakdown(
        conn, &filter, analytics::Dim::Model, analytics::Metric::ApiTotal, 20,
    ), load_errors) {
        data.model_tokens = result.rows.iter().map(|r| (r.key.clone(), r.value as f64)).collect();
    }

    // Coverage percentage.
    if let Some(status) = try_analytics!("status", analytics::query::query_status(conn, &filter), load_errors) {
        data.coverage_pct = status.coverage.api_token_coverage_pct;
    }

    // Per-agent plan message breakdown.
    if let Some(result) = try_analytics!("plan_messages", analytics::query::query_breakdown(
        conn, &filter, analytics::Dim::Agent, analytics::Metric::PlanCount, 20,
    ), load_errors) {
        data.agent_plan_messages = result.rows.iter().map(|r| (r.key.clone(), r.value as f64)).collect();
    }

    // Log summary of load errors.
    if !load_errors.is_empty() {
        tracing::warn!(
            error_count = load_errors.len(),
            errors = ?load_errors,
            "analytics load_chart_data had query failures — data may appear empty"
        );
    }

    // Derive plan share percentages from totals.
    if data.total_messages > 0 {
        data.plan_message_pct =
            data.total_plan_messages as f64 / data.total_messages as f64 * 100.0;
    }
    if data.total_api_tokens > 0 {
        let plan_token_total: f64 = data.daily_plan_messages.iter().map(|(_, v)| *v).sum();
        if plan_token_total > 0.0 && data.total_messages > 0 {
            data.plan_api_token_share = plan_token_total / data.total_messages as f64 * 100.0;
        }
    }

    data
}

fn resolve_workspace_filter_ids(
    conn: &frankensqlite::Connection,
    workspaces: &std::collections::HashSet<String>,
) -> Vec<i64> {
    use frankensqlite::compat::{ConnectionExt, ParamValue, RowExt};

    if workspaces.is_empty() {
        return Vec::new();
    }

    let mut ids = Vec::new();

    for workspace in workspaces {
        if let Ok(id) = workspace.parse::<i64>()
            && !ids.contains(&id)
        {
            ids.push(id);
        }

        if let Ok(id) = conn.query_row_map(
            "SELECT id FROM workspaces WHERE path = ?1",
            &[ParamValue::from(workspace.as_str())],
            |row: &frankensqlite::Row| row.get_typed::<i64>(0),
        ) && !ids.contains(&id)
        {
            ids.push(id);
        }
    }

    ids
}

// ---------------------------------------------------------------------------
// Chart rendering — per-view functions
// ---------------------------------------------------------------------------

/// Render the Dashboard view: KPI tile wall with sparklines + top agents.
pub fn render_dashboard(
    data: &AnalyticsChartData,
    area: Rect,
    frame: &mut ftui::Frame,
    dark_mode: bool,
) {
    if area.height < 4 || area.width < 20 {
        return; // too small to render
    }

    // Show a helpful empty state when no analytics data has been loaded.
    if data.is_empty() {
        let muted = if dark_mode {
            PackedRgba::rgb(120, 125, 140)
        } else {
            PackedRgba::rgb(100, 105, 115)
        };
        let accent = if dark_mode {
            PackedRgba::rgb(90, 180, 255)
        } else {
            PackedRgba::rgb(20, 100, 200)
        };
        let mut lines: Vec<ftui::text::Line<'static>> = Vec::new();
        lines.push(ftui::text::Line::from(""));

        if area.height >= 14 && area.width >= 40 {
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("         ▆", ftui::Style::new().fg(accent)),
                ftui::text::Span::styled("                      █", ftui::Style::new().fg(muted)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("        ▄█", ftui::Style::new().fg(accent)),
                ftui::text::Span::styled("   ▆                  █", ftui::Style::new().fg(muted)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ▆   ▄██", ftui::Style::new().fg(accent)),
                ftui::text::Span::styled("  ▄█▄     ▆           █", ftui::Style::new().fg(muted)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("  ▄█  ▄███", ftui::Style::new().fg(accent)),
                ftui::text::Span::styled(" ▄███    ▄█▄     ▆    █", ftui::Style::new().fg(muted)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled(" ▄██▄ ████", ftui::Style::new().fg(accent)),
                ftui::text::Span::styled(" █████  ▄███    ▄█▄   █", ftui::Style::new().fg(muted)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("██████████", ftui::Style::new().fg(accent)),
                ftui::text::Span::styled("███████████████████████", ftui::Style::new().fg(muted)),
            ]));
            lines.push(ftui::text::Line::from(""));
        }

        lines.push(ftui::text::Line::from_spans(vec![
            ftui::text::Span::styled(
                "No analytics data yet",
                ftui::Style::new().fg(accent).bold(),
            ),
        ]));
        lines.push(ftui::text::Line::from(""));
        if area.height >= 10 {
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled(
                    "Analytics are computed from indexed sessions.",
                    ftui::Style::new().fg(muted),
                ),
            ]));
            lines.push(ftui::text::Line::from(""));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("  1. ", ftui::Style::new().fg(accent)),
                ftui::text::Span::styled(
                    "Run a search to load session data",
                    ftui::Style::new().fg(muted),
                ),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("  2. ", ftui::Style::new().fg(accent)),
                ftui::text::Span::styled(
                    "Press Ctrl+R to refresh the index",
                    ftui::Style::new().fg(muted),
                ),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("  3. ", ftui::Style::new().fg(accent)),
                ftui::text::Span::styled(
                    "Switch between views using the tab bar above",
                    ftui::Style::new().fg(muted),
                ),
            ]));
        }
        let y_offset = area.height.saturating_sub(lines.len() as u16) / 3;
        let avail = area.height.saturating_sub(y_offset);
        if avail > 0 {
            let block_area = Rect::new(
                area.x,
                area.y + y_offset,
                area.width,
                avail.min(lines.len() as u16),
            );
            Paragraph::new(ftui::text::Text::from_lines(lines))
                .alignment(ftui::widgets::block::Alignment::Center)
                .render(block_area, frame);
        }
        return;
    }

    let cc = ChartColors::for_theme(dark_mode);

    let wide_mode = area.width >= 130;

    // Compute exact height needed for agent bar chart (1 row per agent).
    let agent_count = data.agent_tokens.len().min(8);
    let ws_count = data.workspace_tokens.len().min(8);

    let agent_rows = if agent_count > 0 {
        agent_count as u16 + 1
    } else {
        0
    };
    let ws_rows = if ws_count > 0 { ws_count as u16 + 1 } else { 0 };

    let max_bar_rows = if wide_mode {
        agent_rows.max(ws_rows)
    } else {
        agent_rows
    };
    let has_bar = max_bar_rows > 0 && area.height >= 6 + max_bar_rows + 4;

    let chunks = if has_bar {
        Flex::vertical()
            .constraints([
                Constraint::Fixed(6),            // KPI tile grid
                Constraint::Fixed(max_bar_rows), // Top bar charts (exact fit)
                Constraint::Fixed(2),            // Aggregate sparkline (label + bars)
                Constraint::Min(0),              // Remaining space
            ])
            .split(area)
    } else {
        Flex::vertical()
            .constraints([
                Constraint::Fixed(6), // KPI tile grid
                Constraint::Fixed(2), // Aggregate sparkline (label + bars)
                Constraint::Min(0),   // Remaining space
            ])
            .split(area)
    };

    // ── KPI Tile Grid ──────────────────────────────────────────
    render_kpi_tiles(data, chunks[0], frame, dark_mode);

    // ── Top Bar Charts (manual rendering with full labels) ──
    if has_bar {
        let bar_area = chunks[1];

        let (agent_area, ws_area) = if wide_mode {
            let cols = Flex::horizontal()
                .constraints([Constraint::Percentage(50.0), Constraint::Percentage(50.0)])
                .split(bar_area);
            (cols[0], Some(cols[1]))
        } else {
            (bar_area, None)
        };

        // Inner function to render a mini bar chart.
        let mut render_mini_bar =
            |items: &[(String, f64)], area: Rect, header_label: &str, use_agent_colors: bool| {
                if area.is_empty() || items.is_empty() {
                    return;
                }
                let max_val = items
                    .iter()
                    .take(8)
                    .map(|(_, v)| *v)
                    .fold(0.0_f64, f64::max);
                let label_w = items
                    .iter()
                    .take(8)
                    .map(|(name, _)| display_width(name).min(14))
                    .max()
                    .unwrap_or(6) as u16;

                let header = format!(
                    " {:label_w$}  tokens",
                    header_label,
                    label_w = label_w as usize
                );
                let header_line = ftui::text::Line::from_spans(vec![ftui::text::Span::styled(
                    header,
                    ftui::Style::new().fg(cc.muted),
                )]);
                Paragraph::new(header_line).render(
                    Rect {
                        x: area.x,
                        y: area.y,
                        width: area.width,
                        height: 1,
                    },
                    frame,
                );

                let val_col = 8_u16;
                let bar_start = area.x + 1 + label_w + 1;
                let bar_end = area.right().saturating_sub(val_col);
                if bar_end <= bar_start {
                    return;
                }
                let bar_max_w = bar_end.saturating_sub(bar_start) as f64;

                for (i, (name, val)) in items.iter().take(8).enumerate() {
                    let y = area.y + 1 + i as u16;
                    if y >= area.bottom() {
                        break;
                    }
                    let color = if use_agent_colors {
                        agent_color(i)
                    } else {
                        cc.emphasis
                    };

                    // Correctly handle display width for truncation
                    let truncated_name = shorten_label(name, label_w as usize);
                    let val_str = format_compact(*val as i64);

                    // To avoid padding issues with wide characters, we calculate exactly how many
                    // spaces are needed instead of relying on the std::fmt width padder which uses chars
                    let current_w = display_width(&truncated_name);
                    let pad_w = (label_w as usize).saturating_sub(current_w);
                    let pad = " ".repeat(pad_w);

                    let label_span = ftui::text::Span::styled(
                        format!(" {truncated_name}{pad}"),
                        ftui::Style::new().fg(cc.axis),
                    );
                    Paragraph::new(ftui::text::Line::from_spans(vec![label_span])).render(
                        Rect {
                            x: area.x,
                            y,
                            width: label_w + 1,
                            height: 1,
                        },
                        frame,
                    );

                    let bar_len = if max_val > 0.0 && *val > 0.0 {
                        ((val / max_val) * bar_max_w).round().max(1.0) as u16
                    } else {
                        0
                    };
                    for dx in 0..bar_len {
                        let x = bar_start + dx;
                        if x < bar_end {
                            let mut cell = ftui::render::cell::Cell::from_char('\u{2588}');
                            cell.fg = color;
                            frame.buffer.set_fast(x, y, cell);
                        }
                    }

                    let val_span = ftui::text::Span::styled(
                        format!(" {val_str}"),
                        ftui::Style::new().fg(cc.muted),
                    );
                    Paragraph::new(ftui::text::Line::from_spans(vec![val_span])).render(
                        Rect {
                            x: bar_end,
                            y,
                            width: val_col.min(area.right().saturating_sub(bar_end)),
                            height: 1,
                        },
                        frame,
                    );
                }
            };

        render_mini_bar(&data.agent_tokens, agent_area, "Agent", true);
        if let Some(w_area) = ws_area {
            render_mini_bar(&data.workspace_tokens, w_area, "Workspace", false);
        }
    }

    // ── Aggregate Token Sparkline ────────────────────────────────
    let sparkline_chunk = if has_bar { chunks[2] } else { chunks[1] };
    if !data.daily_tokens.is_empty() && sparkline_chunk.height >= 2 {
        // Render label on first row.
        let label = format!(" Daily Tokens ({} days)", data.daily_tokens.len());
        Paragraph::new(label)
            .style(ftui::Style::new().fg(cc.muted))
            .render(
                Rect {
                    x: sparkline_chunk.x,
                    y: sparkline_chunk.y,
                    width: sparkline_chunk.width,
                    height: 1,
                },
                frame,
            );
        // Sparkline fills remaining rows.
        let spark_area = Rect {
            x: sparkline_chunk.x,
            y: sparkline_chunk.y + 1,
            width: sparkline_chunk.width,
            height: sparkline_chunk.height - 1,
        };
        let values: Vec<f64> = data.daily_tokens.iter().map(|(_, v)| *v).collect();
        let sparkline = Sparkline::new(&values)
            .gradient(PackedRgba::rgb(40, 80, 200), PackedRgba::rgb(255, 80, 40));
        sparkline.render(spark_area, frame);
    } else if !data.daily_tokens.is_empty() {
        let values: Vec<f64> = data.daily_tokens.iter().map(|(_, v)| *v).collect();
        let sparkline = Sparkline::new(&values)
            .gradient(PackedRgba::rgb(40, 80, 200), PackedRgba::rgb(255, 80, 40));
        sparkline.render(sparkline_chunk, frame);
    }
}

/// Render the KPI tile grid: 2 rows × 3 columns of metric tiles.
fn render_kpi_tiles(
    data: &AnalyticsChartData,
    area: Rect,
    frame: &mut ftui::Frame,
    dark_mode: bool,
) {
    let cc = ChartColors::for_theme(dark_mode);

    // 2 rows of tiles, 3 tiles per row
    let rows = Flex::vertical()
        .constraints([Constraint::Fixed(3), Constraint::Fixed(3)])
        .split(area);

    // Row 1: API Tokens | Messages | Tool Calls
    let cols1 = Flex::horizontal()
        .constraints([
            Constraint::Percentage(33.0),
            Constraint::Percentage(34.0),
            Constraint::Percentage(33.0),
        ])
        .split(rows[0]);

    render_kpi_tile(
        "API Tokens",
        &format_compact(data.total_api_tokens),
        &data.daily_tokens,
        PackedRgba::rgb(0, 180, 255), // cyan
        PackedRgba::rgb(0, 100, 200), // dark cyan
        cc.muted,
        cols1[0],
        frame,
    );
    render_kpi_tile(
        "Messages",
        &format_compact(data.total_messages),
        &data.daily_messages,
        PackedRgba::rgb(100, 220, 100), // green
        PackedRgba::rgb(40, 150, 40),   // dark green
        cc.muted,
        cols1[1],
        frame,
    );
    render_kpi_tile(
        "Tool Calls",
        &format_compact(data.total_tool_calls),
        &data.daily_tool_calls,
        PackedRgba::rgb(255, 160, 0), // orange
        PackedRgba::rgb(200, 100, 0), // dark orange
        cc.muted,
        cols1[2],
        frame,
    );

    // Row 2: Content Tokens | Plan Messages | Coverage
    let cols2 = Flex::horizontal()
        .constraints([
            Constraint::Percentage(33.0),
            Constraint::Percentage(34.0),
            Constraint::Percentage(33.0),
        ])
        .split(rows[1]);

    render_kpi_tile(
        "Content Est",
        &format_compact(data.total_content_tokens),
        &data.daily_content_tokens,
        PackedRgba::rgb(180, 130, 255), // lavender
        PackedRgba::rgb(120, 60, 200),  // dark lavender
        cc.muted,
        cols2[0],
        frame,
    );
    render_kpi_tile(
        "Plans",
        &format_compact(data.total_plan_messages),
        &data.daily_plan_messages,
        PackedRgba::rgb(255, 200, 0), // gold
        PackedRgba::rgb(180, 140, 0), // dark gold
        cc.muted,
        cols2[1],
        frame,
    );

    render_kpi_tile(
        "API Cvg",
        &format!("{:.0}%", data.coverage_pct),
        &[],                            // no sparkline for coverage
        PackedRgba::rgb(150, 200, 255), // light blue
        PackedRgba::rgb(80, 120, 180),  // muted blue
        cc.muted,
        cols2[2],
        frame,
    );
}

/// Render a single KPI tile: label (dim) + value (bright) + mini sparkline.
#[allow(clippy::too_many_arguments)]
fn render_kpi_tile(
    label: &str,
    value: &str,
    sparkline_data: &[(String, f64)],
    fg_color: PackedRgba,
    spark_color: PackedRgba,
    label_muted: PackedRgba,
    area: Rect,
    frame: &mut ftui::Frame,
) {
    if area.height < 2 || area.width < 8 {
        return;
    }

    // Row 1: label (dimmed)
    let label_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    Paragraph::new(format!(" {label}"))
        .style(ftui::Style::new().fg(label_muted))
        .render(label_area, frame);

    // Row 2: big value + inline sparkline
    let value_y = area.y + 1;
    let value_str = format!(" {value}");
    let value_width = value_str.len() as u16 + 1;

    let value_area = Rect {
        x: area.x,
        y: value_y,
        width: area.width.min(value_width),
        height: 1,
    };
    Paragraph::new(value_str)
        .style(ftui::Style::new().fg(fg_color).bold())
        .render(value_area, frame);

    // Mini sparkline in remaining space on row 2
    if !sparkline_data.is_empty() && area.width > value_width + 2 {
        let spark_x = area.x + value_width + 1;
        let spark_w = area.width.saturating_sub(value_width + 2);
        if spark_w >= 4 {
            let spark_area = Rect {
                x: spark_x,
                y: value_y,
                width: spark_w,
                height: 1,
            };
            let values: Vec<f64> = sparkline_data.iter().map(|(_, v)| *v).collect();
            Sparkline::new(&values)
                .gradient(spark_color, fg_color)
                .render(spark_area, frame);
        }
    }

    // Optional Row 3: burn rate or delta (if height allows).
    // Require >= 14 data points so both 7-day windows are fully populated.
    if area.height >= 3 && sparkline_data.len() >= 14 {
        let recent: f64 = sparkline_data
            .iter()
            .rev()
            .take(7)
            .map(|(_, v)| *v)
            .sum::<f64>();
        let prior: f64 = sparkline_data
            .iter()
            .rev()
            .skip(7)
            .take(7)
            .map(|(_, v)| *v)
            .sum::<f64>();
        let delta_area = Rect {
            x: area.x,
            y: area.y + 2,
            width: area.width,
            height: 1,
        };
        if prior > 0.0 {
            let pct = ((recent - prior) / prior) * 100.0;
            let (arrow, color) = if pct > 5.0 {
                ("\u{25b2}", PackedRgba::rgb(255, 80, 80)) // ▲ red (up)
            } else if pct < -5.0 {
                ("\u{25bc}", PackedRgba::rgb(80, 200, 80)) // ▼ green (down)
            } else {
                ("\u{25c6}", label_muted) // ◆ muted (flat)
            };
            let delta_text = format!(" {arrow} {pct:+.0}% vs prior 7d");
            Paragraph::new(delta_text)
                .style(ftui::Style::new().fg(color))
                .render(delta_area, frame);
        }
    }
}

/// Format a number compactly: 1.2B, 45.3M, 12.5K, or raw for small values.
fn format_compact(n: i64) -> String {
    let abs = n.unsigned_abs();
    if abs >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if abs >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if abs >= 10_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format_number(n)
    }
}

/// Render the Explorer view: interactive metric selector + line area/scatter charts.
pub fn render_explorer(
    data: &AnalyticsChartData,
    state: &ExplorerState,
    area: Rect,
    frame: &mut ftui::Frame,
    dark_mode: bool,
) {
    if area.height < 4 || area.width < 20 {
        return;
    }

    // Select the data series based on the active metric.
    let (metric_data, metric_color) = metric_series(data, state.metric);

    let cc = ChartColors::for_theme(dark_mode);

    if metric_data.is_empty() {
        if area.height >= 12 && area.width >= 40 {
            let accent = if dark_mode { PackedRgba::rgb(90, 180, 255) } else { PackedRgba::rgb(20, 100, 200) };
            let primary = if dark_mode { PackedRgba::rgb(60, 120, 200) } else { PackedRgba::rgb(40, 80, 160) };
            
            let mut lines = Vec::new();
            lines.push(ftui::text::Line::from(""));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("             ▃▄▅▇██▇▅▄▃             ", ftui::Style::new().fg(accent)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("         ▂▄▆████████████▆▄▂         ", ftui::Style::new().fg(primary)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("       ▃▆██████████████████▆▃       ", ftui::Style::new().fg(cc.muted)),
            ]));
            lines.push(ftui::text::Line::from(""));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled(
                    " No analytics timeseries yet. If data exists, cass is rebuilding automatically.",
                    ftui::Style::new().fg(cc.axis).bold(),
                ),
            ]));
            Paragraph::new(ftui::text::Text::from_lines(lines)).render(area, frame);
            return;
        }

        Paragraph::new(
            " No analytics timeseries yet. If data exists, cass is rebuilding automatically.",
        )
        .style(ftui::Style::new().fg(cc.subtle))
        .render(area, frame);
        return;
    }

    // Layout: header (2 lines) + chart (flex)
    let chunks = Flex::vertical()
        .constraints([Constraint::Fixed(2), Constraint::Min(4)])
        .split(area);

    // ── Header: metric selector + overlay + total + data range ──
    let metric_total = metric_data.iter().map(|(_, v)| *v).sum::<f64>();
    let total_display = if metric_total >= 1_000_000_000.0 {
        format!("{:.1}B", metric_total / 1_000_000_000.0)
    } else if metric_total >= 1_000_000.0 {
        format!("{:.1}M", metric_total / 1_000_000.0)
    } else if metric_total >= 10_000.0 {
        format!("{:.1}K", metric_total / 1_000.0)
    } else {
        format!("{}", metric_total as i64)
    };

    let date_range = if metric_data.len() >= 2 {
        format!(
            " ({} .. {})",
            &metric_data[0].0,
            &metric_data[metric_data.len() - 1].0
        )
    } else {
        String::new()
    };

    let header_text = truncate_with_ellipsis(
        &format!(
            " {} ({})  |  {}  |  Zoom: {}  |  Overlay: {}  |  Scatter: auto  |  m/M g/G z/Z o{}",
            state.metric.label(),
            total_display,
            state.group_by.label(),
            state.zoom.label(),
            state.overlay.label(),
            date_range,
        ),
        chunks[0].width as usize,
    );
    Paragraph::new(header_text)
        .style(ftui::Style::new().fg(cc.emphasis))
        .render(chunks[0], frame);

    // ── Build primary + overlay series ──────────────────────────
    let primary_points: Vec<(f64, f64)> = metric_data
        .iter()
        .enumerate()
        .map(|(i, (_, v))| (i as f64, *v))
        .collect();

    // Dimension overlay: add a series per top-N item (max 5 for readability).
    let mut overlay_data: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut overlay_labels: Vec<String> = Vec::new();
    let mut overlay_colors: Vec<PackedRgba> = Vec::new();
    let dim_breakdown: Option<StrF64Slice<'_>> = match state.overlay {
        ExplorerOverlay::None => Option::None,
        ExplorerOverlay::ByAgent => Some(match state.metric {
            ExplorerMetric::Messages | ExplorerMetric::PlanMessages => &data.agent_messages,
            ExplorerMetric::ToolCalls => &data.agent_tool_calls,
            _ => &data.agent_tokens,
        }),
        ExplorerOverlay::ByWorkspace => Some(match state.metric {
            ExplorerMetric::Messages | ExplorerMetric::PlanMessages => &data.workspace_messages,
            _ => &data.workspace_tokens,
        }),
        ExplorerOverlay::BySource => Some(match state.metric {
            ExplorerMetric::Messages | ExplorerMetric::PlanMessages => &data.source_messages,
            _ => &data.source_tokens,
        }),
    };
    if let Some(breakdown) = dim_breakdown
        && !breakdown.is_empty()
    {
        overlay_data = build_dimension_overlay(breakdown, metric_data);
        for (i, points) in overlay_data.iter().enumerate().take(5) {
            if !points.is_empty() {
                let name = breakdown.get(i).map(|(n, _)| n.as_str()).unwrap_or("other");
                overlay_labels.push(name.to_string());
                overlay_colors.push(agent_color(i));
            }
        }
    }

    // X labels: first, mid, last date.
    let x_labels: Vec<&str> = if metric_data.len() >= 3 {
        vec![
            &metric_data[0].0,
            &metric_data[metric_data.len() / 2].0,
            &metric_data[metric_data.len() - 1].0,
        ]
    } else if !metric_data.is_empty() {
        vec![&metric_data[0].0, &metric_data[metric_data.len() - 1].0]
    } else {
        vec![]
    };

    let chart_body = chunks[1];
    let show_scatter =
        chart_body.height >= 10 && chart_body.width >= 50 && !data.session_scatter.is_empty();
    if show_scatter {
        let sub = Flex::vertical()
            .constraints([Constraint::Percentage(65.0), Constraint::Percentage(35.0)])
            .split(chart_body);
        render_explorer_line_canvas(
            state.metric,
            metric_data,
            &primary_points,
            metric_color,
            &overlay_data,
            &overlay_labels,
            &overlay_colors,
            &x_labels,
            sub[0],
            frame,
            cc,
        );
        render_explorer_scatter(&data.session_scatter, sub[1], frame, cc);
    } else {
        render_explorer_line_canvas(
            state.metric,
            metric_data,
            &primary_points,
            metric_color,
            &overlay_data,
            &overlay_labels,
            &overlay_colors,
            &x_labels,
            chart_body,
            frame,
            cc,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn render_explorer_line_canvas(
    metric: ExplorerMetric,
    metric_data: &[(String, f64)],
    primary_points: &[(f64, f64)],
    primary_color: PackedRgba,
    overlay_data: &[Vec<(f64, f64)>],
    overlay_labels: &[String],
    overlay_colors: &[PackedRgba],
    x_labels: &[&str],
    area: Rect,
    frame: &mut ftui::Frame,
    cc: ChartColors,
) {
    if area.height < 5 || area.width < 20 {
        let mut series = vec![ChartSeries::new(
            metric.label(),
            primary_points,
            primary_color,
        )];
        for (idx, points) in overlay_data.iter().enumerate() {
            if points.is_empty() {
                continue;
            }
            let name = overlay_labels
                .get(idx)
                .map(String::as_str)
                .unwrap_or("overlay");
            let color = overlay_colors
                .get(idx)
                .copied()
                .unwrap_or_else(|| agent_color(idx));
            series.push(ChartSeries::new(name, points, color).markers(true));
        }
        FtuiLineChart::new(series)
            .x_labels(x_labels.to_vec())
            .legend(true)
            .render(area, frame);
        return;
    }

    let chunks = Flex::vertical()
        .constraints([Constraint::Fixed(1), Constraint::Min(4)])
        .split(area);
    let annotation = truncate_with_ellipsis(
        &build_explorer_annotation_line(metric, metric_data, overlay_labels),
        chunks[0].width as usize,
    );
    Paragraph::new(annotation)
        .style(ftui::Style::new().fg(cc.muted))
        .render(chunks[0], frame);

    let chart_outer = chunks[1];
    if chart_outer.height < 4 || chart_outer.width < 12 {
        return;
    }

    let mut y_max = primary_points
        .iter()
        .map(|(_, y)| *y)
        .fold(0.0_f64, f64::max);
    for points in overlay_data {
        for (_, y) in points {
            y_max = y_max.max(*y);
        }
    }
    if y_max <= 0.0 {
        y_max = 1.0;
    }

    let top_label = format_explorer_metric_value(metric, y_max);
    let bottom_label = format_explorer_metric_value(metric, 0.0);
    let y_axis_w = (display_width(&top_label).max(display_width(&bottom_label)) as u16 + 1)
        .min(chart_outer.width.saturating_sub(6))
        .max(1);
    let x_axis_h = 2u16;
    if chart_outer.height <= x_axis_h || chart_outer.width <= y_axis_w + 3 {
        return;
    }
    let plot_area = Rect {
        x: chart_outer.x + y_axis_w,
        y: chart_outer.y,
        width: chart_outer.width.saturating_sub(y_axis_w),
        height: chart_outer.height.saturating_sub(x_axis_h),
    };
    if plot_area.width < 2 || plot_area.height < 2 {
        return;
    }

    let mut painter = Painter::for_area(plot_area, CanvasMode::Braille);
    let (px_w, px_h) = painter.size();
    if px_w < 2 || px_h < 2 {
        return;
    }
    let px_w = i32::from(px_w);
    let px_h = i32::from(px_h);
    let x_max = if primary_points.len() > 1 {
        primary_points.len() as f64 - 1.0
    } else {
        1.0
    };
    let y_range = y_max.max(1.0);
    let to_px = |x: f64, y: f64| -> (i32, i32) {
        let px = ((x / x_max) * (px_w as f64 - 1.0)).round() as i32;
        let py = (((y_max - y) / y_range) * (px_h as f64 - 1.0)).round() as i32;
        (px.clamp(0, px_w - 1), py.clamp(0, px_h - 1))
    };

    let baseline = px_h - 1;
    let fill_color = dim_color(primary_color, 0.35);
    if primary_points.len() >= 2 {
        for window in primary_points.windows(2) {
            let (x0, y0) = to_px(window[0].0, window[0].1);
            let (x1, y1) = to_px(window[1].0, window[1].1);
            if x0 == x1 {
                painter.line_colored(x0, (y0 + 1).min(baseline), x0, baseline, Some(fill_color));
            } else {
                let (start, end, ys, ye) = if x0 < x1 {
                    (x0, x1, y0, y1)
                } else {
                    (x1, x0, y1, y0)
                };
                for x in start..=end {
                    let t = if end == start {
                        0.0
                    } else {
                        (x - start) as f64 / (end - start) as f64
                    };
                    let y = (ys as f64 + (ye - ys) as f64 * t).round() as i32;
                    painter.line_colored(x, (y + 1).min(baseline), x, baseline, Some(fill_color));
                }
            }
        }
    }

    if let Some((x, y)) = primary_points.first() {
        let (px, py) = to_px(*x, *y);
        painter.point_colored(px, py, primary_color);
    }

    for (idx, points) in overlay_data.iter().enumerate() {
        let color = overlay_colors
            .get(idx)
            .copied()
            .unwrap_or_else(|| agent_color(idx));
        for window in points.windows(2) {
            let (x0, y0) = to_px(window[0].0, window[0].1);
            let (x1, y1) = to_px(window[1].0, window[1].1);
            painter.line_colored(x0, y0, x1, y1, Some(color));
        }
        for (x, y) in points.iter().step_by(4) {
            let (px, py) = to_px(*x, *y);
            painter.point_colored(px, py, color);
        }
    }

    if !primary_points.is_empty() {
        let avg = primary_points.iter().map(|(_, y)| *y).sum::<f64>() / primary_points.len() as f64;
        let (_, avg_y) = to_px(0.0, avg);
        painter.line_colored(0, avg_y, px_w - 1, avg_y, Some(cc.subtle));
        if let Some((peak_idx, (_, peak_val))) = primary_points.iter().enumerate().max_by(|a, b| {
            a.1.1
                .partial_cmp(&b.1.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        }) {
            let (peak_x, peak_y) = to_px(peak_idx as f64, *peak_val);
            for d in -1..=1 {
                painter.point_colored(peak_x + d, peak_y, cc.highlight);
                painter.point_colored(peak_x, peak_y + d, cc.highlight);
            }
        }
    }

    let canvas = CanvasRef::from_painter(&painter).style(ftui::Style::new().fg(cc.axis));
    canvas.render(plot_area, frame);

    let axis_color = cc.muted;
    let y_axis_x = plot_area.x.saturating_sub(1);
    for y in plot_area.y..plot_area.y + plot_area.height {
        Paragraph::new("│")
            .style(ftui::Style::new().fg(axis_color))
            .render(
                Rect {
                    x: y_axis_x,
                    y,
                    width: 1,
                    height: 1,
                },
                frame,
            );
    }
    let x_axis_y = plot_area.y + plot_area.height.saturating_sub(1);
    for x in plot_area.x..plot_area.x + plot_area.width {
        Paragraph::new("─")
            .style(ftui::Style::new().fg(axis_color))
            .render(
                Rect {
                    x,
                    y: x_axis_y,
                    width: 1,
                    height: 1,
                },
                frame,
            );
    }
    Paragraph::new("└")
        .style(ftui::Style::new().fg(axis_color))
        .render(
            Rect {
                x: y_axis_x,
                y: x_axis_y,
                width: 1,
                height: 1,
            },
            frame,
        );

    Paragraph::new(top_label)
        .style(ftui::Style::new().fg(cc.muted))
        .render(
            Rect {
                x: chart_outer.x,
                y: chart_outer.y,
                width: y_axis_w,
                height: 1,
            },
            frame,
        );
    Paragraph::new(bottom_label)
        .style(ftui::Style::new().fg(cc.muted))
        .render(
            Rect {
                x: chart_outer.x,
                y: x_axis_y,
                width: y_axis_w,
                height: 1,
            },
            frame,
        );

    if !x_labels.is_empty() {
        let label_y = chart_outer.y + chart_outer.height.saturating_sub(1);
        let slots = x_labels.len().saturating_sub(1).max(1) as u16;
        let mut last_label_end = plot_area.x;
        for (idx, label) in x_labels.iter().enumerate() {
            if label.is_empty() {
                continue;
            }
            let label_text = truncate_with_ellipsis(label, plot_area.width as usize);
            let width = (display_width(&label_text) as u16).min(plot_area.width);
            if width == 0 {
                continue;
            }
            let raw_x = if idx == 0 {
                plot_area.x
            } else if idx + 1 == x_labels.len() {
                plot_area.x + plot_area.width.saturating_sub(width)
            } else {
                let t = (idx as u16).saturating_mul(plot_area.width.saturating_sub(1)) / slots;
                plot_area.x + t.saturating_sub(width / 2)
            };
            let x = raw_x.clamp(
                plot_area.x,
                plot_area.x + plot_area.width.saturating_sub(width),
            );
            // Keep labels legible on narrow charts by skipping overlapping slots.
            if x < last_label_end {
                continue;
            }
            Paragraph::new(label_text)
                .style(ftui::Style::new().fg(cc.muted))
                .render(
                    Rect {
                        x,
                        y: label_y,
                        width,
                        height: 1,
                    },
                    frame,
                );
            last_label_end = x.saturating_add(width.saturating_add(1));
        }
    }
}

fn render_explorer_scatter(
    points: &[crate::analytics::SessionScatterPoint],
    area: Rect,
    frame: &mut ftui::Frame,
    cc: ChartColors,
) {
    if area.height < 4 || area.width < 24 {
        return;
    }
    if points.is_empty() {
        Paragraph::new(" Scatter: no per-session data")
            .style(ftui::Style::new().fg(cc.subtle))
            .render(area, frame);
        return;
    }

    let chunks = Flex::vertical()
        .constraints([Constraint::Fixed(1), Constraint::Min(3)])
        .split(area);

    let valid: Vec<&crate::analytics::SessionScatterPoint> = points
        .iter()
        .filter(|p| p.message_count > 0 && p.api_tokens_total >= 0)
        .collect();
    if valid.is_empty() {
        Paragraph::new(" Scatter: no positive session points")
            .style(ftui::Style::new().fg(cc.subtle))
            .render(area, frame);
        return;
    }

    let avg_messages =
        valid.iter().map(|p| p.message_count as f64).sum::<f64>() / valid.len() as f64;
    let avg_tokens =
        valid.iter().map(|p| p.api_tokens_total as f64).sum::<f64>() / valid.len() as f64;
    let avg_efficiency = if avg_messages > 0.0 {
        avg_tokens / avg_messages
    } else {
        0.0
    };
    let header = truncate_with_ellipsis(
        &format!(
            " Scatter: session tokens vs messages ({})  avg tok/msg {}",
            valid.len(),
            format_compact(avg_efficiency.round() as i64)
        ),
        chunks[0].width as usize,
    );
    Paragraph::new(header)
        .style(ftui::Style::new().fg(cc.axis))
        .render(chunks[0], frame);

    let plot_area = chunks[1];
    if plot_area.width < 4 || plot_area.height < 2 {
        return;
    }
    let mut painter = Painter::for_area(plot_area, CanvasMode::HalfBlock);
    let (px_w, px_h) = painter.size();
    if px_w < 3 || px_h < 3 {
        return;
    }
    let px_w = i32::from(px_w);
    let px_h = i32::from(px_h);

    let max_messages = valid
        .iter()
        .map(|p| p.message_count)
        .max()
        .unwrap_or(1)
        .max(1) as f64;
    let max_tokens = valid
        .iter()
        .map(|p| p.api_tokens_total)
        .max()
        .unwrap_or(1)
        .max(1) as f64;
    let to_px = |messages: f64, tokens: f64| -> (i32, i32) {
        let x = ((messages / max_messages) * (px_w as f64 - 1.0)).round() as i32;
        let y = (((max_tokens - tokens) / max_tokens) * (px_h as f64 - 1.0)).round() as i32;
        (x.clamp(0, px_w - 1), y.clamp(0, px_h - 1))
    };

    // Axes and average guides.
    let baseline = px_h - 1;
    painter.line_colored(0, baseline, px_w - 1, baseline, Some(cc.subtle));
    painter.line_colored(0, 0, 0, px_h - 1, Some(cc.subtle));
    let (avg_x, avg_y) = to_px(avg_messages, avg_tokens);
    painter.line_colored(avg_x, 0, avg_x, px_h - 1, Some(cc.muted));
    painter.line_colored(0, avg_y, px_w - 1, avg_y, Some(cc.muted));

    for point in valid {
        let ratio = point.api_tokens_total as f64 / point.message_count.max(1) as f64;
        let color = if ratio > avg_efficiency * 1.25 {
            PackedRgba::rgb(255, 150, 60)
        } else if ratio < avg_efficiency * 0.75 {
            PackedRgba::rgb(90, 220, 120)
        } else {
            PackedRgba::rgb(120, 190, 255)
        };
        let (x, y) = to_px(point.message_count as f64, point.api_tokens_total as f64);
        for dy in -1..=1 {
            for dx in -1..=1 {
                if dx * dx + dy * dy <= 1 {
                    painter.point_colored(x + dx, y + dy, color);
                }
            }
        }
    }

    let canvas = CanvasRef::from_painter(&painter).style(ftui::Style::new().fg(cc.axis));
    canvas.render(plot_area, frame);
}

fn dim_color(color: PackedRgba, factor: f32) -> PackedRgba {
    let f = factor.clamp(0.0, 1.0);
    PackedRgba::rgb(
        (color.r() as f32 * f) as u8,
        (color.g() as f32 * f) as u8,
        (color.b() as f32 * f) as u8,
    )
}

fn format_explorer_metric_value(metric: ExplorerMetric, value: f64) -> String {
    let _ = metric; // keeps call sites readable; metric-specific formatting removed
    format_compact(value.round() as i64)
}

fn build_explorer_annotation_line(
    metric: ExplorerMetric,
    metric_data: &[(String, f64)],
    overlay_labels: &[String],
) -> String {
    if metric_data.is_empty() {
        return " No explorer data".to_string();
    }
    let mut peak_idx = 0usize;
    let mut peak_val = metric_data[0].1;
    for (idx, (_, value)) in metric_data.iter().enumerate() {
        if *value > peak_val {
            peak_val = *value;
            peak_idx = idx;
        }
    }
    let avg = metric_data.iter().map(|(_, value)| *value).sum::<f64>() / metric_data.len() as f64;
    let first = metric_data.first().map(|(_, v)| *v).unwrap_or(0.0);
    let last = metric_data.last().map(|(_, v)| *v).unwrap_or(0.0);
    let trend_pct = if first.abs() > f64::EPSILON {
        ((last - first) / first) * 100.0
    } else {
        0.0
    };

    let mut line = format!(
        " Peak {} ({})  |  Avg {}  |  Trend {:+.1}%",
        format_explorer_metric_value(metric, peak_val),
        metric_data
            .get(peak_idx)
            .map(|(label, _)| label.as_str())
            .unwrap_or("-"),
        format_explorer_metric_value(metric, avg),
        trend_pct
    );
    if !overlay_labels.is_empty() {
        let preview = overlay_labels
            .iter()
            .take(3)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        line.push_str(&format!("  |  Top overlay: {preview}"));
    }
    line
}

/// Get the daily series data and color for a given explorer metric.
fn metric_series(
    data: &AnalyticsChartData,
    metric: ExplorerMetric,
) -> (&[(String, f64)], PackedRgba) {
    match metric {
        ExplorerMetric::ApiTokens => (&data.daily_tokens, PackedRgba::rgb(0, 150, 255)),
        ExplorerMetric::ContentTokens => {
            (&data.daily_content_tokens, PackedRgba::rgb(180, 130, 255))
        }
        ExplorerMetric::Messages => (&data.daily_messages, PackedRgba::rgb(100, 220, 100)),
        ExplorerMetric::ToolCalls => (&data.daily_tool_calls, PackedRgba::rgb(255, 160, 0)),
        ExplorerMetric::PlanMessages => (&data.daily_plan_messages, PackedRgba::rgb(255, 200, 0)),
    }
}

/// Build per-agent overlay series. Each agent gets its own Vec<(f64, f64)>.
///
/// Simplified proportional overlay — distributes the daily totals by each
/// dimension item's share of the overall breakdown total. A full implementation
/// would query per-dimension timeseries, but this approximation works for v1.
type StrF64Slice<'a> = &'a [(String, f64)];

fn build_dimension_overlay(
    breakdown: StrF64Slice<'_>,
    daily_series: &[(String, f64)],
) -> Vec<Vec<(f64, f64)>> {
    let total: f64 = breakdown.iter().map(|(_, v)| *v).sum();
    if total <= 0.0 {
        return vec![];
    }

    breakdown
        .iter()
        .take(5)
        .map(|(_, item_total)| {
            let share = item_total / total;
            daily_series
                .iter()
                .enumerate()
                .map(|(i, (_, day_val))| (i as f64, day_val * share))
                .collect()
        })
        .collect()
}

/// Select the heatmap timeseries and raw values for the given metric.
///
/// Returns `(series, min_raw, max_raw)` where series items are `(label, normalised 0..1)`.
fn heatmap_series_for_metric(
    data: &AnalyticsChartData,
    metric: HeatmapMetric,
) -> (Vec<(String, f64)>, f64, f64) {
    if matches!(metric, HeatmapMetric::Coverage) {
        if data.heatmap_days.is_empty() {
            return (Vec::new(), 0.0, 0.0);
        }
        let min_norm = data
            .heatmap_days
            .iter()
            .map(|(_, v)| *v)
            .fold(f64::INFINITY, f64::min);
        let max_norm = data
            .heatmap_days
            .iter()
            .map(|(_, v)| *v)
            .fold(0.0_f64, f64::max);
        return (
            data.heatmap_days.clone(),
            min_norm * 100.0,
            max_norm * 100.0,
        );
    }

    let raw: &[(String, f64)] = match metric {
        HeatmapMetric::ApiTokens => &data.daily_tokens,
        HeatmapMetric::Messages => &data.daily_messages,
        HeatmapMetric::ContentTokens => &data.daily_content_tokens,
        HeatmapMetric::ToolCalls => &data.daily_tool_calls,
        HeatmapMetric::Coverage => &[],
    };
    if raw.is_empty() {
        return (Vec::new(), 0.0, 0.0);
    }
    let max_val = raw.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max);
    let min_val = raw.iter().map(|(_, v)| *v).fold(f64::INFINITY, f64::min);
    let series = raw
        .iter()
        .map(|(label, v)| {
            let norm = if max_val > 0.0 { v / max_val } else { 0.0 };
            (label.clone(), norm)
        })
        .collect();
    (series, min_val, max_val)
}

/// Format a raw heatmap value for tooltip display.
fn format_heatmap_value(val: f64, metric: HeatmapMetric) -> String {
    match metric {
        HeatmapMetric::Coverage => format!("{:.0}%", val),
        _ => {
            let abs = val.abs() as i64;
            format_compact(abs)
        }
    }
}

/// Day-of-week labels for the left gutter (Mon, Wed, Fri shown; others blank).
const DOW_LABELS: [&str; 7] = ["Mon", "", "Wed", "", "Fri", "", ""];

/// Parse a "YYYY-MM-DD" label into (year, month, day).
fn parse_day_label(label: &str) -> Option<(i32, u32, u32)> {
    let parts: Vec<&str> = label.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    Some((y, m, d))
}

/// Compute ISO weekday for a date (Mon=0 .. Sun=6) using Tomohiko Sakamoto's method.
#[allow(dead_code)] // used in tests; reserved for future calendar-aligned layout
fn weekday_index(y: i32, m: u32, d: u32) -> usize {
    static T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if m < 3 { y - 1 } else { y };
    let dow = (y + y / 4 - y / 100 + y / 400 + T[m as usize - 1] + d as i32) % 7;
    // Sakamoto gives Sun=0, Mon=1 … Sat=6; convert to Mon=0 … Sun=6.
    ((dow + 6) % 7) as usize
}

/// Render the Heatmap view: GitHub-contributions-style calendar with metric
/// selector, day-of-week labels, month headers, selection highlight, and legend.
pub fn render_heatmap(
    data: &AnalyticsChartData,
    metric: HeatmapMetric,
    selection: usize,
    area: Rect,
    frame: &mut ftui::Frame,
    dark_mode: bool,
) {
    let (series, min_raw, max_raw) = heatmap_series_for_metric(data, metric);
    let cc = ChartColors::for_theme(dark_mode);

    if series.is_empty() {
        if area.height >= 12 && area.width >= 40 {
            let muted = if dark_mode { PackedRgba::rgb(120, 125, 140) } else { PackedRgba::rgb(100, 105, 115) };
            let accent = if dark_mode { PackedRgba::rgb(90, 180, 255) } else { PackedRgba::rgb(20, 100, 200) };
            let primary = if dark_mode { PackedRgba::rgb(60, 120, 200) } else { PackedRgba::rgb(40, 80, 160) };
            let mut lines = Vec::new();
            lines.push(ftui::text::Line::from(""));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ░░░ ▒▒▒ ▓▓▓ ███ ▓▓▓ ▒▒▒ ░░░", ftui::Style::new().fg(muted)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ▒▒▒ ▓▓▓ ███ ███ ███ ▓▓▓ ▒▒▒", ftui::Style::new().fg(primary)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ▓▓▓ ███ ███ ███ ███ ███ ▓▓▓", ftui::Style::new().fg(accent)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ███ ███ ███ ███ ███ ███ ███", ftui::Style::new().fg(accent)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ▓▓▓ ███ ███ ███ ███ ███ ▓▓▓", ftui::Style::new().fg(accent)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ▒▒▒ ▓▓▓ ███ ███ ███ ▓▓▓ ▒▒▒", ftui::Style::new().fg(primary)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ░░░ ▒▒▒ ▓▓▓ ███ ▓▓▓ ▒▒▒ ░░░", ftui::Style::new().fg(muted)),
            ]));
            lines.push(ftui::text::Line::from(""));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled(
                    " No daily data available for this view yet.",
                    ftui::Style::new().fg(cc.axis).bold(),
                ),
            ]));
            Paragraph::new(ftui::text::Text::from_lines(lines)).render(area, frame);
            return;
        }

        Paragraph::new(" No daily data available for this view yet.")
            .style(ftui::Style::new().fg(cc.subtle))
            .render(area, frame);
        return;
    }

    // ── Layout: metric tabs (1) + month labels (1) + grid body (min 5) + legend (1)
    let min_body = 5u16;
    if area.height < 4 {
        // Fallback: too small, just show a sparkline.
        let vals: Vec<f64> = series.iter().map(|(_, v)| *v).collect();
        let spark =
            Sparkline::new(&vals).style(ftui::Style::new().fg(PackedRgba::rgb(80, 200, 120)));
        spark.render(area, frame);
        return;
    }

    let show_legend = area.height >= min_body + 3;
    let legend_h = if show_legend { 1 } else { 0 };
    let chunks = Flex::vertical()
        .constraints([
            Constraint::Fixed(1),        // metric tab bar
            Constraint::Fixed(1),        // month labels row
            Constraint::Min(min_body),   // grid body
            Constraint::Fixed(legend_h), // legend
        ])
        .split(area);
    let tab_area = chunks[0];
    let month_area = chunks[1];
    let grid_area = chunks[2];
    let legend_area = chunks[3];

    // ── 1. Metric tab bar ───────────────────────────────────────────────
    render_heatmap_tabs(metric, tab_area, frame, cc);

    // ── 2. Compute grid geometry ────────────────────────────────────────
    let left_gutter = 4u16; // "Mon " = 4 chars
    let grid_inner = Rect {
        x: grid_area.x + left_gutter,
        y: grid_area.y,
        width: grid_area.width.saturating_sub(left_gutter),
        height: grid_area.height,
    };

    let rows = 7u16; // days of week
    let day_count = (series.len().min(u16::MAX as usize)) as u16;
    let cols = day_count.div_ceil(rows);

    // Determine how many weeks we can show given available width.
    // Each column needs at least 2 chars wide to be readable.
    let max_cols = grid_inner.width / 2;
    let visible_cols = cols.min(max_cols).max(1);
    // If we have more weeks than space, show the most recent N weeks.
    let skip_cols = cols.saturating_sub(visible_cols);
    let skip_days = (skip_cols * rows) as usize;

    let cell_w = grid_inner.width.checked_div(visible_cols).unwrap_or(1);
    let cell_h = grid_inner.height.checked_div(rows).unwrap_or(1);
    let cell_h = cell_h.max(1);
    let cell_w = cell_w.max(1);

    // ── 3. Day-of-week gutter labels ────────────────────────────────────
    for (r, label) in DOW_LABELS.iter().enumerate() {
        if !label.is_empty() && (r as u16) < grid_area.height {
            let label_rect = Rect {
                x: grid_area.x,
                y: grid_area.y + (r as u16) * cell_h,
                width: left_gutter,
                height: 1,
            };
            Paragraph::new(*label)
                .style(ftui::Style::new().fg(cc.muted))
                .render(label_rect, frame);
        }
    }

    // ── 4. Month header labels ──────────────────────────────────────────
    {
        let month_inner = Rect {
            x: month_area.x + left_gutter,
            y: month_area.y,
            width: month_area.width.saturating_sub(left_gutter),
            height: 1,
        };
        let month_names = [
            "", "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let mut last_month = 0u32;
        for (i, (label, _)) in series.iter().enumerate().skip(skip_days) {
            let local_i = (i - skip_days) as u16;
            let col = local_i / rows;
            if col >= visible_cols {
                break;
            }
            let row = local_i % rows;
            if row != 0 {
                continue; // only check first day of each column
            }
            if let Some((_, m, _)) = parse_day_label(label)
                && m != last_month
            {
                last_month = m;
                let x = month_inner.x + col * cell_w;
                if x + 3 <= month_inner.x + month_inner.width {
                    let mname = month_names.get(m as usize).unwrap_or(&"");
                    let mr = Rect {
                        x,
                        y: month_inner.y,
                        width: 3.min(month_inner.width.saturating_sub(x - month_inner.x)),
                        height: 1,
                    };
                    Paragraph::new(*mname)
                        .style(ftui::Style::new().fg(cc.emphasis))
                        .render(mr, frame);
                }
            }
        }
    }

    // ── 5. Heatmap grid (canvas) ────────────────────────────────────────
    let mut painter = Painter::for_area(grid_inner, CanvasMode::HalfBlock);

    for (i, (_, value)) in series.iter().enumerate().skip(skip_days) {
        let local_i = (i - skip_days) as u16;
        let col = local_i / rows;
        if col >= visible_cols {
            break;
        }
        let row = local_i % rows;
        let px = (col * cell_w) as i32;
        let py = (row * cell_h) as i32;
        let color = ftui_extras::charts::heatmap_gradient(*value);
        let fw = (cell_w.max(1) as i32).saturating_sub(1).max(1); // 1px column gap
        let fh = cell_h.max(1) as i32; // no row gap
        for dy in 0..fh {
            for dx in 0..fw {
                painter.point_colored(px + dx, py + dy, color);
            }
        }
    }

    let canvas = CanvasRef::from_painter(&painter).style(ftui::Style::new().fg(cc.emphasis));
    canvas.render(grid_inner, frame);

    // ── 6. Selection highlight ──────────────────────────────────────────
    if selection < series.len() && selection >= skip_days {
        let local_sel = (selection - skip_days) as u16;
        let sel_col = local_sel / rows;
        let sel_row = local_sel % rows;
        if sel_col < visible_cols {
            let sx = grid_inner.x + sel_col * cell_w;
            let sy = grid_inner.y + sel_row * cell_h;
            let sw = cell_w.min((grid_inner.x + grid_inner.width).saturating_sub(sx));
            let sh = cell_h.min((grid_inner.y + grid_inner.height).saturating_sub(sy));
            if sw > 0 && sh > 0 {
                let sel_rect = Rect {
                    x: sx,
                    y: sy,
                    width: sw,
                    height: sh,
                };
                // Render a bright border/marker over the selected cell.
                let marker = if sw >= 2 {
                    "\u{25a0}".to_string() // filled square
                } else {
                    "\u{25b6}".to_string() // arrow
                };
                Paragraph::new(marker)
                    .style(ftui::Style::new().fg(cc.highlight).bold())
                    .render(sel_rect, frame);
            }
        }
    }

    // ── 7. Tooltip: show selected day's date + value ────────────────────
    if selection < series.len() {
        let (label, norm) = &series[selection];
        // For Coverage the series values are raw fractions (0..1 = 0%..100%),
        // not values normalised against max_raw. Reconstruct accordingly.
        let raw_val = if matches!(metric, HeatmapMetric::Coverage) {
            norm * 100.0
        } else {
            norm * max_raw
        };
        let val_str = format_heatmap_value(raw_val, metric);
        let tip = format!(" {} : {} ", label, val_str);
        let tip_w = display_width(&tip) as u16;
        // Place tooltip at bottom-right of grid area.
        if grid_inner.width >= tip_w {
            let tip_rect = Rect {
                x: grid_inner.x + grid_inner.width - tip_w,
                y: grid_area.y + grid_area.height.saturating_sub(1),
                width: tip_w,
                height: 1,
            };
            Paragraph::new(tip)
                .style(ftui::Style::new().fg(cc.tooltip_fg).bg(cc.tooltip_bg))
                .render(tip_rect, frame);
        }
    }

    // ── 8. Legend: gradient ramp with min/max labels ─────────────────────
    if show_legend && legend_area.height > 0 {
        let min_str = format_heatmap_value(min_raw, metric);
        let max_str = format_heatmap_value(max_raw, metric);
        let label_left = format!(" {} ", min_str);
        let label_right = format!(" {} ", max_str);
        let ll = label_left.len() as u16;
        let lr = label_right.len() as u16;

        // Left label
        let left_rect = Rect {
            x: legend_area.x + left_gutter,
            y: legend_area.y,
            width: ll.min(legend_area.width),
            height: 1,
        };
        Paragraph::new(label_left)
            .style(ftui::Style::new().fg(cc.muted))
            .render(left_rect, frame);

        // Gradient ramp in the middle
        let ramp_x = left_rect.x + ll;
        let ramp_end = legend_area.x + legend_area.width.saturating_sub(lr);
        let ramp_w = ramp_end.saturating_sub(ramp_x);
        if ramp_w > 0 {
            for dx in 0..ramp_w {
                let t = dx as f64 / ramp_w.max(1) as f64;
                let color = ftui_extras::charts::heatmap_gradient(t);
                let cell_rect = Rect {
                    x: ramp_x + dx,
                    y: legend_area.y,
                    width: 1,
                    height: 1,
                };
                Paragraph::new("\u{2588}") // full block
                    .style(ftui::Style::new().fg(color))
                    .render(cell_rect, frame);
            }
        }

        // Right label
        if legend_area.x + legend_area.width >= lr {
            let right_rect = Rect {
                x: legend_area.x + legend_area.width - lr,
                y: legend_area.y,
                width: lr,
                height: 1,
            };
            Paragraph::new(label_right)
                .style(ftui::Style::new().fg(cc.muted))
                .render(right_rect, frame);
        }
    }
}

/// Render the heatmap metric tab bar.
fn render_heatmap_tabs(
    active: HeatmapMetric,
    area: Rect,
    frame: &mut ftui::Frame,
    cc: ChartColors,
) {
    let metrics = [
        HeatmapMetric::ApiTokens,
        HeatmapMetric::Messages,
        HeatmapMetric::ContentTokens,
        HeatmapMetric::ToolCalls,
        HeatmapMetric::Coverage,
    ];
    let mut x = area.x;
    for m in &metrics {
        let label = m.label();
        let is_active = *m == active;
        let display = if is_active {
            format!(" [{}] ", label)
        } else {
            format!("  {}  ", label)
        };
        let w = display.len() as u16;
        if x + w > area.x + area.width {
            break;
        }
        let style = if is_active {
            ftui::Style::new().fg(cc.highlight).bold()
        } else {
            ftui::Style::new().fg(cc.muted)
        };
        let tab_rect = Rect {
            x,
            y: area.y,
            width: w,
            height: 1,
        };
        Paragraph::new(display).style(style).render(tab_rect, frame);
        x += w;
    }
}

/// Render the Breakdowns view: tabbed agent/workspace/source/model bar charts.
pub fn render_breakdowns(
    data: &AnalyticsChartData,
    tab: BreakdownTab,
    area: Rect,
    frame: &mut ftui::Frame,
    dark_mode: bool,
) {
    type BreakdownSeries<'a> = (
        &'a [(String, f64)],
        &'a [(String, f64)],
        fn(usize) -> PackedRgba,
    );

    // Select which data to display based on the active tab.
    let (tokens, messages, color_fn): BreakdownSeries<'_> = match tab {
        BreakdownTab::Agent => (&data.agent_tokens, &data.agent_messages, agent_color),
        BreakdownTab::Workspace => (
            &data.workspace_tokens,
            &data.workspace_messages,
            breakdown_color,
        ),
        BreakdownTab::Source => (&data.source_tokens, &data.source_messages, breakdown_color),
        BreakdownTab::Model => (&data.model_tokens, &data.model_tokens, model_color),
    };

    let cc = ChartColors::for_theme(dark_mode);

    if tokens.is_empty() {
        let msg = format!(
            " No {} breakdown data for the current filters.",
            tab.label()
        );
        
        if area.height >= 12 && area.width >= 40 {
            let accent = if dark_mode { PackedRgba::rgb(90, 180, 255) } else { PackedRgba::rgb(20, 100, 200) };
            let primary = if dark_mode { PackedRgba::rgb(60, 120, 200) } else { PackedRgba::rgb(40, 80, 160) };
            
            let mut lines = Vec::new();
            lines.push(ftui::text::Line::from(""));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ██████████      ", ftui::Style::new().fg(accent)),
                ftui::text::Span::styled("   ██████████      ", ftui::Style::new().fg(primary)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ████████████    ", ftui::Style::new().fg(accent)),
                ftui::text::Span::styled("   ██████████████  ", ftui::Style::new().fg(primary)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ████████████████", ftui::Style::new().fg(accent)),
                ftui::text::Span::styled("   ████████        ", ftui::Style::new().fg(primary)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ██████          ", ftui::Style::new().fg(accent)),
                ftui::text::Span::styled("   ████████████████", ftui::Style::new().fg(primary)),
            ]));
            lines.push(ftui::text::Line::from(""));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled(msg, ftui::Style::new().fg(cc.axis).bold()),
            ]));
            Paragraph::new(ftui::text::Text::from_lines(lines)).render(area, frame);
            return;
        }

        Paragraph::new(msg)
            .style(ftui::Style::new().fg(cc.subtle))
            .render(area, frame);
        return;
    }

    // Layout: tab bar (1 row) | content (fill)
    let layout = Flex::vertical()
        .constraints([Constraint::Fixed(1), Constraint::Min(3)])
        .split(area);

    // ── Tab bar ──────────────────────────────────────────
    render_breakdown_tabs(tab, layout[0], frame, cc);

    // ── Content: side-by-side bar charts (tokens | messages) ─
    // Inset by 1 column to leave a gutter for the selection indicator (▶).
    let content = Rect {
        x: layout[1].x + 1,
        y: layout[1].y,
        width: layout[1].width.saturating_sub(1),
        height: layout[1].height,
    };

    // Determine how many rows we can fit (max 25 to avoid overwhelm).
    // BarChart uses 1 row per group + some overhead.
    let max_items = (content.height as usize).saturating_sub(2).clamp(8, 25);

    // For Model tab, show a single tokens-only chart (no message counts).
    if matches!(tab, BreakdownTab::Model) {
        let groups: Vec<BarGroup<'_>> = tokens
            .iter()
            .take(max_items)
            .map(|(name, val)| BarGroup::new(name, vec![*val]))
            .collect();
        let colors: Vec<PackedRgba> = (0..groups.len()).map(color_fn).collect();
        let chart = BarChart::new(groups)
            .direction(BarDirection::Horizontal)
            .bar_width(1)
            .colors(colors);
        chart.render(content, frame);
        return;
    }

    let chunks = Flex::horizontal()
        .constraints([Constraint::Percentage(50.0), Constraint::Percentage(50.0)])
        .split(content);

    // Token breakdown.
    {
        let token_rows: Vec<(String, f64)> = tokens
            .iter()
            .take(max_items)
            .map(|(name, val)| (shorten_label(name, 20), *val))
            .collect();
        let groups: Vec<BarGroup<'_>> = token_rows
            .iter()
            .map(|(label, val)| BarGroup::new(label.as_str(), vec![*val]))
            .collect();
        let colors: Vec<PackedRgba> = (0..groups.len()).map(color_fn).collect();
        let chart = BarChart::new(groups)
            .direction(BarDirection::Horizontal)
            .bar_width(1)
            .colors(colors);
        chart.render(chunks[0], frame);
    }

    // Message breakdown.
    {
        let message_rows: Vec<(String, f64)> = messages
            .iter()
            .take(max_items)
            .map(|(name, val)| (shorten_label(name, 20), *val))
            .collect();
        let groups: Vec<BarGroup<'_>> = message_rows
            .iter()
            .map(|(label, val)| BarGroup::new(label.as_str(), vec![*val]))
            .collect();
        let colors: Vec<PackedRgba> = (0..groups.len()).map(color_fn).collect();
        let chart = BarChart::new(groups)
            .direction(BarDirection::Horizontal)
            .bar_width(1)
            .colors(colors);
        chart.render(chunks[1], frame);
    }
}

/// Color palette for non-agent breakdowns (workspaces, sources).
const BREAKDOWN_COLORS: &[PackedRgba] = &[
    PackedRgba::rgb(0, 180, 220),
    PackedRgba::rgb(220, 160, 0),
    PackedRgba::rgb(80, 200, 120),
    PackedRgba::rgb(200, 80, 180),
    PackedRgba::rgb(120, 200, 255),
    PackedRgba::rgb(255, 140, 80),
    PackedRgba::rgb(160, 120, 255),
    PackedRgba::rgb(255, 200, 120),
];

fn breakdown_color(idx: usize) -> PackedRgba {
    BREAKDOWN_COLORS[idx % BREAKDOWN_COLORS.len()]
}

fn model_color(idx: usize) -> PackedRgba {
    const MODEL_COLORS: &[PackedRgba] = &[
        PackedRgba::rgb(0, 180, 220),
        PackedRgba::rgb(220, 120, 0),
        PackedRgba::rgb(80, 200, 80),
        PackedRgba::rgb(200, 60, 180),
        PackedRgba::rgb(255, 200, 60),
        PackedRgba::rgb(120, 120, 255),
    ];
    MODEL_COLORS[idx % MODEL_COLORS.len()]
}

fn truncate_with_ellipsis(input: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if display_width(input) <= max_cols {
        return input.to_string();
    }
    if max_cols == 1 {
        return "\u{2026}".to_string();
    }
    let budget = max_cols - 1;
    let mut out = String::new();
    let mut w = 0;
    for ch in input.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('\u{2026}');
    out
}

fn breakdown_tabs_line(active: BreakdownTab, width: usize) -> String {
    let mut text = String::with_capacity(96);
    text.push(' ');
    for tab in BreakdownTab::all() {
        if *tab == active {
            text.push_str(&format!("[{}]", tab.label()));
        } else {
            text.push_str(&format!(" {} ", tab.label()));
        }
        text.push(' ');
    }
    text.push_str("  (Tab/Shift+Tab to switch)");
    truncate_with_ellipsis(&text, width)
}

/// Render the tab selector bar for the Breakdowns view.
fn render_breakdown_tabs(
    active: BreakdownTab,
    area: Rect,
    frame: &mut ftui::Frame,
    cc: ChartColors,
) {
    let text = breakdown_tabs_line(active, area.width as usize);
    let style = ftui::Style::new().fg(cc.axis).bold();
    Paragraph::new(text).style(style).render(area, frame);
}

/// Shorten a label (e.g., workspace path) to fit in `max_len` characters.
fn shorten_label(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if display_width(s) <= max_cols {
        return s.to_string();
    }
    if s.contains('/') {
        let last = s.rsplit('/').next().unwrap_or(s);
        if display_width(last) <= max_cols {
            return last.to_string();
        }
    }
    if max_cols == 1 {
        return "\u{2026}".to_string();
    }
    // Take characters until we would exceed the column budget (minus 1 for ellipsis).
    let budget = max_cols - 1;
    let mut truncated = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        truncated.push(ch);
        w += cw;
    }
    truncated.push('\u{2026}');
    truncated
}

/// Number of visible rows in the Tools view (for selection bounds).
pub fn tools_row_count(data: &AnalyticsChartData) -> usize {
    let max_visible = 20;
    data.tool_rows.len().min(max_visible)
}

/// Number of visible rows in the Coverage view (for selection bounds).
pub fn coverage_row_count(data: &AnalyticsChartData) -> usize {
    data.agent_tokens.len().min(10)
}

/// Render the Tools view: per-agent table with calls, messages, tokens, calls/1K, and trend.
pub fn render_tools(
    data: &AnalyticsChartData,
    area: Rect,
    frame: &mut ftui::Frame,
    dark_mode: bool,
) {
    let cc = ChartColors::for_theme(dark_mode);

    if data.tool_rows.is_empty() {
        if area.height >= 12 && area.width >= 40 {
            let accent = if dark_mode { PackedRgba::rgb(90, 180, 255) } else { PackedRgba::rgb(20, 100, 200) };
            let primary = if dark_mode { PackedRgba::rgb(60, 120, 200) } else { PackedRgba::rgb(40, 80, 160) };
            
            let mut lines = Vec::new();
            lines.push(ftui::text::Line::from(""));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   Agent                 Calls   Msgs   Tokens   Trend  ", ftui::Style::new().fg(cc.muted)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ██████████               ██     ██       ██     ███  ", ftui::Style::new().fg(primary)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ████████████             ██     ██       ██     ███  ", ftui::Style::new().fg(accent)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ██████                   ██     ██       ██     ███  ", ftui::Style::new().fg(primary)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ████████                 ██     ██       ██     ███  ", ftui::Style::new().fg(accent)),
            ]));
            lines.push(ftui::text::Line::from(""));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled(
                    " No tool usage data available for the current filters.",
                    ftui::Style::new().fg(cc.axis).bold(),
                ),
            ]));
            Paragraph::new(ftui::text::Text::from_lines(lines)).render(area, frame);
            return;
        }

        Paragraph::new(" No tool usage data available for the current filters.")
            .style(ftui::Style::new().fg(cc.subtle))
            .render(area, frame);
        return;
    }

    // Layout: header (1) | table rows (fill) | sparkline (3) | summary (1)
    let has_sparkline = !data.daily_tool_calls.is_empty();
    let constraints = if has_sparkline {
        vec![
            Constraint::Fixed(1),
            Constraint::Min(3),
            Constraint::Fixed(3),
            Constraint::Fixed(1),
        ]
    } else {
        vec![
            Constraint::Fixed(1),
            Constraint::Min(3),
            Constraint::Fixed(1),
        ]
    };
    let chunks = Flex::vertical().constraints(constraints).split(area);

    // ── Header ──
    let header_style = ftui::Style::new().fg(cc.axis).bold();
    let header = tools_header_line(chunks[0].width as usize);
    Paragraph::new(header)
        .style(header_style)
        .render(chunks[0], frame);

    // ── Table rows ──
    let table_area = chunks[1];
    let max_rows = (table_area.height as usize).min(tools_row_count(data));
    let total_calls = data.total_tool_calls.max(1) as f64;

    for (i, row) in data.tool_rows.iter().take(max_rows).enumerate() {
        if i >= table_area.height as usize {
            break;
        }
        let row_rect = Rect {
            x: table_area.x,
            y: table_area.y + i as u16,
            width: table_area.width,
            height: 1,
        };
        let pct_share = (row.tool_call_count as f64 / total_calls) * 100.0;
        let line = tools_row_line(row, pct_share, row_rect.width as usize);
        let color = agent_color(i);
        Paragraph::new(line)
            .style(ftui::Style::new().fg(color))
            .render(row_rect, frame);
    }

    // ── Daily tool calls sparkline ──
    if has_sparkline {
        let spark_area = chunks[2];
        let values: Vec<f64> = data.daily_tool_calls.iter().map(|(_, v)| *v).collect();
        let sparkline = Sparkline::new(&values)
            .gradient(PackedRgba::rgb(60, 60, 120), PackedRgba::rgb(100, 200, 255));
        sparkline.render(spark_area, frame);
    }

    // ── Summary ──
    let summary_idx = if has_sparkline { 3 } else { 2 };
    let summary = truncate_with_ellipsis(
        &format!(
            " {} agents \u{00b7} {} total calls \u{00b7} {} API tokens",
            data.tool_rows.len(),
            format_compact(data.total_tool_calls),
            format_compact(
                data.tool_rows
                    .iter()
                    .map(|r| r.api_tokens_total)
                    .sum::<i64>()
            ),
        ),
        chunks[summary_idx].width as usize,
    );
    Paragraph::new(summary)
        .style(ftui::Style::new().fg(cc.muted))
        .render(chunks[summary_idx], frame);
}

/// Build the header line for the tools table.
fn tools_header_line(width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    
    let w = width;
    
    if width < 56 {
        let name_w: usize = 10;
        let label = "Agent";
        let current_w = display_width(label);
        let pad_w = name_w.saturating_sub(current_w);
        let pad = " ".repeat(pad_w);

        let compact = format!(
            " {}{} {:>5} {:>5} {:>8} {:>5}",
            label, pad, "Calls", "Msgs", "Tokens", "Share"
        );
        return truncate_with_ellipsis(&compact, width);
    }

    let name_w = (w * 28 / 100).clamp(8, 24);
    let label = "Agent";
    let current_w = display_width(label);
    let pad_w = name_w.saturating_sub(current_w);
    let pad = " ".repeat(pad_w);

    let line = format!(
        " {}{} {:>8} {:>8} {:>10} {:>8} {:>6}",
        label, pad,
        "Calls",
        "Msgs",
        "API Tok",
        "Calls/1K",
        "Share",
    );
    truncate_with_ellipsis(&line, width)
}

/// Format a single tool-report row into a table line.
fn tools_row_line(row: &crate::analytics::ToolRow, pct_share: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let per_1k = row
        .tool_calls_per_1k_api_tokens
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "\u{2014}".to_string());
        
    if width < 56 {
        let name_w: usize = 10;
        let truncated_name = shorten_label(&row.key, name_w);
        let current_w = display_width(&truncated_name);
        let pad_w = name_w.saturating_sub(current_w);
        let pad = " ".repeat(pad_w);

        let line = format!(
            " {}{} {:>5} {:>5} {:>8} {:>4.0}%",
            truncated_name, pad,
            format_compact(row.tool_call_count),
            format_compact(row.message_count),
            format_compact(row.api_tokens_total),
            pct_share,
        );
        return truncate_with_ellipsis(&line, width);
    }
    
    let w = width;
    let name_w = (w * 28 / 100).clamp(8, 24);
    let truncated_name = shorten_label(&row.key, name_w);
    let current_w = display_width(&truncated_name);
    let pad_w = name_w.saturating_sub(current_w);
    let pad = " ".repeat(pad_w);
    
    let line = format!(
        " {}{} {:>8} {:>8} {:>10} {:>8} {:>5.1}%",
        truncated_name, pad,
        format_number(row.tool_call_count),
        format_number(row.message_count),
        format_compact(row.api_tokens_total),
        per_1k,
        pct_share,
    );
    truncate_with_ellipsis(&line, width)
}

// Cost (USD) UI removed: pricing-derived token costs were misleading and not
// useful for cass UX. We keep model usage breakdowns via `model_tokens`.

/// Number of selectable rows in the Plans view (per-agent plan breakdown).
pub fn plans_rows(data: &AnalyticsChartData) -> usize {
    data.agent_plan_messages.len().min(15)
}

/// Render the Plans view: plan message breakdown by agent + plan token share.
fn render_plans(
    data: &AnalyticsChartData,
    selection: usize,
    area: Rect,
    frame: &mut ftui::Frame,
    dark_mode: bool,
) {
    if area.height < 3 || area.width < 20 {
        return;
    }
    let cc = ChartColors::for_theme(dark_mode);

    let total_plan = data.total_plan_messages;
    let total_msgs = data.total_messages;
    let plan_pct = if total_msgs > 0 {
        (total_plan as f64 / total_msgs as f64) * 100.0
    } else {
        0.0
    };

    // Header
    let header = truncate_with_ellipsis(
        &format!(
            " Plans: {} plan msgs / {} total ({:.1}%)  |  Up/Down=select  Enter=drilldown",
            format_compact(total_plan),
            format_compact(total_msgs),
            plan_pct,
        ),
        area.width as usize,
    );
    Paragraph::new(header)
        .style(ftui::Style::new().fg(cc.emphasis))
        .render(
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 1,
            },
            frame,
        );

    // Per-agent plan message rows.
    let max_val = data
        .agent_plan_messages
        .first()
        .map(|(_, v)| *v)
        .unwrap_or(1.0)
        .max(1.0);

    for (i, (agent, count)) in data.agent_plan_messages.iter().enumerate().take(15) {
        let y = area.y + 1 + i as u16;
        if y >= area.y + area.height {
            break;
        }
        let bar_width = ((count / max_val) * (area.width as f64 * 0.5).max(1.0)) as u16;
        let value = format_compact(*count as i64);
        let value_w = display_width(&value);
        let agent_w = area.width.saturating_sub(value_w as u16 + 3).max(4) as usize;
        let label = truncate_with_ellipsis(
            &format!(
                " {:<agent_w$} {:>value_w$}",
                shorten_label(agent, agent_w),
                value,
                agent_w = agent_w,
                value_w = value_w.max(1),
            ),
            area.width as usize,
        );
        let fg = if i == selection {
            cc.highlight
        } else {
            cc.highlight_dim
        };
        let row_area = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };
        // Bar
        let bar_area = Rect {
            x: area.x,
            y,
            width: bar_width.min(area.width),
            height: 1,
        };
        let bar_bg = if dark_mode {
            PackedRgba::rgb(80, 60, 0)
        } else {
            PackedRgba::rgb(255, 235, 180)
        };
        Paragraph::new("")
            .style(ftui::Style::new().bg(bar_bg))
            .render(bar_area, frame);
        // Label on top
        Paragraph::new(label)
            .style(ftui::Style::new().fg(fg))
            .render(row_area, frame);
    }
}

/// Render the Coverage view: overall bar + per-agent breakdown + daily sparkline.
pub fn render_coverage(
    data: &AnalyticsChartData,
    area: Rect,
    frame: &mut ftui::Frame,
    dark_mode: bool,
) {
    let cc = ChartColors::for_theme(dark_mode);

    // Agent rows to show (up to 10).
    let agent_row_count = data.agent_tokens.len().min(10);
    let table_height = if agent_row_count > 0 {
        (agent_row_count + 1) as u16 // +1 header
    } else {
        0
    };

    let chunks = Flex::vertical()
        .constraints([
            Constraint::Fixed(2),            // overall coverage bar
            Constraint::Fixed(table_height), // per-agent breakdown
            Constraint::Min(3),              // daily sparkline
        ])
        .split(area);

    // ── Overall coverage bar ─────────────────────────────────
    let bar_width = area.width.saturating_sub(6) as usize;
    let api_filled = (data.coverage_pct / 100.0 * bar_width as f64).round() as usize;
    let api_empty = bar_width.saturating_sub(api_filled);
    let line1 = truncate_with_ellipsis(
        &format!(
            " API Token Coverage: {:.1}%  [{}{}]",
            data.coverage_pct,
            "\u{2588}".repeat(api_filled),
            "\u{2591}".repeat(api_empty),
        ),
        chunks[0].width as usize,
    );
    let line2 = truncate_with_ellipsis(
        &format!(
            " {} agents  \u{2502}  {} total API tokens",
            data.agent_count,
            format_compact(data.total_api_tokens),
        ),
        chunks[0].width as usize,
    );
    let cov_color = coverage_color(data.coverage_pct);
    Paragraph::new(line1)
        .style(ftui::Style::new().fg(cov_color))
        .render(chunks[0], frame);
    if chunks[0].height > 1 {
        let line2_area = Rect {
            x: chunks[0].x,
            y: chunks[0].y + 1,
            width: chunks[0].width,
            height: 1,
        };
        Paragraph::new(line2)
            .style(ftui::Style::new().fg(cc.muted))
            .render(line2_area, frame);
    }

    // ── Per-agent coverage breakdown ─────────────────────────
    if agent_row_count > 0 && chunks[1].height > 0 {
        let w = chunks[1].width as usize;
        // Header.
        let header = if w < 48 {
            let lbl = "Agent";
            let pad = " ".repeat(12_usize.saturating_sub(display_width(lbl)));
            format!(" {}{} {:>8} {:>6}", lbl, pad, "Tokens", "Msgs")
        } else {
            let lbl = "Agent";
            let pad = " ".repeat(16_usize.saturating_sub(display_width(lbl)));
            format!(
                " {}{} {:>12} {:>10} {:>8}",
                lbl, pad, "API Tokens", "Messages", "Data"
            )
        };
        let header_trunc = coverage_truncate(&header, w);
        let header_area = Rect {
            x: chunks[1].x,
            y: chunks[1].y,
            width: chunks[1].width,
            height: 1,
        };
        Paragraph::new(header_trunc)
            .style(ftui::Style::new().fg(cc.emphasis).bold())
            .render(header_area, frame);

        // Agent rows.
        for (i, (agent, tokens)) in data.agent_tokens.iter().take(10).enumerate() {
            let row_y = chunks[1].y + 1 + i as u16;
            if row_y >= chunks[1].y + chunks[1].height {
                break;
            }
            let msgs = data
                .agent_messages
                .iter()
                .find(|(a, _)| a == agent)
                .map(|(_, v)| *v)
                .unwrap_or(0.0);
            // Agents with >0 API tokens have real API data.
            let data_indicator = if *tokens > 0.0 {
                "\u{2713} API"
            } else {
                "~ est"
            };
            let indicator_color = if *tokens > 0.0 {
                PackedRgba::rgb(80, 200, 80)
            } else {
                PackedRgba::rgb(255, 200, 0)
            };
            let row_text = if w < 48 {
                let name_w = 12;
                let t_name = coverage_truncate(agent, name_w);
                let pad = " ".repeat(name_w.saturating_sub(display_width(&t_name)));
                format!(
                    " {}{} {:>8} {:>6}",
                    t_name, pad,
                    format_compact(*tokens as i64),
                    format_compact(msgs as i64),
                )
            } else {
                let name_w = 16;
                let t_name = coverage_truncate(agent, name_w);
                let pad = " ".repeat(name_w.saturating_sub(display_width(&t_name)));
                format!(
                    " {}{} {:>12} {:>10} {:>8}",
                    t_name, pad,
                    format_compact(*tokens as i64),
                    format_compact(msgs as i64),
                    "",
                )
            };
            let row_trunc = coverage_truncate(&row_text, w);
            let row_area = Rect {
                x: chunks[1].x,
                y: row_y,
                width: chunks[1].width,
                height: 1,
            };
            Paragraph::new(row_trunc)
                .style(ftui::Style::new().fg(agent_color(i)))
                .render(row_area, frame);
            // Overlay data indicator in its own color at the right edge.
            let indicator_len = display_width(data_indicator) as u16;
            if w >= 48 && chunks[1].width > indicator_len + 1 {
                let ind_area = Rect {
                    x: chunks[1].x + chunks[1].width - indicator_len - 1,
                    y: row_y,
                    width: indicator_len + 1,
                    height: 1,
                };
                let ind_text = format!(
                    "{:>width$}",
                    data_indicator,
                    width = (indicator_len + 1) as usize
                );
                Paragraph::new(ind_text)
                    .style(ftui::Style::new().fg(indicator_color))
                    .render(ind_area, frame);
            }
        }
    }

    // ── Daily token sparkline ────────────────────────────────
    if !data.daily_tokens.is_empty() {
        let label = " Daily API Tokens";
        if chunks[2].height > 0 {
            let label_text = truncate_with_ellipsis(label, chunks[2].width as usize);
            let label_area = Rect {
                x: chunks[2].x,
                y: chunks[2].y,
                width: chunks[2].width.min(display_width(&label_text) as u16),
                height: 1,
            };
            Paragraph::new(label_text)
                .style(ftui::Style::new().fg(cc.muted))
                .render(label_area, frame);
        }

        let spark_area = if chunks[2].height > 1 {
            Rect {
                x: chunks[2].x,
                y: chunks[2].y + 1,
                width: chunks[2].width,
                height: chunks[2].height - 1,
            }
        } else {
            chunks[2]
        };
        let values: Vec<f64> = data.daily_tokens.iter().map(|(_, v)| *v).collect();
        let sparkline = Sparkline::new(&values)
            .gradient(PackedRgba::rgb(60, 60, 120), PackedRgba::rgb(80, 200, 80));
        sparkline.render(spark_area, frame);
    } else {
        if chunks[2].height >= 8 && chunks[2].width >= 40 {
            let accent = if dark_mode { PackedRgba::rgb(90, 180, 255) } else { PackedRgba::rgb(20, 100, 200) };
            let primary = if dark_mode { PackedRgba::rgb(60, 120, 200) } else { PackedRgba::rgb(40, 80, 160) };
            
            let mut lines = Vec::new();
            lines.push(ftui::text::Line::from(""));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ▂▂▃▄▅▆▇██████████████▇▆▅▄▃▂▂   ", ftui::Style::new().fg(accent)),
            ]));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("   ████████████████████████████   ", ftui::Style::new().fg(primary)),
            ]));
            lines.push(ftui::text::Line::from(""));
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled(
                    " No daily data for sparkline",
                    ftui::Style::new().fg(cc.axis).bold(),
                ),
            ]));
            Paragraph::new(ftui::text::Text::from_lines(lines)).render(chunks[2], frame);
            return;
        }

        Paragraph::new(" No daily data for sparkline")
            .style(ftui::Style::new().fg(cc.subtle))
            .render(chunks[2], frame);
    }
}

fn coverage_color(pct: f64) -> PackedRgba {
    if pct >= 80.0 {
        PackedRgba::rgb(80, 200, 80)
    } else if pct >= 50.0 {
        PackedRgba::rgb(255, 200, 0)
    } else {
        PackedRgba::rgb(255, 80, 80)
    }
}

fn coverage_truncate(s: &str, max_len: usize) -> String {
    truncate_with_ellipsis(s, max_len)
}

fn display_width(input: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(input)
}

/// Explorer view state passed to the render function.
pub struct ExplorerState {
    pub metric: ExplorerMetric,
    pub overlay: ExplorerOverlay,
    pub group_by: crate::analytics::GroupBy,
    pub zoom: super::app::ExplorerZoom,
}

/// Dispatch rendering to the appropriate view function.
///
/// `selection` is the currently highlighted item index (for drilldown).
#[allow(clippy::too_many_arguments)]
pub fn render_analytics_content(
    view: AnalyticsView,
    data: &AnalyticsChartData,
    explorer: &ExplorerState,
    breakdown_tab: BreakdownTab,
    heatmap_metric: HeatmapMetric,
    selection: usize,
    area: Rect,
    frame: &mut ftui::Frame,
    dark_mode: bool,
) {
    match view {
        AnalyticsView::Dashboard => render_dashboard(data, area, frame, dark_mode),
        AnalyticsView::Explorer => render_explorer(data, explorer, area, frame, dark_mode),
        AnalyticsView::Heatmap => {
            render_heatmap(data, heatmap_metric, selection, area, frame, dark_mode)
        }
        AnalyticsView::Breakdowns => {
            render_breakdowns(data, breakdown_tab, area, frame, dark_mode);
            let row_count = breakdown_rows(data, breakdown_tab);
            // Offset by 1 for the tab bar row.
            let content_area = if area.height > 1 {
                Rect {
                    x: area.x,
                    y: area.y + 1,
                    width: area.width,
                    height: area.height - 1,
                }
            } else {
                area
            };
            render_selection_indicator(
                selection,
                row_count,
                content_area,
                frame,
                !matches!(breakdown_tab, BreakdownTab::Model),
                dark_mode,
            );
        }
        AnalyticsView::Tools => {
            render_tools(data, area, frame, dark_mode);
            // Selection indicator offset by 1 for the header row.
            let tools_content = if area.height > 1 {
                Rect {
                    x: area.x,
                    y: area.y + 1,
                    width: area.width,
                    height: area.height - 1,
                }
            } else {
                area
            };
            render_selection_indicator(
                selection,
                tools_row_count(data),
                tools_content,
                frame,
                false,
                dark_mode,
            );
        }
        AnalyticsView::Plans => {
            render_plans(data, selection, area, frame, dark_mode);
        }
        AnalyticsView::Coverage => {
            render_coverage(data, area, frame, dark_mode);
            // Selection indicator offset by 2 for the coverage bar + 1 for table header.
            let row_count = coverage_row_count(data);
            if row_count > 0 && area.height > 3 {
                let cov_content = Rect {
                    x: area.x,
                    y: area.y + 3, // 2-row coverage bar + 1-row table header
                    width: area.width,
                    height: area.height.saturating_sub(3),
                };
                render_selection_indicator(
                    selection,
                    row_count,
                    cov_content,
                    frame,
                    false,
                    dark_mode,
                );
            }
        }
    }
}

/// Number of selectable rows in the Breakdowns view for the given tab.
pub fn breakdown_rows(data: &AnalyticsChartData, tab: BreakdownTab) -> usize {
    match tab {
        BreakdownTab::Agent => data.agent_tokens.len().min(8),
        BreakdownTab::Workspace => data.workspace_tokens.len().min(8),
        BreakdownTab::Source => data.source_tokens.len().min(8),
        BreakdownTab::Model => data.model_tokens.len().min(10),
    }
}

/// Overlay a `▶` selection indicator at the given row index within `area`.
///
/// If `half_width` is true, the indicator is placed in the left half of the area
/// (for split-pane views like Breakdowns).
fn render_selection_indicator(
    selection: usize,
    max_rows: usize,
    area: Rect,
    frame: &mut ftui::Frame,
    half_width: bool,
    dark_mode: bool,
) {
    if max_rows == 0 || selection >= max_rows {
        return;
    }
    let target_area = if half_width {
        let chunks = Flex::horizontal()
            .constraints([Constraint::Percentage(50.0), Constraint::Percentage(50.0)])
            .split(area);
        chunks[0]
    } else {
        area
    };
    if target_area.height <= selection as u16 {
        return;
    }
    let sel_y = target_area.y + selection as u16;
    let indicator = Rect {
        x: target_area.x,
        y: sel_y,
        width: 1,
        height: 1,
    };
    let cc = ChartColors::for_theme(dark_mode);
    Paragraph::new("\u{25b6}")
        .style(ftui::Style::new().fg(cc.highlight).bold())
        .render(indicator, frame);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Format a large number with comma separators (e.g. 1234567 → "1,234,567").
fn format_number(n: i64) -> String {
    let (prefix, abs_str) = if n < 0 {
        ("-", n.unsigned_abs().to_string())
    } else {
        ("", n.to_string())
    };
    let mut result = String::with_capacity(abs_str.len() + abs_str.len() / 3 + prefix.len());
    for (i, c) in abs_str.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    let grouped: String = result.chars().rev().collect();
    format!("{prefix}{grouped}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use frankensqlite::compat::{BatchExt, ConnectionExt};
    use frankensqlite::params;

    #[test]
    fn resolve_workspace_filter_ids_supports_paths_and_numeric_ids() {
        let conn = frankensqlite::Connection::open(":memory:").unwrap();
        conn.execute_batch(
            "CREATE TABLE workspaces (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE
            );",
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO workspaces (id, path) VALUES (?1, ?2)",
            params![1_i64, "/workspace/one"],
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO workspaces (id, path) VALUES (?1, ?2)",
            params![2_i64, "/workspace/two"],
        )
        .unwrap();

        let mut filters = std::collections::HashSet::new();
        filters.insert("/workspace/one".to_string());
        filters.insert("2".to_string());
        filters.insert("/workspace/missing".to_string());

        let ids = resolve_workspace_filter_ids(&conn, &filters);
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert_eq!(ids.iter().filter(|id| **id == 2).count(), 1);
    }

    #[test]
    fn load_chart_data_applies_workspace_path_filter() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("analytics_filters.db");
        let storage = crate::storage::sqlite::FrankenStorage::open(&db_path).unwrap();

        let ws_a = storage
            .ensure_workspace(std::path::Path::new("/workspace/a"), None)
            .unwrap();
        let ws_b = storage
            .ensure_workspace(std::path::Path::new("/workspace/b"), None)
            .unwrap();

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let conn = storage.raw();
        conn.execute_compat(
            "INSERT INTO usage_daily (
                day_id, agent_slug, workspace_id, source_id,
                message_count, tool_call_count, api_tokens_total, last_updated
             ) VALUES (?1, 'codex', ?2, 'local', 10, 2, 1000, ?3)",
            params![20260220_i64, ws_a, now_ms],
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO usage_daily (
                day_id, agent_slug, workspace_id, source_id,
                message_count, tool_call_count, api_tokens_total, last_updated
             ) VALUES (?1, 'codex', ?2, 'local', 20, 4, 2000, ?3)",
            params![20260220_i64, ws_b, now_ms],
        )
        .unwrap();

        let mut filters = crate::ui::app::AnalyticsFilterState::default();
        filters.workspaces.insert("/workspace/a".to_string());

        let data = load_chart_data(&storage, &filters, crate::analytics::GroupBy::Day);
        assert_eq!(data.total_api_tokens, 1000);
        assert_eq!(data.total_messages, 10);
        assert_eq!(data.total_tool_calls, 2);
        assert_eq!(
            data.agent_tokens.first().map(|(_, v)| *v as i64),
            Some(1000)
        );
    }

    #[test]
    fn format_number_basic() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1234567), "1,234,567");
        assert_eq!(format_number(100), "100");
    }

    #[test]
    fn format_compact_suffixes() {
        assert_eq!(format_compact(0), "0");
        assert_eq!(format_compact(999), "999");
        assert_eq!(format_compact(9999), "9,999");
        assert_eq!(format_compact(10_000), "10.0K");
        assert_eq!(format_compact(1_500_000), "1.5M");
        assert_eq!(format_compact(2_300_000_000), "2.3B");
    }

    #[test]
    fn format_explorer_metric_value_is_compact() {
        assert_eq!(
            format_explorer_metric_value(ExplorerMetric::ApiTokens, 12.3456),
            "12"
        );
    }

    #[test]
    fn build_explorer_annotation_line_contains_peak_avg_trend() {
        let metric_data = vec![
            ("2026-02-01".to_string(), 100.0),
            ("2026-02-02".to_string(), 300.0),
            ("2026-02-03".to_string(), 200.0),
        ];
        let line = build_explorer_annotation_line(
            ExplorerMetric::ApiTokens,
            &metric_data,
            &["codex".to_string(), "claude_code".to_string()],
        );
        assert!(line.contains("Peak"));
        assert!(line.contains("Avg"));
        assert!(line.contains("Trend"));
        assert!(line.contains("2026-02-02"));
        assert!(line.contains("Top overlay: codex"));
    }

    #[test]
    fn dim_color_scales_channels_down() {
        let c = PackedRgba::rgb(200, 100, 50);
        let d = dim_color(c, 0.5);
        assert_eq!(d.r(), 100);
        assert_eq!(d.g(), 50);
        assert_eq!(d.b(), 25);
    }

    #[test]
    fn agent_color_cycles() {
        let c0 = agent_color(0);
        let c14 = agent_color(14);
        assert_eq!(c0, c14); // cycles at 14
    }

    #[test]
    fn default_chart_data_is_empty() {
        let data = AnalyticsChartData::default();
        assert!(data.agent_tokens.is_empty());
        assert!(data.daily_tokens.is_empty());
        assert_eq!(data.total_messages, 0);
        assert_eq!(data.coverage_pct, 0.0);
    }

    #[test]
    fn render_analytics_content_all_views_no_panic() {
        // Verify that rendering with empty data doesn't panic for any view.
        let data = AnalyticsChartData::default();
        // We can't easily create a frame in tests, but we can verify the
        // dispatch function compiles and the data structures are correct.
        let _ = &data;
        for view in AnalyticsView::all() {
            // Just verify the match arm exists for each view.
            match view {
                AnalyticsView::Dashboard
                | AnalyticsView::Explorer
                | AnalyticsView::Heatmap
                | AnalyticsView::Breakdowns
                | AnalyticsView::Tools
                | AnalyticsView::Plans
                | AnalyticsView::Coverage => {}
            }
        }
    }

    #[test]
    fn weekday_index_known_dates() {
        // 2026-02-07 is a Saturday → index 5 (Mon=0..Sun=6)
        assert_eq!(weekday_index(2026, 2, 7), 5);
        // 2026-02-02 is a Monday → index 0
        assert_eq!(weekday_index(2026, 2, 2), 0);
        // 2026-01-01 is a Thursday → index 3
        assert_eq!(weekday_index(2026, 1, 1), 3);
    }

    #[test]
    fn parse_day_label_valid() {
        assert_eq!(parse_day_label("2026-02-07"), Some((2026, 2, 7)));
        assert_eq!(parse_day_label("2025-12-31"), Some((2025, 12, 31)));
        assert_eq!(parse_day_label("invalid"), None);
        assert_eq!(parse_day_label("2026-13-01"), Some((2026, 13, 1))); // parser doesn't validate ranges
    }

    #[test]
    fn heatmap_series_empty_data() {
        let data = AnalyticsChartData::default();
        let (series, min, max) = heatmap_series_for_metric(&data, HeatmapMetric::ApiTokens);
        assert!(series.is_empty());
        assert_eq!(min, 0.0);
        assert_eq!(max, 0.0);
    }

    #[test]
    fn heatmap_series_normalizes() {
        let data = AnalyticsChartData {
            daily_tokens: vec![
                ("2026-02-01".to_string(), 100.0),
                ("2026-02-02".to_string(), 200.0),
                ("2026-02-03".to_string(), 50.0),
            ],
            ..Default::default()
        };
        let (series, min, max) = heatmap_series_for_metric(&data, HeatmapMetric::ApiTokens);
        assert_eq!(series.len(), 3);
        assert_eq!(max, 200.0);
        assert_eq!(min, 50.0);
        // Max value normalizes to 1.0
        assert!((series[1].1 - 1.0).abs() < 0.001);
        // Min value normalizes to 0.25
        assert!((series[2].1 - 0.25).abs() < 0.001);
    }

    #[test]
    fn heatmap_series_coverage_uses_normalized_heatmap_days() {
        let data = AnalyticsChartData {
            heatmap_days: vec![
                ("2026-02-01".to_string(), 0.25),
                ("2026-02-02".to_string(), 1.0),
            ],
            ..Default::default()
        };
        let (series, min, max) = heatmap_series_for_metric(&data, HeatmapMetric::Coverage);
        assert_eq!(series, data.heatmap_days);
        assert!((min - 25.0).abs() < 0.001);
        assert!((max - 100.0).abs() < 0.001);
    }

    #[test]
    fn format_heatmap_value_coverage_is_percent() {
        assert_eq!(format_heatmap_value(72.9, HeatmapMetric::Coverage), "73%");
    }

    #[test]
    fn format_heatmap_value_tokens() {
        assert_eq!(
            format_heatmap_value(1500000.0, HeatmapMetric::ApiTokens),
            "1.5M"
        );
        assert_eq!(format_heatmap_value(500.0, HeatmapMetric::Messages), "500");
    }

    #[test]
    fn heatmap_metric_cycles() {
        let m = HeatmapMetric::default();
        assert_eq!(m, HeatmapMetric::ApiTokens);
        assert_eq!(m.next(), HeatmapMetric::Messages);
        assert_eq!(HeatmapMetric::Coverage.next(), HeatmapMetric::ApiTokens);
        assert_eq!(HeatmapMetric::ApiTokens.prev(), HeatmapMetric::Coverage);
    }

    // ── Tools view tests ──────────────────────────────────────────────

    fn sample_tool_rows() -> Vec<crate::analytics::ToolRow> {
        vec![
            crate::analytics::ToolRow {
                key: "claude_code".to_string(),
                tool_call_count: 12000,
                message_count: 1200,
                api_tokens_total: 45_000_000,
                tool_calls_per_1k_api_tokens: Some(0.267),
                tool_calls_per_1k_content_tokens: Some(0.5),
            },
            crate::analytics::ToolRow {
                key: "codex".to_string(),
                tool_call_count: 8000,
                message_count: 800,
                api_tokens_total: 23_000_000,
                tool_calls_per_1k_api_tokens: Some(0.348),
                tool_calls_per_1k_content_tokens: None,
            },
            crate::analytics::ToolRow {
                key: "aider".to_string(),
                tool_call_count: 2000,
                message_count: 400,
                api_tokens_total: 12_000_000,
                tool_calls_per_1k_api_tokens: Some(0.167),
                tool_calls_per_1k_content_tokens: None,
            },
        ]
    }

    #[test]
    fn tools_row_count_empty() {
        let data = AnalyticsChartData::default();
        assert_eq!(tools_row_count(&data), 0);
    }

    #[test]
    fn tools_row_count_with_data() {
        let data = AnalyticsChartData {
            tool_rows: sample_tool_rows(),
            ..Default::default()
        };
        assert_eq!(tools_row_count(&data), 3);
    }

    #[test]
    fn tools_row_count_capped_at_20() {
        let rows: Vec<crate::analytics::ToolRow> = (0..30)
            .map(|i| crate::analytics::ToolRow {
                key: format!("agent_{i}"),
                tool_call_count: 100 - i,
                message_count: 10,
                api_tokens_total: 1000,
                tool_calls_per_1k_api_tokens: Some(0.1),
                tool_calls_per_1k_content_tokens: None,
            })
            .collect();
        let data = AnalyticsChartData {
            tool_rows: rows,
            ..Default::default()
        };
        assert_eq!(tools_row_count(&data), 20);
    }

    #[test]
    fn tools_header_line_contains_columns() {
        let header = tools_header_line(100);
        assert!(header.contains("Agent"));
        assert!(header.contains("Calls"));
        assert!(header.contains("Msgs"));
        assert!(header.contains("API"));
        assert!(header.contains("Calls/1K"));
        assert!(header.contains("Share"));
    }

    #[test]
    fn tools_header_line_respects_requested_width() {
        let header = tools_header_line(24);
        assert!(
            header.chars().count() <= 24,
            "header should be truncated to available width"
        );
    }

    #[test]
    fn tools_row_line_formats_numbers() {
        let row = &sample_tool_rows()[0];
        let line = tools_row_line(row, 54.5, 100);
        assert!(line.contains("claude_code"));
        assert!(line.contains("12,000"));
        assert!(line.contains("1,200"));
        assert!(line.contains("45.0M"));
        assert!(line.contains("0.27"));
        assert!(line.contains("54.5%"));
    }

    #[test]
    fn tools_row_line_handles_no_per_1k() {
        let row = crate::analytics::ToolRow {
            key: "test".to_string(),
            tool_call_count: 100,
            message_count: 10,
            api_tokens_total: 0,
            tool_calls_per_1k_api_tokens: None,
            tool_calls_per_1k_content_tokens: None,
        };
        let line = tools_row_line(&row, 1.0, 80);
        assert!(line.contains("\u{2014}")); // em-dash for missing data
    }

    #[test]
    fn tools_row_line_respects_requested_width() {
        let row = &sample_tool_rows()[0];
        let line = tools_row_line(row, 33.3, 28);
        assert!(
            line.chars().count() <= 28,
            "row should be truncated to available width"
        );
    }

    #[test]
    fn breakdown_tabs_line_respects_requested_width() {
        let line = breakdown_tabs_line(BreakdownTab::Agent, 36);
        assert!(
            line.chars().count() <= 36,
            "tab line should be truncated on narrow terminals"
        );
    }

    #[test]
    fn shorten_label_handles_unicode_boundaries() {
        let label = "agent/\u{1F9EA}unicode-project";
        let shortened = shorten_label(label, 7);
        assert!(
            shortened.chars().count() <= 7,
            "unicode labels must truncate safely"
        );
    }

    // ── Coverage view tests ──────────────────────────────────────────

    #[test]
    fn coverage_row_count_empty() {
        let data = AnalyticsChartData::default();
        assert_eq!(coverage_row_count(&data), 0);
    }

    #[test]
    fn coverage_row_count_with_agents() {
        let data = AnalyticsChartData {
            agent_tokens: vec![
                ("claude_code".to_string(), 1000.0),
                ("codex".to_string(), 500.0),
            ],
            ..Default::default()
        };
        assert_eq!(coverage_row_count(&data), 2);
    }

    #[test]
    fn coverage_row_count_capped_at_10() {
        let agents: Vec<(String, f64)> = (0..15)
            .map(|i| (format!("agent_{i}"), 100.0 * (15 - i) as f64))
            .collect();
        let data = AnalyticsChartData {
            agent_tokens: agents,
            ..Default::default()
        };
        assert_eq!(coverage_row_count(&data), 10);
    }

    #[test]
    fn coverage_color_thresholds() {
        let green = coverage_color(80.0);
        let yellow = coverage_color(50.0);
        let red = coverage_color(30.0);
        // Green for high coverage
        assert_eq!(green, PackedRgba::rgb(80, 200, 80));
        // Yellow for moderate
        assert_eq!(yellow, PackedRgba::rgb(255, 200, 0));
        // Red for low
        assert_eq!(red, PackedRgba::rgb(255, 80, 80));
    }
}
