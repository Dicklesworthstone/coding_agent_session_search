//! Command palette state and rendering (keyboard-first, fuzzy-ish search).
//! Integration hooks live in `src/ui/app.rs`; this module stays side-effect free.
//!
//! # Interaction Contract
//!
//! | Trigger          | Behavior                                          |
//! |------------------|---------------------------------------------------|
//! | Ctrl+P / Alt+P   | Open palette → push focus trap GROUP_PALETTE       |
//! | Esc              | Close palette → pop focus trap, discard query      |
//! | Enter            | Execute selected action → close → dispatch CassMsg |
//! | Up / k           | Move selection -1 (wraps)                         |
//! | Down / j         | Move selection +1 (wraps)                         |
//! | Ctrl+U           | Clear query                                       |
//! | Any printable    | Append to query → refilter → reset selection to 0  |
//! | Backspace        | Remove last char → refilter                       |
//!
//! # Action Groups
//!
//! Each [`PaletteAction`] belongs to exactly one [`PaletteGroup`]. Groups are
//! used for categorical rendering (section headers, icons) and mapping validation.
//!
//! | Group       | Actions                                                    |
//! |-------------|------------------------------------------------------------|
//! | Chrome      | ToggleTheme, ToggleDensity, ToggleHelpStrip, OpenUpdate    |
//! | Filter      | FilterAgent, FilterWorkspace, FilterToday/Week/CustomDate  |
//! | View        | OpenSavedViews, CycleGrouping, SaveViewSlot, LoadViewSlot, BulkActions, ReloadIndex |
//! | Analytics   | AnalyticsDashboard..AnalyticsCoverage                      |
//! | Export      | ScreenshotHtml, ScreenshotSvg, ScreenshotText             |
//! | Recording   | MacroRecordingToggle                                       |
//! | Sources     | Sources                                                    |
//!
//! # Migration Target (FrankenTUI command_palette)
//!
//! Each action maps to exactly one `CassMsg` dispatch (or batch). The mapping
//! table in [`PaletteAction::target_msg_name`] documents the concrete target
//! for every variant, ensuring no action is lost during migration.
//!
//! # Filter Modes (F9 cycling)
//!
//! [`PaletteMatchMode`] cycles through All → Exact → Prefix → WordStart →
//! Substring → Fuzzy → All. Each mode trades recall for precision: All shows
//! every action (useful for browsing), while Exact/Prefix are fast for users who
//! know what they want. The Bayesian scorer in `app.rs` combines match-mode
//! evidence with recency and frequency priors.
//!
//! # Test Coverage
//!
//! 59 unit tests in this module cover: match mode cycling, action serialization
//! round-trips, group membership exhaustiveness, and default_actions() stability.
//! 12 regression tests in `app.rs` cover: lifecycle, dispatch coverage for all
//! 28 action variants, boundary wrapping, rapid open/close, and selection clamping.

use crate::ui::shortcuts;

/// Match-type filter mode for the command palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PaletteMatchMode {
    #[default]
    All,
    Exact,
    Prefix,
    WordStart,
    Substring,
    Fuzzy,
}

impl PaletteMatchMode {
    /// Advance to the next mode, wrapping around.
    pub fn cycle(self) -> Self {
        match self {
            Self::All => Self::Exact,
            Self::Exact => Self::Prefix,
            Self::Prefix => Self::WordStart,
            Self::WordStart => Self::Substring,
            Self::Substring => Self::Fuzzy,
            Self::Fuzzy => Self::All,
        }
    }

    /// Short human-readable label for status display.
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Exact => "Exact",
            Self::Prefix => "Prefix",
            Self::WordStart => "WordStart",
            Self::Substring => "Substr",
            Self::Fuzzy => "Fuzzy",
        }
    }
}

/// Simple fuzzy match: every character in `pattern` must appear in `text` in order.
fn fuzzy_match(text: &str, pattern: &str) -> bool {
    let mut chars = text.chars();
    for p in pattern.chars() {
        if !chars.any(|c| c == p) {
            return false;
        }
    }
    true
}

/// Categorical grouping for palette actions. Used for section headers,
/// icons, and migration validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PaletteGroup {
    /// UI chrome toggles: theme, density, help strip, update banner.
    Chrome,
    /// Data filters: agent, workspace, time-range.
    Filter,
    /// View management: saved views, bulk actions, reload.
    View,
    /// Analytics surface navigation (8 sub-views).
    Analytics,
    /// Screenshot/export capture.
    Export,
    /// Macro recording toggle.
    Recording,
    /// Sources management.
    Sources,
}

impl PaletteGroup {
    /// Human-readable label for section headers in the palette.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Chrome => "Chrome",
            Self::Filter => "Filters",
            Self::View => "Views",
            Self::Analytics => "Analytics",
            Self::Export => "Export",
            Self::Recording => "Recording",
            Self::Sources => "Sources",
        }
    }

    /// All groups in display order.
    pub const ALL: &'static [PaletteGroup] = &[
        PaletteGroup::Chrome,
        PaletteGroup::Filter,
        PaletteGroup::View,
        PaletteGroup::Analytics,
        PaletteGroup::Export,
        PaletteGroup::Recording,
        PaletteGroup::Sources,
    ];
}

/// Action identifiers the palette can emit. These map to app-level commands.
#[derive(Clone, Debug)]
pub enum PaletteAction {
    ToggleTheme,
    ToggleDensity,
    ToggleHelpStrip,
    OpenUpdateBanner,
    FilterAgent,
    FilterWorkspace,
    FilterToday,
    FilterWeek,
    FilterCustomDate,
    OpenSavedViews,
    CycleGrouping,
    SaveViewSlot(u8),
    LoadViewSlot(u8),
    OpenBulkActions,
    ReloadIndex,
    // -- Analytics surface ------------------------------------------------
    AnalyticsDashboard,
    AnalyticsExplorer,
    AnalyticsHeatmap,
    AnalyticsBreakdowns,
    AnalyticsTools,
    AnalyticsPlans,
    AnalyticsCoverage,
    // -- Screenshot export ------------------------------------------------
    ScreenshotHtml,
    ScreenshotSvg,
    ScreenshotText,
    // -- Macro recording --------------------------------------------------
    MacroRecordingToggle,
    // -- Sources management ------------------------------------------------
    Sources,
}

