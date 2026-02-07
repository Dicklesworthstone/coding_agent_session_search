//! Command palette state and rendering (keyboard-first, fuzzy-ish search).
//! Integration hooks live in `src/ui/app.rs`; this module stays side-effect free.

use crate::ui::shortcuts;

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
    AnalyticsCost,
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
        }
    }

    /// Recompute filtered list using simple case-insensitive substring matching.
    pub fn refilter(&mut self) {
        if self.query.trim().is_empty() {
            self.filtered = self.all_actions.clone();
        } else {
            let q = self.query.to_lowercase();
            self.filtered = self
                .all_actions
                .iter()
                .filter(|a| {
                    a.label.to_lowercase().contains(&q) || a.hint.to_lowercase().contains(&q)
                })
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
        item(PaletteAction::ToggleTheme, "Toggle theme", shortcuts::THEME),
        item(
            PaletteAction::ToggleDensity,
            "Toggle density",
            shortcuts::DENSITY,
        ),
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
        item(
            PaletteAction::OpenSavedViews,
            "Saved views",
            "List saved slots",
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
        PaletteAction::AnalyticsCost,
        "Analytics: Cost",
        "USD cost estimation",
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
}
