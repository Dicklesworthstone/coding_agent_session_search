//! FrankenTUI (ftui) application model for cass TUI.
//!
//! Defines the Elm-architecture types: [`CassApp`] (Model), [`CassMsg`] (Message),
//! and service trait boundaries.  This module is the foundational type definition
//! that all subsequent ftui feature work builds on (bead 2noh9.2.2).
//!
//! # Architecture
//!
//! ```text
//!   Event (key/mouse/resize/tick)
//!        │
//!        ▼
//!   CassMsg (from Event)
//!        │
//!        ▼
//!   CassApp::update(&mut self, msg) -> Cmd<CassMsg>
//!        │
//!        ├── Pure state transition  → Cmd::none()
//!        ├── Async search           → Cmd::task(SearchService::execute)
//!        ├── Spawn editor           → Cmd::task(EditorService::open)
//!        ├── Export                  → Cmd::task(ExportService::export)
//!        └── Persist state          → Cmd::save_state()
//!
//!   CassApp::view(&self, frame)
//!        │
//!        ▼
//!   Renders current state to ftui Frame
//! ```

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::search::model_manager::SemanticAvailability;
use crate::search::query::{QuerySuggestion, SearchFilters, SearchHit, SearchMode};
use crate::sources::provenance::SourceFilter;
use crate::storage::sqlite::SqliteStorage;
use crate::ui::components::export_modal::{ExportModalState, ExportProgress};
use crate::ui::components::palette::{PaletteAction, PaletteState, default_actions};
use crate::ui::components::pills::Pill;
use crate::ui::components::toast::ToastManager;
use crate::ui::data::{ConversationView, InputMode};
use crate::update_check::UpdateInfo;
use ftui::widgets::Widget;
use ftui::widgets::block::{Alignment, Block};
use ftui::widgets::borders::{BorderType, Borders};
use ftui::widgets::paragraph::Paragraph;

// ---------------------------------------------------------------------------
// Re-export ftui primitives through the adapter
// ---------------------------------------------------------------------------
use super::ftui_adapter::{Constraint, Flex, Rect};
use super::style_system::{self, StyleContext, StyleOptions, UiThemePreset};
use ftui::widgets::focus::{FocusId, FocusManager};

/// Well-known focus node IDs for the cass TUI layout.
pub mod focus_ids {
    use super::FocusId;
    pub const SEARCH_BAR: FocusId = 1;
    pub const RESULTS_LIST: FocusId = 2;
    pub const DETAIL_PANE: FocusId = 3;
    pub const COMMAND_PALETTE: FocusId = 10;
    pub const HELP_OVERLAY: FocusId = 11;
    pub const EXPORT_MODAL: FocusId = 12;
    pub const CONSENT_DIALOG: FocusId = 13;
    pub const GROUP_PALETTE: u32 = 100;
    pub const GROUP_HELP: u32 = 101;
    pub const GROUP_EXPORT: u32 = 102;
    pub const GROUP_CONSENT: u32 = 103;
}

// =========================================================================
// Enums (ported from tui.rs, canonical for ftui)
// =========================================================================

/// Which tab is active in the detail pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DetailTab {
    #[default]
    Messages,
    Snippets,
    Raw,
}

/// Text matching strategy for search queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MatchMode {
    #[default]
    Standard,
    Prefix,
}

/// How search results are ranked and ordered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RankingMode {
    RecentHeavy,
    #[default]
    Balanced,
    RelevanceHeavy,
    MatchQualityHeavy,
    DateNewest,
    DateOldest,
}

/// How much surrounding context to show per result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ContextWindow {
    Small,
    #[default]
    Medium,
    Large,
    XLarge,
}

/// Visual density of the result list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DensityMode {
    Compact,
    #[default]
    Cozy,
    Spacious,
}

/// Which pane currently holds keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FocusRegion {
    #[default]
    Results,
    Detail,
}

/// Responsive layout breakpoint based on terminal width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutBreakpoint {
    /// <100 cols: single pane with tab switching
    Narrow,
    /// 100-159 cols: stacked results/detail with adjustable ratio
    Medium,
    /// >=160 cols: side-by-side results + detail panes
    Wide,
}

impl LayoutBreakpoint {
    /// Classify from terminal width.
    pub fn from_width(cols: u16) -> Self {
        if cols >= 160 {
            Self::Wide
        } else if cols >= 100 {
            Self::Medium
        } else {
            Self::Narrow
        }
    }
}

impl DensityMode {
    /// Lines per result row for this density.
    pub fn row_height(self) -> u16 {
        match self {
            Self::Compact => 1,
            Self::Cozy => 2,
            Self::Spacious => 3,
        }
    }
}

/// Inline find state within the detail pane.
#[derive(Clone, Debug, Default)]
pub struct DetailFindState {
    pub query: String,
    pub matches: Vec<u16>,
    pub current: usize,
}

/// One column of results, grouped by agent.
#[derive(Clone, Debug)]
pub struct AgentPane {
    pub agent: String,
    pub hits: Vec<SearchHit>,
    pub selected: usize,
    pub total_count: usize,
}

/// Persisted filters+ranking for a saved-view slot.
#[derive(Clone, Debug)]
pub struct SavedView {
    pub slot: u8,
    pub agents: HashSet<String>,
    pub workspaces: HashSet<String>,
    pub created_from: Option<i64>,
    pub created_to: Option<i64>,
    pub ranking: RankingMode,
    pub source_filter: SourceFilter,
}

// =========================================================================
// CassApp — the ftui Model
// =========================================================================

/// Top-level application state for the cass TUI.
///
/// Implements `ftui::Model` in the runtime skeleton (bead 2noh9.2.3).
/// Every field here is the single source of truth; rendering and
/// event handling derive all behavior from this struct.
pub struct CassApp {
    // -- Search & query ---------------------------------------------------
    /// Current search query text.
    pub query: String,
    /// Active search filters (agents, workspaces, time range, source).
    pub filters: SearchFilters,
    /// Last search results (flat list, before pane grouping).
    pub results: Vec<SearchHit>,
    /// Results grouped into per-agent panes.
    pub panes: Vec<AgentPane>,
    /// Currently active pane index.
    pub active_pane: usize,
    /// Scroll offset within the pane list.
    pub pane_scroll_offset: usize,
    /// Items shown per pane.
    pub per_pane_limit: usize,
    /// Whether wildcard fallback was triggered for the current query.
    pub wildcard_fallback: bool,
    /// Did-you-mean / filter suggestions for the current query.
    pub suggestions: Vec<QuerySuggestion>,
    /// Elapsed time of the last search (for latency badge).
    pub last_search_ms: Option<u128>,
    /// Which search mode is active (lexical / semantic / hybrid).
    pub search_mode: SearchMode,
    /// Text matching strategy.
    pub match_mode: MatchMode,
    /// Result ordering strategy.
    pub ranking_mode: RankingMode,
    /// Context window size.
    pub context_window: ContextWindow,

    // -- Focus & input ----------------------------------------------------
    /// What the user is currently typing into.
    pub input_mode: InputMode,
    /// Ephemeral input buffer for filter prompts.
    pub input_buffer: String,
    /// Which pane region has keyboard focus (legacy compat).
    pub focus_region: FocusRegion,
    /// FocusGraph-based navigation manager.
    pub focus_manager: FocusManager,
    /// Cursor position within query history.
    pub history_cursor: Option<usize>,
    /// Past query strings (most recent first), deduplicated.
    pub query_history: VecDeque<String>,
    /// Local pane filter text (/ key in results).
    pub pane_filter: Option<String>,

    // -- Multi-select -----------------------------------------------------
    /// Set of (pane_index, item_index) pairs for multi-selected items.
    pub selected: HashSet<(usize, usize)>,

    // -- Detail view ------------------------------------------------------
    /// Scroll position in the detail pane.
    pub detail_scroll: u16,
    /// Active tab in the detail pane.
    pub detail_tab: DetailTab,
    /// Inline find state within the detail pane.
    pub detail_find: Option<DetailFindState>,
    /// Whether the detail drill-in modal is open.
    pub show_detail_modal: bool,
    /// Scroll position within the detail modal.
    pub modal_scroll: u16,
    /// Cached conversation for the currently selected result.
    pub cached_detail: Option<(String, ConversationView)>,

    // -- Display & theming ------------------------------------------------
    /// Whether dark theme is active.
    pub theme_dark: bool,
    /// Active ftui theme preset.
    pub theme_preset: UiThemePreset,
    /// Runtime style options derived from environment + user toggles.
    pub style_options: StyleOptions,
    /// Whether fancy (rounded) borders are enabled.
    pub fancy_borders: bool,
    /// Visual density mode.
    pub density_mode: DensityMode,
    /// Saved context window before peek-XL override.
    pub peek_window_saved: Option<ContextWindow>,

    // -- Modals & overlays ------------------------------------------------
    /// Whether the help overlay is visible.
    pub show_help: bool,
    /// Scroll position within the help overlay.
    pub help_scroll: u16,
    /// Whether the help strip is pinned.
    pub help_pinned: bool,
    /// Whether the export modal is visible.
    pub show_export_modal: bool,
    /// State of the export modal form.
    pub export_modal_state: Option<ExportModalState>,
    /// Whether the bulk actions modal is visible.
    pub show_bulk_modal: bool,
    /// Whether the consent dialog (model download) is visible.
    pub show_consent_dialog: bool,
    /// Semantic search availability state.
    pub semantic_availability: SemanticAvailability,
    /// Whether the source filter popup menu is open.
    pub source_filter_menu_open: bool,
    /// Command palette state.
    pub palette_state: PaletteState,

    // -- Notifications ----------------------------------------------------
    /// Toast notification manager.
    pub toast_manager: ToastManager,

    // -- Animation & timing -----------------------------------------------
    /// Start time of the reveal animation.
    pub reveal_anim_start: Option<Instant>,
    /// End time of the focus-flash indicator.
    pub focus_flash_until: Option<Instant>,
    /// End time of the peek badge indicator.
    pub peek_badge_until: Option<Instant>,
    /// Last tick timestamp for animation frame delta.
    pub last_tick: Instant,
    /// When state became dirty (for debounced persistence).
    pub dirty_since: Option<Instant>,
    /// When query/filters changed (for debounced search, 60ms).
    pub search_dirty_since: Option<Instant>,
    /// Current spinner frame index.
    pub spinner_frame: usize,

    // -- Saved views ------------------------------------------------------
    /// Up to 9 saved filter+ranking presets (Ctrl+1..9).
    pub saved_views: Vec<SavedView>,

    // -- Layout hit regions (for mouse) -----------------------------------
    /// Last rendered detail area rectangle.
    pub last_detail_area: Option<Rect>,
    /// Last rendered pane rectangles.
    pub last_pane_rects: Vec<Rect>,
    /// Last rendered pill hit-test rectangles.
    pub last_pill_rects: Vec<(Rect, Pill)>,

    // -- Lazy-loaded services ---------------------------------------------
    /// Database reader (initialized on first use).
    pub db_reader: Option<Arc<SqliteStorage>>,
    /// Known workspace list (populated on first filter prompt).
    pub known_workspaces: Option<Vec<String>>,

    // -- Status line ------------------------------------------------------
    /// Footer status text.
    pub status: String,
}