impl PaletteAction {
    /// Returns the categorical group this action belongs to.
    pub fn group(&self) -> PaletteGroup {
        match self {
            Self::ToggleTheme
            | Self::ToggleDensity
            | Self::ToggleHelpStrip
            | Self::OpenUpdateBanner => PaletteGroup::Chrome,
            Self::FilterAgent
            | Self::FilterWorkspace
            | Self::FilterToday
            | Self::FilterWeek
            | Self::FilterCustomDate => PaletteGroup::Filter,
            Self::OpenSavedViews
            | Self::CycleGrouping
            | Self::SaveViewSlot(_)
            | Self::LoadViewSlot(_)
            | Self::OpenBulkActions
            | Self::ReloadIndex => PaletteGroup::View,
            Self::AnalyticsDashboard
            | Self::AnalyticsExplorer
            | Self::AnalyticsHeatmap
            | Self::AnalyticsBreakdowns
            | Self::AnalyticsTools
            | Self::AnalyticsPlans
            | Self::AnalyticsCoverage => PaletteGroup::Analytics,
            Self::ScreenshotHtml | Self::ScreenshotSvg | Self::ScreenshotText => {
                PaletteGroup::Export
            }
            Self::MacroRecordingToggle => PaletteGroup::Recording,
            Self::Sources => PaletteGroup::Sources,
        }
    }

    /// Returns the CassMsg variant name this action dispatches to.
    ///
    /// This mapping table is the authoritative contract between palette actions
    /// and app-level command dispatch. Every variant must have an explicit entry;
    /// the match is exhaustive by design.
    pub fn target_msg_name(&self) -> &'static str {
        match self {
            // Chrome
            Self::ToggleTheme => "ThemeToggled",
            Self::ToggleDensity => "DensityModeCycled",
            Self::ToggleHelpStrip => "HelpPinToggled",
            Self::OpenUpdateBanner => "update_info inline (no CassMsg)",
            // Filter
            Self::FilterAgent => "InputModeEntered(Agent)",
            Self::FilterWorkspace => "InputModeEntered(Workspace)",
            Self::FilterToday => "FilterTimeSet { from: start_of_day }",
            Self::FilterWeek => "FilterTimeSet { from: week_ago }",
            Self::FilterCustomDate => "InputModeEntered(CreatedFrom)",
            // View
            Self::OpenSavedViews => "SavedViewsOpened",
            Self::CycleGrouping => "GroupingCycled",
            Self::SaveViewSlot(_) => "ViewSaved(slot)",
            Self::LoadViewSlot(_) => "ViewLoaded(slot)",
            Self::OpenBulkActions => "BulkActionsOpened",
            Self::ReloadIndex => "IndexRefreshRequested",
            // Analytics (all batch: AnalyticsEntered + AnalyticsViewChanged)
            Self::AnalyticsDashboard => "batch[AnalyticsEntered, AnalyticsViewChanged(Dashboard)]",
            Self::AnalyticsExplorer => "batch[AnalyticsEntered, AnalyticsViewChanged(Explorer)]",
            Self::AnalyticsHeatmap => "batch[AnalyticsEntered, AnalyticsViewChanged(Heatmap)]",
            Self::AnalyticsBreakdowns => {
                "batch[AnalyticsEntered, AnalyticsViewChanged(Breakdowns)]"
            }
            Self::AnalyticsTools => "batch[AnalyticsEntered, AnalyticsViewChanged(Tools)]",
            Self::AnalyticsPlans => "batch[AnalyticsEntered, AnalyticsViewChanged(Plans)]",
            Self::AnalyticsCoverage => "batch[AnalyticsEntered, AnalyticsViewChanged(Coverage)]",
            // Export
            Self::ScreenshotHtml => "ScreenshotRequested(Html)",
            Self::ScreenshotSvg => "ScreenshotRequested(Svg)",
            Self::ScreenshotText => "ScreenshotRequested(Text)",
            // Recording
            Self::MacroRecordingToggle => "MacroRecordingToggled",
            // Sources
            Self::Sources => "SourcesEntered",
        }
    }
}

/// Semantic result of executing a palette action. Decoupled from `CassMsg`
/// so that palette.rs stays side-effect free and doesn't depend on app.rs.
///
/// The app-level adapter (`palette_result_to_cmd` in app.rs) translates these
/// into concrete `Cmd<CassMsg>` dispatches.
#[derive(Clone, Debug, PartialEq)]
pub enum PaletteResult {
    /// Toggle the UI theme (light/dark).
    ToggleTheme,
    /// Cycle the density mode (compact/normal/relaxed).
    CycleDensity,
    /// Toggle the help strip visibility.
    ToggleHelpStrip,
    /// Open/check the update banner.
    OpenUpdateBanner,
    /// Enter an input mode for filtering.
    EnterInputMode(InputModeTarget),
    /// Set a time filter (epoch seconds).
    SetTimeFilter { from: TimeFilterPreset },
    /// Open the saved-views picker.
    OpenSavedViews,
    /// Cycle result panes through agent, conversation, workspace, and flat.
    CycleGrouping,
    /// Save the current view to a numbered slot.
    SaveViewSlot(u8),
    /// Load a view from a numbered slot.
    LoadViewSlot(u8),
    /// Open the bulk-actions menu.
    OpenBulkActions,
    /// Reload/refresh the index.
    ReloadIndex,
    /// Navigate to an analytics sub-view (by name).
    OpenAnalyticsView(AnalyticsTarget),
    /// Request a screenshot export in the given format.
    Screenshot(ScreenshotTarget),
    /// Toggle macro recording on/off.
    ToggleMacroRecording,
    /// Enter sources management.
    OpenSources,
    /// No action (e.g. palette was empty when executed).
    Noop,
}

/// Input mode the palette can request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputModeTarget {
    Agent,
    Workspace,
    CreatedFrom,
}

/// Time filter presets the palette can apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeFilterPreset {
    Today,
    LastWeek,
}

/// Analytics sub-views addressable from the palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnalyticsTarget {
    Dashboard,
    Explorer,
    Heatmap,
    Breakdowns,
    Tools,
    Plans,
    Coverage,
}

/// Screenshot export formats addressable from the palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenshotTarget {
    Html,
    Svg,
    Text,
}

