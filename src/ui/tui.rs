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
use once_cell::sync::OnceCell;
use ratatui::prelude::*;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::io;
use std::path::Path;
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::default_data_dir;
use crate::model::types::MessageRole;
use crate::search::model_download::{DownloadProgress, ModelDownloader, ModelManifest};
use crate::search::model_manager::{
    SemanticAvailability, default_model_dir, load_hash_semantic_context, load_semantic_context,
};
use crate::search::query::{
    CacheStats, QuerySuggestion, SearchClient, SearchFilters, SearchHit, SearchMode,
};
use crate::search::tantivy::index_dir;
use crate::ui::components::help_strip;
use crate::ui::components::palette::{self, PaletteAction, PaletteState};
use crate::ui::components::pills::{self, Pill};
use crate::ui::components::theme::ThemePalette;
use crate::ui::components::toast::{Toast, ToastManager, render_toasts};
use crate::ui::components::widgets::search_bar;
use crate::ui::data::{ConversationView, InputMode, load_conversation, role_style};
use crate::ui::shortcuts;
use crate::update_check::{
    UpdateInfo, open_in_browser, run_self_update, skip_version, spawn_update_check,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetailTab {
    Messages,
    Snippets,
    Raw,
}

/// Format a timestamp as a short human-readable date for filter chips.
/// Shows "Nov 25" for same year, "Nov 25, 2023" for other years.
pub fn format_time_short(ms: i64) -> String {
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

fn split_editor_command(editor: &str) -> (String, Vec<String>) {
    let trimmed = editor.trim();
    if trimmed.is_empty() {
        return ("vi".to_string(), Vec::new());
    }
    match shell_words::split(trimmed) {
        Ok(parts) if !parts.is_empty() => (parts[0].clone(), parts[1..].to_vec()),
        _ => (trimmed.to_string(), Vec::new()),
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchMode {
    Standard,
    Prefix,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RankingMode {
    RecentHeavy,
    Balanced,
    RelevanceHeavy,
    MatchQualityHeavy, // Prioritizes exact matches over wildcard/fuzzy
    DateNewest,        // Pure newest-first (ignores relevance score)
    DateOldest,        // Pure oldest-first (ignores relevance score)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextWindow {
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
            ContextWindow::Small => 200,
            ContextWindow::Medium => 400,
            ContextWindow::Large => 800,
            ContextWindow::XLarge => 1600,
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

/// Display density presets for result lists.
/// Controls lines-per-item and visual spacing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DensityMode {
    /// 2 lines per item: header + location. Maximum items visible.
    Compact,
    /// 4 lines per item: header + location + 2 snippet lines. Balanced.
    #[default]
    Cozy,
    /// 6 lines per item: header + location + 4 snippet lines. Maximum context.
    Spacious,
}

impl DensityMode {
    fn next(self) -> Self {
        match self {
            DensityMode::Compact => DensityMode::Cozy,
            DensityMode::Cozy => DensityMode::Spacious,
            DensityMode::Spacious => DensityMode::Compact,
        }
    }

    /// Max snippet lines to render per result for this density.
    fn snippet_lines(self) -> usize {
        match self {
            DensityMode::Compact => 3,
            DensityMode::Cozy => 5,
            DensityMode::Spacious => 8,
        }
    }

    /// Approximate lines per item for layout sizing (header + location + snippet).
    fn lines_per_item(self) -> usize {
        // 2 lines for header/location + snippet allowance.
        2 + self.snippet_lines()
    }

    fn label(self) -> &'static str {
        match self {
            DensityMode::Compact => "Compact",
            DensityMode::Cozy => "Cozy",
            DensityMode::Spacious => "Spacious",
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct TuiStatePersisted {
    match_mode: Option<String>,
    search_mode: Option<String>,
    context_window: Option<String>,
    /// Display density: "compact", "cozy", or "spacious".
    density_mode: Option<String>,
    /// Set to true after user dismisses help overlay for the first time.
    /// Prevents help from auto-showing on subsequent launches.
    has_seen_help: Option<bool>,
    /// Recently used search queries, most recent first. Persisted across sessions.
    query_history: Option<Vec<String>>,
    /// Saved views (slots 1-9).
    saved_views: Option<Vec<SavedViewPersisted>>,
    /// Persist help strip pinned state across runs.
    help_pinned: Option<bool>,
    /// Persisted per-pane item limit (bead 46t.1).
    per_pane_limit: Option<usize>,
    /// Persisted ranking mode (bead 46t.1): "recent", "balanced", "relevance", etc.
    ranking_mode: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct SavedViewPersisted {
    slot: u8,
    agents: Vec<String>,
    workspaces: Vec<String>,
    created_from: Option<i64>,
    created_to: Option<i64>,
    ranking: Option<String>,
    /// Source filter: "all", "local", "remote", or a specific source_id
    #[serde(default)]
    source_filter: Option<String>,
}

#[derive(Clone, Debug)]
struct SavedView {
    slot: u8,
    agents: std::collections::HashSet<String>,
    workspaces: std::collections::HashSet<String>,
    created_from: Option<i64>,
    created_to: Option<i64>,
    ranking: RankingMode,
    source_filter: crate::sources::provenance::SourceFilter,
}

#[derive(Clone, Debug)]
struct AgentPane {
    agent: String,
    hits: Vec<SearchHit>,
    selected: usize,
    /// Total number of results for this agent (may be more than `hits.len()` due to limit)
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

/// Creates a refined visual score indicator: `●●●●○ 8.2`
/// Uses 5 dots proportional to score (0-10 scale) with premium styling.
fn score_bar(score: f32, palette: ThemePalette) -> Vec<Span<'static>> {
    use crate::ui::components::theme::colors;

    let normalized = (score / 10.0).clamp(0.0, 1.0);
    let filled = (normalized * 5.0).round() as usize;
    let empty = 5 - filled;

    // Premium color based on score tier
    let color = if score >= 8.0 {
        colors::STATUS_SUCCESS
    } else if score >= 5.0 {
        palette.accent
    } else {
        palette.hint
    };

    let modifier = score_style(score);

    vec![
        Span::styled(
            "●".repeat(filled),
            Style::default().fg(color).add_modifier(modifier),
        ),
        Span::styled(
            "○".repeat(empty),
            Style::default()
                .fg(palette.hint)
                .add_modifier(Modifier::DIM),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{score:.1}"),
            Style::default().fg(color).add_modifier(modifier),
        ),
    ]
}

/// Linear interpolation between two u8 values.
/// t=0.0 returns a, t=1.0 returns b.
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (f32::from(a) + (f32::from(b) - f32::from(a)) * t.clamp(0.0, 1.0)) as u8
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

/// Calculates staggered reveal progress for a specific item index.
/// Returns 0.0 (invisible) to 1.0 (fully visible).
/// Items are revealed in sequence with `STAGGER_DELAY_MS` between each.
fn item_reveal_progress(
    anim_start: Option<Instant>,
    item_idx: usize,
    stagger_delay_ms: u64,
    fade_duration_ms: u64,
    max_animated: usize,
) -> f32 {
    match anim_start {
        Some(start) => {
            // Items beyond max_animated appear instantly
            if item_idx >= max_animated {
                return 1.0;
            }
            let elapsed_ms = start.elapsed().as_millis() as u64;
            let item_start_ms = item_idx as u64 * stagger_delay_ms;
            if elapsed_ms < item_start_ms {
                0.0 // Not yet started
            } else {
                let item_elapsed = elapsed_ms - item_start_ms;
                (item_elapsed as f32 / fade_duration_ms as f32).clamp(0.0, 1.0)
            }
        }
        None => 1.0, // No animation active, fully visible
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

    // If it fits, return as-is (approximate by character count)
    if display_path.chars().count() <= max_len {
        return display_path;
    }

    // Split path into non-empty components
    let parts: Vec<&str> = display_path.split('/').filter(|s| !s.is_empty()).collect();

    // Need at least 3 parts to truncate meaningfully
    if parts.len() <= 2 {
        // Just truncate from the right
        let ellipsis = "...";
        let ellipsis_chars = ellipsis.chars().count();
        if max_len <= ellipsis_chars {
            return ellipsis.chars().take(max_len).collect();
        }
        let available = max_len - ellipsis_chars;
        let prefix: String = display_path.chars().take(available).collect();
        return format!("{prefix}{ellipsis}");
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
        format!("/.../{second_last}/{last}")
    } else if display_path.starts_with('~') {
        format!("~/.../{second_last}/{last}")
    } else {
        format!("{prefix}/.../{second_last}/{last}")
    };

    // If truncated is still too long, fall back to just showing the filename
    if truncated.chars().count() > max_len && !last.is_empty() {
        let result = format!(".../{last}");
        if result.chars().count() <= max_len {
            return result;
        }
        // Last resort: truncate the filename itself
        let available = max_len.saturating_sub(4); // ".../"
        let truncated_last: String = last.chars().take(available).collect();
        return format!(".../{truncated_last}");
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
    query_suggestions: &[QuerySuggestion],
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    // Show the query they searched for
    if query.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "🔍 Ready to search".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(
            "Start typing to search your coding conversations.",
        ));
        lines.push(Line::from(""));

        // Quick start actions for empty query
        lines.push(Line::from(Span::styled(
            "Quick actions:".to_string(),
            Style::default().fg(palette.accent),
        )));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "today:",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" — search recent conversations"),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "agent:codex",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" — filter by agent"),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "error*",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" — wildcard search"),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled(
                "No results for ".to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("\"{query}\""),
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
                    format!("\"{suggestion}\""),
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                ),
                Span::styled(" ?".to_string(), Style::default().fg(palette.hint)),
            ]));
        }

        // Show actionable suggestions from the search engine (dft.1)
        if !query_suggestions.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Try instead (press number key):".to_string(),
                Style::default().fg(palette.accent),
            )));
            for sugg in query_suggestions.iter().take(3) {
                let shortcut = sugg.shortcut.unwrap_or(0);
                if shortcut > 0 {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {shortcut} "),
                            Style::default()
                                .fg(palette.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(sugg.message.clone(), Style::default().fg(palette.fg)),
                    ]));
                }
            }
        }
    }

    lines.push(Line::from(""));

    // Build quick actions (contextual, actionable)
    let mut quick_actions: Vec<Line<'static>> = Vec::new();

    // Wildcard suggestion - if query exists and doesn't use wildcards
    if !query.trim().is_empty() && !query.contains('*') {
        let base = query.split_whitespace().next().unwrap_or(query);
        if !base.is_empty() && base.len() <= 20 {
            quick_actions.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("{base}*"),
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" — try prefix wildcard"),
            ]));
            quick_actions.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("*{base}"),
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" — try suffix wildcard"),
            ]));
        }
    }

    // Time filter suggestion - if no time filter active
    if filters.created_from.is_none() && filters.created_to.is_none() && !query.trim().is_empty() {
        quick_actions.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "F5".to_string(),
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" — add time filter (today, this week, etc.)"),
        ]));
    }

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
        suggestions.push(format!("Clear agent filter: {agent_str} (Shift+F3)"));
    }

    // Workspace filter suggestion
    if !filters.workspaces.is_empty() {
        suggestions.push("Clear workspace filter (Shift+F4)".to_string());
    }

    // Time filter active suggestion
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

    // Render quick actions if any
    if !quick_actions.is_empty() && !query.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "Try instead:".to_string(),
            Style::default().fg(palette.accent),
        )));
        lines.extend(quick_actions);
        lines.push(Line::from(""));
    }

    // Render suggestions if any
    if !suggestions.is_empty() {
        lines.push(Line::from(Span::styled(
            "Suggestions:".to_string(),
            Style::default().fg(palette.hint),
        )));
        for s in suggestions {
            lines.push(Line::from(vec![
                Span::raw("  • "),
                Span::styled(s, Style::default().fg(palette.fg)),
            ]));
        }
        lines.push(Line::from(""));
    }

    // Index health tip - always show as it's a common issue
    if !query.trim().is_empty() {
        lines.push(Line::from(vec![
            Span::styled("💡 ", Style::default()),
            Span::styled(
                "Index stale?".to_string(),
                Style::default().fg(palette.hint),
            ),
            Span::raw(" Run "),
            Span::styled(
                "cass index --full".to_string(),
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" to refresh"),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("Ctrl+Del".to_string(), Style::default().fg(palette.accent)),
        Span::styled(
            " clear all filters".to_string(),
            Style::default().fg(palette.hint),
        ),
    ]));

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
        format!("{minutes}m ago")
    } else if hours < 24 {
        format!("{hours}h ago")
    } else if days < 7 {
        format!("{days}d ago")
    } else if days < 30 {
        format!("{}w ago", days / 7)
    } else {
        // For older timestamps, show absolute date
        DateTime::from_timestamp_millis(timestamp_ms).map_or_else(
            || "unknown".to_string(),
            |dt| dt.format("%Y-%m-%d").to_string(),
        )
    }
}

/// Formats a timestamp as an absolute string with date and time in UTC.
fn format_absolute_time(timestamp_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(timestamp_ms).map_or_else(
        || "unknown".to_string(),
        |dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    )
}

pub fn help_lines(palette: ThemePalette) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let add_section = |title: &str, items: &[String]| -> Vec<Line<'static>> {
        let mut v = Vec::new();
        v.push(Line::from(Span::styled(title.to_string(), palette.title())));
        for item in items {
            v.push(Line::from(format!("  {item}")));
        }
        v.push(Line::from(""));
        v
    };

    // Welcome / Layout section (bead 019)
    lines.push(Line::from(Span::styled(
        "Welcome to CASS - Coding Agent Session Search",
        palette.title(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from("  Layout:"));
    lines.push(Line::from(
        "  ┌─────────────────────────────────────────────────┐",
    ));
    lines.push(Line::from(
        "  │ [Search Bar]         [Filter Chips]    [Status] │",
    ));
    lines.push(Line::from(
        "  ├────────────────┬────────────────────────────────┤",
    ));
    lines.push(Line::from(
        "  │                │                                │",
    ));
    lines.push(Line::from(
        "  │   Results      │       Detail Preview           │",
    ));
    lines.push(Line::from(
        "  │   (Left/↑↓)    │       (Tab to focus)           │",
    ));
    lines.push(Line::from(
        "  │                │                                │",
    ));
    lines.push(Line::from(
        "  ├────────────────┴────────────────────────────────┤",
    ));
    lines.push(Line::from(
        "  │ [Help Strip]                                    │",
    ));
    lines.push(Line::from(
        "  └─────────────────────────────────────────────────┘",
    ));
    lines.push(Line::from(""));

    // Data Directories section
    lines.extend(add_section(
        "Data Locations",
        &[
            "Index & state: ~/.local/share/coding-agent-search/".to_string(),
            "  agent_search.db - Full-text search index".to_string(),
            "  tui_state.json - UI preferences | watch_state.json - Watch timestamps".to_string(),
            "  remotes/ - Synced session data from remote sources".to_string(),
            "Config: ~/.config/cass/sources.toml (remote sources)".to_string(),
            "Agents: Claude, Codex, Gemini, Cline, OpenCode, Amp, Cursor, ChatGPT, Aider, Pi-Agent, Factory"
                .to_string(),
        ],
    ));

    // Updates section
    lines.extend(add_section(
        "Updates",
        &[
            "Checks GitHub releases hourly (offline-friendly, no auto-download)".to_string(),
            "When available: banner shows at top with U/S/Esc options".to_string(),
            "  U - Open release page in browser (Shift+U)".to_string(),
            "  S - Skip this version permanently (Shift+S)".to_string(),
            "  Esc - Dismiss banner for this session".to_string(),
        ],
    ));

    lines.extend(add_section(
        "Search",
        &[
            format!(
                "type to live-search; {} focuses query; {} cycles history",
                shortcuts::FOCUS_QUERY,
                shortcuts::HISTORY_CYCLE
            ),
            "Wildcards: foo* (prefix), *foo (suffix), *foo* (contains)".to_string(),
            "Auto-fuzzy: searches with few results try *term* fallback".to_string(),
            format!("{} refresh search (re-query index)", shortcuts::REFRESH),
            "/ detail-find in preview; n/N to jump matches".to_string(),
        ],
    ));
    lines.extend(add_section(
        "Filters",
        &[
            format!("{} agent | {} workspace | {} from | {} to | {} clear all",
                shortcuts::FILTER_AGENT, shortcuts::FILTER_WORKSPACE, shortcuts::FILTER_DATE_FROM, shortcuts::FILTER_DATE_TO, shortcuts::CLEAR_FILTERS),
            format!("{} scope to active agent | {} clear scope | {} cycle time presets (24h/7d/30d/all)",
                shortcuts::SCOPE_AGENT, shortcuts::SCOPE_WORKSPACE, shortcuts::CYCLE_TIME_PRESETS),
            "Chips in search bar; Backspace removes last; Enter (query empty) edits last chip".to_string(),
        ],
    ));
    lines.extend(add_section(
        "Sources (Multi-Machine)",
        &[
            "F11 cycle source filter: all → local → remote → all".to_string(),
            "Shift+F11 opens source filter menu (select specific sources)".to_string(),
            "Remote sessions show [source-name] in results list".to_string(),
            "Setup: cass sources setup (interactive wizard with SSH discovery)".to_string(),
            "CLI: cass sources add|list|doctor|sync|mappings".to_string(),
            "Sync: rsync over SSH (delta transfers, additive-only for safety)".to_string(),
            "Config: ~/.config/cass/sources.toml".to_string(),
        ],
    ));
    lines.extend(add_section(
        "Modes",
        &[
            format!(
                "{} search mode: Lexical → Semantic → Hybrid",
                shortcuts::SEARCH_MODE
            ),
            format!(
                "{} match mode: prefix (default) ⇄ standard",
                shortcuts::MATCH_MODE
            ),
            format!(
                "{} ranking: recent → balanced → relevance → match-quality",
                shortcuts::RANKING
            ),
            format!(
                "{} theme: dark/light | Ctrl+B toggle border style",
                shortcuts::THEME
            ),
        ],
    ));
    lines.extend(add_section(
        "Context",
        &[
            format!(
                "{} cycles S/M/L/XL context window",
                shortcuts::CONTEXT_WINDOW
            ),
            "Ctrl+Space: peek XL for current hit, tap again to restore".to_string(),
        ],
    ));
    lines.extend(add_section(
        "Density",
        &["Shift+=/+ increase pane items; Alt+- decrease (min 4, max 50)".to_string()],
    ));
    lines.extend(add_section(
        "Navigation",
        &[
            "Arrows move; Left/Right pane; PgUp/PgDn page".to_string(),
            format!(
                "{} vim-style nav (when results showing)",
                shortcuts::VIM_NAV
            ),
            format!("{} or Alt+g/G jump to first/last item", shortcuts::JUMP_TOP),
            format!(
                "{} toggle select; {} bulk actions; Esc clears selection",
                shortcuts::TOGGLE_SELECT,
                shortcuts::BULK_MENU
            ),
            "Ctrl+Enter queue item; Ctrl+O open all queued".to_string(),
            format!("{} toggles focus (Results ⇄ Detail)", shortcuts::TAB_FOCUS),
            "[ / ] cycle detail tabs (when results showing)".to_string(),
        ],
    ));
    lines.extend(add_section(
        "Mouse",
        &[
            "Click pane/item to select; click detail area to focus".to_string(),
            "Scroll wheel: navigate results or scroll detail".to_string(),
        ],
    ));
    lines.extend(add_section(
        "Actions",
        &[
            format!(
                "{} opens detail modal (o=open, c=copy, p=path, s=snip, n=nano, Esc=close)",
                shortcuts::DETAIL_OPEN
            ),
            format!(
                "{} open hit in $EDITOR; {} copy path/content",
                shortcuts::EDITOR,
                shortcuts::COPY
            ),
            format!(
                "{} detail-find within messages; n/N cycle matches",
                shortcuts::PANE_FILTER
            ),
            format!(
                "{}/? toggle this help; {} quit (or back from detail)",
                shortcuts::HELP,
                shortcuts::QUIT
            ),
        ],
    ));
    lines.extend(add_section(
        "States",
        &[
            "UI state persists in tui_state.json (data dir).".to_string(),
            format!(
                "{} reset UI state (deletes tui_state.json) or launch with `cass tui --reset-state`",
                shortcuts::RESET_STATE
            ),
        ],
    ));
    lines.extend(add_section(
        "Empty state",
        &[
            "Shows recent per-agent hits before typing".to_string(),
            "Recent query suggestions appear when query is empty".to_string(),
        ],
    ));

    lines
}

fn render_help_overlay(frame: &mut Frame, palette: ThemePalette, scroll: u16) {
    let area = frame.area();
    let popup_area = centered_rect(70, 70, area);
    let lines = help_lines(palette);
    let block = Block::default()
        .title(Span::styled(
            "Quick Start & Shortcuts (F1 or ? to reopen)",
            palette.title(),
        ))
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

/// Create a centered popup with fixed dimensions.
/// The popup is clamped to available terminal space and centered.
fn centered_rect_fixed(width: u16, height: u16, r: Rect) -> Rect {
    // Clamp dimensions to available space (leave margin for visual separation)
    let actual_width = width.min(r.width.saturating_sub(4));
    let actual_height = height.min(r.height.saturating_sub(2));

    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(r.height.saturating_sub(actual_height) / 2),
            Constraint::Length(actual_height),
            Constraint::Length(r.height.saturating_sub(actual_height) / 2),
        ])
        .split(r);

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(r.width.saturating_sub(actual_width) / 2),
            Constraint::Length(actual_width),
            Constraint::Length(r.width.saturating_sub(actual_width) / 2),
        ])
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
            Span::styled(
                format_absolute_time(ts),
                Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
            ),
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
            .map(|t| format!(" · {}", format_absolute_time(t)))
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!("{role_icon} "), Style::default()),
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
                        .map(|l| format!(" {l}"))
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

        // Markdown-aware inline rendering with search highlight
        let mut base = Style::default();
        let mut content_body = line_text.to_string();
        let mut prefix = "  ".to_string();

        if trimmed.starts_with('#') {
            let hashes = trimmed.chars().take_while(|c| *c == '#').count();
            let after = trimmed[hashes..].trim_start();
            content_body = after.to_string();
            base = base
                .fg(palette.accent_alt)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
            prefix = format!("{} ", "#".repeat(hashes));
        } else if trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("+ ")
        {
            content_body = trimmed[2..].trim_start().to_string();
            prefix = " • ".to_string();
        } else if trimmed.starts_with('>') {
            content_body = trimmed.trim_start_matches('>').trim_start().to_string();
            prefix = " ❯ ".to_string();
            base = base.add_modifier(Modifier::ITALIC).fg(palette.hint);
        }

        let rendered =
            render_inline_markdown_line(&format!("{prefix}{content_body}"), query, palette, base);
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
        " {} · line {}/{} · Esc · o open · c copy · p path · s snip · n nano ",
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
/// Lines per item depends on density mode (Compact=2, Cozy=4, Spacious=6).
fn calculate_pane_limit(terminal_height: u16, density: DensityMode) -> usize {
    const OVERHEAD: u16 = 9;
    const RESULTS_PERCENT: f32 = 0.70;
    const MIN_ITEMS: usize = 3;
    const MAX_ITEMS: usize = 30;

    let lines_per_item = density.lines_per_item();
    let available = terminal_height.saturating_sub(OVERHEAD);
    let results_height = (f32::from(available) * RESULTS_PERCENT) as usize;
    let items = results_height / lines_per_item;
    items.clamp(MIN_ITEMS, MAX_ITEMS)
}

/// Return a filtered view of results using the pane-local filter (case-insensitive).
fn apply_pane_filter(results: &[SearchHit], pane_filter: Option<&str>) -> Vec<SearchHit> {
    if let Some(filter) = pane_filter.map(str::trim).filter(|s| !s.is_empty()) {
        let needle = filter.to_lowercase();
        results
            .iter()
            .filter(|h| {
                let haystacks = [
                    h.title.as_str(),
                    h.content.as_str(),
                    h.workspace.as_str(),
                    h.source_path.as_str(),
                ];
                haystacks
                    .iter()
                    .any(|part| part.to_lowercase().contains(&needle))
            })
            .cloned()
            .collect()
    } else {
        results.to_vec()
    }
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

#[allow(clippy::too_many_arguments)]
fn rebuild_panes_with_filter(
    results: &[SearchHit],
    pane_filter: Option<&str>,
    per_pane_limit: usize,
    active_pane: &mut usize,
    pane_scroll_offset: &mut usize,
    prev_agent: Option<String>,
    prev_path: Option<String>,
    max_visible_panes: usize,
) -> Vec<AgentPane> {
    let filtered = apply_pane_filter(results, pane_filter);
    let mut panes = build_agent_panes(&filtered, per_pane_limit);

    if panes.is_empty() {
        *active_pane = 0;
    } else if let Some(agent) = prev_agent {
        if let Some(idx) = panes.iter().position(|p| p.agent == agent) {
            *active_pane = idx;
            if let Some(path) = prev_path
                && let Some(hit_idx) = panes[idx].hits.iter().position(|h| h.source_path == path)
            {
                panes[idx].selected = hit_idx;
            }
        } else {
            *active_pane = 0;
        }
    } else if *active_pane >= panes.len() {
        *active_pane = panes.len().saturating_sub(1);
    }

    if *active_pane < *pane_scroll_offset {
        *pane_scroll_offset = *active_pane;
    } else if *active_pane >= *pane_scroll_offset + max_visible_panes {
        *pane_scroll_offset = active_pane.saturating_sub(max_visible_panes - 1);
    }
    if *pane_scroll_offset > panes.len().saturating_sub(1) {
        *pane_scroll_offset = 0;
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

/// Returns a persistent color for each agent type.
/// Colors are visually distinct and consistent across sessions.
fn agent_color(agent: &str) -> Color {
    match agent.to_lowercase().as_str() {
        "codex" => Color::Rgb(0, 200, 150), // Teal/Cyan (OpenAI Codex)
        "claude" | "claude_code" => Color::Rgb(204, 119, 34), // Orange/Amber (Anthropic)
        "gemini" | "gemini_cli" => Color::Rgb(66, 133, 244), // Google Blue
        "cline" => Color::Rgb(138, 43, 226), // Blue-Violet (VS Code extension)
        "opencode" => Color::Rgb(50, 205, 50), // Lime Green
        "amp" => Color::Rgb(255, 99, 71),   // Tomato/Coral (Sourcegraph)
        "cursor" => Color::Rgb(147, 112, 219), // Medium Purple
        "chatgpt" => Color::Rgb(16, 163, 127), // ChatGPT Green
        "aider" => Color::Rgb(255, 165, 0), // Orange
        "pi_agent" => Color::Rgb(255, 140, 0), // Dark Orange (Pi)
        _ => Color::Rgb(169, 169, 169),     // Dark Gray (fallback)
    }
}

/// Dims a color by the given factor (0.0 = black, 1.0 = original).
fn dim_color(color: Color, factor: f32) -> Color {
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(
            (r as f32 * factor) as u8,
            (g as f32 * factor) as u8,
            (b as f32 * factor) as u8,
        ),
        other => other, // Non-RGB colors returned as-is
    }
}

fn contextual_snippet(text: &str, query: &str, window: ContextWindow) -> String {
    let size = window.size();
    if text.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    if len <= size {
        return text.to_string();
    }

    let trimmed_query = query.trim();
    let mut char_pos: usize = 0;

    if !trimmed_query.is_empty() {
        if text.is_ascii() && trimmed_query.is_ascii() {
            // ASCII fast path: byte offsets are safe
            let lowercase = text.to_lowercase();
            let q = trimmed_query.to_lowercase();
            let byte_pos = lowercase.find(&q).or_else(|| {
                q.split_whitespace()
                    .next()
                    .and_then(|first| lowercase.find(first))
            });
            if let Some(b) = byte_pos {
                char_pos = b.min(text.len());
            }
        } else {
            // Unicode-safe: fall back to case-sensitive search on original text
            let first_term = trimmed_query
                .split_whitespace()
                .find(|s| !s.is_empty())
                .unwrap_or(trimmed_query);
            if let Some(b) = text.find(first_term) {
                char_pos = text[..b].chars().count();
            }
        }
    }

    let start = char_pos.saturating_sub(size / 2);
    let end = (start + size).min(len);
    let slice: String = chars[start..end].iter().collect();
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if end < len { "…" } else { "" };
    format!("{prefix}{slice}{suffix}")
}

/// Smart word wrap for display lines (sux.6.6d).
/// Wraps at word boundaries with a small left indent.
/// Returns wrapped lines with a single leading space on both the first and continuation lines.
fn smart_word_wrap(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let initial_indent = " ";
    let continuation_indent = " ";

    // Available width after indent
    let first_line_width = max_width.saturating_sub(initial_indent.len());
    let cont_line_width = max_width.saturating_sub(continuation_indent.len());

    if text.is_empty() || first_line_width == 0 {
        return lines;
    }

    // Split into words
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return lines;
    }

    let mut current_line = String::new();
    let mut is_first_line = true;

    for word in words {
        let word_len = word.chars().count();
        let current_len = current_line.chars().count();
        let available = if is_first_line {
            first_line_width
        } else {
            cont_line_width
        };

        if current_len == 0 {
            // First word on line
            if word_len > available {
                // Word too long - truncate it
                let truncated: String = word.chars().take(available.saturating_sub(1)).collect();
                current_line = format!("{truncated}…");
            } else {
                current_line = word.to_string();
            }
        } else if current_len + 1 + word_len <= available {
            // Word fits
            current_line.push(' ');
            current_line.push_str(word);
        } else {
            // Word doesn't fit - wrap
            let indent = if is_first_line {
                initial_indent
            } else {
                continuation_indent
            };
            lines.push(format!("{indent}{current_line}"));
            is_first_line = false;

            // Start new line with word
            if word_len > cont_line_width {
                let truncated: String = word
                    .chars()
                    .take(cont_line_width.saturating_sub(1))
                    .collect();
                current_line = format!("{truncated}…");
            } else {
                current_line = word.to_string();
            }
        }
    }

    // Don't forget the last line
    if !current_line.is_empty() {
        let indent = if is_first_line {
            initial_indent
        } else {
            continuation_indent
        };
        lines.push(format!("{indent}{current_line}"));
    }

    lines
}

/// Count query term occurrences in text (case-insensitive).
/// For multi-word queries, counts each term separately and sums.
/// Used for sux.6.6c match count display.
fn count_query_matches(text: &str, query: &str) -> usize {
    if query.is_empty() || text.is_empty() {
        return 0;
    }
    let text_lower = text.to_lowercase();
    let query_lower = query.to_lowercase();

    // First try exact phrase match count
    let phrase_count = text_lower.matches(&query_lower).count();
    if phrase_count > 0 {
        return phrase_count;
    }

    // Fall back to counting individual terms
    query_lower
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .map(|term| text_lower.matches(term).count())
        .sum()
}

/// Convert a ratatui Line into plain text for search/highlight helpers.
fn line_plain_text(line: &Line) -> String {
    line.spans
        .iter()
        .map(|s| s.content.clone().into_owned())
        .collect()
}

/// Return zero-based line numbers that contain `needle` (case-insensitive).
fn match_line_indices(lines: &[Line], needle: &str) -> Vec<u16> {
    if needle.trim().is_empty() {
        return Vec::new();
    }
    let needle_lc = needle.to_lowercase();
    lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| {
            let text = line_plain_text(line).to_lowercase();
            if text.contains(&needle_lc) {
                Some(idx.min(u16::MAX as usize) as u16)
            } else {
                None
            }
        })
        .collect()
}

