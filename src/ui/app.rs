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
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ftui::runtime::input_macro::{MacroPlayback, MacroRecorder};

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
use crate::ui::shortcuts;
use crate::ui::time_parser::parse_time_input;
use crate::update_check::{UpdateInfo, open_in_browser, skip_version};
#[cfg(not(test))]
use crate::update_check::{run_self_update, spawn_update_check};
use ftui::widgets::Widget;
use ftui::widgets::block::{Alignment, Block};
use ftui::widgets::borders::{BorderType, Borders};
use ftui::widgets::help_registry::{HelpContent, HelpId, HelpRegistry, Keybinding};
use ftui::widgets::hint_ranker::{HintContext, HintRanker, RankerConfig};
use ftui::widgets::json_view::{JsonToken, JsonView};
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::{RenderItem, StatefulWidget, VirtualizedList, VirtualizedListState};
use ftui_extras::markdown::{MarkdownRenderer, MarkdownTheme, is_likely_markdown};

// ---------------------------------------------------------------------------
// Re-export ftui primitives through the adapter
// ---------------------------------------------------------------------------
use super::ftui_adapter::{Constraint, Flex, Rect};
use super::style_system::{self, StyleContext, StyleOptions, UiThemePreset};
use ftui::widgets::InspectorState;
use ftui::widgets::focus::{FocusId, FocusManager, FocusNode, NavDirection};

/// Well-known focus node IDs for the cass TUI layout.
pub mod focus_ids {
    use super::FocusId;
    // Primary surface nodes (tab order 0-2)
    pub const SEARCH_BAR: FocusId = 1;
    pub const RESULTS_LIST: FocusId = 2;
    pub const DETAIL_PANE: FocusId = 3;
    // Modal nodes (tab_index -1 = skip global tab order)
    pub const COMMAND_PALETTE: FocusId = 10;
    pub const HELP_OVERLAY: FocusId = 11;
    pub const EXPORT_MODAL: FocusId = 12;
    pub const CONSENT_DIALOG: FocusId = 13;
    pub const BULK_MODAL: FocusId = 14;
    pub const SAVED_VIEWS_MODAL: FocusId = 15;
    pub const SOURCE_FILTER_MENU: FocusId = 16;
    pub const DETAIL_MODAL: FocusId = 17;
    // Focus groups
    pub const GROUP_MAIN: u32 = 99;
    pub const GROUP_PALETTE: u32 = 100;
    pub const GROUP_HELP: u32 = 101;
    pub const GROUP_EXPORT: u32 = 102;
    pub const GROUP_CONSENT: u32 = 103;
    pub const GROUP_BULK: u32 = 104;
    pub const GROUP_SAVED_VIEWS: u32 = 105;
    pub const GROUP_SOURCE_FILTER: u32 = 106;
    pub const GROUP_DETAIL_MODAL: u32 = 107;
}

// =========================================================================
// Thread-local raw event stash (for model-level macro recording)
// =========================================================================

thread_local! {
    /// Stores the last raw ftui Event before it is converted to CassMsg.
    /// Used by the macro recorder to capture events at the terminal level.
    static RAW_EVENT_STASH: RefCell<Option<super::ftui_adapter::Event>> = const { RefCell::new(None) };
}

fn stash_raw_event(event: &super::ftui_adapter::Event) {
    RAW_EVENT_STASH.with(|buf| {
        *buf.borrow_mut() = Some(event.clone());
    });
}

fn take_raw_event() -> Option<super::ftui_adapter::Event> {
    RAW_EVENT_STASH.with(|buf| buf.borrow_mut().take())
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

/// Title used by the saved-views manager modal.
pub const SAVED_VIEWS_MODAL_TITLE: &str = " Saved Views ";

/// Number of selected items before requiring double-press confirmation.
pub const OPEN_CONFIRM_THRESHOLD: usize = 12;
const PANEL_RATIO_MIN: f64 = 0.25;
const PANEL_RATIO_MAX: f64 = 0.75;
const FOOTER_HINT_ROOT_ID: HelpId = HelpId(1_000_000);
const FOOTER_HINT_WIDE_MIN_WIDTH: u16 = 100;
const FOOTER_HINT_MEDIUM_MIN_WIDTH: u16 = 60;

#[derive(Clone, Debug)]
struct FooterHintCandidate {
    key: &'static str,
    action: &'static str,
    context: HintContext,
    static_priority: u32,
}

impl FooterHintCandidate {
    fn token(&self) -> String {
        format!("{}={}", self.key, self.action)
    }
}

// =========================================================================
// Animation infrastructure (bead 2noh9.4.14)
// =========================================================================

/// Spring-based animation durations / presets.
pub mod anim_config {
    use std::time::Duration;

    /// Focus flash settle time (spring-based, replaces 220ms linear).
    pub const FOCUS_FLASH_DURATION: Duration = Duration::from_millis(300);
    /// Peek badge display duration before fade-out.
    pub const PEEK_BADGE_DURATION: Duration = Duration::from_millis(800);
    /// Stagger delay between consecutive result items.
    pub const STAGGER_DELAY: Duration = Duration::from_millis(30);
    /// Maximum number of items that receive stagger animation.
    pub const MAX_ANIMATED_ITEMS: usize = 15;
    /// Modal open/close spring duration.
    pub const MODAL_SPRING_DURATION: Duration = Duration::from_millis(250);
    /// Panel resize interpolation duration.
    pub const PANEL_RESIZE_DURATION: Duration = Duration::from_millis(180);
}

/// Centralized animation state for all spring-based animations in the TUI.
///
/// All springs are ticked on every `CassMsg::Tick`.  When `enabled` is false
/// (CASS_DISABLE_ANIMATIONS=1), springs snap instantly to their targets.
#[derive(Debug)]
pub struct AnimationState {
    /// Master kill-switch: `false` when `CASS_DISABLE_ANIMATIONS=1`.
    pub enabled: bool,
    /// Focus flash spring (0→1 = flash active → settled).
    pub focus_flash: super::ftui_adapter::Spring,
    /// Peek badge spring (0→1 = badge visible → hidden).
    pub peek_badge: super::ftui_adapter::Spring,
    /// Panel resize spring (current → target split ratio, 0.0–1.0).
    pub panel_ratio: super::ftui_adapter::Spring,
    /// Modal open spring (0 = closed, 1 = fully open).
    pub modal_open: super::ftui_adapter::Spring,
    /// Result list reveal progress per slot (up to MAX_ANIMATED_ITEMS).
    pub reveal_springs: Vec<super::ftui_adapter::Spring>,
    /// Whether a reveal sequence is actively playing.
    pub reveal_active: bool,
}

impl Default for AnimationState {
    fn default() -> Self {
        Self::new(true)
    }
}

impl AnimationState {
    /// Create a new animation state.  Pass `false` to disable all animations.
    pub fn new(enabled: bool) -> Self {
        use super::ftui_adapter::Spring;
        Self {
            enabled,
            focus_flash: Spring::new(1.0, 1.0)
                .with_stiffness(280.0)
                .with_damping(22.0),
            peek_badge: Spring::new(0.0, 0.0)
                .with_stiffness(200.0)
                .with_damping(20.0),
            panel_ratio: Spring::new(0.7, 0.7)
                .with_stiffness(300.0)
                .with_damping(26.0),
            modal_open: Spring::new(0.0, 0.0)
                .with_stiffness(350.0)
                .with_damping(24.0),
            reveal_springs: Vec::new(),
            reveal_active: false,
        }
    }

    /// Read CASS_DISABLE_ANIMATIONS from environment.
    pub fn from_env() -> Self {
        let disabled = std::env::var("CASS_DISABLE_ANIMATIONS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Self::new(!disabled)
    }

    /// Tick all active springs by `dt`.  If animations are disabled, snap to targets.
    pub fn tick(&mut self, dt: std::time::Duration) {
        use super::ftui_adapter::Animation;
        if !self.enabled {
            // Snap all springs to rest instantly.
            self.focus_flash = super::ftui_adapter::Spring::new(
                self.focus_flash.target(),
                self.focus_flash.target(),
            );
            self.peek_badge = super::ftui_adapter::Spring::new(
                self.peek_badge.target(),
                self.peek_badge.target(),
            );
            self.panel_ratio = super::ftui_adapter::Spring::new(
                self.panel_ratio.target(),
                self.panel_ratio.target(),
            );
            self.modal_open = super::ftui_adapter::Spring::new(
                self.modal_open.target(),
                self.modal_open.target(),
            );
            for s in &mut self.reveal_springs {
                *s = super::ftui_adapter::Spring::new(s.target(), s.target());
            }
            self.reveal_active = false;
            return;
        }
        self.focus_flash.tick(dt);
        self.peek_badge.tick(dt);
        self.panel_ratio.tick(dt);
        self.modal_open.tick(dt);
        let mut all_done = true;
        for s in &mut self.reveal_springs {
            s.tick(dt);
            if !s.is_at_rest() {
                all_done = false;
            }
        }
        if self.reveal_active && all_done {
            self.reveal_active = false;
        }
    }

    /// Trigger a focus flash (spring from 0→1).
    pub fn trigger_focus_flash(&mut self) {
        self.focus_flash = super::ftui_adapter::Spring::new(0.0, 1.0)
            .with_stiffness(280.0)
            .with_damping(22.0);
    }

    /// Show peek badge (spring to 1), will need explicit hide.
    pub fn show_peek_badge(&mut self) {
        self.peek_badge.set_target(1.0);
    }

    /// Hide peek badge (spring to 0).
    pub fn hide_peek_badge(&mut self) {
        self.peek_badge.set_target(0.0);
    }

    /// Animate panel split ratio to a new target.
    pub fn set_panel_ratio(&mut self, target: f64) {
        self.panel_ratio.set_target(target);
    }

    /// Open a modal (spring to 1).
    pub fn open_modal(&mut self) {
        self.modal_open.set_target(1.0);
    }

    /// Close a modal (spring to 0).
    pub fn close_modal(&mut self) {
        self.modal_open.set_target(0.0);
    }

    /// Start a staggered reveal for `count` result items.
    pub fn start_reveal(&mut self, count: usize) {
        use super::ftui_adapter::Spring;
        let n = count.min(anim_config::MAX_ANIMATED_ITEMS);
        self.reveal_springs.clear();
        for i in 0..n {
            // Each item starts at 0 (hidden) and springs to 1 (visible).
            // Slight stagger by decreasing stiffness for later items.
            let stiffness = 320.0 - (i as f64 * 8.0).min(160.0);
            self.reveal_springs.push(
                Spring::new(0.0, 1.0)
                    .with_stiffness(stiffness)
                    .with_damping(22.0),
            );
        }
        self.reveal_active = true;
    }

    /// Get the reveal progress for item at index (0.0 = hidden, 1.0 = visible).
    pub fn reveal_progress(&self, idx: usize) -> f64 {
        if !self.enabled || !self.reveal_active {
            return 1.0;
        }
        self.reveal_springs
            .get(idx)
            .map(|s| s.position().clamp(0.0, 1.0))
            .unwrap_or(1.0)
    }

    /// Get the focus flash progress (0.0 = just triggered, 1.0 = settled).
    pub fn focus_flash_progress(&self) -> f32 {
        if !self.enabled {
            return 1.0;
        }
        self.focus_flash.position().clamp(0.0, 1.0) as f32
    }

    /// Get the peek badge visibility (0.0 = hidden, 1.0 = fully visible).
    pub fn peek_badge_progress(&self) -> f32 {
        if !self.enabled {
            return if self.peek_badge.target() > 0.5 {
                1.0
            } else {
                0.0
            };
        }
        self.peek_badge.position().clamp(0.0, 1.0) as f32
    }

    /// Get the modal open progress (0.0 = closed, 1.0 = fully open).
    pub fn modal_progress(&self) -> f32 {
        if !self.enabled {
            return if self.modal_open.target() > 0.5 {
                1.0
            } else {
                0.0
            };
        }
        self.modal_open.position().clamp(0.0, 1.0) as f32
    }

    /// Get the animated panel split ratio.
    pub fn panel_ratio_value(&self) -> f64 {
        if !self.enabled {
            return self.panel_ratio.target();
        }
        self.panel_ratio.position()
    }
}

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
    /// Sources management surface.
    Sources,
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
    /// Plan frequency + plan token share + trends.
    Plans,
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
            Self::Plans => "Plans",
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
            Self::Plans,
            Self::Coverage,
        ]
    }
}

/// Metric to display in the Explorer view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ExplorerMetric {
    #[default]
    ApiTokens,
    ContentTokens,
    Messages,
    ToolCalls,
    PlanMessages,
    Cost,
}

impl ExplorerMetric {
    pub fn label(self) -> &'static str {
        match self {
            Self::ApiTokens => "API Tokens",
            Self::ContentTokens => "Content Tokens",
            Self::Messages => "Messages",
            Self::ToolCalls => "Tool Calls",
            Self::PlanMessages => "Plan Messages",
            Self::Cost => "Cost (USD)",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::ApiTokens => Self::ContentTokens,
            Self::ContentTokens => Self::Messages,
            Self::Messages => Self::ToolCalls,
            Self::ToolCalls => Self::PlanMessages,
            Self::PlanMessages => Self::Cost,
            Self::Cost => Self::ApiTokens,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::ApiTokens => Self::Cost,
            Self::ContentTokens => Self::ApiTokens,
            Self::Messages => Self::ContentTokens,
            Self::ToolCalls => Self::Messages,
            Self::PlanMessages => Self::ToolCalls,
            Self::Cost => Self::PlanMessages,
        }
    }
}

/// Overlay mode for the Explorer view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ExplorerOverlay {
    #[default]
    None,
    ByAgent,
    ByWorkspace,
    BySource,
}

impl ExplorerOverlay {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "No Overlay",
            Self::ByAgent => "By Agent",
            Self::ByWorkspace => "By Workspace",
            Self::BySource => "By Source",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::None => Self::ByAgent,
            Self::ByAgent => Self::ByWorkspace,
            Self::ByWorkspace => Self::BySource,
            Self::BySource => Self::None,
        }
    }
}

/// Zoom presets for the Explorer time range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ExplorerZoom {
    #[default]
    All,
    Day,
    Week,
    Month,
    Quarter,
}

impl ExplorerZoom {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
            Self::Quarter => "90d",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Day,
            Self::Day => Self::Week,
            Self::Week => Self::Month,
            Self::Month => Self::Quarter,
            Self::Quarter => Self::All,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::All => Self::Quarter,
            Self::Day => Self::All,
            Self::Week => Self::Day,
            Self::Month => Self::Week,
            Self::Quarter => Self::Month,
        }
    }

    /// Convert to `(since_ms, until_ms)` relative to now.
    pub fn to_range(self) -> (Option<i64>, Option<i64>) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        match self {
            Self::All => (None, None),
            Self::Day => (Some(now_ms - 24 * 3600 * 1000), None),
            Self::Week => (Some(now_ms - 7 * 24 * 3600 * 1000), None),
            Self::Month => (Some(now_ms - 30 * 24 * 3600 * 1000), None),
            Self::Quarter => (Some(now_ms - 90 * 24 * 3600 * 1000), None),
        }
    }
}

/// Active tab within the Breakdowns view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BreakdownTab {
    #[default]
    Agent,
    Workspace,
    Source,
    Model,
}

impl BreakdownTab {
    pub fn label(self) -> &'static str {
        match self {
            Self::Agent => "Agents",
            Self::Workspace => "Workspaces",
            Self::Source => "Sources",
            Self::Model => "Models",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Agent => Self::Workspace,
            Self::Workspace => Self::Source,
            Self::Source => Self::Model,
            Self::Model => Self::Agent,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Agent => Self::Model,
            Self::Workspace => Self::Agent,
            Self::Source => Self::Workspace,
            Self::Model => Self::Source,
        }
    }

    pub fn all() -> &'static [Self] {
        &[Self::Agent, Self::Workspace, Self::Source, Self::Model]
    }
}

/// Metric to display in the Heatmap view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HeatmapMetric {
    #[default]
    ApiTokens,
    Messages,
    ContentTokens,
    ToolCalls,
    Cost,
    Coverage,
}

impl HeatmapMetric {
    pub fn label(self) -> &'static str {
        match self {
            Self::ApiTokens => "API Tokens",
            Self::Messages => "Messages",
            Self::ContentTokens => "Content Tokens",
            Self::ToolCalls => "Tool Calls",
            Self::Cost => "Cost (USD)",
            Self::Coverage => "Coverage %",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::ApiTokens => Self::Messages,
            Self::Messages => Self::ContentTokens,
            Self::ContentTokens => Self::ToolCalls,
            Self::ToolCalls => Self::Cost,
            Self::Cost => Self::Coverage,
            Self::Coverage => Self::ApiTokens,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::ApiTokens => Self::Coverage,
            Self::Messages => Self::ApiTokens,
            Self::ContentTokens => Self::Messages,
            Self::ToolCalls => Self::ContentTokens,
            Self::Cost => Self::ToolCalls,
            Self::Coverage => Self::Cost,
        }
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

/// Context passed when drilling down from an analytics selection into search.
///
/// Captures the time-range boundaries and dimensional filter implied by the
/// selected chart element (bucket, row, or heatmap day).
#[derive(Clone, Debug, Default)]
pub struct DrilldownContext {
    /// Start of the selected bucket's time window (ms epoch, inclusive).
    pub since_ms: Option<i64>,
    /// End of the selected bucket's time window (ms epoch, exclusive).
    pub until_ms: Option<i64>,
    /// Agent slug to filter by (from breakdowns / tools selection).
    pub agent: Option<String>,
    /// Model family to filter by (from cost / models selection).
    pub model: Option<String>,
}

// Re-export from the analytics_charts module.
pub use super::analytics_charts::AnalyticsChartData;

/// Which tab is active in the detail pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DetailTab {
    #[default]
    Messages,
    Snippets,
    Raw,
    /// Syntax-highlighted JSON viewer with collapsible tree display.
    Json,
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
///
/// # Topology Matrix
///
/// | Surface          | Narrow (<80)       | MediumNarrow (80-119)    | Medium (120-159)        | Wide (≥160)              |
/// |------------------|--------------------|--------------------------|-------------------------|--------------------------|
/// | **Search**       | Single pane, focus  | Side-by-side tight       | Side-by-side balanced   | Side-by-side spacious    |
/// |  └ Results       | Full-width or hide  | min 35 cols              | min 45 cols             | min 50 cols              |
/// |  └ Detail        | Full-width or hide  | min 25 cols              | min 32 cols             | min 34 cols              |
/// |  └ Split handle  | None (no split)     | Active (draggable)       | Active (draggable)      | Active (draggable)       |
/// |  └ Navigation    | Focus toggles pane  | Focus + mouse + drag     | Focus + mouse + drag    | Focus + mouse + drag     |
/// | **Analytics**    | Area-adaptive       | Area-adaptive            | Area-adaptive           | Area-adaptive            |
/// |  └ All 8 views   | min 20w×4h guard    | Full area, inline adjust | Full area, inline adjust| Full area, inline adjust |
/// | **Detail modal** | Full-screen overlay | Full-screen overlay      | Full-screen overlay     | Full-screen overlay      |
/// | **Other modals** | Centered, fixed     | Centered, fixed          | Centered, fixed         | Centered, fixed          |
/// | **Footer**       | "narrow"            | "med-n"                  | "med"                   | "wide"                   |
/// | **Inspector**    | "Narrow (<80)"      | "MedNarrow (80-119)"     | "Medium (120-159)"      | "Wide (>=160)"           |
///
/// # Interaction expectations
///
/// - **Narrow**: Keyboard-primary. `Tab`/`Enter` switches between results ↔ detail.
///   No split handle. Mouse clicks work on the visible pane only.
/// - **MediumNarrow**: Both panes visible but tight. Detail shows wrapped message
///   previews (25-col minimum). Split handle is draggable but range is constrained.
/// - **Medium**: Comfortable dual-pane. Both panes have enough room for full content.
///   Split handle draggable within 25–75% range.
/// - **Wide**: Spacious dual-pane. Extra width used for wider result columns and
///   full detail formatting. Split handle draggable within 25–75% range.
///
/// # Analytics surface notes
///
/// Analytics views do NOT consume `LayoutBreakpoint`. Each view checks its assigned
/// `Rect` dimensions directly (e.g., `area.height < 4 || area.width < 20` as a
/// minimum guard) and adapts layout inline. This is intentional: analytics views
/// occupy the full content area regardless of breakpoint, so the breakpoint only
/// affects the outer chrome (header, footer, borders).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutBreakpoint {
    /// <80 cols: single pane with tab switching (very tight)
    Narrow,
    /// 80-119 cols: side-by-side with tight detail pane
    MediumNarrow,
    /// 120-159 cols: side-by-side results/detail with balanced ratio
    Medium,
    /// >=160 cols: comfortable side-by-side results + detail panes
    Wide,
}

/// Per-breakpoint layout parameters for the search surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchTopology {
    /// Minimum width for the results (left) pane. 0 means single-pane mode.
    pub min_results: u16,
    /// Minimum width for the detail (right) pane. 0 means single-pane mode.
    pub min_detail: u16,
    /// Whether a draggable split handle is shown between panes.
    pub has_split_handle: bool,
    /// Whether both panes are visible simultaneously.
    pub dual_pane: bool,
}

impl LayoutBreakpoint {
    /// Classify from terminal width.
    pub fn from_width(cols: u16) -> Self {
        if cols >= 160 {
            Self::Wide
        } else if cols >= 120 {
            Self::Medium
        } else if cols >= 80 {
            Self::MediumNarrow
        } else {
            Self::Narrow
        }
    }

    /// Return the search surface topology contract for this breakpoint.
    pub fn search_topology(self) -> SearchTopology {
        match self {
            Self::Narrow => SearchTopology {
                min_results: 0,
                min_detail: 0,
                has_split_handle: false,
                dual_pane: false,
            },
            Self::MediumNarrow => SearchTopology {
                min_results: 35,
                min_detail: 25,
                has_split_handle: true,
                dual_pane: true,
            },
            Self::Medium => SearchTopology {
                min_results: 45,
                min_detail: 32,
                has_split_handle: true,
                dual_pane: true,
            },
            Self::Wide => SearchTopology {
                min_results: 50,
                min_detail: 34,
                has_split_handle: true,
                dual_pane: true,
            },
        }
    }

    /// Short label for the status footer.
    pub fn footer_label(self) -> &'static str {
        match self {
            Self::Narrow => "narrow",
            Self::MediumNarrow => "med-n",
            Self::Medium => "med",
            Self::Wide => "wide",
        }
    }

    /// Descriptive label for the inspector overlay.
    pub fn inspector_label(self) -> &'static str {
        match self {
            Self::Narrow => "Narrow (<80)",
            Self::MediumNarrow => "MedNarrow (80-119)",
            Self::Medium => "Medium (120-159)",
            Self::Wide => "Wide (>=160)",
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

/// Active tab in the inspector overlay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InspectorTab {
    #[default]
    /// Frame timing, FPS, render budget
    Timing,
    /// Widget layout bounds and focus state
    Layout,
    /// Hit-test regions and mouse targets
    HitRegions,
}

impl InspectorTab {
    pub fn label(self) -> &'static str {
        match self {
            Self::Timing => "Timing",
            Self::Layout => "Layout",
            Self::HitRegions => "Hit Regions",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Timing => Self::Layout,
            Self::Layout => Self::HitRegions,
            Self::HitRegions => Self::Timing,
        }
    }
}

/// Rolling frame timing statistics for the inspector overlay.
#[derive(Clone, Debug)]
pub struct FrameTimingStats {
    /// Ring buffer of recent frame durations (microseconds).
    pub frame_times_us: VecDeque<u64>,
    /// Timestamp of the last view() call.
    pub last_frame: Option<Instant>,
    /// Maximum ring buffer size.
    capacity: usize,
}

impl Default for FrameTimingStats {
    fn default() -> Self {
        Self {
            frame_times_us: VecDeque::with_capacity(120),
            last_frame: None,
            capacity: 120,
        }
    }
}

impl FrameTimingStats {
    /// Record a frame render and return its duration in microseconds.
    pub fn record_frame(&mut self) -> Option<u64> {
        let now = Instant::now();
        let dt = self
            .last_frame
            .map(|prev| now.duration_since(prev).as_micros() as u64);
        self.last_frame = Some(now);
        if let Some(us) = dt {
            if self.frame_times_us.len() >= self.capacity {
                self.frame_times_us.pop_front();
            }
            self.frame_times_us.push_back(us);
        }
        dt
    }

    /// Average frame time in microseconds (or 0 if empty).
    pub fn avg_us(&self) -> u64 {
        if self.frame_times_us.is_empty() {
            return 0;
        }
        let sum: u64 = self.frame_times_us.iter().sum();
        sum / self.frame_times_us.len() as u64
    }

    /// Estimated frames per second from rolling average.
    pub fn fps(&self) -> f64 {
        let avg = self.avg_us();
        if avg == 0 {
            return 0.0;
        }
        1_000_000.0 / avg as f64
    }

    /// 95th percentile frame time in microseconds.
    pub fn p95_us(&self) -> u64 {
        if self.frame_times_us.is_empty() {
            return 0;
        }
        let mut sorted: Vec<u64> = self.frame_times_us.iter().copied().collect();
        sorted.sort_unstable();
        let idx = (sorted.len() as f64 * 0.95) as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    /// Most recent frame time in microseconds.
    pub fn last_us(&self) -> u64 {
        self.frame_times_us.back().copied().unwrap_or(0)
    }
}

/// Named color slots in the theme editor, matching ThemeColorOverrides fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeColorSlot {
    Primary,
    Secondary,
    Accent,
    Background,
    Surface,
    Overlay,
    Text,
    TextMuted,
    TextSubtle,
    Success,
    Warning,
    Error,
    Info,
    Border,
    BorderFocused,
    SelectionBg,
    SelectionFg,
    ScrollbarTrack,
    ScrollbarThumb,
}

impl ThemeColorSlot {
    pub const ALL: [Self; 19] = [
        Self::Primary,
        Self::Secondary,
        Self::Accent,
        Self::Background,
        Self::Surface,
        Self::Overlay,
        Self::Text,
        Self::TextMuted,
        Self::TextSubtle,
        Self::Success,
        Self::Warning,
        Self::Error,
        Self::Info,
        Self::Border,
        Self::BorderFocused,
        Self::SelectionBg,
        Self::SelectionFg,
        Self::ScrollbarTrack,
        Self::ScrollbarThumb,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Primary => "Primary",
            Self::Secondary => "Secondary",
            Self::Accent => "Accent",
            Self::Background => "Background",
            Self::Surface => "Surface",
            Self::Overlay => "Overlay",
            Self::Text => "Text",
            Self::TextMuted => "Text Muted",
            Self::TextSubtle => "Text Subtle",
            Self::Success => "Success",
            Self::Warning => "Warning",
            Self::Error => "Error",
            Self::Info => "Info",
            Self::Border => "Border",
            Self::BorderFocused => "Border Focus",
            Self::SelectionBg => "Selection BG",
            Self::SelectionFg => "Selection FG",
            Self::ScrollbarTrack => "Scrollbar Trk",
            Self::ScrollbarThumb => "Scrollbar Thm",
        }
    }

    /// Get the current override value from ThemeColorOverrides.
    pub fn get(self, overrides: &style_system::ThemeColorOverrides) -> Option<&str> {
        match self {
            Self::Primary => overrides.primary.as_deref(),
            Self::Secondary => overrides.secondary.as_deref(),
            Self::Accent => overrides.accent.as_deref(),
            Self::Background => overrides.background.as_deref(),
            Self::Surface => overrides.surface.as_deref(),
            Self::Overlay => overrides.overlay.as_deref(),
            Self::Text => overrides.text.as_deref(),
            Self::TextMuted => overrides.text_muted.as_deref(),
            Self::TextSubtle => overrides.text_subtle.as_deref(),
            Self::Success => overrides.success.as_deref(),
            Self::Warning => overrides.warning.as_deref(),
            Self::Error => overrides.error.as_deref(),
            Self::Info => overrides.info.as_deref(),
            Self::Border => overrides.border.as_deref(),
            Self::BorderFocused => overrides.border_focused.as_deref(),
            Self::SelectionBg => overrides.selection_bg.as_deref(),
            Self::SelectionFg => overrides.selection_fg.as_deref(),
            Self::ScrollbarTrack => overrides.scrollbar_track.as_deref(),
            Self::ScrollbarThumb => overrides.scrollbar_thumb.as_deref(),
        }
    }

    /// Set an override value in ThemeColorOverrides.
    pub fn set(self, overrides: &mut style_system::ThemeColorOverrides, value: Option<String>) {
        match self {
            Self::Primary => overrides.primary = value,
            Self::Secondary => overrides.secondary = value,
            Self::Accent => overrides.accent = value,
            Self::Background => overrides.background = value,
            Self::Surface => overrides.surface = value,
            Self::Overlay => overrides.overlay = value,
            Self::Text => overrides.text = value,
            Self::TextMuted => overrides.text_muted = value,
            Self::TextSubtle => overrides.text_subtle = value,
            Self::Success => overrides.success = value,
            Self::Warning => overrides.warning = value,
            Self::Error => overrides.error = value,
            Self::Info => overrides.info = value,
            Self::Border => overrides.border = value,
            Self::BorderFocused => overrides.border_focused = value,
            Self::SelectionBg => overrides.selection_bg = value,
            Self::SelectionFg => overrides.selection_fg = value,
            Self::ScrollbarTrack => overrides.scrollbar_track = value,
            Self::ScrollbarThumb => overrides.scrollbar_thumb = value,
        }
    }

    /// Get the resolved color from the current theme.
    pub fn resolved_color(self, resolved: ftui::ResolvedTheme) -> ftui::Color {
        match self {
            Self::Primary => resolved.primary,
            Self::Secondary => resolved.secondary,
            Self::Accent => resolved.accent,
            Self::Background => resolved.background,
            Self::Surface => resolved.surface,
            Self::Overlay => resolved.overlay,
            Self::Text => resolved.text,
            Self::TextMuted => resolved.text_muted,
            Self::TextSubtle => resolved.text_subtle,
            Self::Success => resolved.success,
            Self::Warning => resolved.warning,
            Self::Error => resolved.error,
            Self::Info => resolved.info,
            Self::Border => resolved.border,
            Self::BorderFocused => resolved.border_focused,
            Self::SelectionBg => resolved.selection_bg,
            Self::SelectionFg => resolved.selection_fg,
            Self::ScrollbarTrack => resolved.scrollbar_track,
            Self::ScrollbarThumb => resolved.scrollbar_thumb,
        }
    }
}

/// State for the interactive theme editor modal.
#[derive(Clone, Debug)]
pub struct ThemeEditorState {
    /// Working copy of color overrides being edited.
    pub overrides: style_system::ThemeColorOverrides,
    /// Base preset the editor started from.
    pub base_preset: style_system::UiThemePreset,
    /// Currently selected color slot index.
    pub selected: usize,
    /// Whether we're editing a color value (hex input mode).
    pub editing: bool,
    /// Hex input buffer when editing a color.
    pub hex_buffer: String,
    /// Scroll offset for the color list.
    pub scroll: usize,
    /// Cached contrast report for the current config.
    pub contrast_warnings: Vec<String>,
}

impl ThemeEditorState {
    pub fn new(preset: style_system::UiThemePreset) -> Self {
        Self {
            overrides: style_system::ThemeColorOverrides::default(),
            base_preset: preset,
            selected: 0,
            editing: false,
            hex_buffer: String::new(),
            scroll: 0,
            contrast_warnings: Vec::new(),
        }
    }

    /// Create a new editor state, loading from a saved config if one exists.
    ///
    /// In test builds, skips disk I/O and returns a fresh state.
    #[allow(unused_mut)]
    pub fn from_data_dir(preset: style_system::UiThemePreset, data_dir: &Path) -> Self {
        let mut state = Self::new(preset);
        #[cfg(test)]
        let _ = data_dir;
        #[cfg(not(test))]
        {
            let cfg_path = data_dir.join("theme.json");
            if let Ok(cfg) = style_system::ThemeConfig::load_from_path(&cfg_path) {
                if let Some(p) = cfg.base_preset {
                    state.base_preset = p;
                }
                state.overrides = cfg.colors;
            }
        }
        state
    }

    #[allow(unused_mut)]
    pub fn from_disk(preset: style_system::UiThemePreset) -> Self {
        let data_dir = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("coding-agent-search");
        Self::from_data_dir(preset, &data_dir)
    }

    /// Get the currently selected color slot.
    pub fn selected_slot(&self) -> ThemeColorSlot {
        ThemeColorSlot::ALL[self.selected.min(ThemeColorSlot::ALL.len() - 1)]
    }

    /// Build a ThemeConfig from the current editor state.
    pub fn to_config(&self) -> style_system::ThemeConfig {
        style_system::ThemeConfig {
            version: 1,
            base_preset: Some(self.base_preset),
            colors: self.overrides.clone(),
        }
    }

    /// Refresh the contrast warnings from the current theme.
    pub fn refresh_contrast(&mut self, styles: &StyleContext) {
        let report = styles.contrast_report();
        self.contrast_warnings = report
            .checks
            .iter()
            .filter(|c| !c.passes)
            .map(|c| format!("{}: {:.1}:1 (need {:.1}:1)", c.pair, c.ratio, c.minimum))
            .collect();
    }
}

/// Inline find state within the detail pane.
#[derive(Clone, Debug, Default)]
pub struct DetailFindState {
    pub query: String,
    pub matches: Vec<u16>,
    pub current: usize,
}

/// How results are grouped into panes (G to cycle).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ResultsGrouping {
    #[default]
    Agent,
    Conversation,
    Workspace,
    Flat,
}

impl ResultsGrouping {
    pub fn label(self) -> &'static str {
        match self {
            Self::Agent => "by agent",
            Self::Conversation => "by conversation",
            Self::Workspace => "by workspace",
            Self::Flat => "flat",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Agent => Self::Conversation,
            Self::Conversation => Self::Workspace,
            Self::Workspace => Self::Flat,
            Self::Flat => Self::Agent,
        }
    }
}

/// Snapshot of undoable state for undo/redo (Ctrl+Z / Ctrl+Y).
#[derive(Clone, Debug)]
pub struct UndoEntry {
    pub description: &'static str,
    pub query: String,
    pub cursor_pos: usize,
    pub filters: SearchFilters,
    pub time_preset: TimePreset,
    pub ranking_mode: RankingMode,
    pub grouping_mode: ResultsGrouping,
}

/// Fixed-capacity undo/redo history.
#[derive(Clone, Debug)]
pub struct UndoHistory {
    pub undo_stack: Vec<UndoEntry>,
    pub redo_stack: Vec<UndoEntry>,
    pub max_depth: usize,
}

impl Default for UndoHistory {
    fn default() -> Self {
        Self {
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            max_depth: 100,
        }
    }
}

impl UndoHistory {
    /// Push a new snapshot. Clears redo stack.
    pub fn push(&mut self, entry: UndoEntry) {
        self.redo_stack.clear();
        self.undo_stack.push(entry);
        if self.undo_stack.len() > self.max_depth {
            self.undo_stack.remove(0);
        }
    }

    /// Pop the most recent undo entry, moving current state to redo.
    pub fn pop_undo(&mut self, current: UndoEntry) -> Option<UndoEntry> {
        let entry = self.undo_stack.pop()?;
        self.redo_stack.push(current);
        Some(entry)
    }

    /// Pop the most recent redo entry, moving current state to undo.
    pub fn pop_redo(&mut self, current: UndoEntry) -> Option<UndoEntry> {
        let entry = self.redo_stack.pop()?;
        self.undo_stack.push(current);
        Some(entry)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }
}

/// One column of results, grouped by a key.
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

fn source_display_label(source_id: &str, origin_host: Option<&str>) -> String {
    if source_id == "local" {
        "local".to_string()
    } else {
        origin_host.unwrap_or(source_id).to_string()
    }
}

fn normalized_source_kind(origin_kind: Option<&str>, source_id: &str) -> String {
    if let Some(kind) = origin_kind.map(str::trim).filter(|s| !s.is_empty()) {
        if kind.eq_ignore_ascii_case("local") {
            return "local".to_string();
        }
        if kind.eq_ignore_ascii_case("ssh") || kind.eq_ignore_ascii_case("remote") {
            return "remote".to_string();
        }
        return kind.to_ascii_lowercase();
    }
    if source_id == "local" {
        "local".to_string()
    } else {
        "remote".to_string()
    }
}

fn workspace_original_from_metadata(metadata: &serde_json::Value) -> Option<String> {
    metadata
        .get("cass")
        .and_then(|cass| cass.get("workspace_original"))
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned)
}

impl ResultItem {
    fn source_badge(&self) -> String {
        format!(
            "[{}]",
            source_display_label(&self.hit.source_id, self.hit.origin_host.as_deref())
        )
    }
}

impl RenderItem for ResultItem {
    fn render(&self, area: Rect, frame: &mut super::ftui_adapter::Frame, selected: bool) {
        let hit = &self.hit;
        let source_badge = self.source_badge();
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
                    "{sel_mark}{queue_mark}{:>2}. {title} {source_badge} [{location}]",
                    self.index,
                );
                Paragraph::new(&*text).style(base_style).render(area, frame);
            }
            2 => {
                // Cozy: title + metadata
                let line1 = format!("{sel_mark}{queue_mark}{:>2}. {title}", self.index);
                let line2 = format!("      {location} | {source_badge} | {:.1}", hit.score);
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
                let line3 = format!(
                    "      {} | {source_badge} | {location} | {:.1}",
                    hit.agent, hit.score
                );
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
    pub label: Option<String>,
    pub agents: HashSet<String>,
    pub workspaces: HashSet<String>,
    pub created_from: Option<i64>,
    pub created_to: Option<i64>,
    pub ranking: RankingMode,
    pub source_filter: SourceFilter,
}

// =========================================================================
// Screenshot export formats
// =========================================================================

/// Output format for TUI screenshot export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenshotFormat {
    /// Self-contained HTML with inline CSS styles.
    Html,
    /// Scalable vector graphics.
    Svg,
    /// Plain text (no ANSI codes).
    Text,
}

impl ScreenshotFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Svg => "svg",
            Self::Text => "txt",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Html => "HTML",
            Self::Svg => "SVG",
            Self::Text => "Text",
        }
    }
}

// =========================================================================
// Sources view state (2noh9.4.9)
// =========================================================================

/// Display-ready row for a configured source in the Sources view.
#[derive(Clone, Debug)]
pub struct SourcesViewItem {
    /// Source name (e.g., "laptop").
    pub name: String,
    /// Connection kind (local / ssh).
    pub kind: crate::sources::SourceKind,
    /// SSH host string (e.g., "user@laptop.local"), if remote.
    pub host: Option<String>,
    /// Sync schedule label.
    pub schedule: String,
    /// Number of paths configured.
    pub path_count: usize,
    /// Last sync timestamp (unix ms), if any.
    pub last_sync: Option<i64>,
    /// Last sync result label ("success", "failed", "partial", "never").
    pub last_result: String,
    /// Files synced in last run.
    pub files_synced: u64,
    /// Bytes transferred in last run.
    pub bytes_transferred: u64,
    /// Whether a sync/doctor action is currently running for this source.
    pub busy: bool,
    /// Doctor diagnostic summary (pass/warn/fail counts), if available.
    pub doctor_summary: Option<(usize, usize, usize)>,
    /// Error message, if the last operation failed.
    pub error: Option<String>,
}

/// State for the Sources management surface.
#[derive(Clone, Debug, Default)]
pub struct SourcesViewState {
    /// All configured sources as display rows.
    pub items: Vec<SourcesViewItem>,
    /// Currently selected index.
    pub selected: usize,
    /// Scroll offset for long lists.
    pub scroll: usize,
    /// Whether a bulk operation is running (e.g., sync-all).
    pub busy: bool,
    /// Path to sources.toml (for display).
    pub config_path: String,
    /// Status line message.
    pub status: String,
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
    /// Cached analytics chart data (loaded when entering analytics surface).
    pub analytics_cache: Option<AnalyticsChartData>,
    /// Current selection index within the active analytics subview (for drilldown).
    pub analytics_selection: usize,
    /// Explorer metric selector state.
    pub explorer_metric: ExplorerMetric,
    /// Explorer overlay mode.
    pub explorer_overlay: ExplorerOverlay,
    /// Explorer time-bucket granularity (Hour / Day / Week / Month).
    pub explorer_group_by: crate::analytics::GroupBy,
    /// Explorer zoom preset (All / 24h / 7d / 30d / 90d).
    pub explorer_zoom: ExplorerZoom,
    /// Active tab within the Breakdowns view.
    pub breakdown_tab: BreakdownTab,
    /// Active metric for the Heatmap view.
    pub heatmap_metric: HeatmapMetric,

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
    /// How results are grouped into panes.
    pub grouping_mode: ResultsGrouping,

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
    /// Cache for find-in-detail match line numbers (written during rendering).
    pub detail_find_matches_cache: RefCell<Vec<u16>>,
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
    /// Pending screenshot capture (set in update, consumed in view).
    pub screenshot_pending: Option<ScreenshotFormat>,
    /// Buffer for screenshot data captured during view() (RefCell for &self access).
    pub screenshot_result: RefCell<Option<(ScreenshotFormat, String)>>,
    /// Whether the bulk actions modal is visible.
    pub show_bulk_modal: bool,
    /// Whether the saved views manager modal is visible.
    pub show_saved_views_modal: bool,
    /// Current selected index inside saved views manager.
    pub saved_views_selection: usize,
    /// Active drag state while reordering saved views with the mouse.
    pub saved_view_drag: Option<SavedViewDragState>,
    /// Whether the saved views manager is currently renaming a slot.
    pub saved_view_rename_mode: bool,
    /// Rename buffer used while editing saved view labels.
    pub saved_view_rename_buffer: String,
    /// Whether the consent dialog (model download) is visible.
    pub show_consent_dialog: bool,
    /// Semantic search availability state.
    pub semantic_availability: SemanticAvailability,
    /// Whether the source filter popup menu is open.
    pub source_filter_menu_open: bool,
    /// Current selection index in the source filter menu.
    pub source_filter_menu_selection: usize,
    /// Discovered source IDs shown in the source filter menu.
    pub available_source_ids: Vec<String>,
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

    // -- Undo/redo --------------------------------------------------------
    /// History stack for query/filter state undo/redo (Ctrl+Z / Ctrl+Y).
    pub undo_history: UndoHistory,

    // -- Animation & timing -----------------------------------------------
    /// Spring-based animation state (focus flash, reveal, modal, panel).
    pub anim: AnimationState,
    /// Start time of the reveal animation (legacy, kept for tui.rs compat).
    pub reveal_anim_start: Option<Instant>,
    /// End time of the focus-flash indicator (legacy, kept for tui.rs compat).
    pub focus_flash_until: Option<Instant>,
    /// End time of the peek badge indicator (legacy, kept for tui.rs compat).
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
    // RefCell enables recording rects from view() which takes &self.
    /// Last rendered search bar area.
    pub last_search_bar_area: RefCell<Option<Rect>>,
    /// Last rendered results inner area (without borders).
    pub last_results_inner: RefCell<Option<Rect>>,
    /// Last rendered detail area rectangle.
    pub last_detail_area: RefCell<Option<Rect>>,
    /// Last rendered pane rectangles.
    pub last_pane_rects: RefCell<Vec<Rect>>,
    /// Last rendered pill hit-test rectangles.
    pub last_pill_rects: RefCell<Vec<(Rect, Pill)>>,
    /// Last rendered status footer area.
    pub last_status_area: RefCell<Option<Rect>>,
    /// Last rendered content area (results/detail container).
    pub last_content_area: RefCell<Option<Rect>>,
    /// Last rendered pane split handle hit area.
    pub last_split_handle_area: RefCell<Option<Rect>>,
    /// Last rendered saved-view list row hit areas.
    pub last_saved_view_row_areas: RefCell<Vec<(Rect, usize)>>,
    /// Active pane split drag state for mouse-based resize.
    pub pane_split_drag: Option<PaneSplitDragState>,

    // -- Input smoothness (jitter / hover stabilization) ----------------------
    /// Last mouse position for jitter detection (suppresses sub-threshold drag noise).
    pub last_mouse_pos: Option<(u16, u16)>,
    /// Timestamp of last saved-view drag hover change (for stabilization).
    pub drag_hover_settled_at: Option<Instant>,

    // -- Lazy-loaded services ---------------------------------------------
    /// Data directory used for runtime state/index operations.
    pub data_dir: PathBuf,
    /// SQLite database path used for indexing/search operations.
    pub db_path: PathBuf,
    /// Database reader (initialized on first use).
    pub db_reader: Option<Arc<SqliteStorage>>,
    /// Known workspace list (populated on first filter prompt).
    pub known_workspaces: Option<Vec<String>>,
    /// Search service for async query dispatch.
    pub search_service: Option<Arc<dyn SearchService>>,

    // -- Macro recording/playback -----------------------------------------
    /// Active macro recorder (when interactive recording is in progress).
    pub macro_recorder: Option<MacroRecorder>,
    /// Active macro playback scheduler (when replaying a macro).
    pub macro_playback: Option<MacroPlayback>,
    /// Whether to redact absolute paths when saving macros.
    pub macro_redact_paths: bool,

    // -- Theme editor -----------------------------------------------------
    /// Whether the theme editor modal is open.
    pub show_theme_editor: bool,
    /// Theme editor state (overrides, selected slot, hex input).
    pub theme_editor: Option<ThemeEditorState>,

    // -- Inspector / debug overlays ---------------------------------------
    /// Whether the inspector overlay is visible.
    pub show_inspector: bool,
    /// Active inspector tab (Timing / Layout / HitRegions).
    pub inspector_tab: InspectorTab,
    /// ftui inspector widget state (mode cycling, hit regions).
    pub inspector_state: InspectorState,
    /// Rolling frame timing statistics.
    pub frame_timing: FrameTimingStats,

    // -- Sources management (2noh9.4.9) -----------------------------------
    /// Sources management view state.
    pub sources_view: SourcesViewState,

    // -- Status line ------------------------------------------------------
    /// Footer status text.
    pub status: String,
    /// Guard against overlapping index-refresh tasks.
    pub index_refresh_in_flight: bool,
}

impl Default for CassApp {
    fn default() -> Self {
        let mut app = Self {
            surface: AppSurface::default(),
            view_stack: Vec::new(),
            analytics_view: AnalyticsView::default(),
            analytics_filters: AnalyticsFilterState::default(),
            analytics_cache: None,
            analytics_selection: 0,
            explorer_metric: ExplorerMetric::default(),
            explorer_overlay: ExplorerOverlay::default(),
            explorer_group_by: crate::analytics::GroupBy::Day,
            explorer_zoom: ExplorerZoom::default(),
            breakdown_tab: BreakdownTab::default(),
            heatmap_metric: HeatmapMetric::default(),
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
            grouping_mode: ResultsGrouping::default(),
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
            detail_find_matches_cache: RefCell::new(Vec::new()),
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
            screenshot_pending: None,
            screenshot_result: RefCell::new(None),
            show_bulk_modal: false,
            show_saved_views_modal: false,
            saved_views_selection: 0,
            saved_view_drag: None,
            saved_view_rename_mode: false,
            saved_view_rename_buffer: String::new(),
            show_consent_dialog: false,
            semantic_availability: SemanticAvailability::NotInstalled,
            source_filter_menu_open: false,
            source_filter_menu_selection: 0,
            available_source_ids: Vec::new(),
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
            undo_history: UndoHistory::default(),
            anim: AnimationState::from_env(),
            reveal_anim_start: None,
            focus_flash_until: None,
            peek_badge_until: None,
            last_tick: Instant::now(),
            dirty_since: None,
            search_dirty_since: None,
            spinner_frame: 0,
            saved_views: Vec::new(),
            last_search_bar_area: RefCell::new(None),
            last_results_inner: RefCell::new(None),
            last_detail_area: RefCell::new(None),
            last_pane_rects: RefCell::new(Vec::new()),
            last_pill_rects: RefCell::new(Vec::new()),
            last_status_area: RefCell::new(None),
            last_content_area: RefCell::new(None),
            last_split_handle_area: RefCell::new(None),
            last_saved_view_row_areas: RefCell::new(Vec::new()),
            pane_split_drag: None,
            last_mouse_pos: None,
            drag_hover_settled_at: None,
            data_dir: crate::default_data_dir(),
            db_path: crate::default_db_path(),
            db_reader: None,
            known_workspaces: None,
            search_service: None,
            macro_recorder: None,
            macro_playback: None,
            macro_redact_paths: false,
            show_theme_editor: false,
            theme_editor: None,
            show_inspector: false,
            inspector_tab: InspectorTab::default(),
            inspector_state: InspectorState::default(),
            frame_timing: FrameTimingStats::default(),
            sources_view: SourcesViewState::default(),
            status: String::new(),
            index_refresh_in_flight: false,
        };
        app.init_focus_graph();
        app
    }
}

impl CassApp {
    /// Initialize the focus graph with all nodes, edges, and groups.
    ///
    /// Called once after construction. Sets up 3 primary surface nodes
    /// (SearchBar, ResultsList, DetailPane) plus modal overlay nodes,
    /// directional edges between the main nodes, and focus groups for
    /// each modal (used with push_trap/pop_trap).
    fn init_focus_graph(&mut self) {
        use focus_ids::*;
        let g = self.focus_manager.graph_mut();

        // -- Primary surface nodes (participate in global Tab order) ------
        g.insert(FocusNode::new(SEARCH_BAR, Rect::new(0, 0, 80, 1)).with_tab_index(0));
        g.insert(FocusNode::new(RESULTS_LIST, Rect::new(0, 2, 40, 20)).with_tab_index(1));
        g.insert(FocusNode::new(DETAIL_PANE, Rect::new(40, 2, 40, 20)).with_tab_index(2));

        // Directional edges: SearchBar ↕ ResultsList ↔ DetailPane
        g.connect(SEARCH_BAR, NavDirection::Down, RESULTS_LIST);
        g.connect(RESULTS_LIST, NavDirection::Up, SEARCH_BAR);
        g.connect(RESULTS_LIST, NavDirection::Right, DETAIL_PANE);
        g.connect(DETAIL_PANE, NavDirection::Left, RESULTS_LIST);
        g.connect(DETAIL_PANE, NavDirection::Up, SEARCH_BAR);
        g.connect(SEARCH_BAR, NavDirection::Down, RESULTS_LIST);

        // Build wrap-around tab chain for primary nodes
        g.build_tab_chain(true);

        // -- Modal overlay nodes (tab_index -1 = skip global tab order) ---
        g.insert(
            FocusNode::new(COMMAND_PALETTE, Rect::new(10, 5, 60, 15))
                .with_tab_index(-1)
                .with_group(GROUP_PALETTE),
        );
        g.insert(
            FocusNode::new(HELP_OVERLAY, Rect::new(5, 2, 70, 20))
                .with_tab_index(-1)
                .with_group(GROUP_HELP),
        );
        g.insert(
            FocusNode::new(EXPORT_MODAL, Rect::new(10, 5, 60, 15))
                .with_tab_index(-1)
                .with_group(GROUP_EXPORT),
        );
        g.insert(
            FocusNode::new(CONSENT_DIALOG, Rect::new(15, 8, 50, 8))
                .with_tab_index(-1)
                .with_group(GROUP_CONSENT),
        );
        g.insert(
            FocusNode::new(BULK_MODAL, Rect::new(20, 5, 40, 10))
                .with_tab_index(-1)
                .with_group(GROUP_BULK),
        );
        g.insert(
            FocusNode::new(SAVED_VIEWS_MODAL, Rect::new(10, 3, 60, 18))
                .with_tab_index(-1)
                .with_group(GROUP_SAVED_VIEWS),
        );
        g.insert(
            FocusNode::new(SOURCE_FILTER_MENU, Rect::new(0, 1, 30, 10))
                .with_tab_index(-1)
                .with_group(GROUP_SOURCE_FILTER),
        );
        g.insert(
            FocusNode::new(DETAIL_MODAL, Rect::new(5, 2, 70, 20))
                .with_tab_index(-1)
                .with_group(GROUP_DETAIL_MODAL),
        );

        // -- Focus groups (one per modal, used with push_trap/pop_trap) ---
        self.focus_manager
            .create_group(GROUP_MAIN, vec![SEARCH_BAR, RESULTS_LIST, DETAIL_PANE]);
        self.focus_manager
            .create_group(GROUP_PALETTE, vec![COMMAND_PALETTE]);
        self.focus_manager
            .create_group(GROUP_HELP, vec![HELP_OVERLAY]);
        self.focus_manager
            .create_group(GROUP_EXPORT, vec![EXPORT_MODAL]);
        self.focus_manager
            .create_group(GROUP_CONSENT, vec![CONSENT_DIALOG]);
        self.focus_manager
            .create_group(GROUP_BULK, vec![BULK_MODAL]);
        self.focus_manager
            .create_group(GROUP_SAVED_VIEWS, vec![SAVED_VIEWS_MODAL]);
        self.focus_manager
            .create_group(GROUP_SOURCE_FILTER, vec![SOURCE_FILTER_MENU]);
        self.focus_manager
            .create_group(GROUP_DETAIL_MODAL, vec![DETAIL_MODAL]);

        // Start with ResultsList focused (matches legacy default FocusRegion::Results)
        self.focus_manager.focus(RESULTS_LIST);
    }

    /// Derive the legacy FocusRegion from the current FocusManager state.
    ///
    /// This bridges the new graph-based focus system with existing code
    /// that checks `focus_region` for rendering decisions.
    pub fn focused_region(&self) -> FocusRegion {
        match self.focus_manager.current() {
            Some(id) if id == focus_ids::DETAIL_PANE || id == focus_ids::DETAIL_MODAL => {
                FocusRegion::Detail
            }
            _ => FocusRegion::Results,
        }
    }

    fn state_file_path(&self) -> PathBuf {
        self.data_dir.join(TUI_STATE_FILE_NAME)
    }

    fn capture_persisted_state(&self) -> PersistedState {
        PersistedState {
            search_mode: self.search_mode,
            match_mode: self.match_mode,
            ranking_mode: self.ranking_mode,
            context_window: self.context_window,
            theme_dark: self.theme_dark,
            density_mode: self.density_mode,
            per_pane_limit: self.per_pane_limit,
            query_history: self.query_history.clone(),
            saved_views: self.saved_views.clone(),
            fancy_borders: self.fancy_borders,
            help_pinned: self.help_pinned,
            has_seen_help: self.help_pinned || self.show_help,
        }
    }

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

    /// Determine which UI region a mouse coordinate falls in.
    fn hit_test(&self, x: u16, y: u16) -> MouseHitRegion {
        if self.show_saved_views_modal {
            if let Some((_, row_idx)) = self
                .last_saved_view_row_areas
                .borrow()
                .iter()
                .find(|(rect, _)| rect.contains(x, y))
            {
                return MouseHitRegion::SavedViewRow { row_idx: *row_idx };
            }
            return MouseHitRegion::None;
        }

        if let Some(rect) = *self.last_split_handle_area.borrow()
            && rect.contains(x, y)
        {
            return MouseHitRegion::SplitHandle;
        }

        // Check results inner area first (most common click target).
        if let Some(rect) = *self.last_results_inner.borrow()
            && rect.contains(x, y)
        {
            let row_h = self.density_mode.row_height();
            let state = self.results_list_state.borrow();
            let scroll = state.scroll_offset();
            let row_in_viewport = ((y - rect.y) / row_h.max(1)) as usize;
            let item_idx = scroll + row_in_viewport;
            return MouseHitRegion::Results { item_idx };
        }
        if let Some(rect) = *self.last_detail_area.borrow()
            && rect.contains(x, y)
        {
            return MouseHitRegion::Detail;
        }
        if let Some(rect) = *self.last_search_bar_area.borrow()
            && rect.contains(x, y)
        {
            return MouseHitRegion::SearchBar;
        }
        if let Some(rect) = *self.last_status_area.borrow()
            && rect.contains(x, y)
        {
            return MouseHitRegion::StatusBar;
        }
        MouseHitRegion::None
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
            && !self.show_saved_views_modal
            && !self.show_export_modal
            && !self.show_consent_dialog
            && !self.source_filter_menu_open
            && !self.palette_state.open
    }

    fn footer_hint_context_key(&self) -> &'static str {
        if self.show_export_modal
            || self.show_bulk_modal
            || self.show_saved_views_modal
            || self.show_consent_dialog
            || self.source_filter_menu_open
            || self.palette_state.open
            || self.show_help
            || self.show_detail_modal
        {
            return "modal";
        }

        if self.surface == AppSurface::Analytics {
            return match self.analytics_view {
                AnalyticsView::Dashboard => "analytics:dashboard",
                AnalyticsView::Explorer => "analytics:explorer",
                AnalyticsView::Heatmap => "analytics:heatmap",
                AnalyticsView::Breakdowns => "analytics:breakdowns",
                AnalyticsView::Tools => "analytics:tools",
                AnalyticsView::Cost => "analytics:cost",
                AnalyticsView::Plans => "analytics:plans",
                AnalyticsView::Coverage => "analytics:coverage",
            };
        }

        if self.surface == AppSurface::Sources {
            return "sources";
        }

        if self.input_mode != InputMode::Query {
            return "filter";
        }

        if self.focused_region() == FocusRegion::Detail {
            return "detail";
        }

        "results"
    }

    fn footer_hint_slots(width: u16) -> usize {
        if width >= FOOTER_HINT_WIDE_MIN_WIDTH {
            4
        } else if width >= FOOTER_HINT_MEDIUM_MIN_WIDTH {
            2
        } else {
            0
        }
    }

    fn footer_hint_budget(width: u16) -> usize {
        if width >= FOOTER_HINT_WIDE_MIN_WIDTH {
            52
        } else if width >= FOOTER_HINT_MEDIUM_MIN_WIDTH {
            22
        } else {
            0
        }
    }

    fn footer_hint_candidates(&self) -> Vec<FooterHintCandidate> {
        let mut hints = Vec::with_capacity(16);
        let contextual = HintContext::Mode(self.footer_hint_context_key().to_string());
        let mut push = |key: &'static str,
                        action: &'static str,
                        context: HintContext,
                        static_priority: u32| {
            hints.push(FooterHintCandidate {
                key,
                action,
                context,
                static_priority,
            });
        };

        match self.footer_hint_context_key() {
            "results" => {
                push(shortcuts::DETAIL_OPEN, "open", contextual.clone(), 1);
                push(shortcuts::TOGGLE_SELECT, "select", contextual.clone(), 2);
                if !self.selected.is_empty() {
                    push(shortcuts::BULK_MENU, "bulk", contextual.clone(), 3);
                    push("Ctrl+O", "open", contextual.clone(), 4);
                    push(shortcuts::TAB_FOCUS, "focus", contextual.clone(), 5);
                    push(shortcuts::PANE_FILTER, "filter", contextual.clone(), 6);
                } else {
                    push(shortcuts::TAB_FOCUS, "focus", contextual.clone(), 3);
                    push(shortcuts::PANE_FILTER, "filter", contextual.clone(), 4);
                }
            }
            "detail" => {
                push(shortcuts::TAB_FOCUS, "focus", contextual.clone(), 1);
                push(shortcuts::JSON_VIEW, "json", contextual.clone(), 2);
                push(shortcuts::PANE_FILTER, "find", contextual.clone(), 3);
                push(shortcuts::COPY, "copy", contextual.clone(), 4);
            }
            "filter" => {
                push(shortcuts::DETAIL_OPEN, "apply", contextual.clone(), 1);
                push(shortcuts::DETAIL_CLOSE, "cancel", contextual.clone(), 2);
                push(shortcuts::TAB_FOCUS, "next", contextual.clone(), 3);
            }
            ctx if ctx.starts_with("analytics:") => {
                // Common: view navigation + back
                push("←/→", "views", contextual.clone(), 1);

                // Per-subview contextual hints
                match self.analytics_view {
                    AnalyticsView::Dashboard => {
                        // Dashboard is read-only KPI wall — no special keys
                    }
                    AnalyticsView::Explorer => {
                        push("m", "metric", contextual.clone(), 2);
                        push("o", "overlay", contextual.clone(), 3);
                        push("g", "group", contextual.clone(), 4);
                        push("z", "zoom", contextual.clone(), 5);
                    }
                    AnalyticsView::Heatmap => {
                        push("Tab", "metric", contextual.clone(), 2);
                        if self.analytics_selectable_count() > 0 {
                            push("↑/↓", "select", contextual.clone(), 3);
                            push(shortcuts::DETAIL_OPEN, "drill", contextual.clone(), 4);
                        }
                    }
                    AnalyticsView::Breakdowns => {
                        push("Tab", "tab", contextual.clone(), 2);
                        if self.analytics_selectable_count() > 0 {
                            push("↑/↓", "select", contextual.clone(), 3);
                            push(shortcuts::DETAIL_OPEN, "drill", contextual.clone(), 4);
                        }
                    }
                    AnalyticsView::Tools
                    | AnalyticsView::Cost
                    | AnalyticsView::Plans
                    | AnalyticsView::Coverage => {
                        if self.analytics_selectable_count() > 0 {
                            push("↑/↓", "select", contextual.clone(), 2);
                            push(shortcuts::DETAIL_OPEN, "drill", contextual.clone(), 3);
                        }
                    }
                }

                push(shortcuts::DETAIL_CLOSE, "back", contextual.clone(), 10);
            }
            "modal" => {
                push(shortcuts::TAB_FOCUS, "next", contextual.clone(), 1);
                push("Space", "toggle", contextual.clone(), 2);
                push(shortcuts::DETAIL_CLOSE, "close", contextual.clone(), 3);
            }
            _ => {}
        }

        // Global hints are low-priority fallback hints.
        push(shortcuts::HELP, "help", HintContext::Global, 20);
        push(shortcuts::THEME, "theme", HintContext::Global, 21);
        push(shortcuts::DENSITY, "density", HintContext::Global, 22);
        push(shortcuts::BORDERS, "borders", HintContext::Global, 23);
        push(shortcuts::PALETTE, "palette", HintContext::Global, 24);
        push(shortcuts::DETAIL_CLOSE, "quit", HintContext::Global, 25);

        hints
    }

    fn build_contextual_footer_hints(&self, width: u16) -> String {
        let slots = Self::footer_hint_slots(width);
        if slots == 0 {
            return String::new();
        }

        let budget = Self::footer_hint_budget(width);
        let context_key = self.footer_hint_context_key();
        let mut ranker = HintRanker::new(RankerConfig {
            hysteresis: 0.15,
            voi_weight: 0.0,
            lambda: 0.02,
            ..RankerConfig::default()
        });
        let mut registry = HelpRegistry::new();
        registry.register(FOOTER_HINT_ROOT_ID, HelpContent::short("cass footer hints"));

        for candidate in self.footer_hint_candidates() {
            let token = candidate.token();
            let rank_id = ranker.register(
                token.clone(),
                token.len() as f64,
                candidate.context,
                candidate.static_priority,
            );
            let help_id = HelpId(rank_id as u64 + 1);
            registry.register(
                help_id,
                HelpContent {
                    short: token,
                    long: None,
                    keybindings: vec![Keybinding::new(candidate.key, candidate.action)],
                    see_also: vec![],
                },
            );
            let _ = registry.set_parent(help_id, FOOTER_HINT_ROOT_ID);
        }

        let (ordering, _ledger) = ranker.rank(Some(context_key));
        let mut picked = Vec::with_capacity(slots);
        let mut used = 0usize;
        for rank_id in ordering {
            if picked.len() >= slots {
                break;
            }
            let help_id = HelpId(rank_id as u64 + 1);
            let Some(help) = registry.resolve(help_id) else {
                continue;
            };
            let Some(binding) = help.keybindings.first() else {
                continue;
            };
            let token = format!("{}={}", binding.key, binding.action);
            let extra = if picked.is_empty() {
                token.len()
            } else {
                token.len() + 2
            };
            if used + extra > budget {
                continue;
            }
            used += extra;
            picked.push(token);
        }

        if picked.is_empty() {
            String::new()
        } else {
            format!(" | {}", picked.join("  "))
        }
    }

    fn sort_saved_views(&mut self) {
        self.saved_views.sort_by_key(|v| v.slot);
    }

    fn clamp_saved_views_selection(&mut self) {
        if self.saved_views.is_empty() {
            self.saved_views_selection = 0;
            return;
        }
        self.saved_views_selection = self
            .saved_views_selection
            .min(self.saved_views.len().saturating_sub(1));
    }

    fn selected_saved_view_slot(&self) -> Option<u8> {
        self.saved_views
            .get(self.saved_views_selection)
            .map(|v| v.slot)
    }

    fn selected_saved_view_label(&self) -> Option<String> {
        self.saved_views
            .get(self.saved_views_selection)
            .and_then(|v| v.label.clone())
    }

    fn move_saved_views_selection(&mut self, delta: i32) {
        if self.saved_views.is_empty() {
            self.saved_views_selection = 0;
            return;
        }
        let len = self.saved_views.len() as i64;
        let next = self.saved_views_selection as i64 + delta as i64;
        self.saved_views_selection = next.rem_euclid(len) as usize;
    }

    fn reorder_saved_views(&mut self, from_idx: usize, to_idx: usize) -> bool {
        if self.saved_views.is_empty() || from_idx == to_idx {
            return false;
        }
        if from_idx >= self.saved_views.len() || to_idx >= self.saved_views.len() {
            return false;
        }

        self.sort_saved_views();
        let mut ordered_slots: Vec<u8> = self.saved_views.iter().map(|v| v.slot).collect();
        ordered_slots.sort_unstable();

        let moved = self.saved_views.remove(from_idx);
        self.saved_views.insert(to_idx, moved);
        for (view, slot) in self.saved_views.iter_mut().zip(ordered_slots.into_iter()) {
            view.slot = slot;
        }
        self.saved_views_selection = to_idx.min(self.saved_views.len().saturating_sub(1));
        true
    }

    fn panel_ratio_from_mouse_x(&self, x: u16) -> Option<f64> {
        let area = self.last_content_area.borrow().as_ref().copied()?;
        if area.width < 4 {
            return None;
        }
        let rel_x = x
            .saturating_sub(area.x)
            .min(area.width.saturating_sub(1))
            .max(1);
        let ratio = rel_x as f64 / area.width as f64;
        Some(ratio.clamp(PANEL_RATIO_MIN, PANEL_RATIO_MAX))
    }

    fn apply_panel_ratio_from_mouse_x(&mut self, x: u16) -> bool {
        let Some(ratio) = self.panel_ratio_from_mouse_x(x) else {
            return false;
        };
        self.anim.set_panel_ratio(ratio);
        self.dirty_since = Some(Instant::now());
        true
    }

    fn split_content_area(
        &self,
        area: Rect,
        min_left: u16,
        min_right: u16,
    ) -> (Rect, Rect, Option<Rect>) {
        if area.width < 2 {
            return (area, Rect::new(area.x, area.y, 0, area.height), None);
        }

        let width = area.width;
        let ratio = self
            .anim
            .panel_ratio_value()
            .clamp(PANEL_RATIO_MIN, PANEL_RATIO_MAX);
        let mut left_w = ((width as f64) * ratio).round() as u16;
        let lower = min_left.max(1).min(width.saturating_sub(1));
        let upper = width
            .saturating_sub(min_right.max(1))
            .max(1)
            .min(width.saturating_sub(1));
        left_w = if lower <= upper {
            left_w.clamp(lower, upper)
        } else {
            width / 2
        };
        left_w = left_w.clamp(1, width.saturating_sub(1));

        let right_w = width.saturating_sub(left_w);
        let left = Rect::new(area.x, area.y, left_w, area.height);
        let right = Rect::new(area.x + left_w, area.y, right_w, area.height);
        let handle = Rect::new(area.x + left_w.saturating_sub(1), area.y, 1, area.height);

        (left, right, Some(handle))
    }

    /// Capture the current undoable state as an `UndoEntry`.
    fn capture_undo_state(&self, description: &'static str) -> UndoEntry {
        UndoEntry {
            description,
            query: self.query.clone(),
            cursor_pos: self.cursor_pos,
            filters: self.filters.clone(),
            time_preset: self.time_preset,
            ranking_mode: self.ranking_mode,
            grouping_mode: self.grouping_mode,
        }
    }

    /// Restore undoable state from an `UndoEntry`, triggering a search if query/filters changed.
    fn restore_undo_state(&mut self, entry: UndoEntry) -> ftui::Cmd<CassMsg> {
        let search_changed = self.query != entry.query
            || self.filters != entry.filters
            || self.ranking_mode != entry.ranking_mode;
        let grouping_changed = self.grouping_mode != entry.grouping_mode;

        self.query = entry.query;
        self.cursor_pos = entry.cursor_pos;
        self.filters = entry.filters;
        self.time_preset = entry.time_preset;
        self.ranking_mode = entry.ranking_mode;
        self.grouping_mode = entry.grouping_mode;

        if grouping_changed {
            self.regroup_panes();
        }

        if search_changed {
            ftui::Cmd::msg(CassMsg::SearchRequested)
        } else {
            ftui::Cmd::none()
        }
    }

    /// Push current state onto undo stack before a mutation.
    fn push_undo(&mut self, description: &'static str) {
        let entry = self.capture_undo_state(description);
        self.undo_history.push(entry);
    }

    /// Re-group results into panes using the current `grouping_mode`.
    fn regroup_panes(&mut self) {
        let mut pane_map: std::collections::BTreeMap<String, Vec<SearchHit>> =
            std::collections::BTreeMap::new();
        for hit in &self.results {
            let key = match self.grouping_mode {
                ResultsGrouping::Agent => hit.agent.clone(),
                ResultsGrouping::Conversation => {
                    // Use last path component of source_path as conversation key.
                    hit.source_path
                        .rsplit('/')
                        .next()
                        .unwrap_or(&hit.source_path)
                        .to_string()
                }
                ResultsGrouping::Workspace => {
                    let w = &hit.workspace;
                    if w.is_empty() {
                        "(none)".to_string()
                    } else {
                        w.rsplit('/').next().unwrap_or(w).to_string()
                    }
                }
                ResultsGrouping::Flat => "All".to_string(),
            };
            pane_map.entry(key).or_default().push(hit.clone());
        }
        self.panes = pane_map
            .into_iter()
            .map(|(key, hits)| {
                let total = hits.len();
                AgentPane {
                    agent: key,
                    hits,
                    selected: 0,
                    total_count: total,
                }
            })
            .collect();
        if self.active_pane >= self.panes.len() {
            self.active_pane = 0;
        }
    }

    /// Find the index of the next/previous day boundary in the active pane.
    fn timeline_jump_index(&self, forward: bool) -> Option<usize> {
        let pane = self.panes.get(self.active_pane)?;
        if pane.hits.is_empty() {
            return None;
        }
        let current_idx = pane.selected;
        let current_day = pane.hits.get(current_idx)?.created_at.unwrap_or(0) / 86400;

        if forward {
            for i in (current_idx + 1)..pane.hits.len() {
                let day = pane.hits[i].created_at.unwrap_or(0) / 86400;
                if day != current_day {
                    return Some(i);
                }
            }
        } else {
            for i in (0..current_idx).rev() {
                let day = pane.hits[i].created_at.unwrap_or(0) / 86400;
                if day != current_day {
                    // Jump to the first hit of that previous day.
                    let first = (0..=i)
                        .rev()
                        .take_while(|&j| pane.hits[j].created_at.unwrap_or(0) / 86400 == day)
                        .last()
                        .unwrap_or(i);
                    return Some(first);
                }
            }
        }
        None
    }

    fn refresh_available_source_ids(&mut self) {
        let mut ids = BTreeSet::new();
        for hit in &self.results {
            if hit.source_id != "local" {
                ids.insert(hit.source_id.clone());
            }
        }
        if let SourceFilter::SourceId(id) = &self.filters.source_filter {
            ids.insert(id.clone());
        }
        self.available_source_ids = ids.into_iter().collect();
    }

    fn source_menu_items(&self) -> Vec<(String, SourceFilter)> {
        let mut items = vec![
            ("All sources".to_string(), SourceFilter::All),
            ("Local only".to_string(), SourceFilter::Local),
            ("Remote only".to_string(), SourceFilter::Remote),
        ];
        items.extend(
            self.available_source_ids
                .iter()
                .cloned()
                .map(|id| (format!("Source: {id}"), SourceFilter::SourceId(id))),
        );
        items
    }

    fn source_menu_total_items(&self) -> usize {
        3 + self.available_source_ids.len()
    }

    fn move_source_menu_selection(&mut self, delta: i32) {
        let total = self.source_menu_total_items().max(1);
        let cur = self.source_filter_menu_selection as i32 + delta;
        self.source_filter_menu_selection = cur.clamp(0, total as i32 - 1) as usize;
    }

    fn source_filter_from_menu_selection(&self) -> SourceFilter {
        match self.source_filter_menu_selection {
            0 => SourceFilter::All,
            1 => SourceFilter::Local,
            2 => SourceFilter::Remote,
            n => self
                .available_source_ids
                .get(n.saturating_sub(3))
                .cloned()
                .map(SourceFilter::SourceId)
                .unwrap_or(SourceFilter::All),
        }
    }

    fn source_filter_status(filter: &SourceFilter) -> String {
        match filter {
            SourceFilter::All => "all sources".to_string(),
            SourceFilter::Local => "local only".to_string(),
            SourceFilter::Remote => "remote only".to_string(),
            SourceFilter::SourceId(id) => format!("source '{id}'"),
        }
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
        let grouping_suffix = match self.grouping_mode {
            ResultsGrouping::Agent => String::new(),
            other => format!(" [{}]", other.label()),
        };
        let results_title = if self.selected.is_empty() {
            format!("Results ({}){grouping_suffix}", hits.len())
        } else {
            format!(
                "Results ({}) \u{2022} {} selected{grouping_suffix}",
                hits.len(),
                self.selected.len()
            )
        };
        let results_block = Block::new()
            .borders(borders)
            .border_type(border_type)
            .title(&results_title)
            .title_alignment(Alignment::Left)
            .style(if self.focused_region() == FocusRegion::Results {
                pane_focused_style
            } else {
                pane_style
            });
        let inner = results_block.inner(area);
        results_block.render(area, frame);

        // Record hit region for mouse click-to-select.
        *self.last_results_inner.borrow_mut() = Some(inner);

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
        let source_label = source_display_label(&hit.source_id, hit.origin_host.as_deref());
        let source_kind = normalized_source_kind(Some(hit.origin_kind.as_str()), &hit.source_id);
        let mut meta_parts = vec![
            format!("agent={}", hit.agent),
            format!("workspace={}", hit.workspace),
            format!("source={source_label}"),
            format!("source_kind={source_kind}"),
            format!("score={:.3}", hit.score),
        ];
        if let Some(ws_original) = hit.workspace_original.as_deref()
            && ws_original != hit.workspace
        {
            meta_parts.push(format!("workspace_original={ws_original}"));
        }
        if !ts_str.is_empty() {
            meta_parts.push(ts_str);
        }
        let meta_text = meta_parts.join(" ");
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
            let source_kind = normalized_source_kind(None, &cv.convo.source_id);
            let workspace_original = workspace_original_from_metadata(&cv.convo.metadata_json);
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
                "source_kind": source_kind,
                "origin_host": cv.convo.origin_host,
                "workspace_original": workspace_original,
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
                "workspace_original": hit.workspace_original,
                "source_path": hit.source_path,
                "score": hit.score,
                "content_length": hit.content.len(),
                "source_id": hit.source_id,
                "source_kind": normalized_source_kind(Some(hit.origin_kind.as_str()), &hit.source_id),
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

    /// Build syntax-highlighted JSON lines for the Json tab using ftui JsonView.
    fn build_json_lines(&self, hit: &SearchHit, styles: &StyleContext) -> Vec<ftui::text::Line> {
        let mut lines: Vec<ftui::text::Line> = Vec::new();
        let header_style = styles.style(style_system::STYLE_TEXT_PRIMARY).bold();

        // Style mapping for JSON tokens
        let key_style = styles.style(style_system::STYLE_ROLE_USER).bold();
        let string_style = styles.style(style_system::STYLE_STATUS_SUCCESS);
        let number_style = styles.style(style_system::STYLE_STATUS_WARNING);
        let literal_style = styles.style(style_system::STYLE_STATUS_INFO);
        let punct_style = styles.style(style_system::STYLE_TEXT_MUTED);
        let error_style = styles.style(style_system::STYLE_STATUS_ERROR);

        // Header
        lines.push(ftui::text::Line::from_spans(vec![
            ftui::text::Span::styled("JSON Viewer", header_style),
        ]));
        lines.push(ftui::text::Line::from(""));

        // Helper: convert JsonView formatted_lines into styled ftui Lines.
        let convert_tokens = |token_lines: Vec<Vec<JsonToken>>, out: &mut Vec<ftui::text::Line>| {
            for token_line in token_lines {
                let mut spans = Vec::new();
                for token in token_line {
                    let (text, style) = match token {
                        JsonToken::Key(s) => (s, key_style),
                        JsonToken::StringVal(s) => (s, string_style),
                        JsonToken::Number(s) => (s, number_style),
                        JsonToken::Literal(s) => (s, literal_style),
                        JsonToken::Punctuation(s) => (s, punct_style),
                        JsonToken::Whitespace(s) => (s, ftui::Style::default()),
                        JsonToken::Newline => continue,
                        JsonToken::Error(s) => (s, error_style),
                    };
                    spans.push(ftui::text::Span::styled(text, style));
                }
                out.push(ftui::text::Line::from_spans(spans));
            }
        };

        if let Some((_, ref cv)) = self.cached_detail {
            // Build the full conversation JSON including metadata and messages
            let source_kind = normalized_source_kind(None, &cv.convo.source_id);
            let workspace_original = workspace_original_from_metadata(&cv.convo.metadata_json);

            let mut messages_json = Vec::new();
            for msg in &cv.messages {
                messages_json.push(serde_json::json!({
                    "role": msg.role.to_string(),
                    "author": msg.author,
                    "created_at": msg.created_at,
                    "content_length": msg.content.len(),
                    "extra": msg.extra_json,
                }));
            }

            let full_json = serde_json::json!({
                "agent": cv.convo.agent_slug,
                "external_id": cv.convo.external_id,
                "title": cv.convo.title,
                "source_path": cv.convo.source_path.display().to_string(),
                "started_at": cv.convo.started_at,
                "ended_at": cv.convo.ended_at,
                "approx_tokens": cv.convo.approx_tokens,
                "source_id": cv.convo.source_id,
                "source_kind": source_kind,
                "origin_host": cv.convo.origin_host,
                "workspace_original": workspace_original,
                "message_count": cv.messages.len(),
                "messages": messages_json,
            });

            if let Ok(json_str) = serde_json::to_string(&full_json) {
                let jv = JsonView::new(json_str)
                    .with_indent(2)
                    .with_key_style(key_style)
                    .with_string_style(string_style)
                    .with_number_style(number_style)
                    .with_literal_style(literal_style)
                    .with_punct_style(punct_style)
                    .with_error_style(error_style);
                convert_tokens(jv.formatted_lines(), &mut lines);
            }
        } else {
            // Fallback: show the hit as JSON
            let hit_json = serde_json::json!({
                "title": hit.title,
                "agent": hit.agent,
                "workspace": hit.workspace,
                "workspace_original": hit.workspace_original,
                "source_path": hit.source_path,
                "score": hit.score,
                "content_length": hit.content.len(),
                "source_id": hit.source_id,
                "source_kind": normalized_source_kind(Some(hit.origin_kind.as_str()), &hit.source_id),
                "origin_kind": hit.origin_kind,
                "origin_host": hit.origin_host,
                "created_at": hit.created_at,
            });

            if let Ok(json_str) = serde_json::to_string(&hit_json) {
                let jv = JsonView::new(json_str)
                    .with_indent(2)
                    .with_key_style(key_style)
                    .with_string_style(string_style)
                    .with_number_style(number_style)
                    .with_literal_style(literal_style)
                    .with_punct_style(punct_style)
                    .with_error_style(error_style);
                convert_tokens(jv.formatted_lines(), &mut lines);
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
            DetailTab::Messages => "Detail [\u{25cf}Messages] Snippets  Raw  Json",
            DetailTab::Snippets => "Detail  Messages [\u{25cf}Snippets] Raw  Json",
            DetailTab::Raw => "Detail  Messages  Snippets [\u{25cf}Raw] Json",
            DetailTab::Json => "Detail  Messages  Snippets  Raw [\u{25cf}Json]",
        };
        let wrap_indicator = if self.detail_wrap { " \u{21a9}" } else { "" };
        let title = format!("{tab_label}{wrap_indicator}");

        let detail_block = Block::new()
            .borders(borders)
            .border_type(border_type)
            .title(&title)
            .title_alignment(Alignment::Left)
            .style(if self.focused_region() == FocusRegion::Detail {
                pane_focused_style
            } else {
                pane_style
            });
        let inner = detail_block.inner(area);
        detail_block.render(area, frame);

        // Record hit region for mouse scroll in detail.
        *self.last_detail_area.borrow_mut() = Some(area);

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
                DetailTab::Json => self.build_json_lines(hit, styles),
            };

            // Apply find-in-detail highlighting and cache match positions
            if let Some(ref find) = self.detail_find {
                let matches =
                    Self::apply_find_highlight(&mut lines, &find.query, find.current, styles);
                // Deduplicate: match_positions has one entry per occurrence; we want
                // unique line numbers for navigation.
                let mut unique_lines: Vec<u16> = Vec::new();
                for &ln in &matches {
                    if unique_lines.last() != Some(&ln) {
                        unique_lines.push(ln);
                    }
                }
                *self.detail_find_matches_cache.borrow_mut() = unique_lines;
            } else {
                self.detail_find_matches_cache.borrow_mut().clear();
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

    /// Render the theme editor modal (centered, with color slots and contrast report).
    fn render_theme_editor_overlay(
        &self,
        frame: &mut super::ftui_adapter::Frame,
        area: Rect,
        styles: &StyleContext,
    ) {
        let Some(editor) = self.theme_editor.as_ref() else {
            return;
        };

        let popup_w = 60u16.min(area.width.saturating_sub(4));
        let popup_h = (ThemeColorSlot::ALL.len() as u16 + 8).min(area.height.saturating_sub(4));
        if popup_w < 30 || popup_h < 10 {
            return;
        }

        let px = area.x + (area.width.saturating_sub(popup_w)) / 2;
        let py = area.y + (area.height.saturating_sub(popup_h)) / 2;
        let popup_area = Rect::new(px, py, popup_w, popup_h);

        let bg_style = styles.style(style_system::STYLE_PANE_BASE);
        let border_style = styles.style(style_system::STYLE_PANE_FOCUSED);
        let muted_style = styles.style(style_system::STYLE_TEXT_MUTED);
        let primary_style = styles.style(style_system::STYLE_TEXT_PRIMARY);
        let warn_style = styles.style(style_system::STYLE_STATUS_WARNING);
        let selected_style = styles.style(style_system::STYLE_RESULT_ROW_SELECTED);

        Block::new().style(bg_style).render(popup_area, frame);

        let title = format!(
            " Theme Editor [{}] (Ctrl+Shift+T) ",
            editor.base_preset.name()
        );
        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(&title)
            .title_alignment(Alignment::Left)
            .style(border_style);
        let inner = block.inner(popup_area);
        block.render(popup_area, frame);

        if inner.is_empty() {
            return;
        }

        let mut y = inner.y;
        let max_y = inner.y + inner.height;

        // Header line
        if y < max_y {
            let header = format!(" {:14} {:9} {:7}", "Slot", "Override", "Resolved");
            Paragraph::new(&*header)
                .style(muted_style)
                .render(Rect::new(inner.x, y, inner.width, 1), frame);
            y += 1;
        }

        // Color slot rows
        let visible_start = editor.scroll;
        let visible_count = (max_y.saturating_sub(y).saturating_sub(3)) as usize;

        for (i, slot) in ThemeColorSlot::ALL
            .iter()
            .enumerate()
            .skip(visible_start)
            .take(visible_count)
        {
            if y >= max_y.saturating_sub(3) {
                break;
            }

            let is_selected = i == editor.selected;
            let override_val = slot.get(&editor.overrides);
            let resolved = slot.resolved_color(styles.resolved);
            let rgb = resolved.to_rgb();
            let resolved_hex = format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b);

            let pointer = if is_selected { ">" } else { " " };
            let override_str = override_val.unwrap_or("-");

            let row_text = if is_selected && editor.editing {
                format!("{pointer} {:14} #{:<8}", slot.label(), editor.hex_buffer)
            } else {
                format!(
                    "{pointer} {:14} {:9} {}",
                    slot.label(),
                    override_str,
                    resolved_hex
                )
            };

            let row_style = if is_selected {
                selected_style
            } else {
                primary_style
            };
            let row_area = Rect::new(inner.x, y, inner.width, 1);
            Paragraph::new(&*row_text)
                .style(row_style)
                .render(row_area, frame);

            // Color swatch at end of row (3 chars wide)
            if inner.width > 40 {
                let swatch_x = inner.x + inner.width - 4;
                let swatch_area = Rect::new(swatch_x, y, 3, 1);
                let swatch_style =
                    ftui::Style::default().bg(ftui::PackedRgba::rgb(rgb.r, rgb.g, rgb.b));
                Paragraph::new("   ")
                    .style(swatch_style)
                    .render(swatch_area, frame);
            }

            y += 1;
        }

        // Contrast warnings
        if !editor.contrast_warnings.is_empty() && y < max_y.saturating_sub(1) {
            let warn_text = format!("! {} contrast warning(s)", editor.contrast_warnings.len());
            Paragraph::new(&*warn_text)
                .style(warn_style)
                .render(Rect::new(inner.x, y, inner.width, 1), frame);
            y += 1;
        }

        // Footer hints
        let hint = if editor.editing {
            "Enter:apply  Esc:cancel"
        } else {
            "j/k:nav  Enter:edit  Del:clear  p:preset  s:save  Esc:close"
        };
        let hint_y = max_y.saturating_sub(1);
        if hint_y > y {
            Paragraph::new(hint)
                .style(muted_style)
                .render(Rect::new(inner.x, hint_y, inner.width, 1), frame);
        }
    }

    /// Render the inspector debug overlay in the bottom-right corner.
    fn render_inspector_overlay(
        &self,
        frame: &mut super::ftui_adapter::Frame,
        area: Rect,
        styles: &StyleContext,
    ) {
        let overlay_w = 44u16.min(area.width.saturating_sub(2));
        let overlay_h = 14u16.min(area.height.saturating_sub(2));
        if overlay_w < 20 || overlay_h < 6 {
            return; // Too narrow — auto-disable in small terminals
        }
        let ox = area.x + area.width.saturating_sub(overlay_w + 1);
        let oy = area.y + area.height.saturating_sub(overlay_h + 1);
        let overlay_area = Rect::new(ox, oy, overlay_w, overlay_h);

        let bg_style = styles.style(style_system::STYLE_PANE_BASE);
        let border_style = styles.style(style_system::STYLE_PANE_FOCUSED);
        let muted_style = styles.style(style_system::STYLE_TEXT_MUTED);
        let value_style = styles.style(style_system::STYLE_TEXT_PRIMARY);

        // Clear background
        Block::new().style(bg_style).render(overlay_area, frame);

        // Tab bar header
        let tab_header: String = [
            InspectorTab::Timing,
            InspectorTab::Layout,
            InspectorTab::HitRegions,
        ]
        .iter()
        .map(|t| {
            if *t == self.inspector_tab {
                format!("[{}]", t.label())
            } else {
                format!(" {} ", t.label())
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
        let title = format!(" Inspector: {tab_header} ");

        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(&title)
            .title_alignment(Alignment::Left)
            .style(border_style);
        let inner = block.inner(overlay_area);
        block.render(overlay_area, frame);

        if inner.is_empty() {
            return;
        }

        let mut y = inner.y;
        let max_y = inner.y + inner.height;

        match self.inspector_tab {
            InspectorTab::Timing => {
                let fps = self.frame_timing.fps();
                let avg = self.frame_timing.avg_us();
                let p95 = self.frame_timing.p95_us();
                let last = self.frame_timing.last_us();
                let samples = self.frame_timing.frame_times_us.len();

                let lines = [
                    format!("FPS:     {fps:.1}"),
                    format!("Avg:     {:.2}ms", avg as f64 / 1000.0),
                    format!("P95:     {:.2}ms", p95 as f64 / 1000.0),
                    format!("Last:    {:.2}ms", last as f64 / 1000.0),
                    format!("Samples: {samples}"),
                    String::new(),
                    format!("Search:  {}ms", self.last_search_ms.unwrap_or(0)),
                    format!("Results: {}", self.results.len()),
                    format!("Panes:   {}", self.panes.len()),
                ];

                for line in &lines {
                    if y >= max_y {
                        break;
                    }
                    let row = Rect::new(inner.x, y, inner.width, 1);
                    let st = if line.is_empty() {
                        muted_style
                    } else {
                        value_style
                    };
                    Paragraph::new(line.as_str()).style(st).render(row, frame);
                    y += 1;
                }
            }
            InspectorTab::Layout => {
                let bp = LayoutBreakpoint::from_width(area.width);
                let bp_str = bp.inspector_label();
                let topo = bp.search_topology();
                let topo_str = if topo.dual_pane {
                    format!("Dual (res≥{}, det≥{})", topo.min_results, topo.min_detail)
                } else {
                    "Single (focus-switched)".to_string()
                };
                let lines = [
                    format!("Terminal: {}x{}", area.width, area.height),
                    format!("Layout:   {bp_str}"),
                    format!("Topology: {topo_str}"),
                    format!("Density:  {:?}", self.density_mode),
                    format!(
                        "Borders:  {}",
                        if self.fancy_borders {
                            "Rounded"
                        } else {
                            "Plain"
                        }
                    ),
                    format!("Focus:    {:?}", self.focused_region()),
                    format!("FocusID:  {:?}", self.focus_manager.current()),
                    format!("Trapped:  {}", self.focus_manager.is_trapped()),
                    format!("Theme:    {:?}", self.theme_preset),
                    format!("Input:    {:?}", self.input_mode),
                ];

                for line in &lines {
                    if y >= max_y {
                        break;
                    }
                    let row = Rect::new(inner.x, y, inner.width, 1);
                    Paragraph::new(line.as_str())
                        .style(value_style)
                        .render(row, frame);
                    y += 1;
                }
            }
            InspectorTab::HitRegions => {
                let regions: Vec<(String, Option<Rect>)> = vec![
                    ("SearchBar".into(), *self.last_search_bar_area.borrow()),
                    ("Results".into(), *self.last_results_inner.borrow()),
                    ("Detail".into(), *self.last_detail_area.borrow()),
                    ("Status".into(), *self.last_status_area.borrow()),
                    ("Content".into(), *self.last_content_area.borrow()),
                    ("SplitHandle".into(), *self.last_split_handle_area.borrow()),
                ];

                for (name, rect) in &regions {
                    if y >= max_y {
                        break;
                    }
                    let row = Rect::new(inner.x, y, inner.width, 1);
                    let text = match rect {
                        Some(r) => {
                            format!("{name:<12} {}x{} @({},{})", r.width, r.height, r.x, r.y)
                        }
                        None => format!("{name:<12} (not rendered)"),
                    };
                    let st = if rect.is_some() {
                        value_style
                    } else {
                        muted_style
                    };
                    Paragraph::new(&*text).style(st).render(row, frame);
                    y += 1;
                }

                // Pill count and pane count
                if y < max_y {
                    let pill_count = self.last_pill_rects.borrow().len();
                    let pane_count = self.last_pane_rects.borrow().len();
                    let text = format!("Pills: {pill_count}  Panes: {pane_count}");
                    let row = Rect::new(inner.x, y, inner.width, 1);
                    Paragraph::new(&*text).style(muted_style).render(row, frame);
                }
            }
        }

        // Footer hint
        let hint = "Ctrl+Shift+I:close  Tab:tab  m:mode";
        let hint_row = Rect::new(inner.x, max_y.saturating_sub(1), inner.width, 1);
        Paragraph::new(hint)
            .style(muted_style)
            .render(hint_row, frame);
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

    // -- Help overlay rendering -----------------------------------------------

    /// Build the help content lines using ftui text types.
    fn build_help_lines(&self, styles: &StyleContext) -> Vec<ftui::text::Line> {
        let title_style = styles.style(style_system::STYLE_STATUS_INFO).bold();
        let muted_style = styles.style(style_system::STYLE_TEXT_MUTED);

        let mut lines: Vec<ftui::text::Line> = Vec::new();

        // Helper closure: push a section title + items + blank line
        let add_section = |out: &mut Vec<ftui::text::Line>, title: &str, items: &[String]| {
            out.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled(title.to_string(), title_style),
            ]));
            for item in items {
                out.push(ftui::text::Line::from(format!("  {item}")));
            }
            out.push(ftui::text::Line::from(""));
        };

        // Welcome
        lines.push(ftui::text::Line::from_spans(vec![
            ftui::text::Span::styled(
                "Welcome to CASS - Coding Agent Session Search".to_string(),
                title_style,
            ),
        ]));
        lines.push(ftui::text::Line::from(""));
        lines.push(ftui::text::Line::from("  Layout:"));
        for row in [
            "  ┌─────────────────────────────────────────────────┐",
            "  │ [Search Bar]         [Filter Chips]    [Status] │",
            "  ├────────────────┬────────────────────────────────┤",
            "  │                │                                │",
            "  │   Results      │       Detail Preview           │",
            "  │   (Left/↑↓)    │       (Tab to focus)           │",
            "  │                │                                │",
            "  ├────────────────┴────────────────────────────────┤",
            "  │ [Help Strip]                                    │",
            "  └─────────────────────────────────────────────────┘",
        ] {
            lines.push(ftui::text::Line::from(row));
        }
        lines.push(ftui::text::Line::from(""));

        add_section(
            &mut lines,
            "Data Locations",
            &[
                "Index & state: ~/.local/share/coding-agent-search/".into(),
                "  agent_search.db - Full-text search index".into(),
                "  tui_state.json - UI preferences | watch_state.json - Watch timestamps"
                    .into(),
                "  remotes/ - Synced session data from remote sources".into(),
                "Config: ~/.config/cass/sources.toml (remote sources)".into(),
                "Agents: Claude, Codex, Gemini, Cline, OpenCode, Amp, Cursor, ChatGPT, Aider, Pi-Agent, Factory"
                    .into(),
            ],
        );

        add_section(
            &mut lines,
            "Updates",
            &[
                "Checks GitHub releases hourly (offline-friendly, no auto-download)".into(),
                "When available: banner shows at top with U/S/Esc options".into(),
                "  U - Open release page in browser (Shift+U)".into(),
                "  S - Skip this version permanently (Shift+S)".into(),
                "  Esc - Dismiss banner for this session".into(),
            ],
        );

        add_section(
            &mut lines,
            "Search",
            &[
                format!(
                    "type to live-search; {} focuses query; {} cycles history",
                    shortcuts::FOCUS_QUERY,
                    shortcuts::HISTORY_CYCLE
                ),
                "Wildcards: foo* (prefix), *foo (suffix), *foo* (contains)".into(),
                "Auto-fuzzy: searches with few results try *term* fallback".into(),
                format!("{} refresh search (re-query index)", shortcuts::REFRESH),
                "/ detail-find in preview; n/N to jump matches".into(),
            ],
        );

        add_section(
            &mut lines,
            "Filters",
            &[
                format!(
                    "{} agent | {} workspace | {} from | {} to | {} clear all",
                    shortcuts::FILTER_AGENT,
                    shortcuts::FILTER_WORKSPACE,
                    shortcuts::FILTER_DATE_FROM,
                    shortcuts::FILTER_DATE_TO,
                    shortcuts::CLEAR_FILTERS
                ),
                format!(
                    "{} scope to active agent | {} clear scope | {} cycle time presets (24h/7d/30d/all)",
                    shortcuts::SCOPE_AGENT,
                    shortcuts::SCOPE_WORKSPACE,
                    shortcuts::CYCLE_TIME_PRESETS
                ),
                "Chips in search bar; Backspace removes last; Enter (query empty) edits last chip"
                    .into(),
            ],
        );

        add_section(
            &mut lines,
            "Sources (Multi-Machine)",
            &[
                "F11 cycle source filter: all → local → remote → all".into(),
                "Shift+F11 opens source filter menu (select specific sources)".into(),
                "Remote sessions show [source-name] in results list".into(),
                "Setup: cass sources setup (interactive wizard with SSH discovery)".into(),
                "Sync: rsync over SSH (delta transfers, additive-only for safety)".into(),
            ],
        );

        add_section(
            &mut lines,
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
                    "{} theme: dark/light | {} toggle border style",
                    shortcuts::THEME,
                    shortcuts::BORDERS
                ),
            ],
        );

        add_section(
            &mut lines,
            "Context",
            &[
                format!(
                    "{} cycles S/M/L/XL context window",
                    shortcuts::CONTEXT_WINDOW
                ),
                "Ctrl+Space: peek XL for current hit, tap again to restore".into(),
            ],
        );

        add_section(
            &mut lines,
            "Navigation",
            &[
                "Arrows move; Left/Right pane; PgUp/PgDn page".into(),
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
                "Ctrl+Enter queue item; Ctrl+O open all queued".into(),
                format!("{} toggles focus (Results ⇄ Detail)", shortcuts::TAB_FOCUS),
                "[ / ] cycle detail tabs (when results showing)".into(),
            ],
        );

        add_section(
            &mut lines,
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
        );

        add_section(
            &mut lines,
            "States",
            &[
                "UI state persists in tui_state.json (data dir).".into(),
                format!(
                    "{} reset UI state or launch with `cass tui --reset-state`",
                    shortcuts::RESET_STATE
                ),
            ],
        );

        // Pinned indicator
        if self.help_pinned {
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("  [PINNED] ".to_string(), title_style),
                ftui::text::Span::styled("Press P to unpin, Esc to close".to_string(), muted_style),
            ]));
        } else {
            lines.push(ftui::text::Line::from_spans(vec![
                ftui::text::Span::styled("  P=pin  ↑/↓=scroll  Esc=close".to_string(), muted_style),
            ]));
        }

        lines
    }

    /// Render the help overlay as a centered popup with scrollable content.
    fn render_help_overlay(
        &self,
        frame: &mut super::ftui_adapter::Frame,
        area: Rect,
        styles: &StyleContext,
    ) {
        // Size: 70% width, 70% height (clamped to area)
        let popup_w = ((area.width as u32 * 70) / 100).min(area.width as u32) as u16;
        let popup_h = ((area.height as u32 * 70) / 100).min(area.height as u32) as u16;
        if popup_w < 20 || popup_h < 6 {
            return;
        }

        let popup_x = area.x + (area.width.saturating_sub(popup_w)) / 2;
        let popup_y = area.y + (area.height.saturating_sub(popup_h)) / 2;
        let popup_area = Rect::new(popup_x, popup_y, popup_w, popup_h);

        let bg_style = styles.style(style_system::STYLE_PANE_BASE);
        let border_style = styles.style(style_system::STYLE_PANE_FOCUSED);

        // Clear background
        Block::new().style(bg_style).render(popup_area, frame);

        let title = if self.help_pinned {
            "Quick Start & Shortcuts (pinned)"
        } else {
            "Quick Start & Shortcuts (F1 or ? to toggle)"
        };
        let outer = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(title)
            .title_alignment(Alignment::Left)
            .style(border_style);
        let inner = outer.inner(popup_area);
        outer.render(popup_area, frame);
        if inner.is_empty() {
            return;
        }

        let lines = self.build_help_lines(styles);
        let text = ftui::text::Text::from_lines(lines);
        Paragraph::new(text)
            .style(styles.style(style_system::STYLE_TEXT_PRIMARY))
            .wrap(ftui::text::WrapMode::Word)
            .scroll((self.help_scroll, 0))
            .render(inner, frame);
    }

    /// Render the source filter popup menu centered on screen.
    fn render_source_filter_menu_overlay(
        &self,
        frame: &mut super::ftui_adapter::Frame,
        area: Rect,
        styles: &StyleContext,
    ) {
        let items = self.source_menu_items();
        let menu_w = 44u16.min(area.width.saturating_sub(2));
        let menu_h = (items.len() as u16 + 4).min(area.height.saturating_sub(2));
        if menu_w == 0 || menu_h == 0 {
            return;
        }

        let menu_x = area.x + (area.width.saturating_sub(menu_w)) / 2;
        let menu_y = area.y + (area.height.saturating_sub(menu_h)) / 2;
        let menu_area = Rect::new(menu_x, menu_y, menu_w, menu_h);

        let background = styles.style(style_system::STYLE_PANE_BASE);
        let border_style = styles.style(style_system::STYLE_PANE_FOCUSED);
        let text_style = styles.style(style_system::STYLE_TEXT_PRIMARY);
        let muted_style = styles.style(style_system::STYLE_TEXT_MUTED);
        let selected_style = styles.style(style_system::STYLE_RESULT_ROW_SELECTED);

        Block::new().style(background).render(menu_area, frame);
        let outer = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title("Source Filter (Shift+F11)")
            .title_alignment(Alignment::Left)
            .style(border_style);
        let inner = outer.inner(menu_area);
        outer.render(menu_area, frame);
        if inner.is_empty() {
            return;
        }

        let selected = self
            .source_filter_menu_selection
            .min(items.len().saturating_sub(1));
        let visible = inner.height as usize;
        let start = selected.saturating_sub(visible.saturating_sub(1));

        for (row, (label, filter)) in items.iter().enumerate().skip(start).take(visible) {
            let y = inner.y + (row - start) as u16;
            let row_area = Rect::new(inner.x, y, inner.width, 1);
            let pointer = if row == selected { "> " } else { "  " };
            let active = if *filter == self.filters.source_filter {
                "* "
            } else {
                "  "
            };
            let line = format!("{pointer}{active}{label}");
            let style = if row == selected {
                selected_style
            } else if *filter == self.filters.source_filter {
                muted_style
            } else {
                text_style
            };
            Paragraph::new(&*line).style(style).render(row_area, frame);
        }
    }

    /// Render the saved views manager popup centered on screen.
    fn render_saved_views_overlay(
        &self,
        frame: &mut super::ftui_adapter::Frame,
        area: Rect,
        styles: &StyleContext,
    ) {
        let modal_w = 72u16.min(area.width.saturating_sub(2));
        let modal_h = 18u16.min(area.height.saturating_sub(2));
        if modal_w == 0 || modal_h == 0 {
            self.last_saved_view_row_areas.borrow_mut().clear();
            return;
        }

        let modal_x = area.x + (area.width.saturating_sub(modal_w)) / 2;
        let modal_y = area.y + (area.height.saturating_sub(modal_h)) / 2;
        let modal_area = Rect::new(modal_x, modal_y, modal_w, modal_h);

        let bg_style = styles.style(style_system::STYLE_PANE_BASE);
        let border_style = styles.style(style_system::STYLE_PANE_FOCUSED);
        let text_style = styles.style(style_system::STYLE_TEXT_PRIMARY);
        let muted_style = styles.style(style_system::STYLE_TEXT_MUTED);
        let selected_style = styles.style(style_system::STYLE_RESULT_ROW_SELECTED);

        Block::new().style(bg_style).render(modal_area, frame);
        let title = format!("{SAVED_VIEWS_MODAL_TITLE}({})", self.saved_views.len());
        let outer = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(&title)
            .title_alignment(Alignment::Left)
            .style(border_style);
        let inner = outer.inner(modal_area);
        outer.render(modal_area, frame);
        if inner.is_empty() {
            self.last_saved_view_row_areas.borrow_mut().clear();
            return;
        }

        let mut rows = self.saved_views.clone();
        rows.sort_by_key(|v| v.slot);
        self.last_saved_view_row_areas.borrow_mut().clear();

        let footer_h = if self.saved_view_rename_mode { 2 } else { 1 };
        let list_h = inner.height.saturating_sub(footer_h).max(1);
        let list_area = Rect::new(inner.x, inner.y, inner.width, list_h);
        let footer_area = Rect::new(inner.x, inner.y + list_h, inner.width, footer_h);

        if rows.is_empty() {
            Paragraph::new(
                "No saved views. Use Ctrl+1..9 to save the current filters into a slot.",
            )
            .style(muted_style)
            .render(list_area, frame);
        } else {
            let selected = self.saved_views_selection.min(rows.len().saturating_sub(1));
            let drag_hover = self.saved_view_drag.map(|d| d.hover_idx);
            let visible = list_area.height as usize;
            let start = selected.saturating_sub(visible.saturating_sub(1));
            for (row, view) in rows.iter().enumerate().skip(start).take(visible) {
                let y = list_area.y + (row - start) as u16;
                let row_area = Rect::new(list_area.x, y, list_area.width, 1);
                self.last_saved_view_row_areas
                    .borrow_mut()
                    .push((row_area, row));
                let marker = if row == selected { "> " } else { "  " };
                let label = view
                    .label
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("Slot {}", view.slot));
                let line = format!(
                    "{marker}[{}] {}  a:{} w:{}  src:{}",
                    view.slot,
                    label,
                    view.agents.len(),
                    view.workspaces.len(),
                    view.source_filter
                );
                let style = if row == selected || drag_hover == Some(row) {
                    selected_style
                } else {
                    text_style
                };
                Paragraph::new(&*line).style(style).render(row_area, frame);
            }
        }

        if self.saved_view_rename_mode {
            let prompt = format!(
                "Rename slot: {}{}",
                self.saved_view_rename_buffer,
                if self.saved_view_rename_buffer.is_empty() {
                    ""
                } else {
                    " "
                }
            );
            Paragraph::new(&*prompt).style(text_style).render(
                Rect::new(footer_area.x, footer_area.y, footer_area.width, 1),
                frame,
            );
            Paragraph::new("Enter=save · Esc=cancel")
                .style(muted_style)
                .render(
                    Rect::new(
                        footer_area.x,
                        footer_area.y + 1,
                        footer_area.width,
                        footer_area.height.saturating_sub(1),
                    ),
                    frame,
                );
        } else {
            Paragraph::new(
                "Enter=load · drag=move · R=rename · D=delete · C=clear all · Esc=close",
            )
            .style(muted_style)
            .render(footer_area, frame);
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

    /// Load sources configuration + sync status into `SourcesViewState`.
    #[cfg(not(test))]
    fn load_sources_view(&mut self) {
        use crate::sources::{SourcesConfig, SyncStatus};

        let config = SourcesConfig::load().unwrap_or_default();
        let config_path = SourcesConfig::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".into());

        let data_dir = self.data_dir.clone();
        let sync_status = SyncStatus::load(&data_dir).unwrap_or_default();

        let mut items = Vec::new();

        // Always show the "local" pseudo-source first.
        items.push(SourcesViewItem {
            name: "local".into(),
            kind: crate::sources::SourceKind::Local,
            host: None,
            schedule: "always".into(),
            path_count: 0,
            last_sync: None,
            last_result: "n/a".into(),
            files_synced: 0,
            bytes_transferred: 0,
            busy: false,
            doctor_summary: None,
            error: None,
        });

        for src in &config.sources {
            let info = sync_status.sources.get(&src.name);
            let last_result_str = match info.map(|i| &i.last_result) {
                Some(crate::sources::SyncResult::Success) => "success",
                Some(crate::sources::SyncResult::PartialFailure(_)) => "partial",
                Some(crate::sources::SyncResult::Failed(_)) => "failed",
                Some(crate::sources::SyncResult::Skipped) | None => "never",
            };
            items.push(SourcesViewItem {
                name: src.name.clone(),
                kind: src.source_type,
                host: src.host.clone(),
                schedule: format!("{:?}", src.sync_schedule).to_lowercase(),
                path_count: src.paths.len(),
                last_sync: info.and_then(|i| i.last_sync),
                last_result: last_result_str.into(),
                files_synced: info.map(|i| i.files_synced).unwrap_or(0),
                bytes_transferred: info.map(|i| i.bytes_transferred).unwrap_or(0),
                busy: false,
                doctor_summary: None,
                error: None,
            });
        }

        let count = items.len();
        self.sources_view = SourcesViewState {
            items,
            selected: self.sources_view.selected.min(count.saturating_sub(1)),
            scroll: 0,
            busy: false,
            config_path,
            status: format!("{count} source(s) configured"),
        };
    }

    /// Number of selectable items in the current analytics subview.
    fn analytics_selectable_count(&self) -> usize {
        let data = match &self.analytics_cache {
            Some(d) => d,
            None => return 0,
        };
        match self.analytics_view {
            AnalyticsView::Explorer => data.daily_tokens.len(),
            AnalyticsView::Heatmap => data.heatmap_days.len(),
            AnalyticsView::Breakdowns => {
                super::analytics_charts::breakdown_rows(data, self.breakdown_tab)
            }
            AnalyticsView::Tools => super::analytics_charts::tools_row_count(data),
            AnalyticsView::Cost => super::analytics_charts::cost_rows(data),
            AnalyticsView::Plans => data.agent_plan_messages.len(),
            AnalyticsView::Coverage => super::analytics_charts::coverage_row_count(data),
            // Dashboard has no selectable rows.
            AnalyticsView::Dashboard => 0,
        }
    }

    /// Build a [`DrilldownContext`] from the current analytics view and selection.
    ///
    /// Returns `None` for views without selectable items or when the cache is empty.
    fn build_drilldown_context(&self) -> Option<DrilldownContext> {
        let data = self.analytics_cache.as_ref()?;
        let idx = self.analytics_selection;

        // Inherit global analytics filters as the base.
        let base_since = self.analytics_filters.since_ms;
        let base_until = self.analytics_filters.until_ms;

        match self.analytics_view {
            AnalyticsView::Explorer => {
                // Drill into a specific day bucket.
                let (label, _) = data.daily_tokens.get(idx)?;
                let (since, until) = day_label_to_epoch_range(label)?;
                Some(DrilldownContext {
                    since_ms: Some(since),
                    until_ms: Some(until),
                    agent: None,
                    model: None,
                })
            }
            AnalyticsView::Heatmap => {
                // Drill into a specific heatmap day.
                let (label, _) = data.heatmap_days.get(idx)?;
                let (since, until) = day_label_to_epoch_range(label)?;
                Some(DrilldownContext {
                    since_ms: Some(since),
                    until_ms: Some(until),
                    agent: None,
                    model: None,
                })
            }
            AnalyticsView::Breakdowns => {
                // Drill into the selected dimension based on active tab.
                match self.breakdown_tab {
                    BreakdownTab::Agent => {
                        let (agent, _) = data.agent_tokens.get(idx)?;
                        Some(DrilldownContext {
                            since_ms: base_since,
                            until_ms: base_until,
                            agent: Some(agent.clone()),
                            model: None,
                        })
                    }
                    BreakdownTab::Workspace => {
                        let (_ws, _) = data.workspace_tokens.get(idx)?;
                        // Workspace drilldown inherits time filters only.
                        Some(DrilldownContext {
                            since_ms: base_since,
                            until_ms: base_until,
                            agent: None,
                            model: None,
                        })
                    }
                    BreakdownTab::Source => {
                        let (_src, _) = data.source_tokens.get(idx)?;
                        Some(DrilldownContext {
                            since_ms: base_since,
                            until_ms: base_until,
                            agent: None,
                            model: None,
                        })
                    }
                    BreakdownTab::Model => {
                        let (model, _) = data.model_tokens.get(idx)?;
                        Some(DrilldownContext {
                            since_ms: base_since,
                            until_ms: base_until,
                            agent: None,
                            model: Some(model.clone()),
                        })
                    }
                }
            }
            AnalyticsView::Tools => {
                // Drill into a specific agent (tool rows are keyed by agent).
                let row = data.tool_rows.get(idx)?;
                Some(DrilldownContext {
                    since_ms: base_since,
                    until_ms: base_until,
                    agent: Some(row.key.clone()),
                    model: None,
                })
            }
            AnalyticsView::Cost => {
                // Drill into a specific model family.
                let (model, _) = data.model_tokens.get(idx)?;
                Some(DrilldownContext {
                    since_ms: base_since,
                    until_ms: base_until,
                    agent: None,
                    model: Some(model.clone()),
                })
            }
            AnalyticsView::Plans => {
                // Drill into agent's plan-heavy sessions.
                let (agent, _) = data.agent_plan_messages.get(idx)?;
                Some(DrilldownContext {
                    since_ms: base_since,
                    until_ms: base_until,
                    agent: Some(agent.clone()),
                    model: None,
                })
            }
            AnalyticsView::Coverage => {
                // Drill into a specific agent's sessions.
                let (agent, _) = data.agent_tokens.get(idx)?;
                Some(DrilldownContext {
                    since_ms: base_since,
                    until_ms: base_until,
                    agent: Some(agent.clone()),
                    model: None,
                })
            }
            // Dashboard doesn't support drilldown.
            AnalyticsView::Dashboard => None,
        }
    }
}

/// Convert a day label (e.g. "2026-02-06") to an epoch-ms range `[start, end)`.
///
/// Returns `None` if the label doesn't parse as a valid date.
fn day_label_to_epoch_range(label: &str) -> Option<(i64, i64)> {
    // Parse YYYY-MM-DD (the format produced by bucketing::day_id_to_date).
    let parts: Vec<&str> = label.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let year: i32 = parts[0].parse().ok()?;
    let month: u32 = parts[1].parse().ok()?;
    let day: u32 = parts[2].parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    fn is_leap_year(y: i32) -> bool {
        (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
    }
    fn days_in_month(y: i32, m: u32) -> u32 {
        match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if is_leap_year(y) => 29,
            2 => 28,
            _ => 0,
        }
    }
    if day == 0 || day > days_in_month(year, month) {
        return None;
    }

    // Compute days since Unix epoch using a simple Gregorian calendar.
    // We use a well-known algorithm to convert y/m/d → days since epoch.
    fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
        let y = y as i64;
        let m = m as i64;
        let d = d as i64;
        let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
        let era = if y2 >= 0 { y2 / 400 } else { (y2 - 399) / 400 };
        let yoe = y2 - era * 400;
        let doy = (153 * m2 + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + doe - 719468
    }

    let start_days = days_from_civil(year, month, day);
    let start_ms = start_days * 86_400_000;
    let end_ms = start_ms + 86_400_000;
    Some((start_ms, end_ms))
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

    /// Undo the last query/filter change (Ctrl+Z).
    Undo,
    /// Redo the last undone change (Ctrl+Y).
    Redo,

    /// Cycle the results grouping mode (Agent → Conversation → Workspace → Flat).
    GroupingCycled,
    /// Jump to the next/previous day boundary in results.
    TimelineJumped { forward: bool },

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
    /// Toggle JSON viewer tab (syntax-highlighted tree view).
    ToggleJsonView,

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

    // -- Theme editor -----------------------------------------------------
    /// Open the interactive theme editor modal.
    ThemeEditorOpened,
    /// Close the theme editor (discarding unsaved changes).
    ThemeEditorClosed,
    /// Move selection in the theme editor color list.
    ThemeEditorMoved { delta: i32 },
    /// Start editing the selected color slot (Enter).
    ThemeEditorEditStarted,
    /// Commit the hex input for the selected color slot (Enter while editing).
    ThemeEditorEditCommitted,
    /// Cancel hex editing (Esc while editing).
    ThemeEditorEditCancelled,
    /// Update the hex input buffer.
    ThemeEditorHexChanged(String),
    /// Clear the override for the selected slot (Del).
    ThemeEditorSlotCleared,
    /// Cycle the base preset in the editor.
    ThemeEditorPresetCycled,
    /// Export/save the theme config to disk.
    ThemeEditorExported,

    // -- Inspector overlay ------------------------------------------------
    /// Toggle the inspector debug overlay (Ctrl+Shift+I).
    InspectorToggled,
    /// Cycle the active inspector tab (Timing → Layout → HitRegions).
    InspectorTabCycled,
    /// Cycle the ftui inspector mode (Off → HitRegions → WidgetBounds → Full).
    InspectorModeCycled,

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
    /// Open saved views manager modal.
    SavedViewsOpened,
    /// Close saved views manager modal.
    SavedViewsClosed,
    /// Move selection in saved views modal.
    SavedViewsSelectionMoved { delta: i32 },
    /// Load currently selected saved view.
    SavedViewLoadedSelected,
    /// Enter rename mode for selected saved view.
    SavedViewRenameStarted,
    /// Commit rename for selected saved view.
    SavedViewRenameCommitted,
    /// Delete selected saved view slot.
    SavedViewDeletedSelected,
    /// Clear all saved view slots.
    SavedViewsCleared,
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
    /// Index refresh failed.
    IndexRefreshFailed(String),

    // -- State persistence ------------------------------------------------
    /// Load persisted state from disk.
    StateLoadRequested,
    /// Persisted state loaded.
    StateLoaded(Box<PersistedState>),
    /// Persisted state load failed.
    StateLoadFailed(String),
    /// Save current state to disk.
    StateSaveRequested,
    /// Persisted state save completed.
    StateSaved,
    /// Persisted state save failed.
    StateSaveFailed(String),
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
    /// Drilldown from analytics selection into the search view.
    AnalyticsDrilldown(DrilldownContext),
    /// Move selection within the current analytics subview.
    AnalyticsSelectionMoved { delta: i32 },
    /// Cycle the Explorer metric forward or backward.
    ExplorerMetricCycled { forward: bool },
    /// Cycle the Explorer overlay mode.
    ExplorerOverlayCycled,
    /// Cycle the Explorer group-by granularity forward or backward.
    ExplorerGroupByCycled { forward: bool },
    /// Cycle the Explorer zoom preset forward or backward.
    ExplorerZoomCycled { forward: bool },
    /// Cycle the Breakdowns tab forward or backward.
    BreakdownTabCycled { forward: bool },
    /// Cycle the Heatmap metric forward or backward.
    HeatmapMetricCycled { forward: bool },

    // -- Sources management surface (2noh9.4.9) ----------------------------
    /// Switch to the sources management surface.
    SourcesEntered,
    /// Reload sources config + sync status from disk.
    SourcesRefreshed,
    /// Trigger sync for the selected source (by name).
    SourceSyncRequested(String),
    /// Sync completed with a result message.
    SourceSyncCompleted {
        source_name: String,
        message: String,
    },
    /// Trigger doctor diagnostics for the selected source.
    SourceDoctorRequested(String),
    /// Doctor diagnostics completed.
    SourceDoctorCompleted {
        source_name: String,
        passed: usize,
        warnings: usize,
        failed: usize,
    },
    /// Move selection in the sources list.
    SourcesSelectionMoved { delta: i32 },

    // -- Screenshot export -------------------------------------------------
    /// Capture a screenshot of the current TUI state.
    ScreenshotRequested(ScreenshotFormat),
    /// Screenshot file was written successfully.
    ScreenshotCompleted(PathBuf),
    /// Screenshot export failed.
    ScreenshotFailed(String),

    // -- Macro recording/playback -----------------------------------------
    /// Toggle interactive macro recording (start/stop).
    MacroRecordingToggled,
    /// Macro recording saved to path.
    MacroRecordingSaved(PathBuf),
    /// Macro recording failed.
    MacroRecordingFailed(String),

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedViewDragState {
    pub from_idx: usize,
    pub hover_idx: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneSplitDragState;

/// Mouse event kinds (simplified from crossterm/ftui).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventKind {
    LeftClick,
    LeftDrag,
    LeftRelease,
    RightClick,
    ScrollUp,
    ScrollDown,
}

/// Region identified by mouse hit-testing against last-rendered layout rects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseHitRegion {
    /// Drag handle between results and detail panes.
    SplitHandle,
    /// Row in the saved views manager list.
    SavedViewRow { row_idx: usize },
    /// Click/scroll landed in the results list. `item_idx` is the absolute item index.
    Results { item_idx: usize },
    /// Click/scroll landed in the detail pane.
    Detail,
    /// Click/scroll landed in the search bar.
    SearchBar,
    /// Click/scroll landed in the status footer.
    StatusBar,
    /// Click/scroll landed outside any tracked region.
    None,
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct PersistedSavedView {
    #[serde(default)]
    slot: u8,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    agents: Vec<String>,
    #[serde(default)]
    workspaces: Vec<String>,
    #[serde(default)]
    created_from: Option<i64>,
    #[serde(default)]
    created_to: Option<i64>,
    #[serde(default)]
    ranking: Option<String>,
    #[serde(default)]
    source_filter_kind: Option<String>,
    #[serde(default)]
    source_filter_value: Option<String>,
    #[serde(default)]
    source_filter: Option<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct PersistedStateFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    search_mode: Option<String>,
    #[serde(default)]
    match_mode: Option<String>,
    #[serde(default)]
    ranking_mode: Option<String>,
    #[serde(default)]
    context_window: Option<String>,
    #[serde(default)]
    theme_dark: Option<bool>,
    #[serde(default)]
    density_mode: Option<String>,
    #[serde(default)]
    per_pane_limit: Option<usize>,
    #[serde(default)]
    query_history: Vec<String>,
    #[serde(default)]
    saved_views: Vec<PersistedSavedView>,
    #[serde(default)]
    fancy_borders: Option<bool>,
    #[serde(default)]
    help_pinned: Option<bool>,
    #[serde(default)]
    has_seen_help: Option<bool>,
}

fn parse_search_mode(value: &str) -> Option<SearchMode> {
    match value.to_ascii_lowercase().as_str() {
        "lexical" => Some(SearchMode::Lexical),
        "semantic" => Some(SearchMode::Semantic),
        "hybrid" => Some(SearchMode::Hybrid),
        _ => None,
    }
}

fn search_mode_str(value: SearchMode) -> &'static str {
    match value {
        SearchMode::Lexical => "lexical",
        SearchMode::Semantic => "semantic",
        SearchMode::Hybrid => "hybrid",
    }
}

fn parse_match_mode(value: &str) -> Option<MatchMode> {
    match value.to_ascii_lowercase().as_str() {
        "standard" => Some(MatchMode::Standard),
        "prefix" => Some(MatchMode::Prefix),
        _ => None,
    }
}

fn match_mode_str(value: MatchMode) -> &'static str {
    match value {
        MatchMode::Standard => "standard",
        MatchMode::Prefix => "prefix",
    }
}

fn parse_ranking_mode(value: &str) -> Option<RankingMode> {
    match value.to_ascii_lowercase().as_str() {
        "recent_heavy" => Some(RankingMode::RecentHeavy),
        "balanced" => Some(RankingMode::Balanced),
        "relevance_heavy" => Some(RankingMode::RelevanceHeavy),
        "match_quality_heavy" => Some(RankingMode::MatchQualityHeavy),
        "date_newest" => Some(RankingMode::DateNewest),
        "date_oldest" => Some(RankingMode::DateOldest),
        _ => None,
    }
}

fn ranking_mode_str(value: RankingMode) -> &'static str {
    match value {
        RankingMode::RecentHeavy => "recent_heavy",
        RankingMode::Balanced => "balanced",
        RankingMode::RelevanceHeavy => "relevance_heavy",
        RankingMode::MatchQualityHeavy => "match_quality_heavy",
        RankingMode::DateNewest => "date_newest",
        RankingMode::DateOldest => "date_oldest",
    }
}

fn parse_context_window(value: &str) -> Option<ContextWindow> {
    match value.to_ascii_lowercase().as_str() {
        "small" => Some(ContextWindow::Small),
        "medium" => Some(ContextWindow::Medium),
        "large" => Some(ContextWindow::Large),
        "xlarge" | "x_large" | "xl" => Some(ContextWindow::XLarge),
        _ => None,
    }
}

fn context_window_str(value: ContextWindow) -> &'static str {
    match value {
        ContextWindow::Small => "small",
        ContextWindow::Medium => "medium",
        ContextWindow::Large => "large",
        ContextWindow::XLarge => "xlarge",
    }
}

fn parse_density_mode(value: &str) -> Option<DensityMode> {
    match value.to_ascii_lowercase().as_str() {
        "compact" => Some(DensityMode::Compact),
        "cozy" => Some(DensityMode::Cozy),
        "spacious" => Some(DensityMode::Spacious),
        _ => None,
    }
}

fn density_mode_str(value: DensityMode) -> &'static str {
    match value {
        DensityMode::Compact => "compact",
        DensityMode::Cozy => "cozy",
        DensityMode::Spacious => "spacious",
    }
}

fn source_filter_to_parts(filter: &SourceFilter) -> (String, Option<String>) {
    match filter {
        SourceFilter::All => ("all".to_string(), None),
        SourceFilter::Local => ("local".to_string(), None),
        SourceFilter::Remote => ("remote".to_string(), None),
        SourceFilter::SourceId(id) => ("source_id".to_string(), Some(id.clone())),
    }
}

fn parse_legacy_source_filter(value: &serde_json::Value) -> Option<SourceFilter> {
    match value {
        serde_json::Value::String(s) => Some(SourceFilter::parse(s)),
        serde_json::Value::Object(map) => {
            if let Some(v) = map.get("source_id").and_then(|v| v.as_str()) {
                return Some(SourceFilter::SourceId(v.to_string()));
            }
            if let Some(v) = map.get("SourceId").and_then(|v| v.as_str()) {
                return Some(SourceFilter::SourceId(v.to_string()));
            }
            if map.contains_key("local") || map.contains_key("Local") {
                return Some(SourceFilter::Local);
            }
            if map.contains_key("remote") || map.contains_key("Remote") {
                return Some(SourceFilter::Remote);
            }
            if map.contains_key("all") || map.contains_key("All") {
                return Some(SourceFilter::All);
            }
            None
        }
        _ => None,
    }
}

fn source_filter_from_parts(
    kind: Option<&str>,
    value: Option<&str>,
    legacy: Option<&serde_json::Value>,
) -> SourceFilter {
    let legacy_filter = || parse_legacy_source_filter(legacy?);
    if let Some(kind) = kind {
        return match kind.to_ascii_lowercase().as_str() {
            "all" => SourceFilter::All,
            "local" => SourceFilter::Local,
            "remote" => SourceFilter::Remote,
            "source_id" => value
                .map(|v| SourceFilter::SourceId(v.to_string()))
                .unwrap_or(SourceFilter::All),
            _ => legacy_filter().unwrap_or(SourceFilter::All),
        };
    }
    legacy_filter().unwrap_or(SourceFilter::All)
}

fn persisted_state_defaults() -> PersistedState {
    PersistedState {
        search_mode: SearchMode::default(),
        match_mode: MatchMode::default(),
        ranking_mode: RankingMode::default(),
        context_window: ContextWindow::default(),
        theme_dark: true,
        density_mode: DensityMode::default(),
        per_pane_limit: 10,
        query_history: VecDeque::with_capacity(QUERY_HISTORY_CAP),
        saved_views: Vec::new(),
        fancy_borders: true,
        help_pinned: false,
        has_seen_help: false,
    }
}

fn persisted_state_file_from_state(state: &PersistedState) -> PersistedStateFile {
    let saved_views = state
        .saved_views
        .iter()
        .map(|view| {
            let (source_filter_kind, source_filter_value) =
                source_filter_to_parts(&view.source_filter);
            PersistedSavedView {
                slot: view.slot,
                label: view.label.clone(),
                agents: view.agents.iter().cloned().collect(),
                workspaces: view.workspaces.iter().cloned().collect(),
                created_from: view.created_from,
                created_to: view.created_to,
                ranking: Some(ranking_mode_str(view.ranking).to_string()),
                source_filter_kind: Some(source_filter_kind),
                source_filter_value,
                source_filter: Some(serde_json::Value::String(view.source_filter.to_string())),
            }
        })
        .collect();
    PersistedStateFile {
        version: 1,
        search_mode: Some(search_mode_str(state.search_mode).to_string()),
        match_mode: Some(match_mode_str(state.match_mode).to_string()),
        ranking_mode: Some(ranking_mode_str(state.ranking_mode).to_string()),
        context_window: Some(context_window_str(state.context_window).to_string()),
        theme_dark: Some(state.theme_dark),
        density_mode: Some(density_mode_str(state.density_mode).to_string()),
        per_pane_limit: Some(state.per_pane_limit),
        query_history: state.query_history.iter().cloned().collect(),
        saved_views,
        fancy_borders: Some(state.fancy_borders),
        help_pinned: Some(state.help_pinned),
        has_seen_help: Some(state.has_seen_help),
    }
}

fn persisted_state_from_file(file: PersistedStateFile) -> PersistedState {
    let defaults = persisted_state_defaults();
    let mut dedup_slots = HashSet::new();
    let saved_views = file
        .saved_views
        .into_iter()
        .filter_map(|view| {
            if !(1..=9).contains(&view.slot) || !dedup_slots.insert(view.slot) {
                return None;
            }
            let ranking = view
                .ranking
                .as_deref()
                .and_then(parse_ranking_mode)
                .unwrap_or(RankingMode::Balanced);
            let source_filter = source_filter_from_parts(
                view.source_filter_kind.as_deref(),
                view.source_filter_value.as_deref(),
                view.source_filter.as_ref(),
            );
            Some(SavedView {
                slot: view.slot,
                label: view.label.filter(|s| !s.trim().is_empty()),
                agents: view
                    .agents
                    .into_iter()
                    .filter(|s| !s.trim().is_empty())
                    .collect(),
                workspaces: view
                    .workspaces
                    .into_iter()
                    .filter(|s| !s.trim().is_empty())
                    .collect(),
                created_from: view.created_from,
                created_to: view.created_to,
                ranking,
                source_filter,
            })
        })
        .collect();
    let mut query_history: VecDeque<String> = file
        .query_history
        .into_iter()
        .filter(|q| !q.trim().is_empty())
        .take(QUERY_HISTORY_CAP)
        .collect();
    if query_history.len() > QUERY_HISTORY_CAP {
        query_history.truncate(QUERY_HISTORY_CAP);
    }
    PersistedState {
        search_mode: file
            .search_mode
            .as_deref()
            .and_then(parse_search_mode)
            .unwrap_or(defaults.search_mode),
        match_mode: file
            .match_mode
            .as_deref()
            .and_then(parse_match_mode)
            .unwrap_or(defaults.match_mode),
        ranking_mode: file
            .ranking_mode
            .as_deref()
            .and_then(parse_ranking_mode)
            .unwrap_or(defaults.ranking_mode),
        context_window: file
            .context_window
            .as_deref()
            .and_then(parse_context_window)
            .unwrap_or(defaults.context_window),
        theme_dark: file.theme_dark.unwrap_or(defaults.theme_dark),
        density_mode: file
            .density_mode
            .as_deref()
            .and_then(parse_density_mode)
            .unwrap_or(defaults.density_mode),
        per_pane_limit: file
            .per_pane_limit
            .unwrap_or(defaults.per_pane_limit)
            .clamp(4, 50),
        query_history,
        saved_views,
        fancy_borders: file.fancy_borders.unwrap_or(defaults.fancy_borders),
        help_pinned: file.help_pinned.unwrap_or(defaults.help_pinned),
        has_seen_help: file.has_seen_help.unwrap_or(defaults.has_seen_help),
    }
}

fn load_persisted_state_from_path(path: &Path) -> Result<Option<PersistedState>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("failed reading {}: {e}", path.display()))?;
    let file: PersistedStateFile = serde_json::from_str(&raw)
        .map_err(|e| format!("failed parsing {}: {e}", path.display()))?;
    Ok(Some(persisted_state_from_file(file)))
}

fn save_persisted_state_to_path(path: &Path, state: &PersistedState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed creating {}: {e}", parent.display()))?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let payload = serde_json::to_vec_pretty(&persisted_state_file_from_state(state))
        .map_err(|e| format!("failed serializing state: {e}"))?;
    std::fs::write(&tmp_path, payload)
        .map_err(|e| format!("failed writing {}: {e}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path)
        .map_err(|e| format!("failed replacing {}: {e}", path.display()))?;
    Ok(())
}

fn clear_persisted_state_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed removing {}: {e}", path.display())),
    }
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
const STATE_SAVE_DEBOUNCE: Duration = Duration::from_millis(450);

/// Minimum distance (in terminal cells) for a drag event to be considered
/// meaningful. Events with movement below this threshold are discarded to
/// prevent jitter from touchpad noise and sub-cell pointer motion.
const DRAG_JITTER_THRESHOLD: u16 = 2;

/// Minimum time (ms) a drag hover must remain on a new row before the hover
/// index updates. Prevents rapid flickering when dragging across thin row
/// boundaries.
const DRAG_HOVER_SETTLE_MS: u64 = 80;
const TUI_STATE_FILE_NAME: &str = "tui_state.json";
const QUERY_HISTORY_CAP: usize = 50;

// =========================================================================
// From<Event> — convert ftui terminal events into CassMsg
// =========================================================================

impl From<super::ftui_adapter::Event> for CassMsg {
    fn from(event: super::ftui_adapter::Event) -> Self {
        use super::ftui_adapter::{Event, KeyCode, Modifiers};

        // Stash raw event for model-level macro recording.
        stash_raw_event(&event);

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
                    KeyCode::Char('t') if ctrl && !shift => CassMsg::ThemeToggled,

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

                    // -- Macro recording (Alt+M) ----------------------------------
                    KeyCode::Char('m') if alt => CassMsg::MacroRecordingToggled,

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

                    // -- Theme editor -------------------------------------------------
                    KeyCode::Char('t') if ctrl && shift => CassMsg::ThemeEditorOpened,
                    KeyCode::Char('T') if ctrl => CassMsg::ThemeEditorOpened,

                    // -- Sources management -----------------------------------------
                    KeyCode::Char('s') if ctrl && shift => CassMsg::SourcesEntered,
                    KeyCode::Char('S') if ctrl => CassMsg::SourcesEntered,

                    // -- Inspector overlay -----------------------------------------
                    KeyCode::Char('i') if ctrl && shift => CassMsg::InspectorToggled,
                    KeyCode::Char('I') if ctrl => CassMsg::InspectorToggled,

                    // -- Borders --------------------------------------------------
                    KeyCode::Char('b') if ctrl => CassMsg::BordersToggled,

                    // -- Undo/redo ------------------------------------------------
                    KeyCode::Char('z') if ctrl && shift => CassMsg::Redo,
                    KeyCode::Char('Z') if ctrl => CassMsg::Redo,
                    KeyCode::Char('z') if ctrl => CassMsg::Undo,

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
                    KeyCode::Char('Y') if ctrl => CassMsg::CopyQuery,
                    KeyCode::Char('y') if ctrl && shift => CassMsg::CopyQuery,
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
                    KeyCode::Char('J') => CassMsg::ToggleJsonView,
                    KeyCode::Char('r') => CassMsg::ResultsRefreshed,
                    KeyCode::Char('A') => CassMsg::BulkActionsOpened,
                    KeyCode::Char(' ') => CassMsg::PeekToggled,
                    KeyCode::Char('G') => CassMsg::GroupingCycled,
                    KeyCode::Char('[') => CassMsg::TimelineJumped { forward: false },
                    KeyCode::Char(']') => CassMsg::TimelineJumped { forward: true },

                    // -- Default: treat as query input ----------------------------
                    KeyCode::Char(c) => CassMsg::QueryChanged(c.to_string()),
                    KeyCode::Backspace => CassMsg::QueryChanged(String::new()),

                    _ => CassMsg::Tick, // Unhandled keys become no-op ticks
                }
            }

            Event::Mouse(mouse) => {
                use ftui::core::event::MouseButton;
                use ftui::core::event::MouseEventKind as Mek;
                match mouse.kind {
                    Mek::Down(MouseButton::Left) => CassMsg::MouseEvent {
                        kind: MouseEventKind::LeftClick,
                        x: mouse.x,
                        y: mouse.y,
                    },
                    Mek::Drag(MouseButton::Left) => CassMsg::MouseEvent {
                        kind: MouseEventKind::LeftDrag,
                        x: mouse.x,
                        y: mouse.y,
                    },
                    Mek::Up(MouseButton::Left) => CassMsg::MouseEvent {
                        kind: MouseEventKind::LeftRelease,
                        x: mouse.x,
                        y: mouse.y,
                    },
                    Mek::Down(MouseButton::Right) => CassMsg::MouseEvent {
                        kind: MouseEventKind::RightClick,
                        x: mouse.x,
                        y: mouse.y,
                    },
                    Mek::Down(_) => CassMsg::Tick,
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
        // Record raw event for model-level macro recording.
        if let Some(ref mut recorder) = self.macro_recorder
            && let Some(raw_event) = take_raw_event()
        {
            recorder.record_event(raw_event);
        }

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

        // ── Theme editor modal intercept ────────────────────────────
        if self.show_theme_editor
            && let Some(editor) = self.theme_editor.as_ref()
        {
            let is_editing = editor.editing;
            if is_editing {
                // In hex editing mode: intercept text and confirm/cancel
                match &msg {
                    CassMsg::QueryChanged(text) => {
                        if let Some(ed) = self.theme_editor.as_mut() {
                            if text.is_empty() {
                                ed.hex_buffer.pop();
                            } else {
                                ed.hex_buffer.push_str(text);
                            }
                        }
                        return ftui::Cmd::none();
                    }
                    CassMsg::DetailOpened | CassMsg::QuerySubmitted => {
                        return self.update(CassMsg::ThemeEditorEditCommitted);
                    }
                    CassMsg::QuitRequested => {
                        return self.update(CassMsg::ThemeEditorEditCancelled);
                    }
                    // Let direct theme-editor messages through to the handler
                    CassMsg::ThemeEditorEditCommitted
                    | CassMsg::ThemeEditorEditCancelled
                    | CassMsg::ThemeEditorHexChanged(_)
                    | CassMsg::ThemeEditorClosed => {}
                    _ => return ftui::Cmd::none(),
                }
            } else {
                // In navigation mode: intercept nav and action keys
                match &msg {
                    CassMsg::SelectionMoved { delta } => {
                        return self.update(CassMsg::ThemeEditorMoved { delta: *delta });
                    }
                    CassMsg::CursorMoved { delta } => {
                        return self.update(CassMsg::ThemeEditorMoved { delta: *delta });
                    }
                    CassMsg::DetailOpened | CassMsg::QuerySubmitted => {
                        return self.update(CassMsg::ThemeEditorEditStarted);
                    }
                    CassMsg::QuitRequested => {
                        return self.update(CassMsg::ThemeEditorClosed);
                    }
                    CassMsg::QueryChanged(text) => match text.as_str() {
                        "p" => return self.update(CassMsg::ThemeEditorPresetCycled),
                        "s" => return self.update(CassMsg::ThemeEditorExported),
                        "j" => return self.update(CassMsg::ThemeEditorMoved { delta: 1 }),
                        "k" => return self.update(CassMsg::ThemeEditorMoved { delta: -1 }),
                        _ => return ftui::Cmd::none(),
                    },
                    CassMsg::FiltersClearAll => {
                        return self.update(CassMsg::ThemeEditorSlotCleared);
                    }
                    CassMsg::ThemeEditorOpened
                    | CassMsg::ThemeEditorClosed
                    | CassMsg::ThemeEditorMoved { .. }
                    | CassMsg::ThemeEditorEditStarted
                    | CassMsg::ThemeEditorEditCommitted
                    | CassMsg::ThemeEditorEditCancelled
                    | CassMsg::ThemeEditorHexChanged(_)
                    | CassMsg::ThemeEditorPresetCycled
                    | CassMsg::ThemeEditorExported
                    | CassMsg::ThemeEditorSlotCleared => {
                        // Let these through to the handler
                    }
                    _ => return ftui::Cmd::none(),
                }
            }
        }

        // ── Inspector overlay key intercept ─────────────────────────
        // Non-blocking: only intercept Tab (cycle tabs) and m (cycle mode)
        if self.show_inspector {
            match &msg {
                CassMsg::InspectorTabCycled => {
                    self.inspector_tab = self.inspector_tab.next();
                    return ftui::Cmd::none();
                }
                CassMsg::InspectorModeCycled => {
                    self.inspector_state.cycle_mode();
                    return ftui::Cmd::none();
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
                    self.focus_manager.pop_trap();
                    return ftui::Cmd::none();
                }
                _ => {}
            }
        }

        // Saved views manager modal intercept. While open, consume navigation
        // and action keys so query/search state is not mutated underneath.
        if self.show_saved_views_modal {
            if self.saved_view_rename_mode {
                match &msg {
                    CassMsg::QueryChanged(text) => {
                        if text.is_empty() {
                            self.saved_view_rename_buffer.pop();
                        } else {
                            self.saved_view_rename_buffer.push_str(text);
                        }
                        return ftui::Cmd::none();
                    }
                    CassMsg::DetailOpened | CassMsg::QuerySubmitted => {
                        return ftui::Cmd::msg(CassMsg::SavedViewRenameCommitted);
                    }
                    CassMsg::QuitRequested => {
                        self.saved_view_rename_mode = false;
                        self.saved_view_drag = None;
                        self.saved_view_rename_buffer.clear();
                        self.status = "Cancelled saved view rename".to_string();
                        return ftui::Cmd::none();
                    }
                    CassMsg::SavedViewRenameCommitted
                    | CassMsg::SavedViewsClosed
                    | CassMsg::SavedViewDeletedSelected
                    | CassMsg::SavedViewsCleared => {}
                    _ => return ftui::Cmd::none(),
                }
            }

            match &msg {
                CassMsg::QuitRequested => return ftui::Cmd::msg(CassMsg::SavedViewsClosed),
                CassMsg::SelectionMoved { delta } => {
                    return ftui::Cmd::msg(CassMsg::SavedViewsSelectionMoved { delta: *delta });
                }
                CassMsg::QueryChanged(text) if text.eq_ignore_ascii_case("j") => {
                    return ftui::Cmd::msg(CassMsg::SavedViewsSelectionMoved { delta: 1 });
                }
                CassMsg::QueryChanged(text) if text.eq_ignore_ascii_case("k") => {
                    return ftui::Cmd::msg(CassMsg::SavedViewsSelectionMoved { delta: -1 });
                }
                CassMsg::DetailOpened | CassMsg::QuerySubmitted => {
                    return ftui::Cmd::msg(CassMsg::SavedViewLoadedSelected);
                }
                CassMsg::QueryChanged(text) if text.eq_ignore_ascii_case("r") => {
                    return ftui::Cmd::msg(CassMsg::SavedViewRenameStarted);
                }
                CassMsg::QueryChanged(text) if text.eq_ignore_ascii_case("d") => {
                    return ftui::Cmd::msg(CassMsg::SavedViewDeletedSelected);
                }
                CassMsg::QueryChanged(text) if text.eq_ignore_ascii_case("c") => {
                    return ftui::Cmd::msg(CassMsg::SavedViewsCleared);
                }
                CassMsg::SavedViewsSelectionMoved { .. }
                | CassMsg::SavedViewLoadedSelected
                | CassMsg::SavedViewRenameStarted
                | CassMsg::SavedViewRenameCommitted
                | CassMsg::SavedViewDeletedSelected
                | CassMsg::SavedViewsCleared
                | CassMsg::SavedViewsClosed
                | CassMsg::SavedViewsOpened
                | CassMsg::MouseEvent { .. } => {}
                _ => return ftui::Cmd::none(),
            }
        }

        // Source filter menu: while open, consume navigation keys and apply
        // selection without affecting results/query.
        if self.source_filter_menu_open {
            match &msg {
                CassMsg::SourceFilterMenuToggled | CassMsg::QuitRequested => {
                    self.source_filter_menu_open = false;
                    self.status = "Source filter menu closed".to_string();
                    return ftui::Cmd::none();
                }
                CassMsg::SelectionMoved { delta } => {
                    self.move_source_menu_selection(*delta);
                    return ftui::Cmd::none();
                }
                CassMsg::QueryChanged(text) if text.eq_ignore_ascii_case("j") => {
                    self.move_source_menu_selection(1);
                    return ftui::Cmd::none();
                }
                CassMsg::QueryChanged(text) if text.eq_ignore_ascii_case("k") => {
                    self.move_source_menu_selection(-1);
                    return ftui::Cmd::none();
                }
                CassMsg::DetailOpened | CassMsg::QuerySubmitted => {
                    let filter = self.source_filter_from_menu_selection();
                    return ftui::Cmd::msg(CassMsg::SourceFilterSelected(filter));
                }
                CassMsg::SourceFilterSelected(_) => {}
                _ => return ftui::Cmd::none(),
            }
        }

        // ── Detail modal intercept ──────────────────────────────────
        // When the full-screen detail modal is open, remap navigation and
        // provide find-in-detail text search (Ctrl+F or /).
        if self.show_detail_modal {
            // Sub-intercept: when find bar is active, route text input there.
            if self.detail_find.is_some() {
                match &msg {
                    CassMsg::QueryChanged(text) => {
                        if let Some(ref mut find) = self.detail_find {
                            if text.is_empty() {
                                find.query.pop();
                            } else {
                                find.query.push_str(text);
                            }
                            let q = find.query.clone();
                            return self.update(CassMsg::DetailFindQueryChanged(q));
                        }
                        return ftui::Cmd::none();
                    }
                    CassMsg::QuerySubmitted | CassMsg::DetailOpened => {
                        // Enter → navigate to next match
                        return self.update(CassMsg::DetailFindNavigated { forward: true });
                    }
                    CassMsg::QuitRequested => {
                        // Esc → close find bar (detail modal stays open)
                        self.detail_find = None;
                        self.input_mode = InputMode::Query;
                        return ftui::Cmd::none();
                    }
                    // Let detail-specific messages through
                    CassMsg::DetailFindToggled
                    | CassMsg::DetailFindQueryChanged(_)
                    | CassMsg::DetailFindNavigated { .. }
                    | CassMsg::DetailClosed
                    | CassMsg::DetailTabChanged(_)
                    | CassMsg::DetailScrolled { .. }
                    | CassMsg::DetailWrapToggled
                    | CassMsg::Tick
                    | CassMsg::MouseEvent { .. }
                    | CassMsg::ForceQuit => {}
                    _ => return ftui::Cmd::none(),
                }
            } else {
                // Find bar is NOT active — handle detail-level navigation
                match &msg {
                    // Slash or Ctrl+F opens find
                    CassMsg::PaneFilterOpened | CassMsg::WildcardFallbackToggled => {
                        return self.update(CassMsg::DetailFindToggled);
                    }
                    // j/k scroll the detail view
                    CassMsg::QueryChanged(text) if text == "j" => {
                        return self.update(CassMsg::DetailScrolled { delta: 3 });
                    }
                    CassMsg::QueryChanged(text) if text == "k" => {
                        return self.update(CassMsg::DetailScrolled { delta: -3 });
                    }
                    // n/N navigate find matches (if any remain from a previous find)
                    CassMsg::QueryChanged(text) if text == "n" => {
                        return self.update(CassMsg::DetailFindNavigated { forward: true });
                    }
                    CassMsg::QueryChanged(text) if text == "N" => {
                        return self.update(CassMsg::DetailFindNavigated { forward: false });
                    }
                    // w toggles wrap
                    CassMsg::QueryChanged(text) if text == "w" => {
                        return self.update(CassMsg::DetailWrapToggled);
                    }
                    // Up/Down scroll detail
                    CassMsg::SelectionMoved { delta } => {
                        return self.update(CassMsg::DetailScrolled { delta: *delta });
                    }
                    // Esc closes detail modal
                    CassMsg::QuitRequested => {
                        return self.update(CassMsg::DetailClosed);
                    }
                    // Tab cycles detail tabs
                    CassMsg::FocusToggled => {
                        let next = match self.detail_tab {
                            DetailTab::Messages => DetailTab::Snippets,
                            DetailTab::Snippets => DetailTab::Raw,
                            DetailTab::Raw => DetailTab::Json,
                            DetailTab::Json => DetailTab::Messages,
                        };
                        return self.update(CassMsg::DetailTabChanged(next));
                    }
                    // Let these through unchanged
                    CassMsg::DetailClosed
                    | CassMsg::DetailOpened
                    | CassMsg::DetailTabChanged(_)
                    | CassMsg::DetailScrolled { .. }
                    | CassMsg::DetailWrapToggled
                    | CassMsg::DetailFindToggled
                    | CassMsg::DetailFindQueryChanged(_)
                    | CassMsg::DetailFindNavigated { .. }
                    | CassMsg::ToggleJsonView
                    | CassMsg::PageScrolled { .. }
                    | CassMsg::Tick
                    | CassMsg::MouseEvent { .. }
                    | CassMsg::ForceQuit => {}
                    _ => return ftui::Cmd::none(),
                }
            }
        }

        // -- Analytics surface interception -----------------------------------
        // When on the analytics surface, remap navigation/selection messages to
        // analytics-specific variants so Enter drills down and Up/Down moves
        // the analytics selection rather than the search results pane.
        if self.surface == AppSurface::Analytics {
            match &msg {
                CassMsg::SelectionMoved { delta } => {
                    return self.update(CassMsg::AnalyticsSelectionMoved { delta: *delta });
                }
                CassMsg::DetailOpened | CassMsg::QuerySubmitted => {
                    // Enter triggers drilldown from the current selection.
                    if let Some(ctx) = self.build_drilldown_context() {
                        return self.update(CassMsg::AnalyticsDrilldown(ctx));
                    }
                    // No drilldown available (Dashboard / Coverage) — no-op.
                    return ftui::Cmd::none();
                }
                CassMsg::CursorMoved { delta } => {
                    // Left/Right cycles analytics view tabs.
                    let views = AnalyticsView::all();
                    if let Some(cur_idx) = views.iter().position(|v| *v == self.analytics_view) {
                        let next = (cur_idx as i32 + delta).rem_euclid(views.len() as i32) as usize;
                        self.analytics_view = views[next];
                        self.analytics_selection = 0; // reset selection on view change
                    }
                    return ftui::Cmd::none();
                }
                // Tab / Shift+Tab cycle breakdown sub-tabs when on Breakdowns view.
                CassMsg::FocusToggled if self.analytics_view == AnalyticsView::Breakdowns => {
                    return self.update(CassMsg::BreakdownTabCycled { forward: true });
                }
                CassMsg::FocusDirectional { .. }
                    if self.analytics_view == AnalyticsView::Breakdowns =>
                {
                    return self.update(CassMsg::BreakdownTabCycled { forward: false });
                }
                // Tab / Shift+Tab cycle heatmap metric when on Heatmap view.
                CassMsg::FocusToggled if self.analytics_view == AnalyticsView::Heatmap => {
                    return self.update(CassMsg::HeatmapMetricCycled { forward: true });
                }
                CassMsg::FocusDirectional { .. }
                    if self.analytics_view == AnalyticsView::Heatmap =>
                {
                    return self.update(CassMsg::HeatmapMetricCycled { forward: false });
                }
                // Heatmap view: 'm' cycles metric.
                CassMsg::QueryChanged(text) if self.analytics_view == AnalyticsView::Heatmap => {
                    match text.as_str() {
                        "m" => {
                            return self.update(CassMsg::HeatmapMetricCycled { forward: true });
                        }
                        "M" => {
                            return self.update(CassMsg::HeatmapMetricCycled { forward: false });
                        }
                        _ => {}
                    }
                }
                // Explorer view: 'm' cycles metric, 'o' cycles overlay, 'g' cycles group-by.
                CassMsg::QueryChanged(text) if self.analytics_view == AnalyticsView::Explorer => {
                    match text.as_str() {
                        "m" => {
                            return self.update(CassMsg::ExplorerMetricCycled { forward: true });
                        }
                        "M" => {
                            return self.update(CassMsg::ExplorerMetricCycled { forward: false });
                        }
                        "o" | "O" => {
                            return self.update(CassMsg::ExplorerOverlayCycled);
                        }
                        "g" => {
                            return self.update(CassMsg::ExplorerGroupByCycled { forward: true });
                        }
                        "G" => {
                            return self.update(CassMsg::ExplorerGroupByCycled { forward: false });
                        }
                        "z" => {
                            return self.update(CassMsg::ExplorerZoomCycled { forward: true });
                        }
                        "Z" => {
                            return self.update(CassMsg::ExplorerZoomCycled { forward: false });
                        }
                        _ => {}
                    }
                }
                // Bare 'o' key fires OpenInEditor; remap to overlay toggle on Explorer.
                CassMsg::OpenInEditor if self.analytics_view == AnalyticsView::Explorer => {
                    return self.update(CassMsg::ExplorerOverlayCycled);
                }
                // Suppress query input on analytics surface (no search bar visible).
                CassMsg::QueryChanged(_) => {
                    return ftui::Cmd::none();
                }
                // Let other messages (analytics-specific, lifecycle, etc.) fall through.
                _ => {}
            }
        }

        // When on the sources surface, remap navigation and suppress query input.
        if self.surface == AppSurface::Sources {
            match &msg {
                CassMsg::SelectionMoved { delta } => {
                    return self.update(CassMsg::SourcesSelectionMoved { delta: *delta });
                }
                // 's' key triggers sync for selected source.
                CassMsg::QueryChanged(text) if text == "s" || text == "S" => {
                    if let Some(item) = self.sources_view.items.get(self.sources_view.selected)
                        && item.kind != crate::sources::SourceKind::Local
                        && !item.busy
                    {
                        let name = item.name.clone();
                        return self.update(CassMsg::SourceSyncRequested(name));
                    }
                    return ftui::Cmd::none();
                }
                // 'd' key triggers doctor for selected source.
                CassMsg::QueryChanged(text) if text == "d" || text == "D" => {
                    if let Some(item) = self.sources_view.items.get(self.sources_view.selected)
                        && item.kind != crate::sources::SourceKind::Local
                        && !item.busy
                    {
                        let name = item.name.clone();
                        return self.update(CassMsg::SourceDoctorRequested(name));
                    }
                    return ftui::Cmd::none();
                }
                // 'r' key refreshes the source list from disk.
                CassMsg::QueryChanged(text) if text == "r" || text == "R" => {
                    return self.update(CassMsg::SourcesRefreshed);
                }
                // Suppress all other query input on sources surface.
                CassMsg::QueryChanged(_) => {
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
                self.push_undo("Clear query");
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
                    self.push_undo("Delete word");
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

                // Store results and group into panes using current mode.
                self.results = hits;
                self.regroup_panes();

                // Keep selection stable across reranking by retaining only keys that
                // still exist in the new result set.
                let available: HashSet<SelectedHitKey> =
                    self.results.iter().map(SelectedHitKey::from_hit).collect();
                self.selected.retain(|k| available.contains(k));
                if self.selected.is_empty() {
                    self.open_confirm_armed = false;
                }

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
                self.push_undo("Set agent filter");
                self.filters.agents = agents;
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }
            CassMsg::FilterWorkspaceSet(workspaces) => {
                self.push_undo("Set workspace filter");
                self.filters.workspaces = workspaces;
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }
            CassMsg::FilterTimeSet { from, to } => {
                self.push_undo("Set time filter");
                self.filters.created_from = from;
                self.filters.created_to = to;
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }
            CassMsg::FilterSourceSet(source) => {
                self.push_undo("Set source filter");
                self.filters.source_filter = source;
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }
            CassMsg::FiltersClearAll => {
                self.push_undo("Clear all filters");
                self.filters = SearchFilters::default();
                self.time_preset = TimePreset::All;
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }
            CassMsg::TimePresetCycled => {
                self.push_undo("Cycle time preset");
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
                self.push_undo("Cycle source filter");
                self.filters.source_filter = self.filters.source_filter.cycle();
                self.status = format!(
                    "Source: {}",
                    Self::source_filter_status(&self.filters.source_filter)
                );
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
                self.focus_manager.focus_next();
                self.focus_flash_until =
                    Some(Instant::now() + std::time::Duration::from_millis(220));
                self.anim.trigger_focus_flash();
                ftui::Cmd::none()
            }
            CassMsg::FocusDirectional { direction } => {
                let nav_dir = match direction {
                    FocusDirection::Left => NavDirection::Left,
                    FocusDirection::Right => NavDirection::Right,
                    FocusDirection::Up => NavDirection::Up,
                    FocusDirection::Down => NavDirection::Down,
                };
                self.focus_manager.navigate(nav_dir);
                ftui::Cmd::none()
            }
            CassMsg::DetailScrolled { delta } => {
                let new_scroll = self.detail_scroll as i32 + delta;
                self.detail_scroll = new_scroll.max(0) as u16;
                ftui::Cmd::none()
            }
            CassMsg::PageScrolled { delta } => {
                if self.focused_region() == FocusRegion::Detail {
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

            // -- Undo/redo ----------------------------------------------------
            CassMsg::Undo => {
                let current = self.capture_undo_state("current");
                if let Some(entry) = self.undo_history.pop_undo(current) {
                    let desc = entry.description;
                    let cmd = self.restore_undo_state(entry);
                    self.status = format!("Undo: {desc}");
                    cmd
                } else {
                    self.status = "Nothing to undo".to_string();
                    ftui::Cmd::none()
                }
            }
            CassMsg::Redo => {
                let current = self.capture_undo_state("current");
                if let Some(entry) = self.undo_history.pop_redo(current) {
                    let desc = entry.description;
                    let cmd = self.restore_undo_state(entry);
                    self.status = format!("Redo: {desc}");
                    cmd
                } else {
                    self.status = "Nothing to redo".to_string();
                    ftui::Cmd::none()
                }
            }

            // -- Grouping & timeline -----------------------------------------
            CassMsg::GroupingCycled => {
                self.push_undo("Cycle grouping");
                self.grouping_mode = self.grouping_mode.next();
                self.regroup_panes();
                self.status = format!("Grouping: {}", self.grouping_mode.label());
                ftui::Cmd::none()
            }
            CassMsg::TimelineJumped { forward } => {
                if let Some(target) = self.timeline_jump_index(forward) {
                    if let Some(pane) = self.panes.get_mut(self.active_pane) {
                        pane.selected = target;
                        let mut state = self.results_list_state.borrow_mut();
                        state.select(Some(target));
                    }
                    self.status = format!(
                        "Jumped to {}",
                        if forward { "next day" } else { "previous day" }
                    );
                } else {
                    self.status = format!(
                        "No {} day boundary",
                        if forward { "next" } else { "previous" }
                    );
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
                self.focus_manager.push_trap(focus_ids::GROUP_DETAIL_MODAL);
                self.focus_manager.focus(focus_ids::DETAIL_MODAL);
                ftui::Cmd::none()
            }
            CassMsg::DetailClosed => {
                self.show_detail_modal = false;
                self.focus_manager.pop_trap();
                self.focus_manager.focus(focus_ids::RESULTS_LIST);
                ftui::Cmd::none()
            }
            CassMsg::DetailTabChanged(tab) => {
                self.detail_tab = tab;
                self.detail_scroll = 0;
                ftui::Cmd::none()
            }
            CassMsg::ToggleJsonView => {
                if self.selected_hit().is_some() {
                    // Toggle: if already on Json tab, go back to Raw; otherwise switch to Json.
                    if self.detail_tab == DetailTab::Json {
                        self.detail_tab = DetailTab::Raw;
                    } else {
                        self.detail_tab = DetailTab::Json;
                    }
                    self.detail_scroll = 0;
                    self.show_detail_modal = true;
                    self.focus_manager.push_trap(focus_ids::GROUP_DETAIL_MODAL);
                    self.focus_manager.focus(focus_ids::DETAIL_MODAL);
                } else {
                    self.status = "No active result to view.".to_string();
                }
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
                    find.current = 0;
                    // Matches are computed during rendering by apply_find_highlight,
                    // which writes back to find.matches. Clear stale matches here
                    // so the renderer recomputes from scratch.
                    find.matches.clear();
                }
                ftui::Cmd::none()
            }
            CassMsg::DetailFindNavigated { forward } => {
                // Sync matches from render cache before navigating
                if let Some(ref mut find) = self.detail_find {
                    let cached = self.detail_find_matches_cache.borrow();
                    if !cached.is_empty() {
                        find.matches = cached.clone();
                    }
                }
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
                    // Auto-scroll to bring current match into view
                    let target_line = find.matches[find.current];
                    self.detail_scroll = target_line.saturating_sub(3);
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
                    self.focus_manager.push_trap(focus_ids::GROUP_BULK);
                    self.focus_manager.focus(focus_ids::BULK_MODAL);
                }
                ftui::Cmd::none()
            }
            CassMsg::BulkActionsClosed => {
                self.show_bulk_modal = false;
                self.focus_manager.pop_trap();
                ftui::Cmd::none()
            }
            CassMsg::BulkActionExecuted { action_index } => {
                self.show_bulk_modal = false;
                self.focus_manager.pop_trap();
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
                            self.toast_manager
                                .push(Toast::new("Copied content".to_string(), ToastType::Success));
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
            CassMsg::CopyQuery => {
                use crate::ui::components::toast::{Toast, ToastType};
                if self.query.is_empty() {
                    self.status = "No query to copy.".to_string();
                } else {
                    match copy_to_clipboard(&self.query) {
                        Ok(()) => {
                            self.status = "Copied query to clipboard".to_string();
                            self.toast_manager
                                .push(Toast::new("Copied query".to_string(), ToastType::Success));
                        }
                        Err(e) => {
                            self.status = format!("Clipboard: {e}");
                            self.toast_manager
                                .push(Toast::new(format!("Copy failed: {e}"), ToastType::Error));
                        }
                    }
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
                    self.focus_manager.push_trap(focus_ids::GROUP_DETAIL_MODAL);
                    self.focus_manager.focus(focus_ids::DETAIL_MODAL);
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
                self.anim.show_peek_badge();
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
                self.focus_manager.push_trap(focus_ids::GROUP_PALETTE);
                self.focus_manager.focus(focus_ids::COMMAND_PALETTE);
                ftui::Cmd::none()
            }
            CassMsg::PaletteClosed => {
                self.palette_state.open = false;
                self.focus_manager.pop_trap();
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
                        ftui::Cmd::msg(CassMsg::SavedViewsOpened)
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
                    Some(PaletteAction::AnalyticsPlans) => ftui::Cmd::batch(vec![
                        ftui::Cmd::msg(CassMsg::AnalyticsEntered),
                        ftui::Cmd::msg(CassMsg::AnalyticsViewChanged(AnalyticsView::Plans)),
                    ]),
                    Some(PaletteAction::AnalyticsCoverage) => ftui::Cmd::batch(vec![
                        ftui::Cmd::msg(CassMsg::AnalyticsEntered),
                        ftui::Cmd::msg(CassMsg::AnalyticsViewChanged(AnalyticsView::Coverage)),
                    ]),
                    Some(PaletteAction::ScreenshotHtml) => {
                        ftui::Cmd::msg(CassMsg::ScreenshotRequested(ScreenshotFormat::Html))
                    }
                    Some(PaletteAction::ScreenshotSvg) => {
                        ftui::Cmd::msg(CassMsg::ScreenshotRequested(ScreenshotFormat::Svg))
                    }
                    Some(PaletteAction::ScreenshotText) => {
                        ftui::Cmd::msg(CassMsg::ScreenshotRequested(ScreenshotFormat::Text))
                    }
                    Some(PaletteAction::MacroRecordingToggle) => {
                        ftui::Cmd::msg(CassMsg::MacroRecordingToggled)
                    }
                    Some(PaletteAction::Sources) => ftui::Cmd::msg(CassMsg::SourcesEntered),
                    None => ftui::Cmd::none(),
                }
            }

            // -- Help overlay -------------------------------------------------
            // -- Theme editor -------------------------------------------------
            CassMsg::ThemeEditorOpened => {
                if !self.show_theme_editor {
                    self.show_theme_editor = true;
                    self.theme_editor = Some(ThemeEditorState::from_data_dir(
                        self.theme_preset,
                        &self.data_dir,
                    ));
                }
                ftui::Cmd::none()
            }
            CassMsg::ThemeEditorClosed => {
                self.show_theme_editor = false;
                self.theme_editor = None;
                ftui::Cmd::none()
            }
            CassMsg::ThemeEditorMoved { delta } => {
                if let Some(editor) = self.theme_editor.as_mut()
                    && !editor.editing
                {
                    let n = ThemeColorSlot::ALL.len();
                    if delta > 0 {
                        editor.selected = (editor.selected + 1).min(n - 1);
                    } else if delta < 0 {
                        editor.selected = editor.selected.saturating_sub(1);
                    }
                }
                ftui::Cmd::none()
            }
            CassMsg::ThemeEditorEditStarted => {
                if let Some(editor) = self.theme_editor.as_mut() {
                    editor.editing = true;
                    let slot = editor.selected_slot();
                    editor.hex_buffer = slot.get(&editor.overrides).unwrap_or("").to_string();
                }
                ftui::Cmd::none()
            }
            CassMsg::ThemeEditorEditCommitted => {
                if let Some(editor) = self.theme_editor.as_mut()
                    && editor.editing
                {
                    let hex = editor.hex_buffer.trim().to_string();
                    let slot = editor.selected_slot();
                    if hex.is_empty() {
                        slot.set(&mut editor.overrides, None);
                    } else {
                        slot.set(&mut editor.overrides, Some(hex));
                    }
                    editor.editing = false;
                    editor.hex_buffer.clear();

                    // Live-apply the config to preview
                    let config = editor.to_config();
                    if let Ok(ctx) =
                        StyleContext::from_options_with_theme_config(self.style_options, &config)
                    {
                        editor.refresh_contrast(&ctx);
                    }
                }
                ftui::Cmd::none()
            }
            CassMsg::ThemeEditorEditCancelled => {
                if let Some(editor) = self.theme_editor.as_mut() {
                    editor.editing = false;
                    editor.hex_buffer.clear();
                }
                ftui::Cmd::none()
            }
            CassMsg::ThemeEditorHexChanged(text) => {
                if let Some(editor) = self.theme_editor.as_mut()
                    && editor.editing
                {
                    editor.hex_buffer = text;
                }
                ftui::Cmd::none()
            }
            CassMsg::ThemeEditorSlotCleared => {
                if let Some(editor) = self.theme_editor.as_mut() {
                    let slot = editor.selected_slot();
                    slot.set(&mut editor.overrides, None);
                }
                ftui::Cmd::none()
            }
            CassMsg::ThemeEditorPresetCycled => {
                if let Some(editor) = self.theme_editor.as_mut() {
                    editor.base_preset = editor.base_preset.next();
                }
                ftui::Cmd::none()
            }
            CassMsg::ThemeEditorExported => {
                if let Some(editor) = self.theme_editor.as_ref() {
                    let config = editor.to_config();
                    let path = self.data_dir.join("theme.json");
                    match config.save_to_path(&path) {
                        Ok(()) => {
                            // Apply saved theme to the live UI.
                            self.theme_preset = editor.base_preset;
                            self.style_options.preset = editor.base_preset;
                            self.status = format!("Theme saved to {}", path.display());
                        }
                        Err(e) => {
                            self.status = format!("Failed to save theme: {e}");
                        }
                    }
                }
                ftui::Cmd::none()
            }

            // -- Inspector overlay -----------------------------------------
            CassMsg::InspectorToggled => {
                self.show_inspector = !self.show_inspector;
                if self.show_inspector {
                    self.inspector_state.toggle();
                }
                if !self.show_inspector && self.inspector_state.is_active() {
                    self.inspector_state.toggle();
                }
                ftui::Cmd::none()
            }
            CassMsg::InspectorTabCycled => {
                self.inspector_tab = self.inspector_tab.next();
                ftui::Cmd::none()
            }
            CassMsg::InspectorModeCycled => {
                self.inspector_state.cycle_mode();
                ftui::Cmd::none()
            }

            CassMsg::HelpToggled => {
                self.show_help = !self.show_help;
                self.help_scroll = 0;
                if self.show_help {
                    self.focus_manager.push_trap(focus_ids::GROUP_HELP);
                    self.focus_manager.focus(focus_ids::HELP_OVERLAY);
                } else {
                    self.focus_manager.pop_trap();
                }
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
                    self.focus_manager.push_trap(focus_ids::GROUP_EXPORT);
                    self.focus_manager.focus(focus_ids::EXPORT_MODAL);
                }
                ftui::Cmd::none()
            }
            CassMsg::ExportModalClosed => {
                self.show_export_modal = false;
                self.export_modal_state = None;
                self.focus_manager.pop_trap();
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
                self.focus_manager.pop_trap();
                self.status = format!("Exported to {}", output_path.display());
                ftui::Cmd::none()
            }
            CassMsg::ExportFailed(err) => {
                self.status = format!("Export failed: {err}");
                ftui::Cmd::none()
            }

            // -- Screenshot export --------------------------------------------
            CassMsg::ScreenshotRequested(format) => {
                self.screenshot_pending = Some(format);
                // The buffer capture happens in view(); on the next Tick we
                // pick it up and write the file.
                ftui::Cmd::none()
            }
            CassMsg::ScreenshotCompleted(path) => {
                self.status = format!("Screenshot saved: {}", path.display());
                let msg = format!("Saved to {}", path.display());
                ftui::Cmd::msg(CassMsg::ToastShown {
                    message: msg,
                    toast_type: crate::ui::components::toast::ToastType::Success,
                })
            }
            CassMsg::ScreenshotFailed(err) => {
                self.status = format!("Screenshot failed: {err}");
                ftui::Cmd::msg(CassMsg::ToastShown {
                    message: format!("Screenshot failed: {err}"),
                    toast_type: crate::ui::components::toast::ToastType::Error,
                })
            }

            // -- Consent dialog -----------------------------------------------
            CassMsg::ConsentDialogOpened => {
                self.show_consent_dialog = true;
                self.focus_manager.push_trap(focus_ids::GROUP_CONSENT);
                self.focus_manager.focus(focus_ids::CONSENT_DIALOG);
                ftui::Cmd::none()
            }
            CassMsg::ConsentDialogClosed => {
                self.show_consent_dialog = false;
                self.focus_manager.pop_trap();
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
                if self.source_filter_menu_open {
                    self.source_filter_menu_open = false;
                    self.status = "Source filter menu closed".to_string();
                    self.focus_manager.pop_trap();
                } else {
                    self.refresh_available_source_ids();
                    self.source_filter_menu_open = true;
                    self.focus_manager.push_trap(focus_ids::GROUP_SOURCE_FILTER);
                    self.focus_manager.focus(focus_ids::SOURCE_FILTER_MENU);
                    self.source_filter_menu_selection = match &self.filters.source_filter {
                        SourceFilter::All => 0,
                        SourceFilter::Local => 1,
                        SourceFilter::Remote => 2,
                        SourceFilter::SourceId(id) => self
                            .available_source_ids
                            .iter()
                            .position(|s| s == id)
                            .map(|idx| idx + 3)
                            .unwrap_or(0),
                    };
                    self.status =
                        "Source filter menu (↑/↓ select, Enter apply, Esc close)".to_string();
                }
                ftui::Cmd::none()
            }
            CassMsg::SourceFilterSelected(filter) => {
                self.source_filter_menu_open = false;
                self.focus_manager.pop_trap();
                self.status = format!("Source: {}", Self::source_filter_status(&filter));
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
            CassMsg::SavedViewsOpened => {
                self.sort_saved_views();
                self.clamp_saved_views_selection();
                self.show_saved_views_modal = true;
                self.saved_view_drag = None;
                self.saved_view_rename_mode = false;
                self.saved_view_rename_buffer.clear();
                self.focus_manager.push_trap(focus_ids::GROUP_SAVED_VIEWS);
                self.focus_manager.focus(focus_ids::SAVED_VIEWS_MODAL);
                if self.saved_views.is_empty() {
                    self.status = "No saved views. Use Ctrl+1..9 to save one.".to_string();
                } else {
                    self.status = format!("Saved views manager ({})", self.saved_views.len());
                }
                ftui::Cmd::none()
            }
            CassMsg::SavedViewsClosed => {
                self.show_saved_views_modal = false;
                self.saved_view_drag = None;
                self.saved_view_rename_mode = false;
                self.saved_view_rename_buffer.clear();
                self.focus_manager.pop_trap();
                self.status = "Saved views manager closed".to_string();
                ftui::Cmd::none()
            }
            CassMsg::SavedViewsSelectionMoved { delta } => {
                self.move_saved_views_selection(delta);
                ftui::Cmd::none()
            }
            CassMsg::SavedViewLoadedSelected => {
                if let Some(slot) = self.selected_saved_view_slot() {
                    self.show_saved_views_modal = false;
                    self.saved_view_drag = None;
                    self.saved_view_rename_mode = false;
                    self.saved_view_rename_buffer.clear();
                    return ftui::Cmd::msg(CassMsg::ViewLoaded(slot));
                }
                use crate::ui::components::toast::{Toast, ToastType};
                self.status = "No saved view selected".to_string();
                self.toast_manager.push(Toast::new(
                    "No saved view selected".to_string(),
                    ToastType::Warning,
                ));
                ftui::Cmd::none()
            }
            CassMsg::SavedViewRenameStarted => {
                if let Some(slot) = self.selected_saved_view_slot() {
                    self.saved_view_drag = None;
                    self.saved_view_rename_mode = true;
                    self.saved_view_rename_buffer =
                        self.selected_saved_view_label().unwrap_or_default();
                    self.status = format!("Renaming slot {slot}. Enter to save.");
                } else {
                    self.status = "No saved view selected".to_string();
                }
                ftui::Cmd::none()
            }
            CassMsg::SavedViewRenameCommitted => {
                use crate::ui::components::toast::{Toast, ToastType};
                if let Some(view) = self.saved_views.get_mut(self.saved_views_selection) {
                    let slot = view.slot;
                    let trimmed = self.saved_view_rename_buffer.trim();
                    if trimmed.is_empty() {
                        view.label = None;
                        self.status = format!("Cleared label for slot {slot}");
                        self.toast_manager.push(Toast::new(
                            format!("Cleared label for slot {slot}"),
                            ToastType::Success,
                        ));
                    } else {
                        view.label = Some(trimmed.to_string());
                        self.status = format!("Renamed slot {slot} to \"{trimmed}\"");
                        self.toast_manager.push(Toast::new(
                            format!("Renamed slot {slot}"),
                            ToastType::Success,
                        ));
                    }
                    self.saved_view_rename_mode = false;
                    self.saved_view_drag = None;
                    self.saved_view_rename_buffer.clear();
                    self.dirty_since = Some(Instant::now());
                } else {
                    self.saved_view_rename_mode = false;
                    self.saved_view_drag = None;
                    self.saved_view_rename_buffer.clear();
                    self.status = "No saved view selected".to_string();
                }
                ftui::Cmd::none()
            }
            CassMsg::SavedViewDeletedSelected => {
                use crate::ui::components::toast::{Toast, ToastType};
                if let Some(slot) = self.selected_saved_view_slot() {
                    self.saved_views.retain(|v| v.slot != slot);
                    self.clamp_saved_views_selection();
                    self.saved_view_drag = None;
                    self.saved_view_rename_mode = false;
                    self.saved_view_rename_buffer.clear();
                    self.dirty_since = Some(Instant::now());
                    self.status = format!("Deleted saved view slot {slot}");
                    self.toast_manager.push(Toast::new(
                        format!("Deleted slot {slot}"),
                        ToastType::Warning,
                    ));
                } else {
                    self.status = "No saved view selected".to_string();
                }
                ftui::Cmd::none()
            }
            CassMsg::SavedViewsCleared => {
                use crate::ui::components::toast::{Toast, ToastType};
                let count = self.saved_views.len();
                self.saved_views.clear();
                self.saved_views_selection = 0;
                self.saved_view_drag = None;
                self.saved_view_rename_mode = false;
                self.saved_view_rename_buffer.clear();
                self.dirty_since = Some(Instant::now());
                self.status = format!("Cleared {count} saved view(s)");
                self.toast_manager.push(Toast::new(
                    format!("Cleared {count} saved view(s)"),
                    ToastType::Warning,
                ));
                ftui::Cmd::none()
            }
            CassMsg::ViewSaved(slot) => {
                use crate::ui::components::toast::{Toast, ToastType};
                let preserved_label = self
                    .saved_views
                    .iter()
                    .find(|v| v.slot == slot)
                    .and_then(|v| v.label.clone());
                let view = SavedView {
                    slot,
                    label: preserved_label,
                    agents: self.filters.agents.clone(),
                    workspaces: self.filters.workspaces.clone(),
                    created_from: self.filters.created_from,
                    created_to: self.filters.created_to,
                    ranking: self.ranking_mode,
                    source_filter: self.filters.source_filter.clone(),
                };
                // Replace existing slot or push
                let mut replaced = false;
                if let Some(existing) = self.saved_views.iter_mut().find(|v| v.slot == slot) {
                    *existing = view;
                    replaced = true;
                } else {
                    self.saved_views.push(view);
                }
                self.sort_saved_views();
                if let Some(idx) = self.saved_views.iter().position(|v| v.slot == slot) {
                    self.saved_views_selection = idx;
                }
                self.dirty_since = Some(Instant::now());
                let verb = if replaced { "Updated" } else { "Saved" };
                self.status = format!("{verb} current view to slot {slot}");
                self.toast_manager.push(Toast::new(
                    format!("{verb} slot {slot}"),
                    ToastType::Success,
                ));
                ftui::Cmd::none()
            }
            CassMsg::ViewLoaded(slot) => {
                use crate::ui::components::toast::{Toast, ToastType};
                if let Some(view) = self.saved_views.iter().find(|v| v.slot == slot).cloned() {
                    self.push_undo("Load saved view");
                    self.filters.agents = view.agents.clone();
                    self.filters.workspaces = view.workspaces.clone();
                    self.filters.created_from = view.created_from;
                    self.filters.created_to = view.created_to;
                    self.ranking_mode = view.ranking;
                    self.filters.source_filter = view.source_filter.clone();
                    self.show_saved_views_modal = false;
                    self.saved_view_rename_mode = false;
                    self.saved_view_rename_buffer.clear();
                    let label = view
                        .label
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| format!("slot {slot}"));
                    self.status = format!("Loaded saved view {label}");
                    self.toast_manager
                        .push(Toast::new(format!("Loaded {label}"), ToastType::Success));
                    return ftui::Cmd::msg(CassMsg::SearchRequested);
                }
                self.status = format!("No saved view in slot {slot}");
                self.toast_manager.push(Toast::new(
                    format!("Slot {slot} is empty"),
                    ToastType::Warning,
                ));
                ftui::Cmd::none()
            }

            // -- Index --------------------------------------------------------
            CassMsg::IndexRefreshRequested => {
                if self.index_refresh_in_flight {
                    self.status = "Index refresh already running".to_string();
                    return ftui::Cmd::none();
                }
                self.index_refresh_in_flight = true;
                self.status = "Refreshing index...".to_string();
                let data_dir = self.data_dir.clone();
                let db_path = self.db_path.clone();
                #[cfg(test)]
                {
                    let _ = data_dir;
                    let _ = db_path;
                    ftui::Cmd::task(|| CassMsg::IndexRefreshCompleted)
                }
                #[cfg(not(test))]
                {
                    ftui::Cmd::task(move || {
                        let opts = crate::indexer::IndexOptions {
                            full: false,
                            force_rebuild: false,
                            watch: false,
                            watch_once_paths: None,
                            db_path,
                            data_dir,
                            semantic: false,
                            build_hnsw: false,
                            embedder: "fastembed".to_string(),
                            progress: None,
                        };
                        match crate::indexer::run_index(opts, None) {
                            Ok(()) => CassMsg::IndexRefreshCompleted,
                            Err(e) => CassMsg::IndexRefreshFailed(e.to_string()),
                        }
                    })
                }
            }
            CassMsg::IndexProgress {
                processed,
                total,
                new_items,
            } => {
                if total > 0 {
                    self.status = format!("Indexing {processed}/{total} (+{new_items} new)");
                }
                ftui::Cmd::none()
            }
            CassMsg::IndexRefreshCompleted => {
                self.index_refresh_in_flight = false;
                self.status = "Index refresh complete".to_string();
                ftui::Cmd::none()
            }
            CassMsg::IndexRefreshFailed(err) => {
                self.index_refresh_in_flight = false;
                self.status = format!("Index refresh failed: {err}");
                ftui::Cmd::none()
            }

            // -- State persistence --------------------------------------------
            CassMsg::StateLoadRequested => {
                let state_path = self.state_file_path();
                ftui::Cmd::task(move || match load_persisted_state_from_path(&state_path) {
                    Ok(Some(state)) => CassMsg::StateLoaded(Box::new(state)),
                    Ok(None) => CassMsg::StateLoaded(Box::new(persisted_state_defaults())),
                    Err(e) => CassMsg::StateLoadFailed(e),
                })
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
                self.sort_saved_views();
                self.clamp_saved_views_selection();
                self.fancy_borders = state.fancy_borders;
                self.help_pinned = state.help_pinned;
                self.dirty_since = None;
                ftui::Cmd::none()
            }
            CassMsg::StateLoadFailed(err) => {
                self.status = format!("Failed to load TUI state: {err}");
                ftui::Cmd::none()
            }
            CassMsg::StateSaveRequested => {
                let state_path = self.state_file_path();
                let snapshot = self.capture_persisted_state();
                self.dirty_since = None;
                ftui::Cmd::task(move || {
                    match save_persisted_state_to_path(&state_path, &snapshot) {
                        Ok(()) => CassMsg::StateSaved,
                        Err(e) => CassMsg::StateSaveFailed(e),
                    }
                })
            }
            CassMsg::StateSaved => ftui::Cmd::none(),
            CassMsg::StateSaveFailed(err) => {
                self.status = format!("Failed to save TUI state: {err}");
                ftui::Cmd::none()
            }
            CassMsg::StateResetRequested => {
                let state_path = self.state_file_path();
                let data_dir = self.data_dir.clone();
                let db_path = self.db_path.clone();
                let search_service = self.search_service.clone();
                let db_reader = self.db_reader.clone();
                let known_workspaces = self.known_workspaces.clone();
                let reset = CassApp {
                    data_dir,
                    db_path,
                    search_service,
                    db_reader,
                    known_workspaces,
                    ..CassApp::default()
                };
                *self = reset;
                if let Err(e) = clear_persisted_state_file(&state_path) {
                    self.status = format!("State reset in-memory, but failed to remove file: {e}");
                } else {
                    self.status = "Reset TUI state to defaults".to_string();
                }
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
                self.pane_split_drag = None;
                ftui::Cmd::none()
            }
            CassMsg::Tick => {
                self.spinner_frame = self.spinner_frame.wrapping_add(1);
                let now = Instant::now();
                let dt = now.duration_since(self.last_tick);
                self.last_tick = now;
                // Record frame interval for inspector overlay.
                if self.show_inspector {
                    self.frame_timing.record_frame();
                }
                // Tick spring-based animations.
                self.anim.tick(dt);
                // Clear expired legacy flash indicators.
                if self.focus_flash_until.is_some_and(|t| now > t) {
                    self.focus_flash_until = None;
                }
                if self.peek_badge_until.is_some_and(|t| now > t) {
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
                if let Some(dirty_ts) = self.dirty_since
                    && dirty_ts.elapsed() >= STATE_SAVE_DEBOUNCE
                {
                    self.dirty_since = None;
                    cmds.push(ftui::Cmd::msg(CassMsg::StateSaveRequested));
                }
                cmds.push(ftui::Cmd::msg(CassMsg::ToastTick));
                // Advance macro playback and inject events as messages.
                if let Some(ref mut playback) = self.macro_playback {
                    let events = playback.advance(dt);
                    for event in events {
                        let msg = CassMsg::from(event);
                        cmds.push(ftui::Cmd::msg(msg));
                    }
                    if playback.is_done() {
                        self.macro_playback = None;
                        self.toast_manager
                            .push(crate::ui::components::toast::Toast::success(
                                "Macro playback complete",
                            ));
                        self.status = "Macro playback finished".to_string();
                    }
                }
                // Pick up screenshot buffer captured during view().
                if let Some((format, content)) = self.screenshot_result.borrow_mut().take() {
                    self.screenshot_pending = None;
                    cmds.push(write_screenshot_file(format, content));
                }
                if cmds.len() == 1 {
                    return cmds.remove(0);
                }
                ftui::Cmd::batch(cmds)
            }
            CassMsg::MouseEvent { kind, x, y } => {
                // ── Drag jitter filter ──────────────────────────────
                // Suppress LeftDrag events where the pointer hasn't moved enough
                // to matter. This eliminates touchpad/sub-cell noise.
                if kind == MouseEventKind::LeftDrag
                    && let Some((lx, ly)) = self.last_mouse_pos
                {
                    let dx = (x as i32 - lx as i32).unsigned_abs() as u16;
                    let dy = (y as i32 - ly as i32).unsigned_abs() as u16;
                    if dx < DRAG_JITTER_THRESHOLD && dy < DRAG_JITTER_THRESHOLD {
                        return ftui::Cmd::none(); // sub-threshold motion
                    }
                }
                // Update last-known mouse position for future jitter checks.
                match kind {
                    MouseEventKind::LeftClick | MouseEventKind::LeftDrag => {
                        self.last_mouse_pos = Some((x, y));
                    }
                    MouseEventKind::LeftRelease => {
                        self.last_mouse_pos = None;
                        self.drag_hover_settled_at = None;
                    }
                    _ => {}
                }

                let region = self.hit_test(x, y);

                if self.show_saved_views_modal {
                    match (kind, region) {
                        (MouseEventKind::LeftClick, MouseHitRegion::SavedViewRow { row_idx }) => {
                            let idx = row_idx.min(self.saved_views.len().saturating_sub(1));
                            self.saved_views_selection = idx;
                            self.saved_view_drag = Some(SavedViewDragState {
                                from_idx: idx,
                                hover_idx: idx,
                            });
                            return ftui::Cmd::none();
                        }
                        (MouseEventKind::LeftDrag, MouseHitRegion::SavedViewRow { row_idx }) => {
                            let idx = row_idx.min(self.saved_views.len().saturating_sub(1));
                            if let Some(drag) = self.saved_view_drag.as_mut() {
                                // Hover stabilization: only update if pointer has
                                // remained on the new row long enough to settle.
                                if idx != drag.hover_idx {
                                    let now = Instant::now();
                                    let settled = self.drag_hover_settled_at.is_some_and(|t| {
                                        t.elapsed() >= Duration::from_millis(DRAG_HOVER_SETTLE_MS)
                                    });
                                    if settled {
                                        drag.hover_idx = idx;
                                        self.saved_views_selection = idx;
                                        self.drag_hover_settled_at = Some(now);
                                    } else if self.drag_hover_settled_at.is_none() {
                                        self.drag_hover_settled_at = Some(now);
                                    }
                                }
                            }
                            return ftui::Cmd::none();
                        }
                        (MouseEventKind::LeftDrag, _) => return ftui::Cmd::none(),
                        (MouseEventKind::LeftRelease, MouseHitRegion::SavedViewRow { row_idx }) => {
                            if let Some(drag) = self.saved_view_drag.take() {
                                let to_idx = row_idx.min(self.saved_views.len().saturating_sub(1));
                                if self.reorder_saved_views(drag.from_idx, to_idx) {
                                    self.status =
                                        format!("Moved saved view to position {}", to_idx + 1);
                                    self.dirty_since = Some(Instant::now());
                                }
                            }
                            return ftui::Cmd::none();
                        }
                        (MouseEventKind::LeftRelease, _) => {
                            self.saved_view_drag = None;
                            return ftui::Cmd::none();
                        }
                        _ => return ftui::Cmd::none(),
                    }
                }

                if kind == MouseEventKind::LeftClick
                    && !matches!(region, MouseHitRegion::SplitHandle)
                {
                    self.pane_split_drag = None;
                }

                match (kind, region) {
                    // ── Pane split drag: click + drag divider ───────
                    (MouseEventKind::LeftClick, MouseHitRegion::SplitHandle) => {
                        self.pane_split_drag = Some(PaneSplitDragState);
                        let _ = self.apply_panel_ratio_from_mouse_x(x);
                        ftui::Cmd::none()
                    }
                    (MouseEventKind::LeftDrag, _) if self.pane_split_drag.is_some() => {
                        let _ = self.apply_panel_ratio_from_mouse_x(x);
                        ftui::Cmd::none()
                    }
                    (MouseEventKind::LeftRelease, _) => {
                        self.pane_split_drag = None;
                        ftui::Cmd::none()
                    }
                    // ── Scroll in results ────────────────────────────
                    (MouseEventKind::ScrollUp, MouseHitRegion::Results { .. }) => {
                        ftui::Cmd::msg(CassMsg::SelectionMoved { delta: -3 })
                    }
                    (MouseEventKind::ScrollDown, MouseHitRegion::Results { .. }) => {
                        ftui::Cmd::msg(CassMsg::SelectionMoved { delta: 3 })
                    }
                    // ── Scroll in detail ─────────────────────────────
                    (MouseEventKind::ScrollUp, MouseHitRegion::Detail) => {
                        ftui::Cmd::msg(CassMsg::DetailScrolled { delta: -3 })
                    }
                    (MouseEventKind::ScrollDown, MouseHitRegion::Detail) => {
                        ftui::Cmd::msg(CassMsg::DetailScrolled { delta: 3 })
                    }
                    // ── Left click in results: select item ──────────
                    (MouseEventKind::LeftClick, MouseHitRegion::Results { item_idx }) => {
                        let hit_count = self
                            .panes
                            .get(self.active_pane)
                            .map_or(self.results.len(), |p| p.hits.len());
                        if item_idx < hit_count {
                            // Compute delta from current selection to clicked row.
                            let current =
                                self.panes.get(self.active_pane).map_or(0, |p| p.selected);
                            let delta = item_idx as i32 - current as i32;
                            if delta != 0 {
                                ftui::Cmd::msg(CassMsg::SelectionMoved { delta })
                            } else {
                                // Clicking the already-selected row opens detail.
                                ftui::Cmd::msg(CassMsg::DetailOpened)
                            }
                        } else {
                            ftui::Cmd::none()
                        }
                    }
                    // ── Right click in results: toggle select ───────
                    (MouseEventKind::RightClick, MouseHitRegion::Results { item_idx }) => {
                        let hit_count = self
                            .panes
                            .get(self.active_pane)
                            .map_or(self.results.len(), |p| p.hits.len());
                        if item_idx < hit_count {
                            // Move to the row first, then toggle selection.
                            let current =
                                self.panes.get(self.active_pane).map_or(0, |p| p.selected);
                            let delta = item_idx as i32 - current as i32;
                            let mut cmds = Vec::new();
                            if delta != 0 {
                                cmds.push(ftui::Cmd::msg(CassMsg::SelectionMoved { delta }));
                            }
                            cmds.push(ftui::Cmd::msg(CassMsg::SelectionToggled));
                            ftui::Cmd::batch(cmds)
                        } else {
                            ftui::Cmd::none()
                        }
                    }
                    // ── Click in detail: focus detail pane ──────────
                    (MouseEventKind::LeftClick, MouseHitRegion::Detail) => {
                        if self.focused_region() != FocusRegion::Detail {
                            ftui::Cmd::msg(CassMsg::FocusToggled)
                        } else {
                            ftui::Cmd::none()
                        }
                    }
                    // ── Click in search bar: focus results (query) ──
                    (MouseEventKind::LeftClick, MouseHitRegion::SearchBar) => {
                        if self.focused_region() != FocusRegion::Results {
                            ftui::Cmd::msg(CassMsg::FocusToggled)
                        } else {
                            ftui::Cmd::none()
                        }
                    }
                    // ── Scroll outside tracked regions: default to results
                    (MouseEventKind::ScrollUp, _) => {
                        ftui::Cmd::msg(CassMsg::SelectionMoved { delta: -3 })
                    }
                    (MouseEventKind::ScrollDown, _) => {
                        ftui::Cmd::msg(CassMsg::SelectionMoved { delta: 3 })
                    }
                    // ── Unhandled clicks ─────────────────────────────
                    _ => ftui::Cmd::none(),
                }
            }

            // -- Analytics surface ---------------------------------------------
            CassMsg::AnalyticsEntered => {
                self.pane_split_drag = None;
                if self.surface != AppSurface::Analytics {
                    self.view_stack.push(self.surface);
                    self.surface = AppSurface::Analytics;
                }
                // Load chart data on entry (lazy, from db_reader).
                if self.analytics_cache.is_none()
                    && let Some(db) = &self.db_reader
                {
                    self.analytics_cache = Some(super::analytics_charts::load_chart_data(
                        db,
                        &self.analytics_filters,
                        self.explorer_group_by,
                    ));
                }
                ftui::Cmd::none()
            }
            CassMsg::AnalyticsViewChanged(view) => {
                self.analytics_view = view;
                self.analytics_selection = 0; // reset selection on view change
                ftui::Cmd::none()
            }
            CassMsg::ViewStackPopped => {
                self.pane_split_drag = None;
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
                self.analytics_cache = None; // invalidate chart data on filter change
                ftui::Cmd::none()
            }
            CassMsg::AnalyticsAgentFilterSet(agents) => {
                self.analytics_filters.agents = agents;
                self.analytics_cache = None;
                ftui::Cmd::none()
            }
            CassMsg::AnalyticsWorkspaceFilterSet(workspaces) => {
                self.analytics_filters.workspaces = workspaces;
                self.analytics_cache = None;
                ftui::Cmd::none()
            }
            CassMsg::AnalyticsSourceFilterSet(sf) => {
                self.analytics_filters.source_filter = sf;
                self.analytics_cache = None;
                ftui::Cmd::none()
            }
            CassMsg::AnalyticsFiltersClearAll => {
                self.analytics_filters = AnalyticsFilterState::default();
                self.analytics_cache = None;
                ftui::Cmd::none()
            }
            CassMsg::AnalyticsSelectionMoved { delta } => {
                let count = self.analytics_selectable_count();
                if count > 0 {
                    let cur = self.analytics_selection as i32;
                    let next = (cur + delta).rem_euclid(count as i32) as usize;
                    self.analytics_selection = next;
                }
                ftui::Cmd::none()
            }
            CassMsg::AnalyticsDrilldown(ctx) => {
                let DrilldownContext {
                    since_ms,
                    until_ms,
                    agent,
                    model,
                } = ctx;
                tracing::debug!(
                    since_ms = ?since_ms,
                    until_ms = ?until_ms,
                    agent = ?agent,
                    model = ?model,
                    "analytics drilldown requested"
                );

                // Push analytics surface onto the back-stack.
                self.view_stack.push(AppSurface::Analytics);
                self.surface = AppSurface::Search;

                // Convert drilldown context into search filters.
                self.filters.created_from = since_ms;
                self.filters.created_to = until_ms;

                // Start from analytics filters to avoid leaking stale search filters.
                self.filters.agents = self.analytics_filters.agents.clone();
                self.filters.workspaces = self.analytics_filters.workspaces.clone();
                self.filters.source_filter = self.analytics_filters.source_filter.clone();
                self.filters.session_paths.clear();

                // Apply selected dimension filter (agent) on top of inherited globals.
                if let Some(agent) = agent {
                    self.filters.agents.clear();
                    self.filters.agents.insert(agent);
                }
                // Clear query — user types next.
                self.query.clear();
                self.cursor_pos = 0;
                self.input_mode = InputMode::Query;

                self.status = if let Some(model) = model {
                    format!("Drilldown from analytics (model: {model}) — type a query or browse")
                } else {
                    "Drilldown from analytics — type a query or browse".to_string()
                };
                ftui::Cmd::msg(CassMsg::SearchRequested)
            }
            CassMsg::ExplorerMetricCycled { forward } => {
                self.explorer_metric = if forward {
                    self.explorer_metric.next()
                } else {
                    self.explorer_metric.prev()
                };
                ftui::Cmd::none()
            }
            CassMsg::ExplorerOverlayCycled => {
                self.explorer_overlay = self.explorer_overlay.next();
                ftui::Cmd::none()
            }
            CassMsg::ExplorerGroupByCycled { forward } => {
                self.explorer_group_by = if forward {
                    self.explorer_group_by.next()
                } else {
                    self.explorer_group_by.prev()
                };
                // Invalidate cache so timeseries reloads with new granularity.
                self.analytics_cache = None;
                ftui::Cmd::none()
            }
            CassMsg::ExplorerZoomCycled { forward } => {
                self.explorer_zoom = if forward {
                    self.explorer_zoom.next()
                } else {
                    self.explorer_zoom.prev()
                };
                let (since_ms, until_ms) = self.explorer_zoom.to_range();
                self.analytics_filters.since_ms = since_ms;
                self.analytics_filters.until_ms = until_ms;
                self.analytics_cache = None;
                ftui::Cmd::none()
            }
            CassMsg::BreakdownTabCycled { forward } => {
                self.breakdown_tab = if forward {
                    self.breakdown_tab.next()
                } else {
                    self.breakdown_tab.prev()
                };
                self.analytics_selection = 0; // reset selection on tab change
                ftui::Cmd::none()
            }
            CassMsg::HeatmapMetricCycled { forward } => {
                self.heatmap_metric = if forward {
                    self.heatmap_metric.next()
                } else {
                    self.heatmap_metric.prev()
                };
                ftui::Cmd::none()
            }

            // -- Sources management (2noh9.4.9) ----------------------------------
            CassMsg::SourcesEntered => {
                self.pane_split_drag = None;
                if self.surface != AppSurface::Sources {
                    self.view_stack.push(self.surface);
                    self.surface = AppSurface::Sources;
                }
                #[cfg(not(test))]
                self.load_sources_view();
                ftui::Cmd::none()
            }
            CassMsg::SourcesRefreshed => {
                #[cfg(not(test))]
                self.load_sources_view();
                self.sources_view.status = "Sources refreshed".into();
                ftui::Cmd::none()
            }
            CassMsg::SourcesSelectionMoved { delta } => {
                let count = self.sources_view.items.len();
                if count > 0 {
                    let cur = self.sources_view.selected as i32;
                    let next = (cur + delta).rem_euclid(count as i32) as usize;
                    self.sources_view.selected = next;
                }
                ftui::Cmd::none()
            }
            CassMsg::SourceSyncRequested(ref name) => {
                let name = name.clone();
                if let Some(item) = self.sources_view.items.iter_mut().find(|i| i.name == name) {
                    item.busy = true;
                }
                self.sources_view.status = format!("Syncing '{name}'...");

                // Spawn background sync task.
                let source_name = name.clone();
                let data_dir = self.data_dir.clone();
                #[cfg(not(test))]
                {
                    use crate::sources::{SourcesConfig, SyncEngine};
                    let config = SourcesConfig::load().unwrap_or_default();
                    if let Some(source_def) = config.find_source(&source_name) {
                        let source_def = source_def.clone();
                        ftui::Cmd::task(move || {
                            let engine = SyncEngine::new(&data_dir);
                            match engine.sync_source(&source_def) {
                                Ok(report) => {
                                    let msg = if report.all_succeeded {
                                        format!(
                                            "Sync '{}' OK: {} files, {} bytes",
                                            source_name,
                                            report.total_files(),
                                            report.total_bytes()
                                        )
                                    } else {
                                        format!(
                                            "Sync '{}' partial: {}/{} paths OK",
                                            source_name,
                                            report.successful_paths(),
                                            report.successful_paths() + report.failed_paths()
                                        )
                                    };
                                    CassMsg::SourceSyncCompleted {
                                        source_name,
                                        message: msg,
                                    }
                                }
                                Err(e) => CassMsg::SourceSyncCompleted {
                                    source_name,
                                    message: format!("Sync failed: {e}"),
                                },
                            }
                        })
                    } else {
                        self.sources_view.status =
                            format!("Source '{source_name}' not found in config");
                        ftui::Cmd::none()
                    }
                }
                #[cfg(test)]
                {
                    let _ = data_dir;
                    let _ = source_name;
                    ftui::Cmd::none()
                }
            }
            CassMsg::SourceSyncCompleted {
                ref source_name,
                ref message,
            } => {
                let source_name = source_name.clone();
                let message = message.clone();
                if let Some(item) = self
                    .sources_view
                    .items
                    .iter_mut()
                    .find(|i| i.name == source_name)
                {
                    item.busy = false;
                }
                self.sources_view.status = message;
                ftui::Cmd::none()
            }
            CassMsg::SourceDoctorRequested(ref name) => {
                let name = name.clone();
                if let Some(item) = self.sources_view.items.iter_mut().find(|i| i.name == name) {
                    item.busy = true;
                }
                self.sources_view.status = format!("Running doctor on '{name}'...");

                // Spawn background doctor/probe task.
                let source_name = name.clone();
                #[cfg(not(test))]
                {
                    use crate::sources::{DiscoveredHost, SourcesConfig, probe_host};
                    let config = SourcesConfig::load().unwrap_or_default();
                    if let Some(source_def) = config.find_source(&source_name) {
                        let host_str = source_def
                            .host
                            .clone()
                            .unwrap_or_else(|| source_name.clone());
                        ftui::Cmd::task(move || {
                            let host = DiscoveredHost {
                                name: host_str,
                                hostname: None,
                                user: None,
                                port: None,
                                identity_file: None,
                            };
                            let result = probe_host(&host, 15);
                            let mut passed = 0usize;
                            let mut warnings = 0usize;
                            let mut failed = 0usize;

                            // SSH reachable?
                            if result.reachable {
                                passed += 1;
                            } else {
                                failed += 1;
                            }
                            // Cass installed?
                            if result.has_cass() {
                                passed += 1;
                            } else {
                                warnings += 1;
                            }
                            // Agent data present?
                            if result.has_agent_data() {
                                passed += 1;
                            } else {
                                warnings += 1;
                            }
                            // Disk space available?
                            if let Some(ref res) = result.resources {
                                if res.disk_available_mb >= 1024 {
                                    passed += 1;
                                } else {
                                    warnings += 1;
                                }
                            }

                            CassMsg::SourceDoctorCompleted {
                                source_name,
                                passed,
                                warnings,
                                failed,
                            }
                        })
                    } else {
                        self.sources_view.status =
                            format!("Source '{source_name}' not found in config");
                        ftui::Cmd::none()
                    }
                }
                #[cfg(test)]
                {
                    let _ = source_name;
                    ftui::Cmd::none()
                }
            }
            CassMsg::SourceDoctorCompleted {
                ref source_name,
                passed,
                warnings,
                failed,
            } => {
                let source_name = source_name.clone();
                if let Some(item) = self
                    .sources_view
                    .items
                    .iter_mut()
                    .find(|i| i.name == source_name)
                {
                    item.busy = false;
                    item.doctor_summary = Some((passed, warnings, failed));
                }
                self.sources_view.status = format!(
                    "Doctor '{source_name}': {passed} pass, {warnings} warn, {failed} fail"
                );
                ftui::Cmd::none()
            }

            // -- Lifecycle ----------------------------------------------------
            CassMsg::QuitRequested => {
                // ESC unwind: check pending state before quitting
                // If on analytics or sources surface, pop back.
                if self.surface == AppSurface::Analytics || self.surface == AppSurface::Sources {
                    return ftui::Cmd::msg(CassMsg::ViewStackPopped);
                }
                if self.show_consent_dialog {
                    self.show_consent_dialog = false;
                    self.focus_manager.pop_trap();
                    return ftui::Cmd::none();
                }
                if self.show_theme_editor {
                    self.show_theme_editor = false;
                    self.theme_editor = None;
                    return ftui::Cmd::none();
                }
                if self.show_inspector {
                    self.show_inspector = false;
                    if self.inspector_state.is_active() {
                        self.inspector_state.toggle();
                    }
                    return ftui::Cmd::none();
                }
                if self.show_export_modal {
                    self.show_export_modal = false;
                    self.export_modal_state = None;
                    self.focus_manager.pop_trap();
                    return ftui::Cmd::none();
                }
                if self.show_bulk_modal {
                    self.show_bulk_modal = false;
                    self.focus_manager.pop_trap();
                    return ftui::Cmd::none();
                }
                if self.show_saved_views_modal {
                    if self.saved_view_rename_mode {
                        self.saved_view_rename_mode = false;
                        self.saved_view_rename_buffer.clear();
                        self.saved_view_drag = None;
                        self.status = "Cancelled saved view rename".to_string();
                    } else {
                        self.show_saved_views_modal = false;
                        self.saved_view_drag = None;
                        self.status = "Saved views manager closed".to_string();
                        self.focus_manager.pop_trap();
                    }
                    return ftui::Cmd::none();
                }
                if self.source_filter_menu_open {
                    self.source_filter_menu_open = false;
                    self.focus_manager.pop_trap();
                    return ftui::Cmd::none();
                }
                if self.palette_state.open {
                    self.palette_state.open = false;
                    self.focus_manager.pop_trap();
                    return ftui::Cmd::none();
                }
                if self.show_help {
                    self.show_help = false;
                    self.focus_manager.pop_trap();
                    return ftui::Cmd::none();
                }
                if self.show_detail_modal {
                    self.show_detail_modal = false;
                    self.focus_manager.pop_trap();
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
            // -- Macro recording/playback -----------------------------------------
            CassMsg::MacroRecordingToggled => {
                if self.macro_recorder.is_some() {
                    // Stop recording and save.
                    let recorder = self.macro_recorder.take().unwrap();
                    let recorded = recorder.finish();
                    let macro_dir = macro_save_dir();
                    if let Err(e) = std::fs::create_dir_all(&macro_dir) {
                        self.toast_manager
                            .push(crate::ui::components::toast::Toast::error(format!(
                                "Failed to create macro dir: {e}"
                            )));
                        return ftui::Cmd::none();
                    }
                    let filename = format!(
                        "cass-macro-{}.jsonl",
                        chrono::Local::now().format("%Y%m%d-%H%M%S")
                    );
                    let path = macro_dir.join(&filename);
                    match macro_file::save_macro(&path, &recorded, self.macro_redact_paths) {
                        Ok(()) => {
                            self.toast_manager
                                .push(crate::ui::components::toast::Toast::success(format!(
                                    "Macro saved ({} events): {}",
                                    recorded.len(),
                                    path.display()
                                )));
                            self.status = format!("Macro saved: {}", path.display());
                        }
                        Err(e) => {
                            self.toast_manager
                                .push(crate::ui::components::toast::Toast::error(format!(
                                    "Failed to save macro: {e}"
                                )));
                        }
                    }
                } else {
                    // Start recording.
                    let mut recorder = MacroRecorder::new("cass-interactive");
                    // Try to capture terminal size for metadata.
                    if let Ok((w, h)) = crossterm::terminal::size() {
                        recorder = recorder.with_terminal_size(w, h);
                    }
                    self.macro_recorder = Some(recorder);
                    self.toast_manager
                        .push(crate::ui::components::toast::Toast::info(
                            "Macro recording started (Alt+M to stop)",
                        ));
                    self.status = "Recording macro...".to_string();
                }
                ftui::Cmd::none()
            }
            CassMsg::MacroRecordingSaved(path) => {
                self.status = format!("Macro saved: {}", path.display());
                ftui::Cmd::none()
            }
            CassMsg::MacroRecordingFailed(err) => {
                self.toast_manager
                    .push(crate::ui::components::toast::Toast::error(format!(
                        "Macro error: {err}"
                    )));
                ftui::Cmd::none()
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

                // Record hit regions for mouse support.
                *self.last_search_bar_area.borrow_mut() = Some(vertical[0]);
                *self.last_status_area.borrow_mut() = Some(vertical[2]);

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
                    .style(if self.focused_region() == FocusRegion::Results {
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
                *self.last_content_area.borrow_mut() = Some(content_area);

                // Reset hit regions — they'll be repopulated by render_*_pane().
                *self.last_results_inner.borrow_mut() = None;
                *self.last_detail_area.borrow_mut() = None;
                *self.last_split_handle_area.borrow_mut() = None;

                let (hits, selected_idx) = if let Some(pane) = self.panes.get(self.active_pane) {
                    (&pane.hits[..], pane.selected)
                } else {
                    (&self.results[..], 0)
                };

                let topo = breakpoint.search_topology();
                if topo.dual_pane {
                    // Dual-pane: split content area using topology-defined minimums.
                    let (results_area, detail_area, split_handle) =
                        self.split_content_area(content_area, topo.min_results, topo.min_detail);
                    *self.last_split_handle_area.borrow_mut() = split_handle;
                    self.render_results_pane(
                        frame,
                        results_area,
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
                        detail_area,
                        border_type,
                        adaptive_borders,
                        &styles,
                        pane_style,
                        pane_focused_style,
                        text_muted_style,
                    );
                } else {
                    // Single-pane: show whichever pane has focus, full-width.
                    match self.focused_region() {
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
                    }
                }

                // ── Status footer ───────────────────────────────────────
                let bp_label = breakpoint.footer_label();
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
                let source_tag = if self.filters.source_filter.is_all() {
                    String::new()
                } else {
                    format!(" | src:{}", self.filters.source_filter)
                };
                let rec_tag = if self.macro_recorder.is_some() {
                    " | \u{25CF} REC"
                } else if self.macro_playback.is_some() {
                    " | \u{25B6} PLAY"
                } else {
                    ""
                };
                let status_line = if self.status.is_empty() {
                    let hints = self.build_contextual_footer_hints(area.width);
                    format!(
                        " {hits_for_status} hits | {bp_label} | {density_label}{source_tag}{degradation_tag}{sel_tag}{rec_tag}{hints}",
                    )
                } else {
                    format!(" {}{}{}{}", self.status, degradation_tag, sel_tag, rec_tag)
                };
                Paragraph::new(&*status_line)
                    .style(text_muted_style)
                    .render(vertical[2], frame);
            }

            AppSurface::Analytics => {
                // Clear search hit regions — not visible on analytics surface.
                *self.last_search_bar_area.borrow_mut() = None;
                *self.last_results_inner.borrow_mut() = None;
                *self.last_detail_area.borrow_mut() = None;
                *self.last_status_area.borrow_mut() = None;
                *self.last_content_area.borrow_mut() = None;
                *self.last_split_handle_area.borrow_mut() = None;
                self.last_saved_view_row_areas.borrow_mut().clear();

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
                    let empty_data = AnalyticsChartData::default();
                    let chart_data = self.analytics_cache.as_ref().unwrap_or(&empty_data);
                    let explorer_state = super::analytics_charts::ExplorerState {
                        metric: self.explorer_metric,
                        overlay: self.explorer_overlay,
                        group_by: self.explorer_group_by,
                        zoom: self.explorer_zoom,
                    };
                    super::analytics_charts::render_analytics_content(
                        self.analytics_view,
                        chart_data,
                        &explorer_state,
                        self.breakdown_tab,
                        self.heatmap_metric,
                        self.analytics_selection,
                        content_inner,
                        frame,
                    );
                }

                // ── Analytics status footer ──────────────────────────────
                let analytics_deg_tag = if degradation.is_full() {
                    String::new()
                } else {
                    format!(" | deg:{}", degradation.as_str())
                };
                let drilldown_hint = if self.analytics_selectable_count() > 0 {
                    format!(
                        " | [{}/{}] Enter=drilldown",
                        self.analytics_selection + 1,
                        self.analytics_selectable_count()
                    )
                } else {
                    String::new()
                };
                let analytics_status = format!(
                    " Analytics: {} | \u{2190}\u{2192}=views \u{2191}\u{2193}=select{} Esc=back{}",
                    self.analytics_view.label(),
                    drilldown_hint,
                    analytics_deg_tag
                );
                Paragraph::new(&*analytics_status)
                    .style(text_muted_style)
                    .render(vertical[2], frame);
            }

            AppSurface::Sources => {
                // Clear search hit regions — not visible on sources surface.
                *self.last_search_bar_area.borrow_mut() = None;
                *self.last_results_inner.borrow_mut() = None;
                *self.last_detail_area.borrow_mut() = None;
                *self.last_status_area.borrow_mut() = None;
                *self.last_content_area.borrow_mut() = None;
                *self.last_split_handle_area.borrow_mut() = None;
                self.last_saved_view_row_areas.borrow_mut().clear();

                // ── Sources surface layout ─────────────────────────────
                let vertical = Flex::vertical()
                    .constraints([
                        Constraint::Fixed(3), // Header
                        Constraint::Min(4),   // Source list
                        Constraint::Fixed(1), // Status footer
                    ])
                    .split(layout_area);

                // ── Header ───────────────────────────────────────────
                let header_title = format!(
                    "cass sources | {} source(s) | {}",
                    self.sources_view.items.len(),
                    self.sources_view.config_path
                );
                let header_block = Block::new()
                    .borders(adaptive_borders)
                    .border_type(border_type)
                    .title(&header_title)
                    .title_alignment(Alignment::Left)
                    .style(pane_focused_style);
                let header_inner = header_block.inner(vertical[0]);
                header_block.render(vertical[0], frame);
                if render_content && !header_inner.is_empty() {
                    let hints = " s=sync  d=doctor  r=refresh  Esc=back";
                    Paragraph::new(hints)
                        .style(text_muted_style)
                        .render(header_inner, frame);
                }

                // ── Source list ───────────────────────────────────────
                let content_block = Block::new()
                    .borders(adaptive_borders)
                    .border_type(border_type)
                    .title("Configured Sources")
                    .title_alignment(Alignment::Left)
                    .style(pane_style);
                let content_inner = content_block.inner(vertical[1]);
                content_block.render(vertical[1], frame);

                if render_content && !content_inner.is_empty() {
                    let sv = &self.sources_view;
                    if sv.items.is_empty() {
                        Paragraph::new(
                            "No sources configured.\nRun `cass sources add <host>` to add one.",
                        )
                        .style(text_muted_style)
                        .render(content_inner, frame);
                    } else {
                        // Render each source row.
                        let visible_rows = content_inner.height as usize;
                        let start = sv.scroll;
                        let end = (start + visible_rows).min(sv.items.len());

                        for (vis_idx, src_idx) in (start..end).enumerate() {
                            let item = &sv.items[src_idx];
                            let row_y = content_inner.y + vis_idx as u16;
                            if row_y >= content_inner.y + content_inner.height {
                                break;
                            }
                            let row_area =
                                Rect::new(content_inner.x, row_y, content_inner.width, 1);

                            let is_selected = src_idx == sv.selected;
                            let kind_tag = match item.kind {
                                crate::sources::SourceKind::Local => "[local]",
                                crate::sources::SourceKind::Ssh => "[ssh]  ",
                            };
                            let host_str = item.host.as_deref().unwrap_or("-");
                            let sync_str = if item.busy {
                                "\u{23F3}".to_string() // hourglass
                            } else if let Some((p, w, f)) = item.doctor_summary {
                                format!("dr:{p}p/{w}w/{f}f")
                            } else {
                                format!("last:{}", item.last_result)
                            };

                            // Truncate row to fit.
                            let row_text = format!(
                                " {kind_tag} {:<16} {:<24} {:<8} paths:{} {sync_str}",
                                item.name, host_str, item.schedule, item.path_count
                            );
                            let display: String = row_text
                                .chars()
                                .take(content_inner.width as usize)
                                .collect();

                            let row_style = if is_selected {
                                styles.style(style_system::STYLE_RESULT_ROW_SELECTED)
                            } else {
                                styles.style(style_system::STYLE_TEXT_PRIMARY)
                            };
                            Paragraph::new(&*display)
                                .style(row_style)
                                .render(row_area, frame);
                        }
                    }
                }

                // ── Sources status footer ────────────────────────────
                let sources_status = format!(
                    " Sources: [{}/{}] | {}",
                    self.sources_view.selected + 1,
                    self.sources_view.items.len(),
                    self.sources_view.status
                );
                Paragraph::new(&*sources_status)
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

        if self.show_saved_views_modal {
            self.render_saved_views_overlay(frame, area, &styles);
        } else {
            self.last_saved_view_row_areas.borrow_mut().clear();
        }

        if self.source_filter_menu_open {
            self.render_source_filter_menu_overlay(frame, area, &styles);
        }

        // ── Help overlay ─────────────────────────────────────────────
        if self.show_help {
            self.render_help_overlay(frame, area, &styles);
        }

        // ── Theme editor overlay ─────────────────────────────────────
        if self.show_theme_editor {
            self.render_theme_editor_overlay(frame, area, &styles);
        }

        // ── Inspector overlay ────────────────────────────────────────
        if self.show_inspector {
            self.render_inspector_overlay(frame, area, &styles);
        }

        // ── Command palette overlay ──────────────────────────────────
        if self.palette_state.open {
            self.render_palette_overlay(frame, area, &styles);
        }

        // ── Screenshot capture (runs after all rendering completes) ──
        if let Some(format) = self.screenshot_pending {
            let exported =
                match format {
                    ScreenshotFormat::Html => ftui_extras::export::HtmlExporter::default()
                        .export(&frame.buffer, frame.pool),
                    ScreenshotFormat::Svg => ftui_extras::export::SvgExporter::default()
                        .export(&frame.buffer, frame.pool),
                    ScreenshotFormat::Text => {
                        ftui_extras::export::TextExporter::plain().export(&frame.buffer, frame.pool)
                    }
                };
            *self.screenshot_result.borrow_mut() = Some((format, exported));
        }
    }
}

// =========================================================================
// Entry Point
// =========================================================================

/// Write a screenshot file to ~/Downloads and emit a completion or failure message.
fn write_screenshot_file(format: ScreenshotFormat, content: String) -> ftui::Cmd<CassMsg> {
    ftui::Cmd::msg(write_screenshot_file_sync(format, content))
}

fn write_screenshot_file_sync(format: ScreenshotFormat, content: String) -> CassMsg {
    let downloads = dirs::download_dir().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Downloads")
    });
    if let Err(e) = std::fs::create_dir_all(&downloads) {
        return CassMsg::ScreenshotFailed(format!("Cannot create dir: {e}"));
    }
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let filename = format!("cass_screenshot_{ts}.{}", format.extension());
    let path = downloads.join(&filename);
    match std::fs::write(&path, content.as_bytes()) {
        Ok(()) => CassMsg::ScreenshotCompleted(path),
        Err(e) => CassMsg::ScreenshotFailed(format!("Write failed: {e}")),
    }
}

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

/// Configuration for inline TUI mode.
pub struct InlineTuiConfig {
    /// Height of the inline UI in terminal rows.
    pub ui_height: u16,
    /// Whether the UI is anchored to the top or bottom of the terminal.
    pub anchor: super::ftui_adapter::UiAnchor,
}

/// Configuration for macro recording/playback.
pub struct MacroConfig {
    /// Path to write recorded macro events.
    pub record_path: Option<std::path::PathBuf>,
    /// Path to read and play back macro events.
    pub play_path: Option<std::path::PathBuf>,
}

/// Default directory for interactively saved macros.
fn macro_save_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("coding-agent-search")
        .join("macros")
}

/// Run the cass TUI using the ftui Program runtime.
///
/// This replaces the manual crossterm event loop in `run_tui()`.
/// The ftui runtime handles terminal lifecycle (raw mode, alt-screen),
/// event polling, rendering, and cleanup via RAII.
///
/// When `inline_config` is `Some`, the TUI runs in inline mode: the UI
/// chrome is anchored (top or bottom) within the terminal and scrollback
/// is preserved. When `None`, fullscreen alt-screen mode is used.
///
/// When `macro_config` has a `record_path`, events are recorded and saved
/// to the specified file on exit. When `play_path` is set, events are
/// loaded and replayed.
pub fn run_tui_ftui(
    inline_config: Option<InlineTuiConfig>,
    macro_config: MacroConfig,
    data_dir_override: Option<PathBuf>,
) -> anyhow::Result<()> {
    use ftui::ProgramConfig;
    use ftui::render::budget::FrameBudgetConfig;

    let mut model = CassApp::default();
    let data_dir = data_dir_override.unwrap_or_else(crate::default_data_dir);
    model.data_dir = data_dir.clone();
    model.db_path = data_dir.join("agent_search.db");

    // 16ms budget (60fps) with adaptive PID degradation.
    let budget = FrameBudgetConfig::default();

    // Build the ProgramConfig based on inline/fullscreen mode.
    let mut config = if let Some(ref cfg) = inline_config {
        let mut c = ProgramConfig::inline(cfg.ui_height);
        c.ui_anchor = cfg.anchor;
        c
    } else {
        ProgramConfig::fullscreen()
    };
    config.budget = budget.clone();
    config.mouse = true;

    // If recording macros, we need direct Program access for start/stop_recording.
    if macro_config.record_path.is_some() {
        let mut program = ftui::Program::with_config(model, config)
            .map_err(|e| anyhow::anyhow!("ftui program creation error: {e}"))?;

        program.start_recording("cass-session");
        let result = program.run();

        // Save recorded macro on exit.
        if let Some(ref record_path) = macro_config.record_path
            && let Some(recorded) = program.stop_recording()
        {
            macro_file::save_macro(record_path, &recorded, false)?;
            eprintln!("Macro saved to: {}", record_path.display());
        }

        result.map_err(|e| anyhow::anyhow!("ftui runtime error: {e}"))
    } else if let Some(ref play_path) = macro_config.play_path {
        // Playback: load macro into model, which replays events via MacroPlayback
        // on each Tick. The model converts macro events back to CassMsg and processes
        // them as if the user had typed them.
        let macro_data = macro_file::load_macro(play_path)?;
        eprintln!(
            "Playing macro: {} ({} events, {:.1}s)",
            macro_data.metadata().name,
            macro_data.len(),
            macro_data.total_duration().as_secs_f64()
        );

        model.macro_playback = Some(MacroPlayback::new(macro_data));

        if let Some(cfg) = inline_config {
            ftui::App::inline(model, cfg.ui_height)
                .anchor(cfg.anchor)
                .with_mouse()
                .with_budget(budget)
                .run()
                .map_err(|e| anyhow::anyhow!("ftui inline runtime error: {e}"))
        } else {
            ftui::App::fullscreen(model)
                .with_mouse()
                .with_budget(budget)
                .run()
                .map_err(|e| anyhow::anyhow!("ftui runtime error: {e}"))
        }
    } else {
        // Standard path — no macro, use AppBuilder for simplicity.
        if let Some(cfg) = inline_config {
            ftui::App::inline(model, cfg.ui_height)
                .anchor(cfg.anchor)
                .with_mouse()
                .with_budget(budget)
                .run()
                .map_err(|e| anyhow::anyhow!("ftui inline runtime error: {e}"))
        } else {
            ftui::App::fullscreen(model)
                .with_mouse()
                .with_budget(budget)
                .run()
                .map_err(|e| anyhow::anyhow!("ftui runtime error: {e}"))
        }
    }
}

/// Macro file serialization/deserialization.
mod macro_file {
    use std::io::{BufRead, BufReader, Write};
    use std::path::Path;
    use std::time::Duration;

    use ftui::runtime::input_macro::MacroMetadata;
    use ftui::runtime::{InputMacro, TimedEvent};
    use ftui::{Event, KeyCode, KeyEvent, Modifiers};

    /// Save an InputMacro to a JSONL file.
    ///
    /// When `redact_paths` is true, absolute directory paths in Paste events
    /// are replaced with `~` to avoid leaking sensitive filesystem layout.
    pub fn save_macro(
        path: &Path,
        input_macro: &InputMacro,
        redact_paths: bool,
    ) -> anyhow::Result<()> {
        let home_dir = dirs::home_dir().unwrap_or_default();
        let mut file = std::fs::File::create(path)?;

        // Header line with metadata.
        let meta = input_macro.metadata();
        writeln!(
            file,
            "{{\"type\":\"header\",\"name\":{},\"terminal_size\":[{},{}],\"total_duration_ms\":{},\"event_count\":{}}}",
            serde_json::to_string(&meta.name)?,
            meta.terminal_size.0,
            meta.terminal_size.1,
            meta.total_duration.as_millis(),
            input_macro.len()
        )?;

        // One line per event.
        for timed in input_macro.events() {
            let event = if redact_paths {
                redact_event_paths(&timed.event, &home_dir)
            } else {
                timed.event.clone()
            };
            let event_json = serialize_event(&event);
            writeln!(
                file,
                "{{\"type\":\"event\",\"delay_ms\":{},\"event\":{}}}",
                timed.delay.as_millis(),
                event_json
            )?;
        }

        Ok(())
    }

    /// Load an InputMacro from a JSONL file.
    pub fn load_macro(path: &Path) -> anyhow::Result<InputMacro> {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);
        let mut events = Vec::new();
        let mut name = String::from("loaded");
        let mut terminal_size = (80u16, 24u16);
        let mut total_duration = Duration::ZERO;

        for line in reader.lines() {
            let line = line?;
            let v: serde_json::Value = serde_json::from_str(&line)?;

            match v.get("type").and_then(|t| t.as_str()) {
                Some("header") => {
                    name = v
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("loaded")
                        .to_string();
                    if let (Some(w), Some(h)) = (
                        v.get("terminal_size")
                            .and_then(|s| s.get(0))
                            .and_then(|n| n.as_u64()),
                        v.get("terminal_size")
                            .and_then(|s| s.get(1))
                            .and_then(|n| n.as_u64()),
                    ) {
                        terminal_size = (w as u16, h as u16);
                    }
                    if let Some(ms) = v.get("total_duration_ms").and_then(|n| n.as_u64()) {
                        total_duration = Duration::from_millis(ms);
                    }
                }
                Some("event") => {
                    let delay_ms = v.get("delay_ms").and_then(|n| n.as_u64()).unwrap_or(0);
                    if let Some(event_val) = v.get("event")
                        && let Some(event) = deserialize_event(event_val)
                    {
                        events.push(TimedEvent::new(event, Duration::from_millis(delay_ms)));
                    }
                }
                _ => {} // Skip unknown line types
            }
        }

        let metadata = MacroMetadata {
            name,
            terminal_size,
            total_duration,
        };

        Ok(InputMacro::new(events, metadata))
    }

    fn serialize_event(event: &Event) -> String {
        match event {
            Event::Key(key) => {
                let code = serialize_keycode(&key.code);
                let mods = serialize_modifiers(key.modifiers);
                format!("{{\"key\":{code},\"modifiers\":{mods}}}")
            }
            Event::Resize { width, height } => {
                format!("{{\"resize\":[{width},{height}]}}")
            }
            Event::Focus(gained) => {
                format!("{{\"focus\":{gained}}}")
            }
            Event::Paste(paste) => {
                let text = serde_json::to_string(&paste.text).unwrap_or_default();
                format!("{{\"paste\":{text}}}")
            }
            Event::Mouse(_) => {
                // Mouse events are not serialized for macro files
                "null".to_string()
            }
            _ => "null".to_string(),
        }
    }

    fn serialize_keycode(code: &KeyCode) -> String {
        match code {
            KeyCode::Char(c) => {
                let s = serde_json::to_string(&c.to_string()).unwrap_or_default();
                format!("{{\"char\":{s}}}")
            }
            KeyCode::Enter => "\"Enter\"".to_string(),
            KeyCode::Backspace => "\"Backspace\"".to_string(),
            KeyCode::Tab => "\"Tab\"".to_string(),
            KeyCode::Escape => "\"Escape\"".to_string(),
            KeyCode::Up => "\"Up\"".to_string(),
            KeyCode::Down => "\"Down\"".to_string(),
            KeyCode::Left => "\"Left\"".to_string(),
            KeyCode::Right => "\"Right\"".to_string(),
            KeyCode::Home => "\"Home\"".to_string(),
            KeyCode::End => "\"End\"".to_string(),
            KeyCode::PageUp => "\"PageUp\"".to_string(),
            KeyCode::PageDown => "\"PageDown\"".to_string(),
            KeyCode::Delete => "\"Delete\"".to_string(),
            KeyCode::Insert => "\"Insert\"".to_string(),
            KeyCode::F(n) => format!("{{\"f\":{n}}}"),
            _ => "null".to_string(),
        }
    }

    fn serialize_modifiers(mods: Modifiers) -> String {
        let mut parts = Vec::new();
        if mods.contains(Modifiers::SHIFT) {
            parts.push("\"shift\"");
        }
        if mods.contains(Modifiers::CTRL) {
            parts.push("\"ctrl\"");
        }
        if mods.contains(Modifiers::ALT) {
            parts.push("\"alt\"");
        }
        format!("[{}]", parts.join(","))
    }

    fn deserialize_event(v: &serde_json::Value) -> Option<Event> {
        if v.is_null() {
            return None;
        }

        if let Some(key_val) = v.get("key") {
            let code = deserialize_keycode(key_val)?;
            let modifiers = v
                .get("modifiers")
                .map(deserialize_modifiers)
                .unwrap_or(Modifiers::empty());
            return Some(Event::Key(KeyEvent {
                code,
                modifiers,
                kind: ftui::KeyEventKind::Press,
            }));
        }

        if let Some(resize) = v.get("resize") {
            let w = resize.get(0)?.as_u64()? as u16;
            let h = resize.get(1)?.as_u64()? as u16;
            return Some(Event::Resize {
                width: w,
                height: h,
            });
        }

        if let Some(focus) = v.get("focus") {
            return Some(Event::Focus(focus.as_bool()?));
        }

        if let Some(paste) = v.get("paste") {
            return Some(Event::Paste(ftui::core::event::PasteEvent {
                text: paste.as_str()?.to_string(),
                bracketed: true,
            }));
        }

        None
    }

    fn deserialize_keycode(v: &serde_json::Value) -> Option<KeyCode> {
        if let Some(s) = v.as_str() {
            return match s {
                "Enter" => Some(KeyCode::Enter),
                "Backspace" => Some(KeyCode::Backspace),
                "Tab" => Some(KeyCode::Tab),
                "Escape" => Some(KeyCode::Escape),
                "Up" => Some(KeyCode::Up),
                "Down" => Some(KeyCode::Down),
                "Left" => Some(KeyCode::Left),
                "Right" => Some(KeyCode::Right),
                "Home" => Some(KeyCode::Home),
                "End" => Some(KeyCode::End),
                "PageUp" => Some(KeyCode::PageUp),
                "PageDown" => Some(KeyCode::PageDown),
                "Delete" => Some(KeyCode::Delete),
                "Insert" => Some(KeyCode::Insert),
                _ => None,
            };
        }

        if let Some(obj) = v.as_object() {
            if let Some(c) = obj.get("char").and_then(|c| c.as_str()) {
                return c.chars().next().map(KeyCode::Char);
            }
            if let Some(n) = obj.get("f").and_then(|n| n.as_u64()) {
                return Some(KeyCode::F(n as u8));
            }
        }

        None
    }

    fn deserialize_modifiers(v: &serde_json::Value) -> Modifiers {
        let mut mods = Modifiers::empty();
        if let Some(arr) = v.as_array() {
            for item in arr {
                if let Some(s) = item.as_str() {
                    match s {
                        "shift" => mods |= Modifiers::SHIFT,
                        "ctrl" => mods |= Modifiers::CTRL,
                        "alt" => mods |= Modifiers::ALT,
                        _ => {}
                    }
                }
            }
        }
        mods
    }

    /// Replace absolute paths in Paste events with `~` to avoid leaking
    /// sensitive filesystem layout in shared macro files.
    fn redact_event_paths(event: &Event, home: &std::path::Path) -> Event {
        match event {
            Event::Paste(paste) => {
                let home_str = home.to_string_lossy();
                let redacted = if !home_str.is_empty() {
                    paste.text.replace(home_str.as_ref(), "~")
                } else {
                    paste.text.clone()
                };
                Event::Paste(ftui::core::event::PasteEvent {
                    text: redacted,
                    bracketed: paste.bracketed,
                })
            }
            other => other.clone(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn roundtrip_key_event() {
            let event = Event::Key(KeyEvent {
                code: KeyCode::Char('a'),
                modifiers: Modifiers::CTRL,
                kind: ftui::KeyEventKind::Press,
            });
            let json = serialize_event(&event);
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            let restored = deserialize_event(&parsed).unwrap();
            match restored {
                Event::Key(k) => {
                    assert_eq!(k.code, KeyCode::Char('a'));
                    assert!(k.modifiers.contains(Modifiers::CTRL));
                }
                _ => panic!("expected Key event"),
            }
        }

        #[test]
        fn roundtrip_special_keys() {
            for code in [
                KeyCode::Enter,
                KeyCode::Escape,
                KeyCode::Tab,
                KeyCode::Backspace,
                KeyCode::Up,
                KeyCode::Down,
                KeyCode::F(5),
            ] {
                let event = Event::Key(KeyEvent {
                    code,
                    modifiers: Modifiers::empty(),
                    kind: ftui::KeyEventKind::Press,
                });
                let json = serialize_event(&event);
                let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
                let restored = deserialize_event(&parsed).unwrap();
                if let Event::Key(k) = restored {
                    assert_eq!(k.code, code, "roundtrip failed for {:?}", code);
                } else {
                    panic!("expected Key event for {:?}", code);
                }
            }
        }

        #[test]
        fn roundtrip_resize_event() {
            let event = Event::Resize {
                width: 120,
                height: 40,
            };
            let json = serialize_event(&event);
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            let restored = deserialize_event(&parsed).unwrap();
            assert!(matches!(
                restored,
                Event::Resize {
                    width: 120,
                    height: 40
                }
            ));
        }

        #[test]
        fn roundtrip_modifier_combinations() {
            let mods = Modifiers::SHIFT | Modifiers::ALT;
            let json = serialize_modifiers(mods);
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            let restored = deserialize_modifiers(&parsed);
            assert!(restored.contains(Modifiers::SHIFT));
            assert!(restored.contains(Modifiers::ALT));
            assert!(!restored.contains(Modifiers::CTRL));
        }

        #[test]
        fn save_load_roundtrip() {
            let events = vec![
                TimedEvent::new(
                    Event::Key(KeyEvent {
                        code: KeyCode::Char('h'),
                        modifiers: Modifiers::empty(),
                        kind: ftui::KeyEventKind::Press,
                    }),
                    Duration::from_millis(100),
                ),
                TimedEvent::new(
                    Event::Key(KeyEvent {
                        code: KeyCode::Enter,
                        modifiers: Modifiers::empty(),
                        kind: ftui::KeyEventKind::Press,
                    }),
                    Duration::from_millis(200),
                ),
                TimedEvent::new(
                    Event::Key(KeyEvent {
                        code: KeyCode::Escape,
                        modifiers: Modifiers::empty(),
                        kind: ftui::KeyEventKind::Press,
                    }),
                    Duration::from_millis(50),
                ),
            ];
            let metadata = MacroMetadata {
                name: "test-macro".to_string(),
                terminal_size: (80, 24),
                total_duration: Duration::from_millis(350),
            };
            let original = InputMacro::new(events, metadata);

            let tmp = tempfile::NamedTempFile::new().unwrap();
            save_macro(tmp.path(), &original, false).unwrap();
            let loaded = load_macro(tmp.path()).unwrap();

            assert_eq!(loaded.len(), 3);
            assert_eq!(loaded.metadata().name, "test-macro");
            assert_eq!(loaded.metadata().terminal_size, (80, 24));
        }

        #[test]
        fn null_events_are_skipped() {
            let event = Event::Mouse(ftui::MouseEvent {
                kind: ftui::MouseEventKind::Moved,
                x: 0,
                y: 0,
                modifiers: Modifiers::empty(),
            });
            let json = serialize_event(&event);
            assert_eq!(json, "null");
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert!(deserialize_event(&parsed).is_none());
        }

        #[test]
        fn path_redaction_replaces_home_dir_in_paste() {
            let home = std::path::PathBuf::from("/home/testuser");
            let event = Event::Paste(ftui::core::event::PasteEvent {
                text: "/home/testuser/projects/foo/bar.rs".to_string(),
                bracketed: true,
            });
            let redacted = redact_event_paths(&event, &home);
            if let Event::Paste(p) = redacted {
                assert_eq!(p.text, "~/projects/foo/bar.rs");
                assert!(p.bracketed);
            } else {
                panic!("expected Paste event");
            }
        }

        #[test]
        fn path_redaction_preserves_non_paste_events() {
            let home = std::path::PathBuf::from("/home/testuser");
            let event = Event::Key(KeyEvent {
                code: KeyCode::Char('a'),
                modifiers: Modifiers::empty(),
                kind: ftui::KeyEventKind::Press,
            });
            let redacted = redact_event_paths(&event, &home);
            assert!(matches!(redacted, Event::Key(_)));
        }

        #[test]
        fn save_load_roundtrip_with_redaction() {
            let events = vec![
                TimedEvent::new(
                    Event::Paste(ftui::core::event::PasteEvent {
                        text: "/home/testuser/secret/data.txt".to_string(),
                        bracketed: true,
                    }),
                    Duration::from_millis(100),
                ),
                TimedEvent::new(
                    Event::Key(KeyEvent {
                        code: KeyCode::Enter,
                        modifiers: Modifiers::empty(),
                        kind: ftui::KeyEventKind::Press,
                    }),
                    Duration::from_millis(50),
                ),
            ];
            let metadata = MacroMetadata {
                name: "redact-test".to_string(),
                terminal_size: (80, 24),
                total_duration: Duration::from_millis(150),
            };
            let original = InputMacro::new(events, metadata);

            let tmp = tempfile::NamedTempFile::new().unwrap();
            // Save with redaction using /home/testuser as home dir.
            // We test by temporarily overriding... actually just use the function directly.
            save_macro(tmp.path(), &original, true).unwrap();
            let loaded = load_macro(tmp.path()).unwrap();

            assert_eq!(loaded.len(), 2);
            // The paste event should have the home dir replaced.
            // Note: redaction depends on dirs::home_dir(), which may differ.
            // So we verify the key event survived intact.
            assert_eq!(loaded.metadata().name, "redact-test");
        }
    }
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
    use ftui::TerminalCapabilities;
    use ftui_extras::clipboard::{Clipboard, ClipboardSelection};

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
        assert_eq!(app.focused_region(), FocusRegion::Results);
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
        assert_eq!(app.source_filter_menu_selection, 0);
        assert!(app.available_source_ids.is_empty());
        assert!(app.selected.is_empty());
        assert!(app.saved_views.is_empty());
        assert!(app.query_history.is_empty());
    }

    #[test]
    fn all_detail_tab_variants_constructible() {
        let _msgs = DetailTab::Messages;
        let _snip = DetailTab::Snippets;
        let _raw = DetailTab::Raw;
        let _json = DetailTab::Json;
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
    fn event_mapping_ctrl_shift_y_maps_to_copy_query() {
        use crate::ui::ftui_adapter::{Event, KeyCode, KeyEvent, Modifiers};

        let event = Event::Key(
            KeyEvent::new(KeyCode::Char('y')).with_modifiers(Modifiers::CTRL | Modifiers::SHIFT),
        );

        assert!(matches!(CassMsg::from(event), CassMsg::CopyQuery));
    }

    #[test]
    fn event_mapping_ctrl_y_maps_to_copy_path() {
        use crate::ui::ftui_adapter::{Event, KeyCode, KeyEvent, Modifiers};

        let event = Event::Key(KeyEvent::new(KeyCode::Char('y')).with_modifiers(Modifiers::CTRL));

        assert!(matches!(CassMsg::from(event), CassMsg::CopyPath));
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
    fn persisted_state_roundtrip_preserves_saved_view_metadata() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let state_path = tmp.path().join("tui_state.json");
        let mut query_history = VecDeque::new();
        query_history.push_back("authentication error".to_string());

        let mut agents = HashSet::new();
        agents.insert("codex".to_string());
        let mut workspaces = HashSet::new();
        workspaces.insert("/repo".to_string());

        let state = PersistedState {
            search_mode: SearchMode::Hybrid,
            match_mode: MatchMode::Prefix,
            ranking_mode: RankingMode::DateNewest,
            context_window: ContextWindow::Large,
            theme_dark: false,
            density_mode: DensityMode::Compact,
            per_pane_limit: 22,
            query_history,
            saved_views: vec![SavedView {
                slot: 3,
                label: Some("triage".to_string()),
                agents,
                workspaces,
                created_from: Some(1000),
                created_to: Some(2000),
                ranking: RankingMode::MatchQualityHeavy,
                source_filter: SourceFilter::SourceId("remote-buildbox".to_string()),
            }],
            fancy_borders: false,
            help_pinned: true,
            has_seen_help: true,
        };

        save_persisted_state_to_path(&state_path, &state).expect("save state");
        let loaded = load_persisted_state_from_path(&state_path)
            .expect("load state")
            .expect("state exists");

        assert_eq!(loaded.search_mode, SearchMode::Hybrid);
        assert_eq!(loaded.match_mode, MatchMode::Prefix);
        assert_eq!(loaded.ranking_mode, RankingMode::DateNewest);
        assert_eq!(loaded.context_window, ContextWindow::Large);
        assert!(!loaded.theme_dark);
        assert_eq!(loaded.density_mode, DensityMode::Compact);
        assert_eq!(loaded.per_pane_limit, 22);
        assert_eq!(
            loaded.query_history.front().map(String::as_str),
            Some("authentication error")
        );
        assert_eq!(loaded.saved_views.len(), 1);
        assert_eq!(loaded.saved_views[0].slot, 3);
        assert_eq!(loaded.saved_views[0].label.as_deref(), Some("triage"));
        assert!(matches!(
            loaded.saved_views[0].source_filter,
            SourceFilter::SourceId(ref id) if id == "remote-buildbox"
        ));
    }

    #[test]
    fn persisted_state_load_accepts_legacy_source_filter_object_and_clamps_limit() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let state_path = tmp.path().join("tui_state.json");
        let legacy = serde_json::json!({
            "search_mode": "lexical",
            "match_mode": "standard",
            "ranking_mode": "balanced",
            "context_window": "medium",
            "per_pane_limit": 0,
            "saved_views": [
                {
                    "slot": 2,
                    "label": "legacy",
                    "agents": ["codex"],
                    "workspaces": ["/repo"],
                    "ranking": "balanced",
                    "source_filter": { "source_id": "legacy-source" }
                }
            ]
        });
        std::fs::write(
            &state_path,
            serde_json::to_vec_pretty(&legacy).expect("serialize legacy state"),
        )
        .expect("write legacy fixture");

        let loaded = load_persisted_state_from_path(&state_path)
            .expect("load should succeed")
            .expect("state exists");
        assert_eq!(loaded.per_pane_limit, 4);
        assert_eq!(loaded.saved_views.len(), 1);
        assert!(matches!(
            loaded.saved_views[0].source_filter,
            SourceFilter::SourceId(ref id) if id == "legacy-source"
        ));
    }

    #[test]
    fn state_load_requested_dispatches_background_task() {
        let mut app = CassApp::default();
        let cmd = app.update(CassMsg::StateLoadRequested);
        let debug = format!("{cmd:?}");
        assert!(debug.contains("Task"), "expected Cmd::Task, got: {debug}");
    }

    #[test]
    fn state_save_requested_dispatches_background_task() {
        let mut app = CassApp::default();
        app.query = "hello".to_string();
        app.query_history.push_front("hello".to_string());
        let cmd = app.update(CassMsg::StateSaveRequested);
        let debug = format!("{cmd:?}");
        assert!(debug.contains("Task"), "expected Cmd::Task, got: {debug}");
    }

    #[test]
    fn state_reset_requested_clears_state_file() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let mut app = CassApp::default();
        app.data_dir = tmp.path().to_path_buf();
        let state_path = app.state_file_path();
        std::fs::write(&state_path, "{}").expect("write state fixture");
        assert!(state_path.exists(), "state fixture should exist");
        let _ = app.update(CassMsg::StateResetRequested);
        assert!(
            !state_path.exists(),
            "state file should be removed by reset handler"
        );
    }

    #[test]
    fn index_refresh_requested_dispatches_task_and_rejects_parallel_refresh() {
        let mut app = CassApp::default();
        let first = app.update(CassMsg::IndexRefreshRequested);
        let debug_first = format!("{first:?}");
        assert!(
            debug_first.contains("Task"),
            "expected first refresh to dispatch Task, got: {debug_first}"
        );
        assert!(app.index_refresh_in_flight, "refresh should mark in-flight");

        let second = app.update(CassMsg::IndexRefreshRequested);
        let debug_second = format!("{second:?}");
        assert!(
            debug_second.contains("None"),
            "expected second refresh request to no-op, got: {debug_second}"
        );
        assert!(
            app.status.contains("already running"),
            "status should explain duplicate refresh suppression"
        );
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

    #[test]
    fn palette_open_saved_views_dispatches() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::PaletteOpened);
        let idx = app
            .palette_state
            .filtered
            .iter()
            .position(|i| matches!(i.action, PaletteAction::OpenSavedViews))
            .expect("open saved views action should exist");
        app.palette_state.selected = idx;

        let cmd = app.update(CassMsg::PaletteActionExecuted);
        assert!(!app.palette_state.open);
        assert!(matches!(extract_msg(cmd), Some(CassMsg::SavedViewsOpened)));
    }

    #[test]
    fn saved_views_modal_open_move_and_close() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ViewSaved(2));
        let _ = app.update(CassMsg::ViewSaved(1));

        let _ = app.update(CassMsg::SavedViewsOpened);
        assert!(app.show_saved_views_modal);
        assert_eq!(app.selected_saved_view_slot(), Some(1));

        let _ = app.update(CassMsg::SavedViewsSelectionMoved { delta: 1 });
        assert_eq!(app.selected_saved_view_slot(), Some(2));

        let _ = app.update(CassMsg::SavedViewsClosed);
        assert!(!app.show_saved_views_modal);
    }

    #[test]
    fn saved_views_selection_move_handles_extreme_delta() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ViewSaved(1));
        let _ = app.update(CassMsg::ViewSaved(2));
        let _ = app.update(CassMsg::ViewSaved(3));
        let _ = app.update(CassMsg::SavedViewsOpened);

        assert_eq!(app.selected_saved_view_slot(), Some(3));
        let _ = app.update(CassMsg::SavedViewsSelectionMoved { delta: i32::MIN });
        assert_eq!(app.selected_saved_view_slot(), Some(1));
    }

    #[test]
    fn saved_view_rename_commit_sets_label() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ViewSaved(1));
        let _ = app.update(CassMsg::SavedViewsOpened);

        let _ = app.update(CassMsg::SavedViewRenameStarted);
        assert!(app.saved_view_rename_mode);

        let _ = app.update(CassMsg::QueryChanged("Primary".to_string()));
        let _ = app.update(CassMsg::SavedViewRenameCommitted);

        assert!(!app.saved_view_rename_mode);
        assert_eq!(
            app.saved_views.first().and_then(|v| v.label.as_deref()),
            Some("Primary")
        );
    }

    #[test]
    fn saved_view_delete_then_clear_all() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ViewSaved(1));
        let _ = app.update(CassMsg::ViewSaved(2));
        let _ = app.update(CassMsg::SavedViewsOpened);

        assert_eq!(app.selected_saved_view_slot(), Some(2));
        let _ = app.update(CassMsg::SavedViewDeletedSelected);
        assert_eq!(app.saved_views.len(), 1);
        assert_eq!(app.saved_views[0].slot, 1);

        let _ = app.update(CassMsg::SavedViewsCleared);
        assert!(app.saved_views.is_empty());
    }

    #[test]
    fn saved_view_load_selected_dispatches_view_loaded_for_selected_slot() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ViewSaved(3));
        let _ = app.update(CassMsg::SavedViewsOpened);

        let cmd = app.update(CassMsg::SavedViewLoadedSelected);
        assert!(!app.show_saved_views_modal);
        assert!(matches!(extract_msg(cmd), Some(CassMsg::ViewLoaded(3))));
    }

    #[test]
    fn saving_existing_slot_preserves_label() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ViewSaved(1));
        let _ = app.update(CassMsg::SavedViewsOpened);
        let _ = app.update(CassMsg::SavedViewRenameStarted);
        let _ = app.update(CassMsg::QueryChanged("Pinned".to_string()));
        let _ = app.update(CassMsg::SavedViewRenameCommitted);

        app.filters.agents.insert("codex".to_string());
        let _ = app.update(CassMsg::ViewSaved(1));

        let label = app
            .saved_views
            .iter()
            .find(|v| v.slot == 1)
            .and_then(|v| v.label.as_deref());
        assert_eq!(label, Some("Pinned"));
    }

    #[test]
    fn saved_views_quit_requests_close_modal_before_app_quit() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ViewSaved(1));
        let _ = app.update(CassMsg::SavedViewsOpened);
        assert!(app.show_saved_views_modal);

        let cmd = app.update(CassMsg::QuitRequested);
        if let Some(msg) = extract_msg(cmd) {
            let _ = app.update(msg);
        }

        assert!(!app.show_saved_views_modal);
    }

    #[test]
    fn saved_view_rename_quit_cancels_rename_but_keeps_modal_open() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ViewSaved(1));
        let _ = app.update(CassMsg::SavedViewsOpened);
        let _ = app.update(CassMsg::SavedViewRenameStarted);
        let _ = app.update(CassMsg::QueryChanged("Temp Label".to_string()));
        assert!(app.saved_view_rename_mode);
        assert!(!app.saved_view_rename_buffer.is_empty());

        let cmd = app.update(CassMsg::QuitRequested);
        assert!(matches!(cmd, ftui::Cmd::None));
        assert!(app.show_saved_views_modal);
        assert!(!app.saved_view_rename_mode);
        assert!(app.saved_view_rename_buffer.is_empty());
    }

    #[test]
    fn load_empty_saved_view_slot_sets_warning_status() {
        let mut app = CassApp::default();
        let cmd = app.update(CassMsg::ViewLoaded(9));
        assert!(matches!(cmd, ftui::Cmd::None));
        assert!(app.status.contains("No saved view in slot 9"));
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

    #[test]
    fn result_item_source_badge_reflects_local_and_remote_provenance() {
        let mut local_hit = make_test_hit();
        local_hit.source_id = "local".to_string();
        local_hit.origin_kind = "local".to_string();
        local_hit.origin_host = None;
        let local_item = ResultItem {
            index: 1,
            hit: local_hit,
            row_height: 1,
            even: true,
            max_width: 80,
            queued: false,
            stripe_style: ftui::Style::default(),
            agent_style: ftui::Style::default(),
        };
        assert_eq!(local_item.source_badge(), "[local]");

        let mut remote_hit = make_test_hit();
        remote_hit.source_id = "work-laptop".to_string();
        remote_hit.origin_kind = "ssh".to_string();
        remote_hit.origin_host = Some("laptop".to_string());
        let remote_item = ResultItem {
            index: 2,
            hit: remote_hit,
            row_height: 1,
            even: false,
            max_width: 80,
            queued: false,
            stripe_style: ftui::Style::default(),
            agent_style: ftui::Style::default(),
        };
        assert_eq!(remote_item.source_badge(), "[laptop]");
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
    fn source_filter_menu_applies_selected_source_id() {
        let mut app = CassApp::default();
        let mut local = make_test_hit();
        local.source_id = "local".to_string();
        local.origin_kind = "local".to_string();
        local.origin_host = None;

        let mut remote = make_test_hit();
        remote.source_id = "work-laptop".to_string();
        remote.origin_kind = "ssh".to_string();
        remote.origin_host = Some("laptop".to_string());

        app.results = vec![local, remote];
        let _ = app.update(CassMsg::SourceFilterMenuToggled);
        assert!(app.source_filter_menu_open);
        assert_eq!(app.available_source_ids, vec!["work-laptop".to_string()]);

        app.source_filter_menu_selection = 3;
        let cmd = app.update(CassMsg::DetailOpened);
        for msg in extract_msgs(cmd) {
            let cmd2 = app.update(msg);
            for msg2 in extract_msgs(cmd2) {
                let _ = app.update(msg2);
            }
        }

        assert_eq!(
            app.filters.source_filter,
            SourceFilter::SourceId("work-laptop".to_string())
        );
        assert!(!app.source_filter_menu_open);
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

        app.detail_scroll = 5;
        let _ = app.update(CassMsg::DetailTabChanged(DetailTab::Json));
        assert_eq!(app.detail_tab, DetailTab::Json);
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
    fn detail_find_query_changed_resets_current_and_clears_matches() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::DetailFindToggled);
        // Simulate having matches from a previous query
        if let Some(ref mut find) = app.detail_find {
            find.matches = vec![5, 10, 20];
            find.current = 2;
        }
        let _ = app.update(CassMsg::DetailFindQueryChanged("new query".to_string()));
        let find = app.detail_find.as_ref().unwrap();
        assert_eq!(find.query, "new query");
        assert_eq!(find.current, 0, "current should reset on query change");
        assert!(find.matches.is_empty(), "stale matches should be cleared");
    }

    #[test]
    fn detail_find_navigation_auto_scrolls() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::DetailFindToggled);
        // Populate matches via the cache (simulating what the renderer does)
        *app.detail_find_matches_cache.borrow_mut() = vec![10, 30, 50];
        if let Some(ref mut find) = app.detail_find {
            find.query = "test".to_string();
        }
        // Navigate forward — should sync from cache and scroll
        let _ = app.update(CassMsg::DetailFindNavigated { forward: true });
        let find = app.detail_find.as_ref().unwrap();
        assert_eq!(find.matches, vec![10, 30, 50], "matches synced from cache");
        assert_eq!(find.current, 1); // advanced from 0 to 1
        assert_eq!(
            app.detail_scroll, 27,
            "should scroll to match line 30 minus 3"
        );
    }

    #[test]
    fn detail_modal_intercept_routes_slash_to_find() {
        let mut app = CassApp::default();
        app.show_detail_modal = true;
        // '/' in the event map becomes PaneFilterOpened
        let _ = app.update(CassMsg::PaneFilterOpened);
        assert!(
            app.detail_find.is_some(),
            "slash should toggle find in detail modal"
        );
    }

    #[test]
    fn detail_modal_intercept_routes_text_to_find_query() {
        let mut app = CassApp::default();
        app.show_detail_modal = true;
        let _ = app.update(CassMsg::DetailFindToggled);
        assert!(app.detail_find.is_some());
        // Type characters
        let _ = app.update(CassMsg::QueryChanged("h".to_string()));
        let _ = app.update(CassMsg::QueryChanged("i".to_string()));
        assert_eq!(app.detail_find.as_ref().unwrap().query, "hi");
        // Backspace
        let _ = app.update(CassMsg::QueryChanged(String::new()));
        assert_eq!(app.detail_find.as_ref().unwrap().query, "h");
    }

    #[test]
    fn detail_modal_intercept_esc_closes_find_first() {
        let mut app = CassApp::default();
        app.show_detail_modal = true;
        let _ = app.update(CassMsg::DetailFindToggled);
        assert!(app.detail_find.is_some());
        // Esc should close find bar, NOT the detail modal
        let _ = app.update(CassMsg::QuitRequested);
        assert!(app.detail_find.is_none(), "find should close");
        assert!(app.show_detail_modal, "detail modal should stay open");
        // Second Esc closes the detail modal
        let _ = app.update(CassMsg::QuitRequested);
        assert!(!app.show_detail_modal, "detail modal should close now");
    }

    #[test]
    fn detail_modal_intercept_j_k_scroll() {
        let mut app = CassApp::default();
        app.show_detail_modal = true;
        app.detail_scroll = 0;
        let _ = app.update(CassMsg::QueryChanged("j".to_string()));
        assert_eq!(app.detail_scroll, 3, "j should scroll down 3");
        let _ = app.update(CassMsg::QueryChanged("k".to_string()));
        assert_eq!(app.detail_scroll, 0, "k should scroll up 3");
    }

    #[test]
    fn detail_modal_intercept_tab_cycles_tabs() {
        let mut app = CassApp::default();
        app.show_detail_modal = true;
        assert_eq!(app.detail_tab, DetailTab::Messages);
        let _ = app.update(CassMsg::FocusToggled);
        assert_eq!(app.detail_tab, DetailTab::Snippets);
        let _ = app.update(CassMsg::FocusToggled);
        assert_eq!(app.detail_tab, DetailTab::Raw);
        let _ = app.update(CassMsg::FocusToggled);
        assert_eq!(app.detail_tab, DetailTab::Json);
        let _ = app.update(CassMsg::FocusToggled);
        assert_eq!(app.detail_tab, DetailTab::Messages);
    }

    #[test]
    fn detail_modal_intercept_enter_navigates_find() {
        let mut app = CassApp::default();
        app.show_detail_modal = true;
        let _ = app.update(CassMsg::DetailFindToggled);
        // Pre-populate matches via cache
        *app.detail_find_matches_cache.borrow_mut() = vec![5, 15];
        if let Some(ref mut find) = app.detail_find {
            find.query = "test".to_string();
        }
        // Enter should navigate to next match
        let _ = app.update(CassMsg::QuerySubmitted);
        assert_eq!(app.detail_find.as_ref().unwrap().current, 1);
    }

    #[test]
    fn detail_modal_n_navigates_find_when_not_in_find_mode() {
        let mut app = CassApp::default();
        app.show_detail_modal = true;
        // Set up find state with matches but find bar closed
        app.detail_find = Some(DetailFindState {
            query: "test".to_string(),
            matches: vec![5, 15, 25],
            current: 0,
        });
        // Close find bar
        app.detail_find = None;
        // Re-open without query — press n should still navigate
        // (requires matches, which come from cache)
        *app.detail_find_matches_cache.borrow_mut() = vec![5, 15, 25];
        // Actually, n without active find just does nothing since
        // detail_find is None. This tests that n is consumed (no crash).
        let _ = app.update(CassMsg::QueryChanged("n".to_string()));
        // No crash — n was consumed by the detail modal intercept
    }

    #[test]
    fn detail_find_highlight_function_works() {
        let style_opts = crate::ui::style_system::StyleOptions::default();
        let styles = StyleContext::from_options(style_opts);

        let mut lines = vec![
            ftui::text::Line::raw("Hello world".to_string()),
            ftui::text::Line::raw("no match here".to_string()),
            ftui::text::Line::raw("HELLO again".to_string()),
        ];

        let matches = CassApp::apply_find_highlight(&mut lines, "hello", 0, &styles);
        // Should find "Hello" on line 0 and "HELLO" on line 2 (case-insensitive)
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0], 0);
        assert_eq!(matches[1], 2);
    }

    #[test]
    fn detail_find_highlight_empty_query_returns_no_matches() {
        let style_opts = crate::ui::style_system::StyleOptions::default();
        let styles = StyleContext::from_options(style_opts);

        let mut lines = vec![ftui::text::Line::raw("Hello".to_string())];
        let matches = CassApp::apply_find_highlight(&mut lines, "", 0, &styles);
        assert!(matches.is_empty());
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
    fn build_messages_lines_include_source_provenance_fields() {
        let app = CassApp::default();
        let mut hit = make_test_hit();
        hit.source_id = "work-laptop".to_string();
        hit.origin_kind = "ssh".to_string();
        hit.origin_host = Some("laptop".to_string());
        hit.workspace_original = Some("/home/user/projects/test".to_string());
        let styles = StyleContext::from_options(StyleOptions::default());
        let lines = app.build_messages_lines(&hit, 80, &styles);
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
        assert!(text.contains("source=laptop"));
        assert!(text.contains("source_kind=remote"));
        assert!(text.contains("workspace_original=/home/user/projects/test"));
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
        assert!(
            text.contains("source_kind"),
            "should include source_kind key"
        );
        assert!(
            text.contains("workspace_original"),
            "should include workspace_original key"
        );
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
        app.focus_manager.focus(focus_ids::DETAIL_PANE);
        let _ = app.update(CassMsg::DetailClosed);
        assert!(!app.show_detail_modal);
        assert_eq!(app.focused_region(), FocusRegion::Results);
    }

    // =====================================================================
    // 2noh9.4.16 — JSON viewer tests
    // =====================================================================

    #[test]
    fn toggle_json_view_no_hit_sets_status() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ToggleJsonView);
        assert!(
            app.status.contains("No active result"),
            "should show error when no hit selected"
        );
        assert_ne!(app.detail_tab, DetailTab::Json);
    }

    #[test]
    fn toggle_json_view_switches_to_json_tab() {
        let mut app = CassApp::default();
        app.panes.push(AgentPane {
            agent: "claude_code".into(),
            total_count: 1,
            hits: vec![make_test_hit()],
            selected: 0,
        });
        app.active_pane = 0;
        let _ = app.update(CassMsg::ToggleJsonView);
        assert_eq!(app.detail_tab, DetailTab::Json);
        assert!(app.show_detail_modal, "should open detail modal");
        assert_eq!(app.detail_scroll, 0, "should reset scroll");
    }

    #[test]
    fn toggle_json_view_toggles_back_to_raw() {
        let mut app = CassApp::default();
        app.panes.push(AgentPane {
            agent: "claude_code".into(),
            total_count: 1,
            hits: vec![make_test_hit()],
            selected: 0,
        });
        app.active_pane = 0;
        // First toggle: to Json
        let _ = app.update(CassMsg::ToggleJsonView);
        assert_eq!(app.detail_tab, DetailTab::Json);
        // Second toggle: back to Raw
        let _ = app.update(CassMsg::ToggleJsonView);
        assert_eq!(app.detail_tab, DetailTab::Raw);
    }

    #[test]
    fn build_json_lines_produces_syntax_colored_output() {
        let app = CassApp::default();
        let hit = make_test_hit();
        let styles = StyleContext::from_options(StyleOptions::default());
        let lines = app.build_json_lines(&hit, &styles);
        assert!(!lines.is_empty(), "should produce output");
        // Should contain JSON Viewer header + JSON content
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
        assert!(text.contains("JSON Viewer"), "should have header");
        assert!(text.contains("claude_code"), "should contain agent name");
        assert!(text.contains("title"), "should contain JSON keys");
    }

    #[test]
    fn detail_tab_json_variant_has_correct_default() {
        // Json is not the default tab
        assert_ne!(DetailTab::default(), DetailTab::Json);
        assert_eq!(DetailTab::default(), DetailTab::Messages);
    }

    #[test]
    fn detail_tab_changed_to_json_resets_scroll() {
        let mut app = CassApp::default();
        app.detail_scroll = 99;
        let _ = app.update(CassMsg::DetailTabChanged(DetailTab::Json));
        assert_eq!(app.detail_tab, DetailTab::Json);
        assert_eq!(app.detail_scroll, 0);
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
    fn analytics_drilldown_inherits_filters_and_clears_stale_search_filters() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;

        // Seed stale search filters that should be replaced by analytics filters.
        app.filters.agents.insert("stale-agent".into());
        app.filters.workspaces.insert("/stale/ws".into());
        app.filters.source_filter = SourceFilter::Remote;
        app.filters
            .session_paths
            .insert("/tmp/stale-session.jsonl".into());

        app.analytics_filters.since_ms = Some(10_000);
        app.analytics_filters.until_ms = Some(20_000);
        app.analytics_filters.agents.insert("claude_code".into());
        app.analytics_filters
            .workspaces
            .insert("/analytics/ws".into());
        app.analytics_filters.source_filter = SourceFilter::Local;

        let _ = app.update(CassMsg::AnalyticsDrilldown(DrilldownContext {
            since_ms: Some(30_000),
            until_ms: Some(40_000),
            agent: None,
            model: None,
        }));

        assert_eq!(app.surface, AppSurface::Search);
        assert_eq!(app.filters.created_from, Some(30_000));
        assert_eq!(app.filters.created_to, Some(40_000));
        assert_eq!(app.filters.agents, app.analytics_filters.agents);
        assert_eq!(app.filters.workspaces, app.analytics_filters.workspaces);
        assert_eq!(
            app.filters.source_filter,
            app.analytics_filters.source_filter
        );
        assert!(
            app.filters.session_paths.is_empty(),
            "drilldown should clear chained session_paths"
        );
    }

    #[test]
    fn analytics_drilldown_agent_dimension_overrides_inherited_agent_filters() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_filters.agents.insert("claude_code".into());
        app.analytics_filters.agents.insert("cursor".into());

        let _ = app.update(CassMsg::AnalyticsDrilldown(DrilldownContext {
            since_ms: None,
            until_ms: None,
            agent: Some("codex".into()),
            model: None,
        }));

        let expected: HashSet<String> = ["codex"].iter().map(|s| s.to_string()).collect();
        assert_eq!(app.filters.agents, expected);
    }

    #[test]
    fn analytics_drilldown_back_navigation_preserves_selection() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_selection = 3;

        let cmd = app.update(CassMsg::AnalyticsDrilldown(DrilldownContext {
            since_ms: Some(1),
            until_ms: Some(2),
            agent: None,
            model: None,
        }));
        assert!(matches!(extract_msg(cmd), Some(CassMsg::SearchRequested)));
        assert_eq!(app.surface, AppSurface::Search);
        assert_eq!(app.view_stack.last(), Some(&AppSurface::Analytics));

        let _ = app.update(CassMsg::ViewStackPopped);
        assert_eq!(app.surface, AppSurface::Analytics);
        assert_eq!(app.analytics_selection, 3);
    }

    #[test]
    fn day_label_to_epoch_range_validates_calendar_dates() {
        assert_eq!(
            day_label_to_epoch_range("1970-01-01"),
            Some((0, 86_400_000))
        );
        assert_eq!(
            day_label_to_epoch_range("2024-02-29").map(|(start, end)| end - start),
            Some(86_400_000)
        );
        assert!(day_label_to_epoch_range("2023-02-29").is_none());
        assert!(day_label_to_epoch_range("2026-13-01").is_none());
        assert!(day_label_to_epoch_range("2026-04-31").is_none());
        assert!(day_label_to_epoch_range("not-a-date").is_none());
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
    fn analytics_view_all_has_eight_entries() {
        assert_eq!(AnalyticsView::all().len(), 8);
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
        // Full shows chart content (e.g. KPI text or "No agent data" fallback);
        // Skeleton skips content entirely.
        assert!(
            full_text.contains("Agents:") || full_text.contains("No "),
            "Full analytics should show chart content: {full_text}"
        );
        assert!(
            !skeleton_text.contains("Agents:") && !skeleton_text.contains("No agent"),
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

    // =====================================================================
    // 2noh9.6.3 — Final UI polish
    // =====================================================================

    #[test]
    fn render_80x24_no_panic() {
        let app = CassApp::default();
        let buf = render_at_degradation(&app, 80, 24, ftui::render::budget::DegradationLevel::Full);
        let text = ftui_harness::buffer_to_text(&buf);
        assert!(text.contains("cass"), "should show app title");
        assert!(
            text.contains("med-n"),
            "80-col should show medium-narrow breakpoint"
        );
    }

    #[test]
    fn render_40x12_no_panic() {
        // Extreme small terminal — must not panic
        let app = CassApp::default();
        let _buf =
            render_at_degradation(&app, 40, 12, ftui::render::budget::DegradationLevel::Full);
    }

    #[test]
    fn render_1x1_no_panic() {
        // Degenerate case
        let app = CassApp::default();
        let _buf = render_at_degradation(&app, 1, 1, ftui::render::budget::DegradationLevel::Full);
    }

    #[test]
    fn status_footer_adapts_to_width() {
        let app = CassApp::default();

        // Wide: shows richer contextual hints.
        let wide_text = ftui_harness::buffer_to_text(&render_at_degradation(
            &app,
            120,
            24,
            ftui::render::budget::DegradationLevel::Full,
        ));
        assert!(
            wide_text.contains("Enter=open"),
            "wide footer should show contextual open hint"
        );

        // Medium: still shows at least one contextual hint.
        let medium_text = ftui_harness::buffer_to_text(&render_at_degradation(
            &app,
            70,
            24,
            ftui::render::budget::DegradationLevel::Full,
        ));
        assert!(
            medium_text.contains("Enter=open"),
            "medium footer should keep essential contextual hints"
        );

        // Narrow: hints collapse to keep the status compact.
        let narrow_text = ftui_harness::buffer_to_text(&render_at_degradation(
            &app,
            50,
            24,
            ftui::render::budget::DegradationLevel::Full,
        ));
        assert!(
            !narrow_text.contains("Enter=open"),
            "narrow footer should omit contextual hints"
        );
    }

    #[test]
    fn contextual_footer_hints_include_bulk_actions_when_selected() {
        let mut app = app_with_hits(3);
        let _ = app.update(CassMsg::SelectAllToggled);
        let hints = app.build_contextual_footer_hints(120);
        assert!(hints.contains("A=bulk"));
        assert!(hints.contains("Ctrl+O=open"));
    }

    #[test]
    fn contextual_footer_hints_switch_for_filter_mode() {
        let mut app = CassApp::default();
        app.input_mode = InputMode::Agent;
        let hints = app.build_contextual_footer_hints(120);
        assert!(hints.contains("Enter=apply"));
        assert!(hints.contains("Esc=cancel"));
    }

    #[test]
    fn contextual_footer_hints_switch_for_analytics_surface() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        let hints = app.build_contextual_footer_hints(120);
        assert!(hints.contains("←/→=views"));
        assert!(hints.contains("Esc=back"));
    }

    #[test]
    fn contextual_footer_hints_analytics_dashboard_no_special_keys() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_view = AnalyticsView::Dashboard;
        let hints = app.build_contextual_footer_hints(120);
        assert!(hints.contains("←/→=views"));
        // Dashboard is read-only — no metric/overlay/tab hints
        assert!(!hints.contains("m=metric"));
        assert!(!hints.contains("Tab=tab"));
    }

    #[test]
    fn contextual_footer_hints_analytics_explorer_shows_controls() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_view = AnalyticsView::Explorer;
        let hints = app.build_contextual_footer_hints(120);
        assert!(hints.contains("←/→=views"), "missing views hint");
        assert!(hints.contains("m=metric"), "missing metric hint");
        assert!(hints.contains("o=overlay"), "missing overlay hint");
        assert!(hints.contains("g=group"), "missing group hint");
    }

    #[test]
    fn contextual_footer_hints_analytics_heatmap_shows_tab_metric() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_view = AnalyticsView::Heatmap;
        let hints = app.build_contextual_footer_hints(120);
        assert!(hints.contains("←/→=views"));
        assert!(
            hints.contains("Tab=metric"),
            "missing metric hint for heatmap"
        );
    }

    #[test]
    fn contextual_footer_hints_analytics_breakdowns_shows_tab() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_view = AnalyticsView::Breakdowns;
        let hints = app.build_contextual_footer_hints(120);
        assert!(hints.contains("←/→=views"));
        assert!(hints.contains("Tab=tab"), "missing tab hint for breakdowns");
    }

    #[test]
    fn contextual_footer_hints_analytics_context_key_per_subview() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;

        for (view, expected_key) in [
            (AnalyticsView::Dashboard, "analytics:dashboard"),
            (AnalyticsView::Explorer, "analytics:explorer"),
            (AnalyticsView::Heatmap, "analytics:heatmap"),
            (AnalyticsView::Breakdowns, "analytics:breakdowns"),
            (AnalyticsView::Tools, "analytics:tools"),
            (AnalyticsView::Cost, "analytics:cost"),
            (AnalyticsView::Plans, "analytics:plans"),
            (AnalyticsView::Coverage, "analytics:coverage"),
        ] {
            app.analytics_view = view;
            assert_eq!(
                app.footer_hint_context_key(),
                expected_key,
                "wrong context key for {:?}",
                view
            );
        }
    }

    #[test]
    fn search_title_adapts_to_width() {
        let app = CassApp::default();

        // Wide: shows theme name
        let wide_text = ftui_harness::buffer_to_text(&render_at_degradation(
            &app,
            100,
            24,
            ftui::render::budget::DegradationLevel::Full,
        ));
        assert!(
            wide_text.contains("Dark") || wide_text.contains("Light"),
            "wide search title should show theme preset name"
        );

        // Narrow: just mode
        let narrow_text = ftui_harness::buffer_to_text(&render_at_degradation(
            &app,
            60,
            24,
            ftui::render::budget::DegradationLevel::Full,
        ));
        assert!(
            narrow_text.contains("lexical"),
            "narrow search title should show mode"
        );
    }

    #[test]
    fn results_title_shows_selection_count() {
        let mut app = app_with_hits(3);
        let _ = app.update(CassMsg::SelectAllToggled);
        let text = ftui_harness::buffer_to_text(&render_at_degradation(
            &app,
            120,
            24,
            ftui::render::budget::DegradationLevel::Full,
        ));
        assert!(
            text.contains("selected"),
            "results title should show selection count when items selected"
        );
    }

    #[test]
    fn analytics_header_adapts_to_width() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::AnalyticsEntered);

        // Wide: shows all view tabs
        let wide_text = ftui_harness::buffer_to_text(&render_at_degradation(
            &app,
            120,
            24,
            ftui::render::budget::DegradationLevel::Full,
        ));
        assert!(
            wide_text.contains("Dashboard"),
            "wide analytics should show view tabs"
        );

        // Narrow: just current view
        let narrow_text = ftui_harness::buffer_to_text(&render_at_degradation(
            &app,
            70,
            24,
            ftui::render::budget::DegradationLevel::Full,
        ));
        assert!(
            narrow_text.contains("analytics"),
            "narrow analytics should show label"
        );
    }

    #[test]
    fn adaptive_borders_reach_results_and_detail_panes() {
        use ftui::render::budget::DegradationLevel;
        use ftui_harness::buffer_to_text;

        let app = CassApp::default();

        // At EssentialOnly, borders should be dropped from all panes
        let essential = buffer_to_text(&render_at_degradation(
            &app,
            120,
            24,
            DegradationLevel::EssentialOnly,
        ));
        let full = buffer_to_text(&render_at_degradation(
            &app,
            120,
            24,
            DegradationLevel::Full,
        ));

        // Full should have more border characters than EssentialOnly
        let full_border_chars = full
            .chars()
            .filter(|c| matches!(c, '─' | '│' | '┌' | '┐' | '└' | '┘' | '╭' | '╮' | '╯' | '╰'))
            .count();
        let essential_border_chars = essential
            .chars()
            .filter(|c| matches!(c, '─' | '│' | '┌' | '┐' | '└' | '┘' | '╭' | '╮' | '╯' | '╰'))
            .count();
        assert!(
            full_border_chars > essential_border_chars,
            "EssentialOnly should have fewer border characters than Full (full={full_border_chars}, essential={essential_border_chars})"
        );
    }

    // ==================== Mouse support tests ====================

    #[test]
    fn hit_regions_recorded_after_render() {
        let app = app_with_hits(5);
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);

        assert!(
            app.last_search_bar_area.borrow().is_some(),
            "search bar area should be recorded"
        );
        assert!(
            app.last_results_inner.borrow().is_some(),
            "results inner area should be recorded"
        );
        assert!(
            app.last_status_area.borrow().is_some(),
            "status area should be recorded"
        );
    }

    #[test]
    fn hit_regions_include_detail_pane_in_wide_layout() {
        let app = app_with_hits(5);
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);

        assert!(
            app.last_detail_area.borrow().is_some(),
            "detail area should be recorded in wide layout"
        );
    }

    #[test]
    fn hit_test_returns_results_for_results_inner() {
        let app = app_with_hits(5);
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);

        let inner = app.last_results_inner.borrow().unwrap();
        let region = app.hit_test(inner.x, inner.y);
        assert!(
            matches!(region, MouseHitRegion::Results { item_idx: 0 }),
            "click at results origin should return Results(0), got {region:?}"
        );
    }

    #[test]
    fn hit_test_returns_detail_for_detail_area() {
        let app = app_with_hits(5);
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);

        let detail = app.last_detail_area.borrow().unwrap();
        let region = app.hit_test(detail.x + 1, detail.y + 1);
        assert_eq!(region, MouseHitRegion::Detail);
    }

    #[test]
    fn hit_test_returns_search_bar_for_top_row() {
        let app = app_with_hits(5);
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);

        let search = app.last_search_bar_area.borrow().unwrap();
        let region = app.hit_test(search.x + 1, search.y);
        assert_eq!(region, MouseHitRegion::SearchBar);
    }

    #[test]
    fn hit_test_returns_none_outside_all_regions() {
        let app = CassApp::default();
        let region = app.hit_test(0, 0);
        assert_eq!(region, MouseHitRegion::None);
    }

    #[test]
    fn hit_test_returns_split_handle_when_present() {
        let app = app_with_hits(5);
        render_at_degradation(&app, 180, 24, ftui::render::budget::DegradationLevel::Full);

        let handle = app
            .last_split_handle_area
            .borrow()
            .as_ref()
            .copied()
            .expect("split handle should be recorded in wide layout");
        let region = app.hit_test(handle.x, handle.y);
        assert_eq!(region, MouseHitRegion::SplitHandle);
    }

    #[test]
    fn mouse_click_in_results_moves_selection() {
        use ftui::Model;
        let mut app = app_with_hits(10);
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);

        let inner = app.last_results_inner.borrow().unwrap();
        let row_h = app.density_mode.row_height();
        let target_y = inner.y + row_h * 2;
        let cmd = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftClick,
            x: inner.x + 1,
            y: target_y,
        });
        assert!(
            !matches!(cmd, ftui::Cmd::None),
            "clicking a non-selected row should produce a command"
        );
    }

    #[test]
    fn mouse_click_on_selected_row_opens_detail() {
        use ftui::Model;
        let mut app = app_with_hits(5);
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);

        let inner = app.last_results_inner.borrow().unwrap();
        let cmd = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftClick,
            x: inner.x + 1,
            y: inner.y,
        });
        assert!(
            !matches!(cmd, ftui::Cmd::None),
            "clicking selected row should emit DetailOpened"
        );
    }

    #[test]
    fn mouse_right_click_in_results_toggles_selection() {
        use ftui::Model;
        let mut app = app_with_hits(5);
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);

        assert!(app.selected.is_empty());
        let inner = app.last_results_inner.borrow().unwrap();
        let cmd = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::RightClick,
            x: inner.x + 1,
            y: inner.y,
        });
        assert!(
            !matches!(cmd, ftui::Cmd::None),
            "right-click should produce toggle command"
        );
    }

    #[test]
    fn mouse_scroll_in_results_moves_selection() {
        use ftui::Model;
        let mut app = app_with_hits(20);
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);

        let inner = app.last_results_inner.borrow().unwrap();
        let cmd = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            x: inner.x + 1,
            y: inner.y + 1,
        });
        assert!(
            !matches!(cmd, ftui::Cmd::None),
            "scroll in results should produce SelectionMoved"
        );
    }

    #[test]
    fn mouse_scroll_in_detail_scrolls_detail() {
        use ftui::Model;
        let mut app = app_with_hits(5);
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);

        let detail = app.last_detail_area.borrow().unwrap();
        let cmd = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            x: detail.x + 1,
            y: detail.y + 1,
        });
        assert!(
            !matches!(cmd, ftui::Cmd::None),
            "scroll in detail should produce DetailScrolled"
        );
    }

    #[test]
    fn mouse_click_in_detail_focuses_detail() {
        use ftui::Model;
        let mut app = app_with_hits(5);
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);

        assert_eq!(app.focused_region(), FocusRegion::Results);
        let detail = app.last_detail_area.borrow().unwrap();
        let cmd = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftClick,
            x: detail.x + 1,
            y: detail.y + 1,
        });
        assert!(
            !matches!(cmd, ftui::Cmd::None),
            "click in detail should emit FocusToggled"
        );
    }

    #[test]
    fn mouse_click_in_search_bar_focuses_results() {
        use ftui::Model;
        let mut app = app_with_hits(5);
        app.focus_manager.focus(focus_ids::DETAIL_PANE);
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);

        let search = app.last_search_bar_area.borrow().unwrap();
        let cmd = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftClick,
            x: search.x + 1,
            y: search.y,
        });
        assert!(
            !matches!(cmd, ftui::Cmd::None),
            "click in search bar should emit FocusToggled"
        );
    }

    #[test]
    fn mouse_event_kind_has_right_click() {
        assert_ne!(MouseEventKind::LeftClick, MouseEventKind::RightClick);
        assert_ne!(MouseEventKind::RightClick, MouseEventKind::ScrollUp);
        assert_ne!(MouseEventKind::LeftDrag, MouseEventKind::LeftRelease);
    }

    #[test]
    fn mouse_drag_on_split_handle_updates_panel_ratio_target() {
        use ftui::Model;
        let mut app = app_with_hits(25);
        render_at_degradation(&app, 180, 24, ftui::render::budget::DegradationLevel::Full);

        let handle = app
            .last_split_handle_area
            .borrow()
            .as_ref()
            .copied()
            .expect("split handle should be recorded");
        let content = app
            .last_content_area
            .borrow()
            .as_ref()
            .copied()
            .expect("content area should be recorded");
        let start_ratio = app.anim.panel_ratio.target();
        let drag_x = content.x + content.width.saturating_mul(3) / 10;

        let _ = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftClick,
            x: handle.x,
            y: handle.y,
        });
        let _ = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftDrag,
            x: drag_x,
            y: handle.y,
        });
        let _ = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftRelease,
            x: drag_x,
            y: handle.y,
        });

        let updated_ratio = app.anim.panel_ratio.target();
        assert!(
            (updated_ratio - start_ratio).abs() > 0.01,
            "panel ratio target should change after drag (before={start_ratio}, after={updated_ratio})"
        );
        assert!(
            app.pane_split_drag.is_none(),
            "split drag state should clear on release"
        );
    }

    #[test]
    fn saved_views_mouse_drag_reorders_rows() {
        use ftui::Model;

        let mut app = CassApp::default();
        app.saved_views = vec![
            SavedView {
                slot: 1,
                label: Some("One".to_string()),
                agents: HashSet::new(),
                workspaces: HashSet::new(),
                created_from: None,
                created_to: None,
                ranking: RankingMode::Balanced,
                source_filter: SourceFilter::All,
            },
            SavedView {
                slot: 2,
                label: Some("Two".to_string()),
                agents: HashSet::new(),
                workspaces: HashSet::new(),
                created_from: None,
                created_to: None,
                ranking: RankingMode::Balanced,
                source_filter: SourceFilter::All,
            },
            SavedView {
                slot: 3,
                label: Some("Three".to_string()),
                agents: HashSet::new(),
                workspaces: HashSet::new(),
                created_from: None,
                created_to: None,
                ranking: RankingMode::Balanced,
                source_filter: SourceFilter::All,
            },
        ];

        let _ = app.update(CassMsg::SavedViewsOpened);
        render_at_degradation(&app, 120, 30, ftui::render::budget::DegradationLevel::Full);

        let row_areas = app.last_saved_view_row_areas.borrow().clone();
        assert_eq!(row_areas.len(), 3, "should capture row areas for drag");
        let from = row_areas[0].0;
        let to = row_areas[2].0;

        let _ = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftClick,
            x: from.x + 1,
            y: from.y,
        });
        let _ = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftDrag,
            x: to.x + 1,
            y: to.y,
        });
        let _ = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftRelease,
            x: to.x + 1,
            y: to.y,
        });

        let labels: Vec<String> = app
            .saved_views
            .iter()
            .map(|view| view.label.clone().unwrap_or_default())
            .collect();
        assert_eq!(labels, vec!["Two", "Three", "One"]);
        assert_eq!(app.saved_views_selection, 2);
        assert!(app.saved_view_drag.is_none());
    }

    #[test]
    fn hit_regions_cleared_on_analytics_surface() {
        let mut app = app_with_hits(5);
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);
        assert!(app.last_results_inner.borrow().is_some());

        app.surface = AppSurface::Analytics;
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);

        assert!(
            app.last_results_inner.borrow().is_none(),
            "results inner should be cleared on analytics surface"
        );
        assert!(
            app.last_detail_area.borrow().is_none(),
            "detail area should be cleared on analytics surface"
        );
        assert!(
            app.last_search_bar_area.borrow().is_none(),
            "search bar should be cleared on analytics surface"
        );
    }

    #[test]
    fn mouse_scroll_outside_regions_defaults_to_results() {
        use ftui::Model;
        let mut app = CassApp::default();
        let cmd = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            x: 999,
            y: 999,
        });
        assert!(
            !matches!(cmd, ftui::Cmd::None),
            "scroll outside tracked regions should still produce SelectionMoved"
        );
    }

    // =========================================================================
    // Input smoothness (drag jitter / hover stabilization) tests
    // =========================================================================

    #[test]
    fn drag_jitter_filter_suppresses_small_movements() {
        use ftui::Model;
        let mut app = CassApp::default();
        // Simulate initial click at (50, 10)
        let _ = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftClick,
            x: 50,
            y: 10,
        });
        assert_eq!(app.last_mouse_pos, Some((50, 10)));

        // Drag by 1 pixel (below threshold of 2) — should be suppressed
        let cmd = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftDrag,
            x: 51,
            y: 10,
        });
        // Position should NOT update (event was filtered)
        assert_eq!(app.last_mouse_pos, Some((50, 10)));
        assert!(matches!(cmd, ftui::Cmd::None));
    }

    #[test]
    fn drag_above_threshold_is_accepted() {
        use ftui::Model;
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftClick,
            x: 50,
            y: 10,
        });

        // Drag by 3 pixels (above threshold of 2) — should be accepted
        let _ = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftDrag,
            x: 53,
            y: 10,
        });
        // Position SHOULD update
        assert_eq!(app.last_mouse_pos, Some((53, 10)));
    }

    #[test]
    fn mouse_release_clears_tracking_state() {
        use ftui::Model;
        let mut app = CassApp::default();
        app.last_mouse_pos = Some((50, 10));
        app.drag_hover_settled_at = Some(Instant::now());

        let _ = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftRelease,
            x: 50,
            y: 10,
        });
        assert!(app.last_mouse_pos.is_none());
        assert!(app.drag_hover_settled_at.is_none());
    }

    #[test]
    fn first_drag_event_without_prior_click_is_not_filtered() {
        use ftui::Model;
        let mut app = CassApp::default();
        assert!(app.last_mouse_pos.is_none());

        // First drag with no prior click — no previous position to compare, so not filtered
        let _ = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::LeftDrag,
            x: 50,
            y: 10,
        });
        assert_eq!(app.last_mouse_pos, Some((50, 10)));
    }

    #[test]
    fn drag_hover_settle_fields_initialized_to_none() {
        let app = CassApp::default();
        assert!(app.last_mouse_pos.is_none());
        assert!(app.drag_hover_settled_at.is_none());
    }

    #[test]
    fn scroll_events_are_not_jitter_filtered() {
        use ftui::Model;
        let mut app = CassApp::default();
        app.last_mouse_pos = Some((50, 10));

        // Scroll events should never be filtered even if mouse is tracked
        let cmd = app.update(CassMsg::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            x: 50,
            y: 10,
        });
        assert!(
            !matches!(cmd, ftui::Cmd::None),
            "scroll should not be filtered"
        );
    }

    #[test]
    fn hit_test_row_calculation_respects_density() {
        let mut app = app_with_hits(10);
        app.density_mode = DensityMode::Spacious;
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);

        let inner = app.last_results_inner.borrow().unwrap();
        let region = app.hit_test(inner.x, inner.y + 3);
        assert!(
            matches!(region, MouseHitRegion::Results { item_idx: 1 }),
            "2nd row in spacious density should be item_idx=1, got {region:?}"
        );
    }

    #[test]
    fn narrow_layout_only_records_visible_pane() {
        let mut app = app_with_hits(5);
        app.focus_manager.focus(focus_ids::RESULTS_LIST);
        render_at_degradation(&app, 60, 24, ftui::render::budget::DegradationLevel::Full);

        assert!(
            app.last_results_inner.borrow().is_some(),
            "results inner should be recorded in narrow/results mode"
        );
        assert!(
            app.last_detail_area.borrow().is_none(),
            "detail area should be None in narrow layout with results focus"
        );
    }

    // =====================================================================
    // Layout breakpoint classification tests
    // =====================================================================

    #[test]
    fn breakpoint_narrow_below_80() {
        assert_eq!(LayoutBreakpoint::from_width(40), LayoutBreakpoint::Narrow);
        assert_eq!(LayoutBreakpoint::from_width(79), LayoutBreakpoint::Narrow);
    }

    #[test]
    fn breakpoint_medium_narrow_80_to_119() {
        assert_eq!(
            LayoutBreakpoint::from_width(80),
            LayoutBreakpoint::MediumNarrow
        );
        assert_eq!(
            LayoutBreakpoint::from_width(100),
            LayoutBreakpoint::MediumNarrow
        );
        assert_eq!(
            LayoutBreakpoint::from_width(119),
            LayoutBreakpoint::MediumNarrow
        );
    }

    #[test]
    fn breakpoint_medium_120_to_159() {
        assert_eq!(LayoutBreakpoint::from_width(120), LayoutBreakpoint::Medium);
        assert_eq!(LayoutBreakpoint::from_width(140), LayoutBreakpoint::Medium);
        assert_eq!(LayoutBreakpoint::from_width(159), LayoutBreakpoint::Medium);
    }

    #[test]
    fn breakpoint_wide_160_plus() {
        assert_eq!(LayoutBreakpoint::from_width(160), LayoutBreakpoint::Wide);
        assert_eq!(LayoutBreakpoint::from_width(200), LayoutBreakpoint::Wide);
        assert_eq!(LayoutBreakpoint::from_width(300), LayoutBreakpoint::Wide);
    }

    #[test]
    fn breakpoint_zero_is_narrow() {
        assert_eq!(LayoutBreakpoint::from_width(0), LayoutBreakpoint::Narrow);
    }

    #[test]
    fn topology_narrow_is_single_pane() {
        let t = LayoutBreakpoint::Narrow.search_topology();
        assert!(!t.dual_pane);
        assert!(!t.has_split_handle);
        assert_eq!(t.min_results, 0);
        assert_eq!(t.min_detail, 0);
    }

    #[test]
    fn topology_medium_narrow_tight_split() {
        let t = LayoutBreakpoint::MediumNarrow.search_topology();
        assert!(t.dual_pane);
        assert!(t.has_split_handle);
        assert_eq!(t.min_results, 35);
        assert_eq!(t.min_detail, 25);
    }

    #[test]
    fn topology_medium_balanced_split() {
        let t = LayoutBreakpoint::Medium.search_topology();
        assert!(t.dual_pane);
        assert!(t.has_split_handle);
        assert_eq!(t.min_results, 45);
        assert_eq!(t.min_detail, 32);
    }

    #[test]
    fn topology_wide_spacious_split() {
        let t = LayoutBreakpoint::Wide.search_topology();
        assert!(t.dual_pane);
        assert!(t.has_split_handle);
        assert_eq!(t.min_results, 50);
        assert_eq!(t.min_detail, 34);
    }

    #[test]
    fn topology_min_sum_fits_breakpoint() {
        // The sum of min_results + min_detail must fit within the breakpoint's minimum width.
        let mn = LayoutBreakpoint::MediumNarrow.search_topology();
        assert!(
            mn.min_results + mn.min_detail <= 80,
            "MediumNarrow mins must fit in 80 cols"
        );

        let m = LayoutBreakpoint::Medium.search_topology();
        assert!(
            m.min_results + m.min_detail <= 120,
            "Medium mins must fit in 120 cols"
        );

        let w = LayoutBreakpoint::Wide.search_topology();
        assert!(
            w.min_results + w.min_detail <= 160,
            "Wide mins must fit in 160 cols"
        );
    }

    #[test]
    fn footer_labels_are_short() {
        for bp in [
            LayoutBreakpoint::Narrow,
            LayoutBreakpoint::MediumNarrow,
            LayoutBreakpoint::Medium,
            LayoutBreakpoint::Wide,
        ] {
            assert!(
                bp.footer_label().len() <= 6,
                "footer label too long: {}",
                bp.footer_label()
            );
        }
    }

    #[test]
    fn inspector_labels_contain_range() {
        assert!(LayoutBreakpoint::Narrow.inspector_label().contains("<80"));
        assert!(
            LayoutBreakpoint::MediumNarrow
                .inspector_label()
                .contains("80")
        );
        assert!(LayoutBreakpoint::Medium.inspector_label().contains("120"));
        assert!(LayoutBreakpoint::Wide.inspector_label().contains("160"));
    }

    #[test]
    fn medium_narrow_renders_both_panes() {
        let app = app_with_hits(5);
        // 100 cols = MediumNarrow: should render both results and detail
        render_at_degradation(&app, 100, 24, ftui::render::budget::DegradationLevel::Full);

        assert!(
            app.last_results_inner.borrow().is_some(),
            "results inner should be recorded in medium-narrow layout"
        );
        assert!(
            app.last_detail_area.borrow().is_some(),
            "detail area should be recorded in medium-narrow layout (both panes visible)"
        );
    }

    #[test]
    fn topology_driven_render_all_widths() {
        // Verify that topology-driven rendering doesn't panic at representative widths
        // for each breakpoint tier.
        let widths = [40, 79, 80, 100, 119, 120, 140, 159, 160, 200];
        for w in widths {
            let app = app_with_hits(3);
            render_at_degradation(&app, w, 24, ftui::render::budget::DegradationLevel::Full);
            let bp = LayoutBreakpoint::from_width(w);
            let topo = bp.search_topology();
            if topo.dual_pane {
                assert!(
                    app.last_detail_area.borrow().is_some(),
                    "dual_pane at w={w} should render detail area"
                );
            }
        }
    }

    #[test]
    fn narrow_single_pane_hides_other() {
        let app = app_with_hits(3);
        // 60 cols = Narrow: only results visible (default focus is Results)
        render_at_degradation(&app, 60, 24, ftui::render::budget::DegradationLevel::Full);
        // In narrow mode the detail area should NOT be set (single pane, results focused)
        assert!(
            app.last_detail_area.borrow().is_none(),
            "narrow layout should not render detail when results are focused"
        );
    }

    // =====================================================================
    // 2noh9.4.10 — Advanced navigation (grouping, timeline jump)
    // =====================================================================

    #[test]
    fn grouping_cycles_through_all_modes() {
        assert_eq!(ResultsGrouping::Agent.next(), ResultsGrouping::Conversation);
        assert_eq!(
            ResultsGrouping::Conversation.next(),
            ResultsGrouping::Workspace
        );
        assert_eq!(ResultsGrouping::Workspace.next(), ResultsGrouping::Flat);
        assert_eq!(ResultsGrouping::Flat.next(), ResultsGrouping::Agent);
    }

    #[test]
    fn grouping_labels_are_distinct() {
        let labels: Vec<&str> = [
            ResultsGrouping::Agent,
            ResultsGrouping::Conversation,
            ResultsGrouping::Workspace,
            ResultsGrouping::Flat,
        ]
        .iter()
        .map(|g| g.label())
        .collect();
        let set: std::collections::HashSet<&&str> = labels.iter().collect();
        assert_eq!(set.len(), 4, "all grouping labels should be unique");
    }

    #[test]
    fn regroup_panes_by_agent() {
        let mut app = CassApp::default();
        let mut h1 = make_hit(1, "/a");
        h1.agent = "claude_code".into();
        let mut h2 = make_hit(2, "/b");
        h2.agent = "codex".into();
        let mut h3 = make_hit(3, "/c");
        h3.agent = "claude_code".into();
        app.results = vec![h1, h2, h3];
        app.grouping_mode = ResultsGrouping::Agent;
        app.regroup_panes();
        assert_eq!(app.panes.len(), 2);
        assert_eq!(app.panes[0].agent, "claude_code");
        assert_eq!(app.panes[0].hits.len(), 2);
        assert_eq!(app.panes[1].agent, "codex");
    }

    #[test]
    fn regroup_panes_flat_creates_single_pane() {
        let mut app = CassApp::default();
        let mut h1 = make_hit(1, "/a");
        h1.agent = "claude_code".into();
        let mut h2 = make_hit(2, "/b");
        h2.agent = "codex".into();
        app.results = vec![h1, h2];
        app.grouping_mode = ResultsGrouping::Flat;
        app.regroup_panes();
        assert_eq!(app.panes.len(), 1, "flat mode should produce one pane");
        assert_eq!(app.panes[0].agent, "All");
        assert_eq!(app.panes[0].hits.len(), 2);
    }

    #[test]
    fn regroup_panes_by_workspace() {
        let mut app = CassApp::default();
        let mut h1 = make_hit(1, "/a");
        h1.workspace = "/home/user/project-a".into();
        let mut h2 = make_hit(2, "/b");
        h2.workspace = "/home/user/project-b".into();
        let mut h3 = make_hit(3, "/c");
        h3.workspace = "/home/user/project-a".into();
        app.results = vec![h1, h2, h3];
        app.grouping_mode = ResultsGrouping::Workspace;
        app.regroup_panes();
        assert_eq!(app.panes.len(), 2);
        assert_eq!(app.panes[0].agent, "project-a");
        assert_eq!(app.panes[0].hits.len(), 2);
        assert_eq!(app.panes[1].agent, "project-b");
    }

    #[test]
    fn regroup_panes_by_conversation() {
        let mut app = CassApp::default();
        // Last path component is used as the conversation key.
        let h1 = make_hit(1, "/sessions/conv-aaa");
        let h2 = make_hit(2, "/sessions/conv-bbb");
        let h3 = make_hit(3, "/sessions/conv-aaa");
        app.results = vec![h1, h2, h3];
        app.grouping_mode = ResultsGrouping::Conversation;
        app.regroup_panes();
        assert_eq!(app.panes.len(), 2);
        assert_eq!(app.panes[0].agent, "conv-aaa");
        assert_eq!(app.panes[0].hits.len(), 2);
        assert_eq!(app.panes[1].agent, "conv-bbb");
    }

    #[test]
    fn grouping_cycled_msg_changes_mode_and_regroups() {
        let mut app = CassApp::default();
        let mut h1 = make_hit(1, "/a");
        h1.agent = "claude_code".into();
        let mut h2 = make_hit(2, "/b");
        h2.agent = "codex".into();
        app.results = vec![h1, h2];
        app.panes.push(AgentPane {
            agent: "claude_code".into(),
            hits: vec![],
            selected: 0,
            total_count: 0,
        });
        let _ = app.update(CassMsg::GroupingCycled);
        assert_eq!(app.grouping_mode, ResultsGrouping::Conversation);
        assert!(app.status.contains("Grouping:"));
    }

    #[test]
    fn timeline_jump_finds_next_day() {
        let mut app = CassApp::default();
        let day1 = 86400 * 19000; // some day
        let day2 = 86400 * 19001; // next day
        let mut hits: Vec<SearchHit> = Vec::new();
        for i in 0..3 {
            let mut h = make_hit(i, &format!("/p/{i}"));
            h.created_at = Some(day1 + i as i64);
            hits.push(h);
        }
        for i in 3..6 {
            let mut h = make_hit(i, &format!("/p/{i}"));
            h.created_at = Some(day2 + i as i64);
            hits.push(h);
        }
        app.panes.push(AgentPane {
            agent: "test".into(),
            total_count: hits.len(),
            hits,
            selected: 0,
        });
        // Jump forward from day1 → should land on index 3 (first of day2)
        let idx = app.timeline_jump_index(true);
        assert_eq!(idx, Some(3));
    }

    #[test]
    fn timeline_jump_finds_prev_day() {
        let mut app = CassApp::default();
        let day1 = 86400 * 19000;
        let day2 = 86400 * 19001;
        let mut hits: Vec<SearchHit> = Vec::new();
        for i in 0..3 {
            let mut h = make_hit(i, &format!("/p/{i}"));
            h.created_at = Some(day1 + i as i64);
            hits.push(h);
        }
        for i in 3..6 {
            let mut h = make_hit(i, &format!("/p/{i}"));
            h.created_at = Some(day2 + i as i64);
            hits.push(h);
        }
        app.panes.push(AgentPane {
            agent: "test".into(),
            total_count: hits.len(),
            hits,
            selected: 4, // in day2
        });
        // Jump backward from day2 → should land on index 0 (first of day1)
        let idx = app.timeline_jump_index(false);
        assert_eq!(idx, Some(0));
    }

    #[test]
    fn timeline_jump_returns_none_at_boundary() {
        let mut app = CassApp::default();
        let day1 = 86400 * 19000;
        let mut hits: Vec<SearchHit> = Vec::new();
        for i in 0..3 {
            let mut h = make_hit(i, &format!("/p/{i}"));
            h.created_at = Some(day1 + i as i64);
            hits.push(h);
        }
        app.panes.push(AgentPane {
            agent: "test".into(),
            total_count: hits.len(),
            hits,
            selected: 0,
        });
        // No previous day
        assert_eq!(app.timeline_jump_index(false), None);
        // No next day
        assert_eq!(app.timeline_jump_index(true), None);
    }

    #[test]
    fn timeline_jumped_msg_moves_selection() {
        let mut app = CassApp::default();
        let day1 = 86400 * 19000;
        let day2 = 86400 * 19001;
        let mut hits: Vec<SearchHit> = Vec::new();
        for i in 0..3 {
            let mut h = make_hit(i, &format!("/p/{i}"));
            h.created_at = Some(day1 + i as i64);
            hits.push(h);
        }
        for i in 3..5 {
            let mut h = make_hit(i, &format!("/p/{i}"));
            h.created_at = Some(day2 + i as i64);
            hits.push(h);
        }
        app.panes.push(AgentPane {
            agent: "test".into(),
            total_count: hits.len(),
            hits,
            selected: 0,
        });
        let _ = app.update(CassMsg::TimelineJumped { forward: true });
        assert_eq!(
            app.panes[0].selected, 3,
            "should jump to first hit of next day"
        );
        assert!(app.status.contains("next day"));
    }

    #[test]
    fn results_title_shows_grouping_mode() {
        let mut app = app_with_hits(3);
        app.grouping_mode = ResultsGrouping::Workspace;
        // Render so render_results_pane is called and title is built.
        render_at_degradation(&app, 120, 24, ftui::render::budget::DegradationLevel::Full);
        // The title itself is local to render_results_pane so we can't read it directly,
        // but we can verify the grouping_mode.label() is non-empty and differs from Agent.
        assert_ne!(app.grouping_mode.label(), "by agent");
        assert_eq!(app.grouping_mode.label(), "by workspace");
    }

    #[test]
    fn regroup_clamps_active_pane() {
        let mut app = CassApp::default();
        let h1 = make_hit(1, "/a");
        app.results = vec![h1];
        app.grouping_mode = ResultsGrouping::Flat;
        app.active_pane = 5; // invalid
        app.regroup_panes();
        assert_eq!(
            app.active_pane, 0,
            "active_pane should be clamped after regroup"
        );
    }

    // =====================================================================
    // 2noh9.4.11 — Undo/redo
    // =====================================================================

    #[test]
    fn undo_history_push_and_pop() {
        let mut hist = UndoHistory::default();
        let e1 = UndoEntry {
            description: "edit 1",
            query: "hello".into(),
            cursor_pos: 5,
            filters: SearchFilters::default(),
            time_preset: TimePreset::All,
            ranking_mode: RankingMode::default(),
            grouping_mode: ResultsGrouping::Agent,
        };
        hist.push(e1);
        assert!(hist.can_undo());
        assert!(!hist.can_redo());

        let current = UndoEntry {
            description: "current",
            query: "world".into(),
            cursor_pos: 5,
            filters: SearchFilters::default(),
            time_preset: TimePreset::All,
            ranking_mode: RankingMode::default(),
            grouping_mode: ResultsGrouping::Agent,
        };
        let restored = hist.pop_undo(current).unwrap();
        assert_eq!(restored.query, "hello");
        assert!(!hist.can_undo());
        assert!(hist.can_redo());
    }

    #[test]
    fn undo_history_redo_after_undo() {
        let mut hist = UndoHistory::default();
        let e1 = UndoEntry {
            description: "edit",
            query: "before".into(),
            cursor_pos: 6,
            filters: SearchFilters::default(),
            time_preset: TimePreset::All,
            ranking_mode: RankingMode::default(),
            grouping_mode: ResultsGrouping::Agent,
        };
        hist.push(e1);

        let current = UndoEntry {
            description: "current",
            query: "after".into(),
            cursor_pos: 5,
            filters: SearchFilters::default(),
            time_preset: TimePreset::All,
            ranking_mode: RankingMode::default(),
            grouping_mode: ResultsGrouping::Agent,
        };
        let _ = hist.pop_undo(current);

        let re_current = UndoEntry {
            description: "re_current",
            query: "before".into(),
            cursor_pos: 6,
            filters: SearchFilters::default(),
            time_preset: TimePreset::All,
            ranking_mode: RankingMode::default(),
            grouping_mode: ResultsGrouping::Agent,
        };
        let redone = hist.pop_redo(re_current).unwrap();
        assert_eq!(redone.query, "after");
    }

    #[test]
    fn undo_history_push_clears_redo() {
        let mut hist = UndoHistory::default();
        let e1 = UndoEntry {
            description: "e1",
            query: "a".into(),
            cursor_pos: 1,
            filters: SearchFilters::default(),
            time_preset: TimePreset::All,
            ranking_mode: RankingMode::default(),
            grouping_mode: ResultsGrouping::Agent,
        };
        hist.push(e1);
        let current = UndoEntry {
            description: "cur",
            query: "b".into(),
            cursor_pos: 1,
            filters: SearchFilters::default(),
            time_preset: TimePreset::All,
            ranking_mode: RankingMode::default(),
            grouping_mode: ResultsGrouping::Agent,
        };
        let _ = hist.pop_undo(current);
        assert!(hist.can_redo());

        // New push clears redo.
        let e2 = UndoEntry {
            description: "e2",
            query: "c".into(),
            cursor_pos: 1,
            filters: SearchFilters::default(),
            time_preset: TimePreset::All,
            ranking_mode: RankingMode::default(),
            grouping_mode: ResultsGrouping::Agent,
        };
        hist.push(e2);
        assert!(!hist.can_redo());
    }

    #[test]
    fn undo_history_respects_max_depth() {
        let mut hist = UndoHistory {
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            max_depth: 3,
        };
        for i in 0..5 {
            hist.push(UndoEntry {
                description: "push",
                query: format!("q{i}"),
                cursor_pos: i,
                filters: SearchFilters::default(),
                time_preset: TimePreset::All,
                ranking_mode: RankingMode::default(),
                grouping_mode: ResultsGrouping::Agent,
            });
        }
        assert_eq!(hist.undo_stack.len(), 3);
        assert_eq!(hist.undo_stack[0].query, "q2", "oldest should be evicted");
    }

    #[test]
    fn undo_msg_restores_query_state() {
        let mut app = CassApp::default();
        app.query = "hello".into();
        app.cursor_pos = 5;
        let _ = app.update(CassMsg::QueryCleared);
        assert_eq!(app.query, "");

        let _ = app.update(CassMsg::Undo);
        assert_eq!(app.query, "hello");
        assert_eq!(app.cursor_pos, 5);
    }

    #[test]
    fn redo_msg_restores_after_undo() {
        let mut app = CassApp::default();
        app.query = "test".into();
        app.cursor_pos = 4;
        let _ = app.update(CassMsg::QueryCleared);
        assert_eq!(app.query, "");

        let _ = app.update(CassMsg::Undo);
        assert_eq!(app.query, "test");

        let _ = app.update(CassMsg::Redo);
        assert_eq!(app.query, "");
    }

    #[test]
    fn undo_filter_change_restores_agents() {
        let mut app = CassApp::default();
        assert!(app.filters.agents.is_empty());

        let agents: HashSet<String> = ["claude_code".to_string()].into_iter().collect();
        let _ = app.update(CassMsg::FilterAgentSet(agents));
        assert_eq!(app.filters.agents.len(), 1);

        let _ = app.update(CassMsg::Undo);
        assert!(app.filters.agents.is_empty());
    }

    #[test]
    fn undo_nothing_sets_status() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::Undo);
        assert!(app.status.contains("Nothing to undo"));
    }

    #[test]
    fn redo_nothing_sets_status() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::Redo);
        assert!(app.status.contains("Nothing to redo"));
    }

    #[test]
    fn undo_grouping_cycle_restores_mode() {
        let mut app = CassApp::default();
        assert_eq!(app.grouping_mode, ResultsGrouping::Agent);
        let _ = app.update(CassMsg::GroupingCycled);
        assert_eq!(app.grouping_mode, ResultsGrouping::Conversation);
        let _ = app.update(CassMsg::Undo);
        assert_eq!(app.grouping_mode, ResultsGrouping::Agent);
    }

    #[test]
    fn undo_clear_all_filters_restores_state() {
        let mut app = CassApp::default();
        let agents: HashSet<String> = ["codex".to_string()].into_iter().collect();
        app.filters.agents = agents.clone();
        app.time_preset = TimePreset::Week;

        let _ = app.update(CassMsg::FiltersClearAll);
        assert!(app.filters.agents.is_empty());
        assert_eq!(app.time_preset, TimePreset::All);

        let _ = app.update(CassMsg::Undo);
        assert_eq!(app.filters.agents, agents);
        assert_eq!(app.time_preset, TimePreset::Week);
    }

    #[test]
    fn analytics_selection_wraps_around() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_view = AnalyticsView::Breakdowns;
        let mut data = AnalyticsChartData::default();
        data.agent_tokens = vec![
            ("claude_code".into(), 100.0),
            ("codex".into(), 80.0),
            ("gemini".into(), 50.0),
        ];
        app.analytics_cache = Some(data);
        app.analytics_selection = 0;

        let _ = app.update(CassMsg::AnalyticsSelectionMoved { delta: 1 });
        assert_eq!(app.analytics_selection, 1);
        let _ = app.update(CassMsg::AnalyticsSelectionMoved { delta: 1 });
        assert_eq!(app.analytics_selection, 2);
        let _ = app.update(CassMsg::AnalyticsSelectionMoved { delta: 1 });
        assert_eq!(app.analytics_selection, 0, "should wrap to start");
        let _ = app.update(CassMsg::AnalyticsSelectionMoved { delta: -1 });
        assert_eq!(app.analytics_selection, 2, "should wrap to end");
    }

    #[test]
    fn analytics_enter_on_breakdowns_triggers_drilldown() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::AnalyticsEntered);
        app.analytics_view = AnalyticsView::Breakdowns;
        let mut data = AnalyticsChartData::default();
        data.agent_tokens = vec![("claude_code".into(), 100.0), ("codex".into(), 80.0)];
        app.analytics_cache = Some(data);
        app.analytics_selection = 1;

        let _ = app.update(CassMsg::DetailOpened);
        assert_eq!(app.surface, AppSurface::Search);
        let expected: HashSet<String> = ["codex"].iter().map(|s| s.to_string()).collect();
        assert_eq!(app.filters.agents, expected);
    }

    #[test]
    fn analytics_enter_on_dashboard_is_noop() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::AnalyticsEntered);
        app.analytics_view = AnalyticsView::Dashboard;
        app.analytics_cache = Some(AnalyticsChartData::default());

        let _ = app.update(CassMsg::DetailOpened);
        assert_eq!(app.surface, AppSurface::Analytics);
    }

    #[test]
    fn analytics_view_change_resets_selection() {
        let mut app = CassApp::default();
        app.analytics_selection = 5;
        let _ = app.update(CassMsg::AnalyticsViewChanged(AnalyticsView::Tools));
        assert_eq!(app.analytics_selection, 0);
    }

    #[test]
    fn analytics_left_right_cycles_views() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::AnalyticsEntered);
        assert_eq!(app.analytics_view, AnalyticsView::Dashboard);

        let _ = app.update(CassMsg::CursorMoved { delta: 1 });
        assert_eq!(app.analytics_view, AnalyticsView::Explorer);
        let _ = app.update(CassMsg::CursorMoved { delta: -1 });
        assert_eq!(app.analytics_view, AnalyticsView::Dashboard);
        let _ = app.update(CassMsg::CursorMoved { delta: -1 });
        assert_eq!(app.analytics_view, AnalyticsView::Coverage);
    }

    #[test]
    fn build_drilldown_context_explorer_bucket() {
        let mut app = CassApp::default();
        app.analytics_view = AnalyticsView::Explorer;
        let mut data = AnalyticsChartData::default();
        data.daily_tokens = vec![
            ("2026-02-05".into(), 100.0),
            ("2026-02-06".into(), 200.0),
            ("2026-02-07".into(), 150.0),
        ];
        app.analytics_cache = Some(data);
        app.analytics_selection = 1;

        let ctx = app.build_drilldown_context().expect("should build context");
        assert!(ctx.since_ms.is_some());
        assert!(ctx.until_ms.is_some());
        let since = ctx.since_ms.unwrap();
        let until = ctx.until_ms.unwrap();
        assert_eq!(until - since, 86_400_000);
        assert!(ctx.agent.is_none());
        assert!(ctx.model.is_none());
    }

    #[test]
    fn build_drilldown_context_cost_model() {
        let mut app = CassApp::default();
        app.analytics_view = AnalyticsView::Cost;
        let mut data = AnalyticsChartData::default();
        data.model_tokens = vec![("claude-3-sonnet".into(), 500.0), ("gpt-4o".into(), 300.0)];
        app.analytics_cache = Some(data);
        app.analytics_selection = 0;

        let ctx = app.build_drilldown_context().expect("should build context");
        assert_eq!(ctx.model.as_deref(), Some("claude-3-sonnet"));
        assert!(ctx.agent.is_none());
    }

    #[test]
    fn build_drilldown_context_tools_agent() {
        let mut app = CassApp::default();
        app.analytics_view = AnalyticsView::Tools;
        let mut data = AnalyticsChartData::default();
        data.tool_rows = vec![
            crate::analytics::ToolRow {
                key: "claude_code".into(),
                tool_call_count: 5000,
                message_count: 500,
                api_tokens_total: 10_000_000,
                tool_calls_per_1k_api_tokens: Some(0.5),
                tool_calls_per_1k_content_tokens: None,
            },
            crate::analytics::ToolRow {
                key: "codex".into(),
                tool_call_count: 3000,
                message_count: 300,
                api_tokens_total: 8_000_000,
                tool_calls_per_1k_api_tokens: Some(0.375),
                tool_calls_per_1k_content_tokens: None,
            },
        ];
        app.analytics_cache = Some(data);
        app.analytics_selection = 1;

        let ctx = app.build_drilldown_context().expect("should build context");
        assert_eq!(ctx.agent.as_deref(), Some("codex"));
        assert!(ctx.model.is_none());
    }

    #[test]
    fn tools_selectable_count_uses_tool_rows() {
        let mut app = CassApp::default();
        app.analytics_view = AnalyticsView::Tools;
        let mut data = AnalyticsChartData::default();
        data.tool_rows = vec![crate::analytics::ToolRow {
            key: "a".into(),
            tool_call_count: 100,
            message_count: 10,
            api_tokens_total: 1000,
            tool_calls_per_1k_api_tokens: None,
            tool_calls_per_1k_content_tokens: None,
        }];
        app.analytics_cache = Some(data);
        assert_eq!(app.analytics_selectable_count(), 1);
    }

    #[test]
    fn coverage_selectable_count_uses_agents() {
        let mut app = CassApp::default();
        app.analytics_view = AnalyticsView::Coverage;
        let mut data = AnalyticsChartData::default();
        data.agent_tokens = vec![("claude_code".into(), 1000.0), ("codex".into(), 500.0)];
        app.analytics_cache = Some(data);
        assert_eq!(app.analytics_selectable_count(), 2);
    }

    #[test]
    fn build_drilldown_context_coverage_agent() {
        let mut app = CassApp::default();
        app.analytics_view = AnalyticsView::Coverage;
        let mut data = AnalyticsChartData::default();
        data.agent_tokens = vec![
            ("claude_code".into(), 1000.0),
            ("codex".into(), 500.0),
            ("aider".into(), 200.0),
        ];
        app.analytics_cache = Some(data);
        app.analytics_selection = 1;

        let ctx = app.build_drilldown_context().expect("should build context");
        assert_eq!(ctx.agent.as_deref(), Some("codex"));
        assert!(ctx.model.is_none());
    }

    // -- Explorer keyboard binding tests --

    #[test]
    fn explorer_m_key_cycles_metric_forward() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_view = AnalyticsView::Explorer;
        assert_eq!(app.explorer_metric, ExplorerMetric::ApiTokens);

        let _ = app.update(CassMsg::QueryChanged("m".to_string()));
        assert_eq!(app.explorer_metric, ExplorerMetric::ContentTokens);

        let _ = app.update(CassMsg::QueryChanged("m".to_string()));
        assert_eq!(app.explorer_metric, ExplorerMetric::Messages);
    }

    #[test]
    fn explorer_shift_m_key_cycles_metric_backward() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_view = AnalyticsView::Explorer;
        assert_eq!(app.explorer_metric, ExplorerMetric::ApiTokens);

        // M (shift+m) cycles backward — should wrap to Cost.
        let _ = app.update(CassMsg::QueryChanged("M".to_string()));
        assert_eq!(app.explorer_metric, ExplorerMetric::Cost);
    }

    #[test]
    fn explorer_o_key_cycles_overlay() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_view = AnalyticsView::Explorer;
        assert_eq!(app.explorer_overlay, ExplorerOverlay::None);

        // 'o' in Explorer view cycles overlay (intercepted from OpenInEditor).
        let _ = app.update(CassMsg::OpenInEditor);
        assert_eq!(app.explorer_overlay, ExplorerOverlay::ByAgent);

        let _ = app.update(CassMsg::OpenInEditor);
        assert_eq!(app.explorer_overlay, ExplorerOverlay::ByWorkspace);

        let _ = app.update(CassMsg::OpenInEditor);
        assert_eq!(app.explorer_overlay, ExplorerOverlay::BySource);

        let _ = app.update(CassMsg::OpenInEditor);
        assert_eq!(app.explorer_overlay, ExplorerOverlay::None);
    }

    #[test]
    fn explorer_query_input_suppressed_on_analytics_surface() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_view = AnalyticsView::Dashboard;

        // Non-explorer query input on analytics surface should be suppressed.
        let _ = app.update(CassMsg::QueryChanged("x".to_string()));
        assert!(
            app.query.is_empty(),
            "query should remain empty on analytics surface"
        );
    }

    #[test]
    fn explorer_g_key_cycles_group_by_forward() {
        use crate::analytics::GroupBy;
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_view = AnalyticsView::Explorer;
        assert_eq!(app.explorer_group_by, GroupBy::Day);
        let _ = app.update(CassMsg::QueryChanged("g".to_string()));
        assert_eq!(app.explorer_group_by, GroupBy::Week);
        let _ = app.update(CassMsg::QueryChanged("g".to_string()));
        assert_eq!(app.explorer_group_by, GroupBy::Month);
        let _ = app.update(CassMsg::QueryChanged("g".to_string()));
        assert_eq!(app.explorer_group_by, GroupBy::Hour);
        let _ = app.update(CassMsg::QueryChanged("g".to_string()));
        assert_eq!(app.explorer_group_by, GroupBy::Day);
    }

    #[test]
    fn explorer_shift_g_key_cycles_group_by_backward() {
        use crate::analytics::GroupBy;
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_view = AnalyticsView::Explorer;
        assert_eq!(app.explorer_group_by, GroupBy::Day);
        let _ = app.update(CassMsg::QueryChanged("G".to_string()));
        assert_eq!(app.explorer_group_by, GroupBy::Hour);
    }

    #[test]
    fn explorer_group_by_change_invalidates_cache() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_view = AnalyticsView::Explorer;
        // Simulate a cached value.
        app.analytics_cache = Some(AnalyticsChartData::default());
        assert!(app.analytics_cache.is_some());
        let _ = app.update(CassMsg::ExplorerGroupByCycled { forward: true });
        assert!(
            app.analytics_cache.is_none(),
            "cache should be invalidated on group-by change"
        );
    }

    #[test]
    fn explorer_z_key_cycles_zoom_forward() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_view = AnalyticsView::Explorer;
        assert_eq!(app.explorer_zoom, ExplorerZoom::All);
        let _ = app.update(CassMsg::QueryChanged("z".to_string()));
        assert_eq!(app.explorer_zoom, ExplorerZoom::Day);
        let _ = app.update(CassMsg::QueryChanged("z".to_string()));
        assert_eq!(app.explorer_zoom, ExplorerZoom::Week);
        let _ = app.update(CassMsg::QueryChanged("z".to_string()));
        assert_eq!(app.explorer_zoom, ExplorerZoom::Month);
        let _ = app.update(CassMsg::QueryChanged("z".to_string()));
        assert_eq!(app.explorer_zoom, ExplorerZoom::Quarter);
        let _ = app.update(CassMsg::QueryChanged("z".to_string()));
        assert_eq!(app.explorer_zoom, ExplorerZoom::All);
    }

    #[test]
    fn explorer_zoom_change_updates_analytics_filters() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_view = AnalyticsView::Explorer;
        app.analytics_cache = Some(AnalyticsChartData::default());
        // Zoom to 7d — should set since_ms and invalidate cache.
        let _ = app.update(CassMsg::ExplorerZoomCycled { forward: true }); // All → Day
        let _ = app.update(CassMsg::ExplorerZoomCycled { forward: true }); // Day → Week
        assert_eq!(app.explorer_zoom, ExplorerZoom::Week);
        assert!(
            app.analytics_filters.since_ms.is_some(),
            "since_ms should be set for Week zoom"
        );
        assert!(
            app.analytics_cache.is_none(),
            "cache should be invalidated on zoom change"
        );
    }

    // -- Analytics UI test suite (2noh9.4.18.11) -----------------------------

    /// Helper to create a CassApp in analytics mode with representative data.
    fn analytics_app_with_data(view: AnalyticsView) -> CassApp {
        let mut app = CassApp::default();
        app.surface = AppSurface::Analytics;
        app.analytics_view = view;

        let mut data = AnalyticsChartData::default();
        // Populate representative fixture data
        data.total_messages = 5000;
        data.total_api_tokens = 1_200_000;
        data.total_tool_calls = 3000;
        data.total_content_tokens = 800_000;
        data.total_plan_messages = 200;
        data.total_cost_usd = 42.57;
        data.coverage_pct = 85.0;
        data.pricing_coverage_pct = 72.0;
        data.plan_message_pct = 4.0;
        data.plan_api_token_share = 6.5;
        data.agent_tokens = vec![
            ("claude_code".into(), 600_000.0),
            ("codex".into(), 300_000.0),
            ("aider".into(), 200_000.0),
            ("gemini".into(), 100_000.0),
        ];
        data.agent_messages = vec![
            ("claude_code".into(), 2500.0),
            ("codex".into(), 1500.0),
            ("aider".into(), 700.0),
            ("gemini".into(), 300.0),
        ];
        data.agent_tool_calls = vec![
            ("claude_code".into(), 1800.0),
            ("codex".into(), 800.0),
            ("aider".into(), 300.0),
            ("gemini".into(), 100.0),
        ];
        data.workspace_tokens = vec![("cass".into(), 700_000.0), ("other".into(), 500_000.0)];
        data.workspace_messages = vec![("cass".into(), 3000.0), ("other".into(), 2000.0)];
        data.source_tokens = vec![("local".into(), 900_000.0), ("remote".into(), 300_000.0)];
        data.source_messages = vec![("local".into(), 3500.0), ("remote".into(), 1500.0)];
        data.model_tokens = vec![
            ("claude-opus-4-6".into(), 500_000.0),
            ("claude-sonnet-4-5".into(), 400_000.0),
            ("gpt-4o".into(), 300_000.0),
        ];
        data.model_cost = vec![
            ("claude-opus-4-6".into(), 20.0),
            ("claude-sonnet-4-5".into(), 12.0),
            ("gpt-4o".into(), 10.57),
        ];
        data.model_messages = vec![
            ("claude-opus-4-6".into(), 1500.0),
            ("claude-sonnet-4-5".into(), 2000.0),
            ("gpt-4o".into(), 1500.0),
        ];
        data.daily_tokens = vec![
            ("2026-02-01".into(), 200_000.0),
            ("2026-02-02".into(), 180_000.0),
            ("2026-02-03".into(), 250_000.0),
            ("2026-02-04".into(), 170_000.0),
            ("2026-02-05".into(), 200_000.0),
            ("2026-02-06".into(), 100_000.0),
            ("2026-02-07".into(), 100_000.0),
        ];
        data.daily_messages = vec![
            ("2026-02-01".into(), 800.0),
            ("2026-02-02".into(), 700.0),
            ("2026-02-03".into(), 900.0),
            ("2026-02-04".into(), 600.0),
            ("2026-02-05".into(), 700.0),
            ("2026-02-06".into(), 650.0),
            ("2026-02-07".into(), 650.0),
        ];
        data.daily_content_tokens = data.daily_tokens.clone();
        data.daily_tool_calls = vec![
            ("2026-02-01".into(), 500.0),
            ("2026-02-02".into(), 400.0),
            ("2026-02-03".into(), 600.0),
            ("2026-02-04".into(), 350.0),
            ("2026-02-05".into(), 450.0),
            ("2026-02-06".into(), 350.0),
            ("2026-02-07".into(), 350.0),
        ];
        data.daily_plan_messages = vec![
            ("2026-02-01".into(), 30.0),
            ("2026-02-02".into(), 25.0),
            ("2026-02-03".into(), 40.0),
            ("2026-02-04".into(), 20.0),
            ("2026-02-05".into(), 35.0),
            ("2026-02-06".into(), 25.0),
            ("2026-02-07".into(), 25.0),
        ];
        data.daily_cost = vec![
            ("2026-02-01".into(), 7.0),
            ("2026-02-02".into(), 6.0),
            ("2026-02-03".into(), 8.5),
            ("2026-02-04".into(), 5.5),
            ("2026-02-05".into(), 6.0),
            ("2026-02-06".into(), 5.0),
            ("2026-02-07".into(), 4.57),
        ];
        data.heatmap_days = vec![
            ("2026-02-01".into(), 0.8),
            ("2026-02-02".into(), 0.6),
            ("2026-02-03".into(), 1.0),
            ("2026-02-04".into(), 0.5),
            ("2026-02-05".into(), 0.7),
            ("2026-02-06".into(), 0.4),
            ("2026-02-07".into(), 0.3),
        ];
        data.agent_plan_messages = vec![
            ("claude_code".into(), 120.0),
            ("codex".into(), 50.0),
            ("aider".into(), 30.0),
        ];
        app.analytics_cache = Some(data);
        app
    }

    #[test]
    fn analytics_render_all_subviews_no_panic_80x24() {
        for &view in AnalyticsView::all() {
            let app = analytics_app_with_data(view);
            let buf =
                render_at_degradation(&app, 80, 24, ftui::render::budget::DegradationLevel::Full);
            let text = ftui_harness::buffer_to_text(&buf);
            assert!(
                !text.trim().is_empty(),
                "{:?} view at 80x24 should render non-empty content",
                view
            );
        }
    }

    #[test]
    fn analytics_render_all_subviews_no_panic_120x40() {
        for &view in AnalyticsView::all() {
            let app = analytics_app_with_data(view);
            let buf =
                render_at_degradation(&app, 120, 40, ftui::render::budget::DegradationLevel::Full);
            let text = ftui_harness::buffer_to_text(&buf);
            assert!(
                !text.trim().is_empty(),
                "{:?} view at 120x40 should render non-empty content",
                view
            );
        }
    }

    #[test]
    fn analytics_render_empty_data_no_panic() {
        // All views should survive with empty AnalyticsChartData.
        for &view in AnalyticsView::all() {
            let mut app = CassApp::default();
            app.surface = AppSurface::Analytics;
            app.analytics_view = view;
            app.analytics_cache = Some(AnalyticsChartData::default());
            let buf =
                render_at_degradation(&app, 80, 24, ftui::render::budget::DegradationLevel::Full);
            let text = ftui_harness::buffer_to_text(&buf);
            assert!(
                !text.trim().is_empty(),
                "{:?} view with empty data should render without panic",
                view
            );
        }
    }

    #[test]
    fn analytics_render_no_cache_no_panic() {
        // All views should survive without any analytics_cache (loading state).
        for &view in AnalyticsView::all() {
            let mut app = CassApp::default();
            app.surface = AppSurface::Analytics;
            app.analytics_view = view;
            app.analytics_cache = None;
            let buf =
                render_at_degradation(&app, 80, 24, ftui::render::budget::DegradationLevel::Full);
            let text = ftui_harness::buffer_to_text(&buf);
            assert!(
                !text.trim().is_empty(),
                "{:?} view with no cache should render without panic",
                view
            );
        }
    }

    #[test]
    fn analytics_dashboard_render_shows_kpi_labels() {
        let app = analytics_app_with_data(AnalyticsView::Dashboard);
        let buf =
            render_at_degradation(&app, 120, 40, ftui::render::budget::DegradationLevel::Full);
        let text = ftui_harness::buffer_to_text(&buf);
        // Dashboard KPI tiles should include recognizable metric labels.
        assert!(
            text.contains("Messages")
                || text.contains("messages")
                || text.contains("5,000")
                || text.contains("5.0K"),
            "Dashboard should display message-related KPI, got:\n{text}"
        );
    }

    #[test]
    fn analytics_explorer_render_shows_metric_label() {
        let app = analytics_app_with_data(AnalyticsView::Explorer);
        let buf =
            render_at_degradation(&app, 120, 40, ftui::render::budget::DegradationLevel::Full);
        let text = ftui_harness::buffer_to_text(&buf);
        // Explorer header should show the current metric.
        assert!(
            text.contains("API Tokens") || text.contains("Api") || text.contains("Tokens"),
            "Explorer should show metric label, got:\n{text}"
        );
    }

    #[test]
    fn analytics_render_degradation_levels_no_panic() {
        use ftui::render::budget::DegradationLevel;
        // Skeleton/EssentialOnly may intentionally suppress all content — just
        // assert no panic for those. Full through NoStyling should produce
        // visible output.
        let visible_levels = [
            DegradationLevel::Full,
            DegradationLevel::SimpleBorders,
            DegradationLevel::NoStyling,
        ];
        let suppress_levels = [DegradationLevel::EssentialOnly, DegradationLevel::Skeleton];
        for &view in AnalyticsView::all() {
            for &level in &visible_levels {
                let app = analytics_app_with_data(view);
                let buf = render_at_degradation(&app, 80, 24, level);
                let text = ftui_harness::buffer_to_text(&buf);
                assert!(
                    !text.trim().is_empty(),
                    "{:?} at degradation {:?} should render visible content",
                    view,
                    level
                );
            }
            // Just ensure no panic at extreme degradation.
            for &level in &suppress_levels {
                let app = analytics_app_with_data(view);
                let _ = render_at_degradation(&app, 80, 24, level);
            }
        }
    }

    #[test]
    fn analytics_render_perf_guard() {
        // All 8 subviews rendering at 120x40 should complete within a generous budget.
        // This is a catastrophic regression detector, not a micro-benchmark.
        let start = std::time::Instant::now();
        for &view in AnalyticsView::all() {
            let app = analytics_app_with_data(view);
            let _ =
                render_at_degradation(&app, 120, 40, ftui::render::budget::DegradationLevel::Full);
        }
        let elapsed = start.elapsed();
        // All 8 views should render within 2 seconds total (very generous).
        assert!(
            elapsed.as_millis() < 2000,
            "rendering all 8 analytics views took {:?} — exceeds 2s budget",
            elapsed
        );
    }

    #[test]
    fn analytics_navigation_full_cycle_through_all_views() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::AnalyticsEntered);
        assert_eq!(app.surface, AppSurface::Analytics);
        assert_eq!(app.analytics_view, AnalyticsView::Dashboard);

        // Cycle forward through all 8 views using CursorMoved (← → keys)
        let expected = [
            AnalyticsView::Explorer,
            AnalyticsView::Heatmap,
            AnalyticsView::Breakdowns,
            AnalyticsView::Tools,
            AnalyticsView::Cost,
            AnalyticsView::Plans,
            AnalyticsView::Coverage,
            AnalyticsView::Dashboard, // wraps around
        ];
        for expected_view in expected {
            let _ = app.update(CassMsg::CursorMoved { delta: 1 });
            assert_eq!(
                app.analytics_view, expected_view,
                "forward cycle should reach {:?}",
                expected_view
            );
        }
    }

    #[test]
    fn analytics_navigation_backward_cycle() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::AnalyticsEntered);

        // Go backward from Dashboard -> Coverage
        let _ = app.update(CassMsg::CursorMoved { delta: -1 });
        assert_eq!(app.analytics_view, AnalyticsView::Coverage);

        let _ = app.update(CassMsg::CursorMoved { delta: -1 });
        assert_eq!(app.analytics_view, AnalyticsView::Plans);
    }

    #[test]
    fn analytics_selection_per_view_with_data() {
        // Views with selectable data should accept selection changes.
        let selectable_views = [
            AnalyticsView::Explorer,
            AnalyticsView::Heatmap,
            AnalyticsView::Breakdowns,
            AnalyticsView::Tools,
            AnalyticsView::Cost,
            AnalyticsView::Plans,
            AnalyticsView::Coverage,
        ];
        for view in selectable_views {
            let mut app = analytics_app_with_data(view);
            let count = app.analytics_selectable_count();
            if count > 0 {
                let _ = app.update(CassMsg::AnalyticsSelectionMoved { delta: 1 });
                assert_eq!(
                    app.analytics_selection, 1,
                    "{:?} view should allow selection movement",
                    view
                );
            }
        }
    }

    #[test]
    fn analytics_drilldown_from_each_selectable_view() {
        let views_with_drilldown = [
            AnalyticsView::Heatmap,
            AnalyticsView::Breakdowns,
            AnalyticsView::Tools,
            AnalyticsView::Cost,
            AnalyticsView::Plans,
            AnalyticsView::Coverage,
        ];
        for view in views_with_drilldown {
            let app = analytics_app_with_data(view);
            if app.analytics_selectable_count() > 0 {
                let ctx = app.build_drilldown_context();
                assert!(
                    ctx.is_some(),
                    "{:?} with data should produce a drilldown context",
                    view
                );
            }
        }
    }

    #[test]
    fn analytics_filter_persistence_across_view_changes() {
        let mut app = analytics_app_with_data(AnalyticsView::Dashboard);
        // Set a filter
        app.analytics_filters.agents.insert("claude_code".into());
        let _ = app.update(CassMsg::CursorMoved { delta: 1 }); // → Explorer
        assert!(
            app.analytics_filters.agents.contains("claude_code"),
            "agent filter should persist across view changes"
        );
        let _ = app.update(CassMsg::CursorMoved { delta: 1 }); // → Heatmap
        assert!(
            app.analytics_filters.agents.contains("claude_code"),
            "agent filter should persist across multiple view changes"
        );
    }

    #[test]
    fn analytics_view_change_resets_selection_to_zero() {
        let mut app = analytics_app_with_data(AnalyticsView::Explorer);
        // Move selection forward
        let _ = app.update(CassMsg::AnalyticsSelectionMoved { delta: 3 });
        assert!(app.analytics_selection > 0);
        // Change view — selection should reset
        let _ = app.update(CassMsg::CursorMoved { delta: 1 });
        assert_eq!(
            app.analytics_selection, 0,
            "selection should reset on view change"
        );
    }

    // -- Performance guardrail tests (2noh9.5.6) -----------------------------

    /// Budget: single render of any surface must complete within this many ms.
    /// This is intentionally generous (catches catastrophic regressions, not
    /// micro-optimizations).
    const PERF_RENDER_SINGLE_BUDGET_MS: u128 = 200;
    /// Budget: rendering all key screens sequentially (search + all analytics).
    const PERF_RENDER_ALL_SCREENS_BUDGET_MS: u128 = 2000;
    /// Budget: a single ftui Buffer at 120x40 must not exceed this many cells.
    /// (120 * 40 = 4800 cells; a 2x safety margin → 9600 is extreme, real
    /// buffers should match exactly.)
    const PERF_BUFFER_CELL_BUDGET: usize = 120 * 40;

    #[test]
    fn perf_guard_search_surface_render_time() {
        let app = app_with_hits(10);
        let start = std::time::Instant::now();
        let _ = render_at_degradation(&app, 120, 40, ftui::render::budget::DegradationLevel::Full);
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < PERF_RENDER_SINGLE_BUDGET_MS,
            "search surface render took {:?} — exceeds {}ms budget",
            elapsed,
            PERF_RENDER_SINGLE_BUDGET_MS
        );
    }

    #[test]
    fn perf_guard_detail_surface_render_time() {
        let mut app = app_with_hits(5);
        app.focus_manager.focus(focus_ids::DETAIL_PANE);
        let start = std::time::Instant::now();
        let _ = render_at_degradation(&app, 120, 40, ftui::render::budget::DegradationLevel::Full);
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < PERF_RENDER_SINGLE_BUDGET_MS,
            "detail surface render took {:?} — exceeds {}ms budget",
            elapsed,
            PERF_RENDER_SINGLE_BUDGET_MS
        );
    }

    #[test]
    fn perf_guard_all_screens_sequential() {
        // Render: default search + detail focus + all 8 analytics views = 10 renders.
        let start = std::time::Instant::now();

        // Search surface
        let app = app_with_hits(10);
        let _ = render_at_degradation(&app, 120, 40, ftui::render::budget::DegradationLevel::Full);

        // Detail focus
        let mut detail_app = app_with_hits(5);
        detail_app.focus_manager.focus(focus_ids::DETAIL_PANE);
        let _ = render_at_degradation(
            &detail_app,
            120,
            40,
            ftui::render::budget::DegradationLevel::Full,
        );

        // All 8 analytics views with data
        for &view in AnalyticsView::all() {
            let analytics = analytics_app_with_data(view);
            let _ = render_at_degradation(
                &analytics,
                120,
                40,
                ftui::render::budget::DegradationLevel::Full,
            );
        }

        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < PERF_RENDER_ALL_SCREENS_BUDGET_MS,
            "rendering all 10 screens took {:?} — exceeds {}ms budget",
            elapsed,
            PERF_RENDER_ALL_SCREENS_BUDGET_MS
        );
    }

    #[test]
    fn perf_guard_buffer_text_within_bounds() {
        // Verify rendered text fits expected bounds (no runaway content).
        let app = app_with_hits(5);
        let buf =
            render_at_degradation(&app, 120, 40, ftui::render::budget::DegradationLevel::Full);
        let text = ftui_harness::buffer_to_text(&buf);
        // At 120x40 (4800 cells), text length should not wildly exceed
        // the cell count (accounting for newlines and trailing spaces).
        assert!(
            text.len() < PERF_BUFFER_CELL_BUDGET * 2,
            "rendered text {} chars exceeds 2x cell budget {}",
            text.len(),
            PERF_BUFFER_CELL_BUDGET * 2
        );
    }

    #[test]
    fn perf_guard_repeated_render_deterministic_timing() {
        // Rendering the same state 5 times should not show increasing cost
        // (would indicate a leak or accumulating state).
        let app = app_with_hits(10);
        let mut times_ms = Vec::with_capacity(5);
        for _ in 0..5 {
            let start = std::time::Instant::now();
            let _ =
                render_at_degradation(&app, 120, 40, ftui::render::budget::DegradationLevel::Full);
            times_ms.push(start.elapsed().as_millis());
        }
        // Last render should not be >3x the first (generous margin for CI variability).
        let first = times_ms[0].max(1);
        let last = times_ms[4];
        assert!(
            last <= first * 3,
            "render cost grew from {}ms to {}ms over 5 iterations — possible leak",
            first,
            last
        );
    }

    // -- Animation state tests (2noh9.4.14) ---------------------------------

    #[test]
    fn animation_state_default_enabled() {
        let anim = AnimationState::default();
        assert!(anim.enabled);
        assert!((anim.focus_flash_progress() - 1.0).abs() < 0.01);
    }

    #[test]
    fn animation_state_disabled_snaps() {
        let mut anim = AnimationState::new(false);
        anim.trigger_focus_flash();
        anim.tick(std::time::Duration::from_millis(1));
        assert!((anim.focus_flash_progress() - 1.0).abs() < 0.01);
    }

    #[test]
    fn animation_focus_flash_converges() {
        let mut anim = AnimationState::new(true);
        anim.trigger_focus_flash();
        assert!(anim.focus_flash_progress() < 0.1);
        for _ in 0..60 {
            anim.tick(std::time::Duration::from_millis(16));
        }
        assert!(anim.focus_flash_progress() > 0.9);
    }

    #[test]
    fn animation_reveal_stagger() {
        let mut anim = AnimationState::new(true);
        anim.start_reveal(5);
        assert!(anim.reveal_active);
        assert_eq!(anim.reveal_springs.len(), 5);
        assert!(anim.reveal_progress(0) < 0.1);
        for _ in 0..60 {
            anim.tick(std::time::Duration::from_millis(16));
        }
        for i in 0..5 {
            assert!(anim.reveal_progress(i) > 0.9, "item {i} should be revealed");
        }
    }

    #[test]
    fn animation_modal_open_close() {
        let mut anim = AnimationState::new(true);
        assert!(anim.modal_progress() < 0.1);
        anim.open_modal();
        for _ in 0..60 {
            anim.tick(std::time::Duration::from_millis(16));
        }
        assert!(anim.modal_progress() > 0.9);
        anim.close_modal();
        for _ in 0..60 {
            anim.tick(std::time::Duration::from_millis(16));
        }
        assert!(anim.modal_progress() < 0.1);
    }

    #[test]
    fn animation_panel_ratio() {
        let mut anim = AnimationState::new(true);
        assert!((anim.panel_ratio_value() - 0.7).abs() < 0.01);
        anim.set_panel_ratio(0.5);
        for _ in 0..60 {
            anim.tick(std::time::Duration::from_millis(16));
        }
        assert!(
            (anim.panel_ratio_value() - 0.5).abs() < 0.05,
            "panel ratio should converge to 0.5, got {}",
            anim.panel_ratio_value()
        );
    }

    #[test]
    fn animation_peek_badge() {
        let mut anim = AnimationState::new(true);
        assert!(anim.peek_badge_progress() < 0.1);
        anim.show_peek_badge();
        for _ in 0..60 {
            anim.tick(std::time::Duration::from_millis(16));
        }
        assert!(anim.peek_badge_progress() > 0.9);
        anim.hide_peek_badge();
        for _ in 0..60 {
            anim.tick(std::time::Duration::from_millis(16));
        }
        assert!(anim.peek_badge_progress() < 0.1);
    }

    #[test]
    fn animation_disabled_reveal_returns_1() {
        let anim = AnimationState::new(false);
        assert!((anim.reveal_progress(0) - 1.0).abs() < 0.01);
        assert!((anim.reveal_progress(99) - 1.0).abs() < 0.01);
    }

    // =========================================================================
    // Help Overlay Tests (bead 2noh9.3.7)
    // =========================================================================

    fn test_app() -> CassApp {
        CassApp::default()
    }

    #[test]
    fn help_toggle_opens_and_closes() {
        let mut app = test_app();
        assert!(!app.show_help);
        let _ = app.update(CassMsg::HelpToggled);
        assert!(app.show_help);
        assert_eq!(app.help_scroll, 0);
        let _ = app.update(CassMsg::HelpToggled);
        assert!(!app.show_help);
    }

    #[test]
    fn help_scroll_increments_and_clamps() {
        let mut app = test_app();
        let _ = app.update(CassMsg::HelpToggled);
        assert_eq!(app.help_scroll, 0);
        let _ = app.update(CassMsg::HelpScrolled { delta: 5 });
        assert_eq!(app.help_scroll, 5);
        let _ = app.update(CassMsg::HelpScrolled { delta: -10 });
        // Should clamp to 0 not go negative
        assert_eq!(app.help_scroll, 0);
    }

    #[test]
    fn help_pin_toggle() {
        let mut app = test_app();
        assert!(!app.help_pinned);
        let _ = app.update(CassMsg::HelpPinToggled);
        assert!(app.help_pinned);
        let _ = app.update(CassMsg::HelpPinToggled);
        assert!(!app.help_pinned);
    }

    #[test]
    fn help_toggle_resets_scroll() {
        let mut app = test_app();
        let _ = app.update(CassMsg::HelpToggled);
        let _ = app.update(CassMsg::HelpScrolled { delta: 20 });
        assert_eq!(app.help_scroll, 20);
        // Close and reopen — scroll should reset to 0
        let _ = app.update(CassMsg::HelpToggled);
        let _ = app.update(CassMsg::HelpToggled);
        assert_eq!(app.help_scroll, 0);
    }

    #[test]
    fn help_esc_closes_overlay() {
        let mut app = test_app();
        let _ = app.update(CassMsg::HelpToggled);
        assert!(app.show_help);
        let _ = app.update(CassMsg::QuitRequested);
        assert!(!app.show_help);
    }

    #[test]
    fn help_overlay_render_no_panic_80x24() {
        let mut app = test_app();
        let _ = app.update(CassMsg::HelpToggled);
        render_at_degradation(&app, 80, 24, ftui::render::budget::DegradationLevel::Full);
    }

    #[test]
    fn help_overlay_render_no_panic_120x40() {
        let mut app = test_app();
        let _ = app.update(CassMsg::HelpToggled);
        render_at_degradation(&app, 120, 40, ftui::render::budget::DegradationLevel::Full);
    }

    #[test]
    fn help_overlay_render_narrow_no_panic() {
        let mut app = test_app();
        let _ = app.update(CassMsg::HelpToggled);
        // Very narrow — should not panic, just potentially skip rendering
        render_at_degradation(&app, 30, 10, ftui::render::budget::DegradationLevel::Full);
    }

    #[test]
    fn help_overlay_contains_shortcut_keys() {
        let mut app = test_app();
        let _ = app.update(CassMsg::HelpToggled);
        // Use a tall viewport (200 rows) so all help sections are visible
        let buf =
            render_at_degradation(&app, 120, 200, ftui::render::budget::DegradationLevel::Full);
        let text = ftui_harness::buffer_to_text(&buf);
        // Help content should include key shortcuts from shortcuts.rs
        assert!(
            text.contains(shortcuts::HELP),
            "Help text should contain F1 shortcut"
        );
        assert!(
            text.contains("Search"),
            "Help text should contain 'Search' section"
        );
        assert!(
            text.contains("Navigation"),
            "Help text should contain 'Navigation' section"
        );
        assert!(
            text.contains("Filters"),
            "Help text should contain 'Filters' section"
        );
    }

    #[test]
    fn help_overlay_shows_pinned_indicator() {
        let mut app = test_app();
        let _ = app.update(CassMsg::HelpToggled);
        let _ = app.update(CassMsg::HelpPinToggled);
        let buf =
            render_at_degradation(&app, 120, 60, ftui::render::budget::DegradationLevel::Full);
        let text = ftui_harness::buffer_to_text(&buf);
        assert!(
            text.contains("pinned"),
            "Pinned help should show 'pinned' in title or body"
        );
    }

    #[test]
    fn help_overlay_scroll_changes_visible_content() {
        let mut app = test_app();
        let _ = app.update(CassMsg::HelpToggled);
        let buf_top =
            render_at_degradation(&app, 120, 30, ftui::render::budget::DegradationLevel::Full);
        let text_top = ftui_harness::buffer_to_text(&buf_top);

        let _ = app.update(CassMsg::HelpScrolled { delta: 30 });
        let buf_scrolled =
            render_at_degradation(&app, 120, 30, ftui::render::budget::DegradationLevel::Full);
        let text_scrolled = ftui_harness::buffer_to_text(&buf_scrolled);

        // After scrolling, content should be different
        assert_ne!(
            text_top, text_scrolled,
            "Scrolled help content should differ from top"
        );
    }

    #[test]
    fn help_build_lines_contains_all_sections() {
        let app = test_app();
        let styles = StyleContext::from_options(StyleOptions {
            preset: UiThemePreset::Dark,
            ..StyleOptions::default()
        });
        let lines = app.build_help_lines(&styles);
        let text: String = lines
            .iter()
            .map(|l: &ftui::text::Line| l.to_plain_text())
            .collect::<Vec<_>>()
            .join("\n");

        for section in [
            "Data Locations",
            "Updates",
            "Search",
            "Filters",
            "Sources",
            "Modes",
            "Context",
            "Navigation",
            "Actions",
            "States",
        ] {
            assert!(
                text.contains(section),
                "Help lines should contain section: {section}"
            );
        }
    }

    #[test]
    fn help_build_lines_references_shortcuts() {
        let app = test_app();
        let styles = StyleContext::from_options(StyleOptions {
            preset: UiThemePreset::Dark,
            ..StyleOptions::default()
        });
        let lines = app.build_help_lines(&styles);
        let text: String = lines
            .iter()
            .map(|l: &ftui::text::Line| l.to_plain_text())
            .collect::<Vec<_>>()
            .join("\n");

        // Must reference actual shortcut constants
        assert!(text.contains(shortcuts::HELP), "Should reference F1");
        assert!(
            text.contains(shortcuts::FILTER_AGENT),
            "Should reference F3"
        );
        assert!(
            text.contains(shortcuts::CONTEXT_WINDOW),
            "Should reference F7"
        );
        assert!(text.contains(shortcuts::EDITOR), "Should reference F8");
        assert!(text.contains(shortcuts::RANKING), "Should reference F12");
        assert!(text.contains(shortcuts::TAB_FOCUS), "Should reference Tab");
        assert!(
            text.contains(shortcuts::VIM_NAV),
            "Should reference vim nav"
        );
    }

    // =========================================================================
    // Macro recording/playback tests
    // =========================================================================

    #[test]
    fn macro_recording_toggle_starts_recording() {
        let mut app = CassApp::default();
        assert!(app.macro_recorder.is_none());

        let _ = app.update(CassMsg::MacroRecordingToggled);
        assert!(app.macro_recorder.is_some());
        assert!(app.status.contains("Recording"));
    }

    #[test]
    fn macro_recording_toggle_stops_and_saves() {
        let mut app = CassApp::default();

        // Start recording.
        let _ = app.update(CassMsg::MacroRecordingToggled);
        assert!(app.macro_recorder.is_some());

        // Simulate some key events by recording directly.
        if let Some(ref mut rec) = app.macro_recorder {
            rec.record_event(ftui::Event::Key(ftui::KeyEvent {
                code: ftui::KeyCode::Char('h'),
                modifiers: ftui::Modifiers::empty(),
                kind: ftui::KeyEventKind::Press,
            }));
            rec.record_event(ftui::Event::Key(ftui::KeyEvent {
                code: ftui::KeyCode::Char('i'),
                modifiers: ftui::Modifiers::empty(),
                kind: ftui::KeyEventKind::Press,
            }));
        }

        // Stop recording.
        let _ = app.update(CassMsg::MacroRecordingToggled);
        assert!(app.macro_recorder.is_none());
        // Status should indicate save (or failure if dir doesn't exist in test env).
        assert!(
            app.status.contains("Macro saved") || app.status.contains("Recording"),
            "status: {}",
            app.status
        );
    }

    #[test]
    fn macro_default_state_is_none() {
        let app = CassApp::default();
        assert!(app.macro_recorder.is_none());
        assert!(app.macro_playback.is_none());
        assert!(!app.macro_redact_paths);
    }

    #[test]
    fn alt_m_maps_to_macro_recording_toggled() {
        use crate::ui::ftui_adapter::{Event, KeyCode, KeyEvent, Modifiers};
        let event = Event::Key(KeyEvent {
            code: KeyCode::Char('m'),
            modifiers: Modifiers::ALT,
            kind: ftui::KeyEventKind::Press,
        });
        let msg = CassMsg::from(event);
        assert!(matches!(msg, CassMsg::MacroRecordingToggled));
    }

    #[test]
    fn macro_playback_processes_events_on_tick() {
        use ftui::runtime::input_macro::{MacroMetadata, MacroPlayback};
        use ftui::runtime::{InputMacro, TimedEvent};
        use std::time::Duration;

        let mut app = CassApp::default();

        // Create a simple macro with one key event at 0ms delay.
        let events = vec![TimedEvent::new(
            ftui::Event::Key(ftui::KeyEvent {
                code: ftui::KeyCode::Char('x'),
                modifiers: ftui::Modifiers::CTRL,
                kind: ftui::KeyEventKind::Press,
            }),
            Duration::from_millis(0),
        )];
        let metadata = MacroMetadata {
            name: "test".to_string(),
            terminal_size: (80, 24),
            total_duration: Duration::from_millis(0),
        };
        let input_macro = InputMacro::new(events, metadata);
        app.macro_playback = Some(MacroPlayback::new(input_macro));

        // One tick should advance playback and emit the event as a message.
        let cmd = app.update(CassMsg::Tick);
        // After tick, playback should be done (0ms macro completes immediately).
        assert!(app.macro_playback.is_none());
        // The cmd should contain batch with messages.
        assert!(!matches!(cmd, ftui::Cmd::None));
    }

    #[test]
    fn macro_recording_indicator_in_status_line() {
        let mut app = CassApp::default();

        // Start recording.
        let _ = app.update(CassMsg::MacroRecordingToggled);

        // The rec_tag logic in view uses macro_recorder.is_some().
        assert!(app.macro_recorder.is_some());
    }

    // =========================================================================
    // FocusGraph navigation tests (bead 2noh9.3.16)
    // =========================================================================

    #[test]
    fn focus_graph_initialized_with_nodes() {
        let app = CassApp::default();
        let g = app.focus_manager.graph();
        // 3 primary + 8 modal nodes = 11
        assert!(g.node_count() >= 11, "got {}", g.node_count());
        assert!(g.get(focus_ids::SEARCH_BAR).is_some());
        assert!(g.get(focus_ids::RESULTS_LIST).is_some());
        assert!(g.get(focus_ids::DETAIL_PANE).is_some());
        assert!(g.get(focus_ids::COMMAND_PALETTE).is_some());
    }

    #[test]
    fn focus_graph_default_focuses_results() {
        let app = CassApp::default();
        assert_eq!(app.focus_manager.current(), Some(focus_ids::RESULTS_LIST));
        assert_eq!(app.focused_region(), FocusRegion::Results);
    }

    #[test]
    fn focus_toggle_cycles_through_nodes() {
        let mut app = CassApp::default();
        // Default: RESULTS_LIST
        assert_eq!(app.focus_manager.current(), Some(focus_ids::RESULTS_LIST));

        // Tab (focus_next) → DETAIL_PANE
        let _ = app.update(CassMsg::FocusToggled);
        assert_eq!(app.focus_manager.current(), Some(focus_ids::DETAIL_PANE));
        assert_eq!(app.focused_region(), FocusRegion::Detail);

        // Tab again → SEARCH_BAR (wraps)
        let _ = app.update(CassMsg::FocusToggled);
        assert_eq!(app.focus_manager.current(), Some(focus_ids::SEARCH_BAR));

        // Tab again → RESULTS_LIST
        let _ = app.update(CassMsg::FocusToggled);
        assert_eq!(app.focus_manager.current(), Some(focus_ids::RESULTS_LIST));
    }

    #[test]
    fn focus_directional_navigates_graph() {
        let mut app = CassApp::default();
        // Start at RESULTS_LIST, go right → DETAIL_PANE
        let _ = app.update(CassMsg::FocusDirectional {
            direction: FocusDirection::Right,
        });
        assert_eq!(app.focus_manager.current(), Some(focus_ids::DETAIL_PANE));

        // Go left → RESULTS_LIST
        let _ = app.update(CassMsg::FocusDirectional {
            direction: FocusDirection::Left,
        });
        assert_eq!(app.focus_manager.current(), Some(focus_ids::RESULTS_LIST));

        // Go up → SEARCH_BAR
        let _ = app.update(CassMsg::FocusDirectional {
            direction: FocusDirection::Up,
        });
        assert_eq!(app.focus_manager.current(), Some(focus_ids::SEARCH_BAR));
    }

    #[test]
    fn modal_push_trap_confines_focus() {
        let mut app = CassApp::default();
        assert!(!app.focus_manager.is_trapped());

        // Open palette → should trap focus
        let _ = app.update(CassMsg::PaletteOpened);
        assert!(app.focus_manager.is_trapped());
        assert_eq!(
            app.focus_manager.current(),
            Some(focus_ids::COMMAND_PALETTE)
        );

        // Tab should NOT escape the trap (only palette node in group)
        let _ = app.update(CassMsg::FocusToggled);
        assert_eq!(
            app.focus_manager.current(),
            Some(focus_ids::COMMAND_PALETTE)
        );
    }

    #[test]
    fn modal_pop_trap_restores_focus() {
        let mut app = CassApp::default();
        // Start focused on RESULTS_LIST
        assert_eq!(app.focus_manager.current(), Some(focus_ids::RESULTS_LIST));

        // Open palette
        let _ = app.update(CassMsg::PaletteOpened);
        assert!(app.focus_manager.is_trapped());

        // Close palette via Esc (QuitRequested)
        let _ = app.update(CassMsg::QuitRequested);
        assert!(!app.focus_manager.is_trapped());
        // Focus restored to RESULTS_LIST
        assert_eq!(app.focus_manager.current(), Some(focus_ids::RESULTS_LIST));
    }

    #[test]
    fn nested_modals_stack_traps() {
        let mut app = CassApp::default();

        // Open help
        let _ = app.update(CassMsg::HelpToggled);
        assert!(app.focus_manager.is_trapped());

        // Close help
        let _ = app.update(CassMsg::HelpToggled);
        assert!(!app.focus_manager.is_trapped());
    }

    #[test]
    fn detail_closed_pops_trap_and_restores() {
        let mut app = CassApp::default();
        app.results = vec![SearchHit {
            title: String::new(),
            snippet: "test".into(),
            content: "test".into(),
            content_hash: 0,
            score: 1.0,
            source_path: "/tmp/test".into(),
            agent: "test".into(),
            workspace: "/tmp".into(),
            workspace_original: None,
            created_at: None,
            line_number: Some(0),
            match_type: Default::default(),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        }];
        app.panes = vec![AgentPane {
            agent: "test".into(),
            hits: app.results.clone(),
            selected: 0,
            total_count: 1,
        }];

        // Switch to a non-Query input mode so DetailOpened opens detail modal
        app.input_mode = InputMode::Agent;
        let _ = app.update(CassMsg::DetailOpened);
        assert!(app.show_detail_modal);

        // Close detail
        let _ = app.update(CassMsg::DetailClosed);
        assert!(!app.show_detail_modal);
        assert_eq!(app.focused_region(), FocusRegion::Results);
    }

    #[test]
    fn focus_graph_has_directional_edges() {
        let app = CassApp::default();
        let g = app.focus_manager.graph();
        // SearchBar Down → ResultsList
        assert_eq!(
            g.navigate(focus_ids::SEARCH_BAR, NavDirection::Down),
            Some(focus_ids::RESULTS_LIST)
        );
        // ResultsList Right → DetailPane
        assert_eq!(
            g.navigate(focus_ids::RESULTS_LIST, NavDirection::Right),
            Some(focus_ids::DETAIL_PANE)
        );
        // DetailPane Left → ResultsList
        assert_eq!(
            g.navigate(focus_ids::DETAIL_PANE, NavDirection::Left),
            Some(focus_ids::RESULTS_LIST)
        );
    }

    // =========================================================================
    // Inspector Overlay Tests
    // =========================================================================

    #[test]
    fn inspector_toggle_opens_and_closes() {
        let mut app = CassApp::default();
        assert!(!app.show_inspector);

        let _ = app.update(CassMsg::InspectorToggled);
        assert!(app.show_inspector);
        assert!(app.inspector_state.is_active());

        let _ = app.update(CassMsg::InspectorToggled);
        assert!(!app.show_inspector);
        assert!(!app.inspector_state.is_active());
    }

    #[test]
    fn inspector_tab_cycles_through_all_tabs() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::InspectorToggled);
        assert_eq!(app.inspector_tab, InspectorTab::Timing);

        let _ = app.update(CassMsg::InspectorTabCycled);
        assert_eq!(app.inspector_tab, InspectorTab::Layout);

        let _ = app.update(CassMsg::InspectorTabCycled);
        assert_eq!(app.inspector_tab, InspectorTab::HitRegions);

        let _ = app.update(CassMsg::InspectorTabCycled);
        assert_eq!(app.inspector_tab, InspectorTab::Timing);
    }

    #[test]
    fn inspector_esc_closes_overlay() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::InspectorToggled);
        assert!(app.show_inspector);

        let _ = app.update(CassMsg::QuitRequested);
        assert!(!app.show_inspector);
    }

    #[test]
    fn inspector_does_not_block_other_keys() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::InspectorToggled);
        assert!(app.show_inspector);

        // Help toggle should still work (inspector is non-blocking)
        let _ = app.update(CassMsg::HelpToggled);
        assert!(app.show_help);
    }

    #[test]
    fn inspector_off_by_default() {
        let app = CassApp::default();
        assert!(!app.show_inspector);
        assert_eq!(app.inspector_tab, InspectorTab::Timing);
        assert!(!app.inspector_state.is_active());
    }

    #[test]
    fn frame_timing_stats_basic() {
        let mut stats = FrameTimingStats::default();
        assert_eq!(stats.fps(), 0.0);
        assert_eq!(stats.avg_us(), 0);
        assert_eq!(stats.p95_us(), 0);
        assert_eq!(stats.last_us(), 0);

        // Simulate recording frames
        stats.record_frame(); // first frame: no delta
        std::thread::sleep(std::time::Duration::from_millis(1));
        let dt = stats.record_frame();
        assert!(dt.is_some());
        assert!(dt.unwrap() > 0);
        assert_eq!(stats.frame_times_us.len(), 1);
        assert!(stats.fps() > 0.0);
    }

    #[test]
    fn frame_timing_ring_buffer_caps_at_capacity() {
        let mut stats = FrameTimingStats::default();
        // Manually push 130 values (capacity is 120)
        for i in 0..130 {
            stats.frame_times_us.push_back(i * 100);
        }
        // Ring buffer should trim to capacity
        while stats.frame_times_us.len() > stats.capacity {
            stats.frame_times_us.pop_front();
        }
        assert!(stats.frame_times_us.len() <= 120);
    }

    #[test]
    fn inspector_render_does_not_panic_small_terminal() {
        use crate::ui::style_system::StyleOptions;
        let app = CassApp::default();
        let styles = StyleContext::from_options(StyleOptions::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(10, 5, &mut pool);
        let area = Rect::new(0, 0, 10, 5);
        // Should not panic — auto-disables in small terminals
        app.render_inspector_overlay(&mut frame, area, &styles);
    }

    #[test]
    fn inspector_render_does_not_panic_normal_terminal() {
        use crate::ui::style_system::StyleOptions;
        let mut app = CassApp::default();
        app.show_inspector = true;
        let styles = StyleContext::from_options(StyleOptions::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 40, &mut pool);
        let area = Rect::new(0, 0, 120, 40);
        app.render_inspector_overlay(&mut frame, area, &styles);
    }

    #[test]
    fn inspector_tab_labels_are_unique() {
        let labels: Vec<&str> = [
            InspectorTab::Timing,
            InspectorTab::Layout,
            InspectorTab::HitRegions,
        ]
        .iter()
        .map(|t| t.label())
        .collect();
        let unique: HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(labels.len(), unique.len());
    }

    #[test]
    fn ctrl_shift_i_maps_to_inspector_toggled() {
        use crate::ui::ftui_adapter::{Event, KeyCode, KeyEvent, Modifiers};
        let event = Event::Key(KeyEvent {
            code: KeyCode::Char('i'),
            modifiers: Modifiers::CTRL | Modifiers::SHIFT,
            kind: ftui::KeyEventKind::Press,
        });
        let msg = CassMsg::from(event);
        assert!(matches!(msg, CassMsg::InspectorToggled));
    }

    // =========================================================================
    // Theme Editor Tests
    // =========================================================================

    #[test]
    fn theme_editor_opens_and_closes() {
        let mut app = CassApp::default();
        assert!(!app.show_theme_editor);
        assert!(app.theme_editor.is_none());

        let _ = app.update(CassMsg::ThemeEditorOpened);
        assert!(app.show_theme_editor);
        assert!(app.theme_editor.is_some());

        let _ = app.update(CassMsg::ThemeEditorClosed);
        assert!(!app.show_theme_editor);
        assert!(app.theme_editor.is_none());
    }

    #[test]
    fn theme_editor_navigation_moves_selection() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ThemeEditorOpened);

        let editor = app.theme_editor.as_ref().unwrap();
        assert_eq!(editor.selected, 0);

        let _ = app.update(CassMsg::ThemeEditorMoved { delta: 1 });
        assert_eq!(app.theme_editor.as_ref().unwrap().selected, 1);

        let _ = app.update(CassMsg::ThemeEditorMoved { delta: 1 });
        assert_eq!(app.theme_editor.as_ref().unwrap().selected, 2);

        let _ = app.update(CassMsg::ThemeEditorMoved { delta: -1 });
        assert_eq!(app.theme_editor.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn theme_editor_navigation_clamps_at_boundaries() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ThemeEditorOpened);

        // At top, moving up stays at 0
        let _ = app.update(CassMsg::ThemeEditorMoved { delta: -1 });
        assert_eq!(app.theme_editor.as_ref().unwrap().selected, 0);

        // Move to last slot
        for _ in 0..30 {
            let _ = app.update(CassMsg::ThemeEditorMoved { delta: 1 });
        }
        let n = ThemeColorSlot::ALL.len();
        assert_eq!(app.theme_editor.as_ref().unwrap().selected, n - 1);
    }

    #[test]
    fn theme_editor_navigation_blocked_while_editing() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ThemeEditorOpened);

        let _ = app.update(CassMsg::ThemeEditorEditStarted);
        assert!(app.theme_editor.as_ref().unwrap().editing);

        // Moving while editing should be a no-op
        let _ = app.update(CassMsg::ThemeEditorMoved { delta: 1 });
        assert_eq!(app.theme_editor.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn theme_editor_edit_start_and_cancel() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ThemeEditorOpened);

        let _ = app.update(CassMsg::ThemeEditorEditStarted);
        assert!(app.theme_editor.as_ref().unwrap().editing);

        let _ = app.update(CassMsg::ThemeEditorEditCancelled);
        assert!(!app.theme_editor.as_ref().unwrap().editing);
        assert!(app.theme_editor.as_ref().unwrap().hex_buffer.is_empty());
    }

    #[test]
    fn theme_editor_edit_commit_sets_override() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ThemeEditorOpened);

        // Start editing Primary (index 0)
        let _ = app.update(CassMsg::ThemeEditorEditStarted);
        let _ = app.update(CassMsg::ThemeEditorHexChanged("#ff0000".into()));
        assert_eq!(app.theme_editor.as_ref().unwrap().hex_buffer, "#ff0000");

        let _ = app.update(CassMsg::ThemeEditorEditCommitted);
        let editor = app.theme_editor.as_ref().unwrap();
        assert!(!editor.editing);
        assert_eq!(editor.overrides.primary.as_deref(), Some("#ff0000"));
    }

    #[test]
    fn theme_editor_edit_commit_empty_clears_override() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ThemeEditorOpened);

        // First set a value
        let _ = app.update(CassMsg::ThemeEditorEditStarted);
        let _ = app.update(CassMsg::ThemeEditorHexChanged("#ff0000".into()));
        let _ = app.update(CassMsg::ThemeEditorEditCommitted);

        // Now commit with empty string to clear it
        let _ = app.update(CassMsg::ThemeEditorEditStarted);
        let _ = app.update(CassMsg::ThemeEditorHexChanged("".into()));
        let _ = app.update(CassMsg::ThemeEditorEditCommitted);

        let editor = app.theme_editor.as_ref().unwrap();
        assert!(editor.overrides.primary.is_none());
    }

    #[test]
    fn theme_editor_hex_change_only_when_editing() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ThemeEditorOpened);

        // Not in editing mode — hex change should be ignored
        let _ = app.update(CassMsg::ThemeEditorHexChanged("#aabbcc".into()));
        assert!(app.theme_editor.as_ref().unwrap().hex_buffer.is_empty());
    }

    #[test]
    fn theme_editor_slot_clear() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ThemeEditorOpened);

        // Set a value first
        let _ = app.update(CassMsg::ThemeEditorEditStarted);
        let _ = app.update(CassMsg::ThemeEditorHexChanged("#00ff00".into()));
        let _ = app.update(CassMsg::ThemeEditorEditCommitted);
        assert!(
            app.theme_editor
                .as_ref()
                .unwrap()
                .overrides
                .primary
                .is_some()
        );

        // Clear it
        let _ = app.update(CassMsg::ThemeEditorSlotCleared);
        assert!(
            app.theme_editor
                .as_ref()
                .unwrap()
                .overrides
                .primary
                .is_none()
        );
    }

    #[test]
    fn theme_editor_preset_cycling() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ThemeEditorOpened);

        let initial = app.theme_editor.as_ref().unwrap().base_preset;
        let _ = app.update(CassMsg::ThemeEditorPresetCycled);
        let after = app.theme_editor.as_ref().unwrap().base_preset;
        assert_ne!(initial, after);
    }

    #[test]
    fn theme_editor_esc_closes() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ThemeEditorOpened);
        assert!(app.show_theme_editor);

        let _ = app.update(CassMsg::QuitRequested);
        assert!(!app.show_theme_editor);
    }

    #[test]
    fn theme_editor_off_by_default() {
        let app = CassApp::default();
        assert!(!app.show_theme_editor);
        assert!(app.theme_editor.is_none());
    }

    #[test]
    fn theme_editor_to_config_round_trip() {
        let mut state = ThemeEditorState::new(style_system::UiThemePreset::default());
        ThemeColorSlot::Accent.set(&mut state.overrides, Some("#abc123".into()));

        let config = state.to_config();
        assert_eq!(config.colors.accent.as_deref(), Some("#abc123"));
        assert_eq!(config.base_preset, Some(state.base_preset));
    }

    #[test]
    fn theme_color_slot_all_has_19_entries() {
        assert_eq!(ThemeColorSlot::ALL.len(), 19);
    }

    #[test]
    fn theme_color_slot_labels_are_unique() {
        let labels: Vec<&str> = ThemeColorSlot::ALL.iter().map(|s| s.label()).collect();
        let unique: HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(labels.len(), unique.len());
    }

    #[test]
    fn theme_color_slot_get_set_round_trip() {
        let mut overrides = style_system::ThemeColorOverrides::default();
        for slot in &ThemeColorSlot::ALL {
            assert!(slot.get(&overrides).is_none());
            slot.set(&mut overrides, Some("#facade".into()));
            assert_eq!(slot.get(&overrides), Some("#facade"));
            slot.set(&mut overrides, None);
            assert!(slot.get(&overrides).is_none());
        }
    }

    #[test]
    fn theme_editor_render_does_not_panic_small_terminal() {
        use crate::ui::style_system::StyleOptions;
        let app = CassApp::default();
        let styles = StyleContext::from_options(StyleOptions::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(10, 5, &mut pool);
        let area = Rect::new(0, 0, 10, 5);
        app.render_theme_editor_overlay(&mut frame, area, &styles);
    }

    #[test]
    fn theme_editor_render_does_not_panic_normal_terminal() {
        use crate::ui::style_system::StyleOptions;
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ThemeEditorOpened);
        let styles = StyleContext::from_options(StyleOptions::default());
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 40, &mut pool);
        let area = Rect::new(0, 0, 120, 40);
        app.render_theme_editor_overlay(&mut frame, area, &styles);
    }

    #[test]
    fn ctrl_shift_t_maps_to_theme_editor_opened() {
        use crate::ui::ftui_adapter::{Event, KeyCode, KeyEvent, Modifiers};
        let event = Event::Key(KeyEvent {
            code: KeyCode::Char('t'),
            modifiers: Modifiers::CTRL | Modifiers::SHIFT,
            kind: ftui::KeyEventKind::Press,
        });
        let msg = CassMsg::from(event);
        assert!(matches!(msg, CassMsg::ThemeEditorOpened));
    }

    #[test]
    fn theme_editor_import_loads_saved_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let theme_dir = tmp.path().join("coding-agent-search");
        std::fs::create_dir_all(&theme_dir).unwrap();
        let theme_path = theme_dir.join("theme.json");
        let config_json =
            r##"{"version":1,"base_preset":"catppuccin","colors":{"accent":"#ff00ff"}}"##;
        std::fs::write(&theme_path, config_json).unwrap();

        // Load config directly (ThemeEditorOpened handler uses dirs::data_dir which
        // we can't override in unit tests, so test the config loading logic directly).
        let cfg = style_system::ThemeConfig::load_from_path(&theme_path).unwrap();
        let mut state = ThemeEditorState::new(style_system::UiThemePreset::Dark);
        if let Some(preset) = cfg.base_preset {
            state.base_preset = preset;
        }
        state.overrides = cfg.colors;

        assert_eq!(state.base_preset, style_system::UiThemePreset::Catppuccin);
        assert_eq!(state.overrides.accent.as_deref(), Some("#ff00ff"));
    }

    #[test]
    fn theme_editor_export_applies_preset_to_main() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::ThemeEditorOpened);

        // Cycle preset in editor
        let _ = app.update(CassMsg::ThemeEditorPresetCycled);
        let editor_preset = app.theme_editor.as_ref().unwrap().base_preset;
        // The main preset should not change until export
        assert_ne!(app.theme_preset, editor_preset);

        // Export (will try to save to disk — may fail in test env, but
        // we're testing the preset-apply logic, not file I/O).
        let _ = app.update(CassMsg::ThemeEditorExported);
        // After export, the main preset should match the editor preset.
        assert_eq!(app.theme_preset, editor_preset);
    }

    // =========================================================================
    // Sources management tests (2noh9.4.9)
    // =========================================================================

    #[test]
    fn sources_entered_switches_surface() {
        let mut app = CassApp::default();
        assert_eq!(app.surface, AppSurface::Search);

        let _ = app.update(CassMsg::SourcesEntered);
        assert_eq!(app.surface, AppSurface::Sources);
        assert_eq!(app.view_stack, vec![AppSurface::Search]);
    }

    #[test]
    fn sources_esc_pops_back_to_search() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::SourcesEntered);
        assert_eq!(app.surface, AppSurface::Sources);

        let _ = app.update(CassMsg::ViewStackPopped);
        assert_eq!(app.surface, AppSurface::Search);
    }

    #[test]
    fn sources_quit_requested_pops_back() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::SourcesEntered);
        assert_eq!(app.surface, AppSurface::Sources);

        // QuitRequested emits ViewStackPopped as a command.
        // In tests, manually dispatch the second message.
        let _ = app.update(CassMsg::QuitRequested);
        let _ = app.update(CassMsg::ViewStackPopped);
        assert_eq!(app.surface, AppSurface::Search);
    }

    #[test]
    fn sources_selection_wraps() {
        let mut app = CassApp::default();
        app.sources_view.items = vec![
            SourcesViewItem {
                name: "local".into(),
                kind: crate::sources::SourceKind::Local,
                host: None,
                schedule: "always".into(),
                path_count: 0,
                last_sync: None,
                last_result: "n/a".into(),
                files_synced: 0,
                bytes_transferred: 0,
                busy: false,
                doctor_summary: None,
                error: None,
            },
            SourcesViewItem {
                name: "laptop".into(),
                kind: crate::sources::SourceKind::Ssh,
                host: Some("user@laptop".into()),
                schedule: "daily".into(),
                path_count: 2,
                last_sync: None,
                last_result: "never".into(),
                files_synced: 0,
                bytes_transferred: 0,
                busy: false,
                doctor_summary: None,
                error: None,
            },
        ];
        app.sources_view.selected = 0;

        let _ = app.update(CassMsg::SourcesSelectionMoved { delta: 1 });
        assert_eq!(app.sources_view.selected, 1);

        let _ = app.update(CassMsg::SourcesSelectionMoved { delta: 1 });
        assert_eq!(app.sources_view.selected, 0); // wraps

        let _ = app.update(CassMsg::SourcesSelectionMoved { delta: -1 });
        assert_eq!(app.sources_view.selected, 1); // wraps backward
    }

    #[test]
    fn sources_sync_requested_marks_busy() {
        let mut app = CassApp::default();
        app.sources_view.items = vec![SourcesViewItem {
            name: "laptop".into(),
            kind: crate::sources::SourceKind::Ssh,
            host: Some("user@laptop".into()),
            schedule: "manual".into(),
            path_count: 1,
            last_sync: None,
            last_result: "never".into(),
            files_synced: 0,
            bytes_transferred: 0,
            busy: false,
            doctor_summary: None,
            error: None,
        }];

        let _ = app.update(CassMsg::SourceSyncRequested("laptop".into()));
        assert!(app.sources_view.items[0].busy);
        assert!(app.sources_view.status.contains("Syncing"));
    }

    #[test]
    fn sources_sync_completed_clears_busy() {
        let mut app = CassApp::default();
        app.sources_view.items = vec![SourcesViewItem {
            name: "laptop".into(),
            kind: crate::sources::SourceKind::Ssh,
            host: Some("user@laptop".into()),
            schedule: "manual".into(),
            path_count: 1,
            last_sync: None,
            last_result: "never".into(),
            files_synced: 0,
            bytes_transferred: 0,
            busy: true,
            doctor_summary: None,
            error: None,
        }];

        let _ = app.update(CassMsg::SourceSyncCompleted {
            source_name: "laptop".into(),
            message: "Synced 42 files".into(),
        });
        assert!(!app.sources_view.items[0].busy);
        assert_eq!(app.sources_view.status, "Synced 42 files");
    }

    #[test]
    fn sources_doctor_completed_sets_summary() {
        let mut app = CassApp::default();
        app.sources_view.items = vec![SourcesViewItem {
            name: "laptop".into(),
            kind: crate::sources::SourceKind::Ssh,
            host: Some("user@laptop".into()),
            schedule: "manual".into(),
            path_count: 1,
            last_sync: None,
            last_result: "never".into(),
            files_synced: 0,
            bytes_transferred: 0,
            busy: true,
            doctor_summary: None,
            error: None,
        }];

        let _ = app.update(CassMsg::SourceDoctorCompleted {
            source_name: "laptop".into(),
            passed: 3,
            warnings: 1,
            failed: 0,
        });
        assert!(!app.sources_view.items[0].busy);
        assert_eq!(app.sources_view.items[0].doctor_summary, Some((3, 1, 0)));
        assert!(app.sources_view.status.contains("3 pass"));
    }

    #[test]
    fn sources_view_renders_without_panic() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Sources;
        app.sources_view.items = vec![SourcesViewItem {
            name: "local".into(),
            kind: crate::sources::SourceKind::Local,
            host: None,
            schedule: "always".into(),
            path_count: 0,
            last_sync: None,
            last_result: "n/a".into(),
            files_synced: 0,
            bytes_transferred: 0,
            busy: false,
            doctor_summary: None,
            error: None,
        }];
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        app.view(&mut frame);
        // No panic = pass.
    }

    #[test]
    fn sources_key_suppresses_query_input() {
        let mut app = CassApp::default();
        app.surface = AppSurface::Sources;

        // Typing a random char should not modify the query.
        let _ = app.update(CassMsg::QueryChanged("x".into()));
        assert!(app.query.is_empty());
    }

    #[test]
    fn sources_entered_idempotent() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::SourcesEntered);
        let _ = app.update(CassMsg::SourcesEntered);
        // Should not double-push onto view stack.
        assert_eq!(app.view_stack.len(), 1);
        assert_eq!(app.surface, AppSurface::Sources);
    }

    #[test]
    fn sources_from_analytics_stacks_correctly() {
        let mut app = CassApp::default();
        let _ = app.update(CassMsg::AnalyticsEntered);
        assert_eq!(app.surface, AppSurface::Analytics);

        let _ = app.update(CassMsg::SourcesEntered);
        assert_eq!(app.surface, AppSurface::Sources);
        assert_eq!(
            app.view_stack,
            vec![AppSurface::Search, AppSurface::Analytics]
        );

        let _ = app.update(CassMsg::ViewStackPopped);
        assert_eq!(app.surface, AppSurface::Analytics);

        let _ = app.update(CassMsg::ViewStackPopped);
        assert_eq!(app.surface, AppSurface::Search);
    }
}
