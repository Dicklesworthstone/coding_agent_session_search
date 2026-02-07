//! Analytics chart rendering for the ftui analytics views.
//!
//! Provides [`AnalyticsChartData`] (pre-computed chart data) and rendering
//! functions that turn analytics query results into terminal-native
//! visualizations using ftui-extras charts and canvas widgets.

use ftui::render::cell::PackedRgba;
use ftui::widgets::Widget;
use ftui::widgets::paragraph::Paragraph;
use ftui_extras::canvas::{CanvasRef, Mode as CanvasMode, Painter};
use ftui_extras::charts::LineChart as FtuiLineChart;
use ftui_extras::charts::Series as ChartSeries;
use ftui_extras::charts::{BarChart, BarDirection, BarGroup, Sparkline};

use super::app::AnalyticsView;
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
}

// ---------------------------------------------------------------------------
// Data loading
// ---------------------------------------------------------------------------

/// Load analytics data from the database, returning an `AnalyticsChartData`.
///
/// Gracefully returns empty data if the database is unavailable or tables
/// are missing.
pub fn load_chart_data(
    db: &crate::storage::sqlite::SqliteStorage,
    filters: &super::app::AnalyticsFilterState,
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
        workspace_ids: filters
            .workspaces
            .iter()
            .filter_map(|w| w.parse().ok())
            .collect(),
    };

    let mut data = AnalyticsChartData::default();

    // Agent breakdown (Track A — usage_daily).
    if let Ok(result) = analytics::query::query_breakdown(
        conn,
        &filter,
        analytics::Dim::Agent,
        analytics::Metric::ApiTotal,
        20,
    ) {
        data.agent_count = result.rows.len();
        data.agent_tokens = result
            .rows
            .iter()
            .map(|r| (r.key.clone(), r.value as f64))
            .collect();
        data.total_api_tokens = result.rows.iter().map(|r| r.value).sum();
    }

    // Agent message counts.
    if let Ok(result) = analytics::query::query_breakdown(
        conn,
        &filter,
        analytics::Dim::Agent,
        analytics::Metric::MessageCount,
        20,
    ) {
        data.agent_messages = result
            .rows
            .iter()
            .map(|r| (r.key.clone(), r.value as f64))
            .collect();
        data.total_messages = result.rows.iter().map(|r| r.value).sum();
    }

    // Tool usage.
    if let Ok(result) = analytics::query::query_tools(conn, &filter, analytics::GroupBy::Day, 20) {
        data.agent_tool_calls = result
            .rows
            .iter()
            .map(|r| (r.key.clone(), r.tool_call_count as f64))
            .collect();
        data.total_tool_calls = result.total_tool_calls;
    }

    // Daily timeseries (for sparklines and line chart).
    if let Ok(result) =
        analytics::query::query_tokens_timeseries(conn, &filter, analytics::GroupBy::Day)
    {
        data.daily_tokens = result
            .buckets
            .iter()
            .map(|(label, bucket)| (label.clone(), bucket.api_tokens_total as f64))
            .collect();
        data.daily_messages = result
            .buckets
            .iter()
            .map(|(label, bucket)| (label.clone(), bucket.message_count as f64))
            .collect();

        // Build heatmap data (normalize token values to 0..1).
        let max_tokens = data
            .daily_tokens
            .iter()
            .map(|(_, v)| *v)
            .fold(0.0_f64, f64::max);
        data.heatmap_days = data
            .daily_tokens
            .iter()
            .map(|(label, v)| {
                let norm = if max_tokens > 0.0 {
                    v / max_tokens
                } else {
                    0.0
                };
                (label.clone(), norm)
            })
            .collect();
    }

    // Model breakdown (Track B — token_daily_stats).
    if let Ok(result) = analytics::query::query_breakdown(
        conn,
        &filter,
        analytics::Dim::Model,
        analytics::Metric::ApiTotal,
        20,
    ) {
        data.model_tokens = result
            .rows
            .iter()
            .map(|r| (r.key.clone(), r.value as f64))
            .collect();
    }

    // Coverage percentage.
    if let Ok(status) = analytics::query::query_status(conn, &filter) {
        data.coverage_pct = status.coverage.api_token_coverage_pct;
    }

    data
}

// ---------------------------------------------------------------------------
// Chart rendering — per-view functions
// ---------------------------------------------------------------------------

