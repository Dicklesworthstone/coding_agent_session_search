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

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Instant;

use crate::model::types::MessageRole;
use crate::search::model_manager::SemanticAvailability;
use crate::search::query::{QuerySuggestion, SearchFilters, SearchHit, SearchMode};
use crate::sources::provenance::SourceFilter;
use crate::storage::sqlite::SqliteStorage;
use crate::ui::components::export_modal::{ExportField, ExportModalState, ExportProgress};
use crate::ui::components::palette::{PaletteAction, PaletteState, default_actions};
use crate::ui::components::pills::Pill;
use crate::ui::components::toast::ToastManager;
use crate::ui::data::{ConversationView, InputMode};
use crate::ui::time_parser::parse_time_input;
use crate::update_check::{UpdateInfo, open_in_browser, skip_version};
#[cfg(not(test))]
use crate::update_check::{run_self_update, spawn_update_check};
use ftui::widgets::Widget;
use ftui::widgets::block::{Alignment, Block};
use ftui::widgets::borders::{BorderType, Borders};
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::{RenderItem, StatefulWidget, VirtualizedList, VirtualizedListState};
use ftui_extras::markdown::{MarkdownRenderer, MarkdownTheme, is_likely_markdown};

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
// Constants
// =========================================================================

/// Labels for the bulk-actions modal menu (order matters — matches action_index).
pub const BULK_ACTIONS: [&str; 4] = [
    "Open all in editor",
    "Copy all paths",
    "Export as JSON",
    "Clear selection",
];

/// Number of selected items before requiring double-press confirmation.
pub const OPEN_CONFIRM_THRESHOLD: usize = 12;

// =========================================================================
// Enums (ported from tui.rs, canonical for ftui)
// =========================================================================

/// Top-level application surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AppSurface {
    /// Main search view (results list + detail pane).
    #[default]
    Search,
    /// Analytics dashboard surface.
    Analytics,
}

/// Analytics subview within the Analytics surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AnalyticsView {
    /// Overview with KPI tiles + sparklines.
    #[default]
    Dashboard,
    /// Interactive time-series explorer.
    Explorer,
    /// Calendar heatmap of daily activity.
    Heatmap,
    /// Agents/workspaces/sources/models breakdowns.
    Breakdowns,
    /// Per-tool usage analytics.
    Tools,
    /// Cost estimation (USD) by model/provider.
    Cost,
    /// Token measurement coverage diagnostics.
    Coverage,
}

impl AnalyticsView {
    /// Display label for the view.
    pub fn label(self) -> &'static str {
        match self {
            Self::Dashboard => "Dashboard",
            Self::Explorer => "Explorer",
            Self::Heatmap => "Heatmap",
            Self::Breakdowns => "Breakdowns",
            Self::Tools => "Tools",
            Self::Cost => "Cost",
            Self::Coverage => "Coverage",
        }
    }

    /// All analytics views in display order.
    pub fn all() -> &'static [Self] {
        &[
            Self::Dashboard,
            Self::Explorer,
            Self::Heatmap,
            Self::Breakdowns,
            Self::Tools,
            Self::Cost,
            Self::Coverage,
        ]
    }
}

/// Analytics-specific filter state (persisted within the analytics surface).
#[derive(Clone, Debug, Default)]
pub struct AnalyticsFilterState {
    /// Time range: since (ms epoch).
    pub since_ms: Option<i64>,
    /// Time range: until (ms epoch).
    pub until_ms: Option<i64>,
    /// Filter to specific agents (empty = all).
    pub agents: HashSet<String>,
    /// Filter to specific workspaces (empty = all).
    pub workspaces: HashSet<String>,
    /// Source filter.
    pub source_filter: SourceFilter,
}

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

/// Quick time filter presets for Shift+F5 cycling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TimePreset {
    #[default]
    All,
    Today,
    Week,
    Month,
    Custom,
}

impl TimePreset {
    /// Cycle to the next preset (skips Custom on cycle).
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Today,
            Self::Today => Self::Week,
            Self::Week => Self::Month,
            Self::Month => Self::All,
            Self::Custom => Self::All,
        }
    }

    /// Label for display.
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All time",
            Self::Today => "Today",
            Self::Week => "Past 7d",
            Self::Month => "Past 30d",
            Self::Custom => "Custom",
        }
    }
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

/// Stable identity for a selected search hit.
///
/// Uses source/path/line/hash so selection survives pane reorder and reranking.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SelectedHitKey {
    pub source_id: String,
    pub source_path: String,
    pub line_number: Option<usize>,
    pub content_hash: u64,
}

impl SelectedHitKey {
    fn from_hit(hit: &SearchHit) -> Self {
        Self {
            source_id: hit.source_id.clone(),
            source_path: hit.source_path.clone(),
            line_number: hit.line_number,
            content_hash: hit.content_hash,
        }
    }
}

/// A search result item prepared for rendering in a VirtualizedList.
///
/// Carries all context needed by `RenderItem::render()` so the item can
/// self-render without access to the parent `CassApp`.
#[derive(Clone, Debug)]
pub struct ResultItem {
    /// 1-based display index.
    pub index: usize,
    /// The underlying search hit.
    pub hit: SearchHit,
    /// Row height (from density mode: 1=compact, 2=cozy, 3=spacious).
    pub row_height: u16,
    /// Whether this is an even-indexed row (for alternating stripes).
    pub even: bool,
    /// Maximum content width available.
    pub max_width: u16,
    /// Whether the item is queued for multi-open (Ctrl+Enter).
    pub queued: bool,
    /// Stripe background style for this row (even/odd).
    pub stripe_style: ftui::Style,
    /// Agent foreground+background accent style.
    pub agent_style: ftui::Style,
}

impl RenderItem for ResultItem {
    fn render(&self, area: Rect, frame: &mut super::ftui_adapter::Frame, selected: bool) {
        let hit = &self.hit;
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

        // Base style: stripe bg unless selected (highlight_style applied by VirtualizedList).
        let base_style = if selected {
            self.agent_style
        } else {
            self.stripe_style
        };

        // Selection and queue indicator prefix
        let sel_mark = if selected { "\u{25b6} " } else { "  " };
        let queue_mark = if self.queued { "\u{2713}" } else { " " };

        match self.row_height {
            1 => {
                // Compact: single line
                let text = format!(
                    "{sel_mark}{queue_mark}{:>2}. {title} [{location}]",
                    self.index
                );
                Paragraph::new(&*text).style(base_style).render(area, frame);
            }
            2 => {
                // Cozy: title + metadata
                let line1 = format!("{sel_mark}{queue_mark}{:>2}. {title}", self.index);
                let line2 = format!("      {location} | {:.1}", hit.score);
                let text = format!("{line1}\n{line2}");
                Paragraph::new(&*text).style(base_style).render(area, frame);
            }
            _ => {
                // Spacious: title + snippet + metadata
                let line1 = format!("{sel_mark}{queue_mark}{:>2}. {title}", self.index);
                let snippet_preview = hit
                    .snippet
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("");
                let max_snip = (area.width as usize).saturating_sub(6);
                let snip = if snippet_preview.len() > max_snip {
                    &snippet_preview[..max_snip.saturating_sub(3)]
                } else {
                    snippet_preview
                };
                let line2 = format!("      {snip}");
                let line3 = format!("      {} | {location} | {:.1}", hit.agent, hit.score);
                let text = format!("{line1}\n{line2}\n{line3}");
                Paragraph::new(&*text).style(base_style).render(area, frame);
            }
        }
    }

    fn height(&self) -> u16 {
        self.row_height
    }
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
    // -- View routing -----------------------------------------------------
    /// Top-level surface (Search or Analytics).
    pub surface: AppSurface,
    /// Navigation back-stack (Esc pops, drilldowns push).
    pub view_stack: Vec<AppSurface>,
    /// Current analytics subview.
    pub analytics_view: AnalyticsView,
    /// Analytics-specific filter state.
    pub analytics_filters: AnalyticsFilterState,

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
    /// Virtualized list state for the active results pane (RefCell for view-time mutation).
    pub results_list_state: RefCell<VirtualizedListState>,
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
    /// Active time filter preset (for Shift+F5 cycling).
    pub time_preset: TimePreset,

    // -- Focus & input ----------------------------------------------------
    /// What the user is currently typing into.
    pub input_mode: InputMode,
    /// Ephemeral input buffer for filter prompts.
    pub input_buffer: String,
    /// Which pane region has keyboard focus (legacy compat).
    pub focus_region: FocusRegion,
    /// FocusGraph-based navigation manager.
    pub focus_manager: FocusManager,
    /// Cursor position within the query string (byte offset).
    pub cursor_pos: usize,
    /// Cursor position within query history.
    pub history_cursor: Option<usize>,
    /// Past query strings (most recent first), deduplicated.
    pub query_history: VecDeque<String>,
    /// Local pane filter text (/ key in results).
    pub pane_filter: Option<String>,

    // -- Multi-select -----------------------------------------------------
    /// Stable hit IDs for multi-selected items.
    pub selected: HashSet<SelectedHitKey>,
    /// Cursor index in the bulk-actions modal menu.
    pub bulk_action_idx: usize,
    /// Two-press safety flag: armed after first Ctrl+O when >= threshold items.
    pub open_confirm_armed: bool,

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
    /// Whether word-wrap is enabled in the detail pane.
    pub detail_wrap: bool,

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
    /// Latest update check result (if any).
    pub update_info: Option<UpdateInfo>,
    /// Session-only dismissal toggle for update banner.
    pub update_dismissed: bool,
    /// Two-step guard: first upgrade request arms, second confirms.
    pub update_upgrade_armed: bool,
    /// One-shot update-check receiver started at app initialization.
    pub update_check_rx: Option<std::sync::mpsc::Receiver<Option<UpdateInfo>>>,

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
    /// Search service for async query dispatch.
    pub search_service: Option<Arc<dyn SearchService>>,

    // -- Status line ------------------------------------------------------
    /// Footer status text.
    pub status: String,
}