impl Default for CassApp {
    fn default() -> Self {
        Self {
            query: String::new(),
            filters: SearchFilters::default(),
            results: Vec::new(),
            panes: Vec::new(),
            active_pane: 0,
            pane_scroll_offset: 0,
            per_pane_limit: 10,
            wildcard_fallback: false,
            suggestions: Vec::new(),
            last_search_ms: None,
            search_mode: SearchMode::default(),
            match_mode: MatchMode::default(),
            ranking_mode: RankingMode::default(),
            context_window: ContextWindow::default(),
            input_mode: InputMode::Query,
            input_buffer: String::new(),
            focus_region: FocusRegion::default(),
            focus_manager: FocusManager::new(),
            history_cursor: None,
            query_history: VecDeque::with_capacity(50),
            pane_filter: None,
            selected: HashSet::new(),
            detail_scroll: 0,
            detail_tab: DetailTab::default(),
            detail_find: None,
            show_detail_modal: false,
            modal_scroll: 0,
            cached_detail: None,
            theme_dark: true,
            theme_preset: UiThemePreset::Dark,
            style_options: StyleOptions::from_env(),
            fancy_borders: true,
            density_mode: DensityMode::default(),
            peek_window_saved: None,
            show_help: false,
            help_scroll: 0,
            help_pinned: false,
            show_export_modal: false,
            export_modal_state: None,
            show_bulk_modal: false,
            show_consent_dialog: false,
            semantic_availability: SemanticAvailability::NotInstalled,
            source_filter_menu_open: false,
            palette_state: PaletteState::new(default_actions()),
            toast_manager: ToastManager::default(),
            reveal_anim_start: None,
            focus_flash_until: None,
            peek_badge_until: None,
            last_tick: Instant::now(),
            dirty_since: None,
            search_dirty_since: None,
            spinner_frame: 0,
            saved_views: Vec::new(),
            last_detail_area: None,
            last_pane_rects: Vec::new(),
            last_pill_rects: Vec::new(),
            db_reader: None,
            known_workspaces: None,
            status: String::new(),
        }
    }
}

impl CassApp {
    fn resolved_style_context(&self) -> StyleContext {
        let mut options = self.style_options;
        options.preset = self.theme_preset;
        options.dark_mode = self.theme_dark;
        StyleContext::from_options(options)
    }

    fn selected_hit(&self) -> Option<&SearchHit> {
        if let Some(pane) = self.panes.get(self.active_pane) {
            return pane.hits.get(pane.selected);
        }
        self.results.first()
    }

    /// Render the results list pane with density-aware row heights.
    #[allow(clippy::too_many_arguments)]
    fn render_results_pane(
        &self,
        frame: &mut super::ftui_adapter::Frame,
        area: Rect,
        hits: &[SearchHit],
        selected_idx: usize,
        row_h: u16,
        border_type: BorderType,
        styles: &StyleContext,
        pane_style: ftui::Style,
        pane_focused_style: ftui::Style,
        row_style: ftui::Style,
        row_alt_style: ftui::Style,
        row_selected_style: ftui::Style,
        text_muted_style: ftui::Style,
    ) {
        let results_title = format!("Results ({})", hits.len());
        let results_block = Block::new()
            .borders(Borders::ALL)
            .border_type(border_type)
            .title(&results_title)
            .title_alignment(Alignment::Left)
            .style(if self.focus_region == FocusRegion::Results {
                pane_focused_style
            } else {
                pane_style
            });
        let inner = results_block.inner(area);
        results_block.render(area, frame);

        if inner.is_empty() {
            return;
        }

        if hits.is_empty() {
            Paragraph::new("No results yet. Type a query and press Enter.")
                .style(text_muted_style)
                .render(inner, frame);
            return;
        }

        // Density-aware row rendering
        let visible_rows = (inner.height / row_h) as usize;
        let max_rows = visible_rows.min(hits.len());

        for (row, hit) in hits.iter().enumerate().take(max_rows) {
            let location = if let Some(line) = hit.line_number {
                format!("{}:{line}", hit.source_path)
            } else {
                hit.source_path.clone()
            };
            let title = if hit.title.trim().is_empty() {
                "<untitled>"
            } else {
                hit.title.trim()
            };

            let style = if row == selected_idx {
                row_selected_style
            } else if row.is_multiple_of(2) {
                row_style
            } else {
                row_alt_style
            };

            let row_area = Rect::new(inner.x, inner.y + (row as u16) * row_h, inner.width, row_h);

            match self.density_mode {
                DensityMode::Compact => {
                    // Single line: number + title + location
                    let row_text = format!("{:>2}. {} [{}]", row + 1, title, location);
                    Paragraph::new(&*row_text)
                        .style(style)
                        .render(row_area, frame);
                }
                DensityMode::Cozy => {
                    // Two lines: title on first, metadata on second
                    let line1 = format!("{:>2}. {}", row + 1, title);
                    let line2 = format!("    {} | {:.1}", location, hit.score);
                    let text = format!("{line1}\n{line2}");
                    Paragraph::new(&*text).style(style).render(row_area, frame);
                }
                DensityMode::Spacious => {
                    // Three lines: title, snippet preview, metadata
                    let line1 = format!("{:>2}. {}", row + 1, title);
                    let snippet_preview = hit
                        .snippet
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("");
                    let snip = if snippet_preview.len() > inner.width as usize - 4 {
                        &snippet_preview[..inner.width as usize - 7]
                    } else {
                        snippet_preview
                    };
                    let line2 = format!("    {snip}");
                    let line3 = format!("    {} | {} | {:.1}", hit.agent, location, hit.score);
                    let text = format!("{line1}\n{line2}\n{line3}");
                    Paragraph::new(&*text).style(style).render(row_area, frame);
                }
            }
        }

        // Render role gutter markers if a11y mode is on
        if styles.options.a11y {
            let marker = styles.role_markers.assistant;
            if !marker.is_empty() && inner.width > 4 {
                let marker_area = Rect::new(inner.x, inner.y, 3, inner.height);
                Paragraph::new(marker)
                    .style(styles.style(style_system::STYLE_ROLE_GUTTER_ASSISTANT))
                    .render(marker_area, frame);
            }
        }
    }

    /// Render the detail/preview pane.
    #[allow(clippy::too_many_arguments)]
    fn render_detail_pane(
        &self,
        frame: &mut super::ftui_adapter::Frame,
        area: Rect,
        border_type: BorderType,
        styles: &StyleContext,
        pane_style: ftui::Style,
        pane_focused_style: ftui::Style,
        text_muted_style: ftui::Style,
    ) {
        let tab_label = match self.detail_tab {
            DetailTab::Messages => "Detail [Messages]",
            DetailTab::Snippets => "Detail [Snippets]",
            DetailTab::Raw => "Detail [Raw]",
        };
        let detail_block = Block::new()
            .borders(Borders::ALL)
            .border_type(border_type)
            .title(tab_label)
            .title_alignment(Alignment::Left)
            .style(if self.focus_region == FocusRegion::Detail {
                pane_focused_style
            } else {
                pane_style
            });
        let inner = detail_block.inner(area);
        detail_block.render(area, frame);

        if inner.is_empty() {
            return;
        }

        if let Some(hit) = self.selected_hit() {
            let preview = hit
                .snippet
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("<no snippet available>");
            let detail_line = format!(
                "{}\n\nagent={} workspace={}\nscore={:.3}",
                preview, hit.agent, hit.workspace, hit.score
            );
            Paragraph::new(&*detail_line)
                .style(styles.style(style_system::STYLE_TEXT_PRIMARY))
                .render(inner, frame);
        } else {
            Paragraph::new("Select a result to preview context and metadata.")
                .style(text_muted_style)
                .render(inner, frame);
        }
    }

    /// Render the command palette overlay centered on screen.
    fn render_palette_overlay(
        &self,
        frame: &mut super::ftui_adapter::Frame,
        area: Rect,
        styles: &StyleContext,
    ) {
        // Palette dimensions: 60 cols or 80% of width, 16 rows or 60% of height.
        let pal_w = (area.width * 4 / 5).clamp(30, 60);
        let pal_h = (area.height * 3 / 5).clamp(8, 20);
        let pal_x = area.x + (area.width.saturating_sub(pal_w)) / 2;
        let pal_y = area.y + (area.height.saturating_sub(pal_h)) / 2;
        let pal_area = Rect::new(pal_x, pal_y, pal_w, pal_h);

        let palette_bg = styles.style(style_system::STYLE_PANE_BASE);
        let border_style = styles.style(style_system::STYLE_PANE_FOCUSED);
        let text_style = styles.style(style_system::STYLE_TEXT_PRIMARY);
        let muted_style = styles.style(style_system::STYLE_TEXT_MUTED);

        // Clear the area and draw the outer block.
        Block::new().style(palette_bg).render(pal_area, frame);
        let outer = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title("Command Palette (Ctrl+P)")
            .title_alignment(Alignment::Left)
            .style(border_style);
        let inner = outer.inner(pal_area);
        outer.render(pal_area, frame);

        if inner.is_empty() {
            return;
        }

        // Split inner into input (1 row) + separator (1 row) + list (rest).
        let input_area = Rect::new(inner.x, inner.y, inner.width, 1);
        let list_y = inner.y + 2;
        let list_h = inner.height.saturating_sub(2);

        // Render query input.
        let query_display = if self.palette_state.query.is_empty() {
            "Type to filter..."
        } else {
            self.palette_state.query.as_str()
        };
        let query_style = if self.palette_state.query.is_empty() {
            muted_style
        } else {
            text_style
        };
        Paragraph::new(query_display)
            .style(query_style)
            .render(input_area, frame);

        // Render separator line.
        if inner.height > 2 {
            let sep = "\u{2500}".repeat(inner.width as usize);
            Paragraph::new(&*sep)
                .style(muted_style)
                .render(Rect::new(inner.x, inner.y + 1, inner.width, 1), frame);
        }

        // Render filtered action list.
        let visible_count = list_h as usize;
        let selected = self.palette_state.selected;
        // Scroll the list so the selection is always visible.
        let scroll_offset = if selected >= visible_count {
            selected - visible_count + 1
        } else {
            0
        };

        let selected_style = styles.style(style_system::STYLE_RESULT_ROW_SELECTED);

        for (i, item) in self
            .palette_state
            .filtered
            .iter()
            .skip(scroll_offset)
            .take(visible_count)
            .enumerate()
        {
            let row_y = list_y + i as u16;
            if row_y >= pal_area.y + pal_area.height {
                break;
            }
            let row_area = Rect::new(inner.x, row_y, inner.width, 1);
            let abs_idx = scroll_offset + i;
            let is_selected = abs_idx == selected;

            // Format: "  label                    hint" or "➜ label ... hint"
            let prefix = if is_selected { "\u{279c} " } else { "  " };
            let hint_len = item.hint.len();
            let label_max = (inner.width as usize).saturating_sub(prefix.len() + hint_len + 2);
            let label = if item.label.len() > label_max {
                &item.label[..label_max]
            } else {
                &item.label
            };
            let padding = inner
                .width
                .saturating_sub(prefix.len() as u16 + label.len() as u16 + hint_len as u16);
            let line = format!(
                "{prefix}{label}{:>pad$}",
                item.hint,
                pad = padding as usize + hint_len
            );

            let row_style = if is_selected {
                selected_style
            } else {
                text_style
            };
            Paragraph::new(&*line)
                .style(row_style)
                .render(row_area, frame);
        }

        // Show count at bottom if items overflow.
        let total = self.palette_state.filtered.len();
        if total > visible_count && list_h > 0 {
            let count_text = format!(" {total} actions");
            let count_area = Rect::new(inner.x, pal_area.y + pal_area.height - 1, inner.width, 1);
            Paragraph::new(&*count_text)
                .style(muted_style)
                .render(count_area, frame);
        }
    }
}