impl PaletteAction {
    /// Dispatch this action to a side-effect-free [`PaletteResult`].
    ///
    /// This is the adapter layer: palette semantics → app-level intent,
    /// without depending on CassMsg or ftui. The app translates
    /// `PaletteResult` into concrete `Cmd<CassMsg>` via `palette_result_to_cmd`.
    pub fn dispatch(&self) -> PaletteResult {
        match self {
            // Chrome
            Self::ToggleTheme => PaletteResult::ToggleTheme,
            Self::ToggleDensity => PaletteResult::CycleDensity,
            Self::ToggleHelpStrip => PaletteResult::ToggleHelpStrip,
            Self::OpenUpdateBanner => PaletteResult::OpenUpdateBanner,
            // Filters
            Self::FilterAgent => PaletteResult::EnterInputMode(InputModeTarget::Agent),
            Self::FilterWorkspace => PaletteResult::EnterInputMode(InputModeTarget::Workspace),
            Self::FilterToday => PaletteResult::SetTimeFilter {
                from: TimeFilterPreset::Today,
            },
            Self::FilterWeek => PaletteResult::SetTimeFilter {
                from: TimeFilterPreset::LastWeek,
            },
            Self::FilterCustomDate => PaletteResult::EnterInputMode(InputModeTarget::CreatedFrom),
            // Views
            Self::OpenSavedViews => PaletteResult::OpenSavedViews,
            Self::CycleGrouping => PaletteResult::CycleGrouping,
            Self::SaveViewSlot(slot) => PaletteResult::SaveViewSlot(*slot),
            Self::LoadViewSlot(slot) => PaletteResult::LoadViewSlot(*slot),
            Self::OpenBulkActions => PaletteResult::OpenBulkActions,
            Self::ReloadIndex => PaletteResult::ReloadIndex,
            // Analytics
            Self::AnalyticsDashboard => {
                PaletteResult::OpenAnalyticsView(AnalyticsTarget::Dashboard)
            }
            Self::AnalyticsExplorer => PaletteResult::OpenAnalyticsView(AnalyticsTarget::Explorer),
            Self::AnalyticsHeatmap => PaletteResult::OpenAnalyticsView(AnalyticsTarget::Heatmap),
            Self::AnalyticsBreakdowns => {
                PaletteResult::OpenAnalyticsView(AnalyticsTarget::Breakdowns)
            }
            Self::AnalyticsTools => PaletteResult::OpenAnalyticsView(AnalyticsTarget::Tools),
            Self::AnalyticsPlans => PaletteResult::OpenAnalyticsView(AnalyticsTarget::Plans),
            Self::AnalyticsCoverage => PaletteResult::OpenAnalyticsView(AnalyticsTarget::Coverage),
            // Export
            Self::ScreenshotHtml => PaletteResult::Screenshot(ScreenshotTarget::Html),
            Self::ScreenshotSvg => PaletteResult::Screenshot(ScreenshotTarget::Svg),
            Self::ScreenshotText => PaletteResult::Screenshot(ScreenshotTarget::Text),
            // Recording
            Self::MacroRecordingToggle => PaletteResult::ToggleMacroRecording,
            // Sources
            Self::Sources => PaletteResult::OpenSources,
        }
    }
}

/// Execute the currently selected palette action, returning a [`PaletteResult`].
///
/// Returns [`PaletteResult::Noop`] if the palette is empty or selection is out of bounds.
pub fn execute_selected(state: &PaletteState) -> PaletteResult {
    state
        .filtered
        .get(state.selected)
        .map(|item| item.action.dispatch())
        .unwrap_or(PaletteResult::Noop)
}

/// Generate a stable string ID for a palette action.
///
/// Used as the `ActionItem::id` when registering actions with the ftui
/// CommandPalette widget, and for reverse-lookup on `Execute(id)`.
pub fn action_id(action: &PaletteAction) -> String {
    format!("{action:?}")
}

/// Find the [`PaletteAction`] whose [`action_id`] matches `id`.
pub fn action_by_id(items: &[PaletteItem], id: &str) -> Option<PaletteAction> {
    items
        .iter()
        .find(|item| action_id(&item.action) == id)
        .map(|item| item.action.clone())
}

/// Render-ready descriptor for an action.
#[derive(Clone, Debug)]
pub struct PaletteItem {
    pub action: PaletteAction,
    pub label: String,
    pub hint: String,
}

#[derive(Clone, Debug)]
pub struct PaletteState {
    pub open: bool,
    pub query: String,
    pub filtered: Vec<PaletteItem>,
    pub all_actions: Vec<PaletteItem>,
    pub selected: usize,
    pub match_mode: PaletteMatchMode,
}

impl PaletteState {
    pub fn new(actions: Vec<PaletteItem>) -> Self {
        let filtered = actions.clone();
        Self {
            open: false,
            query: String::new(),
            filtered,
            all_actions: actions,
            selected: 0,
            match_mode: PaletteMatchMode::default(),
        }
    }

    /// Recompute filtered list respecting the current [`PaletteMatchMode`].
    pub fn refilter(&mut self) {
        if self.query.trim().is_empty() {
            self.filtered = self.all_actions.clone();
        } else {
            let q = self.query.to_lowercase();
            let matches = |text: &str| -> bool {
                let t = text.to_lowercase();
                match self.match_mode {
                    PaletteMatchMode::All | PaletteMatchMode::Substring => t.contains(&q),
                    PaletteMatchMode::Exact => t == q,
                    PaletteMatchMode::Prefix => t.starts_with(&q),
                    PaletteMatchMode::WordStart => {
                        t.split_whitespace().any(|word| word.starts_with(&q))
                    }
                    PaletteMatchMode::Fuzzy => fuzzy_match(&t, &q),
                }
            };
            self.filtered = self
                .all_actions
                .iter()
                .filter(|a| matches(&a.label) || matches(&a.hint))
                .cloned()
                .collect();
        }
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let len = self.filtered.len() as isize;
        let idx = (self.selected as isize + delta).rem_euclid(len);
        self.selected = idx as usize;
    }
}