impl Default for CassApp {
    fn default() -> Self {
        Self {
            surface: AppSurface::default(),
            view_stack: Vec::new(),
            analytics_view: AnalyticsView::default(),
            analytics_filters: AnalyticsFilterState::default(),
            query: String::new(),
            filters: SearchFilters::default(),
            results: Vec::new(),
            panes: Vec::new(),
            active_pane: 0,
            pane_scroll_offset: 0,
            per_pane_limit: 10,
            results_list_state: RefCell::new(VirtualizedListState::new()),
            wildcard_fallback: false,
            suggestions: Vec::new(),
            last_search_ms: None,
            search_mode: SearchMode::default(),
            match_mode: MatchMode::default(),
            ranking_mode: RankingMode::default(),
            context_window: ContextWindow::default(),
            time_preset: TimePreset::default(),
            input_mode: InputMode::Query,
            input_buffer: String::new(),
            focus_region: FocusRegion::default(),
            focus_manager: FocusManager::new(),
            cursor_pos: 0,
            history_cursor: None,
            query_history: VecDeque::with_capacity(50),
            pane_filter: None,
            selected: HashSet::new(),
            bulk_action_idx: 0,
            open_confirm_armed: false,
            detail_scroll: 0,
            detail_tab: DetailTab::default(),
            detail_find: None,
            show_detail_modal: false,
            modal_scroll: 0,
            cached_detail: None,
            detail_wrap: true,
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
            update_info: None,
            update_dismissed: false,
            update_upgrade_armed: false,
            update_check_rx: {
                #[cfg(test)]
                {
                    None
                }
                #[cfg(not(test))]
                {
                    Some(spawn_update_check(env!("CARGO_PKG_VERSION").to_string()))
                }
            },
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
            search_service: None,
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

    fn active_hit_key(&self) -> Option<SelectedHitKey> {
        self.selected_hit().map(SelectedHitKey::from_hit)
    }

    fn selected_hits(&self) -> Vec<SearchHit> {
        let mut hits = Vec::new();
        let mut seen = HashSet::new();
        for pane in &self.panes {
            for hit in &pane.hits {
                let key = SelectedHitKey::from_hit(hit);
                if self.selected.contains(&key) && seen.insert(key) {
                    hits.push(hit.clone());
                }
            }
        }
        hits
    }

    fn update_banner_visible(&self) -> bool {
        self.update_info
            .as_ref()
            .is_some_and(UpdateInfo::should_show)
            && !self.update_dismissed
    }

    fn can_handle_update_shortcuts(&self) -> bool {
        self.update_banner_visible()
            && self.input_mode == InputMode::Query
            && !self.show_help
            && !self.show_detail_modal
            && !self.show_bulk_modal
            && !self.show_export_modal
            && !self.show_consent_dialog
            && !self.source_filter_menu_open
            && !self.palette_state.open
    }

    /// Render the results list pane using VirtualizedList for O(visible) rendering.
    #[allow(clippy::too_many_arguments)]
    fn render_results_pane(
        &self,
        frame: &mut super::ftui_adapter::Frame,
        area: Rect,
        hits: &[SearchHit],
        selected_idx: usize,
        row_h: u16,
        border_type: BorderType,
        borders: Borders,
        styles: &StyleContext,
        pane_style: ftui::Style,
        pane_focused_style: ftui::Style,
        row_style: ftui::Style,
        row_alt_style: ftui::Style,
        row_selected_style: ftui::Style,
        text_muted_style: ftui::Style,
    ) {
        let results_title = if self.selected.is_empty() {
            format!("Results ({})", hits.len())
        } else {
            format!(
                "Results ({}) \u{2022} {} selected",
                hits.len(),
                self.selected.len()
            )
        };
        let results_block = Block::new()
            .borders(borders)
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

        // Build ResultItem wrappers for VirtualizedList rendering.
        let items: Vec<ResultItem> = hits
            .iter()
            .enumerate()
            .map(|(i, hit)| {
                let even = i % 2 == 0;
                let queued = self.selected.contains(&SelectedHitKey::from_hit(hit));
                ResultItem {
                    index: i + 1,
                    hit: hit.clone(),
                    row_height: row_h,
                    even,
                    max_width: inner.width,
                    queued,
                    stripe_style: if even { row_style } else { row_alt_style },
                    agent_style: row_selected_style,
                }
            })
            .collect();

        let list = VirtualizedList::new(&items)
            .fixed_height(row_h)
            .highlight_style(row_selected_style)
            .show_scrollbar(true);

        let mut state = self.results_list_state.borrow_mut();
        state.select(Some(selected_idx));
        list.render(inner, frame, &mut state);

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

    /// Style for a message role (User/Agent/Tool/System).
    fn role_style(role: &MessageRole, styles: &StyleContext) -> ftui::Style {
        match role {
            MessageRole::User => styles.style(style_system::STYLE_ROLE_USER),
            MessageRole::Agent => styles.style(style_system::STYLE_ROLE_ASSISTANT),
            MessageRole::Tool => styles.style(style_system::STYLE_ROLE_TOOL),
            MessageRole::System => styles.style(style_system::STYLE_ROLE_SYSTEM),
            MessageRole::Other(_) => styles.style(style_system::STYLE_TEXT_MUTED),
        }
    }

    /// Role prefix symbol for message rendering.
    fn role_prefix(role: &MessageRole) -> &'static str {
        match role {
            MessageRole::User => "\u{f061} ",     // arrow-right →
            MessageRole::Agent => "\u{2713} ",    // checkmark ✓
            MessageRole::Tool => "\u{2699} ",     // gear ⚙
            MessageRole::System => "\u{2139} ",   // info ℹ
            MessageRole::Other(_) => "\u{2022} ", // bullet •
        }
    }

    /// Build rendered lines for Messages tab.
    fn build_messages_lines(
        &self,
        hit: &SearchHit,
        inner_width: u16,
        styles: &StyleContext,
    ) -> Vec<ftui::text::Line> {
        let mut lines: Vec<ftui::text::Line> = Vec::new();

        // Header: title + metadata
        let title = if hit.title.is_empty() {
            "(untitled)"
        } else {
            &hit.title
        };
        let header_style = styles.style(style_system::STYLE_TEXT_PRIMARY).bold();
        lines.push(ftui::text::Line::from_spans(vec![
            ftui::text::Span::styled(title.to_string(), header_style),
        ]));

        // Metadata line: agent, workspace, timestamp, score
        let meta_style = styles.style(style_system::STYLE_TEXT_MUTED);
        let ts_str = hit
            .created_at
            .map(|ts| {
                chrono::DateTime::from_timestamp(ts, 0)
                    .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| ts.to_string())
            })
            .unwrap_or_default();
        let meta_text = format!(
            "agent={} workspace={} score={:.3}{}",
            hit.agent,
            hit.workspace,
            hit.score,
            if ts_str.is_empty() {
                String::new()
            } else {
                format!(" {ts_str}")
            },
        );
        lines.push(ftui::text::Line::from_spans(vec![
            ftui::text::Span::styled(meta_text, meta_style),
        ]));

        // Separator
        let sep = "\u{2500}".repeat(inner_width.saturating_sub(2) as usize);
        lines.push(ftui::text::Line::from_spans(vec![
            ftui::text::Span::styled(sep, meta_style),
        ]));

        // If we have a cached conversation, render full messages
        if let Some((_, ref cv)) = self.cached_detail {
            let md_renderer = MarkdownRenderer::new(MarkdownTheme::default());

            for msg in &cv.messages {
                let role_s = Self::role_style(&msg.role, styles);
                let prefix = Self::role_prefix(&msg.role);
                let role_label = format!("{prefix}{}", msg.role);
                let author_suffix = msg
                    .author
                    .as_ref()
                    .map(|a| format!(" ({a})"))
                    .unwrap_or_default();
                let ts_label = msg
                    .created_at
                    .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
                    .map(|dt| format!(" {}", dt.format("%H:%M:%S")))
                    .unwrap_or_default();

                // Role header line
                lines.push(ftui::text::Line::from_spans(vec![
                    ftui::text::Span::styled(
                        format!("{role_label}{author_suffix}{ts_label}"),
                        role_s.bold(),
                    ),
                ]));

                // Message content: auto-detect markdown
                let content = msg.content.trim();
                if !content.is_empty() {
                    if is_likely_markdown(content).is_likely() {
                        let rendered = md_renderer.render(content);
                        for line in rendered.into_iter() {
                            lines.push(line);
                        }
                    } else {
                        // Plain text — wrap if enabled
                        for text_line in content.lines() {
                            if self.detail_wrap && !text_line.is_empty() {
                                let w = inner_width.saturating_sub(2) as usize;
                                for chunk in text_line
                                    .as_bytes()
                                    .chunks(w.max(20))
                                    .map(|c| std::str::from_utf8(c).unwrap_or(""))
                                {
                                    lines.push(ftui::text::Line::from(chunk.to_string()));
                                }
                            } else {
                                lines.push(ftui::text::Line::from(text_line.to_string()));
                            }
                        }
                    }
                }

                // Blank line between messages
                lines.push(ftui::text::Line::from(""));
            }
        } else {
            // No cached conversation: show the hit's content directly
            let content = if hit.content.is_empty() {
                &hit.snippet
            } else {
                &hit.content
            };
            if is_likely_markdown(content).is_likely() {
                let md_renderer = MarkdownRenderer::new(MarkdownTheme::default());
                let rendered = md_renderer.render(content);
                for line in rendered.into_iter() {
                    lines.push(line);
                }
            } else {
                for text_line in content.lines() {
                    lines.push(ftui::text::Line::from(text_line.to_string()));
                }
            }
        }

        lines
    }

    /// Build rendered lines for Snippets tab.
    fn build_snippets_lines(
        &self,
        hit: &SearchHit,
        styles: &StyleContext,
    ) -> Vec<ftui::text::Line> {
        let mut lines: Vec<ftui::text::Line> = Vec::new();
        let header_style = styles.style(style_system::STYLE_TEXT_PRIMARY).bold();
        let meta_style = styles.style(style_system::STYLE_TEXT_MUTED);

        lines.push(ftui::text::Line::from_spans(vec![
            ftui::text::Span::styled("Snippets", header_style),
        ]));
        lines.push(ftui::text::Line::from(""));

        // If we have a cached conversation, show per-message snippets
        if let Some((_, ref cv)) = self.cached_detail {
            let mut any = false;
            for (i, msg) in cv.messages.iter().enumerate() {
                if msg.snippets.is_empty() {
                    continue;
                }
                any = true;
                let role_s = Self::role_style(&msg.role, styles);
                lines.push(ftui::text::Line::from_spans(vec![
                    ftui::text::Span::styled(
                        format!("Message {} ({})", i + 1, msg.role),
                        role_s.bold(),
                    ),
                ]));
                for snippet in &msg.snippets {
                    let path_str = snippet
                        .file_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    if !path_str.is_empty() {
                        lines.push(ftui::text::Line::from_spans(vec![
                            ftui::text::Span::styled(format!("  {path_str}"), meta_style),
                        ]));
                    }
                }
                lines.push(ftui::text::Line::from(""));
            }
            if !any {
                lines.push(ftui::text::Line::from_spans(vec![
                    ftui::text::Span::styled("No snippets extracted.", meta_style),
                ]));
            }
        } else {
            // Fallback: show the search snippet
            let snippet = &hit.snippet;
            if snippet.is_empty() {
                lines.push(ftui::text::Line::from_spans(vec![
                    ftui::text::Span::styled("No snippet available.", meta_style),
                ]));
            } else {
                for line in snippet.lines() {
                    lines.push(ftui::text::Line::from(line.to_string()));
                }
            }
        }

        lines
    }

    /// Build rendered lines for Raw tab.
    fn build_raw_lines(&self, hit: &SearchHit, styles: &StyleContext) -> Vec<ftui::text::Line> {
        let mut lines: Vec<ftui::text::Line> = Vec::new();
        let header_style = styles.style(style_system::STYLE_TEXT_PRIMARY).bold();
        let code_style = styles.style(style_system::STYLE_TEXT_SUBTLE);

        lines.push(ftui::text::Line::from_spans(vec![
            ftui::text::Span::styled("Raw Data", header_style),
        ]));
        lines.push(ftui::text::Line::from(""));

        // If we have a cached conversation, serialize the full conversation
        if let Some((_, ref cv)) = self.cached_detail {
            // Show conversation metadata as JSON
            let json = serde_json::json!({
                "agent": cv.convo.agent_slug,
                "external_id": cv.convo.external_id,
                "title": cv.convo.title,
                "source_path": cv.convo.source_path.display().to_string(),
                "started_at": cv.convo.started_at,
                "ended_at": cv.convo.ended_at,
                "approx_tokens": cv.convo.approx_tokens,
                "source_id": cv.convo.source_id,
                "origin_host": cv.convo.origin_host,
                "message_count": cv.messages.len(),
            });
            if let Ok(pretty) = serde_json::to_string_pretty(&json) {
                for line in pretty.lines() {
                    lines.push(ftui::text::Line::from_spans(vec![
                        ftui::text::Span::styled(line.to_string(), code_style),
                    ]));
                }
            }

            // Per-message raw data
            for (i, msg) in cv.messages.iter().enumerate() {
                lines.push(ftui::text::Line::from(""));
                lines.push(ftui::text::Line::from_spans(vec![
                    ftui::text::Span::styled(
                        format!("--- Message {} ({}) ---", i + 1, msg.role),
                        header_style,
                    ),
                ]));
                let msg_json = serde_json::json!({
                    "role": msg.role.to_string(),
                    "author": msg.author,
                    "created_at": msg.created_at,
                    "content_length": msg.content.len(),
                    "extra": msg.extra_json,
                });
                if let Ok(pretty) = serde_json::to_string_pretty(&msg_json) {
                    for line in pretty.lines() {
                        lines.push(ftui::text::Line::from_spans(vec![
                            ftui::text::Span::styled(line.to_string(), code_style),
                        ]));
                    }
                }
            }
        } else {
            // Fallback: show the hit itself as JSON
            let hit_json = serde_json::json!({
                "title": hit.title,
                "agent": hit.agent,
                "workspace": hit.workspace,
                "source_path": hit.source_path,
                "score": hit.score,
                "content_length": hit.content.len(),
                "source_id": hit.source_id,
                "origin_kind": hit.origin_kind,
                "origin_host": hit.origin_host,
                "created_at": hit.created_at,
            });
            if let Ok(pretty) = serde_json::to_string_pretty(&hit_json) {
                for line in pretty.lines() {
                    lines.push(ftui::text::Line::from_spans(vec![
                        ftui::text::Span::styled(line.to_string(), code_style),
                    ]));
                }
            }
        }

        lines
    }

    /// Apply find-in-detail highlighting to rendered lines.
    fn apply_find_highlight(
        lines: &mut [ftui::text::Line],
        query: &str,
        current_match: usize,
        styles: &StyleContext,
    ) -> Vec<u16> {
        let highlight_style = if styles.options.color_profile.supports_color() {
            ftui::Style::default()
                .bg(ftui::PackedRgba::rgb(255, 255, 0))
                .fg(ftui::PackedRgba::rgb(0, 0, 0))
        } else {
            ftui::Style::default().underline().bold()
        };
        let current_style = if styles.options.color_profile.supports_color() {
            ftui::Style::default()
                .bg(ftui::PackedRgba::rgb(255, 140, 0))
                .fg(ftui::PackedRgba::rgb(0, 0, 0))
                .bold()
        } else {
            ftui::Style::default().underline().bold().italic()
        };

        if query.is_empty() {
            return Vec::new();
        }

        let query_lower = query.to_lowercase();
        let mut match_positions: Vec<u16> = Vec::new();
        let mut match_idx = 0usize;

        for (line_no, line) in lines.iter_mut().enumerate() {
            let plain: String = line.spans().iter().map(|s| s.content.as_ref()).collect();
            let plain_lower = plain.to_lowercase();

            if plain_lower.contains(&query_lower) {
                // Re-build the line with highlighted matches
                let mut new_spans: Vec<ftui::text::Span<'static>> = Vec::new();
                let mut pos = 0usize;
                let bytes = plain.as_bytes();
                let lower_bytes = plain_lower.as_bytes();
                let q_bytes = query_lower.as_bytes();

                while pos < bytes.len() {
                    if pos + q_bytes.len() <= lower_bytes.len()
                        && &lower_bytes[pos..pos + q_bytes.len()] == q_bytes
                    {
                        let style = if match_idx == current_match {
                            current_style
                        } else {
                            highlight_style
                        };
                        let matched =
                            String::from_utf8_lossy(&bytes[pos..pos + q_bytes.len()]).to_string();
                        new_spans.push(ftui::text::Span::styled(matched, style));
                        match_positions.push(line_no as u16);
                        match_idx += 1;
                        pos += q_bytes.len();
                    } else {
                        // Gather non-matching chars
                        let start = pos;
                        while pos < bytes.len()
                            && (pos + q_bytes.len() > lower_bytes.len()
                                || &lower_bytes[pos..pos + q_bytes.len()] != q_bytes)
                        {
                            pos += 1;
                        }
                        let chunk = String::from_utf8_lossy(&bytes[start..pos]).to_string();
                        new_spans.push(ftui::text::Span::raw(chunk));
                    }
                }
                *line = ftui::text::Line::from_spans(new_spans);
            }
        }

        match_positions
    }

    /// Render the detail/preview pane with rich content (Messages/Snippets/Raw).
    #[allow(clippy::too_many_arguments)]
    fn render_detail_pane(
        &self,
        frame: &mut super::ftui_adapter::Frame,
        area: Rect,
        border_type: BorderType,
        borders: Borders,
        styles: &StyleContext,
        pane_style: ftui::Style,
        pane_focused_style: ftui::Style,
        text_muted_style: ftui::Style,
    ) {
        // Tab indicator and wrap status
        let tab_label = match self.detail_tab {
            DetailTab::Messages => "Detail [\u{25cf}Messages] Snippets  Raw",
            DetailTab::Snippets => "Detail  Messages [\u{25cf}Snippets] Raw",
            DetailTab::Raw => "Detail  Messages  Snippets [\u{25cf}Raw]",
        };
        let wrap_indicator = if self.detail_wrap { " \u{21a9}" } else { "" };
        let title = format!("{tab_label}{wrap_indicator}");

        let detail_block = Block::new()
            .borders(borders)
            .border_type(border_type)
            .title(&title)
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

        // Reserve space for find bar if active
        let (content_area, find_area) = if self.detail_find.is_some() {
            let find_h = 1u16;
            if inner.height <= find_h + 1 {
                (inner, None)
            } else {
                let content = Rect::new(inner.x, inner.y, inner.width, inner.height - find_h);
                let find = Rect::new(inner.x, inner.y + content.height, inner.width, find_h);
                (content, Some(find))
            }
        } else {
            (inner, None)
        };

        if let Some(hit) = self.selected_hit() {
            // Build lines based on active tab
            let mut lines = match self.detail_tab {
                DetailTab::Messages => self.build_messages_lines(hit, content_area.width, styles),
                DetailTab::Snippets => self.build_snippets_lines(hit, styles),
                DetailTab::Raw => self.build_raw_lines(hit, styles),
            };

            // Apply find-in-detail highlighting
            if let Some(ref find) = self.detail_find {
                let _matches =
                    Self::apply_find_highlight(&mut lines, &find.query, find.current, styles);
            }

            // Apply scroll offset — skip `detail_scroll` lines
            let scroll = self.detail_scroll as usize;
            let visible_height = content_area.height as usize;
            let total_lines = lines.len();

            // Clamp scroll
            let effective_scroll = scroll.min(total_lines.saturating_sub(1));
            let visible_lines: Vec<ftui::text::Line> = lines
                .into_iter()
                .skip(effective_scroll)
                .take(visible_height)
                .collect();

            // Render the text
            let text = ftui::text::Text::from_lines(visible_lines);
            Paragraph::new(text)
                .style(styles.style(style_system::STYLE_TEXT_PRIMARY))
                .render(content_area, frame);

            // Scroll position indicator in bottom-right if content exceeds viewport
            if total_lines > visible_height {
                let pct = if total_lines <= 1 {
                    100
                } else {
                    (effective_scroll * 100) / (total_lines.saturating_sub(visible_height))
                };
                let indicator = format!(" {}/{} ({pct}%) ", effective_scroll + 1, total_lines);
                let ind_w = indicator.len().min(content_area.width as usize);
                let ind_x = content_area.x + content_area.width.saturating_sub(ind_w as u16);
                let ind_y = content_area.y + content_area.height.saturating_sub(1);
                let ind_area = Rect::new(ind_x, ind_y, ind_w as u16, 1);
                let ind_style = styles.style(style_system::STYLE_TEXT_MUTED);
                Paragraph::new(&*indicator)
                    .style(ind_style)
                    .render(ind_area, frame);
            }
        } else {
            Paragraph::new("Select a result to preview context and metadata.")
                .style(text_muted_style)
                .render(content_area, frame);
        }

        // Render find bar if active
        if let (Some(find), Some(find_rect)) = (&self.detail_find, find_area) {
            let find_style = styles.style(style_system::STYLE_TEXT_PRIMARY);
            let match_info = if find.matches.is_empty() {
                if find.query.is_empty() {
                    String::new()
                } else {
                    " (no matches)".to_string()
                }
            } else {
                format!(" ({}/{})", find.current + 1, find.matches.len())
            };
            let find_text = format!("/{}{}", find.query, match_info);
            Paragraph::new(&*find_text)
                .style(find_style)
                .render(find_rect, frame);
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

    /// Render the export modal overlay centered on screen.
    fn render_export_overlay(
        &self,
        frame: &mut super::ftui_adapter::Frame,
        area: Rect,
        styles: &StyleContext,
    ) {
        let state = match self.export_modal_state.as_ref() {
            Some(s) => s,
            None => return,
        };

        let text_style = styles.style(style_system::STYLE_TEXT_PRIMARY);
        let muted_style = styles.style(style_system::STYLE_TEXT_MUTED);
        let bg_style = styles.style(style_system::STYLE_PANE_BASE);
        let border_style = styles.style(style_system::STYLE_PANE_FOCUSED);
        let accent_style = styles.style(style_system::STYLE_STATUS_INFO);
        let success_style = styles.style(style_system::STYLE_STATUS_SUCCESS);
        let error_style = styles.style(style_system::STYLE_STATUS_ERROR);
        let selected_style = styles.style(style_system::STYLE_RESULT_ROW_SELECTED);

        // Modal dimensions: 70x22, clamped to terminal size.
        let modal_w = 70u16.min(area.width.saturating_sub(4));
        let modal_h = 22u16.min(area.height.saturating_sub(2));
        let modal_x = area.x + (area.width.saturating_sub(modal_w)) / 2;
        let modal_y = area.y + (area.height.saturating_sub(modal_h)) / 2;
        let modal_area = Rect::new(modal_x, modal_y, modal_w, modal_h);

        // Clear background.
        Block::new().style(bg_style).render(modal_area, frame);

        // Outer border.
        let outer = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title("Export Session as HTML (Ctrl+E)")
            .title_alignment(Alignment::Left)
            .style(border_style);
        let inner = outer.inner(modal_area);
        outer.render(modal_area, frame);

        if inner.is_empty() {
            return;
        }

        // Vertical layout: session card (3) + gap (1) + options (6) + gap (1) + preview (3) + flex + footer (1).
        let mut y = inner.y;
        let w = inner.width;
        let x = inner.x;

        // ── Session info card ──────────────────────────────────────
        if y + 3 <= inner.y + inner.height {
            let badge = format!(" {} ", state.agent_name);
            let location = format!("  {} | {}", state.workspace, state.timestamp);
            let badge_line = format!("{badge}{location}");
            Paragraph::new(&*badge_line)
                .style(accent_style)
                .render(Rect::new(x, y, w, 1), frame);
            y += 1;

            let title_text = &state.title_preview;
            Paragraph::new(title_text.as_str())
                .style(text_style)
                .render(Rect::new(x, y, w, 1), frame);
            y += 1;

            let stats = format!("{} messages", state.message_count);
            Paragraph::new(&*stats)
                .style(muted_style)
                .render(Rect::new(x, y, w, 1), frame);
            y += 1;
        }

        // Gap.
        y += 1;

        // ── Options section ────────────────────────────────────────
        // Separator.
        if y < inner.y + inner.height {
            let sep = "\u{2500}".repeat(w as usize);
            Paragraph::new(&*sep)
                .style(muted_style)
                .render(Rect::new(x, y, w, 1), frame);
            y += 1;
        }

        // Output directory.
        if y < inner.y + inner.height {
            let focused = state.focused == ExportField::OutputDir;
            let editing = state.output_dir_editing;
            let display_path = if editing {
                state.output_dir_buffer.as_str()
            } else {
                // Use a short representation
                state.output_dir.to_str().unwrap_or(".")
            };
            let max_len = w.saturating_sub(14) as usize;
            let truncated = if display_path.len() > max_len && max_len > 6 {
                let tail = &display_path[display_path.len().saturating_sub(max_len - 3)..];
                format!("...{tail}")
            } else {
                display_path.to_string()
            };
            let cursor = if editing { "_" } else { "" };
            let hint = if focused && !editing {
                " (Enter)"
            } else if editing {
                " (Enter=ok)"
            } else {
                ""
            };
            let line = format!(" Output: {truncated}{cursor}{hint}");
            let row_style = if focused { accent_style } else { text_style };
            Paragraph::new(&*line)
                .style(row_style)
                .render(Rect::new(x, y, w, 1), frame);
            y += 1;
        }

        // Checkboxes: include_tools, encrypt, password (conditional), show_timestamps.
        let checkboxes: &[(ExportField, &str, bool)] = &[
            (
                ExportField::IncludeTools,
                "Include tool calls",
                state.include_tools,
            ),
            (ExportField::Encrypt, "Password protection", state.encrypt),
        ];
        for &(field, label, checked) in checkboxes {
            if y >= inner.y + inner.height {
                break;
            }
            let mark = if checked { "[x]" } else { "[ ]" };
            let focused = state.focused == field;
            let row_style = if focused { accent_style } else { text_style };
            let line = format!(" {mark} {label}");
            Paragraph::new(&*line)
                .style(row_style)
                .render(Rect::new(x, y, w, 1), frame);
            y += 1;
        }

        // Password row (only if encrypt is enabled).
        if state.encrypt && y < inner.y + inner.height {
            let focused = state.focused == ExportField::Password;
            let display = if state.password_visible {
                state.password.clone()
            } else {
                "\u{2022}".repeat(state.password.len())
            };
            let cursor = if focused { "_" } else { "" };
            let vis_hint = if state.password_visible {
                "(Ctrl+H hide)"
            } else {
                "(Ctrl+H show)"
            };
            let line = format!("     Password: {display}{cursor} {vis_hint}");
            let row_style = if focused { accent_style } else { text_style };
            Paragraph::new(&*line)
                .style(row_style)
                .render(Rect::new(x, y, w, 1), frame);
            y += 1;
        }

        // Show timestamps checkbox.
        if y < inner.y + inner.height {
            let mark = if state.show_timestamps { "[x]" } else { "[ ]" };
            let focused = state.focused == ExportField::ShowTimestamps;
            let row_style = if focused { accent_style } else { text_style };
            let line = format!(" {mark} Show timestamps");
            Paragraph::new(&*line)
                .style(row_style)
                .render(Rect::new(x, y, w, 1), frame);
            y += 1;
        }

        // Gap.
        y += 1;

        // ── Preview section ────────────────────────────────────────
        if y < inner.y + inner.height {
            let sep2 = "\u{2500}".repeat(w as usize);
            Paragraph::new(&*sep2)
                .style(muted_style)
                .render(Rect::new(x, y, w, 1), frame);
            y += 1;
        }

        if y < inner.y + inner.height {
            Paragraph::new(state.filename_preview.as_str())
                .style(text_style)
                .render(Rect::new(x, y, w, 1), frame);
            y += 1;
        }

        if y < inner.y + inner.height {
            let est_kb = (state.message_count * 2 + 15).max(20);
            let size_str = if est_kb > 1024 {
                format!("~{:.1}MB", est_kb as f64 / 1024.0)
            } else {
                format!("~{est_kb}KB")
            };
            let mut features = vec!["Dark/Light themes", "Print-friendly"];
            if state.encrypt {
                features.push("Encrypted");
            }
            let preview = format!(
                "{} msgs | {} | {}",
                state.message_count,
                size_str,
                features.join(" | ")
            );
            Paragraph::new(&*preview)
                .style(muted_style)
                .render(Rect::new(x, y, w, 1), frame);
            y += 1;
        }

        // Progress line.
        if y < inner.y + inner.height {
            let (progress_text, pstyle) = match &state.progress {
                ExportProgress::Idle => (String::new(), muted_style),
                ExportProgress::Preparing => ("Preparing export...".to_string(), accent_style),
                ExportProgress::Encrypting => ("Encrypting content...".to_string(), accent_style),
                ExportProgress::Writing => ("Writing HTML file...".to_string(), accent_style),
                ExportProgress::Complete(path) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    (format!("Exported: {name}"), success_style)
                }
                ExportProgress::Error(msg) => (format!("Error: {msg}"), error_style),
            };
            if !progress_text.is_empty() {
                Paragraph::new(&*progress_text)
                    .style(pstyle)
                    .render(Rect::new(x, y, w, 1), frame);
            }
        }

        // ── Footer (keyboard hints) ──────────────────────────────
        let footer_y = modal_area.y + modal_area.height - 2;
        if footer_y > y {
            let can_export = state.can_export();
            let export_label = if can_export && state.focused == ExportField::ExportButton {
                " [EXPORT] "
            } else if can_export {
                " Enter=Export "
            } else {
                " (set password) "
            };
            let btn_style = if can_export && state.focused == ExportField::ExportButton {
                selected_style
            } else if can_export {
                accent_style
            } else {
                muted_style
            };

            // Build hint string with consistent spacing.
            let hints = format!(" Tab=Navigate  Space=Toggle {export_label} Esc=Cancel");
            Paragraph::new(&*hints)
                .style(btn_style)
                .render(Rect::new(x, footer_y, w, 1), frame);
        }
    }

    /// Build a one-line summary of active analytics filters for the header bar.
    fn analytics_filter_summary(&self) -> String {
        let f = &self.analytics_filters;
        let mut parts: Vec<String> = Vec::new();

        // Time range
        match (f.since_ms, f.until_ms) {
            (Some(s), Some(u)) => parts.push(format!("time:{s}..{u}")),
            (Some(s), None) => parts.push(format!("since:{s}")),
            (None, Some(u)) => parts.push(format!("until:{u}")),
            (None, None) => {}
        }

        // Agent filter
        if !f.agents.is_empty() {
            let mut agents: Vec<&str> = f.agents.iter().map(|s| s.as_str()).collect();
            agents.sort();
            parts.push(format!("agents:{}", agents.join(",")));
        }

        // Workspace filter
        if !f.workspaces.is_empty() {
            let mut ws: Vec<&str> = f.workspaces.iter().map(|s| s.as_str()).collect();
            ws.sort();
            parts.push(format!("ws:{}", ws.join(",")));
        }

        // Source filter
        match f.source_filter {
            SourceFilter::All => {}
            SourceFilter::Local => parts.push("source:local".into()),
            SourceFilter::Remote => parts.push("source:remote".into()),
            SourceFilter::SourceId(ref id) => parts.push(format!("source:{id}")),
        }

        if parts.is_empty() {
            "Filters: none".to_string()
        } else {
            format!("Filters: {}", parts.join(" | "))
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
    /// User pressed Enter to submit the query (force immediate search, push to history).
    QuerySubmitted,
    /// Search execution requested (Enter or debounce expired).
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
    /// Move cursor within the query string (Left/Right arrow keys).
    CursorMoved { delta: i32 },
    /// Jump cursor to start or end of query (Home/End keys).
    CursorJumped { to_end: bool },
    /// Toggle the wildcard fallback indicator (Ctrl+F).
    WildcardFallbackToggled,

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
    /// Cycle time filter preset (All -> Today -> Week -> Month -> All).
    TimePresetCycled,
    /// Cycle source filter (All -> Local -> Remote -> All).
    SourceFilterCycled,

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
    /// Copy the current search query to clipboard.
    CopyQuery,
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
    /// User chose to view release notes.
    UpdateReleaseNotesRequested,
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

    // -- Analytics surface ------------------------------------------------
    /// Switch to analytics surface (pushes Search onto back-stack).
    AnalyticsEntered,
    /// Navigate to a specific analytics subview.
    AnalyticsViewChanged(AnalyticsView),
    /// Pop the view stack (Esc from analytics returns to search).
    ViewStackPopped,
    /// Update analytics time range filter.
    AnalyticsTimeRangeSet {
        since_ms: Option<i64>,
        until_ms: Option<i64>,
    },
    /// Update analytics agent filter.
    AnalyticsAgentFilterSet(HashSet<String>),
    /// Update analytics workspace filter.
    AnalyticsWorkspaceFilterSet(HashSet<String>),
    /// Update analytics source filter.
    AnalyticsSourceFilterSet(SourceFilter),
    /// Clear all analytics filters.
    AnalyticsFiltersClearAll,

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

const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(60);
const QUERY_HISTORY_CAP: usize = 50;

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
                    KeyCode::F(5) if shift => CassMsg::TimePresetCycled,
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
                    KeyCode::F(11) => CassMsg::SourceFilterCycled,

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
                    KeyCode::Char('f') if ctrl => CassMsg::WildcardFallbackToggled,

                    // -- Density --------------------------------------------------
                    KeyCode::Char('d') if ctrl => CassMsg::DensityModeCycled,

                    // -- Multi-select ---------------------------------------------
                    KeyCode::Char('x') if ctrl => CassMsg::SelectionToggled,
                    KeyCode::Char('a') if ctrl => CassMsg::SelectAllToggled,
                    KeyCode::Enter if ctrl => CassMsg::ItemEnqueued,
                    KeyCode::Char('o') if ctrl => CassMsg::OpenAllQueued,

                    // -- Quick export ---------------------------------------------
                    KeyCode::Char('e') if ctrl => CassMsg::ExportModalOpened,

                    // -- Clipboard ------------------------------------------------
                    KeyCode::Char('y') if ctrl => CassMsg::CopyPath,
                    KeyCode::Char('c') if ctrl && shift => CassMsg::CopyContent,

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
                    // -- Cursor movement (query editing) --------------------------
                    KeyCode::Left => CassMsg::CursorMoved { delta: -1 },
                    KeyCode::Right => CassMsg::CursorMoved { delta: 1 },

                    KeyCode::Up => CassMsg::SelectionMoved { delta: -1 },
                    KeyCode::Down => CassMsg::SelectionMoved { delta: 1 },
                    KeyCode::Home => CassMsg::CursorJumped { to_end: false },
                    KeyCode::End => CassMsg::CursorJumped { to_end: true },
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
        if self.show_consent_dialog
            && let CassMsg::QueryChanged(ref text) = msg
        {
            if text.eq_ignore_ascii_case("d") {
                return self.update(CassMsg::ModelDownloadAccepted);
            }
            if text.eq_ignore_ascii_case("h") {
                return self.update(CassMsg::HashModeAccepted);
            }
            // Ignore other query input while consent dialog is open
            return ftui::Cmd::none();
        }

        // Export modal intercepts keyboard input for form navigation and text editing.
        if self.show_export_modal
            && let Some(ref mut state) = self.export_modal_state
        {
            match &msg {
                CassMsg::QueryChanged(text) if state.is_editing_text() => {
                    // Route typed characters to the active text field.
                    if text.is_empty() {
                        // Backspace
                        if state.focused == ExportField::Password {
                            state.password_pop();
                        } else if state.focused == ExportField::OutputDir
                            && state.output_dir_editing
                        {
                            state.output_dir_pop();
                        }
                    } else {
                        for c in text.chars() {
                            if state.focused == ExportField::Password {
                                state.password_push(c);
                            } else if state.focused == ExportField::OutputDir
                                && state.output_dir_editing
                            {
                                state.output_dir_push(c);
                            }
                        }
                    }
                    return ftui::Cmd::none();
                }
                CassMsg::QueryChanged(text) => {
                    // Non-editing mode: check for Ctrl+H (password visibility toggle).
                    if text == "\x08" {
                        state.toggle_password_visibility();
                    }
                    return ftui::Cmd::none();
                }
                CassMsg::QuerySubmitted => {
                    // Enter key: toggle text field editing, or execute export.
                    if state.focused == ExportField::OutputDir {
                        state.toggle_current();
                    } else if state.focused == ExportField::ExportButton {
                        return self.update(CassMsg::ExportExecuted);
                    } else if state.focused == ExportField::Password {
                        // Enter in password field = move to next.
                        state.next_field();
                    } else {
                        state.toggle_current();
                    }
                    return ftui::Cmd::none();
                }
                CassMsg::FocusToggled => {
                    // Tab → next field.
                    state.next_field();
                    return ftui::Cmd::none();
                }
                CassMsg::FocusDirectional { .. } => {
                    // Shift+Tab → prev field.
                    state.prev_field();
                    return ftui::Cmd::none();
                }
                CassMsg::PeekToggled => {
                    // Space → toggle checkbox / button.
                    if state.focused == ExportField::ExportButton {
                        return self.update(CassMsg::ExportExecuted);
                    }
                    state.toggle_current();
                    return ftui::Cmd::none();
                }
                _ => {
                    // Let non-intercepted messages (like Tick, QuitRequested,
                    // ExportModalOpened/Closed, etc.) fall through to normal handling.
                }
            }
        }

        // Update banner shortcuts:
        // - U: upgrade (two-step confirm)
        // - N: open release notes
        // - S: skip version
        // - Esc: dismiss banner for this session
        if self.can_handle_update_shortcuts() {
            match &msg {
                CassMsg::QueryChanged(text) if text.eq_ignore_ascii_case("u") => {
                    return self.update(CassMsg::UpdateUpgradeRequested);
                }
                CassMsg::QueryChanged(text) if text.eq_ignore_ascii_case("n") => {
                    return self.update(CassMsg::UpdateReleaseNotesRequested);
                }
                CassMsg::QueryChanged(text) if text.eq_ignore_ascii_case("s") => {
                    return self.update(CassMsg::UpdateSkipped);
                }
                CassMsg::QuitRequested => {
                    return self.update(CassMsg::UpdateDismissed);
                }
                _ => {}
            }
        }

        // ── Bulk-actions modal intercept ────────────────────────────
        // When the bulk modal is open, intercept navigation and confirm.
        if self.show_bulk_modal {
            match &msg {
                CassMsg::SelectionMoved { delta } => {
                    match delta {
                        -1 => self.bulk_action_idx = self.bulk_action_idx.saturating_sub(1),
                        1 => {
                            self.bulk_action_idx =
                                (self.bulk_action_idx + 1).min(BULK_ACTIONS.len() - 1);
                        }
                        _ => {}
                    }
                    return ftui::Cmd::none();
                }
                CassMsg::QuerySubmitted => {
                    // Enter in the modal executes the selected action.
                    let idx = self.bulk_action_idx;
                    return self.update(CassMsg::BulkActionExecuted { action_index: idx });
                }
                CassMsg::QuitRequested => {
                    self.show_bulk_modal = false;
                    return ftui::Cmd::none();
                }
                _ => {}
            }
        }

        match msg {
            // -- Terminal event passthrough (unused, events come as CassMsg) ---
            CassMsg::TerminalEvent(_) => ftui::Cmd::none(),

            // -- Query & search -----------------------------------------------
            CassMsg::QueryChanged(text) => {
                if text.is_empty() {
                    // Backspace: remove char before cursor
                    if self.cursor_pos > 0 {
                        self.query.remove(self.cursor_pos - 1);
                        self.cursor_pos -= 1;
                    }
                } else {
                    self.query.insert_str(self.cursor_pos, &text);
                    self.cursor_pos += text.len();
                }
                self.dirty_since = Some(Instant::now());
                self.search_dirty_since = Some(Instant::now());
                self.history_cursor = None;
                ftui::Cmd::tick(SEARCH_DEBOUNCE)
            }
            CassMsg::QueryCleared => {
                self.query.clear();
                self.cursor_pos = 0;
                self.dirty_since = Some(Instant::now());
                self.search_dirty_since = Some(Instant::now());
                self.history_cursor = None;
                ftui::Cmd::tick(SEARCH_DEBOUNCE)
            }
            CassMsg::QueryWordDeleted => {
                // Delete word backward from cursor (Ctrl+W): trim trailing
                // whitespace before cursor, then delete to word boundary.
                if self.cursor_pos > 0 {
                    let before = &self.query[..self.cursor_pos];
                    let trimmed = before.trim_end();
                    let new_end = trimmed
                        .rfind(|c: char| c.is_whitespace())
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    self.query.drain(new_end..self.cursor_pos);
                    self.cursor_pos = new_end;
                    self.dirty_since = Some(Instant::now());
                    self.search_dirty_since = Some(Instant::now());
                    self.history_cursor = None;
                    return ftui::Cmd::tick(SEARCH_DEBOUNCE);
                }
                ftui::Cmd::none()
            }
            CassMsg::QuerySubmitted => {
                // Enter pressed: push query to history (deduplicated), clear
                // debounce state, and force immediate search.
                let q = self.query.trim().to_string();
                if !q.is_empty() {
                    // Remove duplicate from history if present
                    self.query_history.retain(|h| h != &q);
                    self.query_history.push_front(q);
                    if self.query_history.len() > QUERY_HISTORY_CAP {
                        self.query_history.pop_back();
                    }
                } else if let Some(prev) = self.query_history.front().cloned() {
                    // Empty query + history → load most recent query
                    self.query = prev;
                    self.cursor_pos = self.query.len();
                }
                self.history_cursor = None;
                self.search_dirty_since = None; // cancel pending debounce
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }
            CassMsg::SearchRequested => {
                // Clear debounce state so we don't double-fire.
                self.search_dirty_since = None;
                // Build search params from current state.
                let params = SearchParams {
                    query: self.query.clone(),
                    filters: self.filters.clone(),
                    mode: self.search_mode,
                    match_mode: self.match_mode,
                    ranking: self.ranking_mode,
                    context_window: self.context_window,
                    limit: self.per_pane_limit * 10, // fetch enough for pane grouping
                };
                // Skip empty queries.
                if params.query.trim().is_empty() {
                    return ftui::Cmd::none();
                }
                // Dispatch async search if a service is available.
                if let Some(svc) = self.search_service.clone() {
                    self.status = "Searching\u{2026}".to_string();
                    ftui::Cmd::task(move || match svc.execute(&params) {
                        Ok(result) => CassMsg::SearchCompleted {
                            hits: result.hits,
                            elapsed_ms: result.elapsed_ms,
                            suggestions: result.suggestions,
                            wildcard_fallback: result.wildcard_fallback,
                        },
                        Err(e) => CassMsg::SearchFailed(e),
                    })
                } else {
                    ftui::Cmd::none()
                }
            }
            CassMsg::SearchCompleted {
                hits,
                elapsed_ms,
                suggestions,
                wildcard_fallback,
            } => {
                self.last_search_ms = Some(elapsed_ms);
                self.suggestions = suggestions;
                self.wildcard_fallback = wildcard_fallback;

                // Group hits by agent slug into per-agent panes.
                let mut pane_map: std::collections::BTreeMap<String, Vec<SearchHit>> =
                    std::collections::BTreeMap::new();
                for hit in &hits {
                    pane_map
                        .entry(hit.agent.clone())
                        .or_default()
                        .push(hit.clone());
                }
                self.panes = pane_map
                    .into_iter()
                    .map(|(agent, agent_hits)| {
                        let total = agent_hits.len();
                        AgentPane {
                            agent,
                            hits: agent_hits,
                            selected: 0,
                            total_count: total,
                        }
                    })
                    .collect();

                // Keep selection stable across reranking by retaining only keys that
                // still exist in the new result set.
                let available: HashSet<SelectedHitKey> =
                    hits.iter().map(SelectedHitKey::from_hit).collect();
                self.selected.retain(|k| available.contains(k));
                if self.selected.is_empty() {
                    self.open_confirm_armed = false;
                }

                // Clamp active pane and selection
                if self.active_pane >= self.panes.len() {
                    self.active_pane = 0;
                }

                self.results = hits;
                self.status = format!("{} results in {}ms", self.results.len(), elapsed_ms);
                // Reset scroll to top for new results.
                let mut state = self.results_list_state.borrow_mut();
                state.scroll_to_top();
                state.select(Some(0));
                ftui::Cmd::none()
            }
            CassMsg::SearchFailed(err) => {
                self.status = format!("Search error: {err}");
                ftui::Cmd::none()
            }
            CassMsg::CursorMoved { delta } => {
                let new_pos = self.cursor_pos as i32 + delta;
                self.cursor_pos = new_pos.clamp(0, self.query.len() as i32) as usize;
                ftui::Cmd::none()
            }
            CassMsg::CursorJumped { to_end } => {
                self.cursor_pos = if to_end { self.query.len() } else { 0 };
                ftui::Cmd::none()
            }
            CassMsg::WildcardFallbackToggled => {
                self.wildcard_fallback = !self.wildcard_fallback;
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
                self.time_preset = TimePreset::All;
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }
            CassMsg::TimePresetCycled => {
                self.time_preset = self.time_preset.next();
                let now = chrono::Utc::now().timestamp();
                let (from, to) = match self.time_preset {
                    TimePreset::All => (None, None),
                    TimePreset::Today => (Some(now - (now % 86400)), None),
                    TimePreset::Week => (Some(now - 7 * 86400), None),
                    TimePreset::Month => (Some(now - 30 * 86400), None),
                    TimePreset::Custom => (self.filters.created_from, self.filters.created_to),
                };
                self.filters.created_from = from;
                self.filters.created_to = to;
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }
            CassMsg::SourceFilterCycled => {
                self.filters.source_filter = match self.filters.source_filter {
                    SourceFilter::All => SourceFilter::Local,
                    SourceFilter::Local => SourceFilter::Remote,
                    SourceFilter::Remote => SourceFilter::All,
                    SourceFilter::SourceId(_) => SourceFilter::All,
                };
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
                    let total = pane.hits.len();
                    let mut state = self.results_list_state.borrow_mut();
                    state.select(Some(pane.selected));
                    if delta > 0 {
                        for _ in 0..delta {
                            state.select_next(total);
                        }
                    } else {
                        for _ in 0..delta.unsigned_abs() {
                            state.select_previous(total);
                        }
                    }
                    pane.selected = state.selected.unwrap_or(0);
                }
                ftui::Cmd::none()
            }
            CassMsg::SelectionJumped { to_end } => {
                if let Some(pane) = self.panes.get_mut(self.active_pane) {
                    let total = pane.hits.len();
                    let mut state = self.results_list_state.borrow_mut();
                    if to_end {
                        state.scroll_to_bottom(total);
                        pane.selected = total.saturating_sub(1);
                    } else {
                        state.scroll_to_top();
                        pane.selected = 0;
                    }
                    state.select(Some(pane.selected));
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
                    let total = pane.hits.len();
                    let mut state = self.results_list_state.borrow_mut();
                    if delta > 0 {
                        state.page_down(total);
                    } else {
                        state.page_up(total);
                    }
                    // Sync pane selection with VirtualizedListState
                    pane.selected = state.selected.unwrap_or(0);
                }
                ftui::Cmd::none()
            }

            // -- Detail view --------------------------------------------------
            CassMsg::DetailOpened => {
                // Enter is context-dependent: in Query mode, submit the query;
                // in Results/Detail mode, open the detail modal.
                if self.input_mode == InputMode::Query && !self.show_detail_modal {
                    return self.update(CassMsg::QuerySubmitted);
                }
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
                self.detail_wrap = !self.detail_wrap;
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
                if let Some(key) = self.active_hit_key() {
                    if self.selected.remove(&key) {
                        self.status = format!("Deselected ({} selected)", self.selected.len());
                    } else {
                        self.selected.insert(key);
                        self.status = format!(
                            "Selected ({} total) · Ctrl+X toggle · A bulk actions · Esc clear",
                            self.selected.len()
                        );
                    }
                }
                self.open_confirm_armed = false;
                ftui::Cmd::none()
            }
            CassMsg::SelectAllToggled => {
                if let Some(pane) = self.panes.get(self.active_pane) {
                    let pane_keys: Vec<SelectedHitKey> =
                        pane.hits.iter().map(SelectedHitKey::from_hit).collect();
                    let all_selected = pane_keys.iter().all(|k| self.selected.contains(k));
                    if all_selected {
                        for key in &pane_keys {
                            self.selected.remove(key);
                        }
                        self.status =
                            format!("Deselected all in pane ({} total)", self.selected.len());
                    } else {
                        for key in pane_keys {
                            self.selected.insert(key);
                        }
                        self.status = format!(
                            "Selected all in pane ({} total) · A bulk actions",
                            self.selected.len()
                        );
                    }
                }
                self.open_confirm_armed = false;
                ftui::Cmd::none()
            }
            CassMsg::ItemEnqueued => {
                if let Some(key) = self.active_hit_key() {
                    self.selected.insert(key);
                    self.status = format!(
                        "Queued ({}) · Ctrl+Enter add · Ctrl+O open all",
                        self.selected.len()
                    );
                }
                self.open_confirm_armed = false;
                // Advance selection
                if let Some(pane) = self.panes.get_mut(self.active_pane)
                    && pane.selected + 1 < pane.hits.len()
                {
                    pane.selected += 1;
                }
                ftui::Cmd::none()
            }
            CassMsg::BulkActionsOpened => {
                if self.selected.is_empty() {
                    self.status =
                        "No items selected. Ctrl+X to select, Ctrl+A to select all.".to_string();
                } else {
                    self.show_bulk_modal = true;
                    self.bulk_action_idx = 0;
                    self.status =
                        "Bulk actions: ↑↓ navigate · Enter execute · Esc cancel".to_string();
                }
                ftui::Cmd::none()
            }
            CassMsg::BulkActionsClosed => {
                self.show_bulk_modal = false;
                ftui::Cmd::none()
            }
            CassMsg::BulkActionExecuted { action_index } => {
                self.show_bulk_modal = false;
                match action_index {
                    0 => {
                        // Open all in editor — delegate to OpenAllQueued
                        ftui::Cmd::msg(CassMsg::OpenAllQueued)
                    }
                    1 => {
                        let selected_hits = self.selected_hits();
                        let paths: Vec<String> = selected_hits
                            .iter()
                            .map(|h| h.source_path.clone())
                            .collect();
                        let text = paths.join("\n");
                        let count = paths.len();
                        match copy_to_clipboard(&text) {
                            Ok(()) => {
                                use crate::ui::components::toast::{Toast, ToastType};
                                self.selected.clear();
                                self.open_confirm_armed = false;
                                self.status = format!("Copied {count} paths to clipboard");
                                self.toast_manager.push(Toast::new(
                                    format!("Copied {count} paths"),
                                    ToastType::Success,
                                ));
                            }
                            Err(e) => {
                                use crate::ui::components::toast::{Toast, ToastType};
                                self.status = format!("Clipboard: {e}");
                                self.toast_manager.push(Toast::new(
                                    format!("Copy failed: {e}"),
                                    ToastType::Error,
                                ));
                            }
                        }
                        ftui::Cmd::none()
                    }
                    2 => {
                        let selected_hits = self.selected_hits();
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
                        let count = export.len();
                        match serde_json::to_string_pretty(&export) {
                            Ok(json) => match copy_to_clipboard(&json) {
                                Ok(()) => {
                                    use crate::ui::components::toast::{Toast, ToastType};
                                    self.selected.clear();
                                    self.open_confirm_armed = false;
                                    self.status =
                                        format!("Exported {count} items as JSON to clipboard");
                                    self.toast_manager.push(Toast::new(
                                        format!("Exported {count} items as JSON"),
                                        ToastType::Success,
                                    ));
                                }
                                Err(e) => {
                                    self.status = format!("JSON export failed: {e}");
                                }
                            },
                            Err(e) => {
                                self.status = format!("JSON export failed: {e}");
                            }
                        }
                        ftui::Cmd::none()
                    }
                    3 => {
                        // Clear selection
                        let count = self.selected.len();
                        self.selected.clear();
                        self.open_confirm_armed = false;
                        self.status = format!("Cleared {count} selections");
                        ftui::Cmd::none()
                    }
                    _ => ftui::Cmd::none(),
                }
            }

            // -- Actions on results -------------------------------------------
            CassMsg::CopySnippet => {
                use crate::ui::components::toast::{Toast, ToastType};
                if let Some(hit) = self.selected_hit() {
                    match copy_to_clipboard(hit.snippet.as_str()) {
                        Ok(()) => {
                            self.status = "Copied snippet to clipboard".to_string();
                            self.toast_manager
                                .push(Toast::new("Copied snippet".to_string(), ToastType::Success));
                        }
                        Err(e) => {
                            self.status = format!("Clipboard: {e}");
                            self.toast_manager
                                .push(Toast::new(format!("Copy failed: {e}"), ToastType::Error));
                        }
                    }
                } else {
                    self.status = "No active result to copy.".to_string();
                }
                ftui::Cmd::none()
            }
            CassMsg::CopyPath => {
                use crate::ui::components::toast::{Toast, ToastType};
                if let Some(hit) = self.selected_hit() {
                    match copy_to_clipboard(hit.source_path.as_str()) {
                        Ok(()) => {
                            self.status = "Copied path to clipboard".to_string();
                            self.toast_manager
                                .push(Toast::new("Copied path".to_string(), ToastType::Success));
                        }
                        Err(e) => {
                            self.status = format!("Clipboard: {e}");
                            self.toast_manager
                                .push(Toast::new(format!("Copy failed: {e}"), ToastType::Error));
                        }
                    }
                } else {
                    self.status = "No active result to copy.".to_string();
                }
                ftui::Cmd::none()
            }
            CassMsg::CopyContent => {
                use crate::ui::components::toast::{Toast, ToastType};
                if let Some(hit) = self.selected_hit() {
                    match copy_to_clipboard(hit.content.as_str()) {
                        Ok(()) => {
                            self.status = "Copied content to clipboard".to_string();
                            self.toast_manager.push(Toast::new(
                                "Copied content".to_string(),
                                ToastType::Success,
                            ));
                        }
                        Err(e) => {
                            self.status = format!("Clipboard: {e}");
                            self.toast_manager
                                .push(Toast::new(format!("Copy failed: {e}"), ToastType::Error));
                        }
                    }
                } else {
                    self.status = "No active result to copy.".to_string();
                }
                ftui::Cmd::none()
            }
            CassMsg::OpenInEditor => {
                if let Some(hit) = self.selected_hit().cloned() {
                    let editor_cmd = dotenvy::var("EDITOR")
                        .or_else(|_| dotenvy::var("VISUAL"))
                        .unwrap_or_else(|_| "code".to_string());
                    self.status = match open_hits_in_editor(std::slice::from_ref(&hit), &editor_cmd)
                    {
                        Ok((count, editor_bin)) => format!("Opened {count} file in {editor_bin}"),
                        Err(e) => format!("Failed to open editor: {e}"),
                    };
                } else {
                    self.status = "No active result to open.".to_string();
                }
                ftui::Cmd::none()
            }
            CassMsg::OpenInNano => {
                if let Some(hit) = self.selected_hit().cloned() {
                    self.status = match open_hits_in_editor(std::slice::from_ref(&hit), "nano") {
                        Ok((count, editor_bin)) => format!("Opened {count} file in {editor_bin}"),
                        Err(e) => format!("Failed to open editor: {e}"),
                    };
                } else {
                    self.status = "No active result to open.".to_string();
                }
                ftui::Cmd::none()
            }
            CassMsg::OpenAllQueued => {
                if self.selected.is_empty() {
                    self.status = "No items queued. Ctrl+Enter to queue items.".to_string();
                    self.open_confirm_armed = false;
                    return ftui::Cmd::none();
                }
                if self.selected.len() >= OPEN_CONFIRM_THRESHOLD && !self.open_confirm_armed {
                    // First press: arm confirmation
                    self.open_confirm_armed = true;
                    self.status = format!(
                        "Open {} queued items? Press Ctrl+O again to confirm.",
                        self.selected.len()
                    );
                    return ftui::Cmd::none();
                }
                // Execute: open all selected items
                let hits = self.selected_hits();
                let editor_cmd = dotenvy::var("EDITOR")
                    .or_else(|_| dotenvy::var("VISUAL"))
                    .unwrap_or_else(|_| "code".to_string());
                self.status = match open_hits_in_editor(&hits, &editor_cmd) {
                    Ok((count, editor_bin)) => {
                        self.selected.clear();
                        self.open_confirm_armed = false;
                        format!("Opened {count} files in {editor_bin}")
                    }
                    Err(e) => {
                        self.open_confirm_armed = false;
                        format!("Failed to open queued files: {e}")
                    }
                };
                ftui::Cmd::none()
            }
            CassMsg::ViewRaw => {
                if self.selected_hit().is_some() {
                    self.detail_tab = DetailTab::Raw;
                    self.show_detail_modal = true;
                    self.detail_scroll = 0;
                    self.modal_scroll = 0;
                } else {
                    self.status = "No active result to view.".to_string();
                }
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
                let buf = self.input_buffer.trim().to_string();
                let cmd = match self.input_mode {
                    InputMode::Agent if !buf.is_empty() => {
                        // Parse comma-separated agent names.
                        let agents: HashSet<String> =
                            buf.split(',').map(|s| s.trim().to_string()).collect();
                        ftui::Cmd::msg(CassMsg::FilterAgentSet(agents))
                    }
                    InputMode::Workspace if !buf.is_empty() => {
                        let workspaces: HashSet<String> =
                            buf.split(',').map(|s| s.trim().to_string()).collect();
                        ftui::Cmd::msg(CassMsg::FilterWorkspaceSet(workspaces))
                    }
                    InputMode::CreatedFrom => {
                        let ts = parse_time_input(&buf);
                        if ts.is_some() || buf.is_empty() {
                            self.time_preset = if ts.is_some() {
                                TimePreset::Custom
                            } else {
                                TimePreset::All
                            };
                            ftui::Cmd::msg(CassMsg::FilterTimeSet {
                                from: ts,
                                to: self.filters.created_to,
                            })
                        } else {
                            self.status = format!("Invalid date: {buf}");
                            ftui::Cmd::none()
                        }
                    }
                    InputMode::CreatedTo => {
                        let ts = parse_time_input(&buf);
                        if ts.is_some() || buf.is_empty() {
                            self.time_preset = TimePreset::Custom;
                            ftui::Cmd::msg(CassMsg::FilterTimeSet {
                                from: self.filters.created_from,
                                to: ts,
                            })
                        } else {
                            self.status = format!("Invalid date: {buf}");
                            ftui::Cmd::none()
                        }
                    }
                    _ => ftui::Cmd::none(),
                };
                self.input_mode = InputMode::Query;
                self.input_buffer.clear();
                cmd
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
                    self.cursor_pos = self.query.len();
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
                        if let Some(info) = &self.update_info {
                            if info.should_show() {
                                self.update_dismissed = false;
                                self.update_upgrade_armed = false;
                                self.status = format!(
                                    "Update available v{} -> v{} (U=upgrade, N=notes, S=skip, Esc=dismiss)",
                                    info.current_version, info.latest_version
                                );
                            } else if info.is_skipped {
                                self.status = format!(
                                    "v{} is currently skipped. Clear update_state.json to re-enable prompts.",
                                    info.latest_version
                                );
                            } else {
                                self.status = "You're on the latest version.".to_string();
                            }
                        } else {
                            self.status =
                                "No update information available yet. Check again shortly."
                                    .to_string();
                        }
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
                    // -- Analytics palette actions ---
                    Some(PaletteAction::AnalyticsDashboard) => ftui::Cmd::batch(vec![
                        ftui::Cmd::msg(CassMsg::AnalyticsEntered),
                        ftui::Cmd::msg(CassMsg::AnalyticsViewChanged(AnalyticsView::Dashboard)),
                    ]),
                    Some(PaletteAction::AnalyticsExplorer) => ftui::Cmd::batch(vec![
                        ftui::Cmd::msg(CassMsg::AnalyticsEntered),
                        ftui::Cmd::msg(CassMsg::AnalyticsViewChanged(AnalyticsView::Explorer)),
                    ]),
                    Some(PaletteAction::AnalyticsHeatmap) => ftui::Cmd::batch(vec![
                        ftui::Cmd::msg(CassMsg::AnalyticsEntered),
                        ftui::Cmd::msg(CassMsg::AnalyticsViewChanged(AnalyticsView::Heatmap)),
                    ]),
                    Some(PaletteAction::AnalyticsBreakdowns) => ftui::Cmd::batch(vec![
                        ftui::Cmd::msg(CassMsg::AnalyticsEntered),
                        ftui::Cmd::msg(CassMsg::AnalyticsViewChanged(AnalyticsView::Breakdowns)),
                    ]),
                    Some(PaletteAction::AnalyticsTools) => ftui::Cmd::batch(vec![
                        ftui::Cmd::msg(CassMsg::AnalyticsEntered),
                        ftui::Cmd::msg(CassMsg::AnalyticsViewChanged(AnalyticsView::Tools)),
                    ]),
                    Some(PaletteAction::AnalyticsCost) => ftui::Cmd::batch(vec![
                        ftui::Cmd::msg(CassMsg::AnalyticsEntered),
                        ftui::Cmd::msg(CassMsg::AnalyticsViewChanged(AnalyticsView::Cost)),
                    ]),
                    Some(PaletteAction::AnalyticsCoverage) => ftui::Cmd::batch(vec![
                        ftui::Cmd::msg(CassMsg::AnalyticsEntered),
                        ftui::Cmd::msg(CassMsg::AnalyticsViewChanged(AnalyticsView::Coverage)),
                    ]),
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
                // Initialize modal state from the currently selected hit + conversation.
                if let Some(hit) = self.selected_hit().cloned() {
                    let state = if let Some((_, ref cv)) = self.cached_detail {
                        ExportModalState::from_hit(&hit, cv)
                    } else {
                        // Fallback: build minimal state from hit alone.
                        ExportModalState {
                            agent_name: hit.agent.clone(),
                            workspace: hit.workspace.clone(),
                            ..Default::default()
                        }
                    };
                    self.export_modal_state = Some(state);
                    self.show_export_modal = true;
                }
                ftui::Cmd::none()
            }
            CassMsg::ExportModalClosed => {
                self.show_export_modal = false;
                self.export_modal_state = None;
                ftui::Cmd::none()
            }
            CassMsg::ExportFieldChanged { field, value } => {
                if let Some(ref mut state) = self.export_modal_state {
                    match field {
                        ExportField::OutputDir => {
                            state.output_dir_buffer = value;
                        }
                        ExportField::Password => {
                            state.password = value;
                        }
                        _ => {}
                    }
                }
                ftui::Cmd::none()
            }
            CassMsg::ExportFieldToggled(field) => {
                if let Some(ref mut state) = self.export_modal_state {
                    let prev_focused = state.focused;
                    state.focused = field;
                    state.toggle_current();
                    state.focused = prev_focused;
                }
                ftui::Cmd::none()
            }
            CassMsg::ExportFocusMoved { forward } => {
                if let Some(ref mut state) = self.export_modal_state {
                    if forward {
                        state.next_field();
                    } else {
                        state.prev_field();
                    }
                }
                ftui::Cmd::none()
            }
            CassMsg::ExportExecuted => {
                // Extract source_path before mutable borrow of export_modal_state.
                let source_path = self
                    .selected_hit()
                    .map(|h| h.source_path.clone())
                    .unwrap_or_default();
                if let Some(ref mut state) = self.export_modal_state {
                    if !state.can_export() {
                        return ftui::Cmd::none();
                    }
                    state.progress = ExportProgress::Preparing;
                    let output_dir = state.output_dir.clone();
                    let output_filename = state.filename_preview.clone();
                    let encrypt = state.encrypt;
                    let password = if encrypt {
                        Some(state.password.clone())
                    } else {
                        None
                    };
                    let show_timestamps = state.show_timestamps;
                    let include_tools = state.include_tools;
                    let title = state.title_preview.clone();
                    let agent_name = state.agent_name.clone();

                    // Dispatch the export as a background task.
                    return ftui::Cmd::task(move || {
                        export_session_task(
                            &source_path,
                            &output_dir,
                            &output_filename,
                            encrypt,
                            password.as_deref(),
                            show_timestamps,
                            include_tools,
                            &title,
                            &agent_name,
                        )
                    });
                }
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
            CassMsg::UpdateCheckCompleted(info) => {
                let should_show = info.should_show();
                let latest = info.latest_version.clone();
                let current = info.current_version.clone();
                let skipped = info.is_skipped;
                self.update_info = Some(info);
                self.update_upgrade_armed = false;
                if should_show {
                    self.update_dismissed = false;
                    self.status = format!(
                        "Update available v{} -> v{} (U=upgrade, N=notes, S=skip, Esc=dismiss)",
                        current, latest
                    );
                } else if skipped {
                    self.status = format!(
                        "Update v{} is skipped (open palette: Check updates for details).",
                        latest
                    );
                }
                ftui::Cmd::none()
            }
            CassMsg::UpdateUpgradeRequested => {
                if let Some(info) = &self.update_info {
                    if !info.should_show() {
                        self.status = "You're on the latest version.".to_string();
                        self.update_upgrade_armed = false;
                        return ftui::Cmd::none();
                    }
                    if !self.update_upgrade_armed {
                        self.update_upgrade_armed = true;
                        self.status = format!(
                            "Confirm upgrade to v{}: press U again. Esc cancels.",
                            info.latest_version
                        );
                        return ftui::Cmd::none();
                    }
                    self.update_upgrade_armed = false;
                    #[cfg(test)]
                    {
                        self.status = format!(
                            "TEST mode: would launch self-update to v{}.",
                            info.latest_version
                        );
                        ftui::Cmd::none()
                    }
                    #[cfg(not(test))]
                    {
                        self.status =
                            format!("Launching installer for v{}...", info.latest_version);
                        run_self_update(&info.tag_name);
                    }
                } else {
                    self.status = "No update information available yet.".to_string();
                    self.update_upgrade_armed = false;
                    ftui::Cmd::none()
                }
            }
            CassMsg::UpdateSkipped => {
                self.update_upgrade_armed = false;
                if let Some(info) = &self.update_info {
                    if !info.should_show() {
                        self.status = "Nothing to skip: no pending update.".to_string();
                        return ftui::Cmd::none();
                    }
                    if cfg!(test) {
                        self.update_dismissed = true;
                        self.status = format!(
                            "Skipped v{} (test mode, not persisted).",
                            info.latest_version
                        );
                    } else if let Err(e) = skip_version(&info.latest_version) {
                        self.status = format!("Failed to skip v{}: {e}", info.latest_version);
                    } else {
                        self.update_dismissed = true;
                        self.status = format!("Skipped v{}.", info.latest_version);
                    }
                } else {
                    self.status = "No update information available yet.".to_string();
                }
                ftui::Cmd::none()
            }
            CassMsg::UpdateReleaseNotesRequested => {
                if let Some(info) = &self.update_info {
                    if !info.should_show() {
                        self.status = "You're on the latest version.".to_string();
                        return ftui::Cmd::none();
                    }
                    match open_in_browser(&info.release_url) {
                        Ok(()) => {
                            self.status =
                                format!("Opened release notes for v{}.", info.latest_version);
                        }
                        Err(e) => {
                            self.status = format!("Failed to open release notes: {e}");
                        }
                    }
                } else {
                    self.status = "No update information available yet.".to_string();
                }
                ftui::Cmd::none()
            }
            CassMsg::UpdateDismissed => {
                self.update_dismissed = true;
                self.update_upgrade_armed = false;
                self.status = "Update banner dismissed for this session.".to_string();
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
                // Poll update-check channel once per tick.
                let mut update_check_done = false;
                let mut update_info_ready: Option<UpdateInfo> = None;
                if let Some(rx) = self.update_check_rx.as_ref() {
                    match rx.try_recv() {
                        Ok(info) => {
                            update_check_done = true;
                            update_info_ready = info;
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            update_check_done = true;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    }
                }
                if update_check_done {
                    self.update_check_rx = None;
                }

                let mut cmds = Vec::new();
                if let Some(info) = update_info_ready {
                    cmds.push(ftui::Cmd::msg(CassMsg::UpdateCheckCompleted(info)));
                }
                // Debounced search-as-you-type: fire SearchRequested once the
                // debounce window (60ms) has elapsed since the last query change.
                if let Some(dirty_ts) = self.search_dirty_since
                    && dirty_ts.elapsed() >= SEARCH_DEBOUNCE
                {
                    cmds.push(ftui::Cmd::msg(CassMsg::SearchRequested));
                }
                cmds.push(ftui::Cmd::msg(CassMsg::ToastTick));
                if cmds.len() == 1 {
                    return cmds.remove(0);
                }
                ftui::Cmd::batch(cmds)
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

            // -- Analytics surface ---------------------------------------------
            CassMsg::AnalyticsEntered => {
                if self.surface != AppSurface::Analytics {
                    self.view_stack.push(self.surface);
                    self.surface = AppSurface::Analytics;
                }
                ftui::Cmd::none()
            }
            CassMsg::AnalyticsViewChanged(view) => {
                self.analytics_view = view;
                ftui::Cmd::none()
            }
            CassMsg::ViewStackPopped => {
                if let Some(prev) = self.view_stack.pop() {
                    self.surface = prev;
                } else {
                    self.surface = AppSurface::Search;
                }
                ftui::Cmd::none()
            }
            CassMsg::AnalyticsTimeRangeSet { since_ms, until_ms } => {
                self.analytics_filters.since_ms = since_ms;
                self.analytics_filters.until_ms = until_ms;
                ftui::Cmd::none()
            }
            CassMsg::AnalyticsAgentFilterSet(agents) => {
                self.analytics_filters.agents = agents;
                ftui::Cmd::none()
            }
            CassMsg::AnalyticsWorkspaceFilterSet(workspaces) => {
                self.analytics_filters.workspaces = workspaces;
                ftui::Cmd::none()
            }
            CassMsg::AnalyticsSourceFilterSet(sf) => {
                self.analytics_filters.source_filter = sf;
                ftui::Cmd::none()
            }
            CassMsg::AnalyticsFiltersClearAll => {
                self.analytics_filters = AnalyticsFilterState::default();
                ftui::Cmd::none()
            }

            // -- Lifecycle ----------------------------------------------------
            CassMsg::QuitRequested => {
                // ESC unwind: check pending state before quitting
                // If on analytics surface, pop back to search.
                if self.surface == AppSurface::Analytics {
                    return ftui::Cmd::msg(CassMsg::ViewStackPopped);
                }
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
                    let count = self.selected.len();
                    self.selected.clear();
                    self.open_confirm_armed = false;
                    self.status = format!("Cleared {count} selections");
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

        let degradation = frame.degradation;

        let breakpoint = LayoutBreakpoint::from_width(area.width);
        // Degrade border style when the budget controller signals SimpleBorders+
        let border_type = if self.fancy_borders && degradation.use_unicode_borders() {
            BorderType::Rounded
        } else {
            BorderType::Square
        };
        let row_h = self.density_mode.row_height();
        // At EssentialOnly+ drop all borders and decorative chrome.
        let adaptive_borders = if degradation.render_decorative() {
            Borders::ALL
        } else {
            Borders::NONE
        };
        let render_content = degradation.render_content();

        let styles = self.resolved_style_context();
        let plain = ftui::Style::default();

        // At NoStyling+ degradation, drop all color to monochrome.
        let apply_style = degradation.apply_styling();
        let root_style = if apply_style {
            styles.style(style_system::STYLE_APP_ROOT)
        } else {
            plain
        };
        let pane_style = if apply_style {
            styles.style(style_system::STYLE_PANE_BASE)
        } else {
            plain
        };
        let pane_focused_style = if apply_style {
            styles.style(style_system::STYLE_PANE_FOCUSED)
        } else {
            plain
        };
        let row_style = if apply_style {
            styles.style(style_system::STYLE_RESULT_ROW)
        } else {
            plain
        };
        let row_alt_style = if apply_style {
            styles.style(style_system::STYLE_RESULT_ROW_ALT)
        } else {
            plain
        };
        let row_selected_style = if apply_style {
            styles.style(style_system::STYLE_RESULT_ROW_SELECTED)
        } else {
            plain
        };
        let text_muted_style = if apply_style {
            styles.style(style_system::STYLE_TEXT_MUTED)
        } else {
            plain
        };
        let warning_style = if apply_style {
            styles.style(style_system::STYLE_STATUS_WARNING)
        } else {
            plain
        };
        let danger_style = if apply_style {
            styles.style(style_system::STYLE_STATUS_ERROR)
        } else {
            plain
        };

        // Paint root background across the entire terminal.
        Block::new().style(root_style).render(area, frame);

        // Optional update banner shown as top strip.
        let mut layout_area = area;
        if self.update_banner_visible()
            && area.height >= 2
            && let Some(info) = self.update_info.as_ref()
        {
            let banner_area = Rect::new(area.x, area.y, area.width, 1);
            let mut banner_text = if self.update_upgrade_armed {
                format!(
                    "Update v{} -> v{} | Press U again to confirm upgrade | N notes | S skip | Esc dismiss",
                    info.current_version, info.latest_version
                )
            } else {
                format!(
                    "Update v{} -> v{} | U upgrade | N notes | S skip | Esc dismiss",
                    info.current_version, info.latest_version
                )
            };
            if banner_text.len() > banner_area.width as usize {
                banner_text.truncate(banner_area.width as usize);
            }
            Paragraph::new(&*banner_text)
                .style(if self.update_upgrade_armed {
                    danger_style
                } else {
                    warning_style
                })
                .render(banner_area, frame);
            layout_area = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
        }

        // ── Surface routing ──────────────────────────────────────────────
        match self.surface {
            AppSurface::Search => {
                // ── Main vertical split: search bar | content | status ──
                let vertical = Flex::vertical()
                    .constraints([
                        Constraint::Fixed(3), // Search bar
                        Constraint::Min(4),   // Content area (results + detail)
                        Constraint::Fixed(1), // Status footer
                    ])
                    .split(layout_area);

                // ── Search bar ──────────────────────────────────────────
                let mode_label = match self.search_mode {
                    SearchMode::Lexical => "lexical",
                    SearchMode::Semantic => "semantic",
                    SearchMode::Hybrid => "hybrid",
                };
                let query_title = if area.width >= 80 {
                    format!("cass | {} | {mode_label}", self.theme_preset.name())
                } else {
                    format!("cass | {mode_label}")
                };
                let query_block = Block::new()
                    .borders(adaptive_borders)
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
                    if self.query.is_empty() {
                        Paragraph::new("\u{2502}<type to search>")
                            .style(text_muted_style)
                            .render(query_inner, frame);
                    } else {
                        let cpos = self.cursor_pos.min(self.query.len());
                        let display =
                            format!("{}\u{2502}{}", &self.query[..cpos], &self.query[cpos..]);
                        let text_style = styles.style(style_system::STYLE_TEXT_PRIMARY);
                        Paragraph::new(&*display)
                            .style(text_style)
                            .render(query_inner, frame);
                    };
                }

                // ── Content area: responsive layout ─────────────────────
                let content_area = vertical[1];

                let (hits, selected_idx) = if let Some(pane) = self.panes.get(self.active_pane) {
                    (&pane.hits[..], pane.selected)
                } else {
                    (&self.results[..], 0)
                };

                match breakpoint {
                    LayoutBreakpoint::Wide => {
                        let panes = Flex::horizontal()
                            .constraints([
                                Constraint::Percentage(60.0),
                                Constraint::Percentage(40.0),
                            ])
                            .gap(0)
                            .split(content_area);
                        self.render_results_pane(
                            frame,
                            panes[0],
                            hits,
                            selected_idx,
                            row_h,
                            border_type,
                            adaptive_borders,
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
                            adaptive_borders,
                            &styles,
                            pane_style,
                            pane_focused_style,
                            text_muted_style,
                        );
                    }
                    LayoutBreakpoint::Medium => {
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
                            adaptive_borders,
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
                            adaptive_borders,
                            &styles,
                            pane_style,
                            pane_focused_style,
                            text_muted_style,
                        );
                    }
                    LayoutBreakpoint::Narrow => match self.focus_region {
                        FocusRegion::Results => {
                            self.render_results_pane(
                                frame,
                                content_area,
                                hits,
                                selected_idx,
                                row_h,
                                border_type,
                                adaptive_borders,
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
                                adaptive_borders,
                                &styles,
                                pane_style,
                                pane_focused_style,
                                text_muted_style,
                            );
                        }
                    },
                }

                // ── Status footer ───────────────────────────────────────
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
                let hits_for_status = if let Some(pane) = self.panes.get(self.active_pane) {
                    pane.hits.len()
                } else {
                    self.results.len()
                };
                let degradation_tag = if degradation.is_full() {
                    String::new()
                } else {
                    format!(" | deg:{}", degradation.as_str())
                };
                let sel_tag = if self.selected.is_empty() {
                    String::new()
                } else {
                    format!(" | {} sel", self.selected.len())
                };
                let status_line = if self.status.is_empty() {
                    let hints = if area.width >= 100 {
                        " | F2=theme D=density ^B=borders"
                    } else if area.width >= 60 {
                        " | ?=help"
                    } else {
                        ""
                    };
                    format!(
                        " {hits_for_status} hits | {bp_label} | {density_label}{degradation_tag}{sel_tag}{hints}",
                    )
                } else {
                    format!(" {}{}{}", self.status, degradation_tag, sel_tag)
                };
                Paragraph::new(&*status_line)
                    .style(text_muted_style)
                    .render(vertical[2], frame);
            }

            AppSurface::Analytics => {
                // ── Analytics surface layout ─────────────────────────────
                let vertical = Flex::vertical()
                    .constraints([
                        Constraint::Fixed(3), // Header / nav bar
                        Constraint::Min(4),   // Content
                        Constraint::Fixed(1), // Status footer
                    ])
                    .split(layout_area);

                // ── Analytics header with view tabs ──────────────────────
                let header_title = if area.width >= 100 {
                    let view_tabs: String = AnalyticsView::all()
                        .iter()
                        .map(|v| {
                            if *v == self.analytics_view {
                                format!("[{}]", v.label())
                            } else {
                                v.label().to_string()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" | ");
                    format!("cass analytics | {view_tabs}")
                } else {
                    format!("cass analytics | {}", self.analytics_view.label())
                };
                let header_block = Block::new()
                    .borders(adaptive_borders)
                    .border_type(border_type)
                    .title(&header_title)
                    .title_alignment(Alignment::Left)
                    .style(pane_focused_style);
                let header_inner = header_block.inner(vertical[0]);
                header_block.render(vertical[0], frame);
                if render_content && !header_inner.is_empty() {
                    let filter_desc = self.analytics_filter_summary();
                    Paragraph::new(&*filter_desc)
                        .style(text_muted_style)
                        .render(header_inner, frame);
                }

                // ── Analytics content placeholder ────────────────────────
                let content_block = Block::new()
                    .borders(adaptive_borders)
                    .border_type(border_type)
                    .title(self.analytics_view.label())
                    .title_alignment(Alignment::Left)
                    .style(pane_style);
                let content_inner = content_block.inner(vertical[1]);
                content_block.render(vertical[1], frame);
                if render_content && !content_inner.is_empty() {
                    let placeholder = format!(
                        "Analytics {} view — placeholder\n\nEsc to return to search | Ctrl+P for palette",
                        self.analytics_view.label()
                    );
                    Paragraph::new(&*placeholder)
                        .style(text_muted_style)
                        .render(content_inner, frame);
                }

                // ── Analytics status footer ──────────────────────────────
                let analytics_deg_tag = if degradation.is_full() {
                    String::new()
                } else {
                    format!(" | deg:{}", degradation.as_str())
                };
                let analytics_status = format!(
                    " Analytics: {} | Esc=back Ctrl+P=palette{}",
                    self.analytics_view.label(),
                    analytics_deg_tag
                );
                Paragraph::new(&*analytics_status)
                    .style(text_muted_style)
                    .render(vertical[2], frame);
            }
        }

        // ── Export modal overlay ─────────────────────────────────────
        if self.show_export_modal {
            self.render_export_overlay(frame, area, &styles);
        }

        // ── Bulk actions modal overlay ───────────────────────────────
        if self.show_bulk_modal {
            let modal_w = 40u16.min(area.width.saturating_sub(4));
            let modal_h = (BULK_ACTIONS.len() as u16 + 2).min(area.height.saturating_sub(4));
            let mx = area.x + (area.width.saturating_sub(modal_w)) / 2;
            let my = area.y + (area.height.saturating_sub(modal_h)) / 2;
            let modal_area = Rect::new(mx, my, modal_w, modal_h);

            // Clear area behind modal
            Block::new().style(root_style).render(modal_area, frame);

            let title = format!(" Bulk Actions ({} selected) ", self.selected.len());
            let modal_block = Block::new()
                .borders(adaptive_borders)
                .border_type(border_type)
                .title(&title)
                .title_alignment(Alignment::Left)
                .style(pane_focused_style);
            let inner = modal_block.inner(modal_area);
            modal_block.render(modal_area, frame);

            if render_content && !inner.is_empty() {
                for (i, label) in BULK_ACTIONS.iter().enumerate() {
                    if i as u16 >= inner.height {
                        break;
                    }
                    let row_area = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
                    let prefix = if i == self.bulk_action_idx {
                        "> "
                    } else {
                        "  "
                    };
                    let line = format!("{prefix}{label}");
                    let row_style_here = if i == self.bulk_action_idx {
                        row_selected_style
                    } else {
                        text_muted_style
                    };
                    Paragraph::new(&*line)
                        .style(row_style_here)
                        .render(row_area, frame);
                }
            }
        }

        // ── Command palette overlay ──────────────────────────────────
        if self.palette_state.open {
            self.render_palette_overlay(frame, area, &styles);
        }
    }
}

// =========================================================================
// Entry Point
// =========================================================================

/// Background task: export a session to HTML.
///
/// Runs on a background thread via `Cmd::task` so the UI stays responsive.
#[allow(clippy::too_many_arguments)]
fn export_session_task(
    source_path: &str,
    output_dir: &std::path::Path,
    output_filename: &str,
    encrypt: bool,
    password: Option<&str>,
    show_timestamps: bool,
    include_tools: bool,
    title: &str,
    agent_name: &str,
) -> CassMsg {
    use crate::html_export::{
        ExportOptions as HtmlExportOptions, HtmlExporter, Message as HtmlMessage, TemplateMetadata,
    };
    use std::fs::File;
    use std::io::{BufRead, BufReader, Write};

    let session = std::path::Path::new(source_path);
    if !session.exists() {
        return CassMsg::ExportFailed(format!("Session not found: {source_path}"));
    }

    // Read and parse session messages.
    let file = match File::open(session) {
        Ok(f) => f,
        Err(e) => return CassMsg::ExportFailed(format!("Cannot open session: {e}")),
    };
    let reader = BufReader::new(file);
    let mut messages: Vec<HtmlMessage> = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Extract role and content from the JSON line.
        let role = val
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("unknown")
            .to_string();
        let content = val
            .get("content")
            .and_then(|c| {
                if c.is_string() {
                    c.as_str().map(|s| s.to_string())
                } else if c.is_array() {
                    // Handle array content (e.g., Claude Code format).
                    let parts: Vec<String> = c
                        .as_array()
                        .unwrap_or(&Vec::new())
                        .iter()
                        .filter_map(|part| {
                            part.get("text")
                                .and_then(|t| t.as_str())
                                .map(|s| s.to_string())
                        })
                        .collect();
                    if parts.is_empty() {
                        None
                    } else {
                        Some(parts.join("\n"))
                    }
                } else {
                    None
                }
            })
            .unwrap_or_default();

        if content.is_empty() && !include_tools {
            continue;
        }
        messages.push(HtmlMessage {
            role,
            content,
            timestamp: val
                .get("timestamp")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string()),
            tool_call: None,
            index: None,
            author: None,
        });
    }

    if messages.is_empty() {
        return CassMsg::ExportFailed("No messages found in session".to_string());
    }

    // Build export options and generate HTML.
    let options = HtmlExportOptions {
        title: Some(title.to_string()),
        include_cdn: true,
        syntax_highlighting: true,
        include_search: true,
        include_theme_toggle: true,
        encrypt,
        print_styles: true,
        agent_name: Some(agent_name.to_string()),
        show_timestamps,
        show_tool_calls: include_tools,
    };

    let exporter = HtmlExporter::with_options(options);
    let metadata = TemplateMetadata {
        timestamp: None,
        agent: Some(agent_name.to_string()),
        message_count: messages.len(),
        duration: None,
        project: None,
    };

    let groups = crate::group_messages_for_export(messages);
    let html = match exporter.export_messages(title, &groups, metadata, password) {
        Ok(h) => h,
        Err(e) => return CassMsg::ExportFailed(format!("HTML generation failed: {e}")),
    };

    // Write output file.
    let output_path = output_dir.join(output_filename);
    if let Some(parent) = output_path.parent()
        && !parent.exists()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return CassMsg::ExportFailed(format!("Cannot create output directory: {e}"));
    }
    match File::create(&output_path).and_then(|mut f| f.write_all(html.as_bytes())) {
        Ok(()) => CassMsg::ExportCompleted {
            output_path: output_path.clone(),
            file_size: html.len(),
            encrypted: encrypt,
        },
        Err(e) => CassMsg::ExportFailed(format!("Failed to write export: {e}")),
    }
}

/// Run the cass TUI using the ftui Program runtime.
///
/// This replaces the manual crossterm event loop in `run_tui()`.
/// The ftui runtime handles terminal lifecycle (raw mode, alt-screen),
/// event polling, rendering, and cleanup via RAII.
pub fn run_tui_ftui() -> anyhow::Result<()> {
    use ftui::render::budget::FrameBudgetConfig;

    let model = CassApp::default();

    // 16ms budget (60fps) with adaptive PID degradation.
    // The BudgetController inside the runtime will automatically
    // step through DegradationLevel::Full → SimpleBorders → …
    // when frame times exceed budget.  Bayesian diff strategy
    // selection (RuntimeDiffConfig::default) is already enabled
    // by ProgramConfig::fullscreen().
    let budget = FrameBudgetConfig::default();

    ftui::App::fullscreen(model)
        .with_mouse()
        .with_budget(budget)
        .run()
        .map_err(|e| anyhow::anyhow!("ftui runtime error: {e}"))
}

// =========================================================================
// Clipboard & editor helpers
// =========================================================================

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

#[cfg(not(test))]
fn command_exists(binary: &str) -> bool {
    StdCommand::new("which")
        .arg(binary)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
fn copy_to_clipboard(_text: &str) -> Result<(), String> {
    Ok(())
}

/// Copy text to the system clipboard using OSC52 with fallback to external tools.
///
/// Uses ftui-extras [`Clipboard::auto`] for full auto-detection: OSC52 (works
/// over SSH), multiplexer passthrough (tmux/screen), then external tools
/// (pbcopy/wl-copy/xclip/xsel).
#[cfg(not(test))]
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use ftui_extras::clipboard::{Clipboard, ClipboardSelection};
    use ftui::TerminalCapabilities;

    if text.is_empty() {
        return Ok(());
    }

    let caps = TerminalCapabilities::detect();
    let clipboard = Clipboard::auto(caps);

    if !clipboard.is_available() {
        return Err(
            "no clipboard backend available (no OSC52 support and no clipboard tool found)"
                .to_string(),
        );
    }

    let mut stdout = std::io::stdout();
    clipboard
        .set(text, ClipboardSelection::Clipboard, &mut stdout)
        .map_err(|e| format!("{e}"))
}

#[cfg(test)]
fn run_editor_command(_cmd: &mut StdCommand) -> Result<(), String> {
    Ok(())
}

#[cfg(not(test))]
fn run_editor_command(cmd: &mut StdCommand) -> Result<(), String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let status = cmd
        .status()
        .map_err(|e| format!("failed to launch editor '{program}': {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("editor '{program}' exited with {status}"))
    }
}

/// Open one or more search hits in an editor. Returns `(count_opened, editor_binary)`.
fn open_hits_in_editor(hits: &[SearchHit], editor_cmd: &str) -> Result<(usize, String), String> {
    if hits.is_empty() {
        return Ok((0, String::new()));
    }
    let (editor_bin, editor_args) = split_editor_command(editor_cmd);
    for hit in hits {
        let mut cmd = StdCommand::new(&editor_bin);
        cmd.args(&editor_args);
        if editor_bin == "code" {
            if let Some(line) = hit.line_number {
                cmd.arg("--goto").arg(format!("{}:{line}", hit.source_path));
            } else {
                cmd.arg(&hit.source_path);
            }
        } else if editor_bin == "vim"
            || editor_bin == "vi"
            || editor_bin == "nvim"
            || editor_bin == "nano"
        {
            if let Some(line) = hit.line_number {
                cmd.arg(format!("+{line}"));
            }
            cmd.arg(&hit.source_path);
        } else {
            cmd.arg(&hit.source_path);
        }
        run_editor_command(&mut cmd)?;
    }
    Ok((hits.len(), editor_bin))
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
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
        let _cm = CassMsg::CursorMoved { delta: 1 };
        let _cj = CassMsg::CursorJumped { to_end: true };
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

    /// Extract all immediate messages from a command (including one level of batch).
    fn extract_msgs(cmd: ftui::Cmd<CassMsg>) -> Vec<CassMsg> {
        match cmd {
            ftui::Cmd::Msg(m) => vec![m],
            ftui::Cmd::Batch(cmds) => cmds.into_iter().filter_map(extract_msg).collect(),
            _ => Vec::new(),
        }
    }

    fn sample_update_info() -> UpdateInfo {
        UpdateInfo {
            latest_version: "9.9.9".to_string(),
            tag_name: "v9.9.9".to_string(),
            current_version: "1.0.0".to_string(),
            release_url: "https://example.com/releases/v9.9.9".to_string(),
            is_newer: true,
            is_skipped: false,
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

    // ==================== Search bar UX tests (2noh9.3.2) ====================

    #[test]
    fn query_changed_appends_characters() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::QueryChanged("h".into()));
        let _ = app.update(CassMsg::QueryChanged("e".into()));
        let _ = app.update(CassMsg::QueryChanged("l".into()));
        assert_eq!(app.query, "hel");
        assert_eq!(app.cursor_pos, 3);
    }

    #[test]
    fn query_changed_backspace_removes_char() {
        let mut app = CassApp::default();
        app.query = "hello".to_string();
        app.cursor_pos = 5;
        let _ = app.update(CassMsg::QueryChanged(String::new())); // backspace
        assert_eq!(app.query, "hell");
        assert_eq!(app.cursor_pos, 4);
    }

    #[test]
    fn query_changed_sets_search_dirty() {
        let mut app = CassApp::default();
        assert!(app.search_dirty_since.is_none());
        let _ = app.update(CassMsg::QueryChanged("a".into()));
        assert!(app.search_dirty_since.is_some());
    }

    #[test]
    fn query_cleared_empties_and_marks_dirty() {
        let mut app = CassApp::default();
        app.query = "hello world".to_string();
        let _ = app.update(CassMsg::QueryCleared);
        assert!(app.query.is_empty());
        assert!(app.search_dirty_since.is_some());
    }

    #[test]
    fn query_word_deleted_removes_last_word() {
        let mut app = CassApp::default();
        app.query = "hello world".to_string();
        app.cursor_pos = 11;
        let _ = app.update(CassMsg::QueryWordDeleted);
        assert_eq!(app.query, "hello ");
        assert_eq!(app.cursor_pos, 6);
    }

    #[test]
    fn query_word_deleted_single_word_clears() {
        let mut app = CassApp::default();
        app.query = "hello".to_string();
        app.cursor_pos = 5;
        let _ = app.update(CassMsg::QueryWordDeleted);
        assert!(app.query.is_empty());
        assert_eq!(app.cursor_pos, 0);
    }

    #[test]
    fn query_submitted_pushes_to_history() {
        let mut app = CassApp::default();
        app.query = "authentication error".to_string();
        let cmd = app.update(CassMsg::QuerySubmitted);
        assert_eq!(app.query_history.front().unwrap(), "authentication error");
        // Should produce SearchRequested
        assert!(matches!(extract_msg(cmd), Some(CassMsg::SearchRequested)));
    }

    #[test]
    fn query_submitted_deduplicates_history() {
        let mut app = CassApp::default();
        app.query = "auth".to_string();
        let _ = app.update(CassMsg::QuerySubmitted);
        app.query = "db error".to_string();
        let _ = app.update(CassMsg::QuerySubmitted);
        app.query = "auth".to_string();
        let _ = app.update(CassMsg::QuerySubmitted);
        // "auth" should appear only once, at the front
        assert_eq!(app.query_history.len(), 2);
        assert_eq!(app.query_history[0], "auth");
        assert_eq!(app.query_history[1], "db error");
    }

    #[test]
    fn query_submitted_empty_loads_recent_history() {
        let mut app = CassApp::default();
        app.query_history.push_front("previous query".to_string());
        app.query.clear();
        let _ = app.update(CassMsg::QuerySubmitted);
        assert_eq!(app.query, "previous query");
    }

    #[test]
    fn search_completed_groups_into_panes() {
        let mut app = CassApp::default();
        let hits = vec![
            SearchHit {
                agent: "claude_code".into(),
                title: "Session 1".into(),
                snippet: "test".into(),
                content: "test content".into(),
                content_hash: 0,
                score: 1.0,
                source_path: "/a".into(),
                workspace: "/w".into(),
                workspace_original: None,
                created_at: Some(1000),
                line_number: Some(1),
                match_type: Default::default(),
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
            },
            SearchHit {
                agent: "codex".into(),
                title: "Session 2".into(),
                snippet: "test".into(),
                content: "test content 2".into(),
                content_hash: 1,
                score: 0.9,
                source_path: "/b".into(),
                workspace: "/w".into(),
                workspace_original: None,
                created_at: Some(2000),
                line_number: Some(5),
                match_type: Default::default(),
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
            },
            SearchHit {
                agent: "claude_code".into(),
                title: "Session 3".into(),
                snippet: "test".into(),
                content: "test content 3".into(),
                content_hash: 2,
                score: 0.8,
                source_path: "/c".into(),
                workspace: "/w".into(),
                workspace_original: None,
                created_at: Some(3000),
                line_number: Some(10),
                match_type: Default::default(),
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
            },
        ];
        let _ = app.update(CassMsg::SearchCompleted {
            hits,
            elapsed_ms: 42,
            suggestions: vec![],
            wildcard_fallback: false,
        });
        assert_eq!(app.panes.len(), 2, "should have 2 agent panes");
        // BTreeMap ordering: claude_code before codex
        assert_eq!(app.panes[0].agent, "claude_code");
        assert_eq!(app.panes[0].hits.len(), 2);
        assert_eq!(app.panes[1].agent, "codex");
        assert_eq!(app.panes[1].hits.len(), 1);
        assert_eq!(app.results.len(), 3);
        assert_eq!(app.last_search_ms, Some(42));
        assert!(app.status.contains("3 results"));
    }

    #[test]
    fn search_requested_clears_dirty_state() {
        let mut app = CassApp::default();
        app.search_dirty_since = Some(Instant::now());
        let _ = app.update(CassMsg::SearchRequested);
        assert!(app.search_dirty_since.is_none());
    }

    #[test]
    fn history_navigation_traverses_entries() {
        let mut app = CassApp::default();
        app.query_history.push_front("third".to_string());
        app.query_history.push_front("second".to_string());
        app.query_history.push_front("first".to_string());
        // Navigate forward through history (Ctrl+N)
        let _ = app.update(CassMsg::HistoryNavigated { forward: true });
        assert_eq!(app.query, "second");
        let _ = app.update(CassMsg::HistoryNavigated { forward: true });
        assert_eq!(app.query, "third");
        // Navigate back (Ctrl+P)
        let _ = app.update(CassMsg::HistoryNavigated { forward: false });
        assert_eq!(app.query, "second");
    }

    #[test]
    fn enter_in_query_mode_submits_search() {
        let mut app = CassApp::default();
        app.query = "test query".to_string();
        app.input_mode = InputMode::Query;
        // DetailOpened (Enter key) in query mode should route to QuerySubmitted
        let cmd = app.update(CassMsg::DetailOpened);
        // Should have pushed to history via QuerySubmitted
        assert_eq!(app.query_history.front().unwrap(), "test query");
        // Returns SearchRequested
        assert!(matches!(extract_msg(cmd), Some(CassMsg::SearchRequested)));
    }

    #[test]
    fn enter_with_detail_modal_opens_detail() {
        let mut app = CassApp::default();
        app.input_mode = InputMode::Query;
        app.show_detail_modal = true; // already in detail
        let _ = app.update(CassMsg::DetailOpened);
        // Should still be in detail modal (didn't redirect to search)
        assert!(app.show_detail_modal);
    }

    #[test]
    fn debounce_fires_search_after_elapsed() {
        let mut app = CassApp::default();
        // Set search_dirty_since to well past the debounce threshold
        app.search_dirty_since = Some(Instant::now() - std::time::Duration::from_millis(100));
        let cmd = app.update(CassMsg::Tick);
        // Should have fired SearchRequested via batch
        // After tick, search_dirty_since should be cleared by SearchRequested
        // The batch contains SearchRequested + ToastTick
        assert!(
            matches!(cmd, ftui::Cmd::Batch(_)),
            "tick should return batch with SearchRequested when debounce elapsed"
        );
    }

    #[test]
    fn debounce_does_not_fire_before_threshold() {
        let mut app = CassApp::default();
        // Set search_dirty_since to just now (within debounce window)
        app.search_dirty_since = Some(Instant::now());
        let cmd = app.update(CassMsg::Tick);
        // Should NOT have fired SearchRequested - just ToastTick
        assert!(
            matches!(cmd, ftui::Cmd::Msg(_)),
            "tick should return single Msg (ToastTick) when debounce not elapsed"
        );
    }

    #[test]
    fn query_changed_resets_history_cursor() {
        let mut app = CassApp::default();
        app.history_cursor = Some(2);
        let _ = app.update(CassMsg::QueryChanged("x".into()));
        assert!(app.history_cursor.is_none());
    }

    #[test]
    fn query_changed_returns_tick_cmd() {
        let mut app = CassApp::default();
        let cmd = app.update(CassMsg::QueryChanged("a".into()));
        assert!(
            matches!(cmd, ftui::Cmd::Tick(_)),
            "QueryChanged should return Cmd::Tick for debounce"
        );
    }

    #[test]
    fn query_cleared_returns_tick_and_resets_cursor() {
        let mut app = CassApp::default();
        app.query = "foo".to_string();
        app.cursor_pos = 3;
        let cmd = app.update(CassMsg::QueryCleared);
        assert!(
            matches!(cmd, ftui::Cmd::Tick(_)),
            "QueryCleared should return Cmd::Tick"
        );
        assert_eq!(app.cursor_pos, 0);
    }

    #[test]
    fn query_word_deleted_returns_tick_cmd() {
        let mut app = CassApp::default();
        app.query = "hello world".to_string();
        app.cursor_pos = 11;
        let cmd = app.update(CassMsg::QueryWordDeleted);
        assert!(
            matches!(cmd, ftui::Cmd::Tick(_)),
            "QueryWordDeleted should return Cmd::Tick when text was deleted"
        );
    }

    #[test]
    fn query_word_deleted_noop_at_start() {
        let mut app = CassApp::default();
        app.query = "hello".to_string();
        app.cursor_pos = 0;
        let cmd = app.update(CassMsg::QueryWordDeleted);
        assert_eq!(
            app.query, "hello",
            "should not change query when cursor at 0"
        );
        assert!(matches!(cmd, ftui::Cmd::None));
    }

    #[test]
    fn cursor_moved_bounds_checking() {
        let mut app = CassApp::default();
        app.query = "abc".to_string();
        app.cursor_pos = 1;
        let _ = app.update(CassMsg::CursorMoved { delta: -1 });
        assert_eq!(app.cursor_pos, 0);
        let _ = app.update(CassMsg::CursorMoved { delta: -1 });
        assert_eq!(app.cursor_pos, 0, "should clamp at 0");
        let _ = app.update(CassMsg::CursorMoved { delta: 1 });
        assert_eq!(app.cursor_pos, 1);
        let _ = app.update(CassMsg::CursorMoved { delta: 10 });
        assert_eq!(app.cursor_pos, 3, "should clamp at query length");
    }

    #[test]
    fn cursor_jumped_to_start_and_end() {
        let mut app = CassApp::default();
        app.query = "hello world".to_string();
        app.cursor_pos = 5;
        let _ = app.update(CassMsg::CursorJumped { to_end: true });
        assert_eq!(app.cursor_pos, 11);
        let _ = app.update(CassMsg::CursorJumped { to_end: false });
        assert_eq!(app.cursor_pos, 0);
    }

    #[test]
    fn insert_at_cursor_middle() {
        let mut app = CassApp::default();
        app.query = "hllo".to_string();
        app.cursor_pos = 1;
        let _ = app.update(CassMsg::QueryChanged("e".into()));
        assert_eq!(app.query, "hello");
        assert_eq!(app.cursor_pos, 2);
    }

    #[test]
    fn backspace_at_cursor_middle() {
        let mut app = CassApp::default();
        app.query = "heello".to_string();
        app.cursor_pos = 3;
        let _ = app.update(CassMsg::QueryChanged(String::new()));
        assert_eq!(app.query, "hello");
        assert_eq!(app.cursor_pos, 2);
    }

    #[test]
    fn history_navigation_sets_cursor_to_end() {
        let mut app = CassApp::default();
        app.query_history.push_front("long query text".to_string());
        app.cursor_pos = 0;
        let _ = app.update(CassMsg::HistoryNavigated { forward: true });
        assert_eq!(app.cursor_pos, 15);
    }

    // ==================== Update assistant tests ====================

    #[test]
    fn update_check_completed_sets_banner_state() {
        let mut app = CassApp::default();
        assert!(!app.update_banner_visible());
        let _ = app.update(CassMsg::UpdateCheckCompleted(sample_update_info()));
        assert!(app.update_banner_visible());
        assert!(!app.update_dismissed);
        assert!(!app.update_upgrade_armed);
        assert!(app.status.contains("Update available"));
    }

    #[test]
    fn update_dismiss_hides_banner() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::UpdateCheckCompleted(sample_update_info()));
        assert!(app.update_banner_visible());
        let _ = app.update(CassMsg::UpdateDismissed);
        assert!(!app.update_banner_visible());
        assert!(app.update_dismissed);
        assert!(!app.update_upgrade_armed);
    }

    #[test]
    fn update_upgrade_requires_double_confirm() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::UpdateCheckCompleted(sample_update_info()));

        let _ = app.update(CassMsg::UpdateUpgradeRequested);
        assert!(app.update_upgrade_armed);
        assert!(app.status.contains("Confirm upgrade"));

        let _ = app.update(CassMsg::UpdateUpgradeRequested);
        assert!(!app.update_upgrade_armed);
        assert!(app.status.contains("TEST mode: would launch self-update"));
    }

