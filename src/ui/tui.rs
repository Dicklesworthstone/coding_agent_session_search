//! Ratatui-based interface wired to Tantivy search.

use anyhow::Result;
use chrono::{DateTime, Datelike, Utc};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io;
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use crate::default_data_dir;
use crate::model::types::MessageRole;
use crate::search::query::{SearchClient, SearchFilters, SearchHit};
use crate::search::tantivy::index_dir;
use crate::ui::components::theme::ThemePalette;
use crate::ui::components::widgets::search_bar;
use crate::ui::data::{ConversationView, InputMode, load_conversation, role_style};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetailTab {
    Messages,
    Snippets,
    Raw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatchMode {
    Standard,
    Prefix,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RankingMode {
    RecentHeavy,
    Balanced,
    RelevanceHeavy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextWindow {
    Small,
    Medium,
    Large,
    XLarge,
}

impl ContextWindow {
    fn next(self) -> Self {
        match self {
            ContextWindow::Small => ContextWindow::Medium,
            ContextWindow::Medium => ContextWindow::Large,
            ContextWindow::Large => ContextWindow::XLarge,
            ContextWindow::XLarge => ContextWindow::Small,
        }
    }

    fn size(self) -> usize {
        match self {
            ContextWindow::Small => 80,
            ContextWindow::Medium => 160,
            ContextWindow::Large => 320,
            ContextWindow::XLarge => 640,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ContextWindow::Small => "S",
            ContextWindow::Medium => "M",
            ContextWindow::Large => "L",
            ContextWindow::XLarge => "XL",
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct TuiStatePersisted {
    match_mode: Option<String>,
    context_window: Option<String>,
    /// Set to true after user dismisses help overlay for the first time.
    /// Prevents help from auto-showing on subsequent launches.
    has_seen_help: Option<bool>,
    /// Recently used search queries, most recent first. Persisted across sessions.
    query_history: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
struct AgentPane {
    agent: String,
    hits: Vec<SearchHit>,
    selected: usize,
    /// Total number of results for this agent (may be more than hits.len() due to limit)
    total_count: usize,
}

/// Returns style modifiers based on score magnitude.
/// High scores (>8) get bold, medium scores (>5) normal, low scores dimmed.
fn score_style(score: f32) -> Modifier {
    if score >= 8.0 {
        Modifier::BOLD
    } else if score >= 5.0 {
        Modifier::empty()
    } else {
        Modifier::DIM
    }
}

/// Creates a visual bar representation of score: `████░░ 8.2`
/// Uses 5-character bar with filled/empty blocks proportional to score (0-10 scale).
fn score_bar(score: f32, color: ratatui::style::Color) -> Vec<Span<'static>> {
    let normalized = (score / 10.0).clamp(0.0, 1.0);
    let filled = (normalized * 5.0).round() as usize;
    let empty = 5 - filled;
    vec![
        Span::styled(
            "█".repeat(filled),
            Style::default().fg(color).add_modifier(score_style(score)),
        ),
        Span::styled(
            "░".repeat(empty),
            Style::default().fg(color).add_modifier(Modifier::DIM),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{:.1}", score),
            Style::default().fg(color).add_modifier(score_style(score)),
        ),
    ]
}

/// Linear interpolation between two u8 values.
/// t=0.0 returns a, t=1.0 returns b.
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0)) as u8
}

/// Interpolates between two colors based on progress (0.0 to 1.0).
/// For RGB colors, performs smooth linear interpolation.
/// For non-RGB colors, falls back to binary switch at 50%.
fn lerp_color(
    from: ratatui::style::Color,
    to: ratatui::style::Color,
    progress: f32,
) -> ratatui::style::Color {
    use ratatui::style::Color;
    match (from, to) {
        (Color::Rgb(fr, fg, fb), Color::Rgb(tr, tg, tb)) => Color::Rgb(
            lerp_u8(fr, tr, progress),
            lerp_u8(fg, tg, progress),
            lerp_u8(fb, tb, progress),
        ),
        // Convert named accent colors to approximate RGB values for smooth fades
        (Color::Rgb(fr, fg, fb), named) => {
            let (tr, tg, tb) = named_color_to_rgb(named);
            Color::Rgb(
                lerp_u8(fr, tr, progress),
                lerp_u8(fg, tg, progress),
                lerp_u8(fb, tb, progress),
            )
        }
        (named, Color::Rgb(tr, tg, tb)) => {
            let (fr, fg, fb) = named_color_to_rgb(named);
            Color::Rgb(
                lerp_u8(fr, tr, progress),
                lerp_u8(fg, tg, progress),
                lerp_u8(fb, tb, progress),
            )
        }
        // Both named: binary switch at halfway point
        _ => {
            if progress < 0.5 {
                from
            } else {
                to
            }
        }
    }
}

/// Converts named colors to approximate RGB values for interpolation.
fn named_color_to_rgb(color: ratatui::style::Color) -> (u8, u8, u8) {
    use ratatui::style::Color;
    match color {
        Color::Black => (0, 0, 0),
        Color::Red => (205, 0, 0),
        Color::Green => (0, 205, 0),
        Color::Yellow => (205, 205, 0),
        Color::Blue => (0, 0, 238),
        Color::Magenta => (205, 0, 205),
        Color::Cyan => (0, 205, 205),
        Color::Gray => (128, 128, 128),
        Color::DarkGray => (85, 85, 85),
        Color::LightRed => (255, 85, 85),
        Color::LightGreen => (85, 255, 85),
        Color::LightYellow => (255, 255, 85),
        Color::LightBlue => (85, 85, 255),
        Color::LightMagenta => (255, 85, 255),
        Color::LightCyan => (85, 255, 255),
        Color::White => (255, 255, 255),
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(idx) => {
            // Basic 16-color approximation for indexed colors
            if idx < 16 {
                match idx {
                    0 => (0, 0, 0),
                    1 => (128, 0, 0),
                    2 => (0, 128, 0),
                    3 => (128, 128, 0),
                    4 => (0, 0, 128),
                    5 => (128, 0, 128),
                    6 => (0, 128, 128),
                    7 => (192, 192, 192),
                    8 => (128, 128, 128),
                    9 => (255, 0, 0),
                    10 => (0, 255, 0),
                    11 => (255, 255, 0),
                    12 => (0, 0, 255),
                    13 => (255, 0, 255),
                    14 => (0, 255, 255),
                    15 => (255, 255, 255),
                    _ => (128, 128, 128),
                }
            } else {
                (128, 128, 128) // Default gray for extended palette
            }
        }
        Color::Reset => (255, 255, 255),
    }
}

/// Calculates flash animation progress from 0.0 (just started) to 1.0 (complete).
/// Returns 1.0 if no flash is active.
fn flash_progress(flash_until: Option<Instant>, duration_ms: u64) -> f32 {
    match flash_until {
        Some(end_time) => {
            let now = Instant::now();
            if now >= end_time {
                1.0 // Animation complete
            } else {
                let remaining = end_time.duration_since(now).as_millis() as f32;
                let total = duration_ms as f32;
                // Progress is 0.0 at start (full remaining), 1.0 at end (0 remaining)
                1.0 - (remaining / total).clamp(0.0, 1.0)
            }
        }
        None => 1.0, // No flash active
    }
}

/// Truncates a file path for display, preserving readability.
/// - Replaces home directory with ~
/// - Keeps first and last path components for context
/// - Uses "..." in the middle for long paths
fn truncate_path(path: &str, max_len: usize) -> String {
    // Replace home directory with ~
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_default();

    let display_path = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };

    // If it fits, return as-is
    if display_path.len() <= max_len {
        return display_path;
    }

    // Split path into non-empty components
    let parts: Vec<&str> = display_path.split('/').filter(|s| !s.is_empty()).collect();

    // Need at least 3 parts to truncate meaningfully
    if parts.len() <= 2 {
        // Just truncate from the right
        let ellipsis = "...";
        let available = max_len.saturating_sub(ellipsis.len());
        return format!(
            "{}{}",
            &display_path[..available.min(display_path.len())],
            ellipsis
        );
    }

    // Determine the leading prefix based on path type
    let prefix = if display_path.starts_with('~') {
        "~"
    } else if display_path.starts_with('/') {
        "" // Will add / in format string
    } else {
        parts[0] // Relative path, use first component
    };

    // For absolute/home paths, use all parts; for relative, skip first (already in prefix)
    let skip_first = !display_path.starts_with('/') && !display_path.starts_with('~');
    let relevant_parts: Vec<&str> = if skip_first {
        parts[1..].to_vec()
    } else {
        parts.clone()
    };

    let second_last = relevant_parts
        .get(relevant_parts.len().saturating_sub(2))
        .unwrap_or(&"");
    let last = relevant_parts.last().unwrap_or(&"");

    // Build truncated path
    let truncated = if display_path.starts_with('/') {
        format!("/.../{}/{}", second_last, last)
    } else if display_path.starts_with('~') {
        format!("~/.../{}/{}", second_last, last)
    } else {
        format!("{}/.../{}/{}", prefix, second_last, last)
    };

    // If truncated is still too long, fall back to just showing the filename
    if truncated.len() > max_len && !last.is_empty() {
        let result = format!(".../{}", last);
        if result.len() <= max_len {
            return result;
        }
        // Last resort: truncate the filename itself
        let available = max_len.saturating_sub(4); // ".../"
        return format!(".../{}", &last[..available.min(last.len())]);
    }

    truncated
}

/// Generates contextual empty state messages with actionable suggestions.
/// The suggestions are tailored based on the current query, filters, and search mode.
fn contextual_empty_state(
    query: &str,
    filters: &SearchFilters,
    match_mode: MatchMode,
    palette: ThemePalette,
    fuzzy_suggestion: Option<&str>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    // Show the query they searched for
    if query.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "No results found".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from("Start typing to search your conversations."));
    } else {
        lines.push(Line::from(vec![
            Span::styled(
                "No results for ".to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("\"{}\"", query),
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        // Show "Did you mean?" suggestion if available
        if let Some(suggestion) = fuzzy_suggestion {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(
                    "Did you mean: ".to_string(),
                    Style::default().fg(palette.hint),
                ),
                Span::styled(
                    format!("\"{}\"", suggestion),
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                ),
                Span::styled(" ?".to_string(), Style::default().fg(palette.hint)),
            ]));
        }
    }

    lines.push(Line::from(""));

    // Build contextual suggestions
    let mut suggestions: Vec<String> = Vec::new();

    // Agent filter suggestion
    if !filters.agents.is_empty() {
        let agents: Vec<_> = filters.agents.iter().cloned().collect();
        let agent_str = if agents.len() > 1 {
            format!("{} agents", agents.len())
        } else {
            agents.first().cloned().unwrap_or_default()
        };
        suggestions.push(format!("Clear agent filter: {} (Shift+F3)", agent_str));
    }

    // Workspace filter suggestion
    if !filters.workspaces.is_empty() {
        suggestions.push("Clear workspace filter (Shift+F4)".to_string());
    }

    // Time filter suggestion
    if filters.created_from.is_some() || filters.created_to.is_some() {
        suggestions.push("Remove time filter (Ctrl+Del clears all)".to_string());
    }

    // Match mode suggestion
    if matches!(match_mode, MatchMode::Standard) {
        suggestions.push("Try prefix mode for partial matches (F9)".to_string());
    }

    // Query-based suggestions
    if query.len() > 20 {
        suggestions.push("Try shorter, more specific search terms".to_string());
    }

    if query.contains(' ') && query.split_whitespace().count() > 3 {
        suggestions.push("Try fewer keywords".to_string());
    }

    // If still no suggestions, add generic ones
    if suggestions.is_empty() {
        suggestions.push("Check spelling".to_string());
        suggestions.push("Try different keywords".to_string());
        suggestions.push("Run 'cass index --full' to ensure all data is indexed".to_string());
    }

    // Render suggestions
    lines.push(Line::from(Span::styled(
        "Suggestions:".to_string(),
        Style::default().fg(palette.accent),
    )));
    for s in suggestions {
        lines.push(Line::from(format!("  • {}", s)));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press Ctrl+Del to clear all filters".to_string(),
        Style::default().fg(palette.hint),
    )));

    lines
}

/// Formats a timestamp as a relative time string ("2h ago", "3d ago", etc.)
/// Falls back to absolute date for timestamps older than 30 days.
fn format_relative_time(timestamp_ms: i64) -> String {
    let now = Utc::now().timestamp_millis();
    let diff_ms = now - timestamp_ms;

    if diff_ms < 0 {
        return "in the future".to_string();
    }

    let seconds = diff_ms / 1000;
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;

    if seconds < 60 {
        "just now".to_string()
    } else if minutes < 60 {
        format!("{}m ago", minutes)
    } else if hours < 24 {
        format!("{}h ago", hours)
    } else if days < 7 {
        format!("{}d ago", days)
    } else if days < 30 {
        format!("{}w ago", days / 7)
    } else {
        // For older timestamps, show absolute date
        DateTime::from_timestamp_millis(timestamp_ms)
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }
}