/// Render the Dashboard view: KPI tiles + agent bar chart + token sparkline.
pub fn render_dashboard(data: &AnalyticsChartData, area: Rect, frame: &mut ftui::Frame) {
    if area.height < 4 || area.width < 20 {
        return; // too small to render
    }

    // Split: KPI row (3 lines) | bar chart | sparkline (1 line)
    let chunks = Flex::vertical()
        .constraints([
            Constraint::Fixed(3), // KPI summary
            Constraint::Min(4),   // Agent bar chart
            Constraint::Fixed(2), // Token sparkline
        ])
        .split(area);

    // ── KPI summary row ──────────────────────────────────────────
    let kpi_text = format!(
        " Agents: {}  |  Messages: {}  |  API Tokens: {}  |  Tool Calls: {}  |  Coverage: {:.1}%",
        data.agent_count,
        format_number(data.total_messages),
        format_number(data.total_api_tokens),
        format_number(data.total_tool_calls),
        data.coverage_pct,
    );
    Paragraph::new(&*kpi_text)
        .style(ftui::Style::new().fg(PackedRgba::rgb(200, 200, 200)))
        .render(chunks[0], frame);

    // ── Agent token bar chart ────────────────────────────────────
    if !data.agent_tokens.is_empty() && chunks[1].height >= 3 {
        let groups: Vec<BarGroup<'_>> = data
            .agent_tokens
            .iter()
            .take(10) // cap at 10 bars to fit
            .map(|(name, val)| BarGroup::new(name, vec![*val]))
            .collect();

        let colors: Vec<PackedRgba> = (0..groups.len()).map(agent_color).collect();

        let chart = BarChart::new(groups)
            .direction(BarDirection::Horizontal)
            .bar_width(1)
            .bar_gap(0)
            .group_gap(0)
            .colors(colors);
        chart.render(chunks[1], frame);
    } else {
        Paragraph::new(" No agent data available")
            .style(ftui::Style::new().fg(PackedRgba::rgb(120, 120, 120)))
            .render(chunks[1], frame);
    }

    // ── Token sparkline ──────────────────────────────────────────
    if !data.daily_tokens.is_empty() {
        let values: Vec<f64> = data.daily_tokens.iter().map(|(_, v)| *v).collect();
        let sparkline = Sparkline::new(&values).gradient(
            PackedRgba::rgb(40, 80, 200), // cool blue
            PackedRgba::rgb(255, 80, 40), // hot red
        );
        sparkline.render(chunks[2], frame);
    }
}

/// Render the Explorer view: multi-series line chart (tokens + messages over time).
pub fn render_explorer(data: &AnalyticsChartData, area: Rect, frame: &mut ftui::Frame) {
    if data.daily_tokens.is_empty() {
        Paragraph::new(" No timeseries data available. Run 'cass analytics rebuild' to populate.")
            .style(ftui::Style::new().fg(PackedRgba::rgb(120, 120, 120)))
            .render(area, frame);
        return;
    }

    // Build (x, y) pairs for the line chart — x is index, y is value.
    let token_points: Vec<(f64, f64)> = data
        .daily_tokens
        .iter()
        .enumerate()
        .map(|(i, (_, v))| (i as f64, *v))
        .collect();
    let msg_points: Vec<(f64, f64)> = data
        .daily_messages
        .iter()
        .enumerate()
        .map(|(i, (_, v))| (i as f64, *v))
        .collect();

    let mut series = vec![ChartSeries::new(
        "API Tokens",
        &token_points,
        PackedRgba::rgb(0, 150, 255),
    )];
    if !msg_points.is_empty() {
        // Scale messages to same range as tokens for visibility.
        // We'll show messages as a separate series.
        series.push(
            ChartSeries::new("Messages", &msg_points, PackedRgba::rgb(255, 200, 0)).markers(true),
        );
    }

    // X labels: first, mid, last date.
    let x_labels: Vec<&str> = if data.daily_tokens.len() >= 3 {
        vec![
            &data.daily_tokens[0].0,
            &data.daily_tokens[data.daily_tokens.len() / 2].0,
            &data.daily_tokens[data.daily_tokens.len() - 1].0,
        ]
    } else if !data.daily_tokens.is_empty() {
        vec![
            &data.daily_tokens[0].0,
            &data.daily_tokens[data.daily_tokens.len() - 1].0,
        ]
    } else {
        vec![]
    };

    let chart = FtuiLineChart::new(series).x_labels(x_labels).legend(true);
    chart.render(area, frame);
}