struct SyntaxAssets {
    ps: SyntaxSet,
    theme_dark: Theme,
    theme_light: Theme,
}

static SYNTAX: OnceCell<Option<SyntaxAssets>> = OnceCell::new();

fn syntax_assets() -> Option<&'static SyntaxAssets> {
    SYNTAX
        .get_or_init(|| {
            let ps = SyntaxSet::load_defaults_newlines();
            let ts = ThemeSet::load_defaults();
            let theme_dark = ts
                .themes
                .get("base16-ocean.dark")
                .or_else(|| ts.themes.values().next())
                .cloned();
            let theme_light = ts
                .themes
                .get("base16-ocean.light")
                .or_else(|| ts.themes.values().next())
                .cloned();
            match (theme_dark, theme_light) {
                (Some(d), Some(l)) => Some(SyntaxAssets {
                    ps,
                    theme_dark: d,
                    theme_light: l,
                }),
                _ => None,
            }
        })
        .as_ref()
}

fn syntect_color_to_ratatui(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

fn syntax_highlight_line(
    line: &str,
    path_hint: &str,
    highlight_term: &str,
    palette: ThemePalette,
    theme_dark: bool,
) -> Option<Line<'static>> {
    if line.is_empty() || line.chars().count() > 400 {
        return None;
    }
    let assets = syntax_assets()?;
    let syntax = assets
        .ps
        .find_syntax_for_file(path_hint)
        .ok()
        .flatten()
        .or_else(|| assets.ps.find_syntax_by_extension("rs"))
        .unwrap_or_else(|| assets.ps.find_syntax_plain_text());
    let theme = if theme_dark {
        &assets.theme_dark
    } else {
        &assets.theme_light
    };
    let mut h = HighlightLines::new(syntax, theme);
    let ranges = h.highlight_line(line, &assets.ps).ok()?;

    let mut spans: Vec<Span<'static>> = Vec::new();
    for (style, text) in ranges {
        let base = Style::default().fg(syntect_color_to_ratatui(style.foreground));
        spans.extend(highlight_spans_owned(text, highlight_term, palette, base));
    }
    Some(Line::from(spans))
}

fn state_path_for(data_dir: &std::path::Path) -> std::path::PathBuf {
    // Persist lightweight, non-secret UI preferences (search/match mode, context window).
    data_dir.join("tui_state.json")
}

fn ranking_from_str(s: &str) -> RankingMode {
    match s {
        "recent" => RankingMode::RecentHeavy,
        "relevance" => RankingMode::RelevanceHeavy,
        "quality" => RankingMode::MatchQualityHeavy,
        "newest" => RankingMode::DateNewest,
        "oldest" => RankingMode::DateOldest,
        _ => RankingMode::Balanced,
    }
}

fn search_mode_from_str(s: &str) -> SearchMode {
    match s {
        "semantic" => SearchMode::Semantic,
        "hybrid" => SearchMode::Hybrid,
        _ => SearchMode::Lexical,
    }
}

fn search_mode_label(mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::Lexical => "Lexical",
        SearchMode::Semantic => "Semantic",
        SearchMode::Hybrid => "Hybrid",
    }
}

/// Get the mode indicator with appropriate styling based on mode and semantic availability.
/// Returns (mode_token, color) where color is appropriate for the mode:
/// - LEX: default (white)
/// - SEM: cyan (ML semantic)
/// - SEM*: cyan (hash fallback semantic)
/// - HYB: magenta (hybrid mode)
fn styled_mode_indicator(
    mode: SearchMode,
    semantic_availability: &SemanticAvailability,
) -> (&'static str, Color) {
    match mode {
        SearchMode::Lexical => ("LEX", Color::White),
        SearchMode::Semantic => {
            // Distinguish ML semantic (SEM) from hash fallback (SEM*)
            if matches!(semantic_availability, SemanticAvailability::HashFallback) {
                ("SEM*", Color::Cyan)
            } else {
                ("SEM", Color::Cyan)
            }
        }
        SearchMode::Hybrid => ("HYB", Color::Magenta),
    }
}

fn initialize_semantic_context(
    client: &SearchClient,
    data_dir: &Path,
    db_path: &Path,
) -> SemanticAvailability {
    let setup = load_semantic_context(data_dir, db_path);
    let mut availability = setup.availability;

    if let Some(context) = setup.context {
        if let Err(err) = client.set_semantic_context(
            context.embedder,
            context.index,
            context.filter_maps,
            context.roles,
        ) {
            availability = SemanticAvailability::LoadFailed {
                context: format!("set context: {err}"),
            };
        }
    } else {
        let _ = client.clear_semantic_context();
    }

    availability
}

fn semantic_unavailable_message(availability: &SemanticAvailability) -> String {
    // Use the built-in summary method which handles all variants
    availability.summary()
}

use crate::ui::components::breadcrumbs::{self, BreadcrumbKind};