// =========================================================================
// CassMsg — every event the TUI can process
// =========================================================================

/// Messages that drive the cass TUI state machine.
///
/// Every user action, system event, and async completion maps to exactly
/// one variant.  The `CassApp::update()` function pattern-matches on
/// these to produce pure state transitions + side-effect commands.
#[derive(Debug)]
pub enum CassMsg {
    // -- Keyboard / input -------------------------------------------------
    /// Raw terminal event (key, mouse, resize, paste, tick).
    /// The update() function converts this into more specific messages.
    TerminalEvent(TerminalEventPayload),

    // -- Query & search ---------------------------------------------------
    /// User typed or edited the query string.
    QueryChanged(String),
    /// User cleared the entire query line (Ctrl+U).
    QueryCleared,
    /// User deleted word-backward (Ctrl+W).
    QueryWordDeleted,
    /// User requested immediate search (Enter or debounce expired).
    SearchRequested,
    /// Async search completed with results.
    SearchCompleted {
        hits: Vec<SearchHit>,
        elapsed_ms: u128,
        suggestions: Vec<QuerySuggestion>,
        wildcard_fallback: bool,
    },
    /// Search failed with an error message.
    SearchFailed(String),

    // -- Filters ----------------------------------------------------------
    /// Agent filter added or changed.
    FilterAgentSet(HashSet<String>),
    /// Workspace filter added or changed.
    FilterWorkspaceSet(HashSet<String>),
    /// Time range filter set.
    FilterTimeSet { from: Option<i64>, to: Option<i64> },
    /// Source filter changed.
    FilterSourceSet(SourceFilter),
    /// All filters cleared.
    FiltersClearAll,

    // -- Mode cycling -----------------------------------------------------
    /// Cycle search mode (Lexical -> Semantic -> Hybrid).
    SearchModeCycled,
    /// Cycle match mode (Standard <-> Prefix).
    MatchModeCycled,
    /// Cycle ranking mode through all 6 variants.
    RankingModeCycled,
    /// Cycle context window (S -> M -> L -> XL).
    ContextWindowCycled,
    /// Cycle density mode (Compact -> Cozy -> Spacious).
    DensityModeCycled,
    /// Toggle dark/light theme.
    ThemeToggled,

    // -- Navigation -------------------------------------------------------
    /// Move selection in the results pane.
    SelectionMoved { delta: i32 },
    /// Jump to first or last result.
    SelectionJumped { to_end: bool },
    /// Change active pane.
    ActivePaneChanged { index: usize },
    /// Toggle focus between Results and Detail.
    FocusToggled,
    /// Move focus in a specific direction.
    FocusDirectional { direction: FocusDirection },
    /// Scroll the detail pane.
    DetailScrolled { delta: i32 },
    /// Page-level scroll.
    PageScrolled { delta: i32 },

    // -- Detail view ------------------------------------------------------
    /// Open the detail modal for the currently selected result.
    DetailOpened,
    /// Close the detail modal.
    DetailClosed,
    /// Switch detail tab.
    DetailTabChanged(DetailTab),
    /// Toggle text wrap in detail view.
    DetailWrapToggled,
    /// Enter/exit inline find mode in detail.
    DetailFindToggled,
    /// Update the detail find query.
    DetailFindQueryChanged(String),
    /// Move to next/previous find match.
    DetailFindNavigated { forward: bool },

    // -- Multi-select & bulk actions --------------------------------------
    /// Toggle select on the current item.
    SelectionToggled,
    /// Select or deselect all items in the current pane.
    SelectAllToggled,
    /// Enqueue current item and advance to next.
    ItemEnqueued,
    /// Open bulk actions modal.
    BulkActionsOpened,
    /// Close bulk actions modal.
    BulkActionsClosed,
    /// Execute a bulk action.
    BulkActionExecuted { action_index: usize },

    // -- Actions on results -----------------------------------------------
    /// Copy the current snippet to clipboard.
    CopySnippet,
    /// Copy the current source path to clipboard.
    CopyPath,
    /// Copy the rendered detail content to clipboard.
    CopyContent,
    /// Open the current result in $EDITOR.
    OpenInEditor,
    /// Open content in nano.
    OpenInNano,
    /// Open all enqueued items in $EDITOR.
    OpenAllQueued,
    /// View raw source file.
    ViewRaw,
    /// Peek XL context (toggle).
    PeekToggled,
    /// Refresh results (re-run current query).
    ResultsRefreshed,

    // -- Pane filter (/ in results) ---------------------------------------
    /// Enter pane-local filter mode.
    PaneFilterOpened,
    /// Update pane filter text.
    PaneFilterChanged(String),
    /// Apply or cancel pane filter.
    PaneFilterClosed { apply: bool },

    // -- Input mode transitions -------------------------------------------
    /// Enter a specific input mode (Agent, Workspace, DateFrom, DateTo).
    InputModeEntered(InputMode),
    /// Update the ephemeral input buffer.
    InputBufferChanged(String),
    /// Apply the current input buffer as a filter and return to Query mode.
    InputModeApplied,
    /// Cancel input mode and return to Query mode.
    InputModeCancelled,
    /// Auto-complete the input buffer to the first suggestion.
    InputAutoCompleted,

    // -- History ----------------------------------------------------------
    /// Navigate query history.
    HistoryNavigated { forward: bool },
    /// Cycle through history (Ctrl+R).
    HistoryCycled,

    // -- Command palette --------------------------------------------------
    /// Open the command palette.
    PaletteOpened,
    /// Close the command palette.
    PaletteClosed,
    /// Update the palette search query.
    PaletteQueryChanged(String),
    /// Move palette selection.
    PaletteSelectionMoved { delta: i32 },
    /// Execute the selected palette action.
    PaletteActionExecuted,

    // -- Help overlay -----------------------------------------------------
    /// Toggle the help overlay.
    HelpToggled,
    /// Scroll the help overlay.
    HelpScrolled { delta: i32 },
    /// Toggle help strip pinned state.
    HelpPinToggled,

    // -- Export modal ------------------------------------------------------
    /// Open the export modal.
    ExportModalOpened,
    /// Close the export modal.
    ExportModalClosed,
    /// Update an export modal field.
    ExportFieldChanged {
        field: crate::ui::components::export_modal::ExportField,
        value: String,
    },
    /// Toggle an export modal checkbox.
    ExportFieldToggled(crate::ui::components::export_modal::ExportField),
    /// Move focus within the export modal.
    ExportFocusMoved { forward: bool },
    /// Execute the export.
    ExportExecuted,
    /// Export progress update from background task.
    ExportProgressUpdated(ExportProgress),
    /// Export completed successfully.
    ExportCompleted {
        output_path: PathBuf,
        file_size: usize,
        encrypted: bool,
    },
    /// Export failed.
    ExportFailed(String),

    // -- Consent dialog (semantic model download) -------------------------
    /// Open the consent dialog.
    ConsentDialogOpened,
    /// Close the consent dialog.
    ConsentDialogClosed,
    /// User accepted model download.
    ModelDownloadAccepted,
    /// Model download progress update.
    ModelDownloadProgress { bytes_downloaded: u64, total: u64 },
    /// Model download completed.
    ModelDownloadCompleted,
    /// Model download failed.
    ModelDownloadFailed(String),
    /// User cancelled the active download.
    ModelDownloadCancelled,
    /// User accepted hash mode fallback (no ML model).
    HashModeAccepted,

    // -- Source filter menu ------------------------------------------------
    /// Toggle the source filter popup menu.
    SourceFilterMenuToggled,
    /// Select a source filter from the menu.
    SourceFilterSelected(SourceFilter),

    // -- Update assistant -------------------------------------------------
    /// Update check completed.
    UpdateCheckCompleted(UpdateInfo),
    /// User chose to upgrade.
    UpdateUpgradeRequested,
    /// User chose to skip this version.
    UpdateSkipped,
    /// User dismissed the update banner.
    UpdateDismissed,

    // -- Did-you-mean suggestions -----------------------------------------
    /// Apply a did-you-mean suggestion by index (1, 2, or 3).
    SuggestionApplied(u8),

    // -- Display ----------------------------------------------------------
    /// Toggle fancy/plain borders.
    BordersToggled,
    /// Grow the pane item count.
    PaneGrew,
    /// Shrink the pane item count.
    PaneShrunk,

    // -- Saved views ------------------------------------------------------
    /// Save current view to a slot (1-9).
    ViewSaved(u8),
    /// Load a saved view from a slot (1-9).
    ViewLoaded(u8),

    // -- Index ------------------------------------------------------------
    /// User requested index refresh.
    IndexRefreshRequested,
    /// Index progress update.
    IndexProgress {
        processed: usize,
        total: usize,
        new_items: usize,
    },
    /// Index refresh completed.
    IndexRefreshCompleted,

    // -- State persistence ------------------------------------------------
    /// Load persisted state from disk.
    StateLoadRequested,
    /// Persisted state loaded.
    StateLoaded(Box<PersistedState>),
    /// Save current state to disk.
    StateSaveRequested,
    /// Reset all persisted state to defaults.
    StateResetRequested,

    // -- Toast notifications ----------------------------------------------
    /// Show a toast notification.
    ToastShown {
        message: String,
        toast_type: crate::ui::components::toast::ToastType,
    },
    /// Dismiss expired toasts (called on tick).
    ToastTick,

    // -- Window & terminal ------------------------------------------------
    /// Terminal resized.
    Resized { width: u16, height: u16 },
    /// Periodic tick for animations and debounce.
    Tick,
    /// Mouse event with coordinates.
    MouseEvent {
        kind: MouseEventKind,
        x: u16,
        y: u16,
    },

    // -- Lifecycle ---------------------------------------------------------
    /// Application quit requested.
    QuitRequested,
    /// Force quit (Ctrl+C).
    ForceQuit,
}

/// Direction for focus movement.
#[derive(Debug, Clone, Copy)]
pub enum FocusDirection {
    Up,
    Down,
    Left,
    Right,
}

/// Mouse event kinds (simplified from crossterm/ftui).
#[derive(Debug, Clone, Copy)]
pub enum MouseEventKind {
    LeftClick,
    ScrollUp,
    ScrollDown,
}

/// Wrapper for terminal events that will be converted to specific messages.
#[derive(Debug)]
pub struct TerminalEventPayload {
    /// Opaque event data (will be ftui::Event in the runtime).
    _private: (),
}

// =========================================================================
// Persisted state (for save/load)
// =========================================================================

/// Subset of CassApp state that persists across sessions.
#[derive(Debug, Clone)]
pub struct PersistedState {
    pub search_mode: SearchMode,
    pub match_mode: MatchMode,
    pub ranking_mode: RankingMode,
    pub context_window: ContextWindow,
    pub theme_dark: bool,
    pub density_mode: DensityMode,
    pub per_pane_limit: usize,
    pub query_history: VecDeque<String>,
    pub saved_views: Vec<SavedView>,
    pub fancy_borders: bool,
    pub help_pinned: bool,
    pub has_seen_help: bool,
}