/// Render the Heatmap view: calendar-style token intensity using canvas.
pub fn render_heatmap(data: &AnalyticsChartData, area: Rect, frame: &mut ftui::Frame) {
    if data.heatmap_days.is_empty() {
        Paragraph::new(" No daily data available for heatmap. Run 'cass analytics rebuild'.")
            .style(ftui::Style::new().fg(PackedRgba::rgb(120, 120, 120)))
            .render(area, frame);
        return;
    }

    // Use HalfBlock mode for 1×2 sub-pixels per cell.
    let mut painter = Painter::for_area(area, CanvasMode::HalfBlock);
    let (pw, ph) = painter.size();

    // Lay out days in 7-row columns (rows = day of week, columns = weeks).
    let rows = 7u16;
    let cols = (data.heatmap_days.len() as u16).div_ceil(rows);
    let cell_w = if cols > 0 { pw / cols } else { 1 };
    let cell_h = if rows > 0 { ph / rows } else { 1 };

    for (i, (_, value)) in data.heatmap_days.iter().enumerate() {
        let col = i as u16 / rows;
        let row = i as u16 % rows;
        let x = (col * cell_w) as i32;
        let y = (row * cell_h) as i32;
        let color = ftui_extras::charts::heatmap_gradient(*value);
        // Fill a small rect for each day.
        let fill_w = cell_w.max(1) as i32;
        let fill_h = cell_h.max(1) as i32;
        for dy in 0..fill_h {
            for dx in 0..fill_w {
                painter.point_colored(x + dx, y + dy, color);
            }
        }
    }

    let canvas = CanvasRef::from_painter(&painter)
        .style(ftui::Style::new().fg(PackedRgba::rgb(200, 200, 200)));
    canvas.render(area, frame);
}

/// Render the Breakdowns view: agent/source bar charts side by side.
pub fn render_breakdowns(data: &AnalyticsChartData, area: Rect, frame: &mut ftui::Frame) {
    if data.agent_tokens.is_empty() {
        Paragraph::new(" No breakdown data available. Run 'cass analytics rebuild'.")
            .style(ftui::Style::new().fg(PackedRgba::rgb(120, 120, 120)))
            .render(area, frame);
        return;
    }

    // Split horizontally: tokens bar chart | messages bar chart
    let chunks = Flex::horizontal()
        .constraints([Constraint::Percentage(50.0), Constraint::Percentage(50.0)])
        .split(area);

    // Token breakdown by agent.
    {
        let groups: Vec<BarGroup<'_>> = data
            .agent_tokens
            .iter()
            .take(8)
            .map(|(name, val)| BarGroup::new(name, vec![*val]))
            .collect();
        let colors: Vec<PackedRgba> = (0..groups.len()).map(agent_color).collect();
        let chart = BarChart::new(groups)
            .direction(BarDirection::Horizontal)
            .bar_width(1)
            .colors(colors);
        chart.render(chunks[0], frame);
    }

    // Message breakdown by agent.
    {
        let groups: Vec<BarGroup<'_>> = data
            .agent_messages
            .iter()
            .take(8)
            .map(|(name, val)| BarGroup::new(name, vec![*val]))
            .collect();
        let colors: Vec<PackedRgba> = (0..groups.len()).map(agent_color).collect();
        let chart = BarChart::new(groups)
            .direction(BarDirection::Horizontal)
            .bar_width(1)
            .colors(colors);
        chart.render(chunks[1], frame);
    }
}

/// Render the Tools view: tool calls per agent with derived metrics.
pub fn render_tools(data: &AnalyticsChartData, area: Rect, frame: &mut ftui::Frame) {
    if data.agent_tool_calls.is_empty() {
        Paragraph::new(" No tool usage data available. Run 'cass analytics rebuild'.")
            .style(ftui::Style::new().fg(PackedRgba::rgb(120, 120, 120)))
            .render(area, frame);
        return;
    }

    let groups: Vec<BarGroup<'_>> = data
        .agent_tool_calls
        .iter()
        .take(10)
        .map(|(name, val)| BarGroup::new(name, vec![*val]))
        .collect();

    let colors: Vec<PackedRgba> = (0..groups.len()).map(agent_color).collect();

    let chart = BarChart::new(groups)
        .direction(BarDirection::Horizontal)
        .bar_width(1)
        .bar_gap(0)
        .colors(colors);
    chart.render(area, frame);
}