fn help_lines(palette: ThemePalette) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let add_section = |title: &str, items: &[&str]| -> Vec<Line<'static>> {
        let mut v = Vec::new();
        v.push(Line::from(Span::styled(title.to_string(), palette.title())));
        for item in items {
            v.push(Line::from(format!("  {item}")));
        }
        v.push(Line::from(""));
        v
    };

    lines.extend(add_section(
        "Search",
        &[
            "type to live-search; / focuses query; Ctrl-R cycles history",
            "Ctrl+Shift+R refresh search (re-query index)",
        ],
    ));
    lines.extend(add_section(
        "Filters",
        &[
            "F3 agent | F4 workspace | F5 from | F6 to | Ctrl+Del clear all",
            "Shift+F3 scope to active agent | Shift+F4 clear scope | Shift+F5 cycle time presets (24h/7d/30d/all)",
            "Chips in search bar; Backspace removes last; Enter (query empty) edits last chip",
        ],
    ));
    lines.extend(add_section(
        "Modes",
        &[
            "F9 match mode: prefix (default) ⇄ standard",
            "F12 ranking: recent-heavy → balanced → relevance-heavy",
            "F2 theme: dark/light",
        ],
    ));
    lines.extend(add_section(
        "Context",
        &[
            "F7 cycles S/M/L/XL context window",
            "Space: peek XL for current hit, tap again to restore",
        ],
    ));
    lines.extend(add_section(
        "Density",
        &["Shift+=/+ increase pane items; - decrease (min 4, max 50)"],
    ));
    lines.extend(add_section(
        "Navigation",
        &[
            "Arrows move; Left/Right pane; PgUp/PgDn page",
            "Vim: h/j/k/l (left/down/up/right) when results showing",
            "Alt+NumPad 1-9 jump pane; g/G jump first/last item",
            "Tab toggles focus (Results ⇄ Detail)",
            "[ / ] cycle detail tabs (Messages/Snippets/Raw)",
        ],
    ));
    lines.extend(add_section(
        "Mouse",
        &[
            "Click pane/item to select; click detail area to focus",
            "Scroll wheel: navigate results or scroll detail",
        ],
    ));
    lines.extend(add_section(
        "Actions",
        &[
            "Enter opens detail modal (c=copy, n=nano, Esc=close)",
            "F8 open hit in $EDITOR; y copy path/content",
            "F1 toggle this help; Esc/F10 quit (or back from detail)",
        ],
    ));
    lines.extend(add_section(
        "States",
        &["match mode + context persist in tui_state.json (data dir); delete to reset"],
    ));
    lines.extend(add_section(
        "Empty state",
        &[
            "Shows recent per-agent hits before typing",
            "Recent query suggestions appear when query is empty",
        ],
    ));

    lines
}

fn render_help_overlay(frame: &mut Frame, palette: ThemePalette, scroll: u16) {
    let area = frame.area();
    let popup_area = centered_rect(70, 70, area);
    let lines = help_lines(palette);
    let block = Block::default()
        .title(Span::styled("Help / Shortcuts", palette.title()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.accent));

    frame.render_widget(ratatui::widgets::Clear, popup_area);

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: true })
            .scroll((scroll, 0)),
        popup_area,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Percentage((100 - percent_y) / 2),
                Constraint::Percentage(percent_y),
                Constraint::Percentage((100 - percent_y) / 2),
            ]
            .as_ref(),
        )
        .split(r);

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(
            [
                Constraint::Percentage((100 - percent_x) / 2),
                Constraint::Percentage(percent_x),
                Constraint::Percentage((100 - percent_x) / 2),
            ]
            .as_ref(),
        )
        .split(popup_layout[1]);

    horizontal[1]
}

/// Render parsed content lines from a conversation for the detail modal.
/// Parses tool use, code blocks, and formats beautifully for human reading.
fn render_parsed_content(
    detail: &ConversationView,
    query: &str,
    palette: ThemePalette,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Header with conversation info
    if let Some(title) = &detail.convo.title {
        lines.push(Line::from(vec![
            Span::styled("📋 ", Style::default()),
            Span::styled(
                title.clone(),
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));
    }

    // Workspace info
    if let Some(ws) = &detail.workspace {
        lines.push(Line::from(vec![
            Span::styled("📁 Workspace: ", Style::default().fg(palette.hint)),
            Span::styled(
                ws.display_name
                    .clone()
                    .unwrap_or_else(|| ws.path.display().to_string()),
                Style::default().fg(palette.fg),
            ),
        ]));
        lines.push(Line::from(""));
    }

    // Time info
    if let Some(ts) = detail.convo.started_at {
        lines.push(Line::from(vec![
            Span::styled("🕐 Started: ", Style::default().fg(palette.hint)),
            Span::styled(format_relative_time(ts), Style::default().fg(palette.fg)),
        ]));
        lines.push(Line::from(""));
    }

    lines.push(Line::from(Span::styled(
        "─".repeat(60),
        Style::default().fg(palette.hint),
    )));
    lines.push(Line::from(""));

    // Render messages with beautiful formatting
    for msg in &detail.messages {
        let (role_icon, role_label, role_color) = match &msg.role {
            MessageRole::User => ("👤", "You", palette.user),
            MessageRole::Agent => ("🤖", "Assistant", palette.agent),
            MessageRole::Tool => ("🔧", "Tool", palette.tool),
            MessageRole::System => ("⚙️", "System", palette.system),
            MessageRole::Other(r) => ("📝", r.as_str(), palette.hint),
        };

        // Role header with timestamp
        let ts_text = msg
            .created_at
            .map(|t| format!(" · {}", format_relative_time(t)))
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!("{} ", role_icon), Style::default()),
            Span::styled(
                role_label.to_string(),
                Style::default().fg(role_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(ts_text, Style::default().fg(palette.hint)),
        ]));
        lines.push(Line::from(""));

        // Parse and render content
        let content = &msg.content;
        let parsed_lines = parse_message_content(content, query, palette);
        lines.extend(parsed_lines);
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "─".repeat(60),
            Style::default()
                .fg(palette.hint)
                .add_modifier(Modifier::DIM),
        )));
        lines.push(Line::from(""));
    }

    lines
}