// =========================================================================
// Service Traits
// =========================================================================

/// Async search execution.
///
/// Abstracts the Tantivy + vector index search pipeline so the Model
/// does not hold direct references to index internals.
pub trait SearchService: Send + Sync {
    /// Execute a search query with the given parameters.
    fn execute(&self, params: &SearchParams) -> Result<SearchResult, String>;
}

/// Parameters for a search query.
#[derive(Debug, Clone)]
pub struct SearchParams {
    pub query: String,
    pub filters: SearchFilters,
    pub mode: SearchMode,
    pub match_mode: MatchMode,
    pub ranking: RankingMode,
    pub context_window: ContextWindow,
    pub limit: usize,
}

/// Result returned by [`SearchService::execute`].
#[derive(Debug)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub elapsed_ms: u128,
    pub suggestions: Vec<QuerySuggestion>,
    pub wildcard_fallback: bool,
}

/// Background indexing with progress reporting.
pub trait IndexService: Send + Sync {
    /// Trigger an incremental index refresh.
    fn refresh(&self) -> Result<(), String>;

    /// Check if indexing is currently in progress.
    fn is_running(&self) -> bool;
}

/// Open files in external editors.
pub trait EditorService: Send + Sync {
    /// Open a file at the given path, optionally at a specific line.
    fn open(&self, path: &str, line: Option<usize>) -> Result<(), String>;

    /// Open multiple files.
    fn open_many(&self, paths: &[String]) -> Result<(), String>;
}

/// HTML/markdown export.
pub trait ExportService: Send + Sync {
    /// Export a conversation to HTML.
    fn export_html(
        &self,
        source_path: &str,
        output_dir: &std::path::Path,
        encrypt: bool,
        password: Option<&str>,
        show_timestamps: bool,
    ) -> Result<ExportResult, String>;
}

/// Result returned by [`ExportService::export_html`].
#[derive(Debug)]
pub struct ExportResult {
    pub output_path: PathBuf,
    pub file_size: usize,
    pub encrypted: bool,
    pub message_count: usize,
}

/// Save/load TUI state to disk.
pub trait PersistenceService: Send + Sync {
    /// Load persisted state.
    fn load(&self) -> Result<Option<PersistedState>, String>;

    /// Save current state.
    fn save(&self, state: &PersistedState) -> Result<(), String>;

    /// Delete persisted state (reset).
    fn reset(&self) -> Result<(), String>;
}

const _SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(60);
const _QUERY_HISTORY_CAP: usize = 50;

// =========================================================================
// From<Event> — convert ftui terminal events into CassMsg
// =========================================================================

impl From<super::ftui_adapter::Event> for CassMsg {
    fn from(event: super::ftui_adapter::Event) -> Self {
        use super::ftui_adapter::{Event, KeyCode, Modifiers};

        match event {
            Event::Key(key) => {
                let ctrl = key.modifiers.contains(Modifiers::CTRL);
                let alt = key.modifiers.contains(Modifiers::ALT);
                let shift = key.modifiers.contains(Modifiers::SHIFT);

                match key.code {
                    // -- Force quit -----------------------------------------------
                    KeyCode::Char('c') if ctrl => CassMsg::ForceQuit,

                    // -- Escape / quit --------------------------------------------
                    KeyCode::Escape => CassMsg::QuitRequested,
                    KeyCode::F(10) => CassMsg::QuitRequested,

                    // -- Help -----------------------------------------------------
                    KeyCode::F(1) => CassMsg::HelpToggled,
                    KeyCode::Char('?') => CassMsg::HelpToggled,

                    // -- Theme ----------------------------------------------------
                    KeyCode::F(2) => CassMsg::ThemeToggled,
                    KeyCode::Char('t') if ctrl => CassMsg::ThemeToggled,

                    // -- Filters --------------------------------------------------
                    KeyCode::F(3) if shift => CassMsg::FilterAgentSet(HashSet::new()),
                    KeyCode::F(3) => CassMsg::InputModeEntered(InputMode::Agent),
                    KeyCode::F(4) if shift => CassMsg::FiltersClearAll,
                    KeyCode::F(4) => CassMsg::InputModeEntered(InputMode::Workspace),
                    KeyCode::F(5) if shift => CassMsg::Tick, // cycle time presets
                    KeyCode::F(5) => CassMsg::InputModeEntered(InputMode::CreatedFrom),
                    KeyCode::F(6) => CassMsg::InputModeEntered(InputMode::CreatedTo),

                    // -- Context window -------------------------------------------
                    KeyCode::F(7) => CassMsg::ContextWindowCycled,

                    // -- Editor ---------------------------------------------------
                    KeyCode::F(8) => CassMsg::OpenInEditor,

                    // -- Match mode -----------------------------------------------
                    KeyCode::F(9) => CassMsg::MatchModeCycled,

                    // -- Source filter ---------------------------------------------
                    KeyCode::F(11) if shift => CassMsg::SourceFilterMenuToggled,
                    KeyCode::F(11) => CassMsg::Tick, // cycle source filter

                    // -- Ranking --------------------------------------------------
                    KeyCode::F(12) => CassMsg::RankingModeCycled,

                    // -- Search mode (Alt+S) --------------------------------------
                    KeyCode::Char('s') if alt => CassMsg::SearchModeCycled,

                    // -- Command palette ------------------------------------------
                    KeyCode::Char('p') if ctrl => CassMsg::PaletteOpened,
                    KeyCode::Char('p') if alt => CassMsg::PaletteOpened,

                    // -- History ---------------------------------------------------
                    KeyCode::Char('r') if ctrl && shift => CassMsg::IndexRefreshRequested,
                    KeyCode::Char('r') if ctrl => CassMsg::HistoryCycled,
                    KeyCode::Char('n') if ctrl => CassMsg::HistoryNavigated { forward: true },
                    KeyCode::Char('p') if ctrl => CassMsg::HistoryNavigated { forward: false },

                    // -- Saved views (Ctrl+1..9 save, Shift+1..9 load) -----------
                    KeyCode::Char(c @ '1'..='9') if ctrl => CassMsg::ViewSaved(c as u8 - b'0'),
                    KeyCode::Char(c @ '1'..='9') if shift => CassMsg::ViewLoaded(c as u8 - b'0'),

                    // -- Clear / reset --------------------------------------------
                    KeyCode::Delete if ctrl && shift => CassMsg::StateResetRequested,
                    KeyCode::Delete if ctrl => CassMsg::FiltersClearAll,

                    // -- Borders --------------------------------------------------
                    KeyCode::Char('b') if ctrl => CassMsg::BordersToggled,

                    // -- Line editing ---------------------------------------------
                    KeyCode::Char('u') if ctrl => CassMsg::QueryCleared,
                    KeyCode::Char('w') if ctrl => CassMsg::QueryWordDeleted,

                    // -- Density --------------------------------------------------
                    KeyCode::Char('d') if ctrl => CassMsg::DensityModeCycled,

                    // -- Multi-select ---------------------------------------------
                    KeyCode::Char('x') if ctrl => CassMsg::SelectionToggled,
                    KeyCode::Char('a') if ctrl => CassMsg::SelectAllToggled,
                    KeyCode::Enter if ctrl => CassMsg::ItemEnqueued,
                    KeyCode::Char('o') if ctrl => CassMsg::OpenAllQueued,

                    // -- Quick export ---------------------------------------------
                    KeyCode::Char('e') if ctrl => CassMsg::ExportModalOpened,

                    // -- Peek XL --------------------------------------------------
                    KeyCode::Char(' ') if ctrl => CassMsg::PeekToggled,

                    // -- Navigation -----------------------------------------------
                    KeyCode::Tab if shift => CassMsg::FocusDirectional {
                        direction: FocusDirection::Left,
                    },
                    KeyCode::Tab => CassMsg::FocusToggled,
                    KeyCode::Up | KeyCode::Char('k') if alt => CassMsg::FocusDirectional {
                        direction: FocusDirection::Up,
                    },
                    KeyCode::Down | KeyCode::Char('j') if alt => CassMsg::FocusDirectional {
                        direction: FocusDirection::Down,
                    },
                    KeyCode::Left | KeyCode::Char('h') if alt => CassMsg::FocusDirectional {
                        direction: FocusDirection::Left,
                    },
                    KeyCode::Right | KeyCode::Char('l') if alt => CassMsg::FocusDirectional {
                        direction: FocusDirection::Right,
                    },
                    KeyCode::Up => CassMsg::SelectionMoved { delta: -1 },
                    KeyCode::Down => CassMsg::SelectionMoved { delta: 1 },
                    KeyCode::Home => CassMsg::SelectionJumped { to_end: false },
                    KeyCode::End => CassMsg::SelectionJumped { to_end: true },
                    KeyCode::PageUp => CassMsg::PageScrolled { delta: -1 },
                    KeyCode::PageDown => CassMsg::PageScrolled { delta: 1 },
                    KeyCode::Enter => CassMsg::DetailOpened,

                    // -- Pane sizing (Shift+=, Alt+-) -----------------------------
                    KeyCode::Char('-') if alt => CassMsg::PaneShrunk,
                    KeyCode::Char('=') if shift => CassMsg::PaneGrew,
                    KeyCode::Char('+') => CassMsg::PaneGrew,

                    // -- Alt+digit pane switch ------------------------------------
                    KeyCode::Char(c @ '1'..='9') if alt => CassMsg::ActivePaneChanged {
                        index: (c as u8 - b'1') as usize,
                    },

                    // -- Slash (context-sensitive) ---------------------------------
                    KeyCode::Char('/') => CassMsg::PaneFilterOpened,

                    // -- Result actions (bare keys, only in results focus) ---------
                    KeyCode::Char('y') => CassMsg::CopySnippet,
                    KeyCode::Char('o') => CassMsg::OpenInEditor,
                    KeyCode::Char('v') => CassMsg::ViewRaw,
                    KeyCode::Char('r') => CassMsg::ResultsRefreshed,
                    KeyCode::Char('A') => CassMsg::BulkActionsOpened,
                    KeyCode::Char(' ') => CassMsg::PeekToggled,

                    // -- Default: treat as query input ----------------------------
                    KeyCode::Char(c) => CassMsg::QueryChanged(c.to_string()),
                    KeyCode::Backspace => CassMsg::QueryChanged(String::new()),

                    _ => CassMsg::Tick, // Unhandled keys become no-op ticks
                }
            }

            Event::Mouse(mouse) => {
                use ftui::core::event::MouseEventKind as Mek;
                match mouse.kind {
                    Mek::Down(_) => CassMsg::MouseEvent {
                        kind: MouseEventKind::LeftClick,
                        x: mouse.x,
                        y: mouse.y,
                    },
                    Mek::ScrollUp => CassMsg::MouseEvent {
                        kind: MouseEventKind::ScrollUp,
                        x: mouse.x,
                        y: mouse.y,
                    },
                    Mek::ScrollDown => CassMsg::MouseEvent {
                        kind: MouseEventKind::ScrollDown,
                        x: mouse.x,
                        y: mouse.y,
                    },
                    _ => CassMsg::Tick,
                }
            }

            Event::Resize { width, height } => CassMsg::Resized { width, height },
            Event::Tick => CassMsg::Tick,
            _ => CassMsg::Tick,
        }
    }
}