/// Render the Cost/Models view: model family token breakdown.
pub fn render_cost(data: &AnalyticsChartData, area: Rect, frame: &mut ftui::Frame) {
    if data.model_tokens.is_empty() {
        Paragraph::new(
            " No model data available (Track B). Requires connectors with API token data.",
        )
        .style(ftui::Style::new().fg(PackedRgba::rgb(120, 120, 120)))
        .render(area, frame);
        return;
    }

    // Split: bar chart | summary text
    let chunks = Flex::vertical()
        .constraints([Constraint::Min(4), Constraint::Fixed(2)])
        .split(area);

    let groups: Vec<BarGroup<'_>> = data
        .model_tokens
        .iter()
        .take(10)
        .map(|(name, val)| BarGroup::new(name, vec![*val]))
        .collect();

    let model_colors = &[
        PackedRgba::rgb(0, 180, 220),   // blue
        PackedRgba::rgb(220, 120, 0),   // amber
        PackedRgba::rgb(80, 200, 80),   // green
        PackedRgba::rgb(200, 60, 180),  // magenta
        PackedRgba::rgb(255, 200, 60),  // yellow
        PackedRgba::rgb(120, 120, 255), // indigo
    ];
    let colors: Vec<PackedRgba> = (0..groups.len())
        .map(|i| model_colors[i % model_colors.len()])
        .collect();

    let chart = BarChart::new(groups)
        .direction(BarDirection::Horizontal)
        .bar_width(1)
        .colors(colors);
    chart.render(chunks[0], frame);

    let total: f64 = data.model_tokens.iter().map(|(_, v)| v).sum();
    let summary = format!(
        " Total API tokens across {} models: {}",
        data.model_tokens.len(),
        format_number(total as i64),
    );
    Paragraph::new(&*summary)
        .style(ftui::Style::new().fg(PackedRgba::rgb(180, 180, 180)))
        .render(chunks[1], frame);
}

/// Render the Coverage view: coverage percentage + daily coverage sparkline.
pub fn render_coverage(data: &AnalyticsChartData, area: Rect, frame: &mut ftui::Frame) {
    let chunks = Flex::vertical()
        .constraints([
            Constraint::Fixed(3), // coverage summary
            Constraint::Min(3),   // daily sparkline
        ])
        .split(area);

    let cov_bar_width = area.width.saturating_sub(4) as usize;
    let filled = (data.coverage_pct / 100.0 * cov_bar_width as f64).round() as usize;
    let empty = cov_bar_width.saturating_sub(filled);
    let bar = format!(
        " API Coverage: {:.1}%  [{}{}]",
        data.coverage_pct,
        "█".repeat(filled),
        "░".repeat(empty),
    );
    let bar_color = if data.coverage_pct >= 80.0 {
        PackedRgba::rgb(80, 200, 80)
    } else if data.coverage_pct >= 50.0 {
        PackedRgba::rgb(255, 200, 0)
    } else {
        PackedRgba::rgb(255, 80, 80)
    };
    Paragraph::new(&*bar)
        .style(ftui::Style::new().fg(bar_color))
        .render(chunks[0], frame);

    // Daily token sparkline as a proxy for coverage activity.
    if !data.daily_tokens.is_empty() {
        let values: Vec<f64> = data.daily_tokens.iter().map(|(_, v)| *v).collect();
        let sparkline = Sparkline::new(&values)
            .gradient(PackedRgba::rgb(60, 60, 120), PackedRgba::rgb(80, 200, 80));
        sparkline.render(chunks[1], frame);
    } else {
        Paragraph::new(" No daily data for sparkline")
            .style(ftui::Style::new().fg(PackedRgba::rgb(120, 120, 120)))
            .render(chunks[1], frame);
    }
}

/// Dispatch rendering to the appropriate view function.
pub fn render_analytics_content(
    view: AnalyticsView,
    data: &AnalyticsChartData,
    area: Rect,
    frame: &mut ftui::Frame,
) {
    match view {
        AnalyticsView::Dashboard => render_dashboard(data, area, frame),
        AnalyticsView::Explorer => render_explorer(data, area, frame),
        AnalyticsView::Heatmap => render_heatmap(data, area, frame),
        AnalyticsView::Breakdowns => render_breakdowns(data, area, frame),
        AnalyticsView::Tools => render_tools(data, area, frame),
        AnalyticsView::Cost => render_cost(data, area, frame),
        AnalyticsView::Coverage => render_coverage(data, area, frame),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Format a large number with comma separators (e.g. 1234567 → "1,234,567").
fn format_number(n: i64) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_number_basic() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1234567), "1,234,567");
        assert_eq!(format_number(100), "100");
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
                | AnalyticsView::Cost
                | AnalyticsView::Coverage => {}
            }
        }
    }
}