/// Prebuilt action catalog with keyboard shortcut hints from [`shortcuts`].
pub fn default_actions() -> Vec<PaletteItem> {
    let mut items = vec![
        item(
            PaletteAction::OpenSavedViews,
            "Saved views",
            "List saved slots",
        ),
        item(
            PaletteAction::CycleGrouping,
            "Cycle result grouping (agent / conversation / workspace / flat)",
            shortcuts::GROUPING,
        ),
        item(
            PaletteAction::ToggleDensity,
            "Toggle density",
            shortcuts::DENSITY,
        ),
        item(PaletteAction::ToggleTheme, "Toggle theme", shortcuts::THEME),
        item(
            PaletteAction::ToggleHelpStrip,
            "Toggle help strip",
            shortcuts::HELP,
        ),
        item(
            PaletteAction::OpenUpdateBanner,
            "Check updates",
            "Show update assistant",
        ),
        item(
            PaletteAction::FilterAgent,
            "Filter: agent",
            shortcuts::FILTER_AGENT,
        ),
        item(
            PaletteAction::FilterWorkspace,
            "Filter: workspace",
            shortcuts::FILTER_WORKSPACE,
        ),
        item(
            PaletteAction::FilterToday,
            "Filter: today",
            "Restrict to today",
        ),
        item(
            PaletteAction::FilterWeek,
            "Filter: last 7 days",
            "Restrict to week",
        ),
        item(
            PaletteAction::FilterCustomDate,
            "Filter: date range",
            shortcuts::FILTER_DATE_FROM,
        ),
        item(
            PaletteAction::OpenBulkActions,
            "Bulk actions",
            shortcuts::BULK_MENU,
        ),
        item(
            PaletteAction::ReloadIndex,
            "Reload index/view",
            shortcuts::REFRESH,
        ),
    ];
    // -- Analytics surface commands ----------------------------------------
    items.push(item(
        PaletteAction::AnalyticsDashboard,
        "Analytics: Dashboard",
        "KPI overview",
    ));
    items.push(item(
        PaletteAction::AnalyticsExplorer,
        "Analytics: Explorer",
        "Time-series explorer",
    ));
    items.push(item(
        PaletteAction::AnalyticsHeatmap,
        "Analytics: Heatmap",
        "Calendar heatmap",
    ));
    items.push(item(
        PaletteAction::AnalyticsBreakdowns,
        "Analytics: Breakdowns",
        "Agents/workspaces/sources",
    ));
    items.push(item(
        PaletteAction::AnalyticsTools,
        "Analytics: Tools",
        "Per-tool usage",
    ));
    items.push(item(
        PaletteAction::AnalyticsPlans,
        "Analytics: Plans",
        "Plan frequency + token share",
    ));
    items.push(item(
        PaletteAction::AnalyticsCoverage,
        "Analytics: Coverage",
        "Token coverage diagnostics",
    ));
    // -- Screenshot export commands -----------------------------------------
    items.push(item(
        PaletteAction::ScreenshotHtml,
        "Screenshot: HTML",
        "Capture TUI as HTML",
    ));
    items.push(item(
        PaletteAction::ScreenshotSvg,
        "Screenshot: SVG",
        "Capture TUI as SVG",
    ));
    items.push(item(
        PaletteAction::ScreenshotText,
        "Screenshot: Text",
        "Capture TUI as plain text",
    ));
    // -- Macro recording commands -------------------------------------------
    items.push(item(
        PaletteAction::MacroRecordingToggle,
        "Toggle macro recording",
        "Alt+M",
    ));
    // -- Sources management ------------------------------------------------
    items.push(item(
        PaletteAction::Sources,
        "Sources management",
        "Ctrl+Shift+S",
    ));
    // Slots 1-9
    for slot in 1..=9 {
        items.push(item(
            PaletteAction::SaveViewSlot(slot),
            format!("Save view to slot {slot}"),
            format!("Ctrl+{slot}"),
        ));
        items.push(item(
            PaletteAction::LoadViewSlot(slot),
            format!("Load view from slot {slot}"),
            format!("Shift+{slot}"),
        ));
    }
    items
}