// =========================================================================
// ftui::Model implementation
// =========================================================================

impl super::ftui_adapter::Model for CassApp {
    type Message = CassMsg;

    fn init(&mut self) -> ftui::Cmd<CassMsg> {
        // Request state load on startup.
        ftui::Cmd::msg(CassMsg::StateLoadRequested)
    }

    fn update(&mut self, msg: CassMsg) -> ftui::Cmd<CassMsg> {
        // Consent dialog intercepts D/H keys and blocks other query input
        if self.show_consent_dialog {
            if let CassMsg::QueryChanged(ref text) = msg {
                if text.eq_ignore_ascii_case("d") {
                    return self.update(CassMsg::ModelDownloadAccepted);
                }
                if text.eq_ignore_ascii_case("h") {
                    return self.update(CassMsg::HashModeAccepted);
                }
                // Ignore other query input while consent dialog is open
                return ftui::Cmd::none();
            }
        }

        match msg {
            // -- Terminal event passthrough (unused, events come as CassMsg) ---
            CassMsg::TerminalEvent(_) => ftui::Cmd::none(),

            // -- Query & search -----------------------------------------------
            CassMsg::QueryChanged(text) => {
                if text.is_empty() {
                    // Backspace: remove last char from query
                    self.query.pop();
                } else {
                    self.query.push_str(&text);
                }
                self.dirty_since = Some(Instant::now());
                // TODO(2noh9.3.2): implement debounced search via Cmd::tick
                ftui::Cmd::none()
            }
            CassMsg::QueryCleared => {
                self.query.clear();
                self.dirty_since = Some(Instant::now());
                ftui::Cmd::none()
            }
            CassMsg::QueryWordDeleted => {
                // Delete last word (Ctrl+W): trim trailing whitespace, then trim to
                // the last whitespace boundary.
                let trimmed = self.query.trim_end();
                if let Some(pos) = trimmed.rfind(char::is_whitespace) {
                    self.query.truncate(pos + 1);
                } else {
                    self.query.clear();
                }
                self.dirty_since = Some(Instant::now());
                ftui::Cmd::none()
            }
            CassMsg::SearchRequested => {
                // TODO: dispatch async search via Cmd::task
                ftui::Cmd::none()
            }
            CassMsg::SearchCompleted {
                hits,
                elapsed_ms,
                suggestions,
                wildcard_fallback,
            } => {
                self.results = hits;
                self.last_search_ms = Some(elapsed_ms);
                self.suggestions = suggestions;
                self.wildcard_fallback = wildcard_fallback;
                // TODO: rebuild panes from results
                ftui::Cmd::none()
            }
            CassMsg::SearchFailed(err) => {
                self.status = format!("Search error: {err}");
                ftui::Cmd::none()
            }

            // -- Filters ------------------------------------------------------
            CassMsg::FilterAgentSet(agents) => {
                self.filters.agents = agents;
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }
            CassMsg::FilterWorkspaceSet(workspaces) => {
                self.filters.workspaces = workspaces;
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }
            CassMsg::FilterTimeSet { from, to } => {
                self.filters.created_from = from;
                self.filters.created_to = to;
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }
            CassMsg::FilterSourceSet(source) => {
                self.filters.source_filter = source;
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }
            CassMsg::FiltersClearAll => {
                self.filters = SearchFilters::default();
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }

            // -- Mode cycling -------------------------------------------------
            CassMsg::SearchModeCycled => {
                self.search_mode = match self.search_mode {
                    SearchMode::Lexical => SearchMode::Semantic,
                    SearchMode::Semantic => SearchMode::Hybrid,
                    SearchMode::Hybrid => SearchMode::Lexical,
                };
                self.dirty_since = Some(Instant::now());
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }
            CassMsg::MatchModeCycled => {
                self.match_mode = match self.match_mode {
                    MatchMode::Standard => MatchMode::Prefix,
                    MatchMode::Prefix => MatchMode::Standard,
                };
                self.dirty_since = Some(Instant::now());
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }
            CassMsg::RankingModeCycled => {
                self.ranking_mode = match self.ranking_mode {
                    RankingMode::RecentHeavy => RankingMode::Balanced,
                    RankingMode::Balanced => RankingMode::RelevanceHeavy,
                    RankingMode::RelevanceHeavy => RankingMode::MatchQualityHeavy,
                    RankingMode::MatchQualityHeavy => RankingMode::DateNewest,
                    RankingMode::DateNewest => RankingMode::DateOldest,
                    RankingMode::DateOldest => RankingMode::RecentHeavy,
                };
                self.dirty_since = Some(Instant::now());
                ftui::Cmd::none()
            }
            CassMsg::ContextWindowCycled => {
                self.context_window = match self.context_window {
                    ContextWindow::Small => ContextWindow::Medium,
                    ContextWindow::Medium => ContextWindow::Large,
                    ContextWindow::Large => ContextWindow::XLarge,
                    ContextWindow::XLarge => ContextWindow::Small,
                };
                self.dirty_since = Some(Instant::now());
                ftui::Cmd::none()
            }
            CassMsg::DensityModeCycled => {
                self.density_mode = match self.density_mode {
                    DensityMode::Compact => DensityMode::Cozy,
                    DensityMode::Cozy => DensityMode::Spacious,
                    DensityMode::Spacious => DensityMode::Compact,
                };
                self.dirty_since = Some(Instant::now());
                ftui::Cmd::none()
            }
            CassMsg::ThemeToggled => {
                self.theme_dark = !self.theme_dark;
                self.theme_preset = if self.theme_dark {
                    UiThemePreset::Dark
                } else {
                    UiThemePreset::Light
                };
                self.style_options.dark_mode = self.theme_dark;
                self.style_options.preset = self.theme_preset;
                self.dirty_since = Some(Instant::now());
                ftui::Cmd::none()
            }

            // -- Navigation ---------------------------------------------------
            CassMsg::SelectionMoved { delta } => {
                if let Some(pane) = self.panes.get_mut(self.active_pane) {
                    let new_sel = pane.selected as i32 + delta;
                    pane.selected =
                        new_sel.clamp(0, pane.hits.len().saturating_sub(1) as i32) as usize;
                }
                ftui::Cmd::none()
            }
            CassMsg::SelectionJumped { to_end } => {
                if let Some(pane) = self.panes.get_mut(self.active_pane) {
                    pane.selected = if to_end {
                        pane.hits.len().saturating_sub(1)
                    } else {
                        0
                    };
                }
                ftui::Cmd::none()
            }
            CassMsg::ActivePaneChanged { index } => {
                if index < self.panes.len() {
                    self.active_pane = index;
                }
                ftui::Cmd::none()
            }
            CassMsg::FocusToggled => {
                self.focus_region = match self.focus_region {
                    FocusRegion::Results => FocusRegion::Detail,
                    FocusRegion::Detail => FocusRegion::Results,
                };
                self.focus_flash_until =
                    Some(Instant::now() + std::time::Duration::from_millis(220));
                ftui::Cmd::none()
            }
            CassMsg::FocusDirectional { direction } => {
                self.focus_region = match direction {
                    FocusDirection::Left => FocusRegion::Results,
                    FocusDirection::Right => FocusRegion::Detail,
                    _ => self.focus_region,
                };
                ftui::Cmd::none()
            }
            CassMsg::DetailScrolled { delta } => {
                let new_scroll = self.detail_scroll as i32 + delta;
                self.detail_scroll = new_scroll.max(0) as u16;
                ftui::Cmd::none()
            }
            CassMsg::PageScrolled { delta } => {
                if self.focus_region == FocusRegion::Detail {
                    let new_scroll = self.detail_scroll as i32 + (delta * 20);
                    self.detail_scroll = new_scroll.max(0) as u16;
                } else if let Some(pane) = self.panes.get_mut(self.active_pane) {
                    let page_size = self.per_pane_limit as i32;
                    let new_sel = pane.selected as i32 + (delta * page_size);
                    pane.selected =
                        new_sel.clamp(0, pane.hits.len().saturating_sub(1) as i32) as usize;
                }
                ftui::Cmd::none()
            }

            // -- Detail view --------------------------------------------------
            CassMsg::DetailOpened => {
                self.show_detail_modal = true;
                self.detail_scroll = 0;
                self.modal_scroll = 0;
                ftui::Cmd::none()
            }
            CassMsg::DetailClosed => {
                self.show_detail_modal = false;
                self.focus_region = FocusRegion::Results;
                ftui::Cmd::none()
            }
            CassMsg::DetailTabChanged(tab) => {
                self.detail_tab = tab;
                self.detail_scroll = 0;
                ftui::Cmd::none()
            }
            CassMsg::DetailWrapToggled => {
                // TODO: track wrap state
                ftui::Cmd::none()
            }
            CassMsg::DetailFindToggled => {
                if self.detail_find.is_some() {
                    self.detail_find = None;
                } else {
                    self.detail_find = Some(DetailFindState::default());
                }
                ftui::Cmd::none()
            }
            CassMsg::DetailFindQueryChanged(q) => {
                if let Some(ref mut find) = self.detail_find {
                    find.query = q;
                    // TODO: compute matches
                }
                ftui::Cmd::none()
            }
            CassMsg::DetailFindNavigated { forward } => {
                if let Some(ref mut find) = self.detail_find
                    && !find.matches.is_empty()
                {
                    if forward {
                        find.current = (find.current + 1) % find.matches.len();
                    } else {
                        find.current = find
                            .current
                            .checked_sub(1)
                            .unwrap_or(find.matches.len() - 1);
                    }
                }
                ftui::Cmd::none()
            }

            // -- Multi-select & bulk ------------------------------------------
            CassMsg::SelectionToggled => {
                let key = (
                    self.active_pane,
                    self.panes.get(self.active_pane).map_or(0, |p| p.selected),
                );
                if !self.selected.remove(&key) {
                    self.selected.insert(key);
                }
                ftui::Cmd::none()
            }
            CassMsg::SelectAllToggled => {
                if self.selected.is_empty() {
                    // Select all in current pane
                    if let Some(pane) = self.panes.get(self.active_pane) {
                        for i in 0..pane.hits.len() {
                            self.selected.insert((self.active_pane, i));
                        }
                    }
                } else {
                    self.selected.clear();
                }
                ftui::Cmd::none()
            }
            CassMsg::ItemEnqueued => {
                let key = (
                    self.active_pane,
                    self.panes.get(self.active_pane).map_or(0, |p| p.selected),
                );
                self.selected.insert(key);
                // Advance selection
                if let Some(pane) = self.panes.get_mut(self.active_pane)
                    && pane.selected + 1 < pane.hits.len()
                {
                    pane.selected += 1;
                }
                ftui::Cmd::none()
            }
            CassMsg::BulkActionsOpened => {
                self.show_bulk_modal = true;
                ftui::Cmd::none()
            }
            CassMsg::BulkActionsClosed => {
                self.show_bulk_modal = false;
                ftui::Cmd::none()
            }
            CassMsg::BulkActionExecuted { action_index: _ } => {
                // TODO: dispatch specific bulk action
                self.show_bulk_modal = false;
                ftui::Cmd::none()
            }

            // -- Actions on results -------------------------------------------
            CassMsg::CopySnippet | CassMsg::CopyPath | CassMsg::CopyContent => {
                // TODO: clipboard via Cmd::task
                ftui::Cmd::none()
            }
            CassMsg::OpenInEditor | CassMsg::OpenInNano | CassMsg::OpenAllQueued => {
                // TODO: spawn editor via Cmd::task
                ftui::Cmd::none()
            }
            CassMsg::ViewRaw => {
                // TODO: view raw via Cmd::task
                ftui::Cmd::none()
            }
            CassMsg::PeekToggled => {
                if self.peek_window_saved.is_some() {
                    self.context_window = self
                        .peek_window_saved
                        .take()
                        .unwrap_or(ContextWindow::Medium);
                } else {
                    self.peek_window_saved = Some(self.context_window);
                    self.context_window = ContextWindow::XLarge;
                }
                self.peek_badge_until =
                    Some(Instant::now() + std::time::Duration::from_millis(1500));
                ftui::Cmd::none()
            }
            CassMsg::ResultsRefreshed => ftui::Cmd::msg(CassMsg::SearchRequested),

            // -- Pane filter --------------------------------------------------
            CassMsg::PaneFilterOpened => {
                self.pane_filter = Some(String::new());
                self.input_mode = InputMode::PaneFilter;
                ftui::Cmd::none()
            }
            CassMsg::PaneFilterChanged(text) => {
                self.pane_filter = Some(text);
                ftui::Cmd::none()
            }
            CassMsg::PaneFilterClosed { apply } => {
                if !apply {
                    self.pane_filter = None;
                }
                self.input_mode = InputMode::Query;
                ftui::Cmd::none()
            }

            // -- Input mode transitions ---------------------------------------
            CassMsg::InputModeEntered(mode) => {
                self.input_mode = mode;
                self.input_buffer.clear();
                ftui::Cmd::none()
            }
            CassMsg::InputBufferChanged(text) => {
                self.input_buffer = text;
                ftui::Cmd::none()
            }
            CassMsg::InputModeApplied => {
                // TODO: apply buffer as filter based on input_mode
                self.input_mode = InputMode::Query;
                self.input_buffer.clear();
                ftui::Cmd::none()
            }
            CassMsg::InputModeCancelled => {
                self.input_mode = InputMode::Query;
                self.input_buffer.clear();
                ftui::Cmd::none()
            }
            CassMsg::InputAutoCompleted => {
                // TODO: auto-complete from suggestions
                ftui::Cmd::none()
            }

            // -- History ------------------------------------------------------
            CassMsg::HistoryNavigated { forward } => {
                let len = self.query_history.len();
                if len == 0 {
                    return ftui::Cmd::none();
                }
                let cursor = self.history_cursor.unwrap_or(0);
                self.history_cursor = Some(if forward {
                    (cursor + 1).min(len.saturating_sub(1))
                } else {
                    cursor.saturating_sub(1)
                });
                if let Some(idx) = self.history_cursor
                    && let Some(q) = self.query_history.get(idx)
                {
                    self.query = q.clone();
                }
                ftui::Cmd::none()
            }
            CassMsg::HistoryCycled => ftui::Cmd::msg(CassMsg::HistoryNavigated { forward: true }),

            // -- Command palette ----------------------------------------------
            CassMsg::PaletteOpened => {
                self.palette_state.open = true;
                self.palette_state.query.clear();
                self.palette_state.selected = 0;
                self.palette_state.refilter();
                ftui::Cmd::none()
            }
            CassMsg::PaletteClosed => {
                self.palette_state.open = false;
                ftui::Cmd::none()
            }
            CassMsg::PaletteQueryChanged(q) => {
                self.palette_state.query = q;
                self.palette_state.refilter();
                self.palette_state.selected = 0;
                ftui::Cmd::none()
            }
            CassMsg::PaletteSelectionMoved { delta } => {
                let len = self.palette_state.filtered.len();
                if len > 0 {
                    let new_sel = self.palette_state.selected as i32 + delta;
                    self.palette_state.selected = new_sel.rem_euclid(len as i32) as usize;
                }
                ftui::Cmd::none()
            }
            CassMsg::PaletteActionExecuted => {
                let action = self
                    .palette_state
                    .filtered
                    .get(self.palette_state.selected)
                    .map(|item| item.action.clone());
                self.palette_state.open = false;
                match action {
                    Some(PaletteAction::ToggleTheme) => ftui::Cmd::msg(CassMsg::ThemeToggled),
                    Some(PaletteAction::ToggleDensity) => {
                        ftui::Cmd::msg(CassMsg::DensityModeCycled)
                    }
                    Some(PaletteAction::ToggleHelpStrip) => ftui::Cmd::msg(CassMsg::HelpPinToggled),
                    Some(PaletteAction::OpenUpdateBanner) => {
                        // TODO: show update assistant UI
                        ftui::Cmd::none()
                    }
                    Some(PaletteAction::FilterAgent) => {
                        ftui::Cmd::msg(CassMsg::InputModeEntered(InputMode::Agent))
                    }
                    Some(PaletteAction::FilterWorkspace) => {
                        ftui::Cmd::msg(CassMsg::InputModeEntered(InputMode::Workspace))
                    }
                    Some(PaletteAction::FilterToday) => {
                        let now = chrono::Utc::now().timestamp();
                        let start_of_day = now - (now % 86400);
                        ftui::Cmd::msg(CassMsg::FilterTimeSet {
                            from: Some(start_of_day),
                            to: None,
                        })
                    }
                    Some(PaletteAction::FilterWeek) => {
                        let now = chrono::Utc::now().timestamp();
                        let week_ago = now - (7 * 86400);
                        ftui::Cmd::msg(CassMsg::FilterTimeSet {
                            from: Some(week_ago),
                            to: None,
                        })
                    }
                    Some(PaletteAction::FilterCustomDate) => {
                        ftui::Cmd::msg(CassMsg::InputModeEntered(InputMode::CreatedFrom))
                    }
                    Some(PaletteAction::OpenSavedViews) => {
                        // TODO: show saved views picker
                        ftui::Cmd::none()
                    }
                    Some(PaletteAction::SaveViewSlot(slot)) => {
                        ftui::Cmd::msg(CassMsg::ViewSaved(slot))
                    }
                    Some(PaletteAction::LoadViewSlot(slot)) => {
                        ftui::Cmd::msg(CassMsg::ViewLoaded(slot))
                    }
                    Some(PaletteAction::OpenBulkActions) => {
                        ftui::Cmd::msg(CassMsg::BulkActionsOpened)
                    }
                    Some(PaletteAction::ReloadIndex) => {
                        ftui::Cmd::msg(CassMsg::IndexRefreshRequested)
                    }
                    None => ftui::Cmd::none(),
                }
            }

            // -- Help overlay -------------------------------------------------
            CassMsg::HelpToggled => {
                self.show_help = !self.show_help;
                self.help_scroll = 0;
                ftui::Cmd::none()
            }
            CassMsg::HelpScrolled { delta } => {
                let new_scroll = self.help_scroll as i32 + delta;
                self.help_scroll = new_scroll.max(0) as u16;
                ftui::Cmd::none()
            }
            CassMsg::HelpPinToggled => {
                self.help_pinned = !self.help_pinned;
                ftui::Cmd::none()
            }

            // -- Export modal -------------------------------------------------
            CassMsg::ExportModalOpened => {
                self.show_export_modal = true;
                ftui::Cmd::none()
            }
            CassMsg::ExportModalClosed => {
                self.show_export_modal = false;
                self.export_modal_state = None;
                ftui::Cmd::none()
            }
            CassMsg::ExportFieldChanged { .. }
            | CassMsg::ExportFieldToggled(_)
            | CassMsg::ExportFocusMoved { .. } => {
                // TODO: update export modal state
                ftui::Cmd::none()
            }
            CassMsg::ExportExecuted => {
                // TODO: dispatch export via Cmd::task
                ftui::Cmd::none()
            }
            CassMsg::ExportProgressUpdated(progress) => {
                if let Some(ref mut state) = self.export_modal_state {
                    state.progress = progress;
                }
                ftui::Cmd::none()
            }
            CassMsg::ExportCompleted {
                output_path,
                file_size: _,
                encrypted: _,
            } => {
                self.show_export_modal = false;
                self.export_modal_state = None;
                self.status = format!("Exported to {}", output_path.display());
                ftui::Cmd::none()
            }
            CassMsg::ExportFailed(err) => {
                self.status = format!("Export failed: {err}");
                ftui::Cmd::none()
            }

            // -- Consent dialog -----------------------------------------------
            CassMsg::ConsentDialogOpened => {
                self.show_consent_dialog = true;
                ftui::Cmd::none()
            }
            CassMsg::ConsentDialogClosed => {
                self.show_consent_dialog = false;
                ftui::Cmd::none()
            }
            CassMsg::ModelDownloadAccepted
            | CassMsg::ModelDownloadProgress { .. }
            | CassMsg::ModelDownloadCompleted
            | CassMsg::ModelDownloadFailed(_)
            | CassMsg::ModelDownloadCancelled => {
                // TODO: model download lifecycle
                ftui::Cmd::none()
            }
            CassMsg::HashModeAccepted => {
                // User chose hash embedder fallback instead of downloading ML model.
                self.show_consent_dialog = false;
                ftui::Cmd::none()
            }

            // -- Source filter menu -------------------------------------------
            CassMsg::SourceFilterMenuToggled => {
                self.source_filter_menu_open = !self.source_filter_menu_open;
                ftui::Cmd::none()
            }
            CassMsg::SourceFilterSelected(filter) => {
                self.source_filter_menu_open = false;
                ftui::Cmd::msg(CassMsg::FilterSourceSet(filter))
            }

            // -- Update assistant ---------------------------------------------
            CassMsg::UpdateCheckCompleted(_info) => {
                // TODO: show update banner
                ftui::Cmd::none()
            }
            CassMsg::UpdateUpgradeRequested | CassMsg::UpdateSkipped | CassMsg::UpdateDismissed => {
                // TODO: handle update actions
                ftui::Cmd::none()
            }

            // -- Did-you-mean suggestions -------------------------------------
            CassMsg::SuggestionApplied(idx) => {
                let idx = idx.saturating_sub(1) as usize;
                if let Some(suggestion) = self.suggestions.get(idx)
                    && let Some(ref q) = suggestion.suggested_query
                {
                    self.query = q.clone();
                    return ftui::Cmd::msg(CassMsg::SearchRequested);
                }
                ftui::Cmd::none()
            }

            // -- Display ------------------------------------------------------
            CassMsg::BordersToggled => {
                self.fancy_borders = !self.fancy_borders;
                self.dirty_since = Some(Instant::now());
                ftui::Cmd::none()
            }
            CassMsg::PaneGrew => {
                self.per_pane_limit = (self.per_pane_limit + 2).min(50);
                self.dirty_since = Some(Instant::now());
                ftui::Cmd::none()
            }
            CassMsg::PaneShrunk => {
                self.per_pane_limit = self.per_pane_limit.saturating_sub(2).max(4);
                self.dirty_since = Some(Instant::now());
                ftui::Cmd::none()
            }

            // -- Saved views --------------------------------------------------
            CassMsg::ViewSaved(slot) => {
                let view = SavedView {
                    slot,
                    agents: self.filters.agents.clone(),
                    workspaces: self.filters.workspaces.clone(),
                    created_from: self.filters.created_from,
                    created_to: self.filters.created_to,
                    ranking: self.ranking_mode,
                    source_filter: self.filters.source_filter.clone(),
                };
                // Replace existing slot or push
                if let Some(existing) = self.saved_views.iter_mut().find(|v| v.slot == slot) {
                    *existing = view;
                } else {
                    self.saved_views.push(view);
                }
                self.dirty_since = Some(Instant::now());
                ftui::Cmd::none()
            }
            CassMsg::ViewLoaded(slot) => {
                if let Some(view) = self.saved_views.iter().find(|v| v.slot == slot) {
                    self.filters.agents = view.agents.clone();
                    self.filters.workspaces = view.workspaces.clone();
                    self.filters.created_from = view.created_from;
                    self.filters.created_to = view.created_to;
                    self.ranking_mode = view.ranking;
                    self.filters.source_filter = view.source_filter.clone();
                    return ftui::Cmd::msg(CassMsg::SearchRequested);
                }
                ftui::Cmd::none()
            }

            // -- Index --------------------------------------------------------
            CassMsg::IndexRefreshRequested => {
                // TODO: dispatch index refresh via Cmd::task
                ftui::Cmd::none()
            }
            CassMsg::IndexProgress { .. } | CassMsg::IndexRefreshCompleted => {
                // TODO: update index progress display
                ftui::Cmd::none()
            }

            // -- State persistence --------------------------------------------
            CassMsg::StateLoadRequested => {
                // TODO: dispatch load via Cmd::task
                ftui::Cmd::none()
            }
            CassMsg::StateLoaded(state) => {
                self.search_mode = state.search_mode;
                self.match_mode = state.match_mode;
                self.ranking_mode = state.ranking_mode;
                self.context_window = state.context_window;
                self.theme_dark = state.theme_dark;
                self.theme_preset = if self.theme_dark {
                    UiThemePreset::Dark
                } else {
                    UiThemePreset::Light
                };
                self.style_options.dark_mode = self.theme_dark;
                self.style_options.preset = self.theme_preset;
                self.density_mode = state.density_mode;
                self.per_pane_limit = state.per_pane_limit;
                self.query_history = state.query_history;
                self.saved_views = state.saved_views;
                self.fancy_borders = state.fancy_borders;
                self.help_pinned = state.help_pinned;
                ftui::Cmd::none()
            }
            CassMsg::StateSaveRequested => {
                // TODO: dispatch save via Cmd::task
                ftui::Cmd::none()
            }
            CassMsg::StateResetRequested => {
                *self = CassApp::default();
                ftui::Cmd::none()
            }

            // -- Toast notifications ------------------------------------------
            CassMsg::ToastShown {
                message,
                toast_type,
            } => {
                self.toast_manager
                    .push(crate::ui::components::toast::Toast::new(
                        message, toast_type,
                    ));
                ftui::Cmd::none()
            }
            CassMsg::ToastTick => {
                self.toast_manager.tick();
                ftui::Cmd::none()
            }

            // -- Window & terminal --------------------------------------------
            CassMsg::Resized { .. } => {
                // Frame dimensions update automatically via ftui runtime
                ftui::Cmd::none()
            }
            CassMsg::Tick => {
                self.spinner_frame = self.spinner_frame.wrapping_add(1);
                self.last_tick = Instant::now();
                // Clear expired flash indicators
                if self.focus_flash_until.is_some_and(|t| Instant::now() > t) {
                    self.focus_flash_until = None;
                }
                if self.peek_badge_until.is_some_and(|t| Instant::now() > t) {
                    self.peek_badge_until = None;
                }
                ftui::Cmd::msg(CassMsg::ToastTick)
            }
            CassMsg::MouseEvent { kind, x: _, y: _ } => {
                // TODO: hit-test against last_pane_rects / last_pill_rects
                match kind {
                    MouseEventKind::ScrollUp => {
                        ftui::Cmd::msg(CassMsg::SelectionMoved { delta: -3 })
                    }
                    MouseEventKind::ScrollDown => {
                        ftui::Cmd::msg(CassMsg::SelectionMoved { delta: 3 })
                    }
                    MouseEventKind::LeftClick => ftui::Cmd::none(),
                }
            }

            // -- Lifecycle ----------------------------------------------------
            CassMsg::QuitRequested => {
                // ESC unwind: check pending state before quitting
                if self.show_consent_dialog {
                    self.show_consent_dialog = false;
                    return ftui::Cmd::none();
                }
                if self.show_export_modal {
                    self.show_export_modal = false;
                    self.export_modal_state = None;
                    return ftui::Cmd::none();
                }
                if self.show_bulk_modal {
                    self.show_bulk_modal = false;
                    return ftui::Cmd::none();
                }
                if self.source_filter_menu_open {
                    self.source_filter_menu_open = false;
                    return ftui::Cmd::none();
                }
                if self.palette_state.open {
                    self.palette_state.open = false;
                    return ftui::Cmd::none();
                }
                if self.show_help {
                    self.show_help = false;
                    return ftui::Cmd::none();
                }
                if self.show_detail_modal {
                    self.show_detail_modal = false;
                    self.focus_region = FocusRegion::Results;
                    return ftui::Cmd::none();
                }
                if self.detail_find.is_some() {
                    self.detail_find = None;
                    return ftui::Cmd::none();
                }
                if self.pane_filter.is_some() {
                    self.pane_filter = None;
                    self.input_mode = InputMode::Query;
                    return ftui::Cmd::none();
                }
                if !self.selected.is_empty() {
                    self.selected.clear();
                    return ftui::Cmd::none();
                }
                if self.input_mode != InputMode::Query {
                    self.input_mode = InputMode::Query;
                    self.input_buffer.clear();
                    return ftui::Cmd::none();
                }
                ftui::Cmd::quit()
            }
            CassMsg::ForceQuit => ftui::Cmd::quit(),
        }
    }