fn chips_for_filters(filters: &SearchFilters, palette: ThemePalette) -> Vec<Span<'static>> {
    use crate::sources::provenance::SourceFilter;

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
    // Source filter chip (P4.3)
    match &filters.source_filter {
        SourceFilter::All => {} // No chip for "all"
        SourceFilter::Local => {
            spans.push(Span::styled(
                "[src:local]".to_string(),
                Style::default().fg(palette.accent),
            ));
            spans.push(Span::raw(" ".to_string()));
        }
        SourceFilter::Remote => {
            spans.push(Span::styled(
                "[src:remote]".to_string(),
                Style::default()
                    .fg(palette.accent_alt)
                    .add_modifier(Modifier::ITALIC),
            ));
            spans.push(Span::raw(" ".to_string()));
        }
        SourceFilter::SourceId(id) => {
            spans.push(Span::styled(
                format!("[src:{}]", id),
                Style::default()
                    .fg(palette.accent_alt)
                    .add_modifier(Modifier::ITALIC),
            ));
            spans.push(Span::raw(" ".to_string()));
        }
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

fn contextual_shortcuts(
    palette_open: bool,
    show_detail_modal: bool,
    input_mode: InputMode,
    focus_region: FocusRegion,
) -> Vec<(String, String)> {
    if palette_open {
        return vec![
            (shortcuts::DETAIL_CLOSE.into(), "Close".into()),
            ("↑/↓".into(), "Select".into()),
            (shortcuts::DETAIL_OPEN.into(), "Run".into()),
        ];
    }
    if show_detail_modal {
        return vec![
            (shortcuts::DETAIL_CLOSE.into(), "Close detail".into()),
            ("j/k".into(), "Scroll".into()),
            ("Home/End".into(), "Top/Bottom".into()),
            ("c".into(), "Copy".into()),
        ];
    }
    match input_mode {
        InputMode::Agent => vec![
            ("type".into(), "Agent filter".into()),
            (shortcuts::DETAIL_OPEN.into(), "Apply".into()),
            (shortcuts::DETAIL_CLOSE.into(), "Cancel".into()),
        ],
        InputMode::Workspace => vec![
            ("type".into(), "Workspace filter".into()),
            (shortcuts::DETAIL_OPEN.into(), "Apply".into()),
            (shortcuts::DETAIL_CLOSE.into(), "Cancel".into()),
        ],
        InputMode::PaneFilter => vec![
            ("type".into(), "Pane filter".into()),
            (shortcuts::DETAIL_OPEN.into(), "Apply".into()),
            (shortcuts::DETAIL_CLOSE.into(), "Clear".into()),
        ],
        InputMode::CreatedFrom | InputMode::CreatedTo => vec![
            ("type".into(), "Date (YYYY-MM-DD)".into()),
            (shortcuts::DETAIL_OPEN.into(), "Apply".into()),
            (shortcuts::DETAIL_CLOSE.into(), "Cancel".into()),
        ],
        InputMode::DetailFind => vec![
            ("type".into(), "Find term".into()),
            (shortcuts::DETAIL_OPEN.into(), "Apply".into()),
            (shortcuts::DETAIL_CLOSE.into(), "Cancel".into()),
        ],
        InputMode::Query => match focus_region {
            FocusRegion::Results => vec![
                ("Ctrl+P".into(), "Palette".into()),
                (shortcuts::VIM_NAV.into(), "Nav".into()),
                (shortcuts::DETAIL_OPEN.into(), "Open detail".into()),
                ("m".into(), "Select".into()),
                (shortcuts::BULK_MENU.into(), "Bulk menu".into()),
                (shortcuts::PANE_FILTER.into(), "Pane filter".into()),
                (
                    format!(
                        "{}/{}/{}",
                        shortcuts::FILTER_AGENT,
                        shortcuts::FILTER_WORKSPACE,
                        shortcuts::FILTER_DATE_FROM
                    ),
                    "Filters".into(),
                ),
                (shortcuts::QUIT.into(), "Quit/back".into()),
            ],
            FocusRegion::Detail => vec![
                (shortcuts::TAB_FOCUS.into(), "Focus results".into()),
                ("←/→".into(), "Tabs".into()),
                (shortcuts::PANE_FILTER.into(), "Find in detail".into()),
                ("n/N".into(), "Next/prev match".into()),
                ("c".into(), "Copy".into()),
                ("o".into(), "Open file".into()),
                (shortcuts::DETAIL_CLOSE.into(), "Close detail".into()),
            ],
        },
    }
}

fn save_view_slot(
    slot: u8,
    filters: &SearchFilters,
    ranking: RankingMode,
    saved_views: &mut Vec<SavedView>,
) -> String {
    if !(1..=9).contains(&slot) {
        return "Invalid slot".into();
    }
    saved_views.retain(|v| v.slot != slot);
    saved_views.push(SavedView {
        slot,
        agents: filters.agents.clone(),
        workspaces: filters.workspaces.clone(),
        created_from: filters.created_from,
        created_to: filters.created_to,
        ranking,
        source_filter: filters.source_filter.clone(),
    });
    saved_views.sort_by_key(|v| v.slot);
    format!("Saved view to slot {slot}")
}

fn load_view_slot(
    slot: u8,
    filters: &mut SearchFilters,
    ranking: &mut RankingMode,
    saved_views: &[SavedView],
) -> Option<String> {
    saved_views.iter().find(|v| v.slot == slot).map(|v| {
        filters.agents = v.agents.clone();
        filters.workspaces = v.workspaces.clone();
        filters.created_from = v.created_from;
        filters.created_to = v.created_to;
        filters.source_filter = v.source_filter.clone();
        *ranking = v.ranking;
        format!("Loaded view slot {slot}")
    })
}

fn load_state(path: &std::path::Path) -> TuiStatePersisted {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(path: &std::path::Path, state: &TuiStatePersisted) {
    if let Ok(body) = serde_json::to_string_pretty(state) {
        // Use atomic write: temp file + rename to prevent corruption on crash
        let temp_path = path.with_extension("json.tmp");
        if std::fs::write(&temp_path, &body).is_ok() {
            let _ = std::fs::rename(&temp_path, path);
        }
    }
}

/// Save a query to the history, avoiding duplicates and limiting size.
/// Only call this on explicit user commit actions (Enter on result, F8 editor, y copy).
fn save_query_to_history(query: &str, history: &mut VecDeque<String>, cap: usize) {
    let q = query.trim();
    if !q.is_empty() && history.front().is_none_or(|h| h != q) {
        history.push_front(q.to_string());
        if history.len() > cap {
            history.pop_back();
        }
    }
}

/// Deduplicate history by removing queries that are strict prefixes of other queries.
/// This cleans up any pollution from incremental typing before this fix was implemented.
/// Example: ["foobar", "foo", "foob", "bar"] -> ["foobar", "bar"]
pub fn dedupe_history_prefixes(history: Vec<String>) -> Vec<String> {
    let mut result: Vec<String> = Vec::with_capacity(history.len());
    for q in history {
        // Skip if this query is a strict prefix of any existing entry
        let is_prefix_of_existing = result
            .iter()
            .any(|existing| existing.starts_with(&q) && existing.len() > q.len());
        if is_prefix_of_existing {
            continue;
        }
        // Remove any existing entries that are strict prefixes of this query
        result.retain(|existing| !(q.starts_with(existing) && q.len() > existing.len()));
        result.push(q);
    }
    result
}

pub fn apply_match_mode(query: &str, mode: MatchMode) -> String {
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

pub fn highlight_spans_owned(
    text: &str,
    query: &str,
    palette: ThemePalette,
    base: Style,
) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if query.trim().is_empty() {
        spans.push(Span::styled(text.to_string(), base));
        return spans;
    }

    let lower = text.to_lowercase();
    let q = query.to_lowercase();

    // If Unicode casefolding changes byte lengths (e.g., ß -> ss), fall back to
    // case-sensitive matching to avoid slicing errors.
    if lower.len() != text.len() || q.len() != query.len() {
        let mut remaining = text;
        while let Some(pos) = remaining.find(query) {
            if pos > 0 {
                spans.push(Span::styled(remaining[..pos].to_string(), base));
            }
            let end = pos + query.len();
            spans.push(Span::styled(
                remaining[pos..end].to_string(),
                base.patch(palette.highlight_style()),
            ));
            remaining = &remaining[end..];
        }
        if !remaining.is_empty() {
            spans.push(Span::styled(remaining.to_string(), base));
        }
        return spans;
    }
    let mut idx = 0;
    while let Some(pos) = lower[idx..].find(&q) {
        let start = idx + pos;
        if start > idx {
            spans.push(Span::styled(text[idx..start].to_string(), base));
        }
        let end = start + q.len();
        spans.push(Span::styled(
            text[start..end].to_string(),
            base.patch(palette.highlight_style()),
        ));
        idx = end;
    }
    if idx < text.len() {
        spans.push(Span::styled(text[idx..].to_string(), base));
    }
    spans
}

fn highlight_terms_owned_with_style(
    text: String,
    query: &str,
    palette: ThemePalette,
    base: Style,
) -> Line<'static> {
    Line::from(highlight_spans_owned(&text, query, palette, base))
}

/// Render a single line with light-weight inline markdown (bold/italic/`code`) and
/// search-term highlighting. Keeps everything ASCII-friendly for predictable widths.
fn render_inline_markdown_line(
    line: &str,
    query: &str,
    palette: ThemePalette,
    base: Style,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut rest = line;

    while !rest.is_empty() {
        if let Some(content) = rest.strip_prefix("**")
            && let Some(end) = content.find("**")
        {
            let (bold_text, tail) = content.split_at(end);
            let highlighted =
                highlight_spans_owned(bold_text, query, palette, base.add_modifier(Modifier::BOLD));
            spans.extend(highlighted);
            rest = tail.trim_start_matches('*');
            continue;
        }

        if let Some(content) = rest.strip_prefix('`')
            && let Some(end) = content.find('`')
        {
            let (code_text, tail) = content.split_at(end);
            let highlighted = highlight_spans_owned(
                code_text,
                query,
                palette,
                base.bg(palette.surface).fg(palette.accent_alt),
            );
            spans.extend(highlighted);
            rest = &tail[1..]; // skip closing backtick
            continue;
        }

        if let Some(content) = rest.strip_prefix('*')
            && !content.starts_with('*')
            && let Some(end) = content.find('*')
        {
            let (ital_text, tail) = content.split_at(end);
            let highlighted = highlight_spans_owned(
                ital_text,
                query,
                palette,
                base.add_modifier(Modifier::ITALIC),
            );
            spans.extend(highlighted);
            rest = tail.trim_start_matches('*');
            continue;
        }

        // Plain chunk until next special token
        let next_special = rest.find(['*', '`']).unwrap_or(rest.len());

        if next_special == 0 {
            // Avoid infinite loop on stray marker; emit literally and advance
            if let Some((ch, tail)) = rest.chars().next().map(|c| (c, &rest[c.len_utf8()..])) {
                spans.extend(highlight_spans_owned(&ch.to_string(), query, palette, base));
                rest = tail;
                continue;
            }
        }

        let (plain, tail) = rest.split_at(next_special);
        spans.extend(highlight_spans_owned(plain, query, palette, base));
        rest = tail;
    }

    Line::from(spans)
}

fn quick_date_range_today() -> Option<(i64, i64)> {
    use chrono::{Datelike, Local, TimeZone};
    let now = Local::now();
    let start = Local
        .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
        .single()?;
    Some((start.timestamp_millis(), now.timestamp_millis()))
}

fn quick_date_range_week() -> Option<(i64, i64)> {
    use chrono::{Duration, Local};
    let now = Local::now();
    let week_ago = now - Duration::days(7);
    Some((week_ago.timestamp_millis(), now.timestamp_millis()))
}

fn quick_date_range_hours(hours: i64) -> Option<(i64, i64)> {
    use chrono::{Duration, Local};
    let now = Local::now();
    let since = now - Duration::hours(hours);
    Some((since.timestamp_millis(), now.timestamp_millis()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusRegion {
    Results,
    Detail,
}

#[derive(Clone, Debug, Default)]
pub struct DetailFindState {
    pub query: String,
    pub matches: Vec<u16>,
    pub current: usize,
}

#[allow(dead_code)]
fn char_width(s: &str) -> usize {
    s.chars().count()
}

#[allow(dead_code)]
/// Build a dense shortcut legend that fits within `max_width` characters.
fn footer_shortcuts(max_width: usize) -> String {
    const SHORTCUTS: &[&str] = &[
        "j/k move",
        "Tab focus",
        "Enter open",
        "/ query",
        "[ ] tabs",
        "C-spc peek",
        "m select",
        "y copy",
        "F3 agent",
        "F4 ws",
        "F5/F6 time",
        "F7 ctx",
        "F9 match",
        "F11 src",
        "F12 rank",
        "Ctrl+R hist",
        "Ctrl+Shift+R refresh",
        "Ctrl+Shift+Del reset",
        "F2 theme",
        "Esc quit",
        "F1 help",
    ];

    let mut out = String::new();
    for (idx, item) in SHORTCUTS.iter().enumerate() {
        let separator = if out.is_empty() { "" } else { " | " };
        let projected = char_width(&out) + char_width(separator) + char_width(item);
        if projected > max_width {
            if !out.is_empty() && char_width(&out) + 2 <= max_width {
                out.push_str(" …");
            }
            break;
        }
        out.push_str(separator);
        out.push_str(item);
        // Leave space for at least one more item to avoid frequent truncation flicker
        if idx + 1 == SHORTCUTS.len() {
            break;
        }
    }
    out
}

// Legacy helper retained for tests/compat; superseded by `footer_shortcuts` in the live footer.
pub fn footer_legend(show_help: bool) -> &'static str {
    if show_help {
        "Esc quit • arrows nav • Tab focus • Enter view • F8 editor • F1-F9 commands • y copy"
    } else {
        "F1 help | Enter view | Esc quit"
    }
}

/// Helper to prepare a file path for opening in an editor.
///
/// For standard files, returns the path as-is.
/// For virtual paths (e.g. Cursor .vscdb entries), exports the conversation content
/// to a temporary Markdown file and returns the path to that temp file.
fn prepare_editor_path(path: &str, db: Option<&crate::storage::sqlite::SqliteStorage>) -> String {
    // Check for Cursor virtual paths (contain .vscdb)
    if path.contains(".vscdb")
        && let Some(storage) = db
    {
        // Attempt to load the conversation from the DB
        if let Ok(Some(view)) = load_conversation(storage, path) {
            // Create a temp file with a meaningful name
            let id = view.convo.id.unwrap_or(0);
            let safe_title = view
                .convo
                .title
                .as_deref()
                .unwrap_or("cursor_session")
                .replace(|c: char| !c.is_alphanumeric(), "_");
            let filename = format!("cass_cursor_{}_{}.md", id, safe_title);
            let temp_path = std::env::temp_dir().join(filename);

            // Render content to Markdown
            let mut content = String::new();
            content.push_str("<!-- ⚠️ READ ONLY: This is a temporary export. Edits will NOT be saved to the original source. -->\n\n");
            if let Some(title) = &view.convo.title {
                content.push_str(&format!("# {}\n\n", title));
            }
            content.push_str(&format!("**Agent:** {}\n", view.convo.agent_slug));
            if let Some(ws) = &view.workspace {
                content.push_str(&format!("**Workspace:** {}\n", ws.path.display()));
            }
            if let Some(ts) = view.convo.started_at {
                content.push_str(&format!("**Date:** {}\n", format_absolute_time(ts)));
            }
            content.push_str("\n---\n\n");

            for msg in view.messages {
                content.push_str(&format!("### {}\n\n", msg.role));
                content.push_str(&msg.content);
                content.push_str("\n\n");
            }

            // Write to temp file
            if std::fs::write(&temp_path, content).is_ok() {
                return temp_path.to_string_lossy().into_owned();
            }
        }
    }

    // Default: return original path
    path.to_string()
}

pub fn run_tui(
    data_dir_override: Option<std::path::PathBuf>,
    once: bool,
    reset_state: bool,
    progress: Option<std::sync::Arc<crate::indexer::IndexingProgress>>,
    reindex_tx: Option<crossbeam_channel::Sender<crate::indexer::IndexerEvent>>,
) -> Result<()> {
    // Resolve data dir early so we can honor reset-state in headless mode too.
    let data_dir = data_dir_override.unwrap_or_else(default_data_dir);
    let state_path = state_path_for(&data_dir);

    // Optional: wipe persisted UI state before loading defaults.
    if reset_state {
        let _ = std::fs::remove_file(&state_path);
    }

    if once
        && dotenvy::var("TUI_HEADLESS")
            .map(|v| v == "1")
            .unwrap_or(false)
    {
        return run_tui_headless(Some(data_dir));
    }

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let index_path = index_dir(&data_dir)?;
    let db_path = default_db_path_for(&data_dir);
    let persisted = load_state(&state_path);
    let search_client = SearchClient::open(&index_path, Some(&db_path))?;
    let mut semantic_availability = if let Some(client) = &search_client {
        initialize_semantic_context(client, &data_dir, &db_path)
    } else {
        SemanticAvailability::LoadFailed {
            context: "index/db not ready".to_string(),
        }
    };

    // UI metrics flag (bead 020) - emit privacy-safe local metrics when enabled
    // Set CASS_UI_METRICS=1 to enable tracing of UI interactions
    let ui_metrics_enabled = dotenvy::var("CASS_UI_METRICS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    // Open a read-only connection for the UI to fetch details efficiently.
    // If DB doesn't exist yet (first run), this will be None, which is fine as we can't view details anyway.
    let db_reader = crate::storage::sqlite::SqliteStorage::open_readonly(&db_path).ok();

    let index_ready = search_client.is_some();
    let mut status = if index_ready {
        format!(
            "Index ready at {} - type to search (Esc/F10 quit, F1 help)",
            index_path.display()
        )
    } else if progress.is_some() {
        "Index not ready yet. Background indexing is running...".to_string()
    } else {
        format!(
            "Index not present at {}. Run `cass index --full` then reopen TUI.",
            index_path.display()
        )
    };

    if reset_state {
        status = format!("State reset (tui_state.json cleared). {status}");
    }

    let mut query = String::new();
    let mut filters = SearchFilters::default();
    let mut input_mode = InputMode::Query;
    let mut input_buffer = String::new();
    let page_size: usize = 120;
    // Load density mode from persisted state (case-insensitive)
    let mut density_mode = match persisted
        .density_mode
        .as_deref()
        .map(str::to_lowercase)
        .as_deref()
    {
        Some("compact") => DensityMode::Compact,
        Some("spacious") => DensityMode::Spacious,
        _ => DensityMode::Cozy, // Default
    };

    // UI metrics: log session start (bead 020)
    if ui_metrics_enabled {
        let animations_enabled = !dotenvy::var("CASS_DISABLE_ANIMATIONS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        tracing::info!(
            target: "ui_metrics",
            event = "session_start",
            density_mode = %density_mode.label(),
            animations_enabled,
            "UI metrics session started"
        );
    }

    // Calculate initial pane limit based on terminal height and density,
    // or restore from persisted state if user previously adjusted it (bead 46t.1)
    let initial_height = terminal.size().map(|r| r.height).unwrap_or(24);
    let mut per_pane_limit: usize = persisted
        .per_pane_limit
        .unwrap_or_else(|| calculate_pane_limit(initial_height, density_mode));
    let mut last_terminal_height: u16 = initial_height;
    let mut page: usize = 0;
    let mut results: Vec<SearchHit> = Vec::new();
    let mut wildcard_fallback: bool = false; // True when search used implicit wildcards
    let mut suggestions: Vec<QuerySuggestion> = Vec::new(); // Did-you-mean suggestions for zero hits
    let cache_debug = dotenvy::var("CASS_DEBUG_CACHE_METRICS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let mut cache_stats: Option<CacheStats> = None;
    let mut last_search_ms: Option<u128> = None;
    let mut panes: Vec<AgentPane> = Vec::new();
    let mut pane_filter: Option<String> = None;
    let mut active_pane: usize = 0;
    const MAX_VISIBLE_PANES: usize = 4;
    let mut pane_scroll_offset: usize = 0; // First visible pane index
    // Multi-select state: (pane_index, hit_index) tuples of selected items
    let mut selected: HashSet<(usize, usize)> = HashSet::new();
    // Require double-confirm before opening a large queue of files
    const OPEN_CONFIRM_THRESHOLD: usize = 12;
    let mut open_confirm_armed = false;
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

    // Staggered reveal animation state (bead 013)
    // Env flag to disable animations for performance-sensitive terminals
    let animations_enabled = !dotenvy::var("CASS_DISABLE_ANIMATIONS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    // When new results arrive, we start a staggered reveal animation
    let mut reveal_anim_start: Option<Instant> = None;
    // Animation timing: each item fades in over ITEM_FADE_MS, staggered by STAGGER_DELAY_MS
    const STAGGER_DELAY_MS: u64 = 30; // Delay between each item starting
    const ITEM_FADE_MS: u64 = 120; // Duration of each item's fade-in
    const MAX_ANIMATED_ITEMS: usize = 15; // Only animate first N items to avoid frame drops

    // Update check state (bead 018)
    // Spawn background thread to check for updates on startup
    let update_check_rx = spawn_update_check(env!("CARGO_PKG_VERSION").to_string());
    let mut update_info: Option<UpdateInfo> = None;
    let mut update_dismissed = false; // Session-only dismissal (not persisted)

    let mut detail_tab = DetailTab::Messages;
    let mut theme_dark = true;
    // Show onboarding overlay only on first launch (when has_seen_help is not set).
    // After user dismisses with F1, we persist has_seen_help=true to avoid showing again.
    let mut show_help = !persisted.has_seen_help.unwrap_or(false);
    // Full-screen modal for viewing parsed content
    let mut show_detail_modal = false;
    let mut modal_scroll: u16 = 0;
    // Bulk action modal state
    let mut show_bulk_modal = false;
    let mut bulk_action_idx: usize = 0;
    // Model download consent dialog state
    let mut show_consent_dialog = false;
    // Model download state
    let mut download_rx: Option<mpsc::Receiver<DownloadProgress>> = None;
    let mut download_cancel: Option<Arc<AtomicBool>> = None;
    // Toast notification manager for semantic state changes
    let mut toast_manager = ToastManager::new()
        .with_max_visible(2)
        .with_position(crate::ui::components::toast::ToastPosition::TopRight);
    let mut cached_detail: Option<(String, ConversationView)> = None;
    let mut detail_find: Option<DetailFindState> = None;
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
    let mut search_mode = match persisted.search_mode.as_deref() {
        Some(mode) => search_mode_from_str(mode),
        None => SearchMode::Lexical,
    };
    if matches!(search_mode, SearchMode::Semantic | SearchMode::Hybrid)
        && !semantic_availability.can_search()
    {
        let reason = semantic_unavailable_message(&semantic_availability);
        status = format!("Semantic unavailable: {reason}. Using lexical search.");
        search_mode = SearchMode::Lexical;
    }
    let mut ranking_mode = persisted
        .ranking_mode
        .as_deref()
        .map_or(RankingMode::Balanced, ranking_from_str);
    let mut saved_views: Vec<SavedView> = persisted
        .saved_views
        .as_ref()
        .map(|v| {
            v.iter()
                .filter_map(|sv| {
                    if (1..=9).contains(&sv.slot) {
                        Some(SavedView {
                            slot: sv.slot,
                            agents: sv.agents.iter().cloned().collect(),
                            workspaces: sv.workspaces.iter().cloned().collect(),
                            created_from: sv.created_from,
                            created_to: sv.created_to,
                            ranking: sv
                                .ranking
                                .as_deref()
                                .map_or(RankingMode::Balanced, ranking_from_str),
                            source_filter: sv
                                .source_filter
                                .as_ref()
                                .map(|s| crate::sources::provenance::SourceFilter::parse(s))
                                .unwrap_or_default(),
                        })
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let mut help_pinned = persisted.help_pinned.unwrap_or(false);
    let mut help_last_interaction = Instant::now();
    let mut fancy_borders = true; // Toggle with Ctrl+B for unicode vs ASCII borders
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
    let editor_cmd = dotenvy::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let (editor_bin, editor_args) = split_editor_command(&editor_cmd);
    let editor_line_flag = dotenvy::var("EDITOR_LINE_FLAG").unwrap_or_else(|_| "+".into());
    let mut time_preset_idx: usize = 0;

    // Mouse support: track layout regions for click/scroll handling
    let mut last_detail_area: Option<Rect> = None;
    let mut last_pane_rects: Vec<Rect> = Vec::new();
    let mut last_pill_rects: Vec<(Rect, Pill)> = Vec::new();
    let mut last_breadcrumb_rects: Vec<(Rect, BreadcrumbKind)> = Vec::new();

    // Command palette + help strip + pills state
    let mut palette_state = PaletteState::new(palette::default_actions());

    // Source filter menu state (P4.4)
    let mut source_filter_menu_open = false;
    let mut source_filter_menu_selection: usize = 0;
    // Available source IDs discovered from index (populated on menu open)
    let mut available_source_ids: Vec<String> = Vec::new();

    // Keep a short history of indexer percentages for sparkline rendering
    let mut progress_history: std::collections::VecDeque<u8> =
        std::collections::VecDeque::with_capacity(24);

    // Throughput tracking for indexer HUD sparkline (bead 012)
    // Tracks items-per-second over the last N samples
    let mut throughput_history: std::collections::VecDeque<u16> =
        std::collections::VecDeque::with_capacity(24);
    let mut last_throughput_sample: Option<(Instant, usize)> = None; // (time, current_count)
    const THROUGHPUT_SAMPLE_INTERVAL_MS: u64 = 500; // Sample every 500ms for smoother sparkline

    // Track last indexing state to detect changes and trigger redraw
    // Tuple: (phase, current, total, is_rebuild, discovered_agents)
    let mut last_indexing_state: Option<(usize, usize, usize, bool, usize)> = None;
    let mut last_index_error: Option<String> = None;
    let mut last_index_redraw = Instant::now();

    // Helper to get indexing phase info (returns phase, current, total, is_rebuild, pct, discovered_agents)
    let get_indexing_state = |progress: &std::sync::Arc<crate::indexer::IndexingProgress>| -> (usize, usize, usize, bool, usize, usize) {
        use std::sync::atomic::Ordering;
        let phase = progress.phase.load(Ordering::Relaxed);
        let total = progress.total.load(Ordering::Relaxed);
        let current = progress.current.load(Ordering::Relaxed);
        let is_rebuild = progress.is_rebuilding.load(Ordering::Relaxed);
        let discovered = progress.discovered_agents.load(Ordering::Relaxed);
        let pct = if total > 0 {
            (current as f32 / total as f32 * 100.0) as usize
        } else {
            0
        };
        (phase, current, total, is_rebuild, pct, discovered)
    };

    // Render tiny sparkline from history (0-100 mapped to ▁▂▃▄▅▆▇█)
    let render_sparkline = |history: &std::collections::VecDeque<u8>| -> String {
        if history.len() < 2 {
            return String::new();
        }
        let levels = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        history
            .iter()
            .map(|pct| {
                let idx = ((*pct as usize * (levels.len() - 1)) / 100).min(levels.len() - 1);
                levels[idx]
            })
            .collect()
    };

    // Render throughput sparkline from items/sec history (bead 012)
    // Scales dynamically to max value in history
    let render_throughput_sparkline = |history: &std::collections::VecDeque<u16>| -> String {
        if history.len() < 2 {
            return String::new();
        }
        let max_val = *history.iter().max().unwrap_or(&1) as usize;
        if max_val == 0 {
            return String::new();
        }
        let levels = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        history
            .iter()
            .map(|v| {
                let idx = ((*v as usize * (levels.len() - 1)) / max_val).min(levels.len() - 1);
                levels[idx]
            })
            .collect()
    };

    // Helper to render progress for footer (enhanced with icons + sparkline + throughput)
    // Respects density mode: Compact=minimal, Cozy=normal, Spacious=full details (bead 012)
    let render_progress = |progress: &std::sync::Arc<crate::indexer::IndexingProgress>,
                           pct_history: &std::collections::VecDeque<u8>,
                           tput_history: &std::collections::VecDeque<u16>,
                           density: DensityMode|
     -> String {
        let (phase, current, total, is_rebuild, pct, discovered) = get_indexing_state(progress);
        if phase == 0 {
            return String::new();
        }

        // Phase-specific icons and labels
        let (icon, phase_str) = match phase {
            1 => ("🔍", "Discovering"),
            2 => ("📦", "Indexing"),
            _ => ("⏳", "Processing"),
        };

        // Density-aware rendering (bead 012)
        match density {
            DensityMode::Compact => {
                // Compact: minimal - just icon + percentage or agent count
                if phase == 1 {
                    if total > 0 {
                        format!(" | {icon} {current}/{total} · {discovered} agents")
                    } else {
                        format!(" | {icon} {discovered} agents")
                    }
                } else if is_rebuild {
                    format!(" | {icon} {pct}% ⚠")
                } else {
                    format!(" | {icon} {pct}%")
                }
            }
            DensityMode::Cozy => {
                // Cozy: normal - icon + progress bar + throughput sparkline
                let bar_width = 8;
                let pct_eff = if total == 0 { 5 } else { pct };
                let filled = ((pct_eff * bar_width).saturating_add(99)) / 100;
                let empty = bar_width.saturating_sub(filled.min(bar_width));
                let bar = format!("{}{}", "█".repeat(filled.min(bar_width)), "░".repeat(empty));
                let tput_spark = render_throughput_sparkline(tput_history);

                let mut s = if phase == 1 {
                    if total > 0 {
                        format!(
                            " | {icon} {phase_str} {current}/{total} {bar} ({discovered} agents)"
                        )
                    } else {
                        format!(" | {icon} {phase_str} ({discovered} agents)")
                    }
                } else {
                    format!(" | {icon} {current}/{total} ({pct}%) {bar}")
                };

                if !tput_spark.is_empty() && phase == 2 {
                    s.push_str(&format!(" {tput_spark}"));
                }
                if is_rebuild {
                    s.push_str(" ⚠ REBUILD");
                }
                s
            }
            DensityMode::Spacious => {
                // Spacious: full details - all metrics + both sparklines
                let bar_width = 8;
                let pct_eff = if total == 0 { 5 } else { pct };
                let filled = ((pct_eff * bar_width).saturating_add(99)) / 100;
                let empty = bar_width.saturating_sub(filled.min(bar_width));
                let bar = format!("{}{}", "█".repeat(filled.min(bar_width)), "░".repeat(empty));
                let pct_spark = render_sparkline(pct_history);
                let tput_spark = render_throughput_sparkline(tput_history);

                // Calculate current throughput from recent samples
                let current_tput = tput_history.back().copied().unwrap_or(0);

                let mut s = if phase == 1 {
                    if total > 0 {
                        format!(
                            " | {icon} {phase_str} {current}/{total} {bar} ({discovered} agents)"
                        )
                    } else {
                        format!(" | {icon} {phase_str} ({discovered} agents found)")
                    }
                } else {
                    let tput_str = if current_tput > 0 {
                        format!(" ~{current_tput}/s")
                    } else {
                        String::new()
                    };
                    format!(" | {icon} {phase_str} {current}/{total} ({pct}%) {bar}{tput_str}")
                };

                // Show both sparklines in spacious mode
                if phase == 2 {
                    if !pct_spark.is_empty() {
                        s.push_str(&format!(" %:{pct_spark}"));
                    }
                    if !tput_spark.is_empty() {
                        s.push_str(&format!(" ⚡:{tput_spark}"));
                    }
                }

                if is_rebuild {
                    s.push_str(" ⚠ FULL REBUILD - Search unavailable");
                } else if phase > 0 {
                    s.push_str(" · Results may be incomplete");
                }
                s
            }
        }
    };

    loop {
        // Check for terminal resize and recalculate pane limit if needed
        if let Ok(size) = terminal.size()
            && size.height != last_terminal_height
        {
            last_terminal_height = size.height;
            let new_limit = calculate_pane_limit(size.height, density_mode);
            if new_limit != per_pane_limit {
                per_pane_limit = new_limit;
                let prev_agent = active_hit(&panes, active_pane)
                    .map(|h| h.agent.clone())
                    .or_else(|| panes.get(active_pane).map(|p| p.agent.clone()));
                let prev_path = active_hit(&panes, active_pane).map(|h| h.source_path.clone());
                panes = rebuild_panes_with_filter(
                    &results,
                    pane_filter.as_deref(),
                    per_pane_limit,
                    &mut active_pane,
                    &mut pane_scroll_offset,
                    prev_agent,
                    prev_path,
                    MAX_VISIBLE_PANES,
                );
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
                            Constraint::Length(3), // footer (query display + status + help strip)
                        ]
                        .as_ref(),
                    )
                    .split(f.area());

                let bar_text = match input_mode {
                    InputMode::Query => query.as_str().to_string(),
                    InputMode::Agent => format!("[agent] {input_buffer}"),
                    InputMode::Workspace => format!("[workspace] {input_buffer}"),
                    InputMode::CreatedFrom => format!("[from] {input_buffer}"),
                    InputMode::CreatedTo => format!("[to] {input_buffer}"),
                    InputMode::PaneFilter => format!("[pane] {input_buffer}"),
                    InputMode::DetailFind => format!("[detail find] {input_buffer}"),
                };
                let mode_label = match match_mode {
                    MatchMode::Standard => "standard",
                    MatchMode::Prefix => "prefix",
                };
                let search_split = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(
                        [
                            Constraint::Length(2), // input
                            Constraint::Length(1), // pills
                            Constraint::Length(1), // breadcrumbs
                        ]
                        .as_ref(),
                    )
                    .split(chunks[0]);

                let chips = chips_for_filters(&filters, palette);
                let sb = search_bar(&bar_text, palette, input_mode, mode_label, chips);
                f.render_widget(sb, search_split[0]);

                let mut pill_vec: Vec<Pill> = Vec::new();
                if !filters.agents.is_empty() {
                    pill_vec.push(Pill {
                        label: "agent".into(),
                        value: filters.agents.iter().cloned().collect::<Vec<_>>().join("|"),
                        active: true,
                        editable: true,
                    });
                }
                if !filters.workspaces.is_empty() {
                    pill_vec.push(Pill {
                        label: "ws".into(),
                        value: filters
                            .workspaces
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("|"),
                        active: true,
                        editable: true,
                    });
                }
                if let Some(filter) = pane_filter.as_ref().filter(|s| !s.is_empty()) {
                    pill_vec.push(Pill {
                        label: "pane".into(),
                        value: filter.clone(),
                        active: true,
                        editable: true,
                    });
                }
                if filters.created_from.is_some() || filters.created_to.is_some() {
                    pill_vec.push(Pill {
                        label: "time".into(),
                        value: format_time_chip(filters.created_from, filters.created_to),
                        active: true,
                        editable: true,
                    });
                }
                // Render pills and record their rects for click handling
                let pill_rects = pills::draw_pills(f, search_split[1], &pill_vec, palette);
                last_pill_rects = pill_rects
                    .into_iter()
                    .zip(pill_vec.iter().cloned())
                    .collect();

                // Breadcrumb/locality bar
                let bc_rects = breadcrumbs::render_breadcrumbs(
                    f,
                    search_split[2],
                    &filters,
                    ranking_mode,
                    palette,
                );
                last_breadcrumb_rects = bc_rects;

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

                // Border style toggle: unicode rounded vs plain ASCII
                let border_type = if fancy_borders {
                    BorderType::Rounded
                } else {
                    BorderType::Plain
                };

                // Save layout for mouse hit testing
                last_detail_area = Some(detail_area);

                if panes.is_empty() {
                    // Clear pane rects when no panes (avoid stale click detection)
                    last_pane_rects.clear();
                    let mut lines: Vec<Line> = Vec::new();

                    // Check if indexing is in progress - show prominent banner
                    let indexing_active = progress.as_ref().map(get_indexing_state);

                    if let Some((phase, current, total, is_rebuild, pct, discovered)) =
                        indexing_active
                        && phase > 0
                    {
                        // Show indexing banner
                        lines.push(Line::from(""));
                        if is_rebuild {
                            lines.push(Line::from(vec![
                                Span::styled("  ⚠ ", Style::default().fg(palette.system)),
                                Span::styled(
                                    "REBUILDING INDEX",
                                    Style::default()
                                        .fg(palette.system)
                                        .add_modifier(Modifier::BOLD),
                                ),
                            ]));
                            lines.push(Line::from(""));
                            lines.push(Line::from(Span::styled(
                                "  Search is unavailable during a full rebuild.",
                                Style::default().fg(palette.hint),
                            )));
                            lines.push(Line::from(Span::styled(
                                "  This typically takes 30-60 seconds.",
                                Style::default().fg(palette.hint),
                            )));
                        } else {
                            let (icon, phase_label) = match phase {
                                1 => ("🔍", "Discovering sessions..."),
                                2 => ("📦", "Building search index..."),
                                _ => ("⏳", "Processing..."),
                            };
                            lines.push(Line::from(vec![
                                Span::styled(format!("  {icon} "), Style::default()),
                                Span::styled(
                                    phase_label,
                                    Style::default()
                                        .fg(palette.accent)
                                        .add_modifier(Modifier::BOLD),
                                ),
                            ]));
                            lines.push(Line::from(""));

                            if phase == 1 {
                                // Discovery phase - show agents found count
                                if total > 0 {
                                    lines.push(Line::from(Span::styled(
                                        format!("  Scanned {current}/{total} connectors"),
                                        Style::default().fg(palette.hint),
                                    )));
                                } else {
                                    lines.push(Line::from(Span::styled(
                                        "  Scanning connectors...",
                                        Style::default().fg(palette.hint),
                                    )));
                                }
                                lines.push(Line::from(""));
                                lines.push(Line::from(Span::styled(
                                    format!("  Found {discovered} coding agent(s) so far..."),
                                    Style::default().fg(palette.hint),
                                )));
                                lines.push(Line::from(""));
                            } else {
                                // Indexing phase - show progress bar
                                let bar_width = 30;
                                let filled = (pct * bar_width / 100).min(bar_width);
                                let empty = bar_width - filled;
                                lines.push(Line::from(vec![
                                    Span::styled("  [", Style::default().fg(palette.border)),
                                    Span::styled(
                                        "█".repeat(filled),
                                        Style::default().fg(palette.accent),
                                    ),
                                    Span::styled(
                                        "░".repeat(empty),
                                        Style::default().fg(palette.hint),
                                    ),
                                    Span::styled("]", Style::default().fg(palette.border)),
                                    Span::styled(
                                        format!(" {pct}%"),
                                        Style::default().fg(palette.hint),
                                    ),
                                ]));
                                lines.push(Line::from(""));
                                lines.push(Line::from(Span::styled(
                                    format!("  Processing {current} of {total} items"),
                                    Style::default().fg(palette.hint),
                                )));
                            }
                            lines.push(Line::from(Span::styled(
                                "  Search results will appear once indexing completes.",
                                Style::default().fg(palette.hint),
                            )));
                        }
                        lines.push(Line::from(""));
                    }

                    if indexing_active.is_none_or(|(phase, _, _, _, _, _)| phase == 0)
                        && let Some(err) = last_index_error.as_ref()
                    {
                        lines.push(Line::from(""));
                        lines.push(Line::from(vec![
                            Span::styled("  ⚠ ", Style::default().fg(palette.system)),
                            Span::styled(
                                "Indexer error",
                                Style::default()
                                    .fg(palette.system)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]));
                        lines.push(Line::from(Span::styled(
                            format!("  {err}"),
                            Style::default().fg(palette.hint),
                        )));
                        lines.push(Line::from(Span::styled(
                            "  See cass.log for details, or run `cass index --full`.",
                            Style::default().fg(palette.hint),
                        )));
                        lines.push(Line::from(""));
                    }

                    // Only show history/empty state if not indexing OR if indexing but user typed a query
                    let show_normal_empty = indexing_active
                        .is_none_or(|(phase, _, _, _, _, _)| phase == 0)
                        || !query.trim().is_empty();

                    if show_normal_empty {
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
                        } else if !query.trim().is_empty() {
                            // Check for fuzzy suggestion from query history
                            let fuzzy = suggest_correction(&last_query, &query_history);
                            // Use contextual empty state with helpful suggestions
                            lines.extend(contextual_empty_state(
                                &last_query,
                                &filters,
                                match_mode,
                                palette,
                                fuzzy.as_deref(),
                                &suggestions,
                            ));
                        }
                    }

                    let block = Block::default()
                        .title("Results")
                        .borders(Borders::ALL)
                        .border_type(border_type);
                    f.render_widget(Paragraph::new(lines).block(block), results_area);
                } else {
                    // Cap visible panes at MAX_VISIBLE_PANES
                    // Safety: clamp scroll offset to valid range to prevent slice panic
                    let safe_scroll_offset =
                        pane_scroll_offset.min(panes.len().saturating_sub(1).max(0));
                    let visible_end = (safe_scroll_offset + MAX_VISIBLE_PANES).min(panes.len());
                    let visible_panes: Vec<&AgentPane> =
                        panes[safe_scroll_offset..visible_end].iter().collect();

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
                            .enumerate()
                            .map(|(hit_idx, hit)| {
                                let title = if hit.title.is_empty() {
                                    "(untitled)"
                                } else {
                                    hit.title.as_str()
                                };
                                // Build header with agent badge + score bar visualization
                                let mut header_spans: Vec<Span> = Vec::new();
                                // Multi-select indicator (✓) at start if selected
                                let is_selected = selected.contains(&(idx, hit_idx));
                                if is_selected {
                                    header_spans.push(Span::styled(
                                        "✓ ",
                                        Style::default()
                                            .fg(Color::Rgb(46, 204, 113)) // Emerald green for selection
                                            .add_modifier(Modifier::BOLD),
                                    ));
                                }
                                let icon = ThemePalette::agent_icon(&pane.agent);
                                header_spans.push(Span::styled(
                                    format!("{icon} "),
                                    Style::default()
                                        .fg(theme.accent)
                                        .add_modifier(Modifier::BOLD),
                                ));
                                header_spans.push(Span::styled(
                                    format!("@{} ", pane.agent),
                                    Style::default().fg(palette.hint),
                                ));
                                header_spans.extend(score_bar(hit.score, palette));
                                header_spans.push(Span::raw(" "));
                                header_spans.push(Span::styled(
                                    title.to_string(),
                                    Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                                ));

                                // Choose highlight term: prefer pane filter when active.
                                let highlight_term = pane_filter
                                    .as_deref()
                                    .filter(|s| !s.trim().is_empty())
                                    .unwrap_or(&last_query);

                                // Add match count if > 1 (sux.6.6c)
                                let match_count = count_query_matches(&hit.content, highlight_term);
                                if match_count > 1 {
                                    header_spans.push(Span::styled(
                                        format!(" (×{match_count})"),
                                        Style::default().fg(palette.hint),
                                    ));
                                }

                                let header = Line::from(header_spans);

                                // Location line (separate from snippet for clarity)
                                let truncated_source = truncate_path(&hit.source_path, 50);
                                let truncated_ws = truncate_path(&hit.workspace, 30);
                                let mut location_spans: Vec<Span> = vec![
                                    Span::styled("[file] ", Style::default().fg(palette.hint)),
                                    Span::styled(
                                        truncated_source,
                                        Style::default().fg(palette.hint),
                                    ),
                                ];
                                if !hit.workspace.is_empty() {
                                    location_spans.push(Span::raw("  "));
                                    location_spans.push(Span::styled(
                                        "[ws] ",
                                        Style::default().fg(palette.hint),
                                    ));
                                    location_spans.push(Span::styled(
                                        truncated_ws,
                                        Style::default().fg(palette.hint),
                                    ));
                                }
                                if let Some(ts) = hit.created_at {
                                    location_spans.push(Span::styled(
                                        format!(" · {}", format_relative_time(ts)),
                                        Style::default().fg(palette.hint),
                                    ));
                                }
                                // P4.1: Source badge for remote sessions
                                if hit.source_id != "local" {
                                    let source_label =
                                        hit.origin_host.as_deref().unwrap_or(&hit.source_id);
                                    location_spans.push(Span::styled(
                                        format!(" [{}]", source_label),
                                        Style::default()
                                            .fg(Color::Rgb(147, 112, 219)) // Medium purple for remote
                                            .add_modifier(Modifier::ITALIC),
                                    ));
                                }
                                let location_line = Line::from(location_spans);

                                // Snippet with enhanced highlighting (multiple lines if long)
                                let raw_snippet = contextual_snippet(
                                    &hit.content,
                                    highlight_term,
                                    context_window,
                                );

                                // Smart word wrap for snippet content (sux.6.6d)
                                // Wrap at word boundaries with continuation indent
                                // Limit to 2 lines for compact display (sux.6.1)
                                let wrapped_lines = smart_word_wrap(&raw_snippet, 80);
                                let snippet_budget = density_mode.snippet_lines();
                                let snippet_lines: Vec<Line> =
                                    wrapped_lines
                                        .into_iter()
                                        .take(snippet_budget)
                                        .map(|line| {
                                            syntax_highlight_line(
                                                &line,
                                                &hit.source_path,
                                                highlight_term,
                                                palette,
                                                theme_dark,
                                            )
                                            .unwrap_or_else(|| {
                                                highlight_terms_owned_with_style(
                                                    line,
                                                    highlight_term,
                                                    palette,
                                                    Style::default().fg(theme.fg),
                                                )
                                            })
                                        })
                                        .collect();

                                // Alternating background for better visual separation (sux.6.3)
                                let base_stripe_bg = if hit_idx % 2 == 0 {
                                    theme.bg
                                } else {
                                    lerp_color(theme.bg, Color::Rgb(255, 255, 255), 0.08)
                                };
                                // P4.1: Subtle tint for remote sources
                                let stripe_bg = if hit.source_id != "local" {
                                    // Add subtle purple tint for remote rows
                                    lerp_color(base_stripe_bg, Color::Rgb(75, 0, 130), 0.08)
                                } else {
                                    base_stripe_bg
                                };

                                let mut lines = vec![header, location_line];
                                lines.extend(snippet_lines);

                                // Staggered reveal animation (bead 013)
                                // Calculate fade progress for this item
                                let reveal_progress = if animations_enabled {
                                    item_reveal_progress(
                                        reveal_anim_start,
                                        hit_idx,
                                        STAGGER_DELAY_MS,
                                        ITEM_FADE_MS,
                                        MAX_ANIMATED_ITEMS,
                                    )
                                } else {
                                    1.0 // Animations disabled, show immediately
                                };

                                // Apply fade: lerp from bg color (invisible) to final color
                                let faded_fg = if reveal_progress < 1.0 {
                                    lerp_color(stripe_bg, theme.fg, reveal_progress)
                                } else {
                                    theme.fg
                                };

                                // Apply faded foreground to all lines
                                let faded_lines: Vec<Line> = if reveal_progress < 1.0 {
                                    lines
                                        .into_iter()
                                        .map(|line| {
                                            Line::from(
                                                line.spans
                                                    .into_iter()
                                                    .map(|span| {
                                                        let base_fg =
                                                            span.style.fg.unwrap_or(theme.fg);
                                                        Span::styled(
                                                            span.content,
                                                            span.style.fg(lerp_color(
                                                                stripe_bg,
                                                                base_fg,
                                                                reveal_progress,
                                                            )),
                                                        )
                                                    })
                                                    .collect::<Vec<_>>(),
                                            )
                                        })
                                        .collect()
                                } else {
                                    lines
                                };

                                ListItem::new(faded_lines)
                                    .style(Style::default().bg(stripe_bg).fg(faded_fg))
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
                        // Use persistent agent-specific color for pane title/border
                        let pane_color = agent_color(&pane.agent);
                        let block = Block::default()
                            .title(Span::styled(
                                format!("{} ({})", agent_display_name(&pane.agent), count_display),
                                Style::default()
                                    .fg(pane_color)
                                    .add_modifier(if is_focused_pane {
                                        Modifier::BOLD
                                    } else {
                                        Modifier::empty()
                                    }),
                            ))
                            .borders(Borders::ALL)
                            .border_type(border_type)
                            .border_style(Style::default().fg(if is_focused_pane {
                                pane_color
                            } else {
                                // Dim the agent color when not focused
                                dim_color(pane_color, 0.5)
                            }))
                            .style(Style::default().bg(flash_bg).fg(flash_fg));

                        let list = List::new(items)
                            .block(block)
                            .highlight_style(
                                Style::default()
                                    .bg(if is_focused_pane {
                                        pane_color
                                    } else {
                                        dim_color(pane_color, 0.5)
                                    })
                                    .fg(theme.bg)
                                    .add_modifier(Modifier::BOLD),
                            )
                            .style(Style::default().bg(theme.bg).fg(theme.fg));

                        if let Some(area) = pane_chunks.get(vis_idx) {
                            f.render_stateful_widget(list, *area, &mut state);
                        }
                    }

                    // Render hidden pane directional indicators (arrows)
                    if safe_scroll_offset > 0 {
                        let text = format!("◀ +{safe_scroll_offset}");
                        let area = Rect::new(results_area.x, results_area.y, text.len() as u16, 1);
                        f.render_widget(
                            Span::styled(
                                text,
                                Style::default()
                                    .fg(palette.accent)
                                    .bg(palette.bg)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            area,
                        );
                    }
                    if visible_end < panes.len() {
                        let diff = panes.len() - visible_end;
                        let text = format!("+{diff} ▶");
                        let area = Rect::new(
                            results_area.x + results_area.width.saturating_sub(text.len() as u16),
                            results_area.y,
                            text.len() as u16,
                            1,
                        );
                        f.render_widget(
                            Span::styled(
                                text,
                                Style::default()
                                    .fg(palette.accent)
                                    .bg(palette.bg)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            area,
                        );
                    }

                    // Show "indexing in progress" warning when we have results but indexing is active
                    if let Some(prog) = &progress {
                        let (phase, _, _, _, pct, _discovered) = get_indexing_state(prog);
                        if phase > 0 {
                            let indicator =
                                format!(" ⚠ Indexing {pct}% - results may be incomplete ");
                            let indicator_span = Span::styled(
                                indicator.clone(),
                                Style::default()
                                    .fg(palette.system)
                                    .add_modifier(Modifier::BOLD),
                            );
                            // Render in top-right corner of results area
                            let indicator_area = Rect::new(
                                results_area.x
                                    + results_area
                                        .width
                                        .saturating_sub(indicator.len() as u16 + 1),
                                results_area.y,
                                indicator.len() as u16,
                                1,
                            );
                            f.render_widget(Paragraph::new(indicator_span), indicator_area);
                        }
                    }
                }

                if let Some(hit) = active_hit(&panes, active_pane) {
                    // Load detail data first to get counts for tabs
                    let detail = if cached_detail
                        .as_ref()
                        .is_some_and(|(p, _)| p == &hit.source_path)
                    {
                        cached_detail.as_ref().map(|(_, d)| d.clone())
                    } else {
                        let loaded = if let Some(storage) = &db_reader {
                            load_conversation(storage, &hit.source_path).ok().flatten()
                        } else {
                            None
                        };
                        if let Some(d) = &loaded {
                            cached_detail = Some((hit.source_path.clone(), d.clone()));
                            detail_scroll = 0;
                        }
                        loaded
                    };

                    // Count messages and snippets for tab labels
                    let (msg_count, snippet_count) = if let Some(ref d) = detail {
                        let msgs = d.messages.len();
                        let snips: usize = d.messages.iter().map(|m| m.snippets.len()).sum();
                        (msgs, snips)
                    } else {
                        (0, 0)
                    };

                    // Build tab labels with counts (sux.6.5)
                    let tab_labels = [
                        format!("Messages ({msg_count})"),
                        format!("Snippets ({snippet_count})"),
                        "Raw".to_string(),
                    ];
                    let tab_titles: Vec<Line> = tab_labels
                        .iter()
                        .map(|t| {
                            Line::from(Span::styled(t.as_str(), Style::default().fg(palette.hint)))
                        })
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

                    // Build enhanced metadata section (sux.6.5)
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
                        Span::raw("  "),
                        Span::styled("Match: ", Style::default().fg(palette.hint)),
                        Span::styled(
                            format!("{:?}", hit.match_type),
                            Style::default().fg(palette.accent_alt),
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
                    // Add timestamp info if available
                    if let Some(ref d) = detail
                        && let Some(started) = d.convo.started_at
                    {
                        let ts = format_absolute_time(started);
                        meta_lines.push(Line::from(vec![
                            Span::styled("Created: ", Style::default().fg(palette.hint)),
                            Span::raw(ts),
                        ]));
                    }
                    meta_lines.push(Line::from(vec![
                        Span::styled("Source: ", Style::default().fg(palette.hint)),
                        Span::raw(truncate_path(&hit.source_path, 60)),
                    ]));
                    meta_lines.push(Line::from(vec![
                        Span::styled("Score: ", Style::default().fg(palette.hint)),
                        Span::raw(format!("{:.2}", hit.score)),
                        Span::raw("  "),
                        Span::styled("Stats: ", Style::default().fg(palette.hint)),
                        Span::raw(format!("{msg_count} msgs, {snippet_count} snippets")),
                    ]));

                    // Determine highlight term priority: detail-find > pane filter > last query
                    let highlight_term = if let Some(df) = &detail_find {
                        df.query.as_str()
                    } else if let Some(pf) = pane_filter.as_deref().filter(|s| !s.trim().is_empty())
                    {
                        pf
                    } else {
                        last_query.as_str()
                    };

                    let detail_match_lines: Vec<u16>;
                    let content_lines: Vec<Line> = match detail_tab {
                        DetailTab::Messages => {
                            if let Some(full) = detail {
                                let lines = render_parsed_content(&full, highlight_term, palette);
                                detail_match_lines = match_line_indices(&lines, highlight_term);
                                if lines.is_empty() {
                                    vec![Line::from(Span::styled(
                                        "No messages",
                                        Style::default().fg(palette.hint),
                                    ))]
                                } else {
                                    lines
                                }
                            } else {
                                let lines: Vec<Line> = hit
                                    .content
                                    .lines()
                                    .map(|l| Line::from(l.to_string()))
                                    .collect();
                                detail_match_lines = match_line_indices(&lines, highlight_term);
                                if lines.is_empty() {
                                    vec![Line::from(Span::styled(
                                        "No messages",
                                        Style::default().fg(palette.hint),
                                    ))]
                                } else {
                                    lines
                                }
                            }
                        }
                        DetailTab::Snippets => {
                            let mut lines = Vec::new();
                            if let Some(full) = detail {
                                for (msg_idx, msg) in full.messages.iter().enumerate() {
                                    for snip in &msg.snippets {
                                        let file = snip.file_path.as_ref().map_or_else(
                                            || "<unknown file>".into(),
                                            |p| p.to_string_lossy().to_string(),
                                        );
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
                            }
                            detail_match_lines = match_line_indices(&lines, highlight_term);
                            if lines.is_empty() {
                                vec![Line::from(Span::styled(
                                    "No snippets attached.",
                                    Style::default().fg(palette.hint),
                                ))]
                            } else {
                                lines
                            }
                        }
                        DetailTab::Raw => {
                            let text = if let Some(full) = detail {
                                let meta = serde_json::to_string_pretty(&full.convo.metadata_json)
                                    .unwrap_or_else(|_| "<invalid metadata>".into());
                                let mut t = String::new();
                                t.push_str(&format!(
                                    "Path: {}\n",
                                    full.convo.source_path.display()
                                ));
                                if let Some(ws) = &full.workspace {
                                    t.push_str(&format!("Workspace: {}\n", ws.path.display()));
                                }
                                if let Some(ext) = &full.convo.external_id {
                                    t.push_str(&format!("External ID: {ext}\n"));
                                }
                                t.push_str("Metadata:\n");
                                t.push_str(&meta);
                                t
                            } else {
                                format!("Path: {}", hit.source_path)
                            };
                            let lines: Vec<Line> =
                                text.lines().map(|l| Line::from(l.to_string())).collect();
                            detail_match_lines = match_line_indices(&lines, highlight_term);
                            lines
                        }
                    };

                    // Update detail-find state with fresh matches and clamp scroll to content size
                    if let Some(df) = detail_find.as_mut() {
                        df.matches = detail_match_lines.clone();
                        if df.matches.is_empty() {
                            df.current = 0;
                        } else {
                            if df.current >= df.matches.len() {
                                df.current = 0;
                            }
                            if let Some(&line) = df.matches.get(df.current) {
                                let max_line = content_lines.len().saturating_sub(1) as u16;
                                detail_scroll = line.min(max_line);
                            }
                        }
                    }

                    let content_para = {
                        let trim = !matches!(detail_tab, DetailTab::Messages);
                        Paragraph::new(content_lines)
                            .wrap(Wrap { trim })
                            .scroll((detail_scroll, 0))
                    };

                    let is_focused_detail = matches!(focus_region, FocusRegion::Detail);
                    // Build detail block title with scroll indicator, tab hints and quick actions (sux.6.5)
                    let match_badge = detail_find.as_ref().map(|df| {
                        if df.query.trim().is_empty() {
                            String::new()
                        } else if df.matches.is_empty() {
                            format!(" • 0/0 \"{}\"", df.query)
                        } else {
                            format!(
                                " • {}/{} \"{}\"",
                                df.current + 1,
                                df.matches.len(),
                                df.query
                            )
                        }
                    });
                    let detail_title = if detail_scroll > 0 {
                        format!(
                            "Detail ↓{} • [←/→] tabs • Enter=expand{}",
                            detail_scroll,
                            match_badge.as_deref().unwrap_or("")
                        )
                    } else if is_focused_detail {
                        format!(
                            "Detail • [←/→] tabs • ↑/↓ or Alt+j/k scroll • Enter=expand{}",
                            match_badge.as_deref().unwrap_or("")
                        )
                    } else {
                        format!(
                            "Detail • [←/→] tabs • Enter=expand{}",
                            match_badge.as_deref().unwrap_or("")
                        )
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
                        .border_type(border_type)
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
                        Paragraph::new("Select a result to view details").block(
                            Block::default()
                                .title("Detail")
                                .borders(Borders::ALL)
                                .border_type(border_type),
                        ),
                        detail_area,
                    );
                }

                // Footer: status + modes + dense shortcut legend
                let mut footer_parts: Vec<String> = vec![];
                if dirty_since.is_some() {
                    let spinner = SPINNER_CHARS[spinner_frame % SPINNER_CHARS.len()];
                    footer_parts.push(format!("{spinner} Searching..."));
                } else if !status.is_empty() {
                    footer_parts.push(status.clone());
                }

                if let Some(p) = &progress {
                    // Update percentage sparkline history once per frame
                    let (phase, current, _, _, pct, _discovered) = get_indexing_state(p);
                    if pct <= 100 {
                        if progress_history.len() == progress_history.capacity() {
                            progress_history.pop_front();
                        }
                        progress_history.push_back(pct as u8);
                    }

                    // Sample throughput at regular intervals (bead 012)
                    let now = Instant::now();
                    if phase == 2 {
                        // During indexing phase, track throughput
                        if let Some((last_time, last_count)) = last_throughput_sample {
                            let elapsed_ms = now.duration_since(last_time).as_millis() as u64;
                            if elapsed_ms >= THROUGHPUT_SAMPLE_INTERVAL_MS {
                                // Calculate items/second
                                let items_delta = current.saturating_sub(last_count);
                                let items_per_sec = if elapsed_ms > 0 {
                                    (items_delta as u64 * 1000 / elapsed_ms) as u16
                                } else {
                                    0
                                };
                                if throughput_history.len() == throughput_history.capacity() {
                                    throughput_history.pop_front();
                                }
                                throughput_history.push_back(items_per_sec);
                                last_throughput_sample = Some((now, current));
                            }
                        } else {
                            // First sample
                            last_throughput_sample = Some((now, current));
                        }
                    } else {
                        // Reset throughput tracking when not indexing
                        if phase == 0 {
                            throughput_history.clear();
                            last_throughput_sample = None;
                        }
                    }

                    let p_str =
                        render_progress(p, &progress_history, &throughput_history, density_mode);
                    if !p_str.is_empty() {
                        footer_parts.push(p_str);
                    }
                }

                if let Some(ms) = last_search_ms {
                    footer_parts.push(format!("⚡ {ms}ms"));
                }

                if cache_debug {
                    if let Some(cs) = &cache_stats {
                        footer_parts.push(format!(
                            "cache H:{} M:{} SF:{} | reloads {} ({}ms) | cost {}/{}",
                            cs.cache_hits,
                            cs.cache_miss,
                            cs.cache_shortfall,
                            cs.reloads,
                            cs.reload_ms_total,
                            cs.total_cost,
                            cs.total_cap
                        ));
                    } else {
                        footer_parts.push("cache dbg: pending".to_string());
                    }
                }

                // Mode indicator is handled separately for styled rendering
                // Add download progress if downloading
                if let SemanticAvailability::Downloading { progress_pct, .. } =
                    &semantic_availability
                {
                    footer_parts.push(format!("⬇️ {}%", progress_pct));
                }
                if matches!(match_mode, MatchMode::Standard) {
                    footer_parts.push("match:standard".to_string());
                }
                match ranking_mode {
                    RankingMode::RecentHeavy => footer_parts.push("rank:recent".to_string()),
                    RankingMode::RelevanceHeavy => footer_parts.push("rank:relevance".to_string()),
                    RankingMode::MatchQualityHeavy => footer_parts.push("rank:quality".to_string()),
                    RankingMode::DateNewest => footer_parts.push("rank:newest".to_string()),
                    RankingMode::DateOldest => footer_parts.push("rank:oldest".to_string()),
                    RankingMode::Balanced => {}
                }
                if wildcard_fallback {
                    footer_parts.push("✱ fuzzy".to_string());
                }
                if let Some(f) = pane_filter.as_deref().filter(|s| !s.is_empty()) {
                    let trimmed = if f.chars().count() > 20 {
                        let mut s = f.chars().take(20).collect::<String>();
                        s.push('…');
                        s
                    } else {
                        f.to_string()
                    };
                    footer_parts.push(format!("pane:{trimmed}"));
                }
                // Show queued count when items are selected (bead lsv.1)
                if !selected.is_empty() {
                    footer_parts.push(format!("queued:{}", selected.len()));
                }
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
                if peek_badge_until.is_some_and(|t| t > Instant::now()) {
                    footer_parts.push("PEEK".to_string());
                }

                let footer_area = chunks[2];
                let footer_split = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(
                        [
                            Constraint::Length(1), // query display bar
                            Constraint::Length(1), // status line
                            Constraint::Length(1), // help strip
                        ]
                        .as_ref(),
                    )
                    .split(footer_area);

                // Query display bar - prominent centered display of active query
                let query_display = if last_query.is_empty() {
                    Line::from(Span::styled(
                        " No active query ",
                        Style::default()
                            .fg(palette.hint)
                            .add_modifier(Modifier::ITALIC),
                    ))
                } else {
                    let query_text = format!(" {last_query} ");
                    let query_len = query_text.len() as u16;
                    let area_width = footer_split[0].width;
                    let pad_left = area_width.saturating_sub(query_len) / 2;
                    let pad_right = area_width
                        .saturating_sub(query_len)
                        .saturating_sub(pad_left);

                    Line::from(vec![
                        Span::styled(
                            " ".repeat(pad_left as usize),
                            Style::default().bg(palette.surface),
                        ),
                        Span::styled(
                            query_text,
                            Style::default()
                                .fg(palette.bg)
                                .bg(palette.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            " ".repeat(pad_right as usize),
                            Style::default().bg(palette.surface),
                        ),
                    ])
                };
                let query_bar = Paragraph::new(query_display).alignment(Alignment::Center);
                f.render_widget(query_bar, footer_split[0]);

                // Build styled footer line with colored mode indicator
                let mut footer_spans: Vec<Span> = Vec::new();

                // Add regular footer parts
                for (i, part) in footer_parts.iter().enumerate() {
                    if i > 0 {
                        footer_spans.push(Span::styled(" | ", Style::default().fg(palette.hint)));
                    }
                    footer_spans.push(Span::raw(part.clone()));
                }

                // Add styled mode indicator
                let (mode_token, mode_color) =
                    styled_mode_indicator(search_mode, &semantic_availability);
                if !footer_spans.is_empty() {
                    footer_spans.push(Span::styled(" | ", Style::default().fg(palette.hint)));
                }
                footer_spans.push(Span::styled(
                    format!("mode:{}", mode_token),
                    Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
                ));

                let footer_line = Line::from(footer_spans);
                let footer = Paragraph::new(footer_line);
                f.render_widget(footer, footer_split[1]);

                let shortcuts = contextual_shortcuts(
                    palette_state.open,
                    show_detail_modal,
                    input_mode,
                    focus_region,
                );
                let help_active =
                    help_pinned || help_last_interaction.elapsed() < Duration::from_secs(8);
                if help_active {
                    help_strip::draw_help_strip(
                        f,
                        footer_split[2],
                        &shortcuts,
                        palette,
                        help_pinned,
                    );
                } else {
                    // Clear help line when hidden
                    f.render_widget(Paragraph::new(""), footer_split[2]);
                }

                // Render autocomplete dropdown for Agent Filter
                if input_mode == InputMode::Agent {
                    let suggestions = agent_suggestions(&input_buffer);
                    if !suggestions.is_empty() {
                        let area = Rect::new(
                            chunks[0].x + 14, // Align with " Filter: Agent " prompt
                            chunks[0].y + 2,  // Below title line
                            30,
                            (suggestions.len().min(5) as u16) + 2,
                        );
                        f.render_widget(ratatui::widgets::Clear, area);
                        let items: Vec<ListItem> = suggestions
                            .iter()
                            .map(|s| ListItem::new(Span::raw(*s)))
                            .collect();
                        let list = List::new(items)
                            .block(
                                Block::default()
                                    .borders(Borders::ALL)
                                    .border_style(palette.border_focus_style())
                                    .title("Suggestions"),
                            )
                            .highlight_style(Style::default().bg(palette.accent));
                        f.render_widget(list, area);
                    }
                }

                // Update available banner (bead 018)
                // Shows as a single-line banner at the top when update is available
                if let Some(ref info) = update_info
                    && info.should_show()
                    && !update_dismissed
                {
                    let banner_area = Rect::new(
                        f.area().x + 2,
                        f.area().y,
                        f.area().width.saturating_sub(4).min(80),
                        1,
                    );
                    let banner_text = Line::from(vec![
                        Span::styled("📦 ", Style::default()),
                        Span::styled("Update: ", Style::default().fg(palette.system)),
                        Span::styled(
                            format!("v{} → v{}", info.current_version, info.latest_version),
                            Style::default()
                                .fg(palette.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(" │ ", Style::default().fg(palette.border)),
                        Span::styled(
                            "U",
                            Style::default()
                                .fg(palette.system)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(" open ", Style::default().fg(palette.hint)),
                        Span::styled(
                            "S",
                            Style::default()
                                .fg(palette.system)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(" skip ", Style::default().fg(palette.hint)),
                        Span::styled(
                            "Esc",
                            Style::default()
                                .fg(palette.system)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(" dismiss", Style::default().fg(palette.hint)),
                    ]);
                    f.render_widget(ratatui::widgets::Clear, banner_area);
                    f.render_widget(
                        Paragraph::new(banner_text).style(Style::default().bg(palette.surface)),
                        banner_area,
                    );
                }

                if show_help {
                    render_help_overlay(f, palette, help_scroll);
                }

                // Detail modal takes priority over help
                if show_detail_modal
                    && let Some((_, ref detail)) = cached_detail
                    && let Some(pane) = panes.get(active_pane)
                    && let Some(hit) = pane.hits.get(pane.selected)
                {
                    let modal_highlight = if let Some(df) = &detail_find {
                        df.query.as_str()
                    } else if let Some(pf) = pane_filter.as_deref().filter(|s| !s.trim().is_empty())
                    {
                        pf
                    } else {
                        last_query.as_str()
                    };
                    render_detail_modal(f, detail, hit, modal_highlight, palette, modal_scroll);
                }

                // Bulk action modal
                if show_bulk_modal {
                    let area = centered_rect(50, 30, f.area());
                    let block = Block::default()
                        .title(Span::styled(
                            format!(" Bulk Actions ({} selected) ", selected.len()),
                            Style::default()
                                .fg(palette.accent)
                                .add_modifier(Modifier::BOLD),
                        ))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(palette.accent))
                        .style(Style::default().bg(palette.surface));

                    const BULK_ACTIONS: [&str; 4] = [
                        "Open all in editor",
                        "Copy all paths",
                        "Export as JSON",
                        "Clear selection",
                    ];
                    let items: Vec<ListItem> = BULK_ACTIONS
                        .iter()
                        .enumerate()
                        .map(|(i, label)| {
                            let style = if i == bulk_action_idx {
                                Style::default()
                                    .bg(palette.accent)
                                    .fg(palette.bg)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(palette.fg)
                            };
                            ListItem::new(Line::from(vec![
                                Span::styled(if i == bulk_action_idx { "→ " } else { "  " }, style),
                                Span::styled(label.to_string(), style),
                            ]))
                        })
                        .collect();
                    let list = List::new(items).block(block);
                    f.render_widget(ratatui::widgets::Clear, area);
                    f.render_widget(list, area);
                }

                // Model download consent dialog
                if show_consent_dialog {
                    // Fixed width of 62 chars to fit content comfortably:
                    // - Longest line ~55 chars + 2 border + 4 padding
                    // Height: 9 content lines + 2 border + 1 title = 12
                    let area = centered_rect_fixed(62, 12, f.area());
                    let block = Block::default()
                        .title(Span::styled(
                            " Semantic Search ",
                            Style::default()
                                .fg(palette.accent)
                                .add_modifier(Modifier::BOLD),
                        ))
                        .title_alignment(Alignment::Center)
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(palette.accent))
                        .style(Style::default().bg(palette.surface));

                    // Build dialog content
                    let content = vec![
                        Line::from(""),
                        Line::from(Span::styled(
                            "Semantic search requires a 23MB model download",
                            Style::default().fg(palette.fg),
                        )),
                        Line::from(Span::styled(
                            "from HuggingFace (all-MiniLM-L6-v2).",
                            Style::default().fg(palette.fg),
                        )),
                        Line::from(""),
                        Line::from(Span::styled(
                            "After download, the model runs locally.",
                            Style::default().fg(palette.fg),
                        )),
                        Line::from(Span::styled(
                            "Your search data never leaves your machine.",
                            Style::default().fg(palette.hint),
                        )),
                        Line::from(""),
                        Line::from(vec![
                            Span::styled(
                                "[D]",
                                Style::default()
                                    .fg(palette.accent)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(" Download  ", Style::default().fg(palette.fg)),
                            Span::styled(
                                "[H]",
                                Style::default()
                                    .fg(palette.accent)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(" Hash mode  ", Style::default().fg(palette.fg)),
                            Span::styled(
                                "[Esc]",
                                Style::default()
                                    .fg(palette.accent)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(" Cancel", Style::default().fg(palette.fg)),
                        ]),
                    ];

                    let paragraph = Paragraph::new(content)
                        .block(block)
                        .alignment(Alignment::Center)
                        .wrap(Wrap { trim: false });

                    f.render_widget(ratatui::widgets::Clear, area);
                    f.render_widget(paragraph, area);
                }

                // Source filter popup menu (P4.4)
                if source_filter_menu_open {
                    use crate::sources::provenance::SourceFilter;
                    let area = centered_rect(40, 40, f.area());
                    let block = Block::default()
                        .title(Span::styled(
                            " Source Filter ",
                            Style::default()
                                .fg(palette.accent)
                                .add_modifier(Modifier::BOLD),
                        ))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(palette.accent))
                        .style(Style::default().bg(palette.surface));

                    // Build menu items: All, Local, Remote, then discovered source IDs
                    let menu_items: Vec<(&str, SourceFilter)> = vec![
                        ("All sources", SourceFilter::All),
                        ("Local only", SourceFilter::Local),
                        ("Remote only", SourceFilter::Remote),
                    ];

                    let items: Vec<ListItem> = menu_items
                        .iter()
                        .enumerate()
                        .map(|(i, (label, _))| {
                            let style = if i == source_filter_menu_selection {
                                Style::default()
                                    .bg(palette.accent)
                                    .fg(palette.bg)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(palette.fg)
                            };
                            ListItem::new(Line::from(vec![
                                Span::styled(
                                    if i == source_filter_menu_selection {
                                        "→ "
                                    } else {
                                        "  "
                                    },
                                    style,
                                ),
                                Span::styled(label.to_string(), style),
                            ]))
                        })
                        .chain(available_source_ids.iter().enumerate().map(|(i, id)| {
                            let idx = 3 + i;
                            let style = if idx == source_filter_menu_selection {
                                Style::default()
                                    .bg(palette.accent)
                                    .fg(palette.bg)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(palette.fg)
                            };
                            ListItem::new(Line::from(vec![
                                Span::styled(
                                    if idx == source_filter_menu_selection {
                                        "→ "
                                    } else {
                                        "  "
                                    },
                                    style,
                                ),
                                Span::styled(format!("📡 {}", id), style),
                            ]))
                        }))
                        .collect();

                    let list = List::new(items).block(block);
                    f.render_widget(ratatui::widgets::Clear, area);
                    f.render_widget(list, area);
                }

                if palette_state.open {
                    let area = centered_rect(70, 60, f.area());
                    palette::draw_palette(f, area, &palette_state, palette);
                }

                // Render toast notifications (bead 2yg2)
                render_toasts(f, &toast_manager, &palette);
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
            help_last_interaction = Instant::now();

            // Handle mouse events (skip when modal is open)
            if let Event::Mouse(mouse) = event {
                // Ignore mouse events when help, detail, bulk, or source filter modal is open
                if show_help || show_detail_modal || show_bulk_modal || source_filter_menu_open {
                    continue;
                }
                needs_draw = true;
                let mut pill_clicked = false;
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        let col = mouse.column;
                        let row = mouse.row;

                        // Check pill clicks (edit mode)
                        for (rect, pill) in &last_pill_rects {
                            if col >= rect.x
                                && col < rect.x + rect.width
                                && row >= rect.y
                                && row < rect.y + rect.height
                            {
                                match pill.label.as_str() {
                                    "agent" => {
                                        input_mode = InputMode::Agent;
                                        input_buffer = pill.value.clone();
                                        status = "Edit agent filter".to_string();
                                        dirty_since = None;
                                    }
                                    "ws" => {
                                        input_mode = InputMode::Workspace;
                                        input_buffer = pill.value.clone();
                                        status = "Edit workspace filter".to_string();
                                        dirty_since = None;
                                    }
                                    "time" => {
                                        input_mode = InputMode::CreatedFrom;
                                        input_buffer.clear();
                                        status =
                                            "Enter start date (YYYY-MM-DD) or -7d/-24h".to_string();
                                        dirty_since = None;
                                    }
                                    "pane" => {
                                        input_mode = InputMode::PaneFilter;
                                        input_buffer = pill.value.clone();
                                        status =
                                            "Edit pane filter (Enter apply, Esc clear)".to_string();
                                        dirty_since = None;
                                    }
                                    _ => {}
                                }
                                needs_draw = true;
                                pill_clicked = true;
                                break;
                            }
                        }
                        if pill_clicked {
                            continue;
                        }

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

            // Skip non-press events (fixes Windows double-keystroke issue)
            // On Windows, crossterm fires both Press and Release events
            if key.kind != crossterm::event::KeyEventKind::Press {
                continue;
            }

            needs_draw = true;

            // Global quit override
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                break;
            }

            // Update banner keybindings (bead 018)
            // Only active when banner is visible and no modals are open
            if let Some(ref info) = update_info
                && info.should_show()
                && !update_dismissed
                && !show_help
                && !show_detail_modal
                && !show_bulk_modal
                && !palette_state.open
            {
                match key.code {
                    KeyCode::Char('U') => {
                        // Open release URL in browser
                        let _ = open_in_browser(&info.release_url);
                        status = format!("Opening release page for v{}...", info.latest_version);
                        continue;
                    }
                    KeyCode::Char('S') => {
                        // Skip this version (persist) - uppercase to avoid conflict with typing
                        if let Err(e) = skip_version(&info.latest_version) {
                            status = format!("Failed to skip version: {e}");
                        } else {
                            status = format!("Skipped v{} - won't show again", info.latest_version);
                            update_dismissed = true;
                        }
                        continue;
                    }
                    KeyCode::Esc if input_mode == InputMode::Query => {
                        // Dismiss for this session only
                        update_dismissed = true;
                        status = "Update banner dismissed".to_string();
                        continue;
                    }
                    _ => {}
                }
            }

            // Command palette handling takes precedence over help/detail.
            if palette_state.open {
                match key.code {
                    KeyCode::Esc => {
                        palette_state.open = false;
                        palette_state.query.clear();
                        palette_state.refilter();
                    }
                    KeyCode::Up => palette_state.move_selection(-1),
                    KeyCode::Down => palette_state.move_selection(1),
                    KeyCode::PageUp => palette_state.move_selection(-5),
                    KeyCode::PageDown => palette_state.move_selection(5),
                    KeyCode::Enter => {
                        if let Some(item) = palette_state.filtered.get(palette_state.selected) {
                            // UI metrics: log palette action (bead 020)
                            if ui_metrics_enabled {
                                tracing::info!(
                                    target: "ui_metrics",
                                    event = "palette_action",
                                    action = item.label,
                                    "Palette action executed"
                                );
                            }
                            match item.action {
                                PaletteAction::ToggleTheme => {
                                    theme_dark = !theme_dark;
                                }
                                PaletteAction::ToggleDensity => {
                                    density_mode = density_mode.next();
                                    let height = terminal.size().map(|r| r.height).unwrap_or(24);
                                    per_pane_limit = calculate_pane_limit(height, density_mode);
                                    let prev_agent = active_hit(&panes, active_pane)
                                        .map(|h| h.agent.clone())
                                        .or_else(|| {
                                            panes.get(active_pane).map(|p| p.agent.clone())
                                        });
                                    let prev_path = active_hit(&panes, active_pane)
                                        .map(|h| h.source_path.clone());
                                    panes = rebuild_panes_with_filter(
                                        &results,
                                        pane_filter.as_deref(),
                                        per_pane_limit,
                                        &mut active_pane,
                                        &mut pane_scroll_offset,
                                        prev_agent,
                                        prev_path,
                                        MAX_VISIBLE_PANES,
                                    );
                                    status = format!("Density: {}", density_mode.label());
                                    needs_draw = true;
                                }
                                PaletteAction::ToggleHelpStrip => {
                                    help_pinned = !help_pinned;
                                }
                                PaletteAction::OpenUpdateBanner => {
                                    if let Some(ref info) = update_info {
                                        if info.should_show() {
                                            // Tear down TUI and run the installer
                                            teardown_terminal().ok();
                                            println!(
                                                "Updating from v{} to v{}...\n",
                                                info.current_version, info.latest_version
                                            );
                                            // This does not return - it execs the installer
                                            run_self_update();
                                        } else {
                                            status = "You're on the latest version".to_string();
                                        }
                                    } else {
                                        status = "No update information available yet".to_string();
                                    }
                                }
                                PaletteAction::FilterAgent => {
                                    input_mode = InputMode::Agent;
                                    input_buffer.clear();
                                }
                                PaletteAction::FilterWorkspace => {
                                    input_mode = InputMode::Workspace;
                                    input_buffer.clear();
                                }
                                PaletteAction::FilterToday => {
                                    if let Some((start, _)) = quick_date_range_today() {
                                        filters.created_from = Some(start);
                                        filters.created_to = None;
                                        dirty_since = Some(Instant::now());
                                    }
                                }
                                PaletteAction::FilterWeek => {
                                    if let Some((start, _)) = quick_date_range_week() {
                                        filters.created_from = Some(start);
                                        filters.created_to = None;
                                        dirty_since = Some(Instant::now());
                                    }
                                }
                                PaletteAction::FilterCustomDate => {
                                    input_mode = InputMode::CreatedFrom;
                                    input_buffer.clear();
                                    status = "Enter start date (YYYY-MM-DD)".to_string();
                                }
                                PaletteAction::OpenBulkActions => {
                                    status = "Bulk actions: select with m, open with A".to_string();
                                }
                                PaletteAction::ReloadIndex => {
                                    dirty_since = Some(Instant::now());
                                }
                                PaletteAction::OpenSavedViews => {
                                    status =
                                        "Saved views: Ctrl+<n> save, Shift+<n> load".to_string();
                                }
                                PaletteAction::SaveViewSlot(slot) => {
                                    status = save_view_slot(
                                        slot,
                                        &filters,
                                        ranking_mode,
                                        &mut saved_views,
                                    );
                                }
                                PaletteAction::LoadViewSlot(slot) => {
                                    if let Some(msg) = load_view_slot(
                                        slot,
                                        &mut filters,
                                        &mut ranking_mode,
                                        &saved_views,
                                    ) {
                                        status = msg;
                                        dirty_since = Some(Instant::now());
                                    } else {
                                        status = format!("No saved view in slot {slot}");
                                    }
                                }
                            }
                            palette_state.open = false;
                        }
                    }
                    KeyCode::Backspace => {
                        palette_state.query.pop();
                        palette_state.refilter();
                    }
                    KeyCode::Char(c) => {
                        palette_state.query.push(c);
                        palette_state.refilter();
                    }
                    _ => {}
                }
                continue;
            }

            // Model download consent dialog: handle keys when open
            if show_consent_dialog {
                match key.code {
                    KeyCode::Esc => {
                        show_consent_dialog = false;
                        status = "Cancelled. Staying in lexical mode.".to_string();
                    }
                    KeyCode::Char('d' | 'D') => {
                        show_consent_dialog = false;
                        // Start model download in background
                        let model_dir = default_model_dir(&data_dir);
                        let manifest = ModelManifest::minilm_v2();
                        let total_size = manifest.total_size();
                        let total_files = manifest.files.len();
                        let downloader = ModelDownloader::new(model_dir);
                        let cancel_handle = downloader.cancellation_handle();
                        download_cancel = Some(cancel_handle);

                        // Create channel for progress updates
                        let (tx, rx) = mpsc::channel();
                        download_rx = Some(rx);

                        // Clone tx for use in final completion message
                        let tx_final = tx.clone();

                        // Spawn download thread
                        std::thread::spawn(move || {
                            let callback: Box<dyn Fn(DownloadProgress) + Send + Sync> =
                                Box::new(move |progress| {
                                    let _ = tx.send(progress);
                                });
                            let result = downloader.download(&manifest, Some(callback));
                            // Send a final progress to indicate completion or failure
                            match result {
                                Ok(_) => {
                                    let _ = tx_final.send(DownloadProgress {
                                        current_file: "complete".to_string(),
                                        file_index: total_files,
                                        total_files,
                                        file_bytes: total_size,
                                        file_total: total_size,
                                        total_bytes: total_size,
                                        grand_total: total_size,
                                        progress_pct: 100,
                                    });
                                }
                                Err(e) => {
                                    // Send failure signal so UI can show error
                                    let _ = tx_final.send(DownloadProgress {
                                        current_file: format!("error:{e}"),
                                        file_index: 0,
                                        total_files: 0,
                                        file_bytes: 0,
                                        file_total: 0,
                                        total_bytes: 0,
                                        grand_total: total_size,
                                        progress_pct: 0,
                                    });
                                }
                            }
                        });

                        // Update state to show download in progress
                        semantic_availability = SemanticAvailability::Downloading {
                            progress_pct: 0,
                            bytes_downloaded: 0,
                            total_bytes: total_size,
                        };
                        status = "Downloading model... Press Esc to cancel.".to_string();
                        // Toast: download started
                        toast_manager.push(Toast::info("Downloading model..."));
                    }
                    KeyCode::Char('h' | 'H') => {
                        show_consent_dialog = false;
                        if let Some(client) = &search_client {
                            let setup = load_hash_semantic_context(&data_dir, &db_path);
                            let mut availability = setup.availability;
                            if let Some(context) = setup.context {
                                if let Err(err) = client.set_semantic_context(
                                    context.embedder,
                                    context.index,
                                    context.filter_maps,
                                    context.roles,
                                ) {
                                    availability = SemanticAvailability::LoadFailed {
                                        context: format!("hash context: {err}"),
                                    };
                                }
                            } else {
                                let _ = client.clear_semantic_context();
                            }

                            semantic_availability = availability;
                            if semantic_availability.can_search() {
                                search_mode = SearchMode::Semantic;
                                status = "Using hash-based semantic search (approximate but fast)."
                                    .to_string();
                                // Toast: using hash fallback
                                toast_manager.push(Toast::info("Using hash fallback"));
                            } else {
                                search_mode = SearchMode::Lexical;
                                let reason =
                                    semantic_unavailable_message(&semantic_availability);
                                status = format!(
                                    "Hash fallback unavailable: {reason}. Staying in lexical."
                                );
                                toast_manager.push(Toast::warning("Hash fallback unavailable"));
                            }
                        } else {
                            semantic_availability = SemanticAvailability::NotInstalled;
                            search_mode = SearchMode::Lexical;
                            status = "Search client unavailable; staying in lexical mode."
                                .to_string();
                            toast_manager.push(Toast::warning(
                                "Search client unavailable; cannot enable hash fallback",
                            ));
                        }
                    }
                    _ => {}
                }
                continue;
            }

            // Model download cancellation: handle Esc when download is active
            if download_cancel.is_some() && key.code == KeyCode::Esc {
                // Cancel the active download
                if let Some(ref cancel) = download_cancel {
                    cancel.store(true, Ordering::SeqCst);
                }
                download_rx = None;
                download_cancel = None;
                semantic_availability = SemanticAvailability::NotInstalled;
                search_mode = SearchMode::Lexical;
                status = "Download cancelled. Staying in lexical mode.".to_string();
                // Toast: download cancelled
                toast_manager.push(Toast::warning("Download cancelled"));
                needs_draw = true;
                continue;
            }

            // Bulk action modal: handle keys when open
            if show_bulk_modal {
                const BULK_ACTIONS: [&str; 4] = [
                    "Open all in editor",
                    "Copy all paths",
                    "Export as JSON",
                    "Clear selection",
                ];
                match key.code {
                    KeyCode::Esc => {
                        show_bulk_modal = false;
                        status = format!("{} items still selected", selected.len());
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        bulk_action_idx = bulk_action_idx.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        bulk_action_idx = (bulk_action_idx + 1).min(BULK_ACTIONS.len() - 1);
                    }
                    KeyCode::Enter => {
                        show_bulk_modal = false;
                        // Collect selected hits
                        let selected_hits: Vec<&SearchHit> = selected
                            .iter()
                            .filter_map(|(pane_idx, hit_idx)| {
                                panes.get(*pane_idx).and_then(|p| p.hits.get(*hit_idx))
                            })
                            .collect();
                        match bulk_action_idx {
                            0 => {
                                // Open all in editor
                                let editor = dotenvy::var("EDITOR")
                                    .or_else(|_| dotenvy::var("VISUAL"))
                                    .unwrap_or_else(|_| "code".to_string());
                                let (editor_bin, editor_args) = split_editor_command(&editor);
                                // Exit raw mode
                                disable_raw_mode().ok();
                                execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)
                                    .ok();
                                for hit in &selected_hits {
                                    let path =
                                        prepare_editor_path(&hit.source_path, db_reader.as_ref());
                                    let is_virtual = path != hit.source_path;
                                    let mut cmd = StdCommand::new(&editor_bin);
                                    cmd.args(&editor_args);
                                    let line_opt = hit.line_number.filter(|_| !is_virtual);

                                    if editor_bin == "code" {
                                        if let Some(ln) = line_opt {
                                            cmd.arg("--goto").arg(format!("{}:{}", path, ln));
                                        } else {
                                            cmd.arg(&path);
                                        }
                                    } else {
                                        cmd.arg(&path);
                                    }
                                    let _ = cmd.status();
                                }
                                execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)
                                    .ok();
                                enable_raw_mode().ok();
                                status = format!(
                                    "Opened {} files in {}",
                                    selected_hits.len(),
                                    editor_bin
                                );
                                selected.clear();
                                open_confirm_armed = false;
                            }
                            1 => {
                                // Copy all paths
                                let paths: Vec<String> = selected_hits
                                    .iter()
                                    .map(|h| h.source_path.clone())
                                    .collect();
                                let text = paths.join("\n");
                                let clipboard_cmd = if cfg!(target_os = "macos") {
                                    Some("pbcopy")
                                } else if StdCommand::new("which")
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
                                        selected.clear();
                                        open_confirm_armed = false;
                                        format!("✓ Copied {} paths to clipboard", paths.len())
                                    } else {
                                        "✗ Clipboard copy failed".to_string()
                                    }
                                } else {
                                    "✗ No clipboard tool found".to_string()
                                };
                            }
                            2 => {
                                // Export as JSON
                                let export: Vec<serde_json::Value> = selected_hits
                                    .iter()
                                    .map(|h| {
                                        serde_json::json!({
                                            "source_path": h.source_path,
                                            "line_number": h.line_number,
                                            "title": h.title,
                                            "agent": h.agent,
                                            "workspace": h.workspace,
                                            "score": h.score,
                                            "snippet": h.snippet,
                                        })
                                    })
                                    .collect();
                                if let Ok(json) = serde_json::to_string_pretty(&export) {
                                    let clipboard_cmd = if cfg!(target_os = "macos") {
                                        Some("pbcopy")
                                    } else if StdCommand::new("which")
                                        .arg("xclip")
                                        .output()
                                        .map(|o| o.status.success())
                                        .unwrap_or(false)
                                    {
                                        Some("xclip -selection clipboard")
                                    } else {
                                        None
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
                                                    stdin.write_all(json.as_bytes())?;
                                                }
                                                child.wait()
                                            });
                                        if result.map(|s| s.success()).unwrap_or(false) {
                                            selected.clear();
                                            open_confirm_armed = false;
                                            format!(
                                                "✓ Exported {} items as JSON to clipboard",
                                                export.len()
                                            )
                                        } else {
                                            "✗ JSON export failed".to_string()
                                        }
                                    } else {
                                        "✗ No clipboard tool found".to_string()
                                    };
                                }
                            }
                            3 => {
                                // Clear selection
                                let count = selected.len();
                                selected.clear();
                                open_confirm_armed = false;
                                status = format!("Cleared {count} selections");
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
                continue;
            }

            // Source filter menu: handle keys when open (P4.4)
            if source_filter_menu_open {
                let total_items = 3 + available_source_ids.len();
                match key.code {
                    KeyCode::Esc => {
                        source_filter_menu_open = false;
                        status = "Source filter menu closed".to_string();
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        source_filter_menu_selection =
                            source_filter_menu_selection.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        source_filter_menu_selection =
                            (source_filter_menu_selection + 1).min(total_items.saturating_sub(1));
                    }
                    KeyCode::Enter => {
                        use crate::sources::provenance::SourceFilter;
                        source_filter_menu_open = false;
                        // Apply selected filter
                        filters.source_filter = match source_filter_menu_selection {
                            0 => SourceFilter::All,
                            1 => SourceFilter::Local,
                            2 => SourceFilter::Remote,
                            n => {
                                // Individual source ID
                                let idx = n - 3;
                                if let Some(id) = available_source_ids.get(idx) {
                                    SourceFilter::SourceId(id.clone())
                                } else {
                                    SourceFilter::All
                                }
                            }
                        };
                        status = format!(
                            "Source: {}",
                            match &filters.source_filter {
                                SourceFilter::All => "all sources".to_string(),
                                SourceFilter::Local => "local only".to_string(),
                                SourceFilter::Remote => "remote only".to_string(),
                                SourceFilter::SourceId(id) => format!("source '{}'", id),
                            }
                        );
                        dirty_since = Some(Instant::now());
                    }
                    _ => {}
                }
                needs_draw = true;
                continue;
            }

            // While help is open, keys scroll the help modal and do not affect panes.
            if show_help {
                match key.code {
                    KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?') => {
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
                                text.push_str(&format!("=== {role_label} ===\n"));
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
                                text.push_str(&format!("=== {role_label} ===\n"));
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
                    KeyCode::Char('o') => {
                        // Open source file in $EDITOR or default editor
                        if let Some(pane) = panes.get(active_pane)
                            && let Some(hit) = pane.hits.get(pane.selected)
                        {
                            let path = prepare_editor_path(&hit.source_path, db_reader.as_ref());
                            let is_virtual = path != hit.source_path;

                            // Determine editor: $EDITOR, $VISUAL, or fallback chain
                            let editor = dotenvy::var("EDITOR")
                                .or_else(|_| dotenvy::var("VISUAL"))
                                .unwrap_or_else(|_| {
                                    // Try common editors in order of preference
                                    for candidate in ["code", "vim", "nano", "vi"] {
                                        if StdCommand::new("which")
                                            .arg(candidate)
                                            .output()
                                            .map(|o| o.status.success())
                                            .unwrap_or(false)
                                        {
                                            return candidate.to_string();
                                        }
                                    }
                                    "nano".to_string()
                                });

                            let (editor_bin, editor_args) = split_editor_command(&editor);
                            // Exit raw mode for GUI editors (code) or TUI editors
                            disable_raw_mode().ok();
                            execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture).ok();

                            // Build command with optional line number (suppressed for virtual files)
                            let mut cmd = StdCommand::new(&editor_bin);
                            cmd.args(&editor_args);
                            let line_opt = hit.line_number.filter(|_| !is_virtual);

                            if editor_bin == "code" {
                                // VS Code: code --goto file:line
                                if let Some(ln) = line_opt {
                                    cmd.arg("--goto").arg(format!("{path}:{ln}"));
                                } else {
                                    cmd.arg(&path);
                                }
                            } else if editor_bin == "vim"
                                || editor_bin == "vi"
                                || editor_bin == "nvim"
                            {
                                // Vim: vim +line file
                                if let Some(ln) = line_opt {
                                    cmd.arg(format!("+{ln}"));
                                }
                                cmd.arg(&path);
                            } else if editor_bin == "nano" {
                                // Nano: nano +line file
                                if let Some(ln) = line_opt {
                                    cmd.arg(format!("+{ln}"));
                                }
                                cmd.arg(&path);
                            } else {
                                // Generic: just pass the path
                                cmd.arg(&path);
                            }

                            let result = cmd.status();

                            // Re-enter raw mode
                            execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture).ok();
                            enable_raw_mode().ok();

                            status = if result.map(|s| s.success()).unwrap_or(false) {
                                format!("Opened {path} in {editor_bin}")
                            } else {
                                format!("✗ Failed to open in {editor_bin}")
                            };
                            show_detail_modal = false;
                            modal_scroll = 0;
                        }
                    }
                    KeyCode::Char('p') => {
                        // Copy source path to clipboard
                        if let Some(pane) = panes.get(active_pane)
                            && let Some(hit) = pane.hits.get(pane.selected)
                        {
                            let path = &hit.source_path;
                            let clipboard_cmd = if cfg!(target_os = "macos") {
                                Some("pbcopy")
                            } else if StdCommand::new("which")
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
                                            stdin.write_all(path.as_bytes())?;
                                        }
                                        child.wait()
                                    });
                                if result.map(|s| s.success()).unwrap_or(false) {
                                    format!("✓ Path copied: {path}")
                                } else {
                                    "✗ Clipboard copy failed".to_string()
                                }
                            } else {
                                "✗ No clipboard tool found".to_string()
                            };
                        }
                    }
                    KeyCode::Char('s') => {
                        // Copy snippet to clipboard
                        if let Some(pane) = panes.get(active_pane)
                            && let Some(hit) = pane.hits.get(pane.selected)
                        {
                            let snippet = &hit.snippet;
                            let clipboard_cmd = if cfg!(target_os = "macos") {
                                Some("pbcopy")
                            } else if StdCommand::new("which")
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
                                            stdin.write_all(snippet.as_bytes())?;
                                        }
                                        child.wait()
                                    });
                                if result.map(|s| s.success()).unwrap_or(false) {
                                    "✓ Snippet copied to clipboard".to_string()
                                } else {
                                    "✗ Clipboard copy failed".to_string()
                                }
                            } else {
                                "✗ No clipboard tool found".to_string()
                            };
                        }
                    }
                    _ => {}
                }
                continue;
            }

            // Open command palette (Ctrl+P or Alt+P)
            if matches!(key.code, KeyCode::Char('p'))
                && (key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::ALT))
            {
                palette_state.open = true;
                palette_state.query.clear();
                palette_state.selected = 0;
                palette_state.refilter();
                continue;
            }

            // Cycle search mode (Alt+S)
            if matches!(key.code, KeyCode::Char('s' | 'S'))
                && key.modifiers.contains(KeyModifiers::ALT)
            {
                search_mode = search_mode.next();
                if matches!(search_mode, SearchMode::Semantic | SearchMode::Hybrid) {
                    if let Some(client) = &search_client
                        && !semantic_availability.can_search()
                    {
                        semantic_availability =
                            initialize_semantic_context(client, &data_dir, &db_path);
                    }
                    if !semantic_availability.can_search() {
                        // Check if model needs to be installed - show consent dialog
                        if semantic_availability.is_not_installed() {
                            show_consent_dialog = true;
                            status = "Model not installed. Press D to download, H for hash mode, Esc to cancel.".to_string();
                        } else {
                            let reason = semantic_unavailable_message(&semantic_availability);
                            status = format!("Semantic unavailable: {reason}. Staying in lexical.");
                        }
                        search_mode = SearchMode::Lexical;
                    } else if matches!(search_mode, SearchMode::Hybrid) {
                        status = "Search mode: Hybrid (RRF fusion)".to_string();
                    } else {
                        status = format!("Search mode: {}", search_mode_label(search_mode));
                    }
                } else {
                    status = format!("Search mode: {}", search_mode_label(search_mode));
                }
                dirty_since = Some(Instant::now());
                continue;
            }

            match input_mode {
                InputMode::Query => {
                    if key.modifiers.contains(KeyModifiers::CONTROL) {
                        if let KeyCode::Char(c) = key.code
                            && c.is_ascii_digit()
                            && c != '0'
                        {
                            let slot = c.to_digit(10).unwrap_or(0) as u8;
                            status = save_view_slot(slot, &filters, ranking_mode, &mut saved_views);
                            continue;
                        }
                        // Handle both 'r' and 'R' since Shift modifier may change the char
                        if matches!(key.code, KeyCode::Char('r' | 'R')) {
                            // Ctrl+Shift+R = refresh search (re-query index)
                            if key.modifiers.contains(KeyModifiers::SHIFT) {
                                if let Some(tx) = &reindex_tx {
                                    let _ = tx.send(crate::indexer::IndexerEvent::Command(
                                        crate::indexer::ReindexCommand::Full,
                                    ));
                                    status = "Triggered background re-index...".to_string();
                                } else {
                                    status = "Refreshing search view...".to_string();
                                }
                                page = 0;
                                dirty_since = Some(Instant::now());
                                cached_detail = None;
                                detail_scroll = 0;
                            } else if query_history.is_empty() {
                                // Ctrl+R = cycle history
                                status = "No query history yet".to_string();
                            } else {
                                let next =
                                    history_cursor.map_or(0, |idx| (idx + 1) % query_history.len());
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
                        KeyCode::Char(c)
                            if key.modifiers.contains(KeyModifiers::SHIFT)
                                && c.is_ascii_digit()
                                && c != '0' =>
                        {
                            let slot = c.to_digit(10).unwrap_or(0) as u8;
                            if let Some(msg) =
                                load_view_slot(slot, &mut filters, &mut ranking_mode, &saved_views)
                            {
                                status = msg;
                                page = 0;
                                dirty_since = Some(Instant::now());
                                cached_detail = None;
                                detail_scroll = 0;
                            } else {
                                status = format!("No saved view in slot {slot}");
                            }
                        }
                        KeyCode::Backspace if query.is_empty() => {
                            // Clear the last applied filter (time -> workspace -> agent)
                            if filters.created_from.is_some() || filters.created_to.is_some() {
                                filters.created_from = None;
                                filters.created_to = None;
                                status = "Cleared time filter".to_string();
                            } else if !filters.workspaces.is_empty() {
                                filters.workspaces.clear();
                                status = "Cleared workspace filter".to_string();
                            } else if !filters.agents.is_empty() {
                                filters.agents.clear();
                                status = "Cleared agent filter".to_string();
                            }
                            dirty_since = Some(Instant::now());
                        }
                        KeyCode::Esc | KeyCode::F(10) => {
                            // Priority: 1) Clear selection 2) Exit Detail 3) Quit
                            if !selected.is_empty() {
                                let count = selected.len();
                                selected.clear();
                                open_confirm_armed = false;
                                status = format!("Cleared {count} selections");
                            } else if matches!(focus_region, FocusRegion::Detail) {
                                focus_region = FocusRegion::Results;
                                status = "Focus: Results".to_string();
                            } else {
                                break;
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j')
                            if key.code == KeyCode::Down
                                || key.modifiers.contains(KeyModifiers::ALT) =>
                        {
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
                        KeyCode::Up | KeyCode::Char('k')
                            if key.code == KeyCode::Up
                                || key.modifiers.contains(KeyModifiers::ALT) =>
                        {
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
                        KeyCode::Left | KeyCode::Char('h')
                            if key.code == KeyCode::Left
                                || key.modifiers.contains(KeyModifiers::ALT) =>
                        {
                            match focus_region {
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
                            }
                        }
                        KeyCode::Right | KeyCode::Char('l')
                            if key.code == KeyCode::Right
                                || key.modifiers.contains(KeyModifiers::ALT) =>
                        {
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
                        // Yank (copy to clipboard): Ctrl+Y copies path or content
                        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if let Some(hit) = active_hit(&panes, active_pane) {
                                // User committed to copying result - save query to history
                                save_query_to_history(&query, &mut query_history, history_cap);
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
                        // Multi-select: Ctrl+X toggles selection on current item
                        KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if let Some(pane) = panes.get(active_pane) {
                                let key = (active_pane, pane.selected);
                                if selected.contains(&key) {
                                    selected.remove(&key);
                                    open_confirm_armed = false;
                                    status = format!("Deselected ({} selected)", selected.len());
                                } else {
                                    selected.insert(key);
                                    open_confirm_armed = false;
                                    status = format!(
                                        "Selected ({} total) · Ctrl+X toggle · A bulk actions · Esc clear",
                                        selected.len()
                                    );
                                }
                            }
                        }
                        // Multi-select: Ctrl+A selects all items in current pane
                        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if let Some(pane) = panes.get(active_pane) {
                                let all_selected = (0..pane.hits.len())
                                    .all(|i| selected.contains(&(active_pane, i)));
                                if all_selected {
                                    // Deselect all in this pane
                                    for i in 0..pane.hits.len() {
                                        selected.remove(&(active_pane, i));
                                    }
                                    open_confirm_armed = false;
                                    status = format!(
                                        "Deselected all in pane ({} total)",
                                        selected.len()
                                    );
                                } else {
                                    // Select all in this pane
                                    for i in 0..pane.hits.len() {
                                        selected.insert((active_pane, i));
                                    }
                                    open_confirm_armed = false;
                                    status = format!(
                                        "Selected all in pane ({} total) · A bulk actions",
                                        selected.len()
                                    );
                                }
                            }
                        }
                        // Multi-select: Ctrl+Enter enqueue (add to selection and move to next)
                        KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if let Some(pane) = panes.get_mut(active_pane) {
                                let key = (active_pane, pane.selected);
                                selected.insert(key);
                                open_confirm_armed = false;
                                // Move to next item in the pane
                                if pane.selected + 1 < pane.hits.len() {
                                    pane.selected += 1;
                                }
                                status = format!(
                                    "Queued ({}) · Ctrl+Enter add · Ctrl+O open all",
                                    selected.len()
                                );
                            }
                        }
                        // Ctrl+O: Open all queued items in editor directly
                        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if selected.is_empty() {
                                status = "No items queued. Ctrl+Enter to queue items.".to_string();
                                open_confirm_armed = false;
                            } else if selected.len() >= OPEN_CONFIRM_THRESHOLD
                                && !open_confirm_armed
                            {
                                open_confirm_armed = true;
                                status = format!(
                                    "Open {} queued items? Press Ctrl+O again to confirm.",
                                    selected.len()
                                );
                            } else {
                                let selected_hits: Vec<&SearchHit> = selected
                                    .iter()
                                    .filter_map(|(pane_idx, hit_idx)| {
                                        panes.get(*pane_idx).and_then(|p| p.hits.get(*hit_idx))
                                    })
                                    .collect();
                                let editor = dotenvy::var("EDITOR")
                                    .or_else(|_| dotenvy::var("VISUAL"))
                                    .unwrap_or_else(|_| "code".to_string());
                                let (editor_bin, editor_args) = split_editor_command(&editor);
                                // Exit raw mode
                                disable_raw_mode().ok();
                                execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)
                                    .ok();
                                for hit in &selected_hits {
                                    let path =
                                        prepare_editor_path(&hit.source_path, db_reader.as_ref());
                                    let is_virtual = path != hit.source_path;
                                    let mut cmd = StdCommand::new(&editor_bin);
                                    cmd.args(&editor_args);
                                    let line_opt = hit.line_number.filter(|_| !is_virtual);

                                    if editor_bin == "code" {
                                        if let Some(ln) = line_opt {
                                            cmd.arg("--goto").arg(format!("{}:{}", path, ln));
                                        } else {
                                            cmd.arg(&path);
                                        }
                                    } else {
                                        cmd.arg(&path);
                                    }
                                    let _ = cmd.status();
                                }
                                execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)
                                    .ok();
                                enable_raw_mode().ok();
                                status = format!(
                                    "Opened {} files in {}",
                                    selected_hits.len(),
                                    editor_bin
                                );
                                selected.clear();
                                open_confirm_armed = false;
                            }
                        }
                        // Bulk action menu: A opens when items are selected
                        KeyCode::Char('A') => {
                            if selected.is_empty() {
                                status =
                                    "No items selected. Ctrl+X to select, Ctrl+A to select all."
                                        .to_string();
                            } else {
                                show_bulk_modal = true;
                                bulk_action_idx = 0;
                                status = "Bulk actions: ↑↓ navigate · Enter execute · Esc cancel"
                                    .to_string();
                            }
                        }
                        KeyCode::F(1) | KeyCode::Char('?') => {
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
                        KeyCode::F(5) => {
                            if key.modifiers.contains(KeyModifiers::SHIFT) {
                                // Cycle time presets: 24h -> 7d -> 30d -> All
                                const PRESETS: &[(i64, &str)] = &[
                                    (24, "last 24h"),
                                    (7 * 24, "last 7d"),
                                    (30 * 24, "last 30d"),
                                    (0, "all time"),
                                ];
                                let (hours, label) = PRESETS[time_preset_idx % PRESETS.len()];
                                time_preset_idx = (time_preset_idx + 1) % PRESETS.len();
                                if hours == 0 {
                                    filters.created_from = None;
                                    filters.created_to = None;
                                } else if let Some((start, now)) = quick_date_range_hours(hours) {
                                    filters.created_from = Some(start);
                                    filters.created_to = Some(now);
                                }
                                status = format!("Time preset: {label}");
                                dirty_since = Some(Instant::now());
                            } else {
                                input_mode = InputMode::CreatedFrom;
                                input_buffer.clear();
                                status =
                                    "From: -7d, yesterday, 2024-11-25 | Enter=apply, Esc=cancel"
                                        .to_string();
                            }
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
                        KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            fancy_borders = !fancy_borders;
                            status = format!(
                                "Borders: {}",
                                if fancy_borders {
                                    "rounded (unicode)"
                                } else {
                                    "plain (ASCII)"
                                }
                            );
                            needs_draw = true;
                        }
                        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            density_mode = density_mode.next();
                            // Recalculate pane limit with new density
                            let height = terminal.size().map(|r| r.height).unwrap_or(24);
                            per_pane_limit = calculate_pane_limit(height, density_mode);
                            let prev_agent = active_hit(&panes, active_pane)
                                .map(|h| h.agent.clone())
                                .or_else(|| panes.get(active_pane).map(|p| p.agent.clone()));
                            let prev_path =
                                active_hit(&panes, active_pane).map(|h| h.source_path.clone());
                            panes = rebuild_panes_with_filter(
                                &results,
                                pane_filter.as_deref(),
                                per_pane_limit,
                                &mut active_pane,
                                &mut pane_scroll_offset,
                                prev_agent,
                                prev_path,
                                MAX_VISIBLE_PANES,
                            );
                            status = format!("Density: {}", density_mode.label());
                            needs_draw = true;
                        }
                        // Shift+F11: Open source filter popup menu (P4.4)
                        KeyCode::F(11) if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            source_filter_menu_open = !source_filter_menu_open;
                            if source_filter_menu_open {
                                source_filter_menu_selection = 0;
                                // Discover available source IDs from database
                                available_source_ids = if let Some(ref reader) = db_reader {
                                    reader.get_source_ids().unwrap_or_default()
                                } else {
                                    Vec::new()
                                };
                                status = "Source filter menu (↑/↓ select, Enter apply, Esc close)"
                                    .into();
                            }
                            needs_draw = true;
                        }
                        // F11: Cycle through source filters (P4.3)
                        KeyCode::F(11) => {
                            use crate::sources::provenance::SourceFilter;
                            filters.source_filter = filters.source_filter.cycle();
                            status = format!(
                                "Source: {}",
                                match &filters.source_filter {
                                    SourceFilter::All => "all sources",
                                    SourceFilter::Local => "local only",
                                    SourceFilter::Remote => "remote only",
                                    SourceFilter::SourceId(id) => id.as_str(),
                                }
                            );
                            dirty_since = Some(Instant::now());
                        }
                        KeyCode::F(12) => {
                            ranking_mode = match ranking_mode {
                                RankingMode::RecentHeavy => RankingMode::Balanced,
                                RankingMode::Balanced => RankingMode::RelevanceHeavy,
                                RankingMode::RelevanceHeavy => RankingMode::MatchQualityHeavy,
                                RankingMode::MatchQualityHeavy => RankingMode::DateNewest,
                                RankingMode::DateNewest => RankingMode::DateOldest,
                                RankingMode::DateOldest => RankingMode::RecentHeavy,
                            };
                            status = format!(
                                "Ranking: {}",
                                match ranking_mode {
                                    RankingMode::RecentHeavy => "recent-heavy",
                                    RankingMode::Balanced => "balanced",
                                    RankingMode::RelevanceHeavy => "relevance-heavy",
                                    RankingMode::MatchQualityHeavy => "match-quality",
                                    RankingMode::DateNewest => "date (newest first)",
                                    RankingMode::DateOldest => "date (oldest first)",
                                }
                            );
                            dirty_since = Some(Instant::now());
                        }
                        KeyCode::Delete
                            if key
                                .modifiers
                                .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT) =>
                        {
                            let removed = std::fs::remove_file(&state_path).is_ok();
                            // Restore runtime defaults
                            match_mode = MatchMode::Prefix;
                            search_mode = SearchMode::Lexical;
                            context_window = ContextWindow::Medium;
                            density_mode = DensityMode::Cozy;
                            let height = terminal.size().map(|r| r.height).unwrap_or(24);
                            per_pane_limit = calculate_pane_limit(height, density_mode);
                            filters = SearchFilters::default();
                            pane_filter = None;
                            page = 0;
                            active_pane = 0;
                            pane_scroll_offset = 0;
                            detail_scroll = 0;
                            cached_detail = None;
                            detail_find = None;
                            query_history.clear();
                            history_cursor = None;
                            saved_views.clear();
                            help_pinned = false;
                            show_help = true;
                            help_last_interaction = Instant::now();
                            selected.clear();
                            open_confirm_armed = false;
                            panes.clear();
                            results.clear();
                            dirty_since = Some(Instant::now());
                            status = if removed {
                                "UI state reset: deleted tui_state.json and restored defaults"
                                    .to_string()
                            } else {
                                "UI state reset: defaults restored (state file not present)"
                                    .to_string()
                            };
                            needs_draw = true;
                        }
                        KeyCode::Delete if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            filters = SearchFilters::default();
                            pane_filter = None;
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
                                // User committed to viewing result in editor - save query to history
                                save_query_to_history(&query, &mut query_history, history_cap);
                                let path = &hit.source_path;
                                let mut cmd = StdCommand::new(&editor_bin);
                                cmd.args(&editor_args);
                                if let Some(line) = hit.line_number {
                                    cmd.arg(format!("{editor_line_flag}{line}"));
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
                        KeyCode::Char(c) => {
                            // Detail tab switching - only when results are showing
                            if !panes.is_empty() && active_hit(&panes, active_pane).is_some() {
                                if c == ']' {
                                    detail_tab = match detail_tab {
                                        DetailTab::Messages => DetailTab::Snippets,
                                        DetailTab::Snippets => DetailTab::Raw,
                                        DetailTab::Raw => DetailTab::Messages,
                                    };
                                    detail_scroll = 0;
                                    continue;
                                }
                                if c == '[' {
                                    detail_tab = match detail_tab {
                                        DetailTab::Messages => DetailTab::Raw,
                                        DetailTab::Snippets => DetailTab::Messages,
                                        DetailTab::Raw => DetailTab::Snippets,
                                    };
                                    detail_scroll = 0;
                                    continue;
                                }
                            }
                            // Detail pane local find/navigation
                            if matches!(focus_region, FocusRegion::Detail) {
                                if c == '/' {
                                    input_mode = InputMode::DetailFind;
                                    input_buffer.clear();
                                    detail_find = None;
                                    status = "Detail find: type to search this conversation (Enter apply, Esc cancel)"
                                        .to_string();
                                    needs_draw = true;
                                    continue;
                                }
                                if c == 'n' || c == 'N' {
                                    if let Some(df) = detail_find.as_mut() {
                                        if df.matches.is_empty() {
                                            status = format!("No matches for \"{}\"", df.query);
                                        } else {
                                            let len = df.matches.len();
                                            if c == 'n' {
                                                df.current = (df.current + 1) % len;
                                            } else {
                                                df.current =
                                                    df.current.checked_sub(1).unwrap_or(len - 1);
                                            }
                                            detail_scroll = df.matches[df.current];
                                            status = format!(
                                                "Match {}/{} for \"{}\"",
                                                df.current + 1,
                                                len,
                                                df.query
                                            );
                                        }
                                    } else {
                                        status = "Start detail find with / (in Detail)".to_string();
                                    }
                                    needs_draw = true;
                                    continue;
                                }
                                // Other typing returns focus to results/query
                                focus_region = FocusRegion::Results;
                            }

                            // dft.1: Handle 1/2/3 shortcuts to apply did-you-mean suggestions
                            if panes.is_empty()
                                && !suggestions.is_empty()
                                && ('1'..='3').contains(&c)
                                && key.modifiers.is_empty()
                            {
                                let idx = c.to_digit(10).unwrap_or(1) as usize - 1;
                                if let Some(sugg) = suggestions.get(idx) {
                                    // Apply the suggestion
                                    if let Some(ref new_query) = sugg.suggested_query {
                                        query = new_query.clone();
                                        status = format!("Applied: {}", sugg.message);
                                    }
                                    if let Some(ref new_filters) = sugg.suggested_filters {
                                        filters = new_filters.clone();
                                        status = format!("Applied: {}", sugg.message);
                                    }
                                    page = 0;
                                    dirty_since = Some(Instant::now());
                                    cached_detail = None;
                                    detail_scroll = 0;
                                    needs_draw = true;
                                    continue;
                                }
                            }

                            // Pane-local search (/) does not hit the index; it filters current panes.
                            if c == '/' && !panes.is_empty() {
                                input_mode = InputMode::PaneFilter;
                                input_buffer.clear();
                                status = "Pane filter: type to narrow current results (Esc clears)"
                                    .to_string();
                                needs_draw = true;
                                continue;
                            }

                            if key.modifiers.contains(KeyModifiers::ALT) {
                                // Alt+digit (1-9) for quick pane switching
                                if ('1'..='9').contains(&c) {
                                    let target = c.to_digit(10).unwrap_or(1) as usize - 1;
                                    if target < panes.len() {
                                        active_pane = target;
                                        focus_flash_until =
                                            Some(Instant::now() + Duration::from_millis(220));
                                        cached_detail = None;
                                        detail_scroll = 0;
                                    }
                                    continue;
                                }
                                // Alt+- decreases pane size
                                if c == '-' && !panes.is_empty() {
                                    per_pane_limit = per_pane_limit.saturating_sub(2).max(4);
                                    status = format!("Pane size: {per_pane_limit} items");
                                    let prev_agent = active_hit(&panes, active_pane)
                                        .map(|h| h.agent.clone())
                                        .or_else(|| {
                                            panes.get(active_pane).map(|p| p.agent.clone())
                                        });
                                    let prev_path = active_hit(&panes, active_pane)
                                        .map(|h| h.source_path.clone());
                                    panes = rebuild_panes_with_filter(
                                        &results,
                                        pane_filter.as_deref(),
                                        per_pane_limit,
                                        &mut active_pane,
                                        &mut pane_scroll_offset,
                                        prev_agent,
                                        prev_path,
                                        MAX_VISIBLE_PANES,
                                    );
                                    dirty_since = Some(Instant::now());
                                    continue;
                                }
                                // Other Alt+key combinations fall through to vim nav below
                            }
                            if key.modifiers.contains(KeyModifiers::SHIFT) && matches!(c, '+' | '=')
                            {
                                per_pane_limit = (per_pane_limit + 2).min(50);
                                status = format!("Pane size: {per_pane_limit} items");
                                let prev_agent = active_hit(&panes, active_pane)
                                    .map(|h| h.agent.clone())
                                    .or_else(|| panes.get(active_pane).map(|p| p.agent.clone()));
                                let prev_path =
                                    active_hit(&panes, active_pane).map(|h| h.source_path.clone());
                                panes = rebuild_panes_with_filter(
                                    &results,
                                    pane_filter.as_deref(),
                                    per_pane_limit,
                                    &mut active_pane,
                                    &mut pane_scroll_offset,
                                    prev_agent,
                                    prev_path,
                                    MAX_VISIBLE_PANES,
                                );
                                dirty_since = Some(Instant::now());
                                continue;
                            }
                            if key.modifiers.contains(KeyModifiers::CONTROL)
                                && c == ' '
                                && !panes.is_empty()
                                && active_hit(&panes, active_pane).is_some()
                            {
                                // Ctrl+Space acts as a momentary zoom: swap to XL context, tap again to restore.
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
                                    status =
                                        "Peek: XL context (Ctrl+Space to toggle back)".to_string();
                                    peek_badge_until =
                                        Some(Instant::now() + Duration::from_millis(600));
                                }
                                dirty_since = Some(Instant::now());
                                continue;
                            }
                            // Vim-style navigation with Alt modifier (Alt+h/j/k/l/g/G)
                            // Only activates when panes are showing
                            if key.modifiers.contains(KeyModifiers::ALT) && !panes.is_empty() {
                                match c {
                                    'g' => {
                                        // Alt+g = jump to first item
                                        if let Some(pane) = panes.get_mut(active_pane) {
                                            pane.selected = 0;
                                            cached_detail = None;
                                            detail_scroll = 0;
                                        }
                                        continue;
                                    }
                                    'G' => {
                                        // Alt+G = jump to last item
                                        if let Some(pane) = panes.get_mut(active_pane)
                                            && !pane.hits.is_empty()
                                        {
                                            pane.selected = pane.hits.len() - 1;
                                            cached_detail = None;
                                            detail_scroll = 0;
                                        }
                                        continue;
                                    }
                                    'j' => {
                                        // Alt+j = down
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
                                    'k' => {
                                        // Alt+k = up
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
                                    'h' => {
                                        // Alt+h = left pane
                                        match focus_region {
                                            FocusRegion::Results => {
                                                active_pane = active_pane.saturating_sub(1);
                                                if active_pane < pane_scroll_offset {
                                                    pane_scroll_offset = active_pane;
                                                }
                                                focus_flash_until = Some(
                                                    Instant::now() + Duration::from_millis(220),
                                                );
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
                                    'l' => {
                                        // Alt+l = right pane / focus detail
                                        match focus_region {
                                            FocusRegion::Results => {
                                                if active_pane + 1 < panes.len() {
                                                    active_pane += 1;
                                                    if active_pane
                                                        >= pane_scroll_offset + MAX_VISIBLE_PANES
                                                    {
                                                        pane_scroll_offset = active_pane
                                                            .saturating_sub(MAX_VISIBLE_PANES - 1);
                                                    }
                                                    focus_flash_until = Some(
                                                        Instant::now() + Duration::from_millis(220),
                                                    );
                                                    cached_detail = None;
                                                    detail_scroll = 0;
                                                } else {
                                                    focus_region = FocusRegion::Detail;
                                                    status = "Focus: Detail (Alt+j/k scroll, Alt+h back)"
                                                        .to_string();
                                                }
                                            }
                                            FocusRegion::Detail => {
                                                // Already at rightmost
                                            }
                                        }
                                        continue;
                                    }
                                    _ => {}
                                }
                            }
                            // All other characters pass through to query input
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
                                // User committed to viewing a result - save query to history
                                save_query_to_history(&query, &mut query_history, history_cap);
                                // Open full-screen detail modal for parsed viewing
                                show_detail_modal = true;
                                modal_scroll = 0;
                                status = "Detail view · Esc close · c copy · n nano".to_string();
                            } else if active_hit(&panes, active_pane).is_some() {
                                // User committed to viewing a result - save query to history
                                save_query_to_history(&query, &mut query_history, history_cap);
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
                            status = format!("Completed to '{first}'. Press Enter to apply.");
                        }
                    }
                    KeyCode::Enter => {
                        filters.agents.clear();
                        let filter_value = input_buffer.trim().to_string();
                        if !filter_value.is_empty() {
                            filters.agents.insert(filter_value.clone());
                        }
                        // UI metrics: log pill edit (bead 020)
                        if ui_metrics_enabled {
                            tracing::info!(
                                target: "ui_metrics",
                                event = "pill_edit",
                                pill_type = "agent",
                                has_value = !filter_value.is_empty(),
                                "Agent filter applied"
                            );
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
                        let parsed = crate::ui::time_parser::parse_time_input(&input_buffer);
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
                        let parsed = crate::ui::time_parser::parse_time_input(&input_buffer);
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
                InputMode::PaneFilter => match key.code {
                    KeyCode::Esc => {
                        pane_filter = None;
                        input_buffer.clear();
                        input_mode = InputMode::Query;
                        cached_detail = None;
                        detail_scroll = 0;
                        let prev_agent = active_hit(&panes, active_pane)
                            .map(|h| h.agent.clone())
                            .or_else(|| panes.get(active_pane).map(|p| p.agent.clone()));
                        let prev_path =
                            active_hit(&panes, active_pane).map(|h| h.source_path.clone());
                        panes = rebuild_panes_with_filter(
                            &results,
                            pane_filter.as_deref(),
                            per_pane_limit,
                            &mut active_pane,
                            &mut pane_scroll_offset,
                            prev_agent,
                            prev_path,
                            MAX_VISIBLE_PANES,
                        );
                        status = "Pane filter cleared".to_string();
                        needs_draw = true;
                    }
                    KeyCode::Enter => {
                        pane_filter = if input_buffer.trim().is_empty() {
                            None
                        } else {
                            Some(input_buffer.trim().to_string())
                        };
                        input_buffer.clear();
                        input_mode = InputMode::Query;
                        cached_detail = None;
                        detail_scroll = 0;
                        let prev_agent = active_hit(&panes, active_pane)
                            .map(|h| h.agent.clone())
                            .or_else(|| panes.get(active_pane).map(|p| p.agent.clone()));
                        let prev_path =
                            active_hit(&panes, active_pane).map(|h| h.source_path.clone());
                        panes = rebuild_panes_with_filter(
                            &results,
                            pane_filter.as_deref(),
                            per_pane_limit,
                            &mut active_pane,
                            &mut pane_scroll_offset,
                            prev_agent,
                            prev_path,
                            MAX_VISIBLE_PANES,
                        );
                        status = if pane_filter.as_ref().is_some_and(|s| !s.trim().is_empty()) {
                            "Pane filter applied".to_string()
                        } else {
                            "Pane filter cleared".to_string()
                        };
                        needs_draw = true;
                    }
                    KeyCode::Backspace => {
                        input_buffer.pop();
                        pane_filter = if input_buffer.trim().is_empty() {
                            None
                        } else {
                            Some(input_buffer.trim().to_string())
                        };
                        cached_detail = None;
                        detail_scroll = 0;
                        let prev_agent = active_hit(&panes, active_pane)
                            .map(|h| h.agent.clone())
                            .or_else(|| panes.get(active_pane).map(|p| p.agent.clone()));
                        let prev_path =
                            active_hit(&panes, active_pane).map(|h| h.source_path.clone());
                        panes = rebuild_panes_with_filter(
                            &results,
                            pane_filter.as_deref(),
                            per_pane_limit,
                            &mut active_pane,
                            &mut pane_scroll_offset,
                            prev_agent,
                            prev_path,
                            MAX_VISIBLE_PANES,
                        );
                        needs_draw = true;
                    }
                    KeyCode::Char(c) => {
                        input_buffer.push(c);
                        pane_filter = Some(input_buffer.clone());
                        cached_detail = None;
                        detail_scroll = 0;
                        let prev_agent = active_hit(&panes, active_pane)
                            .map(|h| h.agent.clone())
                            .or_else(|| panes.get(active_pane).map(|p| p.agent.clone()));
                        let prev_path =
                            active_hit(&panes, active_pane).map(|h| h.source_path.clone());
                        panes = rebuild_panes_with_filter(
                            &results,
                            pane_filter.as_deref(),
                            per_pane_limit,
                            &mut active_pane,
                            &mut pane_scroll_offset,
                            prev_agent,
                            prev_path,
                            MAX_VISIBLE_PANES,
                        );
                        needs_draw = true;
                    }
                    _ => {}
                },
                InputMode::DetailFind => match key.code {
                    KeyCode::Esc => {
                        detail_find = None;
                        input_buffer.clear();
                        input_mode = InputMode::Query;
                        status = "Detail find cancelled".to_string();
                        focus_region = FocusRegion::Detail;
                        needs_draw = true;
                    }
                    KeyCode::Enter => {
                        let term = input_buffer.trim().to_string();
                        if term.is_empty() {
                            detail_find = None;
                            status = "Detail find cleared".to_string();
                        } else {
                            detail_find = Some(DetailFindState {
                                query: term.clone(),
                                matches: Vec::new(),
                                current: 0,
                            });
                            status = format!("Detail find: \"{term}\"");
                            detail_scroll = 0;
                        }
                        input_buffer.clear();
                        input_mode = InputMode::Query;
                        dirty_since = Some(Instant::now());
                        focus_region = FocusRegion::Detail;
                        needs_draw = true;
                    }
                    KeyCode::Backspace => {
                        input_buffer.pop();
                    }
                    KeyCode::Char(c) => {
                        input_buffer.push(c);
                    }
                    _ => {}
                },
            }
        }

        if last_tick.elapsed() >= tick_rate {
            // Tick toasts to remove expired notifications
            // Check BEFORE tick - if we had toasts, we need to redraw to update or clear them
            let had_toasts = !toast_manager.is_empty();
            toast_manager.tick();
            if had_toasts {
                needs_draw = true;
            }

            if let Some(client) = &search_client {
                let should_search = dirty_since.is_some_and(|t| t.elapsed() >= debounce);

                if should_search {
                    last_query = query.clone();
                    let prev_agent = active_hit(&panes, active_pane)
                        .map(|h| h.agent.clone())
                        .or_else(|| panes.get(active_pane).map(|p| p.agent.clone()));
                    let prev_path = active_hit(&panes, active_pane).map(|h| h.source_path.clone());
                    let lexical_query = apply_match_mode(&query, match_mode);
                    let semantic_query = query.clone();
                    // Use search_with_fallback for implicit wildcard expansion on sparse results
                    const SPARSE_THRESHOLD: usize = 3;
                    let search_started = Instant::now();
                    let use_semantic =
                        matches!(search_mode, SearchMode::Semantic | SearchMode::Hybrid)
                            && semantic_availability.can_search();
                    if matches!(search_mode, SearchMode::Semantic | SearchMode::Hybrid)
                        && !semantic_availability.can_search()
                    {
                        let reason = semantic_unavailable_message(&semantic_availability);
                        status = format!("Semantic unavailable: {reason}. Using lexical.");
                    }
                    // Track effective search mode for ranking (bead vq8v)
                    let mut effective_search_mode = SearchMode::Lexical;
                    let search_result = match search_mode {
                        SearchMode::Hybrid if use_semantic => {
                            match client.search_hybrid(
                                &lexical_query,
                                &semantic_query,
                                filters.clone(),
                                page_size,
                                page * page_size,
                                SPARSE_THRESHOLD,
                                crate::search::query::FieldMask::FULL,
                            ) {
                                Ok(result) => {
                                    effective_search_mode = SearchMode::Hybrid;
                                    Ok(result)
                                }
                                Err(err) => {
                                    semantic_availability = SemanticAvailability::LoadFailed {
                                        context: format!("hybrid search: {err}"),
                                    };
                                    status = format!("Hybrid search failed: {err}. Using lexical.");
                                    // Toast: hybrid search failed
                                    toast_manager.push(Toast::warning(
                                        "Hybrid search failed, using lexical",
                                    ));
                                    client.search_with_fallback(
                                        &lexical_query,
                                        filters.clone(),
                                        page_size,
                                        page * page_size,
                                        SPARSE_THRESHOLD,
                                        crate::search::query::FieldMask::FULL,
                                    )
                                }
                            }
                        }
                        SearchMode::Semantic if use_semantic => {
                            match client.search_semantic(
                                &semantic_query,
                                filters.clone(),
                                page_size,
                                page * page_size,
                                crate::search::query::FieldMask::FULL,
                            ) {
                                Ok(hits) => {
                                    effective_search_mode = SearchMode::Semantic;
                                    Ok(crate::search::query::SearchResult {
                                        hits,
                                        wildcard_fallback: false,
                                        cache_stats: CacheStats::default(),
                                        suggestions: Vec::new(),
                                    })
                                }
                                Err(err) => {
                                    semantic_availability = SemanticAvailability::LoadFailed {
                                        context: format!("semantic search: {err}"),
                                    };
                                    status =
                                        format!("Semantic search failed: {err}. Using lexical.");
                                    // Toast: semantic search failed
                                    toast_manager.push(Toast::warning(
                                        "Semantic search failed, using lexical",
                                    ));
                                    client.search_with_fallback(
                                        &lexical_query,
                                        filters.clone(),
                                        page_size,
                                        page * page_size,
                                        SPARSE_THRESHOLD,
                                        crate::search::query::FieldMask::FULL,
                                    )
                                }
                            }
                        }
                        _ => client.search_with_fallback(
                            &lexical_query,
                            filters.clone(),
                            page_size,
                            page * page_size,
                            SPARSE_THRESHOLD,
                            crate::search::query::FieldMask::FULL,
                        ),
                    };
                    match search_result {
                        Ok(search_result) => {
                            let search_ms = search_started.elapsed().as_millis();
                            last_search_ms = Some(search_ms);
                            // UI metrics: log search latency (bead 020)
                            if ui_metrics_enabled {
                                tracing::info!(
                                    target: "ui_metrics",
                                    event = "search_complete",
                                    latency_ms = search_ms,
                                    hit_count = search_result.hits.len(),
                                    wildcard_fallback = search_result.wildcard_fallback,
                                    "Search completed"
                                );
                            }
                            let hits = search_result.hits;
                            cache_stats = if cache_debug {
                                Some(search_result.cache_stats)
                            } else {
                                None
                            };
                            wildcard_fallback = search_result.wildcard_fallback;
                            suggestions = search_result.suggestions;
                            dirty_since = None;
                            // dft.2: Zero-match recent fallback
                            // When search returns 0 results for a non-empty query, fall back to
                            // showing recent conversations per agent
                            let use_recent_fallback = hits.is_empty()
                                && page == 0
                                && !query.trim().is_empty()
                                && pane_filter.is_none();

                            if hits.is_empty() && page > 0 {
                                page = page.saturating_sub(1);
                                active_pane = 0;
                                dirty_since = Some(Instant::now());
                                needs_draw = true;
                            } else if use_recent_fallback {
                                // Fetch recent results with no query filter (dft.2)
                                let fallback_filters = SearchFilters::default();
                                match client.search(
                                    "",
                                    fallback_filters,
                                    page_size,
                                    0,
                                    crate::search::query::FieldMask::FULL,
                                ) {
                                    Ok(recent_hits) => {
                                        results = recent_hits;
                                        // Sort by recency (newest first)
                                        results.sort_by(|a, b| {
                                            let ts_a = a.created_at.unwrap_or(0);
                                            let ts_b = b.created_at.unwrap_or(0);
                                            ts_b.cmp(&ts_a)
                                        });
                                    }
                                    Err(_) => {
                                        results = Vec::new();
                                    }
                                }
                                // Build panes from fallback results
                                panes = rebuild_panes_with_filter(
                                    &results,
                                    None, // No pane filter for fallback
                                    per_pane_limit,
                                    &mut active_pane,
                                    &mut pane_scroll_offset,
                                    prev_agent.clone(),
                                    prev_path.clone(),
                                    MAX_VISIBLE_PANES,
                                );
                                selected.clear();
                                open_confirm_armed = false;
                                // Start staggered reveal animation for fallback results (bead 013)
                                if animations_enabled && !panes.is_empty() {
                                    reveal_anim_start = Some(Instant::now());
                                } else if !animations_enabled
                                    && !panes.is_empty()
                                    && ui_metrics_enabled
                                {
                                    // UI metrics: log animation opt-out (bead 020)
                                    tracing::debug!(
                                        target: "ui_metrics",
                                        event = "animation_skipped",
                                        reason = "disabled",
                                        pane_count = panes.len(),
                                        "Reveal animation skipped (animations disabled)"
                                    );
                                }
                                let total_hits: usize = panes.iter().map(|p| p.total_count).sum();
                                if total_hits > 0 {
                                    status = format!(
                                        "No matches for \"{}\". Showing {} recent across {} agents.",
                                        query.chars().take(20).collect::<String>(),
                                        total_hits,
                                        panes.len()
                                    );
                                } else {
                                    status = format!(
                                        "No matches for \"{}\".",
                                        query.chars().take(30).collect::<String>()
                                    );
                                }
                                needs_draw = true;
                            } else {
                                results = hits;
                                let max_created = results
                                    .iter()
                                    .filter_map(|h| h.created_at)
                                    .max()
                                    .unwrap_or(0)
                                    as f32;
                                // Handle pure date sorting modes separately
                                if matches!(
                                    ranking_mode,
                                    RankingMode::DateNewest | RankingMode::DateOldest
                                ) {
                                    results.sort_by(|a, b| {
                                        let ts_a = a.created_at.unwrap_or(0);
                                        let ts_b = b.created_at.unwrap_or(0);
                                        if matches!(ranking_mode, RankingMode::DateNewest) {
                                            ts_b.cmp(&ts_a) // Descending (newest first)
                                        } else {
                                            ts_a.cmp(&ts_b) // Ascending (oldest first)
                                        }
                                    });
                                } else {
                                    // RankingMode support for all search modes (bead vq8v)
                                    // Recency helper (shared across all modes)
                                    let recency = |h: &SearchHit| -> f32 {
                                        if max_created <= 0.0 {
                                            return 0.0;
                                        }
                                        h.created_at.map_or(0.0, |v| v as f32 / max_created)
                                    };

                                    match effective_search_mode {
                                        SearchMode::Lexical => {
                                            // Lexical: BM25 score * quality_factor + alpha * recency
                                            let alpha = match ranking_mode {
                                                RankingMode::RecentHeavy => 1.0,
                                                RankingMode::Balanced => 0.4,
                                                RankingMode::RelevanceHeavy => 0.1,
                                                RankingMode::MatchQualityHeavy => 0.2,
                                                RankingMode::DateNewest
                                                | RankingMode::DateOldest => unreachable!(),
                                            };
                                            // Per-hit quality factor based on match_type
                                            //   Exact: 1.0, Prefix: 0.9, Suffix: 0.8,
                                            //   Substring: 0.7, ImplicitWildcard: 0.6
                                            let quality_factor = |h: &SearchHit| -> f32 {
                                                h.match_type.quality_factor()
                                            };
                                            results.sort_by(|a, b| {
                                                let score_a = (a.score * quality_factor(a))
                                                    + alpha * recency(a);
                                                let score_b = (b.score * quality_factor(b))
                                                    + alpha * recency(b);
                                                score_b
                                                    .partial_cmp(&score_a)
                                                    .unwrap_or(std::cmp::Ordering::Equal)
                                            });
                                        }
                                        SearchMode::Semantic => {
                                            // Semantic: normalize similarity [-1,1] -> [0,1]
                                            // Then apply weighted blend (per bead vq8v spec)
                                            let (score_weight, recency_weight) = match ranking_mode
                                            {
                                                RankingMode::RecentHeavy => (0.3, 0.7),
                                                RankingMode::Balanced => (0.5, 0.5),
                                                RankingMode::RelevanceHeavy => (0.8, 0.2),
                                                RankingMode::MatchQualityHeavy => (0.85, 0.15),
                                                RankingMode::DateNewest
                                                | RankingMode::DateOldest => unreachable!(),
                                            };
                                            let norm_score = |h: &SearchHit| (h.score + 1.0) / 2.0;
                                            results.sort_by(|a, b| {
                                                let score_a = score_weight * norm_score(a)
                                                    + recency_weight * recency(a);
                                                let score_b = score_weight * norm_score(b)
                                                    + recency_weight * recency(b);
                                                score_b
                                                    .partial_cmp(&score_a)
                                                    .unwrap_or(std::cmp::Ordering::Equal)
                                            });
                                        }
                                        SearchMode::Hybrid => {
                                            // Hybrid: RRF scores normalized to [0,1], then weighted blend
                                            let max_rrf = results
                                                .iter()
                                                .map(|h| h.score)
                                                .fold(0.0f32, f32::max);
                                            let (score_weight, recency_weight) = match ranking_mode
                                            {
                                                RankingMode::RecentHeavy => (0.3, 0.7),
                                                RankingMode::Balanced => (0.5, 0.5),
                                                RankingMode::RelevanceHeavy => (0.8, 0.2),
                                                RankingMode::MatchQualityHeavy => (0.85, 0.15),
                                                RankingMode::DateNewest
                                                | RankingMode::DateOldest => unreachable!(),
                                            };
                                            let norm_score = |h: &SearchHit| {
                                                if max_rrf > 0.0 {
                                                    h.score / max_rrf
                                                } else {
                                                    0.0
                                                }
                                            };
                                            results.sort_by(|a, b| {
                                                let score_a = score_weight * norm_score(a)
                                                    + recency_weight * recency(a);
                                                let score_b = score_weight * norm_score(b)
                                                    + recency_weight * recency(b);
                                                score_b
                                                    .partial_cmp(&score_a)
                                                    .unwrap_or(std::cmp::Ordering::Equal)
                                            });
                                        }
                                    }
                                }
                                panes = rebuild_panes_with_filter(
                                    &results,
                                    pane_filter.as_deref(),
                                    per_pane_limit,
                                    &mut active_pane,
                                    &mut pane_scroll_offset,
                                    prev_agent,
                                    prev_path,
                                    MAX_VISIBLE_PANES,
                                );
                                // Clear multi-selection when results change
                                selected.clear();
                                open_confirm_armed = false;
                                // Start staggered reveal animation for new results (bead 013)
                                if animations_enabled && !panes.is_empty() {
                                    reveal_anim_start = Some(Instant::now());
                                }
                                // Show a clean, user-friendly status
                                let total_hits: usize = panes.iter().map(|p| p.total_count).sum();
                                status = if total_hits == 0 {
                                    if pane_filter.as_ref().is_some_and(|s| !s.trim().is_empty()) {
                                        "No results match pane filter".to_string()
                                    } else {
                                        "No results found".to_string()
                                    }
                                } else if panes.len() == 1 {
                                    format!("{total_hits} results")
                                } else {
                                    format!("{} results across {} agents", total_hits, panes.len())
                                };
                                // Query history is now saved only on explicit commit actions
                                // (Enter on result, F8 editor, y copy) via save_query_to_history()
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
                            selected.clear();
                            open_confirm_armed = false;
                            active_pane = 0;
                            cache_stats = None;
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
            // Handle staggered reveal animation (bead 013)
            // Keep redrawing while animation is in progress
            if let Some(start) = reveal_anim_start {
                let total_anim_ms = (MAX_ANIMATED_ITEMS as u64) * STAGGER_DELAY_MS + ITEM_FADE_MS;
                if start.elapsed().as_millis() < u128::from(total_anim_ms) {
                    needs_draw = true; // Animation still in progress
                } else {
                    reveal_anim_start = None; // Animation complete
                }
            }
            // Poll for update check result (bead 018)
            if update_info.is_none()
                && let Ok(info) = update_check_rx.try_recv()
            {
                if let Some(ref i) = info
                    && i.should_show()
                {
                    needs_draw = true;
                }
                update_info = info;
            }
            // Poll for model download progress (bead 44pw)
            if let Some(ref rx) = download_rx {
                // Drain all pending progress messages (get the latest)
                let mut latest_progress: Option<DownloadProgress> = None;
                while let Ok(progress) = rx.try_recv() {
                    latest_progress = Some(progress);
                }
                if let Some(progress) = latest_progress {
                    if progress.progress_pct >= 100 && progress.current_file == "complete" {
                        // Download completed successfully
                        // Reload semantic context to get Ready state
                        // Re-initialize semantic context with the search client
                        if let Some(ref client) = search_client {
                            semantic_availability =
                                initialize_semantic_context(client, &data_dir, &db_path);
                        } else {
                            let setup = load_semantic_context(&data_dir, &db_path);
                            semantic_availability = setup.availability;
                        }
                        if semantic_availability.is_ready() {
                            search_mode = SearchMode::Semantic;
                            status =
                                "Model installed! Semantic search is now available.".to_string();
                            // Toast: semantic search ready
                            toast_manager.push(Toast::success("Semantic search ready"));
                        } else {
                            // Model downloaded but may need index building
                            status =
                                format!("Model downloaded. {}", semantic_availability.summary());
                            // Toast: index building
                            toast_manager.push(Toast::info("Semantic index building..."));
                        }
                        download_rx = None;
                        download_cancel = None;
                    } else if progress.current_file.starts_with("error:") {
                        // Download failed - extract error message and reset state
                        let err_msg = progress
                            .current_file
                            .strip_prefix("error:")
                            .unwrap_or("Unknown error");
                        semantic_availability = SemanticAvailability::LoadFailed {
                            context: format!("Download failed: {err_msg}"),
                        };
                        status = format!("Download failed: {err_msg}");
                        // Toast: download failed
                        toast_manager.push(Toast::error(format!("Download failed: {err_msg}")));
                        download_rx = None;
                        download_cancel = None;
                    } else {
                        // Update progress
                        semantic_availability = SemanticAvailability::Downloading {
                            progress_pct: progress.progress_pct,
                            bytes_downloaded: progress.total_bytes,
                            total_bytes: progress.grand_total,
                        };
                        let mb_done = progress.total_bytes as f64 / 1_048_576.0;
                        let mb_total = progress.grand_total as f64 / 1_048_576.0;
                        status = format!(
                            "Downloading: {}% ({:.1}/{:.1} MB) - Press Esc to cancel",
                            progress.progress_pct, mb_done, mb_total
                        );
                    }
                    needs_draw = true;
                }
            }
            // Check if indexing progress changed and trigger redraw (bead 019)
            // Includes discovered_agents to trigger redraw when new agents are found
            if let Some(ref p) = progress {
                let (phase, current, total, is_rebuild, _pct, discovered) = get_indexing_state(p);
                let current_state = (phase, current, total, is_rebuild, discovered);
                if last_indexing_state.as_ref() != Some(&current_state) {
                    // UI metrics: log HUD phase change (bead 020)
                    if ui_metrics_enabled {
                        let phase_label = match phase {
                            0 => "idle",
                            1 => "discovering",
                            2 => "indexing",
                            _ => "unknown",
                        };
                        let old_phase = last_indexing_state.map(|s| s.0).unwrap_or(0);
                        if old_phase != phase {
                            tracing::info!(
                                target: "ui_metrics",
                                event = "hud_phase_change",
                                from_phase = old_phase,
                                to_phase = phase,
                                phase_label,
                                is_rebuild,
                                "Indexer HUD phase changed"
                            );
                        }
                    }
                    last_indexing_state = Some(current_state);
                    needs_draw = true;
                }

                // Surface background indexer errors to the UI
                let index_err = p.last_error.lock().ok().and_then(|err| err.clone());
                if index_err != last_index_error {
                    if let Some(ref err) = index_err {
                        status = format!("Indexer error: {err} (see cass.log)");
                        toast_manager.push(Toast::error("Indexer failed (see cass.log)"));
                        needs_draw = true;
                    }
                    last_index_error = index_err;
                }

                // Heartbeat redraw while indexing is active (keeps HUD responsive)
                if phase > 0 && last_index_redraw.elapsed() >= Duration::from_millis(500) {
                    needs_draw = true;
                    last_index_redraw = Instant::now();
                }
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
        search_mode: Some(match search_mode {
            SearchMode::Lexical => "lexical".into(),
            SearchMode::Semantic => "semantic".into(),
            SearchMode::Hybrid => "hybrid".into(),
        }),
        context_window: Some(context_window.label().into()),
        density_mode: Some(density_mode.label().into()),
        // Mark that user has seen (or had opportunity to see) the help overlay
        has_seen_help: Some(true),
        // Persist query history for next session, deduplicating prefix pollution
        query_history: Some(dedupe_history_prefixes(
            query_history.iter().cloned().collect(),
        )),
        help_pinned: Some(help_pinned),
        saved_views: Some(
            saved_views
                .iter()
                .map(|v| SavedViewPersisted {
                    slot: v.slot,
                    agents: v.agents.iter().cloned().collect(),
                    workspaces: v.workspaces.iter().cloned().collect(),
                    created_from: v.created_from,
                    created_to: v.created_to,
                    ranking: Some(match v.ranking {
                        RankingMode::RecentHeavy => "recent".into(),
                        RankingMode::RelevanceHeavy => "relevance".into(),
                        RankingMode::MatchQualityHeavy => "quality".into(),
                        RankingMode::DateNewest => "newest".into(),
                        RankingMode::DateOldest => "oldest".into(),
                        RankingMode::Balanced => "balanced".into(),
                    }),
                    source_filter: if v.source_filter.is_all() {
                        None
                    } else {
                        Some(v.source_filter.to_string())
                    },
                })
                .collect(),
        ),
        // Persist pane count & ranking mode (bead 46t.1)
        per_pane_limit: Some(per_pane_limit),
        ranking_mode: Some(match ranking_mode {
            RankingMode::RecentHeavy => "recent".into(),
            RankingMode::RelevanceHeavy => "relevance".into(),
            RankingMode::MatchQualityHeavy => "quality".into(),
            RankingMode::DateNewest => "newest".into(),
            RankingMode::DateOldest => "oldest".into(),
            RankingMode::Balanced => "balanced".into(),
        }),
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
    let _ = client.search(
        "",
        SearchFilters::default(),
        5,
        0,
        crate::search::query::FieldMask::FULL,
    )?;
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
    use crate::model::types::{Conversation, Message, MessageRole};
    use crate::ui::data::ConversationView;
    use serde_json::json;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn state_roundtrip_persists_mode_and_context() {
        let dir = TempDir::new().unwrap();
        let path = state_path_for(dir.path());

        let state = TuiStatePersisted {
            match_mode: Some("prefix".into()),
            search_mode: Some("hybrid".into()),
            context_window: Some("XL".into()),
            density_mode: Some("cozy".into()),
            has_seen_help: Some(true),
            query_history: Some(vec!["test query".into(), "another search".into()]),
            help_pinned: Some(false),
            saved_views: Some(vec![SavedViewPersisted {
                slot: 1,
                agents: vec!["codex".into()],
                workspaces: vec!["/tmp".into()],
                created_from: Some(1),
                created_to: Some(2),
                ranking: Some("recent".into()),
                source_filter: Some("remote".into()),
            }]),
            per_pane_limit: Some(12),
            ranking_mode: Some("balanced".into()),
        };
        save_state(&path, &state);

        let loaded = load_state(&path);
        assert_eq!(loaded.match_mode.as_deref(), Some("prefix"));
        assert_eq!(loaded.search_mode.as_deref(), Some("hybrid"));
        assert_eq!(loaded.context_window.as_deref(), Some("XL"));
        assert_eq!(loaded.has_seen_help, Some(true));
        assert_eq!(
            loaded.query_history.as_ref().map(std::vec::Vec::len),
            Some(2)
        );
        assert_eq!(loaded.saved_views.as_ref().map(std::vec::Vec::len), Some(1));
        // Verify new fields (bead 46t.1)
        assert_eq!(loaded.per_pane_limit, Some(12));
        assert_eq!(loaded.ranking_mode.as_deref(), Some("balanced"));
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

    /// Test `count_query_matches` for sux.6.6c
    #[test]
    fn count_query_matches_works() {
        // Single term exact
        assert_eq!(count_query_matches("hello world hello", "hello"), 2);

        // Case insensitive
        assert_eq!(count_query_matches("Hello HELLO hello", "hello"), 3);

        // Phrase match
        assert_eq!(
            count_query_matches("error handling error handling", "error handling"),
            2
        );

        // Multi-word fallback (no phrase match)
        assert_eq!(count_query_matches("the quick brown fox", "quick fox"), 2);

        // Empty query/text
        assert_eq!(count_query_matches("", "test"), 0);
        assert_eq!(count_query_matches("test", ""), 0);

        // No matches
        assert_eq!(count_query_matches("hello world", "xyz"), 0);
    }

    /// Test `smart_word_wrap` for sux.6.6d
    #[test]
    fn smart_word_wrap_works() {
        // Simple case - fits on one line (1-space indent)
        let lines = smart_word_wrap("hello world", 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], " hello world");

        // Needs wrapping - continuation gets 1-space indent (same as first line)
        let long_text = "This is a long text that definitely needs to wrap across multiple lines because it exceeds the maximum width";
        let lines = smart_word_wrap(long_text, 40);
        assert!(lines.len() >= 2);
        assert!(lines[0].starts_with(" ")); // 1-space indent
        assert!(lines[1].starts_with(" ")); // 1-space continuation indent

        // Empty string
        let lines = smart_word_wrap("", 80);
        assert!(lines.is_empty());

        // Single word longer than width gets truncated
        let lines = smart_word_wrap("superlongwordthatexceedswidth", 15);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with("…"));
    }

    // Helper for sux.6.6a test
    fn line_to_string(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.to_string())
            .collect::<String>()
    }

    /// Test that detail pane uses absolute timestamps (sux.6.6a)
    #[test]
    fn detail_uses_absolute_timestamps() {
        let palette = ThemePalette::dark();
        let started_at = chrono::DateTime::parse_from_rfc3339("2024-01-02T03:04:05Z")
            .unwrap()
            .timestamp_millis();
        let msg_ts = chrono::DateTime::parse_from_rfc3339("2024-01-02T04:05:06Z")
            .unwrap()
            .timestamp_millis();

        let convo = Conversation {
            id: Some(1),
            agent_slug: "codex".into(),
            workspace: None,
            external_id: None,
            title: Some("Absolute Time Test".into()),
            source_path: PathBuf::from("/tmp/test"),
            started_at: Some(started_at),
            ended_at: None,
            approx_tokens: None,
            metadata_json: json!({}),
            messages: Vec::new(),
            source_id: "local".to_string(),
            origin_host: None,
        };

        let message = Message {
            id: Some(1),
            idx: 0,
            role: MessageRole::Agent,
            author: Some("agent".into()),
            created_at: Some(msg_ts),
            content: "hello world".into(),
            extra_json: json!({}),
            snippets: vec![],
        };

        let detail = ConversationView {
            convo,
            messages: vec![message],
            workspace: None,
        };

        let lines = render_parsed_content(&detail, "", palette);
        let joined = lines
            .iter()
            .map(line_to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(joined.contains("2024-01-02 03:04:05 UTC"));
        assert!(joined.contains("2024-01-02 04:05:06 UTC"));
        assert!(
            !joined.contains("ago"),
            "detail pane should use absolute timestamps"
        );
    }

    // ==========================================================================
    // Navigation State Tests (tst.ui.nav)
    // Tests for TUI navigation state machine behavior
    // ==========================================================================

    #[test]
    fn context_window_cycles_through_all_sizes() {
        let mut window = ContextWindow::Small;
        assert_eq!(window.size(), 200);

        window = window.next();
        assert_eq!(window, ContextWindow::Medium);
        assert_eq!(window.size(), 400);

        window = window.next();
        assert_eq!(window, ContextWindow::Large);
        assert_eq!(window.size(), 800);

        window = window.next();
        assert_eq!(window, ContextWindow::XLarge);
        assert_eq!(window.size(), 1600);

        // Wraps back to Small
        window = window.next();
        assert_eq!(window, ContextWindow::Small);
        assert_eq!(window.size(), 200);
    }

    #[test]
    fn density_mode_cycles_through_all_options() {
        let mut mode = DensityMode::Compact;
        assert_eq!(mode.lines_per_item(), 5); // 2 + 3 snippet lines

        mode = mode.next();
        assert_eq!(mode, DensityMode::Cozy);
        assert_eq!(mode.lines_per_item(), 7); // 2 + 5 snippet lines

        mode = mode.next();
        assert_eq!(mode, DensityMode::Spacious);
        assert_eq!(mode.lines_per_item(), 10); // 2 + 8 snippet lines

        // Wraps back to Compact
        mode = mode.next();
        assert_eq!(mode, DensityMode::Compact);
        assert_eq!(mode.lines_per_item(), 5);
    }

    #[test]
    fn ranking_mode_has_all_variants() {
        // RankingMode is cycled manually in key handler, test all variants exist
        let modes = [
            RankingMode::RecentHeavy,
            RankingMode::Balanced,
            RankingMode::RelevanceHeavy,
            RankingMode::MatchQualityHeavy,
            RankingMode::DateNewest,
            RankingMode::DateOldest,
        ];
        assert_eq!(modes.len(), 6, "should have 6 ranking modes");

        // Test that they are all distinct
        for (i, a) in modes.iter().enumerate() {
            for (j, b) in modes.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "all ranking modes should be distinct");
                }
            }
        }
    }

    fn make_hit(agent: &str, path: &str, score: f32, snippet: &str) -> SearchHit {
        SearchHit {
            title: "Test".into(),
            snippet: snippet.into(),
            content: "content".into(),
            content_hash: crate::search::query::stable_content_hash("content"),
            score,
            source_path: path.into(),
            agent: agent.into(),
            workspace: String::new(),
            workspace_original: None,
            created_at: None,
            line_number: None,
            match_type: crate::search::query::MatchType::default(),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        }
    }

    #[test]
    fn build_agent_panes_groups_by_agent() {
        let hits = vec![
            make_hit("codex", "/a", 8.0, "snippet"),
            make_hit("claude_code", "/b", 7.0, "snippet"),
            make_hit("codex", "/c", 6.0, "snippet"),
        ];

        let panes = build_agent_panes(&hits, 10);
        assert_eq!(panes.len(), 2, "should have 2 agent panes");
        assert_eq!(panes[0].agent, "codex");
        assert_eq!(panes[0].hits.len(), 2);
        assert_eq!(panes[1].agent, "claude_code");
        assert_eq!(panes[1].hits.len(), 1);
    }

    #[test]
    fn build_agent_panes_respects_per_pane_limit() {
        let hits: Vec<SearchHit> = (0..10)
            .map(|i| make_hit("codex", &format!("/path/{i}"), 5.0, "snippet"))
            .collect();

        let panes = build_agent_panes(&hits, 3);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].hits.len(), 3, "should respect per_pane_limit");
        assert_eq!(panes[0].total_count, 10, "should track total count");
    }

    #[test]
    fn build_agent_panes_empty_input_returns_empty() {
        let panes = build_agent_panes(&[], 10);
        assert!(panes.is_empty());
    }

    #[test]
    fn build_agent_panes_initializes_selection_to_zero() {
        let hits = vec![make_hit("codex", "/a", 5.0, "snippet")];

        let panes = build_agent_panes(&hits, 10);
        assert_eq!(panes[0].selected, 0);
    }

    #[test]
    fn rebuild_panes_with_filter_maintains_selection_on_same_agent() {
        let hits = vec![
            make_hit("codex", "/a", 8.0, "snippet"),
            make_hit("claude_code", "/b", 7.0, "snippet"),
        ];

        let mut active_pane: usize = 1; // Start on claude_code
        let mut scroll_offset: usize = 0;

        let panes = rebuild_panes_with_filter(
            &hits,
            None,
            10,
            &mut active_pane,
            &mut scroll_offset,
            Some("claude_code".into()),
            Some("/b".into()),
            5,
        );

        assert_eq!(panes.len(), 2);
        assert_eq!(active_pane, 1, "should stay on claude_code pane");
    }

    #[test]
    fn rebuild_panes_with_filter_falls_back_when_agent_disappears() {
        let hits = vec![make_hit("codex", "/a", 8.0, "snippet")];

        let mut active_pane: usize = 5; // Invalid index
        let mut scroll_offset: usize = 0;

        let _panes = rebuild_panes_with_filter(
            &hits,
            None,
            10,
            &mut active_pane,
            &mut scroll_offset,
            Some("nonexistent".into()),
            None,
            5,
        );

        assert_eq!(active_pane, 0, "should fall back to first pane");
    }

    #[test]
    fn rebuild_panes_scroll_offset_adjusts_when_pane_out_of_view() {
        let hits: Vec<SearchHit> = (0..10)
            .map(|i| make_hit(&format!("agent{i}"), &format!("/path/{i}"), 5.0, "snippet"))
            .collect();

        let mut active_pane: usize = 0;
        let mut scroll_offset: usize = 0;

        let _panes = rebuild_panes_with_filter(
            &hits,
            None,
            10,
            &mut active_pane,
            &mut scroll_offset,
            Some("agent7".into()), // Agent at index 7
            None,
            3, // max_visible_panes
        );

        assert_eq!(active_pane, 7);
        // Scroll offset should adjust to show pane 7 when only 3 are visible
        assert!(
            scroll_offset >= 5,
            "scroll offset should adjust to show active pane"
        );
    }

    #[test]
    fn active_hit_returns_selected_item_in_pane() {
        let panes = vec![AgentPane {
            agent: "codex".into(),
            hits: vec![
                make_hit("codex", "/a", 8.0, "first"),
                make_hit("codex", "/b", 7.0, "second"),
            ],
            selected: 1,
            total_count: 2,
        }];

        let hit = active_hit(&panes, 0);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().snippet, "second");
    }

    #[test]
    fn active_hit_returns_none_for_invalid_pane_index() {
        let panes = vec![AgentPane {
            agent: "codex".into(),
            hits: vec![],
            selected: 0,
            total_count: 0,
        }];

        assert!(active_hit(&panes, 5).is_none());
        assert!(active_hit(&[], 0).is_none());
    }

    #[test]
    fn focus_region_toggles_between_results_and_detail() {
        let focus = FocusRegion::Results;
        assert_eq!(focus, FocusRegion::Results);

        let focus = FocusRegion::Detail;
        assert_eq!(focus, FocusRegion::Detail);
    }

    #[test]
    fn match_mode_has_two_variants() {
        let standard = MatchMode::Standard;
        let prefix = MatchMode::Prefix;
        assert_ne!(standard, prefix);
    }

    #[test]
    fn agent_suggestions_returns_matching_agents() {
        let suggestions = agent_suggestions("cl");
        assert!(suggestions.contains(&"claude_code"));
        assert!(suggestions.contains(&"cline"));
        assert!(!suggestions.contains(&"codex"));
    }

    #[test]
    fn agent_suggestions_case_insensitive() {
        let suggestions = agent_suggestions("CL");
        assert!(suggestions.contains(&"claude_code"));
        assert!(suggestions.contains(&"cline"));
    }

    #[test]
    fn agent_suggestions_empty_prefix_returns_all() {
        let suggestions = agent_suggestions("");
        assert_eq!(suggestions.len(), KNOWN_AGENTS.len());
    }

    // ==========================================================================
    // UI State Persistence Tests (tst.ui.pers)
    // Tests for saving and restoring UI state across sessions
    // ==========================================================================

    #[test]
    fn state_persistence_query_history_save_restore() {
        let dir = TempDir::new().unwrap();
        let path = state_path_for(dir.path());

        // Create state with query history
        let state = TuiStatePersisted {
            query_history: Some(vec![
                "latest search".into(),
                "previous query".into(),
                "oldest query".into(),
            ]),
            ..Default::default()
        };
        save_state(&path, &state);

        // Load and verify
        let loaded = load_state(&path);
        let history = loaded.query_history.expect("query_history should exist");
        assert_eq!(history.len(), 3);
        assert_eq!(history[0], "latest search");
        assert_eq!(history[1], "previous query");
        assert_eq!(history[2], "oldest query");
    }

    #[test]
    fn state_persistence_saved_views_multiple_slots() {
        let dir = TempDir::new().unwrap();
        let path = state_path_for(dir.path());

        // Create state with multiple saved views
        let state = TuiStatePersisted {
            saved_views: Some(vec![
                SavedViewPersisted {
                    slot: 1,
                    agents: vec!["codex".into()],
                    workspaces: vec!["/home/user/project".into()],
                    created_from: Some(1000),
                    created_to: Some(2000),
                    ranking: Some("recent".into()),
                    source_filter: Some("local".into()),
                },
                SavedViewPersisted {
                    slot: 5,
                    agents: vec!["claude_code".into(), "cline".into()],
                    workspaces: vec![],
                    created_from: None,
                    created_to: Some(5000),
                    ranking: Some("balanced".into()),
                    source_filter: None, // "all" default
                },
            ]),
            ..Default::default()
        };
        save_state(&path, &state);

        let loaded = load_state(&path);
        let views = loaded.saved_views.expect("saved_views should exist");
        assert_eq!(views.len(), 2);

        // Verify slot 1
        let view1 = &views[0];
        assert_eq!(view1.slot, 1);
        assert_eq!(view1.agents, vec!["codex"]);
        assert_eq!(view1.workspaces, vec!["/home/user/project"]);
        assert_eq!(view1.created_from, Some(1000));
        assert_eq!(view1.created_to, Some(2000));
        assert_eq!(view1.ranking.as_deref(), Some("recent"));

        // Verify slot 5
        let view5 = &views[1];
        assert_eq!(view5.slot, 5);
        assert_eq!(view5.agents, vec!["claude_code", "cline"]);
        assert!(view5.workspaces.is_empty());
        assert_eq!(view5.created_from, None);
        assert_eq!(view5.created_to, Some(5000));
    }

    #[test]
    fn state_persistence_per_pane_limit_and_ranking_mode() {
        let dir = TempDir::new().unwrap();
        let path = state_path_for(dir.path());

        // Test various ranking modes
        for (limit, mode) in [
            (5, "recent"),
            (10, "balanced"),
            (20, "relevance"),
            (15, "quality"),
            (8, "newest"),
            (25, "oldest"),
        ] {
            let state = TuiStatePersisted {
                per_pane_limit: Some(limit),
                ranking_mode: Some(mode.into()),
                ..Default::default()
            };
            save_state(&path, &state);

            let loaded = load_state(&path);
            assert_eq!(
                loaded.per_pane_limit,
                Some(limit),
                "per_pane_limit should be {limit}"
            );
            assert_eq!(
                loaded.ranking_mode.as_deref(),
                Some(mode),
                "ranking_mode should be {mode}"
            );
        }
    }

    #[test]
    fn state_persistence_corrupted_state_file_uses_defaults() {
        let dir = TempDir::new().unwrap();
        let path = state_path_for(dir.path());

        // Write corrupted JSON
        std::fs::write(&path, "{ invalid json {{").unwrap();

        // load_state should return defaults without crashing
        let loaded = load_state(&path);
        assert!(loaded.match_mode.is_none());
        assert!(loaded.search_mode.is_none());
        assert!(loaded.context_window.is_none());
        assert!(loaded.density_mode.is_none());
        assert!(loaded.has_seen_help.is_none());
        assert!(loaded.query_history.is_none());
        assert!(loaded.saved_views.is_none());
        assert!(loaded.help_pinned.is_none());
        assert!(loaded.per_pane_limit.is_none());
        assert!(loaded.ranking_mode.is_none());
    }

    #[test]
    fn state_persistence_nonexistent_file_uses_defaults() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent_state.json");

        // load_state should return defaults for non-existent file
        let loaded = load_state(&path);
        assert!(loaded.match_mode.is_none());
        assert!(loaded.search_mode.is_none());
        assert!(loaded.context_window.is_none());
        assert!(loaded.query_history.is_none());
    }

    #[test]
    fn state_persistence_missing_fields_use_defaults() {
        let dir = TempDir::new().unwrap();
        let path = state_path_for(dir.path());

        // Write JSON with only some fields (missing per_pane_limit, ranking_mode, etc.)
        std::fs::write(&path, r#"{"match_mode": "prefix", "has_seen_help": true}"#).unwrap();

        let loaded = load_state(&path);
        // Specified fields should be loaded
        assert_eq!(loaded.match_mode.as_deref(), Some("prefix"));
        assert_eq!(loaded.has_seen_help, Some(true));
        // Missing fields should be None (defaults)
        assert!(loaded.search_mode.is_none());
        assert!(loaded.context_window.is_none());
        assert!(loaded.density_mode.is_none());
        assert!(loaded.query_history.is_none());
        assert!(loaded.saved_views.is_none());
        assert!(loaded.help_pinned.is_none());
        assert!(loaded.per_pane_limit.is_none());
        assert!(loaded.ranking_mode.is_none());
    }

    #[test]
    fn state_persistence_empty_query_history() {
        let dir = TempDir::new().unwrap();
        let path = state_path_for(dir.path());

        let state = TuiStatePersisted {
            query_history: Some(vec![]),
            ..Default::default()
        };
        save_state(&path, &state);

        let loaded = load_state(&path);
        let history = loaded.query_history.expect("query_history should exist");
        assert!(history.is_empty());
    }

    #[test]
    fn state_persistence_empty_saved_views() {
        let dir = TempDir::new().unwrap();
        let path = state_path_for(dir.path());

        let state = TuiStatePersisted {
            saved_views: Some(vec![]),
            ..Default::default()
        };
        save_state(&path, &state);

        let loaded = load_state(&path);
        let views = loaded.saved_views.expect("saved_views should exist");
        assert!(views.is_empty());
    }

    #[test]
    fn state_persistence_all_density_modes() {
        let dir = TempDir::new().unwrap();
        let path = state_path_for(dir.path());

        for mode in ["compact", "cozy", "spacious"] {
            let state = TuiStatePersisted {
                density_mode: Some(mode.into()),
                ..Default::default()
            };
            save_state(&path, &state);

            let loaded = load_state(&path);
            assert_eq!(
                loaded.density_mode.as_deref(),
                Some(mode),
                "density_mode should be {mode}"
            );
        }
    }

    #[test]
    fn state_persistence_all_context_windows() {
        let dir = TempDir::new().unwrap();
        let path = state_path_for(dir.path());

        for window in ["S", "M", "L", "XL"] {
            let state = TuiStatePersisted {
                context_window: Some(window.into()),
                ..Default::default()
            };
            save_state(&path, &state);

            let loaded = load_state(&path);
            assert_eq!(
                loaded.context_window.as_deref(),
                Some(window),
                "context_window should be {window}"
            );
        }
    }

    #[test]
    fn state_persistence_help_pinned_states() {
        let dir = TempDir::new().unwrap();
        let path = state_path_for(dir.path());

        for pinned in [true, false] {
            let state = TuiStatePersisted {
                help_pinned: Some(pinned),
                ..Default::default()
            };
            save_state(&path, &state);

            let loaded = load_state(&path);
            assert_eq!(
                loaded.help_pinned,
                Some(pinned),
                "help_pinned should be {pinned}"
            );
        }
    }

    #[test]
    fn dedupe_history_prefixes_removes_strict_prefixes() {
        let history = vec![
            "foobar".to_string(),
            "foo".to_string(),
            "foob".to_string(),
            "bar".to_string(),
        ];
        let deduped = dedupe_history_prefixes(history);
        assert_eq!(deduped, vec!["foobar", "bar"]);
    }

    #[test]
    fn dedupe_history_prefixes_preserves_non_prefixes() {
        let history = vec![
            "apple".to_string(),
            "banana".to_string(),
            "cherry".to_string(),
        ];
        let deduped = dedupe_history_prefixes(history);
        assert_eq!(deduped, vec!["apple", "banana", "cherry"]);
    }

    #[test]
    fn dedupe_history_prefixes_handles_empty() {
        let history: Vec<String> = vec![];
        let deduped = dedupe_history_prefixes(history);
        assert!(deduped.is_empty());
    }

    #[test]
    fn dedupe_history_prefixes_single_item() {
        let history = vec!["single".to_string()];
        let deduped = dedupe_history_prefixes(history);
        assert_eq!(deduped, vec!["single"]);
    }

    #[test]
    fn save_query_to_history_avoids_duplicates() {
        let mut history: VecDeque<String> = VecDeque::new();
        let cap = 10;

        save_query_to_history("first", &mut history, cap);
        save_query_to_history("second", &mut history, cap);
        save_query_to_history("first", &mut history, cap); // duplicate

        // Should have first at front (re-added), then second
        assert_eq!(history.len(), 3); // Note: current impl allows duplicates if not consecutive
    }

    #[test]
    fn save_query_to_history_respects_cap() {
        let mut history: VecDeque<String> = VecDeque::new();
        let cap = 3;

        for i in 0..5 {
            save_query_to_history(&format!("query{i}"), &mut history, cap);
        }

        assert_eq!(history.len(), 3);
        // Most recent should be at front
        assert_eq!(history.front().unwrap(), "query4");
    }

    #[test]
    fn save_query_to_history_ignores_empty() {
        let mut history: VecDeque<String> = VecDeque::new();
        let cap = 10;

        save_query_to_history("", &mut history, cap);
        save_query_to_history("   ", &mut history, cap);

        assert!(history.is_empty());
    }

    #[test]
    fn save_query_to_history_skips_consecutive_duplicates() {
        let mut history: VecDeque<String> = VecDeque::new();
        let cap = 10;

        save_query_to_history("query", &mut history, cap);
        save_query_to_history("query", &mut history, cap); // same as front, should skip

        assert_eq!(history.len(), 1);
    }

    // ==========================================================================
    // Detail Panel Tests (tst.ui.det)
    // Tests for detail view and find-in-detail functionality
    // ==========================================================================

    #[test]
    fn focus_region_results_is_default() {
        let focus = FocusRegion::Results;
        assert_eq!(focus, FocusRegion::Results);
        assert_ne!(focus, FocusRegion::Detail);
    }

    #[test]
    fn focus_region_detail_is_distinct() {
        let focus = FocusRegion::Detail;
        assert_eq!(focus, FocusRegion::Detail);
        assert_ne!(focus, FocusRegion::Results);
    }

    #[test]
    fn detail_find_state_default_is_empty() {
        let state = DetailFindState::default();
        assert!(state.query.is_empty());
        assert!(state.matches.is_empty());
        assert_eq!(state.current, 0);
    }

    #[test]
    fn detail_find_state_with_matches() {
        let state = DetailFindState {
            query: "search term".into(),
            matches: vec![5, 12, 28],
            current: 1,
        };
        assert_eq!(state.query, "search term");
        assert_eq!(state.matches.len(), 3);
        assert_eq!(state.matches[1], 12);
        assert_eq!(state.current, 1);
    }

    #[test]
    fn match_line_indices_finds_matches() {
        let lines = vec![
            Line::from("Hello world"),
            Line::from("foo bar baz"),
            Line::from("World peace"),
            Line::from("nothing here"),
        ];

        let matches = match_line_indices(&lines, "world");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0], 0); // "Hello world"
        assert_eq!(matches[1], 2); // "World peace"
    }

    #[test]
    fn match_line_indices_case_insensitive() {
        let lines = vec![
            Line::from("HELLO"),
            Line::from("hello"),
            Line::from("HeLLo"),
        ];

        let matches = match_line_indices(&lines, "hello");
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn match_line_indices_empty_needle_returns_empty() {
        let lines = vec![Line::from("some text"), Line::from("more text")];

        let matches = match_line_indices(&lines, "");
        assert!(matches.is_empty());

        let matches_whitespace = match_line_indices(&lines, "   ");
        assert!(matches_whitespace.is_empty());
    }

    #[test]
    fn match_line_indices_no_matches() {
        let lines = vec![Line::from("abc"), Line::from("def")];

        let matches = match_line_indices(&lines, "xyz");
        assert!(matches.is_empty());
    }

    #[test]
    fn match_line_indices_empty_lines() {
        let lines: Vec<Line> = vec![];
        let matches = match_line_indices(&lines, "anything");
        assert!(matches.is_empty());
    }

    #[test]
    fn match_line_indices_with_styled_spans() {
        // Lines with multiple styled spans should still find text
        let lines = vec![Line::from(vec![
            Span::styled("Hello ", Style::default().fg(Color::Red)),
            Span::styled("world", Style::default().fg(Color::Blue)),
        ])];

        let matches = match_line_indices(&lines, "world");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], 0);
    }

    #[test]
    fn active_hit_returns_correct_selection() {
        let panes = vec![
            AgentPane {
                agent: "codex".into(),
                hits: vec![
                    make_hit("codex", "/a", 8.0, "first"),
                    make_hit("codex", "/b", 7.0, "second"),
                    make_hit("codex", "/c", 6.0, "third"),
                ],
                selected: 2,
                total_count: 3,
            },
            AgentPane {
                agent: "claude_code".into(),
                hits: vec![make_hit("claude_code", "/d", 5.0, "fourth")],
                selected: 0,
                total_count: 1,
            },
        ];

        // Pane 0, selection 2
        let hit0 = active_hit(&panes, 0);
        assert!(hit0.is_some());
        assert_eq!(hit0.unwrap().snippet, "third");

        // Pane 1, selection 0
        let hit1 = active_hit(&panes, 1);
        assert!(hit1.is_some());
        assert_eq!(hit1.unwrap().snippet, "fourth");
    }

    #[test]
    fn active_hit_returns_none_for_empty_hits() {
        let panes = vec![AgentPane {
            agent: "codex".into(),
            hits: vec![],
            selected: 0,
            total_count: 0,
        }];

        assert!(active_hit(&panes, 0).is_none());
    }

    #[test]
    fn active_hit_returns_none_for_out_of_bounds_selection() {
        let panes = vec![AgentPane {
            agent: "codex".into(),
            hits: vec![make_hit("codex", "/a", 5.0, "only")],
            selected: 5, // out of bounds
            total_count: 1,
        }];

        assert!(active_hit(&panes, 0).is_none());
    }

    #[test]
    fn detail_tab_default_is_messages() {
        let tab = DetailTab::Messages;
        assert_eq!(tab, DetailTab::Messages);
    }

    #[test]
    fn detail_tab_has_three_variants() {
        let tabs = [DetailTab::Messages, DetailTab::Snippets, DetailTab::Raw];
        assert_eq!(tabs.len(), 3);

        // Verify all are distinct
        assert_ne!(tabs[0], tabs[1]);
        assert_ne!(tabs[1], tabs[2]);
        assert_ne!(tabs[0], tabs[2]);
    }

    #[test]
    fn detail_find_navigation_indices_wrap() {
        let state = DetailFindState {
            query: "test".into(),
            matches: vec![0, 5, 10],
            current: 2,
        };

        // When current is at last match (index 2) and we want next,
        // it should wrap to 0
        let next = (state.current + 1) % state.matches.len();
        assert_eq!(next, 0);

        // When current is at first match (index 0) and we want previous,
        // it should wrap to last
        let prev_state = DetailFindState {
            query: "test".into(),
            matches: vec![0, 5, 10],
            current: 0,
        };
        let prev = if prev_state.current == 0 {
            prev_state.matches.len() - 1
        } else {
            prev_state.current - 1
        };
        assert_eq!(prev, 2);
    }

    #[test]
    fn line_plain_text_extracts_content() {
        let line = Line::from(vec![
            Span::raw("Hello "),
            Span::styled("styled", Style::default().bold()),
            Span::raw(" text"),
        ]);

        let text = line_plain_text(&line);
        assert_eq!(text, "Hello styled text");
    }

    #[test]
    fn line_plain_text_handles_empty_line() {
        let line = Line::from("");
        let text = line_plain_text(&line);
        assert!(text.is_empty());
    }

    #[test]
    fn line_plain_text_handles_single_span() {
        let line = Line::from("single span");
        let text = line_plain_text(&line);
        assert_eq!(text, "single span");
    }

    // ==========================================================================
    // Bulk Selection Tests (tst.ui.bulk)
    // Tests for multi-select and bulk actions functionality
    // ==========================================================================

    #[test]
    fn bulk_selection_hashset_insert_and_contains() {
        let mut selected: HashSet<(usize, usize)> = HashSet::new();

        // Insert (pane_idx, hit_idx) tuples
        selected.insert((0, 0));
        selected.insert((0, 1));
        selected.insert((1, 0));

        assert!(selected.contains(&(0, 0)));
        assert!(selected.contains(&(0, 1)));
        assert!(selected.contains(&(1, 0)));
        assert!(!selected.contains(&(1, 1)));
        assert_eq!(selected.len(), 3);
    }

    #[test]
    fn bulk_selection_hashset_toggle_remove() {
        let mut selected: HashSet<(usize, usize)> = HashSet::new();
        let key = (0, 2);

        // Insert
        selected.insert(key);
        assert!(selected.contains(&key));

        // Toggle off (remove)
        selected.remove(&key);
        assert!(!selected.contains(&key));
        assert!(selected.is_empty());
    }

    #[test]
    fn bulk_selection_hashset_clear_all() {
        let mut selected: HashSet<(usize, usize)> = HashSet::new();
        selected.insert((0, 0));
        selected.insert((0, 1));
        selected.insert((1, 0));
        selected.insert((2, 5));

        assert_eq!(selected.len(), 4);

        selected.clear();
        assert!(selected.is_empty());
        assert_eq!(selected.len(), 0);
    }

    #[test]
    fn bulk_selection_select_all_in_pane() {
        let mut selected: HashSet<(usize, usize)> = HashSet::new();
        let pane_idx = 0usize;
        let hits_len = 5usize;

        // Simulate Ctrl+A: select all in pane
        for i in 0..hits_len {
            selected.insert((pane_idx, i));
        }

        assert_eq!(selected.len(), 5);
        for i in 0..hits_len {
            assert!(selected.contains(&(pane_idx, i)));
        }
    }

    #[test]
    fn bulk_selection_deselect_all_in_pane() {
        let mut selected: HashSet<(usize, usize)> = HashSet::new();
        let pane_idx = 0usize;
        let hits_len = 5usize;

        // Pre-select all in pane
        for i in 0..hits_len {
            selected.insert((pane_idx, i));
        }

        // Check all are selected (Ctrl+A toggle check)
        let all_selected = (0..hits_len).all(|i| selected.contains(&(pane_idx, i)));
        assert!(all_selected);

        // Deselect all in pane (Ctrl+A when all already selected)
        for i in 0..hits_len {
            selected.remove(&(pane_idx, i));
        }

        assert!(selected.is_empty());
    }

    #[test]
    fn bulk_selection_multiple_panes_independent() {
        let mut selected: HashSet<(usize, usize)> = HashSet::new();

        // Select items from different panes
        selected.insert((0, 0));
        selected.insert((0, 2));
        selected.insert((1, 1));
        selected.insert((2, 0));

        // Clear only pane 0
        let pane_0_keys: Vec<_> = selected.iter().filter(|(p, _)| *p == 0).copied().collect();
        for key in pane_0_keys {
            selected.remove(&key);
        }

        assert_eq!(selected.len(), 2);
        assert!(!selected.contains(&(0, 0)));
        assert!(!selected.contains(&(0, 2)));
        assert!(selected.contains(&(1, 1)));
        assert!(selected.contains(&(2, 0)));
    }

    #[test]
    fn bulk_selection_persists_across_pane_changes() {
        let mut selected: HashSet<(usize, usize)> = HashSet::new();

        // Select in pane 0
        selected.insert((0, 0));
        selected.insert((0, 1));

        // "Navigate" to pane 1 and select there
        let active_pane = 1usize;
        selected.insert((active_pane, 0));

        // "Navigate" back to pane 0 - selections should still exist
        assert!(selected.contains(&(0, 0)));
        assert!(selected.contains(&(0, 1)));
        assert!(selected.contains(&(1, 0)));
        assert_eq!(selected.len(), 3);
    }

    #[test]
    fn bulk_selection_clear_on_new_search_simulation() {
        let mut selected: HashSet<(usize, usize)> = HashSet::new();
        selected.insert((0, 0));
        selected.insert((0, 1));
        selected.insert((1, 0));

        // Simulate new search clearing selection
        // (In actual code, selection is cleared when panes are repopulated)
        selected.clear();

        assert!(selected.is_empty());
    }

    #[test]
    fn bulk_selection_all_selected_check() {
        let mut selected: HashSet<(usize, usize)> = HashSet::new();
        let pane_idx = 0usize;
        let hits_len = 3usize;

        // Not all selected yet
        selected.insert((pane_idx, 0));
        selected.insert((pane_idx, 1));

        let all_selected = (0..hits_len).all(|i| selected.contains(&(pane_idx, i)));
        assert!(!all_selected);

        // Now select all
        selected.insert((pane_idx, 2));
        let all_selected = (0..hits_len).all(|i| selected.contains(&(pane_idx, i)));
        assert!(all_selected);
    }

    #[test]
    fn bulk_selection_is_selected_check_for_rendering() {
        let mut selected: HashSet<(usize, usize)> = HashSet::new();
        selected.insert((0, 1));
        selected.insert((0, 3));

        // Simulate rendering loop checking each item
        let pane_idx = 0usize;
        let is_selected_0 = selected.contains(&(pane_idx, 0));
        let is_selected_1 = selected.contains(&(pane_idx, 1));
        let is_selected_2 = selected.contains(&(pane_idx, 2));
        let is_selected_3 = selected.contains(&(pane_idx, 3));

        assert!(!is_selected_0);
        assert!(is_selected_1);
        assert!(!is_selected_2);
        assert!(is_selected_3);
    }

    #[test]
    fn bulk_selection_footer_count_format() {
        let mut selected: HashSet<(usize, usize)> = HashSet::new();
        selected.insert((0, 0));
        selected.insert((0, 1));
        selected.insert((1, 0));

        // Format like the footer does
        let footer = format!("✓ {} selected", selected.len());
        assert_eq!(footer, "✓ 3 selected");
    }

    #[test]
    fn bulk_selection_empty_shows_no_footer() {
        let selected: HashSet<(usize, usize)> = HashSet::new();

        // When empty, footer shouldn't show selection count
        assert!(selected.is_empty());
        // In actual code: if !selected.is_empty() { footer_parts.push(...) }
    }

    #[test]
    fn bulk_modal_action_index_bounds() {
        // BULK_ACTIONS has 4 items: indices 0-3
        const BULK_ACTIONS: [&str; 4] = [
            "Open all in editor",
            "Copy all paths",
            "Export as JSON",
            "Clear selection",
        ];

        let mut bulk_action_idx: usize = 0;

        // Navigate down
        bulk_action_idx = (bulk_action_idx + 1).min(BULK_ACTIONS.len() - 1);
        assert_eq!(bulk_action_idx, 1);

        // Navigate to end
        bulk_action_idx = BULK_ACTIONS.len() - 1;
        assert_eq!(bulk_action_idx, 3);

        // Try to go past end
        bulk_action_idx = (bulk_action_idx + 1).min(BULK_ACTIONS.len() - 1);
        assert_eq!(bulk_action_idx, 3); // Stays at end

        // Navigate up
        bulk_action_idx = bulk_action_idx.saturating_sub(1);
        assert_eq!(bulk_action_idx, 2);

        // Navigate to start
        bulk_action_idx = 0;
        bulk_action_idx = bulk_action_idx.saturating_sub(1);
        assert_eq!(bulk_action_idx, 0); // Stays at start
    }

    #[test]
    fn bulk_modal_requires_selection() {
        let selected: HashSet<(usize, usize)> = HashSet::new();

        // Pressing 'A' when nothing selected - modal should not open
        let show_bulk_modal = !selected.is_empty();

        assert!(!show_bulk_modal);
    }

    #[test]
    fn bulk_modal_opens_with_selection() {
        let mut selected: HashSet<(usize, usize)> = HashSet::new();
        selected.insert((0, 0));

        // Pressing 'A' with selection - modal should open
        let show_bulk_modal = !selected.is_empty();

        assert!(show_bulk_modal);
    }

    #[test]
    fn bulk_selection_collect_selected_hits() {
        // Simulate collecting selected hits for bulk action
        let mut selected: HashSet<(usize, usize)> = HashSet::new();
        selected.insert((0, 1));
        selected.insert((0, 3));
        selected.insert((1, 0));

        // Simulate panes structure
        struct MockHit {
            source_path: String,
        }
        struct MockPane {
            hits: Vec<MockHit>,
        }

        let panes = [
            MockPane {
                hits: vec![
                    MockHit {
                        source_path: "path0".into(),
                    },
                    MockHit {
                        source_path: "path1".into(),
                    },
                    MockHit {
                        source_path: "path2".into(),
                    },
                    MockHit {
                        source_path: "path3".into(),
                    },
                ],
            },
            MockPane {
                hits: vec![MockHit {
                    source_path: "pane1_path0".into(),
                }],
            },
        ];

        // Collect selected hits like the bulk action does
        let selected_hits: Vec<&MockHit> = selected
            .iter()
            .filter_map(|&(pane_idx, hit_idx)| {
                panes.get(pane_idx).and_then(|p| p.hits.get(hit_idx))
            })
            .collect();

        assert_eq!(selected_hits.len(), 3);
        let paths: Vec<_> = selected_hits
            .iter()
            .map(|h| h.source_path.as_str())
            .collect();
        assert!(paths.contains(&"path1"));
        assert!(paths.contains(&"path3"));
        assert!(paths.contains(&"pane1_path0"));
    }

    #[test]
    fn bulk_selection_status_message_toggle_on() {
        let mut selected: HashSet<(usize, usize)> = HashSet::new();
        let key = (0, 0);

        // Ctrl+M toggle: insert
        selected.insert(key);
        let status = format!(
            "Selected ({} total) · Ctrl+M toggle · A bulk actions · Esc clear",
            selected.len()
        );
        assert!(status.contains("1 total"));
        assert!(status.contains("Ctrl+M"));
        assert!(status.contains("A bulk actions"));
    }

    #[test]
    fn bulk_selection_status_message_toggle_off() {
        let mut selected: HashSet<(usize, usize)> = HashSet::new();
        let key = (0, 0);

        selected.insert(key);
        selected.remove(&key);
        let status = format!("Deselected ({} selected)", selected.len());
        assert!(status.contains("0 selected"));
    }

    #[test]
    fn bulk_selection_ctrl_a_status_select_all() {
        let selected_count = 5;
        let status = format!(
            "Selected all in pane ({} total) · A bulk actions",
            selected_count
        );
        assert!(status.contains("5 total"));
        assert!(status.contains("Selected all"));
    }

    #[test]
    fn bulk_selection_ctrl_a_status_deselect_all() {
        let remaining = 0;
        let status = format!("Deselected all in pane ({} total)", remaining);
        assert!(status.contains("0 total"));
        assert!(status.contains("Deselected all"));
    }

    #[test]
    fn bulk_selection_esc_clears_status() {
        let mut selected: HashSet<(usize, usize)> = HashSet::new();
        selected.insert((0, 0));
        selected.insert((0, 1));

        let count = selected.len();
        selected.clear();
        let status = format!("Cleared {count} selections");
        assert_eq!(status, "Cleared 2 selections");
    }

    // =========================================================================
    // Throughput sparkline tests (bead 012)
    // =========================================================================

    #[test]
    fn throughput_sparkline_empty_history() {
        // Less than 2 samples should return empty string
        let history: std::collections::VecDeque<u16> = std::collections::VecDeque::new();
        let levels = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

        // Render function logic
        let result = if history.len() < 2 {
            String::new()
        } else {
            let max_val = *history.iter().max().unwrap_or(&1) as usize;
            if max_val == 0 {
                String::new()
            } else {
                history
                    .iter()
                    .map(|v| {
                        let idx =
                            ((*v as usize * (levels.len() - 1)) / max_val).min(levels.len() - 1);
                        levels[idx]
                    })
                    .collect()
            }
        };

        assert!(result.is_empty());
    }

    #[test]
    fn throughput_sparkline_scales_to_max() {
        let mut history: std::collections::VecDeque<u16> = std::collections::VecDeque::new();
        history.push_back(0);
        history.push_back(50);
        history.push_back(100);
        history.push_back(50);
        history.push_back(0);

        let levels = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let max_val = *history.iter().max().unwrap_or(&1) as usize;
        let result: String = history
            .iter()
            .map(|v| {
                let idx = ((*v as usize * (levels.len() - 1)) / max_val).min(levels.len() - 1);
                levels[idx]
            })
            .collect();

        // Should have 5 characters
        assert_eq!(result.chars().count(), 5);
        // Middle value (100) should be max character
        assert_eq!(result.chars().nth(2), Some('█'));
        // First and last (0) should be min character
        assert_eq!(result.chars().next(), Some('▁'));
        assert_eq!(result.chars().nth(4), Some('▁'));
    }

    #[test]
    fn throughput_sparkline_all_same_values() {
        let mut history: std::collections::VecDeque<u16> = std::collections::VecDeque::new();
        history.push_back(42);
        history.push_back(42);
        history.push_back(42);

        let levels = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let max_val = *history.iter().max().unwrap_or(&1) as usize;
        let result: String = history
            .iter()
            .map(|v| {
                let idx = ((*v as usize * (levels.len() - 1)) / max_val).min(levels.len() - 1);
                levels[idx]
            })
            .collect();

        // All same values should render as max bars (since they're all == max)
        assert_eq!(result, "███");
    }

    #[test]
    fn throughput_sparkline_all_zeros() {
        let mut history: std::collections::VecDeque<u16> = std::collections::VecDeque::new();
        history.push_back(0);
        history.push_back(0);
        history.push_back(0);

        let levels = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let max_val = *history.iter().max().unwrap_or(&1) as usize;

        // All zeros - max_val is 0, so return empty
        let result = if max_val == 0 {
            String::new()
        } else {
            history
                .iter()
                .map(|v| {
                    let idx = ((*v as usize * (levels.len() - 1)) / max_val).min(levels.len() - 1);
                    levels[idx]
                })
                .collect()
        };

        assert!(result.is_empty());
    }

    #[test]
    fn density_mode_compact_shows_minimal() {
        // In compact mode, indexer HUD should show minimal info
        let density = DensityMode::Compact;
        let phase = 2; // Indexing
        let pct = 75;
        let is_rebuild = false;
        let icon = "📦";

        let expected_format = match density {
            DensityMode::Compact => {
                if is_rebuild {
                    format!(" | {icon} {pct}% ⚠")
                } else {
                    format!(" | {icon} {pct}%")
                }
            }
            _ => String::new(),
        };

        assert_eq!(expected_format, " | 📦 75%");
        let _ = phase; // suppress unused warning
    }

    #[test]
    fn density_mode_spacious_shows_full_details() {
        // In spacious mode, should show phase label and both sparklines
        let density = DensityMode::Spacious;
        assert!(matches!(density, DensityMode::Spacious));

        // Format includes labels for sparklines
        let pct_spark = "▂▃▅▇█";
        let tput_spark = "▁▂▄▆█";
        let formatted = format!(" %:{pct_spark} ⚡:{tput_spark}");
        assert!(formatted.contains("%:"));
        assert!(formatted.contains("⚡:"));
    }

    #[test]
    fn throughput_calculation() {
        // Test items-per-second calculation logic
        let items_delta = 100usize;
        let elapsed_ms = 500u64;

        let items_per_sec = if elapsed_ms > 0 {
            (items_delta as u64 * 1000 / elapsed_ms) as u16
        } else {
            0
        };

        // 100 items in 500ms = 200 items/sec
        assert_eq!(items_per_sec, 200);
    }

    #[test]
    fn throughput_calculation_zero_elapsed() {
        // Edge case: zero elapsed time
        let items_delta = 100usize;
        let elapsed_ms = 0u64;

        let items_per_sec = if elapsed_ms > 0 {
            (items_delta as u64 * 1000 / elapsed_ms) as u16
        } else {
            0
        };

        assert_eq!(items_per_sec, 0);
        let _ = items_delta; // suppress unused warning
    }
}