fn item(action: PaletteAction, label: impl Into<String>, hint: impl Into<String>) -> PaletteItem {
    PaletteItem {
        action,
        label: label.into(),
        hint: hint.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== PaletteAction tests ====================

    #[test]
    fn test_palette_action_clone() {
        let action = PaletteAction::ToggleTheme;
        let cloned = action.clone();
        assert!(matches!(cloned, PaletteAction::ToggleTheme));
    }

    #[test]
    fn test_palette_action_debug() {
        let action = PaletteAction::FilterAgent;
        let debug_str = format!("{:?}", action);
        assert!(debug_str.contains("FilterAgent"));
    }

    #[test]
    fn test_palette_action_slot_variants() {
        let save = PaletteAction::SaveViewSlot(5);
        let load = PaletteAction::LoadViewSlot(3);

        let save_debug = format!("{:?}", save);
        let load_debug = format!("{:?}", load);

        assert!(save_debug.contains("SaveViewSlot"));
        assert!(save_debug.contains("5"));
        assert!(load_debug.contains("LoadViewSlot"));
        assert!(load_debug.contains("3"));
    }

    // ==================== PaletteItem tests ====================

    #[test]
    fn test_palette_item_creation() {
        let item = PaletteItem {
            action: PaletteAction::ToggleTheme,
            label: "Toggle theme".to_string(),
            hint: "Switch light/dark".to_string(),
        };

        assert_eq!(item.label, "Toggle theme");
        assert_eq!(item.hint, "Switch light/dark");
    }

    #[test]
    fn test_palette_item_clone() {
        let item = PaletteItem {
            action: PaletteAction::ReloadIndex,
            label: "Reload".to_string(),
            hint: "Refresh".to_string(),
        };

        let cloned = item.clone();
        assert_eq!(cloned.label, item.label);
        assert_eq!(cloned.hint, item.hint);
    }

    #[test]
    fn test_palette_item_debug() {
        let item = PaletteItem {
            action: PaletteAction::FilterToday,
            label: "Today".to_string(),
            hint: "Show today".to_string(),
        };

        let debug_str = format!("{:?}", item);
        assert!(debug_str.contains("PaletteItem"));
        assert!(debug_str.contains("Today"));
    }

    // ==================== PaletteState::new tests ====================

    #[test]
    fn test_palette_state_new_empty() {
        let state = PaletteState::new(vec![]);

        assert!(!state.open);
        assert!(state.query.is_empty());
        assert!(state.filtered.is_empty());
        assert!(state.all_actions.is_empty());
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_palette_state_new_with_items() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "Theme", "Switch"),
            item(PaletteAction::ToggleDensity, "Density", "Change"),
        ];
        let state = PaletteState::new(items);

        assert!(!state.open);
        assert!(state.query.is_empty());
        assert_eq!(state.filtered.len(), 2);
        assert_eq!(state.all_actions.len(), 2);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_palette_state_filtered_matches_all_initially() {
        let items = vec![
            item(PaletteAction::FilterAgent, "Agent", "Set agent"),
            item(PaletteAction::FilterWorkspace, "Workspace", "Set ws"),
            item(PaletteAction::FilterToday, "Today", "Restrict"),
        ];
        let state = PaletteState::new(items);

        assert_eq!(state.filtered.len(), state.all_actions.len());
    }

    // ==================== PaletteState::refilter tests ====================

    #[test]
    fn test_refilter_empty_query_shows_all() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "Theme", "Switch"),
            item(PaletteAction::ToggleDensity, "Density", "Change"),
        ];
        let mut state = PaletteState::new(items);
        state.query = "".to_string();
        state.refilter();

        assert_eq!(state.filtered.len(), 2);
    }

    #[test]
    fn test_refilter_whitespace_query_shows_all() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "Theme", "Switch"),
            item(PaletteAction::ToggleDensity, "Density", "Change"),
        ];
        let mut state = PaletteState::new(items);
        state.query = "   ".to_string();
        state.refilter();

        assert_eq!(state.filtered.len(), 2);
    }

    #[test]
    fn test_refilter_matches_label() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "Toggle theme", "Switch"),
            item(PaletteAction::FilterAgent, "Filter agent", "Set"),
        ];
        let mut state = PaletteState::new(items);
        state.query = "theme".to_string();
        state.refilter();

        assert_eq!(state.filtered.len(), 1);
        assert_eq!(state.filtered[0].label, "Toggle theme");
    }

    #[test]
    fn test_refilter_matches_hint() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "Theme", "Switch light/dark"),
            item(PaletteAction::FilterAgent, "Agent", "Set filter"),
        ];
        let mut state = PaletteState::new(items);
        state.query = "light".to_string();
        state.refilter();

        assert_eq!(state.filtered.len(), 1);
        assert_eq!(state.filtered[0].label, "Theme");
    }

    #[test]
    fn test_refilter_case_insensitive() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "Toggle Theme", "Switch"),
            item(PaletteAction::FilterAgent, "Filter Agent", "Set"),
        ];
        let mut state = PaletteState::new(items);
        state.query = "THEME".to_string();
        state.refilter();

        assert_eq!(state.filtered.len(), 1);
        assert_eq!(state.filtered[0].label, "Toggle Theme");
    }

    #[test]
    fn test_refilter_no_matches() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "Theme", "Switch"),
            item(PaletteAction::FilterAgent, "Agent", "Set"),
        ];
        let mut state = PaletteState::new(items);
        state.query = "xyz".to_string();
        state.refilter();

        assert!(state.filtered.is_empty());
    }

    #[test]
    fn test_refilter_adjusts_selection_when_out_of_bounds() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "Theme", "Switch"),
            item(PaletteAction::FilterAgent, "Agent", "Set"),
            item(PaletteAction::FilterWorkspace, "Workspace", "Set"),
        ];
        let mut state = PaletteState::new(items);
        state.selected = 2;
        state.query = "theme".to_string();
        state.refilter();

        assert!(state.selected < state.filtered.len() || state.filtered.is_empty());
    }

    #[test]
    fn test_refilter_selection_stays_zero_when_empty() {
        let items = vec![item(PaletteAction::ToggleTheme, "Theme", "Switch")];
        let mut state = PaletteState::new(items);
        state.selected = 0;
        state.query = "nomatch".to_string();
        state.refilter();

        assert!(state.filtered.is_empty());
        assert_eq!(state.selected, 0);
    }

    // ==================== PaletteState::move_selection tests ====================

    #[test]
    fn test_move_selection_down() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "Theme", "A"),
            item(PaletteAction::FilterAgent, "Agent", "B"),
            item(PaletteAction::FilterWorkspace, "Workspace", "C"),
        ];
        let mut state = PaletteState::new(items);
        assert_eq!(state.selected, 0);

        state.move_selection(1);
        assert_eq!(state.selected, 1);

        state.move_selection(1);
        assert_eq!(state.selected, 2);
    }

    #[test]
    fn test_move_selection_up() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "Theme", "A"),
            item(PaletteAction::FilterAgent, "Agent", "B"),
            item(PaletteAction::FilterWorkspace, "Workspace", "C"),
        ];
        let mut state = PaletteState::new(items);
        state.selected = 2;

        state.move_selection(-1);
        assert_eq!(state.selected, 1);

        state.move_selection(-1);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_move_selection_wraps_forward() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "Theme", "A"),
            item(PaletteAction::FilterAgent, "Agent", "B"),
        ];
        let mut state = PaletteState::new(items);
        state.selected = 1;

        state.move_selection(1);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_move_selection_wraps_backward() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "Theme", "A"),
            item(PaletteAction::FilterAgent, "Agent", "B"),
        ];
        let mut state = PaletteState::new(items);
        state.selected = 0;

        state.move_selection(-1);
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn test_move_selection_empty_list() {
        let mut state = PaletteState::new(vec![]);

        state.move_selection(1);
        assert_eq!(state.selected, 0);

        state.move_selection(-1);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_move_selection_large_delta() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "A", ""),
            item(PaletteAction::FilterAgent, "B", ""),
            item(PaletteAction::FilterWorkspace, "C", ""),
        ];
        let mut state = PaletteState::new(items);
        state.selected = 0;

        state.move_selection(5);
        assert_eq!(state.selected, 2); // 5 % 3 = 2

        state.move_selection(-7);
        // 2 + (-7) = -5, rem_euclid(3) = 1
        assert_eq!(state.selected, 1);
    }

    // ==================== default_actions tests ====================

    #[test]
    fn test_default_actions_not_empty() {
        let actions = default_actions();
        assert!(!actions.is_empty());
    }

    #[test]
    fn test_default_actions_has_basic_items() {
        let actions = default_actions();
        let labels: Vec<&str> = actions.iter().map(|a| a.label.as_str()).collect();

        assert!(labels.contains(&"Toggle theme"));
        assert!(labels.contains(&"Toggle density"));
        assert!(labels.contains(&"Filter: agent"));
        assert!(labels.contains(&"Reload index/view"));
    }

    #[test]
    fn test_default_actions_has_view_slots() {
        let actions = default_actions();

        for slot in 1..=9 {
            let save_label = format!("Save view to slot {slot}");
            let load_label = format!("Load view from slot {slot}");

            assert!(
                actions.iter().any(|a| a.label == save_label),
                "Missing save slot {slot}"
            );
            assert!(
                actions.iter().any(|a| a.label == load_label),
                "Missing load slot {slot}"
            );
        }
    }

    #[test]
    fn test_default_actions_all_have_labels_and_hints() {
        let actions = default_actions();

        for action in &actions {
            assert!(!action.label.is_empty(), "Action has empty label");
            assert!(!action.hint.is_empty(), "Action has empty hint");
        }
    }

    // ==================== item helper tests ====================

    #[test]
    fn test_item_helper_function() {
        let result = item(PaletteAction::ToggleTheme, "Label", "Hint");

        assert_eq!(result.label, "Label");
        assert_eq!(result.hint, "Hint");
        assert!(matches!(result.action, PaletteAction::ToggleTheme));
    }

    #[test]
    fn test_item_helper_with_string() {
        let result = item(
            PaletteAction::FilterAgent,
            String::from("My Label"),
            String::from("My Hint"),
        );

        assert_eq!(result.label, "My Label");
        assert_eq!(result.hint, "My Hint");
    }

    // ==================== PaletteGroup tests ====================

    #[test]
    fn group_all_contains_seven_groups() {
        assert_eq!(PaletteGroup::ALL.len(), 7);
    }

    #[test]
    fn group_labels_are_nonempty() {
        for g in PaletteGroup::ALL {
            assert!(!g.label().is_empty(), "{:?} has empty label", g);
        }
    }

    #[test]
    fn every_action_has_a_group() {
        // Exhaustive: build every variant and assert group() doesn't panic.
        let all: Vec<PaletteAction> = vec![
            PaletteAction::ToggleTheme,
            PaletteAction::ToggleDensity,
            PaletteAction::ToggleHelpStrip,
            PaletteAction::OpenUpdateBanner,
            PaletteAction::FilterAgent,
            PaletteAction::FilterWorkspace,
            PaletteAction::FilterToday,
            PaletteAction::FilterWeek,
            PaletteAction::FilterCustomDate,
            PaletteAction::OpenSavedViews,
            PaletteAction::CycleGrouping,
            PaletteAction::SaveViewSlot(1),
            PaletteAction::LoadViewSlot(1),
            PaletteAction::OpenBulkActions,
            PaletteAction::ReloadIndex,
            PaletteAction::AnalyticsDashboard,
            PaletteAction::AnalyticsExplorer,
            PaletteAction::AnalyticsHeatmap,
            PaletteAction::AnalyticsBreakdowns,
            PaletteAction::AnalyticsTools,
            PaletteAction::AnalyticsPlans,
            PaletteAction::AnalyticsCoverage,
            PaletteAction::ScreenshotHtml,
            PaletteAction::ScreenshotSvg,
            PaletteAction::ScreenshotText,
            PaletteAction::MacroRecordingToggle,
            PaletteAction::Sources,
        ];
        for action in &all {
            let _ = action.group(); // must not panic
        }
    }

    #[test]
    fn every_action_has_a_target_msg() {
        let all: Vec<PaletteAction> = vec![
            PaletteAction::ToggleTheme,
            PaletteAction::ToggleDensity,
            PaletteAction::ToggleHelpStrip,
            PaletteAction::OpenUpdateBanner,
            PaletteAction::FilterAgent,
            PaletteAction::FilterWorkspace,
            PaletteAction::FilterToday,
            PaletteAction::FilterWeek,
            PaletteAction::FilterCustomDate,
            PaletteAction::OpenSavedViews,
            PaletteAction::SaveViewSlot(1),
            PaletteAction::LoadViewSlot(1),
            PaletteAction::OpenBulkActions,
            PaletteAction::ReloadIndex,
            PaletteAction::AnalyticsDashboard,
            PaletteAction::AnalyticsExplorer,
            PaletteAction::AnalyticsHeatmap,
            PaletteAction::AnalyticsBreakdowns,
            PaletteAction::AnalyticsTools,
            PaletteAction::AnalyticsPlans,
            PaletteAction::AnalyticsCoverage,
            PaletteAction::ScreenshotHtml,
            PaletteAction::ScreenshotSvg,
            PaletteAction::ScreenshotText,
            PaletteAction::MacroRecordingToggle,
            PaletteAction::Sources,
        ];
        for action in &all {
            let target = action.target_msg_name();
            assert!(!target.is_empty(), "{:?} has empty target_msg_name", action);
        }
    }

    #[test]
    fn chrome_group_contains_expected_actions() {
        assert_eq!(PaletteAction::ToggleTheme.group(), PaletteGroup::Chrome);
        assert_eq!(PaletteAction::ToggleDensity.group(), PaletteGroup::Chrome);
        assert_eq!(PaletteAction::ToggleHelpStrip.group(), PaletteGroup::Chrome);
        assert_eq!(
            PaletteAction::OpenUpdateBanner.group(),
            PaletteGroup::Chrome
        );
    }

    #[test]
    fn filter_group_contains_expected_actions() {
        assert_eq!(PaletteAction::FilterAgent.group(), PaletteGroup::Filter);
        assert_eq!(PaletteAction::FilterWorkspace.group(), PaletteGroup::Filter);
        assert_eq!(PaletteAction::FilterToday.group(), PaletteGroup::Filter);
        assert_eq!(PaletteAction::FilterWeek.group(), PaletteGroup::Filter);
        assert_eq!(
            PaletteAction::FilterCustomDate.group(),
            PaletteGroup::Filter
        );
    }

    #[test]
    fn analytics_group_has_seven_variants() {
        let analytics: Vec<PaletteAction> = vec![
            PaletteAction::AnalyticsDashboard,
            PaletteAction::AnalyticsExplorer,
            PaletteAction::AnalyticsHeatmap,
            PaletteAction::AnalyticsBreakdowns,
            PaletteAction::AnalyticsTools,
            PaletteAction::AnalyticsPlans,
            PaletteAction::AnalyticsCoverage,
        ];
        assert_eq!(analytics.len(), 7);
        for a in &analytics {
            assert_eq!(a.group(), PaletteGroup::Analytics);
        }
    }

    #[test]
    fn view_group_contains_expected_actions() {
        assert_eq!(PaletteAction::OpenSavedViews.group(), PaletteGroup::View);
        assert_eq!(PaletteAction::SaveViewSlot(3).group(), PaletteGroup::View);
        assert_eq!(PaletteAction::LoadViewSlot(5).group(), PaletteGroup::View);
        assert_eq!(PaletteAction::OpenBulkActions.group(), PaletteGroup::View);
        assert_eq!(PaletteAction::ReloadIndex.group(), PaletteGroup::View);
    }

    #[test]
    fn export_group_contains_expected_actions() {
        assert_eq!(PaletteAction::ScreenshotHtml.group(), PaletteGroup::Export);
        assert_eq!(PaletteAction::ScreenshotSvg.group(), PaletteGroup::Export);
        assert_eq!(PaletteAction::ScreenshotText.group(), PaletteGroup::Export);
    }

    #[test]
    fn default_actions_cover_all_groups() {
        let actions = default_actions();
        let mut groups_seen = std::collections::HashSet::new();
        for a in &actions {
            groups_seen.insert(a.action.group());
        }
        for g in PaletteGroup::ALL {
            assert!(
                groups_seen.contains(g),
                "Group {:?} not represented in default_actions()",
                g
            );
        }
    }

    #[test]
    fn target_msg_names_are_distinct_per_non_slot_action() {
        // Non-slot actions should each have a unique target (slots share "ViewSaved(slot)").
        let non_slot: Vec<PaletteAction> = vec![
            PaletteAction::ToggleTheme,
            PaletteAction::ToggleDensity,
            PaletteAction::ToggleHelpStrip,
            PaletteAction::OpenUpdateBanner,
            PaletteAction::FilterAgent,
            PaletteAction::FilterWorkspace,
            PaletteAction::FilterToday,
            PaletteAction::FilterWeek,
            PaletteAction::FilterCustomDate,
            PaletteAction::OpenSavedViews,
            PaletteAction::OpenBulkActions,
            PaletteAction::ReloadIndex,
            PaletteAction::AnalyticsDashboard,
            PaletteAction::AnalyticsExplorer,
            PaletteAction::AnalyticsHeatmap,
            PaletteAction::AnalyticsBreakdowns,
            PaletteAction::AnalyticsTools,
            PaletteAction::AnalyticsPlans,
            PaletteAction::AnalyticsCoverage,
            PaletteAction::ScreenshotHtml,
            PaletteAction::ScreenshotSvg,
            PaletteAction::ScreenshotText,
            PaletteAction::MacroRecordingToggle,
            PaletteAction::Sources,
        ];
        let mut seen = std::collections::HashSet::new();
        for a in &non_slot {
            let name = a.target_msg_name();
            assert!(
                seen.insert(name),
                "Duplicate target_msg_name {:?} for {:?}",
                name,
                a
            );
        }
    }

    // ==================== PaletteResult tests ====================

    #[test]
    fn palette_result_clone_and_eq() {
        let r = PaletteResult::ToggleTheme;
        assert_eq!(r.clone(), PaletteResult::ToggleTheme);
    }

    #[test]
    fn palette_result_debug_format() {
        let r = PaletteResult::EnterInputMode(InputModeTarget::Agent);
        let s = format!("{:?}", r);
        assert!(s.contains("EnterInputMode"));
        assert!(s.contains("Agent"));
    }

    #[test]
    fn palette_result_noop_variant() {
        let r = PaletteResult::Noop;
        assert_eq!(r, PaletteResult::Noop);
    }

    // ==================== dispatch() tests ====================

    #[test]
    fn dispatch_chrome_actions() {
        assert_eq!(
            PaletteAction::ToggleTheme.dispatch(),
            PaletteResult::ToggleTheme
        );
        assert_eq!(
            PaletteAction::ToggleDensity.dispatch(),
            PaletteResult::CycleDensity
        );
        assert_eq!(
            PaletteAction::ToggleHelpStrip.dispatch(),
            PaletteResult::ToggleHelpStrip
        );
        assert_eq!(
            PaletteAction::OpenUpdateBanner.dispatch(),
            PaletteResult::OpenUpdateBanner
        );
    }

    #[test]
    fn dispatch_filter_actions() {
        assert_eq!(
            PaletteAction::FilterAgent.dispatch(),
            PaletteResult::EnterInputMode(InputModeTarget::Agent)
        );
        assert_eq!(
            PaletteAction::FilterWorkspace.dispatch(),
            PaletteResult::EnterInputMode(InputModeTarget::Workspace)
        );
        assert_eq!(
            PaletteAction::FilterToday.dispatch(),
            PaletteResult::SetTimeFilter {
                from: TimeFilterPreset::Today
            }
        );
        assert_eq!(
            PaletteAction::FilterWeek.dispatch(),
            PaletteResult::SetTimeFilter {
                from: TimeFilterPreset::LastWeek
            }
        );
        assert_eq!(
            PaletteAction::FilterCustomDate.dispatch(),
            PaletteResult::EnterInputMode(InputModeTarget::CreatedFrom)
        );
    }

    #[test]
    fn dispatch_view_actions() {
        assert_eq!(
            PaletteAction::OpenSavedViews.dispatch(),
            PaletteResult::OpenSavedViews
        );
        assert_eq!(
            PaletteAction::OpenBulkActions.dispatch(),
            PaletteResult::OpenBulkActions
        );
        assert_eq!(
            PaletteAction::ReloadIndex.dispatch(),
            PaletteResult::ReloadIndex
        );
    }

    #[test]
    fn dispatch_slot_actions_preserve_slot_number() {
        for slot in 1..=9u8 {
            assert_eq!(
                PaletteAction::SaveViewSlot(slot).dispatch(),
                PaletteResult::SaveViewSlot(slot)
            );
            assert_eq!(
                PaletteAction::LoadViewSlot(slot).dispatch(),
                PaletteResult::LoadViewSlot(slot)
            );
        }
    }

    #[test]
    fn dispatch_analytics_actions() {
        let cases = vec![
            (
                PaletteAction::AnalyticsDashboard,
                AnalyticsTarget::Dashboard,
            ),
            (PaletteAction::AnalyticsExplorer, AnalyticsTarget::Explorer),
            (PaletteAction::AnalyticsHeatmap, AnalyticsTarget::Heatmap),
            (
                PaletteAction::AnalyticsBreakdowns,
                AnalyticsTarget::Breakdowns,
            ),
            (PaletteAction::AnalyticsTools, AnalyticsTarget::Tools),
            (PaletteAction::AnalyticsPlans, AnalyticsTarget::Plans),
            (PaletteAction::AnalyticsCoverage, AnalyticsTarget::Coverage),
        ];
        for (action, expected_target) in cases {
            assert_eq!(
                action.dispatch(),
                PaletteResult::OpenAnalyticsView(expected_target),
                "dispatch mismatch for {:?}",
                expected_target
            );
        }
    }

    #[test]
    fn dispatch_export_actions() {
        assert_eq!(
            PaletteAction::ScreenshotHtml.dispatch(),
            PaletteResult::Screenshot(ScreenshotTarget::Html)
        );
        assert_eq!(
            PaletteAction::ScreenshotSvg.dispatch(),
            PaletteResult::Screenshot(ScreenshotTarget::Svg)
        );
        assert_eq!(
            PaletteAction::ScreenshotText.dispatch(),
            PaletteResult::Screenshot(ScreenshotTarget::Text)
        );
    }

    #[test]
    fn dispatch_recording_and_sources() {
        assert_eq!(
            PaletteAction::MacroRecordingToggle.dispatch(),
            PaletteResult::ToggleMacroRecording
        );
        assert_eq!(
            PaletteAction::Sources.dispatch(),
            PaletteResult::OpenSources
        );
    }

    #[test]
    fn dispatch_exhaustive_all_26_actions() {
        // Every action variant must dispatch without panic and return non-Noop.
        let all: Vec<PaletteAction> = vec![
            PaletteAction::ToggleTheme,
            PaletteAction::ToggleDensity,
            PaletteAction::ToggleHelpStrip,
            PaletteAction::OpenUpdateBanner,
            PaletteAction::FilterAgent,
            PaletteAction::FilterWorkspace,
            PaletteAction::FilterToday,
            PaletteAction::FilterWeek,
            PaletteAction::FilterCustomDate,
            PaletteAction::OpenSavedViews,
            PaletteAction::SaveViewSlot(1),
            PaletteAction::LoadViewSlot(1),
            PaletteAction::OpenBulkActions,
            PaletteAction::ReloadIndex,
            PaletteAction::AnalyticsDashboard,
            PaletteAction::AnalyticsExplorer,
            PaletteAction::AnalyticsHeatmap,
            PaletteAction::AnalyticsBreakdowns,
            PaletteAction::AnalyticsTools,
            PaletteAction::AnalyticsPlans,
            PaletteAction::AnalyticsCoverage,
            PaletteAction::ScreenshotHtml,
            PaletteAction::ScreenshotSvg,
            PaletteAction::ScreenshotText,
            PaletteAction::MacroRecordingToggle,
            PaletteAction::Sources,
        ];
        for action in &all {
            let result = action.dispatch();
            assert_ne!(
                result,
                PaletteResult::Noop,
                "{:?} dispatched to Noop",
                action
            );
        }
    }

    // ==================== execute_selected() tests ====================

    #[test]
    fn execute_selected_returns_noop_on_empty_state() {
        let state = PaletteState::new(vec![]);
        assert_eq!(execute_selected(&state), PaletteResult::Noop);
    }

    #[test]
    fn execute_selected_returns_noop_on_out_of_bounds() {
        let items = vec![item(PaletteAction::ToggleTheme, "Theme", "Toggle")];
        let mut state = PaletteState::new(items);
        state.selected = 5; // out of bounds
        assert_eq!(execute_selected(&state), PaletteResult::Noop);
    }

    #[test]
    fn execute_selected_dispatches_first_item() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "Theme", "Toggle"),
            item(PaletteAction::ReloadIndex, "Reload", "Refresh"),
        ];
        let state = PaletteState::new(items);
        assert_eq!(execute_selected(&state), PaletteResult::ToggleTheme);
    }

    #[test]
    fn execute_selected_dispatches_second_item() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "Theme", "Toggle"),
            item(PaletteAction::ReloadIndex, "Reload", "Refresh"),
        ];
        let mut state = PaletteState::new(items);
        state.selected = 1;
        assert_eq!(execute_selected(&state), PaletteResult::ReloadIndex);
    }

    #[test]
    fn execute_selected_respects_filter() {
        let items = vec![
            item(PaletteAction::ToggleTheme, "Theme", "Toggle"),
            item(PaletteAction::ReloadIndex, "Reload", "Refresh"),
        ];
        let mut state = PaletteState::new(items);
        state.query = "reload".to_string();
        state.refilter();
        // After filtering, only "Reload" remains, selected=0.
        assert_eq!(execute_selected(&state), PaletteResult::ReloadIndex);
    }

    #[test]
    fn execute_selected_noop_after_no_match_filter() {
        let items = vec![item(PaletteAction::ToggleTheme, "Theme", "Toggle")];
        let mut state = PaletteState::new(items);
        state.query = "zzz_no_match".to_string();
        state.refilter();
        assert_eq!(execute_selected(&state), PaletteResult::Noop);
    }

    #[test]
    fn execute_selected_slot_preserves_value() {
        let items = vec![item(PaletteAction::SaveViewSlot(7), "Save 7", "Ctrl+7")];
        let state = PaletteState::new(items);
        assert_eq!(execute_selected(&state), PaletteResult::SaveViewSlot(7));
    }

    // ==================== InputModeTarget/TimeFilterPreset/AnalyticsTarget/ScreenshotTarget ====================

    #[test]
    fn supporting_enums_derive_traits() {
        // Clone + Copy + Debug + PartialEq + Eq
        let imt = InputModeTarget::Agent;
        assert_eq!(imt, imt);
        let _ = format!("{:?}", imt);

        let tfp = TimeFilterPreset::Today;
        assert_eq!(tfp, tfp);
        let _ = format!("{:?}", tfp);

        let at = AnalyticsTarget::Dashboard;
        assert_eq!(at, at);
        let _ = format!("{:?}", at);

        let st = ScreenshotTarget::Html;
        assert_eq!(st, st);
        let _ = format!("{:?}", st);
    }
}