    fn view(&self, frame: &mut super::ftui_adapter::Frame) {
        let area = Rect::from_size(frame.buffer.width(), frame.buffer.height());
        if area.is_empty() {
            return;
        }

        let breakpoint = LayoutBreakpoint::from_width(area.width);
        let border_type = if self.fancy_borders {
            BorderType::Rounded
        } else {
            BorderType::Square
        };
        let row_h = self.density_mode.row_height();

        let styles = self.resolved_style_context();
        let root_style = styles.style(style_system::STYLE_APP_ROOT);
        let pane_style = styles.style(style_system::STYLE_PANE_BASE);
        let pane_focused_style = styles.style(style_system::STYLE_PANE_FOCUSED);
        let row_style = styles.style(style_system::STYLE_RESULT_ROW);
        let row_alt_style = styles.style(style_system::STYLE_RESULT_ROW_ALT);
        let row_selected_style = styles.style(style_system::STYLE_RESULT_ROW_SELECTED);
        let text_muted_style = styles.style(style_system::STYLE_TEXT_MUTED);

        // Paint root background across the entire terminal.
        Block::new().style(root_style).render(area, frame);

        // ── Main vertical split: search bar | content | status ──────────
        let vertical = Flex::vertical()
            .constraints([
                Constraint::Fixed(3), // Search bar
                Constraint::Min(4),   // Content area (results + detail)
                Constraint::Fixed(1), // Status footer
            ])
            .split(area);

        // ── Search bar ──────────────────────────────────────────────────
        let query_title = format!(
            "cass | {} | {:?}/{:?}",
            self.theme_preset.name(),
            self.search_mode,
            self.match_mode
        );
        let query_block = Block::new()
            .borders(Borders::ALL)
            .border_type(border_type)
            .title(&query_title)
            .title_alignment(Alignment::Left)
            .style(if self.focus_region == FocusRegion::Results {
                pane_focused_style
            } else {
                pane_style
            });
        let query_inner = query_block.inner(vertical[0]);
        query_block.render(vertical[0], frame);
        if !query_inner.is_empty() {
            let query_line = if self.query.is_empty() {
                "<type to search>"
            } else {
                self.query.as_str()
            };
            let query_style = if self.query.is_empty() {
                text_muted_style
            } else {
                styles.style(style_system::STYLE_TEXT_PRIMARY)
            };
            Paragraph::new(query_line)
                .style(query_style)
                .render(query_inner, frame);
        }

        // ── Content area: responsive layout ─────────────────────────────
        let content_area = vertical[1];

        let (hits, selected_idx) = if let Some(pane) = self.panes.get(self.active_pane) {
            (&pane.hits[..], pane.selected)
        } else {
            (&self.results[..], 0)
        };

        match breakpoint {
            LayoutBreakpoint::Wide => {
                // Side-by-side: results (60%) | detail (40%)
                let panes = Flex::horizontal()
                    .constraints([Constraint::Percentage(60.0), Constraint::Percentage(40.0)])
                    .gap(0)
                    .split(content_area);
                self.render_results_pane(
                    frame,
                    panes[0],
                    hits,
                    selected_idx,
                    row_h,
                    border_type,
                    &styles,
                    pane_style,
                    pane_focused_style,
                    row_style,
                    row_alt_style,
                    row_selected_style,
                    text_muted_style,
                );
                self.render_detail_pane(
                    frame,
                    panes[1],
                    border_type,
                    &styles,
                    pane_style,
                    pane_focused_style,
                    text_muted_style,
                );
            }
            LayoutBreakpoint::Medium => {
                // Side-by-side but with flexible ratio
                let panes = Flex::horizontal()
                    .constraints([Constraint::Min(40), Constraint::Min(32)])
                    .gap(0)
                    .split(content_area);
                self.render_results_pane(
                    frame,
                    panes[0],
                    hits,
                    selected_idx,
                    row_h,
                    border_type,
                    &styles,
                    pane_style,
                    pane_focused_style,
                    row_style,
                    row_alt_style,
                    row_selected_style,
                    text_muted_style,
                );
                self.render_detail_pane(
                    frame,
                    panes[1],
                    border_type,
                    &styles,
                    pane_style,
                    pane_focused_style,
                    text_muted_style,
                );
            }
            LayoutBreakpoint::Narrow => {
                // Single pane: show whichever has focus
                match self.focus_region {
                    FocusRegion::Results => {
                        self.render_results_pane(
                            frame,
                            content_area,
                            hits,
                            selected_idx,
                            row_h,
                            border_type,
                            &styles,
                            pane_style,
                            pane_focused_style,
                            row_style,
                            row_alt_style,
                            row_selected_style,
                            text_muted_style,
                        );
                    }
                    FocusRegion::Detail => {
                        self.render_detail_pane(
                            frame,
                            content_area,
                            border_type,
                            &styles,
                            pane_style,
                            pane_focused_style,
                            text_muted_style,
                        );
                    }
                }
            }
        }

        // ── Status footer ───────────────────────────────────────────────
        let bp_label = match breakpoint {
            LayoutBreakpoint::Narrow => "narrow",
            LayoutBreakpoint::Medium => "med",
            LayoutBreakpoint::Wide => "wide",
        };
        let density_label = match self.density_mode {
            DensityMode::Compact => "compact",
            DensityMode::Cozy => "cozy",
            DensityMode::Spacious => "spacious",
        };
        let status_line = if self.status.is_empty() {
            format!(
                " {} hits | {} | {} | {:?} | F2=theme D=density Ctrl+B=borders",
                hits.len(),
                bp_label,
                density_label,
                styles.options.color_profile
            )
        } else {
            format!(" {}", self.status)
        };
        Paragraph::new(&*status_line)
            .style(text_muted_style)
            .render(vertical[2], frame);

        // ── Command palette overlay ──────────────────────────────────
        if self.palette_state.open {
            self.render_palette_overlay(frame, area, &styles);
        }
    }
}