/// Parse message content and render with beautiful formatting.
/// Handles code blocks, tool calls, JSON, and highlights search terms.
fn parse_message_content(content: &str, query: &str, palette: ThemePalette) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code_block = false;
    let mut code_lang: Option<String> = None;
    let mut code_buffer: Vec<String> = Vec::new();

    for line_text in content.lines() {
        let trimmed = line_text.trim_start();

        // Handle code block start/end
        if trimmed.starts_with("```") {
            if in_code_block {
                // End of code block - render buffered code
                in_code_block = false;
                if !code_buffer.is_empty() {
                    let lang_label = code_lang
                        .take()
                        .filter(|l| !l.is_empty())
                        .map(|l| format!(" {}", l))
                        .unwrap_or_default();
                    lines.push(Line::from(vec![
                        Span::styled("┌──", Style::default().fg(palette.hint)),
                        Span::styled(
                            lang_label,
                            Style::default()
                                .fg(palette.accent_alt)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    for code_line in code_buffer.drain(..) {
                        lines.push(Line::from(vec![
                            Span::styled("│ ", Style::default().fg(palette.hint)),
                            Span::styled(
                                code_line,
                                Style::default().fg(palette.fg).bg(palette.surface),
                            ),
                        ]));
                    }
                    lines.push(Line::from(Span::styled(
                        "└──",
                        Style::default().fg(palette.hint),
                    )));
                }
            } else {
                // Start of code block - extract language (first word after ```)
                in_code_block = true;
                let lang_str = trimmed.trim_start_matches('`');
                code_lang = Some(lang_str.split_whitespace().next().unwrap_or("").to_string());
            }
            continue;
        }

        if in_code_block {
            code_buffer.push(line_text.to_string());
            continue;
        }

        // Handle tool call markers
        if trimmed.starts_with("[Tool:") || trimmed.starts_with("⚙️") {
            lines.push(Line::from(vec![
                Span::styled("  🔧 ", Style::default()),
                Span::styled(
                    line_text.trim().to_string(),
                    Style::default()
                        .fg(palette.tool)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
            continue;
        }

        // Try to detect and format JSON objects on a single line
        if ((trimmed.starts_with('{') && trimmed.ends_with('}'))
            || (trimmed.starts_with('[') && trimmed.ends_with(']')))
            && let Ok(json_val) = serde_json::from_str::<serde_json::Value>(trimmed)
        {
            // Pretty print JSON
            if let Ok(pretty) = serde_json::to_string_pretty(&json_val) {
                lines.push(Line::from(Span::styled(
                    "  ┌── JSON",
                    Style::default().fg(palette.hint),
                )));
                for json_line in pretty.lines() {
                    lines.push(Line::from(vec![
                        Span::styled("  │ ", Style::default().fg(palette.hint)),
                        Span::styled(
                            json_line.to_string(),
                            Style::default().fg(palette.accent_alt),
                        ),
                    ]));
                }
                lines.push(Line::from(Span::styled(
                    "  └──",
                    Style::default().fg(palette.hint),
                )));
                continue;
            }
        }

        // Regular text with search term highlighting
        let rendered = highlight_terms_owned_with_style(
            format!("  {}", line_text),
            query,
            palette,
            Style::default(),
        );
        lines.push(rendered);
    }

    // Handle unclosed code block
    if in_code_block && !code_buffer.is_empty() {
        lines.push(Line::from(Span::styled(
            "┌── code",
            Style::default().fg(palette.hint),
        )));
        for code_line in code_buffer {
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(palette.hint)),
                Span::styled(
                    code_line,
                    Style::default().fg(palette.fg).bg(palette.surface),
                ),
            ]));
        }
        lines.push(Line::from(Span::styled(
            "└──",
            Style::default().fg(palette.hint),
        )));
    }

    lines
}

/// Render the full-screen detail modal for viewing parsed conversation content.
fn render_detail_modal(
    frame: &mut Frame,
    detail: &ConversationView,
    hit: &SearchHit,
    query: &str,
    palette: ThemePalette,
    scroll: u16,
) {
    let area = frame.area();
    // Use near-full-screen for maximum readability
    let popup_area = centered_rect(90, 90, area);

    let lines = render_parsed_content(detail, query, palette);
    let total_lines = lines.len();
    // Clamp scroll for display (actual scroll handled by Paragraph)
    let display_line = (scroll as usize).min(total_lines.saturating_sub(1)) + 1;

    // Build title with scroll position and hints
    let title_text = format!(
        " {} · line {}/{} · Esc close · c copy · n nano ",
        hit.title, display_line, total_lines
    );

    let block = Block::default()
        .title(Span::styled(
            title_text,
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.accent));

    frame.render_widget(ratatui::widgets::Clear, popup_area);

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        popup_area,
    );
}

/// Calculate optimal items per pane based on terminal height.
///
/// Layout overhead (approximate):
/// - 1 line top margin
/// - 3 lines search bar (border + query + tips)
/// - 1 line filter pills
/// - 2 lines pane borders (top + bottom)
/// - 1 line footer
/// - 1 line bottom margin
///
/// Total: ~9 lines overhead.
/// Results area is 70% of remaining height.
/// Each item is ~2 lines (title + snippet).
fn calculate_pane_limit(terminal_height: u16) -> usize {
    const OVERHEAD: u16 = 9;
    const RESULTS_PERCENT: f32 = 0.70;
    const LINES_PER_ITEM: usize = 2;
    const MIN_ITEMS: usize = 4;
    const MAX_ITEMS: usize = 50;

    let available = terminal_height.saturating_sub(OVERHEAD);
    let results_height = (available as f32 * RESULTS_PERCENT) as usize;
    let items = results_height / LINES_PER_ITEM;
    items.clamp(MIN_ITEMS, MAX_ITEMS)
}

fn build_agent_panes(results: &[SearchHit], per_pane_limit: usize) -> Vec<AgentPane> {
    use std::collections::HashMap;

    // First pass: count total hits per agent
    let mut counts: HashMap<String, usize> = HashMap::new();
    for hit in results {
        *counts.entry(hit.agent.clone()).or_insert(0) += 1;
    }

    // Second pass: build panes with limit
    let mut panes: Vec<AgentPane> = Vec::new();
    for hit in results {
        if let Some(pane) = panes.iter_mut().find(|p| p.agent == hit.agent) {
            if pane.hits.len() < per_pane_limit {
                pane.hits.push(hit.clone());
            }
        } else {
            panes.push(AgentPane {
                agent: hit.agent.clone(),
                hits: vec![hit.clone()],
                selected: 0,
                total_count: *counts.get(&hit.agent).unwrap_or(&1),
            });
        }
    }
    panes
}

fn active_hit(panes: &[AgentPane], active_idx: usize) -> Option<&SearchHit> {
    panes
        .get(active_idx)
        .and_then(|pane| pane.hits.get(pane.selected))
}

/// Known agent slugs for autocomplete suggestions
const KNOWN_AGENTS: &[&str] = &[
    "claude_code",
    "codex",
    "cline",
    "gemini",
    "gemini_cli",
    "amp",
    "opencode",
];

/// Returns agent suggestions matching the given prefix (case-insensitive)
fn agent_suggestions(prefix: &str) -> Vec<&'static str> {
    let prefix_lower = prefix.to_lowercase();
    KNOWN_AGENTS
        .iter()
        .filter(|agent| agent.to_lowercase().starts_with(&prefix_lower))
        .copied()
        .collect()
}

/// Suggests a correction for a query based on history.
/// Uses Levenshtein distance to find close matches (max edit distance 2).
/// Only suggests if the history item is different from the query.
fn suggest_correction(query: &str, history: &std::collections::VecDeque<String>) -> Option<String> {
    use strsim::levenshtein;

    if query.len() < 3 {
        return None; // Don't suggest for very short queries
    }

    let query_lower = query.to_lowercase();

    history
        .iter()
        .filter(|h| {
            let h_lower = h.to_lowercase();
            // Must be different from query (otherwise it's not a correction)
            // and within edit distance 2
            h.len() >= 3 && h_lower != query_lower && levenshtein(&query_lower, &h_lower) <= 2
        })
        .min_by_key(|h| levenshtein(&query_lower, &h.to_lowercase()))
        .cloned()
}

fn agent_display_name(agent: &str) -> String {
    agent
        .replace(['_', '-'], " ")
        .split_whitespace()
        .map(|w| {
            let mut chars = w.chars();
            if let Some(first) = chars.next() {
                format!("{}{}", first.to_uppercase(), chars.as_str())
            } else {
                String::new()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn contextual_snippet(text: &str, query: &str, window: ContextWindow) -> String {
    let size = window.size();
    if text.is_empty() {
        return String::new();
    }
    let lowercase = text.to_lowercase();
    let q = query.to_lowercase();

    let byte_pos = if q.is_empty() {
        Some(0)
    } else {
        lowercase.find(&q)
    }
    .or_else(|| {
        q.split_whitespace()
            .next()
            .and_then(|first| lowercase.find(first))
    });

    let chars: Vec<char> = text.chars().collect();
    let char_pos = byte_pos.map(|b| text[..b].chars().count()).unwrap_or(0);
    let len = chars.len();
    let start = char_pos.saturating_sub(size / 2);
    let end = (start + size).min(len);
    let slice: String = chars[start..end].iter().collect();
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if end < len { "…" } else { "" };
    format!("{prefix}{slice}{suffix}")
}

fn state_path_for(data_dir: &std::path::Path) -> std::path::PathBuf {
    // Persist lightweight, non-secret UI preferences (match mode, context window).
    data_dir.join("tui_state.json")
}

fn chips_for_filters(filters: &SearchFilters, palette: ThemePalette) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if !filters.agents.is_empty() {
        spans.push(Span::styled(
            format!(
                "[agent:{}]",
                filters.agents.iter().cloned().collect::<Vec<_>>().join("|")
            ),
            Style::default()
                .fg(palette.accent_alt)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" ".to_string()));
    }
    if !filters.workspaces.is_empty() {
        spans.push(Span::styled(
            format!(
                "[ws:{}]",
                filters
                    .workspaces
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("|")
            ),
            Style::default().fg(palette.accent_alt),
        ));
        spans.push(Span::raw(" ".to_string()));
    }
    if filters.created_from.is_some() || filters.created_to.is_some() {
        let chip_text = format_time_chip(filters.created_from, filters.created_to);
        if !chip_text.is_empty() {
            spans.push(Span::styled(
                chip_text,
                Style::default().fg(palette.accent_alt),
            ));
            spans.push(Span::raw(" ".to_string()));
        }
    }
    spans
}

fn load_state(path: &std::path::Path) -> TuiStatePersisted {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(path: &std::path::Path, state: &TuiStatePersisted) {
    if let Ok(body) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(path, body);
    }
}

fn apply_match_mode(query: &str, mode: MatchMode) -> String {
    match mode {
        MatchMode::Standard => query.to_string(),
        MatchMode::Prefix => query
            .split_whitespace()
            .filter(|s| !s.is_empty())
            .map(|term| {
                if term.ends_with('*') {
                    term.to_string()
                } else {
                    format!("{term}*")
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn highlight_terms_owned_with_style(
    text: String,
    query: &str,
    palette: ThemePalette,
    base: Style,
) -> Line<'static> {
    let owned = text;
    let mut spans: Vec<Span<'static>> = Vec::new();
    if query.trim().is_empty() {
        spans.push(Span::styled(owned, base));
        return Line::from(spans);
    }
    let lower = owned.to_lowercase();
    let q = query.to_lowercase();
    let mut idx = 0;
    while let Some(pos) = lower[idx..].find(&q) {
        let start = idx + pos;
        if start > idx {
            spans.push(Span::styled(owned[idx..start].to_string(), base));
        }
        let end = start + q.len();
        spans.push(Span::styled(
            owned[start..end].to_string(),
            base.patch(
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ));
        idx = end;
    }
    if idx < owned.len() {
        spans.push(Span::styled(owned[idx..].to_string(), base));
    }
    Line::from(spans)
}

/// Format a timestamp as a short human-readable date for filter chips.
/// Shows "Nov 25" for same year, "Nov 25, 2023" for other years.
fn format_time_short(ms: i64) -> String {
    let now = Utc::now();
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|dt| {
            if dt.year() == now.year() {
                dt.format("%b %d").to_string() // "Nov 25"
            } else {
                dt.format("%b %d, %Y").to_string() // "Nov 25, 2023"
            }
        })
        .unwrap_or_else(|| "?".to_string())
}

/// Format time filter range as readable chip text.
fn format_time_chip(from: Option<i64>, to: Option<i64>) -> String {
    match (from, to) {
        (Some(f), Some(t)) => format!(
            "[time: {} → {}]",
            format_time_short(f),
            format_time_short(t)
        ),
        (Some(f), None) => format!("[time: {} → now]", format_time_short(f)),
        (None, Some(t)) => format!("[time: start → {}]", format_time_short(t)),
        (None, None) => String::new(),
    }
}

/// Parse human-readable time input into milliseconds since epoch.
///
/// Accepts multiple formats:
/// - Relative: `-7d` (7 days ago), `-24h` (24 hours), `-1w` (1 week), `-30m` (30 minutes)
/// - Keywords: `yesterday`, `today`, `now`
/// - ISO dates: `2024-11-25`, `2024-11-25T14:30:00`
/// - Numeric: milliseconds if >= 10^12, otherwise seconds
///
/// Returns None if the input cannot be parsed.
fn parse_time_input(input: &str) -> Option<i64> {
    let input = input.trim().to_lowercase();
    if input.is_empty() {
        return None;
    }

    let now = Utc::now();
    let now_ms = now.timestamp_millis();

    // Relative time formats: -7d, -24h, -1w, -30m
    if let Some(rest) = input.strip_prefix('-') {
        if let Some((num_str, unit)) = rest
            .char_indices()
            .find(|(_, c)| c.is_alphabetic())
            .map(|(i, _)| (&rest[..i], &rest[i..]))
            && let Ok(num) = num_str.parse::<i64>()
        {
            let ms_per_unit = match unit {
                "m" | "min" | "mins" | "minute" | "minutes" => 60 * 1000,
                "h" | "hr" | "hrs" | "hour" | "hours" => 60 * 60 * 1000,
                "d" | "day" | "days" => 24 * 60 * 60 * 1000,
                "w" | "wk" | "wks" | "week" | "weeks" => 7 * 24 * 60 * 60 * 1000,
                _ => return None,
            };
            return Some(now_ms - num * ms_per_unit);
        }
        return None;
    }

    // Keyword shortcuts
    match input.as_str() {
        "now" => return Some(now_ms),
        "today" => {
            let start_of_today = now.date_naive().and_hms_opt(0, 0, 0)?;
            return Some(start_of_today.and_utc().timestamp_millis());
        }
        "yesterday" => {
            let yesterday = now.date_naive().pred_opt()?.and_hms_opt(0, 0, 0)?;
            return Some(yesterday.and_utc().timestamp_millis());
        }
        _ => {}
    }

    // Try ISO date formats
    // Full ISO-8601 with time: 2024-11-25T14:30:00Z or 2024-11-25T14:30:00
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&input) {
        return Some(dt.timestamp_millis());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&input, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc().timestamp_millis());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&input, "%Y-%m-%d %H:%M:%S") {
        return Some(dt.and_utc().timestamp_millis());
    }

    // Date only: 2024-11-25 -> start of that day
    if let Ok(date) = chrono::NaiveDate::parse_from_str(&input, "%Y-%m-%d") {
        let dt = date.and_hms_opt(0, 0, 0)?;
        return Some(dt.and_utc().timestamp_millis());
    }

    // Short date formats: 11/25/2024 or 11-25-2024
    if let Ok(date) = chrono::NaiveDate::parse_from_str(&input, "%m/%d/%Y") {
        let dt = date.and_hms_opt(0, 0, 0)?;
        return Some(dt.and_utc().timestamp_millis());
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(&input, "%m-%d-%Y") {
        let dt = date.and_hms_opt(0, 0, 0)?;
        return Some(dt.and_utc().timestamp_millis());
    }

    // Numeric: try parsing as number
    if let Ok(n) = input.parse::<i64>() {
        // Heuristic: timestamps >= 10^12 are milliseconds, otherwise seconds
        // (10^12 ms = Sep 2001, reasonable cutoff)
        if n >= 1_000_000_000_000 {
            return Some(n); // Already milliseconds
        } else if n >= 1_000_000_000 {
            return Some(n * 1000); // Convert seconds to milliseconds
        }
        // Small numbers are probably not valid timestamps
        return None;
    }

    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusRegion {
    Results,
    Detail,
}

pub fn footer_legend(show_help: bool) -> &'static str {
    if show_help {
        "Esc quit • arrows nav • Tab focus • Enter view • F8 editor • F1-F9 commands • y copy"
    } else {
        "F1 help | Enter view | Esc quit"
    }
}

pub fn run_tui(data_dir_override: Option<std::path::PathBuf>, once: bool) -> Result<()> {
    if once
        && std::env::var("TUI_HEADLESS")
            .map(|v| v == "1")
            .unwrap_or(false)
    {
        return run_tui_headless(data_dir_override);
    }

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let data_dir = data_dir_override.unwrap_or_else(default_data_dir);
    let index_path = index_dir(&data_dir)?;
    let db_path = default_db_path_for(&data_dir);
    let state_path = state_path_for(&data_dir);
    let persisted = load_state(&state_path);
    let search_client = SearchClient::open(&index_path, Some(&db_path))?;
    let index_ready = search_client.is_some();
    let mut status = if index_ready {
        format!(
            "Index ready at {} - type to search (Esc/F10 quit, F1 help)",
            index_path.display()
        )
    } else {
        format!(
            "Index not present at {}. Run `cass index --full` then reopen TUI.",
            index_path.display()
        )
    };

    let mut query = String::new();
    let mut filters = SearchFilters::default();
    let mut input_mode = InputMode::Query;
    let mut input_buffer = String::new();
    let page_size: usize = 120;
    // Calculate initial pane limit based on terminal height
    let initial_height = terminal.size().map(|r| r.height).unwrap_or(24);
    let mut per_pane_limit: usize = calculate_pane_limit(initial_height);
    let mut last_terminal_height: u16 = initial_height;
    let mut page: usize = 0;
    let mut results: Vec<SearchHit> = Vec::new();
    let mut panes: Vec<AgentPane> = Vec::new();
    let mut active_pane: usize = 0;
    const MAX_VISIBLE_PANES: usize = 4;
    let mut pane_scroll_offset: usize = 0; // First visible pane index
    let mut focus_region = FocusRegion::Results;
    let mut detail_scroll: u16 = 0;
    let mut focus_flash_until: Option<Instant> = None;
    let mut last_tick = Instant::now();
    let tick_rate = Duration::from_millis(30);
    let debounce = Duration::from_millis(60);
    let mut dirty_since: Option<Instant> = Some(Instant::now());
    // Loading spinner state
    let mut spinner_frame: usize = 0;
    const SPINNER_CHARS: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

    let mut detail_tab = DetailTab::Messages;
    let mut theme_dark = true;
    // Show onboarding overlay only on first launch (when has_seen_help is not set).
    // After user dismisses with F1, we persist has_seen_help=true to avoid showing again.
    let mut show_help = !persisted.has_seen_help.unwrap_or(false);
    // Full-screen modal for viewing parsed content
    let mut show_detail_modal = false;
    let mut modal_scroll: u16 = 0;
    let mut cached_detail: Option<(String, ConversationView)> = None;
    let mut last_query = String::new();
    let mut needs_draw = true;
    // Load query history from persisted state, or start fresh
    let mut query_history: VecDeque<String> = persisted
        .query_history
        .map(VecDeque::from)
        .unwrap_or_default();
    let history_cap: usize = 50;
    let mut history_cursor: Option<usize> = None;
    let mut suggestion_idx: Option<usize> = None;
    let mut match_mode = match persisted.match_mode.as_deref() {
        Some("standard") => MatchMode::Standard,
        _ => MatchMode::Prefix,
    };
    let mut ranking_mode = RankingMode::Balanced;
    let mut context_window = match persisted.context_window.as_deref() {
        Some("S") => ContextWindow::Small,
        Some("M") => ContextWindow::Medium,
        Some("L") => ContextWindow::Large,
        Some("XL") => ContextWindow::XLarge,
        _ => ContextWindow::Medium,
    };
    let mut peek_window_saved: Option<ContextWindow> = None;
    let mut peek_badge_until: Option<Instant> = None;
    let mut help_scroll: u16 = 0;
    let editor_cmd = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let editor_line_flag = std::env::var("EDITOR_LINE_FLAG").unwrap_or_else(|_| "+".into());

    // Mouse support: track layout regions for click/scroll handling
    let mut last_detail_area: Option<Rect> = None;
    let mut last_pane_rects: Vec<Rect> = Vec::new();

    loop {
        // Check for terminal resize and recalculate pane limit if needed
        if let Ok(size) = terminal.size()
            && size.height != last_terminal_height
        {
            last_terminal_height = size.height;
            let new_limit = calculate_pane_limit(size.height);
            if new_limit != per_pane_limit {
                per_pane_limit = new_limit;
                panes = build_agent_panes(&results, per_pane_limit);
                needs_draw = true;
            }
        }

        if needs_draw {
            terminal.draw(|f| {
                let palette = if theme_dark {
                    ThemePalette::dark()
                } else {
                    ThemePalette::light()
                };

                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .margin(1)
                    .constraints(
                        [
                            Constraint::Length(3), // search bar (includes filter chips)
                            Constraint::Min(0),    // results + detail
                            Constraint::Length(1), // footer
                        ]
                        .as_ref(),
                    )
                    .split(f.area());

                let bar_text = match input_mode {
                    InputMode::Query => query.as_str().to_string(),
                    InputMode::Agent => format!("[agent] {}", input_buffer),
                    InputMode::Workspace => format!("[workspace] {}", input_buffer),
                    InputMode::CreatedFrom => format!("[from] {}", input_buffer),
                    InputMode::CreatedTo => format!("[to] {}", input_buffer),
                };
                let mode_label = match match_mode {
                    MatchMode::Standard => "standard",
                    MatchMode::Prefix => "prefix",
                };
                let chips = chips_for_filters(&filters, palette);
                let sb = search_bar(&bar_text, palette, input_mode, mode_label, chips);
                f.render_widget(sb, chunks[0]);

                // Responsive layout: detail pane expands when focused
                let (results_pct, detail_pct) = match focus_region {
                    FocusRegion::Results => (70, 30),
                    FocusRegion::Detail => (50, 50),
                };
                let main_split = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(
                        [
                            Constraint::Percentage(results_pct),
                            Constraint::Percentage(detail_pct),
                        ]
                        .as_ref(),
                    )
                    .split(chunks[1]);

                let results_area = main_split[0];
                let detail_area = main_split[1];

                // Save layout for mouse hit testing
                last_detail_area = Some(detail_area);

                if panes.is_empty() {
                    // Clear pane rects when no panes (avoid stale click detection)
                    last_pane_rects.clear();
                    let mut lines: Vec<Line> = Vec::new();
                    if query.trim().is_empty() && !query_history.is_empty() {
                        lines.push(Line::from(Span::styled(
                            "Recent queries (Enter to load):",
                            palette.title(),
                        )));
                        for (idx, q) in query_history.iter().take(5).enumerate() {
                            let selected = suggestion_idx == Some(idx);
                            lines.push(Line::from(Span::styled(
                                format!("{} {}", if selected { "▶" } else { " " }, q),
                                if selected {
                                    Style::default()
                                        .fg(palette.accent)
                                        .add_modifier(Modifier::BOLD)
                                } else {
                                    Style::default().fg(palette.hint)
                                },
                            )));
                        }
                    } else {
                        // Check for fuzzy suggestion from query history
                        let fuzzy = suggest_correction(&last_query, &query_history);
                        // Use contextual empty state with helpful suggestions
                        lines.extend(contextual_empty_state(
                            &last_query,
                            &filters,
                            match_mode,
                            palette,
                            fuzzy.as_deref(),
                        ));
                    }

                    let block = Block::default().title("Results").borders(Borders::ALL);
                    f.render_widget(Paragraph::new(lines).block(block), results_area);
                } else {
                    // Cap visible panes at MAX_VISIBLE_PANES
                    // Safety: clamp scroll offset to valid range to prevent slice panic
                    let safe_scroll_offset =
                        pane_scroll_offset.min(panes.len().saturating_sub(1).max(0));
                    let visible_end = (safe_scroll_offset + MAX_VISIBLE_PANES).min(panes.len());
                    let visible_panes: Vec<&AgentPane> =
                        panes[safe_scroll_offset..visible_end].iter().collect();
                    let hidden_count = panes.len().saturating_sub(MAX_VISIBLE_PANES);

                    let pane_width = (100 / std::cmp::max(visible_panes.len(), 1)) as u16;
                    let pane_constraints: Vec<Constraint> = visible_panes
                        .iter()
                        .map(|_| Constraint::Percentage(pane_width))
                        .collect();
                    let pane_chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints(pane_constraints)
                        .split(results_area);

                    // Save pane rects for mouse hit testing
                    last_pane_rects = pane_chunks.iter().copied().collect();

                    for (vis_idx, pane) in visible_panes.iter().enumerate() {
                        let idx = safe_scroll_offset + vis_idx;
                        let theme = ThemePalette::agent_pane(&pane.agent);
                        let mut state = ListState::default();
                        state.select(Some(pane.selected));

                        let items: Vec<ListItem> = pane
                            .hits
                            .iter()
                            .map(|hit| {
                                let title = if hit.title.is_empty() {
                                    "(untitled)"
                                } else {
                                    hit.title.as_str()
                                };
                                // Build header with score bar visualization
                                let mut header_spans = score_bar(hit.score, theme.accent);
                                header_spans.push(Span::raw(" "));
                                header_spans.push(Span::styled(
                                    title.to_string(),
                                    Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                                ));
                                let header = Line::from(header_spans);
                                // Truncate paths for display (max 40 chars each)
                                let truncated_source = truncate_path(&hit.source_path, 40);
                                let location = if hit.workspace.is_empty() {
                                    truncated_source
                                } else {
                                    let truncated_ws = truncate_path(&hit.workspace, 30);
                                    format!("{} ({})", truncated_source, truncated_ws)
                                };
                                let raw_snippet =
                                    contextual_snippet(&hit.content, &last_query, context_window);
                                let body_line = highlight_terms_owned_with_style(
                                    format!("{location} • {raw_snippet}"),
                                    &last_query,
                                    palette,
                                    Style::default().fg(theme.fg),
                                );
                                ListItem::new(vec![header, body_line])
                            })
                            .collect();

                        const FLASH_DURATION_MS: u64 = 220;

                        // Calculate smooth flash progress (0.0 = start/accent, 1.0 = end/normal)
                        let flash_progress_value = if idx == active_pane {
                            flash_progress(focus_flash_until, FLASH_DURATION_MS)
                        } else {
                            1.0 // No flash for non-active panes
                        };

                        // Interpolate colors: accent → bg for background, bg → fg for foreground
                        let flash_bg = lerp_color(theme.accent, theme.bg, flash_progress_value);
                        let flash_fg = lerp_color(theme.bg, theme.fg, flash_progress_value);

                        let is_focused_pane = match focus_region {
                            FocusRegion::Results => idx == active_pane,
                            FocusRegion::Detail => false,
                        };

                        // Show "X/Y" when there are more results than displayed
                        let count_display = if pane.total_count > pane.hits.len() {
                            format!("{}/{}", pane.hits.len(), pane.total_count)
                        } else {
                            pane.hits.len().to_string()
                        };
                        let block = Block::default()
                            .title(Span::styled(
                                format!("{} ({})", agent_display_name(&pane.agent), count_display),
                                Style::default().fg(theme.accent).add_modifier(
                                    if is_focused_pane {
                                        Modifier::BOLD
                                    } else {
                                        Modifier::empty()
                                    },
                                ),
                            ))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(if is_focused_pane {
                                theme.accent
                            } else {
                                palette.hint
                            }))
                            .style(Style::default().bg(flash_bg).fg(flash_fg));

                        let list = List::new(items)
                            .block(block)
                            .highlight_style(
                                Style::default()
                                    .bg(if is_focused_pane {
                                        theme.accent
                                    } else {
                                        palette.hint
                                    })
                                    .fg(theme.bg)
                                    .add_modifier(Modifier::BOLD),
                            )
                            .style(Style::default().bg(theme.bg).fg(theme.fg));

                        if let Some(area) = pane_chunks.get(vis_idx) {
                            f.render_stateful_widget(list, *area, &mut state);
                        }
                    }

                    // Show "+N more" indicator if there are hidden panes
                    if hidden_count > 0 {
                        let indicator =
                            format!(" [{} of {} agents] ", visible_panes.len(), panes.len());
                        let indicator_span = Span::styled(
                            indicator,
                            Style::default()
                                .fg(palette.hint)
                                .add_modifier(Modifier::DIM),
                        );
                        // Render in bottom-right corner of results area
                        let indicator_area = Rect::new(
                            results_area.x
                                + results_area
                                    .width
                                    .saturating_sub(indicator_span.content.len() as u16 + 2),
                            results_area.y + results_area.height.saturating_sub(1),
                            indicator_span.content.len() as u16 + 2,
                            1,
                        );
                        f.render_widget(Paragraph::new(indicator_span), indicator_area);
                    }
                }

                if let Some(hit) = active_hit(&panes, active_pane) {
                    let tabs = ["Messages", "Snippets", "Raw"];
                    let tab_titles: Vec<Line> = tabs
                        .iter()
                        .map(|t| Line::from(Span::styled(*t, Style::default().fg(palette.hint))))
                        .collect();
                    let tab_widget = Tabs::new(tab_titles)
                        .select(match detail_tab {
                            DetailTab::Messages => 0,
                            DetailTab::Snippets => 1,
                            DetailTab::Raw => 2,
                        })
                        .highlight_style(
                            Style::default()
                                .fg(palette.accent)
                                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                        )
                        .divider(" │ ");

                    let mut meta_lines = Vec::new();
                    let agent_theme = ThemePalette::agent_pane(&hit.agent);
                    meta_lines.push(Line::from(vec![
                        Span::styled("Title: ", palette.title()),
                        Span::raw(hit.title.clone()),
                    ]));
                    meta_lines.push(Line::from(vec![
                        Span::styled("Agent: ", Style::default().fg(agent_theme.accent)),
                        Span::styled(
                            agent_display_name(&hit.agent),
                            Style::default().fg(agent_theme.fg),
                        ),
                    ]));
                    meta_lines.push(Line::from(vec![
                        Span::styled("Workspace: ", Style::default().fg(palette.hint)),
                        Span::raw(if hit.workspace.is_empty() {
                            "(none)".into()
                        } else {
                            truncate_path(&hit.workspace, 60)
                        }),
                    ]));
                    meta_lines.push(Line::from(vec![
                        Span::styled("Source: ", Style::default().fg(palette.hint)),
                        Span::raw(truncate_path(&hit.source_path, 60)),
                    ]));
                    meta_lines.push(Line::from(format!("Score: {:.2}", hit.score)));
                    meta_lines.push(Line::from(""));

                    let detail = if cached_detail
                        .as_ref()
                        .map(|(p, _)| p == &hit.source_path)
                        .unwrap_or(false)
                    {
                        cached_detail.as_ref().map(|(_, d)| d.clone())
                    } else {
                        let loaded = load_conversation(&db_path, &hit.source_path).ok().flatten();
                        if let Some(d) = &loaded {
                            cached_detail = Some((hit.source_path.clone(), d.clone()));
                            // Reset scroll when loading new conversation
                            detail_scroll = 0;
                        }
                        loaded
                    };
                    let content_para = match detail_tab {
                        DetailTab::Messages => {
                            if let Some(full) = detail {
                                let mut lines = Vec::new();
                                for msg in full.messages {
                                    let role_label = match msg.role {
                                        MessageRole::User => "you",
                                        MessageRole::Agent => "agent",
                                        MessageRole::Tool => "tool",
                                        MessageRole::System => "system",
                                        MessageRole::Other(ref r) => r.as_str(),
                                    };
                                    let ts = msg
                                        .created_at
                                        .map(|t| format!(" ({})", format_relative_time(t)))
                                        .unwrap_or_default();
                                    lines.push(Line::from(vec![
                                        Span::styled(
                                            format!("[{role_label}]"),
                                            role_style(&msg.role, palette),
                                        ),
                                        Span::raw(ts),
                                    ]));
                                    let mut in_code = false;
                                    for line_text in msg.content.lines() {
                                        if line_text.trim_start().starts_with("```") {
                                            in_code = !in_code;
                                            lines.push(Line::from(Span::styled(
                                                line_text.to_string(),
                                                Style::default().fg(palette.hint),
                                            )));
                                            continue;
                                        }
                                        let base = if in_code {
                                            Style::default().bg(palette.surface)
                                        } else {
                                            Style::default()
                                        };
                                        let rendered = highlight_terms_owned_with_style(
                                            line_text.to_string(),
                                            &last_query,
                                            palette,
                                            base,
                                        );
                                        lines.push(rendered);
                                    }
                                    lines.push(Line::from(""));
                                }
                                if lines.is_empty() {
                                    Paragraph::new("No messages")
                                        .style(Style::default().fg(palette.hint))
                                } else {
                                    Paragraph::new(lines)
                                        .wrap(Wrap { trim: true })
                                        .scroll((detail_scroll, 0))
                                }
                            } else {
                                Paragraph::new(hit.content.clone())
                                    .wrap(Wrap { trim: true })
                                    .scroll((detail_scroll, 0))
                            }
                        }
                        DetailTab::Snippets => {
                            if let Some(full) = detail {
                                let mut lines = Vec::new();
                                for (msg_idx, msg) in full.messages.iter().enumerate() {
                                    for snip in &msg.snippets {
                                        let file = snip
                                            .file_path
                                            .as_ref()
                                            .map(|p| p.to_string_lossy().to_string())
                                            .unwrap_or_else(|| "<unknown file>".into());
                                        let range = match (snip.start_line, snip.end_line) {
                                            (Some(s), Some(e)) => format!("{s}-{e}"),
                                            (Some(s), None) => s.to_string(),
                                            _ => "-".into(),
                                        };
                                        lines.push(Line::from(vec![
                                            Span::styled(file, palette.title()),
                                            Span::raw(format!(":{range} ")),
                                            Span::styled(
                                                format!("msg#{msg_idx} "),
                                                role_style(&msg.role, palette),
                                            ),
                                        ]));
                                        if let Some(text) = &snip.snippet_text {
                                            for l in text.lines() {
                                                lines.push(Line::from(Span::raw(format!("  {l}"))));
                                            }
                                        }
                                        lines.push(Line::from(""));
                                    }
                                }
                                if lines.is_empty() {
                                    Paragraph::new("No snippets attached.")
                                        .style(Style::default().fg(palette.hint))
                                } else {
                                    Paragraph::new(lines)
                                        .wrap(Wrap { trim: true })
                                        .scroll((detail_scroll, 0))
                                }
                            } else {
                                Paragraph::new("No snippets loaded")
                                    .style(Style::default().fg(palette.hint))
                            }
                        }
                        DetailTab::Raw => {
                            if let Some(full) = detail {
                                let meta = serde_json::to_string_pretty(&full.convo.metadata_json)
                                    .unwrap_or_else(|_| "<invalid metadata>".into());
                                let mut text = String::new();
                                text.push_str(&format!(
                                    "Path: {}\n",
                                    full.convo.source_path.display()
                                ));
                                if let Some(ws) = &full.workspace {
                                    text.push_str(&format!("Workspace: {}\n", ws.path.display()));
                                }
                                if let Some(ext) = &full.convo.external_id {
                                    text.push_str(&format!("External ID: {ext}\n"));
                                }
                                text.push_str("Metadata:\n");
                                text.push_str(&meta);
                                Paragraph::new(text)
                                    .wrap(Wrap { trim: true })
                                    .scroll((detail_scroll, 0))
                            } else {
                                Paragraph::new(format!("Path: {}", hit.source_path))
                                    .wrap(Wrap { trim: true })
                                    .scroll((detail_scroll, 0))
                            }
                        }
                    };

                    let is_focused_detail = matches!(focus_region, FocusRegion::Detail);
                    // Build detail block title with scroll indicator and tab hints
                    let detail_title = if detail_scroll > 0 {
                        format!("Detail ↓{} • [ ] tabs", detail_scroll)
                    } else if is_focused_detail {
                        "Detail • [ ] tabs • j/k scroll".to_string()
                    } else {
                        "Detail • [ ] tabs".to_string()
                    };
                    let block = Block::default()
                        .title(Span::styled(
                            detail_title,
                            Style::default().fg(if is_focused_detail {
                                palette.accent
                            } else {
                                palette.hint
                            }),
                        ))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(if is_focused_detail {
                            palette.accent
                        } else {
                            palette.hint
                        }));

                    let layout = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints(
                            [
                                Constraint::Length(2),
                                Constraint::Length(meta_lines.len() as u16 + 2),
                                Constraint::Min(3),
                            ]
                            .as_ref(),
                        )
                        .split(detail_area);

                    f.render_widget(tab_widget, layout[0]);
                    f.render_widget(Paragraph::new(meta_lines), layout[1]);
                    f.render_widget(content_para.block(block), layout[2]);
                } else {
                    f.render_widget(
                        Paragraph::new("Select a result to view details")
                            .block(Block::default().title("Detail").borders(Borders::ALL)),
                        detail_area,
                    );
                }

                // Simplified footer: status + non-default modes + legend
                let mut footer_parts: Vec<String> = vec![];
                // Show spinner when search is pending
                if dirty_since.is_some() {
                    let spinner = SPINNER_CHARS[spinner_frame % SPINNER_CHARS.len()];
                    footer_parts.push(format!("{} Searching...", spinner));
                } else if !status.is_empty() {
                    footer_parts.push(status.clone());
                }
                // Only show mode if non-default (default is Prefix)
                if matches!(match_mode, MatchMode::Standard) {
                    footer_parts.push("mode:standard".to_string());
                }
                // Only show ranking if non-default (default is Balanced)
                match ranking_mode {
                    RankingMode::RecentHeavy => footer_parts.push("rank:recent".to_string()),
                    RankingMode::RelevanceHeavy => footer_parts.push("rank:relevance".to_string()),
                    RankingMode::Balanced => {}
                }
                // Only show context if non-default (default is Medium)
                if !matches!(context_window, ContextWindow::Medium) {
                    footer_parts.push(
                        match context_window {
                            ContextWindow::Small => "ctx:S",
                            ContextWindow::Medium => "ctx:M",
                            ContextWindow::Large => "ctx:L",
                            ContextWindow::XLarge => "ctx:XL",
                        }
                        .to_string(),
                    );
                }
                footer_parts.push(footer_legend(show_help).to_string());
                if peek_badge_until
                    .map(|t| t > Instant::now())
                    .unwrap_or(false)
                {
                    footer_parts.push("PEEK".to_string());
                }
                let footer_line = footer_parts.join(" | ");
                let footer = Paragraph::new(footer_line);
                f.render_widget(footer, chunks[2]);

                if show_help {
                    render_help_overlay(f, palette, help_scroll);
                }

                // Detail modal takes priority over help
                if show_detail_modal
                    && let Some((_, ref detail)) = cached_detail
                    && let Some(pane) = panes.get(active_pane)
                    && let Some(hit) = pane.hits.get(pane.selected)
                {
                    render_detail_modal(f, detail, hit, &last_query, palette, modal_scroll);
                }
            })?;
            needs_draw = false;
        }

        let timeout = if needs_draw {
            Duration::from_millis(0)
        } else {
            tick_rate
                .checked_sub(last_tick.elapsed())
                .unwrap_or_else(|| Duration::from_millis(0))
        };

        if crossterm::event::poll(timeout)? {
            let event = event::read()?;

            // Handle mouse events (skip when modal is open)
            if let Event::Mouse(mouse) = event {
                // Ignore mouse events when help or detail modal is open
                if show_help || show_detail_modal {
                    continue;
                }
                needs_draw = true;
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        let col = mouse.column;
                        let row = mouse.row;

                        // Check if click is in detail area
                        if let Some(detail_rect) = last_detail_area
                            && col >= detail_rect.x
                            && col < detail_rect.x + detail_rect.width
                            && row >= detail_rect.y
                            && row < detail_rect.y + detail_rect.height
                        {
                            focus_region = FocusRegion::Detail;
                            focus_flash_until = Some(Instant::now() + Duration::from_millis(220));
                            status = "Focus: Detail (click)".to_string();
                            continue;
                        }

                        // Check if click is in a pane
                        for (vis_idx, pane_rect) in last_pane_rects.iter().enumerate() {
                            if col >= pane_rect.x
                                && col < pane_rect.x + pane_rect.width
                                && row >= pane_rect.y
                                && row < pane_rect.y + pane_rect.height
                            {
                                // Calculate which pane in the full list
                                let pane_idx = pane_scroll_offset + vis_idx;
                                if pane_idx < panes.len() {
                                    // Switch to this pane
                                    if active_pane != pane_idx {
                                        active_pane = pane_idx;
                                        focus_flash_until =
                                            Some(Instant::now() + Duration::from_millis(220));
                                    }
                                    focus_region = FocusRegion::Results;

                                    // Calculate which item was clicked (2 lines per item + 1 for border)
                                    let relative_row = row.saturating_sub(pane_rect.y + 1);
                                    let item_idx = (relative_row / 2) as usize;
                                    if let Some(pane) = panes.get_mut(pane_idx)
                                        && item_idx < pane.hits.len()
                                    {
                                        pane.selected = item_idx;
                                        cached_detail = None;
                                        detail_scroll = 0;
                                    }
                                }
                                break;
                            }
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        // Scroll up in detail or results depending on focus
                        match focus_region {
                            FocusRegion::Detail => {
                                detail_scroll = detail_scroll.saturating_sub(3);
                            }
                            FocusRegion::Results => {
                                if let Some(pane) = panes.get_mut(active_pane)
                                    && pane.selected > 0
                                {
                                    pane.selected = pane.selected.saturating_sub(1);
                                    cached_detail = None;
                                    detail_scroll = 0;
                                }
                            }
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        // Scroll down in detail or results depending on focus
                        match focus_region {
                            FocusRegion::Detail => {
                                detail_scroll = detail_scroll.saturating_add(3);
                            }
                            FocusRegion::Results => {
                                if let Some(pane) = panes.get_mut(active_pane)
                                    && pane.selected + 1 < pane.hits.len()
                                {
                                    pane.selected += 1;
                                    cached_detail = None;
                                    detail_scroll = 0;
                                }
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }

            // Handle key events
            let Event::Key(key) = event else {
                continue;
            };

            needs_draw = true;

            // Global quit override
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                break;
            }

            // While help is open, keys scroll the help modal and do not affect panes.
            if show_help {
                match key.code {
                    KeyCode::Esc | KeyCode::F(1) => {
                        show_help = false;
                        help_scroll = 0;
                    }
                    KeyCode::Up => {
                        help_scroll = help_scroll.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        help_scroll = help_scroll.saturating_add(1);
                    }
                    KeyCode::PageUp => {
                        help_scroll = help_scroll.saturating_sub(5);
                    }
                    KeyCode::PageDown => {
                        help_scroll = help_scroll.saturating_add(5);
                    }
                    KeyCode::Home => help_scroll = 0,
                    KeyCode::End => help_scroll = help_lines(ThemePalette::dark()).len() as u16,
                    _ => {}
                }
                continue;
            }

            // While detail modal is open, handle its keyboard shortcuts
            if show_detail_modal {
                match key.code {
                    KeyCode::Esc => {
                        show_detail_modal = false;
                        modal_scroll = 0;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        modal_scroll = modal_scroll.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        modal_scroll = modal_scroll.saturating_add(1);
                    }
                    KeyCode::PageUp => {
                        modal_scroll = modal_scroll.saturating_sub(20);
                    }
                    KeyCode::PageDown => {
                        modal_scroll = modal_scroll.saturating_add(20);
                    }
                    KeyCode::Home | KeyCode::Char('g') => modal_scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => modal_scroll = u16::MAX,
                    KeyCode::Char('c') => {
                        // Copy rendered content to clipboard using xclip/xsel/pbcopy
                        if let Some((_, ref detail)) = cached_detail {
                            let mut text = String::new();
                            for msg in &detail.messages {
                                let role_label = match &msg.role {
                                    MessageRole::User => "YOU",
                                    MessageRole::Agent => "ASSISTANT",
                                    MessageRole::Tool => "TOOL",
                                    MessageRole::System => "SYSTEM",
                                    MessageRole::Other(r) => r,
                                };
                                text.push_str(&format!("=== {} ===\n", role_label));
                                text.push_str(&msg.content);
                                text.push_str("\n\n");
                            }
                            // Try clipboard tools in order of preference
                            let clipboard_cmd = if cfg!(target_os = "macos") {
                                Some("pbcopy")
                            } else {
                                // Linux: prefer xclip, fallback to xsel
                                if StdCommand::new("which")
                                    .arg("xclip")
                                    .output()
                                    .map(|o| o.status.success())
                                    .unwrap_or(false)
                                {
                                    Some("xclip -selection clipboard")
                                } else if StdCommand::new("which")
                                    .arg("xsel")
                                    .output()
                                    .map(|o| o.status.success())
                                    .unwrap_or(false)
                                {
                                    Some("xsel --clipboard --input")
                                } else {
                                    None
                                }
                            };

                            status = if let Some(cmd) = clipboard_cmd {
                                let result = StdCommand::new("sh")
                                    .arg("-c")
                                    .arg(cmd)
                                    .stdin(std::process::Stdio::piped())
                                    .spawn()
                                    .and_then(|mut child| {
                                        use std::io::Write;
                                        if let Some(stdin) = child.stdin.as_mut() {
                                            stdin.write_all(text.as_bytes())?;
                                        }
                                        child.wait()
                                    });
                                if result.map(|s| s.success()).unwrap_or(false) {
                                    "✓ Copied to clipboard".to_string()
                                } else {
                                    "✗ Clipboard copy failed".to_string()
                                }
                            } else {
                                "✗ No clipboard tool found (xclip/xsel/pbcopy)".to_string()
                            };
                        }
                    }
                    KeyCode::Char('n') => {
                        // Open content in nano via temp file
                        if let Some((_, ref detail)) = cached_detail {
                            let mut text = String::new();
                            for msg in &detail.messages {
                                let role_label = match &msg.role {
                                    MessageRole::User => "YOU",
                                    MessageRole::Agent => "ASSISTANT",
                                    MessageRole::Tool => "TOOL",
                                    MessageRole::System => "SYSTEM",
                                    MessageRole::Other(r) => r,
                                };
                                text.push_str(&format!("=== {} ===\n", role_label));
                                text.push_str(&msg.content);
                                text.push_str("\n\n");
                            }
                            // Create temp file
                            let tmp_path = std::env::temp_dir().join(format!(
                                "cass_view_{}.md",
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs())
                                    .unwrap_or(0)
                            ));
                            if std::fs::write(&tmp_path, &text).is_ok() {
                                // Exit raw mode, run nano, re-enter
                                disable_raw_mode().ok();
                                execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)
                                    .ok();
                                let nano_result = StdCommand::new("nano")
                                    .arg("--view") // Read-only mode
                                    .arg(&tmp_path)
                                    .status();
                                execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)
                                    .ok();
                                enable_raw_mode().ok();
                                // Clean up temp file
                                std::fs::remove_file(&tmp_path).ok();
                                status = if nano_result.is_ok() {
                                    "Returned from nano".to_string()
                                } else {
                                    "✗ Failed to launch nano".to_string()
                                };
                                show_detail_modal = false;
                                modal_scroll = 0;
                            } else {
                                status = "✗ Failed to create temp file".to_string();
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }
            match input_mode {
                InputMode::Query => {
                    if key.modifiers.contains(KeyModifiers::CONTROL) {
                        // Handle both 'r' and 'R' since Shift modifier may change the char
                        if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) {
                            // Ctrl+Shift+R = refresh search (re-query index)
                            if key.modifiers.contains(KeyModifiers::SHIFT) {
                                status = "Refreshing search...".to_string();
                                page = 0;
                                dirty_since = Some(Instant::now());
                                cached_detail = None;
                                detail_scroll = 0;
                            } else if query_history.is_empty() {
                                // Ctrl+R = cycle history
                                status = "No query history yet".to_string();
                            } else {
                                let next = history_cursor
                                    .map(|idx| (idx + 1) % query_history.len())
                                    .unwrap_or(0);
                                if let Some(saved) = query_history.get(next) {
                                    history_cursor = Some(next);
                                    query = saved.clone();
                                    page = 0;
                                    dirty_since = Some(Instant::now());
                                    status = format!("Loaded query #{next} from history");
                                    cached_detail = None;
                                    detail_scroll = 0;
                                }
                            }
                        }
                        continue;
                    }

                    match key.code {
                        KeyCode::Esc | KeyCode::F(10) => {
                            // If in Detail, Esc goes back to Results.
                            if matches!(focus_region, FocusRegion::Detail) {
                                focus_region = FocusRegion::Results;
                                status = "Focus: Results".to_string();
                            } else {
                                break;
                            }
                        }
                        KeyCode::Down => {
                            match focus_region {
                                FocusRegion::Results => {
                                    if panes.is_empty()
                                        && query.trim().is_empty()
                                        && !query_history.is_empty()
                                    {
                                        let max_idx = query_history.len().min(5).saturating_sub(1);
                                        let next = suggestion_idx.unwrap_or(0).saturating_add(1);
                                        suggestion_idx = Some(std::cmp::min(next, max_idx));
                                        status = "Enter to load selected recent query".to_string();
                                    } else if let Some(pane) = panes.get_mut(active_pane)
                                        && pane.selected + 1 < pane.hits.len()
                                    {
                                        pane.selected += 1;
                                        // Re-load details for new selection
                                        cached_detail = None;
                                        detail_scroll = 0;
                                    }
                                }
                                FocusRegion::Detail => {
                                    detail_scroll = detail_scroll.saturating_add(1);
                                }
                            }
                        }
                        KeyCode::Up => {
                            match focus_region {
                                FocusRegion::Results => {
                                    if panes.is_empty()
                                        && query.trim().is_empty()
                                        && !query_history.is_empty()
                                    {
                                        let next = suggestion_idx.unwrap_or(0).saturating_sub(1);
                                        suggestion_idx = Some(next);
                                        status = "Enter to load selected recent query".to_string();
                                    } else if let Some(pane) = panes.get_mut(active_pane)
                                        && pane.selected > 0
                                    {
                                        pane.selected -= 1;
                                        // Re-load details for new selection
                                        cached_detail = None;
                                        detail_scroll = 0;
                                    }
                                }
                                FocusRegion::Detail => {
                                    detail_scroll = detail_scroll.saturating_sub(1);
                                }
                            }
                        }
                        KeyCode::Left => match focus_region {
                            FocusRegion::Results => {
                                active_pane = active_pane.saturating_sub(1);
                                // Scroll pane view if active moves before visible range
                                if active_pane < pane_scroll_offset {
                                    pane_scroll_offset = active_pane;
                                }
                                focus_flash_until =
                                    Some(Instant::now() + Duration::from_millis(220));
                                cached_detail = None;
                                detail_scroll = 0;
                            }
                            FocusRegion::Detail => {
                                focus_region = FocusRegion::Results;
                                status = "Focus: Results".to_string();
                            }
                        },
                        KeyCode::Right => {
                            match focus_region {
                                FocusRegion::Results => {
                                    if active_pane + 1 < panes.len() {
                                        active_pane += 1;
                                        // Scroll pane view if active moves past visible range
                                        if active_pane >= pane_scroll_offset + MAX_VISIBLE_PANES {
                                            pane_scroll_offset =
                                                active_pane.saturating_sub(MAX_VISIBLE_PANES - 1);
                                        }
                                        focus_flash_until =
                                            Some(Instant::now() + Duration::from_millis(220));
                                        cached_detail = None;
                                        detail_scroll = 0;
                                    } else if !panes.is_empty() {
                                        // At last pane, switch focus to detail
                                        focus_region = FocusRegion::Detail;
                                        status =
                                            "Focus: Detail (arrows scroll, Left back)".to_string();
                                    }
                                }
                                FocusRegion::Detail => {
                                    // Already at rightmost
                                }
                            }
                        }
                        KeyCode::PageDown => match focus_region {
                            FocusRegion::Results => {
                                page = page.saturating_add(1);
                                dirty_since = Some(Instant::now());
                            }
                            FocusRegion::Detail => {
                                detail_scroll = detail_scroll.saturating_add(20);
                            }
                        },
                        KeyCode::PageUp => match focus_region {
                            FocusRegion::Results => {
                                page = page.saturating_sub(1);
                                dirty_since = Some(Instant::now());
                            }
                            FocusRegion::Detail => {
                                detail_scroll = detail_scroll.saturating_sub(20);
                            }
                        },
                        KeyCode::Char('y') => {
                            if let Some(hit) = active_hit(&panes, active_pane) {
                                let text_to_copy = if matches!(focus_region, FocusRegion::Detail) {
                                    if let Some((_, _)) = &cached_detail {
                                        hit.content.clone()
                                    } else {
                                        hit.content.clone()
                                    }
                                } else {
                                    hit.source_path.clone()
                                };

                                #[cfg(any(target_os = "linux", target_os = "macos"))]
                                {
                                    use std::process::Stdio;
                                    let child = std::process::Command::new("sh")
                                        .arg("-c")
                                        .arg("if command -v wl-copy >/dev/null; then wl-copy; elif command -v pbcopy >/dev/null; then pbcopy; elif command -v xclip >/dev/null; then xclip -selection clipboard; fi")
                                        .stdin(Stdio::piped())
                                        .spawn();
                                    if let Ok(mut child) = child
                                        && let Some(mut stdin) = child.stdin.take()
                                    {
                                        use std::io::Write;
                                        let _ = stdin.write_all(text_to_copy.as_bytes());
                                        drop(stdin); // Ensure EOF
                                        let _ = child.wait();
                                        status = "Copied to clipboard".to_string();
                                    } else {
                                        status =
                                            "Clipboard copy failed (missing tool?)".to_string();
                                    }
                                }
                                #[cfg(target_os = "windows")]
                                {
                                    let child = std::process::Command::new("powershell")
                                        .arg("-command")
                                        .arg("$Input | Set-Clipboard")
                                        .stdin(std::process::Stdio::piped())
                                        .spawn();
                                    if let Ok(mut child) = child
                                        && let Some(mut stdin) = child.stdin.take()
                                    {
                                        use std::io::Write;
                                        let _ = stdin.write_all(text_to_copy.as_bytes());
                                        drop(stdin);
                                        let _ = child.wait();
                                        status = "Copied to clipboard".to_string();
                                    }
                                }
                            }
                        }
                        KeyCode::F(1) => {
                            show_help = !show_help;
                            help_scroll = 0;
                        }
                        KeyCode::F(2) => {
                            theme_dark = !theme_dark;
                            status = format!(
                                "Theme: {}, mode: {}",
                                if theme_dark { "dark" } else { "light" },
                                match match_mode {
                                    MatchMode::Standard => "standard",
                                    MatchMode::Prefix => "prefix",
                                }
                            );
                        }
                        KeyCode::F(3) if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            if let Some(hit) = active_hit(&panes, active_pane) {
                                filters.agents.clear();
                                filters.agents.insert(hit.agent.clone());
                                status = format!("Scoped to agent {}", hit.agent);
                                page = 0;
                                dirty_since = Some(Instant::now());
                                focus_region = FocusRegion::Results;
                                cached_detail = None;
                                detail_scroll = 0;
                            }
                        }
                        KeyCode::F(3) => {
                            input_mode = InputMode::Agent;
                            input_buffer.clear();
                            status = format!(
                                "Agents: {} (type to filter, Tab=complete, Enter=apply)",
                                KNOWN_AGENTS.join(", ")
                            );
                        }
                        KeyCode::F(4) if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            filters.agents.clear();
                            status = "Scope: all agents".to_string();
                            page = 0;
                            dirty_since = Some(Instant::now());
                            focus_region = FocusRegion::Results;
                            cached_detail = None;
                            detail_scroll = 0;
                        }
                        KeyCode::F(4) => {
                            input_mode = InputMode::Workspace;
                            input_buffer.clear();
                            status =
                                "Workspace filter: type path fragment, Enter=apply, Esc=cancel"
                                    .to_string();
                        }
                        KeyCode::F(5) if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            let now = chrono::Utc::now().timestamp_millis();
                            // Presets with their labels: (timestamp, label)
                            let presets: [(Option<i64>, &str); 4] = [
                                (Some(now - 86_400_000), "last 24h"),
                                (Some(now - 604_800_000), "last 7 days"),
                                (Some(now - 2_592_000_000), "last 30 days"),
                                (None, "all time"),
                            ];
                            let current = filters.created_from;
                            // Find which preset roughly matches current (within 1 minute tolerance)
                            let idx = presets
                                .iter()
                                .position(|(p, _)| match (p, current) {
                                    (Some(a), Some(b)) => (a - b).abs() < 60_000,
                                    (None, None) => true,
                                    _ => false,
                                })
                                .unwrap_or(presets.len() - 1);
                            let next_idx = (idx + 1) % presets.len();
                            let (next_ts, next_label) = presets[next_idx];
                            filters.created_from = next_ts;
                            filters.created_to = None;
                            page = 0;
                            status = format!("Time filter: {}", next_label);
                            dirty_since = Some(Instant::now());
                            focus_region = FocusRegion::Results;
                            cached_detail = None;
                            detail_scroll = 0;
                        }
                        KeyCode::F(5) => {
                            input_mode = InputMode::CreatedFrom;
                            input_buffer.clear();
                            status = "From: -7d, yesterday, 2024-11-25 | Enter=apply, Esc=cancel"
                                .to_string();
                        }
                        KeyCode::F(6) => {
                            input_mode = InputMode::CreatedTo;
                            input_buffer.clear();
                            status =
                                "To: -7d, yesterday, 2024-11-25, now | Enter=apply, Esc=cancel"
                                    .to_string();
                        }
                        KeyCode::F(7) => {
                            context_window = context_window.next();
                            status = format!(
                                "Context window: {} ({} chars)",
                                context_window.label(),
                                context_window.size()
                            );
                            dirty_since = Some(Instant::now());
                        }
                        KeyCode::F(12) => {
                            ranking_mode = match ranking_mode {
                                RankingMode::RecentHeavy => RankingMode::Balanced,
                                RankingMode::Balanced => RankingMode::RelevanceHeavy,
                                RankingMode::RelevanceHeavy => RankingMode::RecentHeavy,
                            };
                            status = format!(
                                "Ranking: {}",
                                match ranking_mode {
                                    RankingMode::RecentHeavy => "recent-heavy",
                                    RankingMode::Balanced => "balanced",
                                    RankingMode::RelevanceHeavy => "relevance-heavy",
                                }
                            );
                            dirty_since = Some(Instant::now());
                        }
                        KeyCode::Delete if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            filters = SearchFilters::default();
                            page = 0;
                            status = format!(
                                "Filters cleared | mode: {}",
                                match match_mode {
                                    MatchMode::Standard => "standard",
                                    MatchMode::Prefix => "prefix",
                                }
                            );
                            dirty_since = Some(Instant::now());
                            focus_region = FocusRegion::Results;
                            cached_detail = None;
                            detail_scroll = 0;
                        }
                        KeyCode::F(8) => {
                            if let Some(hit) = active_hit(&panes, active_pane) {
                                let path = &hit.source_path;
                                // Prefer line_number field, fallback to parsing snippet
                                let line_hint = hit.line_number.or_else(|| {
                                    hit.snippet
                                        .find("line ")
                                        .and_then(|i| {
                                            hit.snippet[i + 5..].split_whitespace().next()
                                        })
                                        .and_then(|s| s.parse::<usize>().ok())
                                });
                                let mut cmd = StdCommand::new(&editor_cmd);
                                if let Some(line) = line_hint {
                                    cmd.arg(format!("{}{}", editor_line_flag, line));
                                }
                                let _ = cmd.arg(path).status();
                            }
                        }
                        KeyCode::F(9) => {
                            match_mode = match match_mode {
                                MatchMode::Standard => MatchMode::Prefix,
                                MatchMode::Prefix => MatchMode::Standard,
                            };
                            status = format!(
                                "Match mode: {}",
                                match match_mode {
                                    MatchMode::Standard => "standard",
                                    MatchMode::Prefix => "prefix",
                                }
                            );
                            dirty_since = Some(Instant::now());
                        }
                        KeyCode::Tab => {
                            // Toggle focus
                            focus_region = match focus_region {
                                FocusRegion::Results => FocusRegion::Detail,
                                FocusRegion::Detail => FocusRegion::Results,
                            };
                            status = match focus_region {
                                FocusRegion::Results => "Focus: Results".to_string(),
                                FocusRegion::Detail => "Focus: Detail".to_string(),
                            };
                        }
                        KeyCode::Char(']') => {
                            detail_tab = match detail_tab {
                                DetailTab::Messages => DetailTab::Snippets,
                                DetailTab::Snippets => DetailTab::Raw,
                                DetailTab::Raw => DetailTab::Messages,
                            };
                            detail_scroll = 0;
                        }
                        KeyCode::Char('[') => {
                            detail_tab = match detail_tab {
                                DetailTab::Messages => DetailTab::Raw,
                                DetailTab::Snippets => DetailTab::Messages,
                                DetailTab::Raw => DetailTab::Snippets,
                            };
                            detail_scroll = 0;
                        }
                        KeyCode::Char(c) => {
                            // Reset focus to results if typing
                            if matches!(focus_region, FocusRegion::Detail) {
                                focus_region = FocusRegion::Results;
                            }

                            if key.modifiers.contains(KeyModifiers::ALT) {
                                if ('1'..='9').contains(&c) {
                                    let target = c.to_digit(10).unwrap_or(1) as usize - 1;
                                    if target < panes.len() {
                                        active_pane = target;
                                        focus_flash_until =
                                            Some(Instant::now() + Duration::from_millis(220));
                                        cached_detail = None;
                                        detail_scroll = 0;
                                    }
                                }
                                continue;
                            }
                            if key.modifiers.contains(KeyModifiers::SHIFT) && matches!(c, '+' | '=')
                            {
                                per_pane_limit = (per_pane_limit + 2).min(50);
                                status = format!("Pane size: {} items", per_pane_limit);
                                panes = build_agent_panes(&results, per_pane_limit);
                                dirty_since = Some(Instant::now());
                                continue;
                            }
                            // Only resize panes with `-` when there are actual panes showing
                            // Otherwise, allow `-` to be typed in the search query
                            if key.modifiers.is_empty() && c == '-' && !panes.is_empty() {
                                per_pane_limit = per_pane_limit.saturating_sub(2).max(4);
                                status = format!("Pane size: {} items", per_pane_limit);
                                panes = build_agent_panes(&results, per_pane_limit);
                                dirty_since = Some(Instant::now());
                                continue;
                            }
                            if key.modifiers.is_empty()
                                && c == ' '
                                && !panes.is_empty()
                                && active_hit(&panes, active_pane).is_some()
                            {
                                // Space acts as a momentary zoom: swap to XL context, tap again to restore.
                                if let Some(saved) = peek_window_saved.take() {
                                    context_window = saved;
                                    status = format!(
                                        "Context window: {} ({} chars)",
                                        context_window.label(),
                                        context_window.size()
                                    );
                                    peek_badge_until = None;
                                } else {
                                    peek_window_saved = Some(context_window);
                                    context_window = ContextWindow::XLarge;
                                    status = "Peek: XL context (Space to toggle back)".to_string();
                                    peek_badge_until =
                                        Some(Instant::now() + Duration::from_millis(600));
                                }
                                dirty_since = Some(Instant::now());
                                continue;
                            }
                            // Vim-style: g/G jump to first/last only when panes are showing
                            if c == 'g' && !panes.is_empty() {
                                if let Some(pane) = panes.get_mut(active_pane) {
                                    pane.selected = 0;
                                    cached_detail = None;
                                    detail_scroll = 0;
                                }
                                continue;
                            }
                            if c == 'G' && !panes.is_empty() {
                                if let Some(pane) = panes.get_mut(active_pane)
                                    && !pane.hits.is_empty()
                                {
                                    pane.selected = pane.hits.len() - 1;
                                    cached_detail = None;
                                    detail_scroll = 0;
                                }
                                continue;
                            }
                            // Vim-style navigation: j/k/h/l only when panes are showing
                            if c == 'j' && !panes.is_empty() {
                                match focus_region {
                                    FocusRegion::Results => {
                                        if let Some(pane) = panes.get_mut(active_pane)
                                            && pane.selected + 1 < pane.hits.len()
                                        {
                                            pane.selected += 1;
                                            cached_detail = None;
                                            detail_scroll = 0;
                                        }
                                    }
                                    FocusRegion::Detail => {
                                        detail_scroll = detail_scroll.saturating_add(1);
                                    }
                                }
                                continue;
                            }
                            if c == 'k' && !panes.is_empty() {
                                match focus_region {
                                    FocusRegion::Results => {
                                        if let Some(pane) = panes.get_mut(active_pane)
                                            && pane.selected > 0
                                        {
                                            pane.selected -= 1;
                                            cached_detail = None;
                                            detail_scroll = 0;
                                        }
                                    }
                                    FocusRegion::Detail => {
                                        detail_scroll = detail_scroll.saturating_sub(1);
                                    }
                                }
                                continue;
                            }
                            if c == 'h' && !panes.is_empty() {
                                match focus_region {
                                    FocusRegion::Results => {
                                        active_pane = active_pane.saturating_sub(1);
                                        if active_pane < pane_scroll_offset {
                                            pane_scroll_offset = active_pane;
                                        }
                                        focus_flash_until =
                                            Some(Instant::now() + Duration::from_millis(220));
                                        cached_detail = None;
                                        detail_scroll = 0;
                                    }
                                    FocusRegion::Detail => {
                                        focus_region = FocusRegion::Results;
                                        status = "Focus: Results".to_string();
                                    }
                                }
                                continue;
                            }
                            if c == 'l' && !panes.is_empty() {
                                match focus_region {
                                    FocusRegion::Results => {
                                        if active_pane + 1 < panes.len() {
                                            active_pane += 1;
                                            if active_pane >= pane_scroll_offset + MAX_VISIBLE_PANES
                                            {
                                                pane_scroll_offset = active_pane
                                                    .saturating_sub(MAX_VISIBLE_PANES - 1);
                                            }
                                            focus_flash_until =
                                                Some(Instant::now() + Duration::from_millis(220));
                                            cached_detail = None;
                                            detail_scroll = 0;
                                        } else {
                                            focus_region = FocusRegion::Detail;
                                            status =
                                                "Focus: Detail (j/k scroll, h back)".to_string();
                                        }
                                    }
                                    FocusRegion::Detail => {
                                        // Already at rightmost
                                    }
                                }
                                continue;
                            }
                            query.push(c);
                            page = 0;
                            history_cursor = None;
                            suggestion_idx = None;
                            dirty_since = Some(Instant::now());
                            cached_detail = None;
                            detail_scroll = 0;
                        }
                        KeyCode::Backspace => {
                            if query.is_empty() {
                                if filters.created_to.take().is_some() {
                                    status = "Cleared to-timestamp filter".into();
                                } else if filters.created_from.take().is_some() {
                                    status = "Cleared from-timestamp filter".into();
                                } else if let Some(ws) = filters.workspaces.iter().next().cloned() {
                                    filters.workspaces.remove(&ws);
                                    status = format!("Removed workspace filter {ws}");
                                } else if let Some(agent) = filters.agents.iter().next().cloned() {
                                    filters.agents.remove(&agent);
                                    status = format!("Removed agent filter {agent}");
                                } else {
                                    status = "Nothing to delete".into();
                                }
                            } else {
                                query.pop();
                            }
                            page = 0;
                            history_cursor = None;
                            suggestion_idx = None;
                            dirty_since = Some(Instant::now());
                            cached_detail = None;
                            detail_scroll = 0;
                        }
                        KeyCode::Enter => {
                            if panes.is_empty() && query.trim().is_empty() {
                                if let Some(idx) = suggestion_idx
                                    .and_then(|i| query_history.get(i))
                                    .or_else(|| query_history.front())
                                {
                                    query = idx.clone();
                                    status = format!("Loaded recent query: {idx}");
                                    dirty_since = Some(Instant::now());
                                    continue;
                                }
                                if !filters.agents.is_empty() {
                                    input_mode = InputMode::Agent;
                                    if let Some(last) = filters.agents.iter().next() {
                                        input_buffer = last.clone();
                                    }
                                    status =
                                        "Edit agent filter (Enter apply, Esc cancel)".to_string();
                                    continue;
                                }
                                if !filters.workspaces.is_empty() {
                                    input_mode = InputMode::Workspace;
                                    if let Some(last) = filters.workspaces.iter().next() {
                                        input_buffer = last.clone();
                                    }
                                    status = "Edit workspace filter (Enter apply, Esc cancel)"
                                        .to_string();
                                    continue;
                                }
                                if filters.created_from.is_some() {
                                    input_mode = InputMode::CreatedFrom;
                                    input_buffer =
                                        filters.created_from.unwrap_or_default().to_string();
                                    status =
                                        "Edit from timestamp (Enter apply, Esc cancel)".to_string();
                                    continue;
                                }
                                if filters.created_to.is_some() {
                                    input_mode = InputMode::CreatedTo;
                                    input_buffer =
                                        filters.created_to.unwrap_or_default().to_string();
                                    status =
                                        "Edit to timestamp (Enter apply, Esc cancel)".to_string();
                                    continue;
                                }
                            } else if active_hit(&panes, active_pane).is_some()
                                && cached_detail.is_some()
                            {
                                // Open full-screen detail modal for parsed viewing
                                show_detail_modal = true;
                                modal_scroll = 0;
                                status = "Detail view · Esc close · c copy · n nano".to_string();
                            } else if active_hit(&panes, active_pane).is_some() {
                                status = "Loading conversation...".to_string();
                            }
                        }
                        _ => {}
                    }
                }
                InputMode::Agent => match key.code {
                    KeyCode::Esc => {
                        input_mode = InputMode::Query;
                        input_buffer.clear();
                        status = "Agent filter cancelled".to_string();
                    }
                    KeyCode::Tab => {
                        // Tab completes to first matching suggestion
                        let suggestions = agent_suggestions(&input_buffer);
                        if let Some(first) = suggestions.first() {
                            input_buffer = first.to_string();
                            status = format!("Completed to '{}'. Press Enter to apply.", first);
                        }
                    }
                    KeyCode::Enter => {
                        filters.agents.clear();
                        if !input_buffer.trim().is_empty() {
                            filters.agents.insert(input_buffer.trim().to_string());
                        }
                        page = 0;
                        input_mode = InputMode::Query;
                        active_pane = 0;
                        cached_detail = None;
                        detail_scroll = 0;
                        status = format!(
                            "Agent filter set to {}",
                            filters
                                .agents
                                .iter()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        input_buffer.clear();
                        dirty_since = Some(Instant::now());
                        focus_region = FocusRegion::Results;
                    }
                    KeyCode::Backspace => {
                        input_buffer.pop();
                        // Update suggestions in status
                        let suggestions = agent_suggestions(&input_buffer);
                        if !suggestions.is_empty() && !input_buffer.is_empty() {
                            status = format!(
                                "Suggestions: {} (Tab to complete)",
                                suggestions.join(", ")
                            );
                        } else if input_buffer.is_empty() {
                            status = format!(
                                "Agents: {} (type to filter, Tab to complete)",
                                KNOWN_AGENTS.join(", ")
                            );
                        }
                    }
                    KeyCode::Char(c) => {
                        input_buffer.push(c);
                        // Update suggestions in status
                        let suggestions = agent_suggestions(&input_buffer);
                        if suggestions.is_empty() {
                            status =
                                format!("No matching agents. Known: {}", KNOWN_AGENTS.join(", "));
                        } else if suggestions.len() == 1 {
                            status = format!(
                                "Match: {} (Tab to complete, Enter to apply)",
                                suggestions[0]
                            );
                        } else {
                            status = format!(
                                "Suggestions: {} (Tab to complete)",
                                suggestions.join(", ")
                            );
                        }
                    }
                    _ => {}
                },
                InputMode::Workspace => match key.code {
                    KeyCode::Esc => {
                        input_mode = InputMode::Query;
                        input_buffer.clear();
                        status = "Workspace filter cancelled".to_string();
                    }
                    KeyCode::Enter => {
                        filters.workspaces.clear();
                        if !input_buffer.trim().is_empty() {
                            filters.workspaces.insert(input_buffer.trim().to_string());
                        }
                        page = 0;
                        input_mode = InputMode::Query;
                        active_pane = 0;
                        cached_detail = None;
                        detail_scroll = 0;
                        status = format!(
                            "Workspace filter set to {}",
                            filters
                                .workspaces
                                .iter()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        input_buffer.clear();
                        dirty_since = Some(Instant::now());
                        focus_region = FocusRegion::Results;
                    }
                    KeyCode::Backspace => {
                        input_buffer.pop();
                    }
                    KeyCode::Char(c) => input_buffer.push(c),
                    _ => {}
                },
                InputMode::CreatedFrom => match key.code {
                    KeyCode::Esc => {
                        input_mode = InputMode::Query;
                        input_buffer.clear();
                        status = "From timestamp cancelled".to_string();
                    }
                    KeyCode::Enter => {
                        let parsed = parse_time_input(&input_buffer);
                        if parsed.is_some() || input_buffer.trim().is_empty() {
                            filters.created_from = parsed;
                            page = 0;
                            input_mode = InputMode::Query;
                            active_pane = 0;
                            cached_detail = None;
                            detail_scroll = 0;
                            status = if let Some(ts) = parsed {
                                format!("From filter set: {}", format_time_short(ts))
                            } else {
                                "From filter cleared".to_string()
                            };
                            input_buffer.clear();
                            dirty_since = Some(Instant::now());
                            focus_region = FocusRegion::Results;
                        } else {
                            status = format!(
                                "Invalid time format '{}'. Try: -7d, yesterday, 2024-11-25",
                                input_buffer.trim()
                            );
                        }
                    }
                    KeyCode::Backspace => {
                        input_buffer.pop();
                    }
                    KeyCode::Char(c) => input_buffer.push(c),
                    _ => {}
                },
                InputMode::CreatedTo => match key.code {
                    KeyCode::Esc => {
                        input_mode = InputMode::Query;
                        input_buffer.clear();
                        status = "To timestamp cancelled".to_string();
                    }
                    KeyCode::Enter => {
                        let parsed = parse_time_input(&input_buffer);
                        if parsed.is_some() || input_buffer.trim().is_empty() {
                            filters.created_to = parsed;
                            page = 0;
                            input_mode = InputMode::Query;
                            active_pane = 0;
                            cached_detail = None;
                            detail_scroll = 0;
                            status = if let Some(ts) = parsed {
                                format!("To filter set: {}", format_time_short(ts))
                            } else {
                                "To filter cleared".to_string()
                            };
                            input_buffer.clear();
                            dirty_since = Some(Instant::now());
                            focus_region = FocusRegion::Results;
                        } else {
                            status = format!(
                                "Invalid time format '{}'. Try: -7d, yesterday, 2024-11-25",
                                input_buffer.trim()
                            );
                        }
                    }
                    KeyCode::Backspace => {
                        input_buffer.pop();
                    }
                    KeyCode::Char(c) => input_buffer.push(c),
                    _ => {}
                },
            }
        }

        if last_tick.elapsed() >= tick_rate {
            if let Some(client) = &search_client {
                let should_search = dirty_since
                    .map(|t| t.elapsed() >= debounce)
                    .unwrap_or(false);

                if should_search {
                    last_query = query.clone();
                    let prev_agent = active_hit(&panes, active_pane)
                        .map(|h| h.agent.clone())
                        .or_else(|| panes.get(active_pane).map(|p| p.agent.clone()));
                    let prev_path = active_hit(&panes, active_pane).map(|h| h.source_path.clone());
                    let q = apply_match_mode(&query, match_mode);
                    match client.search(&q, filters.clone(), page_size, page * page_size) {
                        Ok(hits) => {
                            dirty_since = None;
                            if hits.is_empty() && page > 0 {
                                page = page.saturating_sub(1);
                                active_pane = 0;
                                dirty_since = Some(Instant::now());
                                needs_draw = true;
                            } else {
                                results = hits;
                                let max_created = results
                                    .iter()
                                    .filter_map(|h| h.created_at)
                                    .max()
                                    .unwrap_or(0)
                                    as f32;
                                let alpha = match ranking_mode {
                                    RankingMode::RecentHeavy => 1.0,
                                    RankingMode::Balanced => 0.4,
                                    RankingMode::RelevanceHeavy => 0.1,
                                };
                                results.sort_by(|a, b| {
                                    let recency = |h: &SearchHit| -> f32 {
                                        if max_created <= 0.0 {
                                            return 0.0;
                                        }
                                        h.created_at.map(|v| v as f32 / max_created).unwrap_or(0.0)
                                    };
                                    let score_a = a.score + alpha * recency(a);
                                    let score_b = b.score + alpha * recency(b);
                                    score_b
                                        .partial_cmp(&score_a)
                                        .unwrap_or(std::cmp::Ordering::Equal)
                                });
                                panes = build_agent_panes(&results, per_pane_limit);
                                if !panes.is_empty()
                                    && let Some(agent) = prev_agent
                                {
                                    if let Some(idx) =
                                        panes.iter().position(|pane| pane.agent == agent)
                                    {
                                        active_pane = idx;
                                        if let Some(path) = prev_path
                                            && let Some(hit_idx) = panes[idx]
                                                .hits
                                                .iter()
                                                .position(|h| h.source_path == path)
                                        {
                                            panes[idx].selected = hit_idx;
                                        }
                                    } else {
                                        active_pane = 0;
                                    }
                                }
                                if panes.is_empty() {
                                    active_pane = 0;
                                }
                                // Ensure scroll offset puts active_pane in visible range
                                if active_pane < pane_scroll_offset {
                                    pane_scroll_offset = active_pane;
                                } else if active_pane >= pane_scroll_offset + MAX_VISIBLE_PANES {
                                    pane_scroll_offset =
                                        active_pane.saturating_sub(MAX_VISIBLE_PANES - 1);
                                }
                                // Reset scroll if it's beyond available panes
                                if pane_scroll_offset > panes.len().saturating_sub(1) {
                                    pane_scroll_offset = 0;
                                }
                                // Show a clean, user-friendly status
                                let total_hits: usize = panes.iter().map(|p| p.total_count).sum();
                                status = if total_hits == 0 {
                                    "No results found".to_string()
                                } else if panes.len() == 1 {
                                    format!("{} results", total_hits)
                                } else {
                                    format!("{} results across {} agents", total_hits, panes.len())
                                };
                                if !query.trim().is_empty()
                                    && query_history.front().map(|q| q != &query).unwrap_or(true)
                                {
                                    query_history.push_front(query.clone());
                                    if query_history.len() > history_cap {
                                        query_history.pop_back();
                                    }
                                }
                                history_cursor = None;
                                needs_draw = true;
                            }
                        }
                        Err(err) => {
                            dirty_since = None;
                            status = "Search error (see footer).".to_string();
                            tracing::warn!("search error: {err}");
                            results.clear();
                            panes.clear();
                            active_pane = 0;
                            needs_draw = true;
                        }
                    }
                }
            }
            // Advance spinner and redraw if search is pending
            if dirty_since.is_some() {
                spinner_frame = spinner_frame.wrapping_add(1);
                needs_draw = true;
            }
            last_tick = Instant::now();
        }
    }

    if let Some(saved) = peek_window_saved.take() {
        context_window = saved;
    }

    let persisted_out = TuiStatePersisted {
        match_mode: Some(match match_mode {
            MatchMode::Standard => "standard".into(),
            MatchMode::Prefix => "prefix".into(),
        }),
        context_window: Some(context_window.label().into()),
        // Mark that user has seen (or had opportunity to see) the help overlay
        has_seen_help: Some(true),
        // Persist query history for next session
        query_history: Some(query_history.iter().cloned().collect()),
    };
    save_state(&state_path, &persisted_out);

    teardown_terminal()
}

fn default_db_path_for(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("agent_search.db")
}

fn run_tui_headless(data_dir_override: Option<std::path::PathBuf>) -> Result<()> {
    let data_dir = data_dir_override.unwrap_or_else(default_data_dir);
    let index_path = index_dir(&data_dir)?;
    let db_path = default_db_path_for(&data_dir);
    let client = SearchClient::open(&index_path, Some(&db_path))?
        .ok_or_else(|| anyhow::anyhow!("index/db not found"))?;
    let _ = client.search("", SearchFilters::default(), 5, 0)?;
    Ok(())
}

fn teardown_terminal() -> Result<()> {
    let mut stdout = io::stdout();
    disable_raw_mode()?;
    execute!(stdout, LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn state_roundtrip_persists_mode_and_context() {
        let dir = TempDir::new().unwrap();
        let path = state_path_for(dir.path());

        let state = TuiStatePersisted {
            match_mode: Some("prefix".into()),
            context_window: Some("XL".into()),
            has_seen_help: Some(true),
            query_history: Some(vec!["test query".into(), "another search".into()]),
        };
        save_state(&path, &state);

        let loaded = load_state(&path);
        assert_eq!(loaded.match_mode.as_deref(), Some("prefix"));
        assert_eq!(loaded.context_window.as_deref(), Some("XL"));
        assert_eq!(loaded.has_seen_help, Some(true));
        assert_eq!(loaded.query_history.as_ref().map(|v| v.len()), Some(2));
    }

    #[test]
    fn parse_time_input_handles_various_formats() {
        // Relative time formats
        let now_ms = Utc::now().timestamp_millis();

        // -7d should be ~7 days ago (within 1 minute tolerance for test duration)
        let seven_days_ago = parse_time_input("-7d").unwrap();
        let expected_7d = now_ms - 7 * 24 * 60 * 60 * 1000;
        assert!((seven_days_ago - expected_7d).abs() < 60000);

        // -24h should be ~24 hours ago
        let day_ago = parse_time_input("-24h").unwrap();
        let expected_24h = now_ms - 24 * 60 * 60 * 1000;
        assert!((day_ago - expected_24h).abs() < 60000);

        // Keyword shortcuts
        assert!(parse_time_input("now").is_some());
        assert!(parse_time_input("today").is_some());
        assert!(parse_time_input("yesterday").is_some());

        // ISO date format
        let iso_date = parse_time_input("2024-11-25").unwrap();
        assert!(iso_date > 0);

        // Numeric timestamp (seconds)
        let ts_seconds = parse_time_input("1732500000").unwrap();
        assert_eq!(ts_seconds, 1732500000000); // Converted to ms

        // Numeric timestamp (milliseconds)
        let ts_ms = parse_time_input("1732500000000").unwrap();
        assert_eq!(ts_ms, 1732500000000);

        // Invalid input returns None
        assert!(parse_time_input("invalid").is_none());
        assert!(parse_time_input("").is_none());
        assert!(parse_time_input("-xyz").is_none());
    }

    #[test]
    fn contextual_snippet_handles_multibyte_and_short_text() {
        let text = "こんにちは世界"; // 5+2 chars in Japanese
        let out = contextual_snippet(text, "世界", ContextWindow::Small);
        assert!(out.contains("世界"));

        let short = "hi";
        let out_short = contextual_snippet(short, "hi", ContextWindow::XLarge);
        assert_eq!(out_short, "hi");

        let empty_q = contextual_snippet(text, "", ContextWindow::Small);
        assert!(!empty_q.is_empty());
    }
}