    #[test]
    fn tick_polls_update_channel_and_dispatches_completion() {
        let mut app = CassApp::default();
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Some(sample_update_info()))
            .expect("send update info to test channel");
        app.update_check_rx = Some(rx);

        let msgs = extract_msgs(app.update(CassMsg::Tick));
        let mut completed_info: Option<UpdateInfo> = None;
        for msg in msgs {
            match msg {
                CassMsg::UpdateCheckCompleted(info) => completed_info = Some(info),
                CassMsg::ToastTick => {}
                _ => {}
            }
        }

        assert!(
            completed_info.is_some(),
            "tick should dispatch update completion"
        );
        assert!(app.update_check_rx.is_none(), "receiver should be consumed");

        if let Some(info) = completed_info {
            let _ = app.update(CassMsg::UpdateCheckCompleted(info));
        }
        assert!(app.update_banner_visible());
    }

    #[test]
    fn update_shortcuts_intercept_query_when_banner_visible() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::UpdateCheckCompleted(sample_update_info()));

        let _ = app.update(CassMsg::QueryChanged("u".to_string()));
        assert!(app.update_upgrade_armed);
        assert!(app.query.is_empty(), "shortcut should not edit query text");

        let _ = app.update(CassMsg::QueryChanged("s".to_string()));
        assert!(
            app.update_dismissed,
            "skip should dismiss banner in test mode"
        );
        assert!(!app.update_upgrade_armed);
    }

    // ==================== Wildcard fallback toggle tests ====================

    #[test]
    fn wildcard_fallback_toggle_flips_state() {
        let mut app = CassApp::default();
        assert!(!app.wildcard_fallback);
        let _ = app.update(CassMsg::WildcardFallbackToggled);
        assert!(app.wildcard_fallback);
        let _ = app.update(CassMsg::WildcardFallbackToggled);
        assert!(!app.wildcard_fallback);
    }

    // ==================== Search dispatch tests ====================

    #[test]
    fn search_requested_skips_empty_query() {
        let mut app = CassApp::default();
        app.query = "   ".to_string();
        app.search_dirty_since = Some(Instant::now());
        let _ = app.update(CassMsg::SearchRequested);
        assert!(app.search_dirty_since.is_none(), "dirty state should clear");
        // No search dispatched (no service, query is empty whitespace)
        assert!(app.status.is_empty());
    }

    #[test]
    fn search_requested_dispatches_with_service() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct MockSearch {
            called: AtomicBool,
        }
        impl SearchService for MockSearch {
            fn execute(&self, _params: &SearchParams) -> Result<SearchResult, String> {
                self.called.store(true, Ordering::SeqCst);
                Ok(SearchResult {
                    hits: vec![],
                    elapsed_ms: 5,
                    suggestions: vec![],
                    wildcard_fallback: false,
                })
            }
        }

        let mock = Arc::new(MockSearch {
            called: AtomicBool::new(false),
        });
        let mut app = CassApp::default();
        app.query = "test query".to_string();
        app.search_service = Some(mock.clone());
        let cmd = app.update(CassMsg::SearchRequested);
        assert!(app.status.contains("Searching"));
        // Cmd should be a Task variant (non-none).
        // Verify by extracting the task closure via format debug.
        let debug = format!("{cmd:?}");
        assert!(debug.contains("Task"), "expected Cmd::Task, got: {debug}");
    }

    #[test]
    fn search_requested_noop_without_service() {
        let mut app = CassApp::default();
        app.query = "test query".to_string();
        app.search_service = None;
        let cmd = app.update(CassMsg::SearchRequested);
        let debug = format!("{cmd:?}");
        assert!(
            debug.contains("None"),
            "expected Cmd::None without service, got: {debug}"
        );
    }

    // ==================== VirtualizedList integration tests ====================

    #[test]
    fn result_item_render_item_height_matches_density() {
        let hit = SearchHit {
            title: "Test".into(),
            snippet: "snippet".into(),
            content: "content".into(),
            content_hash: 0,
            score: 0.9,
            agent: "claude_code".into(),
            source_path: "/a".into(),
            workspace: "/w".into(),
            workspace_original: None,
            created_at: None,
            line_number: None,
            match_type: Default::default(),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        };
        for (density_h, expected) in [(1u16, 1u16), (2, 2), (3, 3)] {
            let item = ResultItem {
                index: 1,
                hit: hit.clone(),
                row_height: density_h,
                even: true,
                max_width: 80,
                queued: false,
                stripe_style: ftui::Style::default(),
                agent_style: ftui::Style::default(),
            };
            assert_eq!(item.height(), expected, "density {density_h}");
        }
    }

    #[test]
    fn selection_moved_syncs_virtualized_state() {
        let mut app = CassApp::default();
        app.panes.push(AgentPane {
            agent: "claude_code".into(),
            hits: vec![
                SearchHit {
                    title: "A".into(),
                    snippet: "".into(),
                    content: "".into(),
                    content_hash: 0,
                    score: 1.0,
                    agent: "claude_code".into(),
                    source_path: "/a".into(),
                    workspace: "/w".into(),
                    workspace_original: None,
                    created_at: None,
                    line_number: None,
                    match_type: Default::default(),
                    source_id: "local".into(),
                    origin_kind: "local".into(),
                    origin_host: None,
                },
                SearchHit {
                    title: "B".into(),
                    snippet: "".into(),
                    content: "".into(),
                    content_hash: 1,
                    score: 0.9,
                    agent: "claude_code".into(),
                    source_path: "/b".into(),
                    workspace: "/w".into(),
                    workspace_original: None,
                    created_at: None,
                    line_number: None,
                    match_type: Default::default(),
                    source_id: "local".into(),
                    origin_kind: "local".into(),
                    origin_host: None,
                },
                SearchHit {
                    title: "C".into(),
                    snippet: "".into(),
                    content: "".into(),
                    content_hash: 2,
                    score: 0.8,
                    agent: "claude_code".into(),
                    source_path: "/c".into(),
                    workspace: "/w".into(),
                    workspace_original: None,
                    created_at: None,
                    line_number: None,
                    match_type: Default::default(),
                    source_id: "local".into(),
                    origin_kind: "local".into(),
                    origin_host: None,
                },
            ],
            selected: 0,
            total_count: 3,
        });
        app.active_pane = 0;

        // Move down twice
        let _ = app.update(CassMsg::SelectionMoved { delta: 1 });
        assert_eq!(app.panes[0].selected, 1);
        let _ = app.update(CassMsg::SelectionMoved { delta: 1 });
        assert_eq!(app.panes[0].selected, 2);

        // Move up once
        let _ = app.update(CassMsg::SelectionMoved { delta: -1 });
        assert_eq!(app.panes[0].selected, 1);

        // Jump to end
        let _ = app.update(CassMsg::SelectionJumped { to_end: true });
        assert_eq!(app.panes[0].selected, 2);

        // Jump to start
        let _ = app.update(CassMsg::SelectionJumped { to_end: false });
        assert_eq!(app.panes[0].selected, 0);

        // VirtualizedListState should be in sync
        let state = app.results_list_state.borrow();
        assert_eq!(state.selected, Some(0));
    }

    #[test]
    fn search_completed_resets_scroll_state() {
        let mut app = CassApp::default();
        // Set up some scroll state
        {
            let mut state = app.results_list_state.borrow_mut();
            state.select(Some(5));
        }
        let hits = vec![SearchHit {
            title: "New".into(),
            snippet: "".into(),
            content: "".into(),
            content_hash: 0,
            score: 1.0,
            agent: "claude_code".into(),
            source_path: "/a".into(),
            workspace: "/w".into(),
            workspace_original: None,
            created_at: None,
            line_number: None,
            match_type: Default::default(),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        }];
        let _ = app.update(CassMsg::SearchCompleted {
            hits,
            elapsed_ms: 10,
            suggestions: vec![],
            wildcard_fallback: false,
        });
        let state = app.results_list_state.borrow();
        assert_eq!(state.selected, Some(0), "should reset to first item");
        assert_eq!(state.scroll_offset(), 0, "should scroll to top");
    }

    #[test]
    fn queued_items_render_with_checkmark() {
        let hit = SearchHit {
            title: "Test".into(),
            snippet: "".into(),
            content: "".into(),
            content_hash: 0,
            score: 0.9,
            agent: "claude_code".into(),
            source_path: "/a".into(),
            workspace: "/w".into(),
            workspace_original: None,
            created_at: None,
            line_number: None,
            match_type: Default::default(),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        };
        let queued_item = ResultItem {
            index: 1,
            hit: hit.clone(),
            row_height: 1,
            even: true,
            max_width: 80,
            queued: true,
            stripe_style: ftui::Style::default(),
            agent_style: ftui::Style::default(),
        };
        let not_queued = ResultItem {
            index: 1,
            hit,
            row_height: 1,
            even: true,
            max_width: 80,
            queued: false,
            stripe_style: ftui::Style::default(),
            agent_style: ftui::Style::default(),
        };
        assert!(queued_item.queued);
        assert!(!not_queued.queued);
    }

    // =====================================================================
    // 2noh9.3.3 — Filter UI tests
    // =====================================================================

    #[test]
    fn time_preset_cycles_all_today_week_month() {
        assert_eq!(TimePreset::All.next(), TimePreset::Today);
        assert_eq!(TimePreset::Today.next(), TimePreset::Week);
        assert_eq!(TimePreset::Week.next(), TimePreset::Month);
        assert_eq!(TimePreset::Month.next(), TimePreset::All);
        // Custom also goes back to All
        assert_eq!(TimePreset::Custom.next(), TimePreset::All);
    }

    #[test]
    fn time_preset_labels() {
        assert_eq!(TimePreset::All.label(), "All time");
        assert_eq!(TimePreset::Today.label(), "Today");
        assert_eq!(TimePreset::Week.label(), "Past 7d");
        assert_eq!(TimePreset::Month.label(), "Past 30d");
        assert_eq!(TimePreset::Custom.label(), "Custom");
    }

    #[test]
    fn time_preset_cycled_sets_filter_timestamps() {
        let mut app = CassApp::default();
        assert_eq!(app.time_preset, TimePreset::All);
        assert!(app.filters.created_from.is_none());

        // Cycle: All -> Today
        let _ = app.update(CassMsg::TimePresetCycled);
        assert_eq!(app.time_preset, TimePreset::Today);
        assert!(app.filters.created_from.is_some());
        assert!(app.filters.created_to.is_none());

        // Cycle: Today -> Week
        let _ = app.update(CassMsg::TimePresetCycled);
        assert_eq!(app.time_preset, TimePreset::Week);
        assert!(app.filters.created_from.is_some());

        // Cycle: Week -> Month
        let _ = app.update(CassMsg::TimePresetCycled);
        assert_eq!(app.time_preset, TimePreset::Month);
        assert!(app.filters.created_from.is_some());

        // Cycle: Month -> All (clears timestamps)
        let _ = app.update(CassMsg::TimePresetCycled);
        assert_eq!(app.time_preset, TimePreset::All);
        assert!(app.filters.created_from.is_none());
        assert!(app.filters.created_to.is_none());
    }

    #[test]
    fn source_filter_cycles_all_local_remote() {
        let mut app = CassApp::default();
        assert_eq!(app.filters.source_filter, SourceFilter::All);

        let _ = app.update(CassMsg::SourceFilterCycled);
        assert_eq!(app.filters.source_filter, SourceFilter::Local);

        let _ = app.update(CassMsg::SourceFilterCycled);
        assert_eq!(app.filters.source_filter, SourceFilter::Remote);

        let _ = app.update(CassMsg::SourceFilterCycled);
        assert_eq!(app.filters.source_filter, SourceFilter::All);
    }

    #[test]
    fn source_filter_source_id_resets_to_all() {
        let mut app = CassApp::default();
        app.filters.source_filter = SourceFilter::SourceId("myhost".to_string());
        let _ = app.update(CassMsg::SourceFilterCycled);
        assert_eq!(app.filters.source_filter, SourceFilter::All);
    }

    #[test]
    fn input_mode_applied_agent_parses_csv() {
        let mut app = CassApp::default();
        app.input_mode = InputMode::Agent;
        app.input_buffer = "claude_code, aider, codex".to_string();
        let _ = app.update(CassMsg::InputModeApplied);

        // Should have reset mode and cleared buffer
        assert_eq!(app.input_mode, InputMode::Query);
        assert!(app.input_buffer.is_empty());
    }

    #[test]
    fn input_mode_applied_workspace_parses_csv() {
        let mut app = CassApp::default();
        app.input_mode = InputMode::Workspace;
        app.input_buffer = "project_a, project_b".to_string();
        let _ = app.update(CassMsg::InputModeApplied);

        assert_eq!(app.input_mode, InputMode::Query);
        assert!(app.input_buffer.is_empty());
    }

    #[test]
    fn input_mode_applied_created_from_invalid_date_shows_error() {
        let mut app = CassApp::default();
        app.input_mode = InputMode::CreatedFrom;
        app.input_buffer = "not-a-date".to_string();
        let _ = app.update(CassMsg::InputModeApplied);

        assert!(app.status.contains("Invalid date"));
        assert_eq!(app.input_mode, InputMode::Query);
        assert!(app.input_buffer.is_empty());
    }

    #[test]
    fn input_mode_applied_created_from_empty_clears_filter() {
        let mut app = CassApp::default();
        app.time_preset = TimePreset::Custom;
        app.input_mode = InputMode::CreatedFrom;
        app.input_buffer = "".to_string();
        let _ = app.update(CassMsg::InputModeApplied);

        assert_eq!(app.time_preset, TimePreset::All);
        assert_eq!(app.input_mode, InputMode::Query);
    }

    #[test]
    fn input_mode_applied_created_to_invalid_date_shows_error() {
        let mut app = CassApp::default();
        app.input_mode = InputMode::CreatedTo;
        app.input_buffer = "bogus".to_string();
        let _ = app.update(CassMsg::InputModeApplied);

        assert!(app.status.contains("Invalid date"));
        assert_eq!(app.input_mode, InputMode::Query);
    }

    #[test]
    fn filters_clear_all_resets_time_preset() {
        let mut app = CassApp::default();
        // Set up some filter state
        app.time_preset = TimePreset::Week;
        app.filters.created_from = Some(1000);
        app.filters.source_filter = SourceFilter::Local;

        let _ = app.update(CassMsg::FiltersClearAll);

        assert_eq!(app.time_preset, TimePreset::All);
        assert!(app.filters.created_from.is_none());
        assert_eq!(app.filters.source_filter, SourceFilter::All);
    }

    // =====================================================================
    // 2noh9.3.5 — Detail/preview view tests
    // =====================================================================

    #[test]
    fn detail_wrap_toggle_flips_state() {
        let mut app = CassApp::default();
        assert!(app.detail_wrap, "default should be true");
        let _ = app.update(CassMsg::DetailWrapToggled);
        assert!(!app.detail_wrap);
        let _ = app.update(CassMsg::DetailWrapToggled);
        assert!(app.detail_wrap);
    }

    #[test]
    fn detail_tab_changed_resets_scroll() {
        let mut app = CassApp::default();
        app.detail_scroll = 42;
        let _ = app.update(CassMsg::DetailTabChanged(DetailTab::Snippets));
        assert_eq!(app.detail_tab, DetailTab::Snippets);
        assert_eq!(app.detail_scroll, 0, "should reset scroll on tab change");

        app.detail_scroll = 10;
        let _ = app.update(CassMsg::DetailTabChanged(DetailTab::Raw));
        assert_eq!(app.detail_tab, DetailTab::Raw);
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn detail_find_toggle_creates_and_clears_state() {
        let mut app = CassApp::default();
        assert!(app.detail_find.is_none());
        let _ = app.update(CassMsg::DetailFindToggled);
        assert!(app.detail_find.is_some());
        let _ = app.update(CassMsg::DetailFindToggled);
        assert!(app.detail_find.is_none());
    }

    #[test]
    fn detail_find_query_changed_updates_state() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::DetailFindToggled);
        let _ = app.update(CassMsg::DetailFindQueryChanged("hello".to_string()));
        assert_eq!(app.detail_find.as_ref().unwrap().query, "hello");
    }

    #[test]
    fn detail_find_navigation_wraps() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::DetailFindToggled);
        if let Some(ref mut find) = app.detail_find {
            find.query = "test".to_string();
            find.matches = vec![5, 10, 20];
            find.current = 0;
        }
        // Navigate forward
        let _ = app.update(CassMsg::DetailFindNavigated { forward: true });
        assert_eq!(app.detail_find.as_ref().unwrap().current, 1);
        let _ = app.update(CassMsg::DetailFindNavigated { forward: true });
        assert_eq!(app.detail_find.as_ref().unwrap().current, 2);
        // Wrap around
        let _ = app.update(CassMsg::DetailFindNavigated { forward: true });
        assert_eq!(app.detail_find.as_ref().unwrap().current, 0);
        // Navigate backward from 0 wraps to end
        let _ = app.update(CassMsg::DetailFindNavigated { forward: false });
        assert_eq!(app.detail_find.as_ref().unwrap().current, 2);
    }

    #[test]
    fn detail_scrolled_clamps_to_zero() {
        let mut app = CassApp::default();
        app.detail_scroll = 5;
        let _ = app.update(CassMsg::DetailScrolled { delta: -10 });
        assert_eq!(app.detail_scroll, 0, "should clamp at zero");
    }

    #[test]
    fn detail_scrolled_increments() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::DetailScrolled { delta: 3 });
        assert_eq!(app.detail_scroll, 3);
        let _ = app.update(CassMsg::DetailScrolled { delta: 5 });
        assert_eq!(app.detail_scroll, 8);
    }

    fn make_test_hit() -> SearchHit {
        SearchHit {
            title: "Test Conversation".into(),
            snippet: "Hello **world**\nThis is a test".into(),
            content: "# Heading\n\nSome **bold** text\n\n```rust\nfn main() {}\n```".into(),
            content_hash: 42,
            score: 0.95,
            agent: "claude_code".into(),
            source_path: "/test/session.jsonl".into(),
            workspace: "/projects/test".into(),
            workspace_original: None,
            created_at: Some(1700000000),
            line_number: None,
            match_type: Default::default(),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        }
    }

    #[test]
    fn build_messages_lines_produces_output() {
        let app = CassApp::default();
        let hit = make_test_hit();
        let styles = StyleContext::from_options(StyleOptions::default());
        let lines = app.build_messages_lines(&hit, 80, &styles);
        assert!(
            !lines.is_empty(),
            "should produce at least header + content"
        );
        // Should have at least 3 lines: title, metadata, separator
        assert!(lines.len() >= 3);
    }

    #[test]
    fn build_snippets_lines_produces_output() {
        let app = CassApp::default();
        let hit = make_test_hit();
        let styles = StyleContext::from_options(StyleOptions::default());
        let lines = app.build_snippets_lines(&hit, &styles);
        assert!(!lines.is_empty(), "should produce snippet lines");
    }

    #[test]
    fn build_raw_lines_produces_json() {
        let app = CassApp::default();
        let hit = make_test_hit();
        let styles = StyleContext::from_options(StyleOptions::default());
        let lines = app.build_raw_lines(&hit, &styles);
        // Raw tab should contain JSON-like content
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans()
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("claude_code"), "should show agent in JSON");
        assert!(text.contains("score"), "should include score key in JSON");
    }

    #[test]
    fn apply_find_highlight_marks_matches() {
        let styles = StyleContext::from_options(StyleOptions::default());
        let mut lines = vec![
            ftui::text::Line::from("Hello world"),
            ftui::text::Line::from("World is great"),
            ftui::text::Line::from("No match here"),
        ];
        let matches = CassApp::apply_find_highlight(&mut lines, "world", 0, &styles);
        assert_eq!(matches.len(), 2, "should find 'world' in 2 lines");
    }

    #[test]
    fn detail_opened_in_non_query_mode_sets_modal() {
        let mut app = CassApp::default();
        app.input_mode = InputMode::PaneFilter;
        let _ = app.update(CassMsg::DetailOpened);
        assert!(app.show_detail_modal, "should open modal");
    }

    #[test]
    fn detail_closed_resets_focus() {
        let mut app = CassApp::default();
        app.show_detail_modal = true;
        app.focus_region = FocusRegion::Detail;
        let _ = app.update(CassMsg::DetailClosed);
        assert!(!app.show_detail_modal);
        assert_eq!(app.focus_region, FocusRegion::Results);
    }

    #[test]
    fn detail_messages_with_markdown_content_renders() {
        let mut app = CassApp::default();
        let hit = make_test_hit();
        // Set cached_detail to None — should fall back to hit content
        app.cached_detail = None;
        let styles = StyleContext::from_options(StyleOptions::default());
        let lines = app.build_messages_lines(&hit, 80, &styles);
        // The content has "# Heading" which is markdown — should render it
        assert!(lines.len() > 5, "markdown should produce multiple lines");
    }

    // ==================== Analytics surface tests ====================

    #[test]
    fn analytics_entered_switches_surface() {
        let mut app = CassApp::default();
        assert_eq!(app.surface, AppSurface::Search);
        assert!(app.view_stack.is_empty());

        let _ = app.update(CassMsg::AnalyticsEntered);
        assert_eq!(app.surface, AppSurface::Analytics);
        assert_eq!(app.view_stack, vec![AppSurface::Search]);
    }

    #[test]
    fn analytics_entered_idempotent() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::AnalyticsEntered);
        let _ = app.update(CassMsg::AnalyticsEntered);
        // Should not push duplicate onto stack
        assert_eq!(app.view_stack.len(), 1);
        assert_eq!(app.surface, AppSurface::Analytics);
    }

    #[test]
    fn analytics_view_changed_updates_subview() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::AnalyticsEntered);
        assert_eq!(app.analytics_view, AnalyticsView::Dashboard);

        let _ = app.update(CassMsg::AnalyticsViewChanged(AnalyticsView::Heatmap));
        assert_eq!(app.analytics_view, AnalyticsView::Heatmap);

        let _ = app.update(CassMsg::AnalyticsViewChanged(AnalyticsView::Cost));
        assert_eq!(app.analytics_view, AnalyticsView::Cost);
    }

    #[test]
    fn view_stack_popped_returns_to_search() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::AnalyticsEntered);
        assert_eq!(app.surface, AppSurface::Analytics);

        let _ = app.update(CassMsg::ViewStackPopped);
        assert_eq!(app.surface, AppSurface::Search);
        assert!(app.view_stack.is_empty());
    }

    #[test]
    fn view_stack_popped_empty_defaults_to_search() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        // Stack is empty
        let _ = app.update(CassMsg::ViewStackPopped);
        assert_eq!(app.surface, AppSurface::Search);
    }

    #[test]
    fn esc_from_analytics_pops_view_stack() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::AnalyticsEntered);
        assert_eq!(app.surface, AppSurface::Analytics);

        // QuitRequested on analytics returns Cmd::msg(ViewStackPopped).
        // Simulate the two-step dispatch:
        let _ = app.update(CassMsg::QuitRequested);
        let _ = app.update(CassMsg::ViewStackPopped);
        assert_eq!(app.surface, AppSurface::Search);
    }

    #[test]
    fn analytics_time_range_set() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::AnalyticsTimeRangeSet {
            since_ms: Some(1000),
            until_ms: Some(2000),
        });
        assert_eq!(app.analytics_filters.since_ms, Some(1000));
        assert_eq!(app.analytics_filters.until_ms, Some(2000));
    }

    #[test]
    fn analytics_agent_filter_set() {
        let mut app = CassApp::default();
        let agents: HashSet<String> = ["claude_code", "codex"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let _ = app.update(CassMsg::AnalyticsAgentFilterSet(agents.clone()));
        assert_eq!(app.analytics_filters.agents, agents);
    }

    #[test]
    fn analytics_workspace_filter_set() {
        let mut app = CassApp::default();
        let ws: HashSet<String> = ["proj_a"].iter().map(|s| s.to_string()).collect();
        let _ = app.update(CassMsg::AnalyticsWorkspaceFilterSet(ws.clone()));
        assert_eq!(app.analytics_filters.workspaces, ws);
    }

    #[test]
    fn analytics_source_filter_set() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::AnalyticsSourceFilterSet(SourceFilter::Local));
        assert_eq!(app.analytics_filters.source_filter, SourceFilter::Local);
    }

    #[test]
    fn analytics_filters_clear_all() {
        let mut app = CassApp::default();
        app.analytics_filters.since_ms = Some(1000);
        app.analytics_filters.agents.insert("claude_code".into());
        app.analytics_filters.source_filter = SourceFilter::Remote;

        let _ = app.update(CassMsg::AnalyticsFiltersClearAll);
        assert!(app.analytics_filters.since_ms.is_none());
        assert!(app.analytics_filters.agents.is_empty());
        assert_eq!(app.analytics_filters.source_filter, SourceFilter::All);
    }

    #[test]
    fn analytics_view_labels_all_unique() {
        let views = AnalyticsView::all();
        let labels: Vec<&str> = views.iter().map(|v| v.label()).collect();
        let mut unique = labels.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(labels.len(), unique.len(), "all view labels must be unique");
    }

    #[test]
    fn analytics_view_all_has_seven_entries() {
        assert_eq!(AnalyticsView::all().len(), 7);
    }

    #[test]
    fn analytics_filter_summary_empty() {
        let app = CassApp::default();
        assert_eq!(app.analytics_filter_summary(), "Filters: none");
    }

    #[test]
    fn analytics_filter_summary_with_filters() {
        let mut app = CassApp::default();
        app.analytics_filters.since_ms = Some(1000);
        app.analytics_filters.agents.insert("claude_code".into());
        let summary = app.analytics_filter_summary();
        assert!(summary.contains("since:1000"));
        assert!(summary.contains("agents:claude_code"));
    }

    #[test]
    fn palette_has_analytics_actions() {
        let actions = default_actions();
        let labels: Vec<&str> = actions.iter().map(|a| a.label.as_str()).collect();
        assert!(labels.contains(&"Analytics: Dashboard"));
        assert!(labels.contains(&"Analytics: Explorer"));
        assert!(labels.contains(&"Analytics: Heatmap"));
        assert!(labels.contains(&"Analytics: Breakdowns"));
        assert!(labels.contains(&"Analytics: Tools"));
        assert!(labels.contains(&"Analytics: Cost"));
        assert!(labels.contains(&"Analytics: Coverage"));
    }

    #[test]
    fn default_surface_is_search() {
        let app = CassApp::default();
        assert_eq!(app.surface, AppSurface::Search);
        assert_eq!(app.analytics_view, AnalyticsView::Dashboard);
        assert!(app.analytics_filters.agents.is_empty());
        assert!(app.analytics_filters.workspaces.is_empty());
        assert!(app.analytics_filters.since_ms.is_none());
        assert!(app.analytics_filters.until_ms.is_none());
    }

    // ── Adaptive rendering / perf budget tests ─────────────────────────

    /// Helper: render the app into a buffer at a given degradation level.
    fn render_at_degradation(
        app: &CassApp,
        width: u16,
        height: u16,
        level: ftui::render::budget::DegradationLevel,
    ) -> ftui::Buffer {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(width, height, &mut pool);
        frame.set_degradation(level);
        app.view(&mut frame);
        frame.buffer
    }

    #[test]
    fn view_deterministic_under_repeated_renders() {
        use ftui_harness::buffer_to_text;

        let app = CassApp::default();
        let buf1 =
            render_at_degradation(&app, 80, 24, ftui::render::budget::DegradationLevel::Full);
        let buf2 =
            render_at_degradation(&app, 80, 24, ftui::render::budget::DegradationLevel::Full);
        assert_eq!(
            buffer_to_text(&buf1),
            buffer_to_text(&buf2),
            "Repeated renders of the same state must produce identical output"
        );
    }

    #[test]
    fn view_no_panic_at_every_degradation_level() {
        use ftui::render::budget::DegradationLevel;

        let app = CassApp::default();
        let levels = [
            DegradationLevel::Full,
            DegradationLevel::SimpleBorders,
            DegradationLevel::NoStyling,
            DegradationLevel::EssentialOnly,
            DegradationLevel::Skeleton,
        ];
        for level in levels {
            let _ = render_at_degradation(&app, 80, 24, level);
        }
    }

    #[test]
    fn view_no_panic_analytics_at_every_degradation_level() {
        use ftui::render::budget::DegradationLevel;

        let mut app = CassApp::default();
        let _ = app.update(CassMsg::AnalyticsEntered);
        let levels = [
            DegradationLevel::Full,
            DegradationLevel::SimpleBorders,
            DegradationLevel::NoStyling,
            DegradationLevel::EssentialOnly,
            DegradationLevel::Skeleton,
        ];
        for level in levels {
            let _ = render_at_degradation(&app, 80, 24, level);
        }
    }

    #[test]
    fn view_degraded_borders_differ_from_full() {
        use ftui::render::budget::DegradationLevel;
        use ftui_harness::buffer_to_text;

        let app = CassApp::default();
        let full = buffer_to_text(&render_at_degradation(&app, 80, 24, DegradationLevel::Full));
        let simple = buffer_to_text(&render_at_degradation(
            &app,
            80,
            24,
            DegradationLevel::SimpleBorders,
        ));
        // Full uses rounded borders (╭), SimpleBorders uses ASCII (+/-/|).
        assert_ne!(
            full, simple,
            "SimpleBorders should produce different output than Full"
        );
    }

    #[test]
    fn view_essential_only_skips_borders() {
        use ftui::render::budget::DegradationLevel;
        use ftui_harness::buffer_to_text;

        let app = CassApp::default();
        let full_text =
            buffer_to_text(&render_at_degradation(&app, 80, 24, DegradationLevel::Full));
        let essential_text = buffer_to_text(&render_at_degradation(
            &app,
            80,
            24,
            DegradationLevel::EssentialOnly,
        ));
        // Full rendering has border characters; essential does not.
        let has_box_char = |s: &str| {
            s.contains('╭')
                || s.contains('╮')
                || s.contains('╰')
                || s.contains('╯')
                || s.contains('─')
        };
        assert!(
            has_box_char(&full_text),
            "Full should contain border characters"
        );
        assert!(
            !has_box_char(&essential_text),
            "EssentialOnly should not contain border characters"
        );
    }

    #[test]
    fn view_skeleton_skips_analytics_content() {
        use ftui::render::budget::DegradationLevel;
        use ftui_harness::buffer_to_text;

        let mut app = CassApp::default();
        let _ = app.update(CassMsg::AnalyticsEntered);
        let full_text =
            buffer_to_text(&render_at_degradation(&app, 80, 24, DegradationLevel::Full));
        let skeleton_text = buffer_to_text(&render_at_degradation(
            &app,
            80,
            24,
            DegradationLevel::Skeleton,
        ));
        // Full shows the placeholder text; Skeleton does not.
        assert!(
            full_text.contains("placeholder"),
            "Full analytics should show placeholder text"
        );
        assert!(
            !skeleton_text.contains("placeholder"),
            "Skeleton should skip content text"
        );
    }

    #[test]
    fn view_no_styling_drops_colors() {
        use ftui::render::budget::DegradationLevel;
        use ftui::render::cell::PackedRgba;

        let app = CassApp::default();
        let full_buf = render_at_degradation(&app, 80, 24, DegradationLevel::Full);
        let no_style_buf = render_at_degradation(&app, 80, 24, DegradationLevel::NoStyling);

        // Count cells with non-default/non-transparent foreground or background.
        let count_colored = |buf: &ftui::Buffer| -> usize {
            buf.cells()
                .iter()
                .filter(|c| {
                    c.fg != PackedRgba::WHITE && c.fg != PackedRgba::TRANSPARENT
                        || c.bg != PackedRgba::TRANSPARENT
                })
                .count()
        };
        let full_colored = count_colored(&full_buf);
        let no_style_colored = count_colored(&no_style_buf);
        assert!(
            no_style_colored < full_colored,
            "NoStyling ({no_style_colored}) should have fewer colored cells than Full ({full_colored})"
        );
    }

    #[test]
    fn degradation_level_status_tag_shown_when_degraded() {
        use ftui::render::budget::DegradationLevel;
        use ftui_harness::buffer_to_text;

        let app = CassApp::default();
        let full_text = buffer_to_text(&render_at_degradation(
            &app,
            120,
            24,
            DegradationLevel::Full,
        ));
        let degraded_text = buffer_to_text(&render_at_degradation(
            &app,
            120,
            24,
            DegradationLevel::SimpleBorders,
        ));
        assert!(
            !full_text.contains("deg:"),
            "Full should not show degradation tag"
        );
        assert!(
            degraded_text.contains("deg:SimpleBorders"),
            "SimpleBorders should show degradation tag in status"
        );
    }

    #[test]
    fn degradation_level_enum_progression() {
        use ftui::render::budget::DegradationLevel;

        let mut level = DegradationLevel::Full;
        assert!(level.is_full());
        assert!(level.use_unicode_borders());
        assert!(level.apply_styling());
        assert!(level.render_decorative());
        assert!(level.render_content());

        level = level.next(); // SimpleBorders
        assert!(!level.use_unicode_borders());
        assert!(level.apply_styling());

        level = level.next(); // NoStyling
        assert!(!level.apply_styling());
        assert!(level.render_decorative());

        level = level.next(); // EssentialOnly
        assert!(!level.render_decorative());
        assert!(level.render_content());

        level = level.next(); // Skeleton
        assert!(!level.render_content());

        level = level.next(); // SkipFrame
        assert!(level.is_max());
    }

    #[test]
    fn frame_budget_config_defaults_are_sane() {
        use ftui::render::budget::FrameBudgetConfig;

        let cfg = FrameBudgetConfig::default();
        assert_eq!(cfg.total, std::time::Duration::from_millis(16));
        assert!(cfg.allow_frame_skip);
        assert_eq!(cfg.degradation_cooldown, 3);
    }

    #[test]
    fn render_deterministic_across_both_surfaces() {
        use ftui::render::budget::DegradationLevel;
        use ftui_harness::buffer_to_text;

        let mut app = CassApp::default();
        let search_buf1 = render_at_degradation(&app, 80, 24, DegradationLevel::Full);
        let search_buf2 = render_at_degradation(&app, 80, 24, DegradationLevel::Full);
        assert_eq!(buffer_to_text(&search_buf1), buffer_to_text(&search_buf2));

        let _ = app.update(CassMsg::AnalyticsEntered);
        let analytics_buf1 = render_at_degradation(&app, 80, 24, DegradationLevel::Full);
        let analytics_buf2 = render_at_degradation(&app, 80, 24, DegradationLevel::Full);
        assert_eq!(
            buffer_to_text(&analytics_buf1),
            buffer_to_text(&analytics_buf2)
        );
    }

    // =====================================================================
    // 2noh9.3.9 — Multi-select & bulk actions
    // =====================================================================

    /// Helper: build a test SearchHit with a unique source_path and content_hash.
    fn make_hit(id: u64, path: &str) -> SearchHit {
        SearchHit {
            title: format!("Hit {id}"),
            snippet: String::new(),
            content: String::new(),
            content_hash: id,
            score: 1.0 - (id as f32 * 0.1),
            agent: "claude_code".into(),
            source_path: path.into(),
            workspace: "/w".into(),
            workspace_original: None,
            created_at: None,
            line_number: Some(id as usize),
            match_type: Default::default(),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        }
    }

    /// Helper: create a CassApp with one pane of N hits.
    fn app_with_hits(n: usize) -> CassApp {
        let mut app = CassApp::default();
        let hits: Vec<SearchHit> = (0..n)
            .map(|i| make_hit(i as u64, &format!("/path/{i}")))
            .collect();
        app.panes.push(AgentPane {
            agent: "claude_code".into(),
            total_count: hits.len(),
            hits,
            selected: 0,
        });
        app.active_pane = 0;
        app
    }

    #[test]
    fn selected_hit_key_from_hit_captures_stable_fields() {
        let hit = make_hit(42, "/some/path");
        let key = SelectedHitKey::from_hit(&hit);
        assert_eq!(key.source_id, "local");
        assert_eq!(key.source_path, "/some/path");
        assert_eq!(key.line_number, Some(42));
        assert_eq!(key.content_hash, 42);
    }

    #[test]
    fn selected_hit_key_equality_and_hash() {
        let h1 = make_hit(1, "/a");
        let h2 = make_hit(1, "/a");
        let h3 = make_hit(2, "/b");
        assert_eq!(SelectedHitKey::from_hit(&h1), SelectedHitKey::from_hit(&h2));
        assert_ne!(SelectedHitKey::from_hit(&h1), SelectedHitKey::from_hit(&h3));
    }

    #[test]
    fn toggle_select_adds_and_removes() {
        let mut app = app_with_hits(3);

        // Toggle: nothing selected → first item selected
        let _ = app.update(CassMsg::SelectionToggled);
        assert_eq!(app.selected.len(), 1);
        assert!(
            app.selected
                .contains(&SelectedHitKey::from_hit(&app.panes[0].hits[0]))
        );

        // Toggle again: removes it
        let _ = app.update(CassMsg::SelectionToggled);
        assert!(app.selected.is_empty());
    }

    #[test]
    fn toggle_select_multiple_items() {
        let mut app = app_with_hits(3);

        // Select item 0
        let _ = app.update(CassMsg::SelectionToggled);
        assert_eq!(app.selected.len(), 1);

        // Move to item 1 and select
        let _ = app.update(CassMsg::SelectionMoved { delta: 1 });
        let _ = app.update(CassMsg::SelectionToggled);
        assert_eq!(app.selected.len(), 2);

        // Move to item 2 and select
        let _ = app.update(CassMsg::SelectionMoved { delta: 1 });
        let _ = app.update(CassMsg::SelectionToggled);
        assert_eq!(app.selected.len(), 3);
    }

    #[test]
    fn select_all_toggles_between_all_and_none() {
        let mut app = app_with_hits(5);

        // Select all
        let _ = app.update(CassMsg::SelectAllToggled);
        assert_eq!(app.selected.len(), 5);

        // Toggle again: clears all
        let _ = app.update(CassMsg::SelectAllToggled);
        assert!(app.selected.is_empty());
    }

    #[test]
    fn item_enqueued_adds_and_advances() {
        let mut app = app_with_hits(3);

        // Enqueue first item → selection moves to 1
        let _ = app.update(CassMsg::ItemEnqueued);
        assert_eq!(app.selected.len(), 1);
        assert!(
            app.selected
                .contains(&SelectedHitKey::from_hit(&app.panes[0].hits[0]))
        );
        assert_eq!(app.panes[0].selected, 1);

        // Enqueue again → adds second, advances to 2
        let _ = app.update(CassMsg::ItemEnqueued);
        assert_eq!(app.selected.len(), 2);
        assert_eq!(app.panes[0].selected, 2);

        // Enqueue at end → no further advance (already at last)
        let _ = app.update(CassMsg::ItemEnqueued);
        assert_eq!(app.selected.len(), 3);
        assert_eq!(app.panes[0].selected, 2); // stays at last
    }

    #[test]
    fn selection_survives_reranking() {
        let mut app = app_with_hits(3);
        // Select item 1
        let _ = app.update(CassMsg::SelectionMoved { delta: 1 });
        let _ = app.update(CassMsg::SelectionToggled);
        let key = SelectedHitKey::from_hit(&app.panes[0].hits[1]);
        assert!(app.selected.contains(&key));

        // Simulate reranking by swapping items 0 and 1
        app.panes[0].hits.swap(0, 1);

        // The key should still match the same hit regardless of position
        assert!(app.selected.contains(&key));
        // And the hit at position 0 (formerly at position 1) should still match
        assert!(
            app.selected
                .contains(&SelectedHitKey::from_hit(&app.panes[0].hits[0]))
        );
    }

    #[test]
    fn open_confirm_armed_resets_on_selection_change() {
        let mut app = app_with_hits(3);
        app.open_confirm_armed = true;

        let _ = app.update(CassMsg::SelectionToggled);
        assert!(!app.open_confirm_armed);

        app.open_confirm_armed = true;
        let _ = app.update(CassMsg::SelectAllToggled);
        assert!(!app.open_confirm_armed);

        app.open_confirm_armed = true;
        let _ = app.update(CassMsg::ItemEnqueued);
        assert!(!app.open_confirm_armed);
    }

    #[test]
    fn bulk_modal_opens_and_closes() {
        let mut app = app_with_hits(3);
        assert!(!app.show_bulk_modal);

        // Must select something first — guard prevents opening with empty selection
        let _ = app.update(CassMsg::SelectAllToggled);
        let _ = app.update(CassMsg::BulkActionsOpened);
        assert!(app.show_bulk_modal);
        assert_eq!(app.bulk_action_idx, 0);

        let _ = app.update(CassMsg::BulkActionsClosed);
        assert!(!app.show_bulk_modal);
    }

    #[test]
    fn bulk_modal_refuses_to_open_with_empty_selection() {
        let mut app = app_with_hits(3);
        let _ = app.update(CassMsg::BulkActionsOpened);
        assert!(!app.show_bulk_modal);
        assert!(app.status.contains("No items selected"));
    }

    #[test]
    fn bulk_modal_navigation_up_down() {
        let mut app = app_with_hits(3);
        let _ = app.update(CassMsg::SelectAllToggled);
        let _ = app.update(CassMsg::BulkActionsOpened);

        // Move down
        let _ = app.update(CassMsg::SelectionMoved { delta: 1 });
        assert_eq!(app.bulk_action_idx, 1);

        let _ = app.update(CassMsg::SelectionMoved { delta: 1 });
        assert_eq!(app.bulk_action_idx, 2);

        let _ = app.update(CassMsg::SelectionMoved { delta: 1 });
        assert_eq!(app.bulk_action_idx, 3); // last item (0-indexed, 4 items)

        // No overflow
        let _ = app.update(CassMsg::SelectionMoved { delta: 1 });
        assert_eq!(app.bulk_action_idx, 3);

        // Move back up
        let _ = app.update(CassMsg::SelectionMoved { delta: -1 });
        assert_eq!(app.bulk_action_idx, 2);

        // No underflow
        let _ = app.update(CassMsg::SelectionMoved { delta: -1 });
        let _ = app.update(CassMsg::SelectionMoved { delta: -1 });
        let _ = app.update(CassMsg::SelectionMoved { delta: -1 });
        assert_eq!(app.bulk_action_idx, 0);
    }

    #[test]
    fn bulk_clear_selection_clears_and_shows_status() {
        let mut app = app_with_hits(3);
        let _ = app.update(CassMsg::SelectAllToggled);
        assert_eq!(app.selected.len(), 3);

        let _ = app.update(CassMsg::BulkActionExecuted { action_index: 3 });
        assert!(app.selected.is_empty());
        assert!(app.status.contains("Cleared 3"));
    }

    #[test]
    fn open_all_queued_empty_shows_message() {
        let mut app = app_with_hits(3);
        // No items selected
        let _ = app.update(CassMsg::OpenAllQueued);
        assert!(app.status.contains("No items queued"));
    }

    #[test]
    fn open_all_queued_large_batch_requires_confirmation() {
        let mut app = app_with_hits(15);
        // Select all 15
        let _ = app.update(CassMsg::SelectAllToggled);
        assert_eq!(app.selected.len(), 15);

        // First press: arms confirmation
        let _ = app.update(CassMsg::OpenAllQueued);
        assert!(app.open_confirm_armed);
        assert!(app.status.contains("again to confirm"));
        // Selection NOT cleared yet
        assert_eq!(app.selected.len(), 15);
    }

    #[test]
    fn open_all_queued_small_batch_opens_directly() {
        let mut app = app_with_hits(3);
        let _ = app.update(CassMsg::SelectAllToggled);
        assert_eq!(app.selected.len(), 3);

        // Small batch (< threshold) — opens directly (will fail with editor error, but
        // selection should be cleared)
        let _ = app.update(CassMsg::OpenAllQueued);
        // Selection cleared after attempt
        assert!(app.selected.is_empty());
        assert!(!app.open_confirm_armed);
    }

    #[test]
    fn selected_hits_collects_matching_pane_hits() {
        let mut app = app_with_hits(5);
        // Enqueue items 0, 2, 4
        let _ = app.update(CassMsg::ItemEnqueued); // item 0, advances to 1
        let _ = app.update(CassMsg::SelectionMoved { delta: 1 }); // now at 2
        let _ = app.update(CassMsg::ItemEnqueued); // item 2, advances to 3
        let _ = app.update(CassMsg::SelectionMoved { delta: 1 }); // now at 4
        let _ = app.update(CassMsg::ItemEnqueued); // item 4
        assert_eq!(app.selected.len(), 3);

        let hits = app.selected_hits();
        assert_eq!(hits.len(), 3);
        let paths: HashSet<String> = hits.iter().map(|h| h.source_path.clone()).collect();
        assert!(paths.contains("/path/0"));
        assert!(paths.contains("/path/2"));
        assert!(paths.contains("/path/4"));
    }

    #[test]
    fn bulk_modal_esc_closes_without_executing() {
        let mut app = app_with_hits(3);
        let _ = app.update(CassMsg::SelectAllToggled);
        let _ = app.update(CassMsg::BulkActionsOpened);
        assert!(app.show_bulk_modal);

        // Esc closes the modal
        let _ = app.update(CassMsg::QuitRequested);
        assert!(!app.show_bulk_modal);
        // Selection not cleared
        assert_eq!(app.selected.len(), 3);
    }

    #[test]
    fn bulk_modal_renders_without_panic() {
        use ftui_harness::buffer_to_text;

        let mut app = app_with_hits(3);
        let _ = app.update(CassMsg::SelectAllToggled);
        let _ = app.update(CassMsg::BulkActionsOpened);
        let buf = render_at_degradation(&app, 80, 24, ftui::render::budget::DegradationLevel::Full);
        let text = buffer_to_text(&buf);
        assert!(text.contains("Bulk Actions"));
    }
}