// =========================================================================
// Entry Point
// =========================================================================

/// Run the cass TUI using the ftui Program runtime.
///
/// This replaces the manual crossterm event loop in `run_tui()`.
/// The ftui runtime handles terminal lifecycle (raw mode, alt-screen),
/// event polling, rendering, and cleanup via RAII.
pub fn run_tui_ftui() -> anyhow::Result<()> {
    let model = CassApp::default();

    ftui::App::fullscreen(model)
        .with_mouse()
        .run()
        .map_err(|e| anyhow::anyhow!("ftui runtime error: {e}"))
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cass_app_default_initializes_with_sane_defaults() {
        let app = CassApp::default();
        assert!(app.query.is_empty());
        assert!(app.panes.is_empty());
        assert!(app.results.is_empty());
        assert_eq!(app.active_pane, 0);
        assert_eq!(app.per_pane_limit, 10);
        assert_eq!(app.input_mode, InputMode::Query);
        assert_eq!(app.focus_region, FocusRegion::Results);
        assert_eq!(app.search_mode, SearchMode::Lexical);
        assert_eq!(app.match_mode, MatchMode::Standard);
        assert_eq!(app.ranking_mode, RankingMode::Balanced);
        assert_eq!(app.context_window, ContextWindow::Medium);
        assert_eq!(app.density_mode, DensityMode::Cozy);
        assert!(app.theme_dark);
        assert_eq!(app.theme_preset, UiThemePreset::Dark);
        assert!(app.fancy_borders);
        assert!(!app.show_help);
        assert!(!app.show_detail_modal);
        assert!(!app.show_export_modal);
        assert!(!app.show_bulk_modal);
        assert!(!app.show_consent_dialog);
        assert!(!app.source_filter_menu_open);
        assert!(app.selected.is_empty());
        assert!(app.saved_views.is_empty());
        assert!(app.query_history.is_empty());
    }

    #[test]
    fn all_detail_tab_variants_constructible() {
        let _msgs = DetailTab::Messages;
        let _snip = DetailTab::Snippets;
        let _raw = DetailTab::Raw;
    }

    #[test]
    fn all_match_mode_variants_constructible() {
        let _std = MatchMode::Standard;
        let _pfx = MatchMode::Prefix;
    }

    #[test]
    fn all_ranking_mode_variants_constructible() {
        let _rh = RankingMode::RecentHeavy;
        let _bal = RankingMode::Balanced;
        let _rel = RankingMode::RelevanceHeavy;
        let _mq = RankingMode::MatchQualityHeavy;
        let _dn = RankingMode::DateNewest;
        let _do_ = RankingMode::DateOldest;
    }

    #[test]
    fn all_context_window_variants_constructible() {
        let _s = ContextWindow::Small;
        let _m = ContextWindow::Medium;
        let _l = ContextWindow::Large;
        let _xl = ContextWindow::XLarge;
    }

    #[test]
    fn all_density_mode_variants_constructible() {
        let _c = DensityMode::Compact;
        let _co = DensityMode::Cozy;
        let _s = DensityMode::Spacious;
    }

    #[test]
    fn all_focus_region_variants_constructible() {
        let _r = FocusRegion::Results;
        let _d = FocusRegion::Detail;
    }

    #[test]
    fn cass_msg_key_variants_constructible() {
        // Verify a representative sample of message variants compile.
        let _q = CassMsg::QueryChanged("test".into());
        let _s = CassMsg::SearchRequested;
        let _f = CassMsg::FiltersClearAll;
        let _m = CassMsg::SearchModeCycled;
        let _n = CassMsg::SelectionMoved { delta: 1 };
        let _d = CassMsg::DetailOpened;
        let _p = CassMsg::PaletteOpened;
        let _h = CassMsg::HelpToggled;
        let _t = CassMsg::ThemeToggled;
        let _tick = CassMsg::Tick;
        let _quit = CassMsg::QuitRequested;
        let _fq = CassMsg::ForceQuit;
    }

    #[test]
    fn persisted_state_constructible() {
        let _state = PersistedState {
            search_mode: SearchMode::Lexical,
            match_mode: MatchMode::Standard,
            ranking_mode: RankingMode::Balanced,
            context_window: ContextWindow::Medium,
            theme_dark: true,
            density_mode: DensityMode::Cozy,
            per_pane_limit: 10,
            query_history: VecDeque::new(),
            saved_views: Vec::new(),
            fancy_borders: true,
            help_pinned: false,
            has_seen_help: false,
        };
    }

    #[test]
    fn search_result_constructible() {
        let _result = SearchResult {
            hits: Vec::new(),
            elapsed_ms: 42,
            suggestions: Vec::new(),
            wildcard_fallback: false,
        };
    }

    #[test]
    fn export_result_constructible() {
        let _result = ExportResult {
            output_path: PathBuf::from("/tmp/export.html"),
            file_size: 1024,
            encrypted: false,
            message_count: 10,
        };
    }

    use crate::ui::ftui_adapter::Model;

    /// Extract the inner message from a Cmd::Msg, if present.
    fn extract_msg(cmd: ftui::Cmd<CassMsg>) -> Option<CassMsg> {
        match cmd {
            ftui::Cmd::Msg(m) => Some(m),
            _ => None,
        }
    }

    // ==================== Command palette tests ====================

    #[test]
    fn palette_state_initialized_with_default_actions() {
        let app = CassApp::default();
        assert!(
            !app.palette_state.all_actions.is_empty(),
            "palette should be initialized with actions"
        );
        // Should have at least the core 12 base actions + 18 slot actions = 30
        assert!(app.palette_state.all_actions.len() >= 30);
    }

    #[test]
    fn palette_state_not_open_by_default() {
        let app = CassApp::default();
        assert!(!app.palette_state.open);
    }

    #[test]
    fn palette_open_sets_state() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::PaletteOpened);
        assert!(app.palette_state.open);
        assert!(app.palette_state.query.is_empty());
        assert_eq!(app.palette_state.selected, 0);
        assert_eq!(
            app.palette_state.filtered.len(),
            app.palette_state.all_actions.len()
        );
    }

    #[test]
    fn palette_close_clears_open() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::PaletteOpened);
        assert!(app.palette_state.open);
        let _ = app.update(CassMsg::PaletteClosed);
        assert!(!app.palette_state.open);
    }

    #[test]
    fn palette_query_filters_actions() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::PaletteOpened);
        let total = app.palette_state.filtered.len();
        let _ = app.update(CassMsg::PaletteQueryChanged("theme".into()));
        assert!(app.palette_state.filtered.len() < total);
        assert!(
            app.palette_state
                .filtered
                .iter()
                .any(|i| i.label.to_lowercase().contains("theme"))
        );
    }

    #[test]
    fn palette_selection_wraps() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::PaletteOpened);
        let len = app.palette_state.filtered.len();
        // Move past end -> wraps
        let _ = app.update(CassMsg::PaletteSelectionMoved {
            delta: len as i32 + 1,
        });
        assert!(app.palette_state.selected < len);
    }

    #[test]
    fn palette_execute_theme_toggles_dark() {
        let mut app = CassApp::default();
        assert!(app.theme_dark);

        // Open palette and select "Toggle theme" (first action)
        let _ = app.update(CassMsg::PaletteOpened);
        app.palette_state.selected = 0;
        // Verify first action is ToggleTheme
        assert!(matches!(
            app.palette_state.filtered[0].action,
            PaletteAction::ToggleTheme
        ));

        // Execute it - should produce ThemeToggled cmd
        let cmd = app.update(CassMsg::PaletteActionExecuted);
        assert!(!app.palette_state.open, "palette should close on execute");
        // The returned Cmd contains CassMsg::ThemeToggled; process it
        if let Some(msg) = extract_msg(cmd) {
            let _ = app.update(msg);
        }
        assert!(!app.theme_dark, "theme should have toggled to light");
    }

    #[test]
    fn palette_execute_density_cycles() {
        let mut app = CassApp::default();
        assert_eq!(app.density_mode, DensityMode::Cozy);

        let _ = app.update(CassMsg::PaletteOpened);
        // Find density action
        let idx = app
            .palette_state
            .filtered
            .iter()
            .position(|i| matches!(i.action, PaletteAction::ToggleDensity))
            .expect("density action should exist");
        app.palette_state.selected = idx;
        let cmd = app.update(CassMsg::PaletteActionExecuted);
        if let Some(msg) = extract_msg(cmd) {
            let _ = app.update(msg);
        }
        assert_eq!(app.density_mode, DensityMode::Spacious);
    }

    #[test]
    fn palette_execute_reload_index() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::PaletteOpened);
        let idx = app
            .palette_state
            .filtered
            .iter()
            .position(|i| matches!(i.action, PaletteAction::ReloadIndex))
            .expect("reload action should exist");
        app.palette_state.selected = idx;
        let cmd = app.update(CassMsg::PaletteActionExecuted);
        // Should produce IndexRefreshRequested
        assert!(!app.palette_state.open);
        // cmd should contain a message (IndexRefreshRequested)
        assert!(extract_msg(cmd).is_some());
    }

    #[test]
    fn palette_escape_closes_before_quit() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::PaletteOpened);
        assert!(app.palette_state.open);
        // ESC should close palette, not quit
        let _ = app.update(CassMsg::QuitRequested);
        assert!(!app.palette_state.open);
    }

    #[test]
    fn palette_hints_use_shortcut_constants() {
        let app = CassApp::default();
        // The Toggle theme action should have the F2 shortcut as hint
        let theme_action = app
            .palette_state
            .all_actions
            .iter()
            .find(|i| matches!(i.action, PaletteAction::ToggleTheme))
            .expect("theme action should exist");
        assert_eq!(theme_action.hint, "F2");

        // Filter agent should have F3
        let filter_action = app
            .palette_state
            .all_actions
            .iter()
            .find(|i| matches!(i.action, PaletteAction::FilterAgent))
            .expect("filter agent should exist");
        assert_eq!(filter_action.hint, "F3");
    }

    #[test]
    fn palette_save_view_slot_dispatches() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::PaletteOpened);
        // Find SaveViewSlot(1)
        let idx = app
            .palette_state
            .filtered
            .iter()
            .position(|i| matches!(i.action, PaletteAction::SaveViewSlot(1)))
            .expect("save slot 1 should exist");
        app.palette_state.selected = idx;
        let cmd = app.update(CassMsg::PaletteActionExecuted);
        if let Some(msg) = extract_msg(cmd) {
            let _ = app.update(msg);
        }
        assert!(
            app.saved_views.iter().any(|v| v.slot == 1),
            "slot 1 should be saved"
        );
    }
}
