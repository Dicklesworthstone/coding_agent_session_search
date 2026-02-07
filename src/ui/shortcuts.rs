//! Keyboard shortcut constants for consistent documentation.

pub const HELP: &str = "F1";
pub const THEME: &str = "F2";
pub const FILTER_AGENT: &str = "F3";
pub const FILTER_WORKSPACE: &str = "F4";
pub const FILTER_DATE_FROM: &str = "F5";
pub const FILTER_DATE_TO: &str = "F6";
pub const CONTEXT_WINDOW: &str = "F7";
pub const EDITOR: &str = "F8";
pub const MATCH_MODE: &str = "F9";
pub const SEARCH_MODE: &str = "Alt+S";
pub const QUIT: &str = "Esc/F10";
pub const CLEAR_FILTERS: &str = "Ctrl+Del";
pub const RESET_STATE: &str = "Ctrl+Shift+Del";
pub const RANKING: &str = "F12";
pub const REFRESH: &str = "Ctrl+Shift+R";
pub const DETAIL_OPEN: &str = "Enter";
pub const DETAIL_CLOSE: &str = "Esc";
pub const FOCUS_QUERY: &str = "/";
pub const HISTORY_NEXT: &str = "Ctrl+n";
pub const HISTORY_PREV: &str = "Ctrl+p";
pub const HISTORY_CYCLE: &str = "Ctrl+R";

// Filter scopes
pub const SCOPE_AGENT: &str = "Shift+F3";
pub const SCOPE_WORKSPACE: &str = "Shift+F4";
pub const CYCLE_TIME_PRESETS: &str = "Shift+F5";

// Command palette
pub const PALETTE: &str = "Ctrl+P";
pub const DENSITY: &str = "Ctrl+D";
pub const BORDERS: &str = "Ctrl+B";

// Actions
pub const COPY: &str = "y";
pub const COPY_PATH: &str = "Ctrl+Y";
pub const COPY_CONTENT: &str = "Ctrl+Shift+C";
pub const BULK_MENU: &str = "A";
pub const JSON_VIEW: &str = "J";
pub const TOGGLE_SELECT: &str = "Ctrl+X";
pub const PANE_FILTER: &str = "/";

// Theme editor
pub const THEME_EDITOR: &str = "Ctrl+Shift+T";

// Sources management
pub const SOURCES: &str = "Ctrl+Shift+S";

// Inspector
pub const INSPECTOR: &str = "Ctrl+Shift+I";

// Macro recording
pub const MACRO_TOGGLE: &str = "Alt+M";

// Navigation
pub const TAB_FOCUS: &str = "Tab";
pub const VIM_NAV: &str = "Alt+h/j/k/l";
pub const JUMP_TOP: &str = "Home";
pub const JUMP_BOTTOM: &str = "End";

// =============================================================================
// Unit Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // =========================================================================
    // Function Key Constants Tests
    // =========================================================================

    #[test]
    fn function_key_shortcuts_are_not_empty() {
        assert!(!HELP.is_empty());
        assert!(!THEME.is_empty());
        assert!(!FILTER_AGENT.is_empty());
        assert!(!FILTER_WORKSPACE.is_empty());
        assert!(!FILTER_DATE_FROM.is_empty());
        assert!(!FILTER_DATE_TO.is_empty());
        assert!(!CONTEXT_WINDOW.is_empty());
        assert!(!EDITOR.is_empty());
        assert!(!MATCH_MODE.is_empty());
        assert!(!RANKING.is_empty());
    }

    #[test]
    fn function_key_shortcuts_have_expected_values() {
        assert_eq!(HELP, "F1");
        assert_eq!(THEME, "F2");
        assert_eq!(FILTER_AGENT, "F3");
        assert_eq!(FILTER_WORKSPACE, "F4");
        assert_eq!(FILTER_DATE_FROM, "F5");
        assert_eq!(FILTER_DATE_TO, "F6");
        assert_eq!(CONTEXT_WINDOW, "F7");
        assert_eq!(EDITOR, "F8");
        assert_eq!(MATCH_MODE, "F9");
        assert_eq!(RANKING, "F12");
    }

    // =========================================================================
    // Modifier Key Constants Tests
    // =========================================================================

    #[test]
    fn modifier_shortcuts_are_not_empty() {
        assert!(!SEARCH_MODE.is_empty());
        assert!(!CLEAR_FILTERS.is_empty());
        assert!(!RESET_STATE.is_empty());
        assert!(!REFRESH.is_empty());
        assert!(!HISTORY_NEXT.is_empty());
        assert!(!HISTORY_PREV.is_empty());
        assert!(!HISTORY_CYCLE.is_empty());
        assert!(!TOGGLE_SELECT.is_empty());
    }

    #[test]
    fn modifier_shortcuts_have_expected_values() {
        assert_eq!(SEARCH_MODE, "Alt+S");
        assert_eq!(CLEAR_FILTERS, "Ctrl+Del");
        assert_eq!(RESET_STATE, "Ctrl+Shift+Del");
        assert_eq!(REFRESH, "Ctrl+Shift+R");
        assert_eq!(HISTORY_NEXT, "Ctrl+n");
        assert_eq!(HISTORY_PREV, "Ctrl+p");
        assert_eq!(HISTORY_CYCLE, "Ctrl+R");
        assert_eq!(TOGGLE_SELECT, "Ctrl+X");
    }

    // =========================================================================
    // Scope Constants Tests
    // =========================================================================

    #[test]
    fn scope_shortcuts_are_not_empty() {
        assert!(!SCOPE_AGENT.is_empty());
        assert!(!SCOPE_WORKSPACE.is_empty());
        assert!(!CYCLE_TIME_PRESETS.is_empty());
    }

    #[test]
    fn scope_shortcuts_have_expected_values() {
        assert_eq!(SCOPE_AGENT, "Shift+F3");
        assert_eq!(SCOPE_WORKSPACE, "Shift+F4");
        assert_eq!(CYCLE_TIME_PRESETS, "Shift+F5");
    }

    // =========================================================================
    // Action Constants Tests
    // =========================================================================

    #[test]
    fn action_shortcuts_are_not_empty() {
        assert!(!COPY.is_empty());
        assert!(!BULK_MENU.is_empty());
        assert!(!PANE_FILTER.is_empty());
    }

    #[test]
    fn action_shortcuts_have_expected_values() {
        assert_eq!(COPY, "y");
        assert_eq!(BULK_MENU, "A");
        assert_eq!(PANE_FILTER, "/");
    }

    // =========================================================================
    // Navigation Constants Tests
    // =========================================================================

    #[test]
    fn navigation_shortcuts_are_not_empty() {
        assert!(!TAB_FOCUS.is_empty());
        assert!(!VIM_NAV.is_empty());
        assert!(!JUMP_TOP.is_empty());
        assert!(!JUMP_BOTTOM.is_empty());
    }

    #[test]
    fn navigation_shortcuts_have_expected_values() {
        assert_eq!(TAB_FOCUS, "Tab");
        assert_eq!(VIM_NAV, "Alt+h/j/k/l");
        assert_eq!(JUMP_TOP, "Home");
        assert_eq!(JUMP_BOTTOM, "End");
    }

    // =========================================================================
    // Detail View Constants Tests
    // =========================================================================

    #[test]
    fn detail_shortcuts_are_not_empty() {
        assert!(!DETAIL_OPEN.is_empty());
        assert!(!DETAIL_CLOSE.is_empty());
        assert!(!FOCUS_QUERY.is_empty());
    }

    #[test]
    fn detail_shortcuts_have_expected_values() {
        assert_eq!(DETAIL_OPEN, "Enter");
        assert_eq!(DETAIL_CLOSE, "Esc");
        assert_eq!(FOCUS_QUERY, "/");
    }

    // =========================================================================
    // Quit Constants Tests
    // =========================================================================

    #[test]
    fn quit_shortcut_is_not_empty() {
        assert!(!QUIT.is_empty());
    }

    #[test]
    fn quit_shortcut_has_expected_value() {
        assert_eq!(QUIT, "Esc/F10");
    }

    // =========================================================================
    // Uniqueness Tests (Primary shortcuts should not conflict)
    // =========================================================================

    #[test]
    fn primary_function_keys_are_unique() {
        let mut seen = HashSet::new();
        let function_keys = [
            HELP,
            THEME,
            FILTER_AGENT,
            FILTER_WORKSPACE,
            FILTER_DATE_FROM,
            FILTER_DATE_TO,
            CONTEXT_WINDOW,
            EDITOR,
            MATCH_MODE,
            RANKING,
        ];

        for key in &function_keys {
            assert!(seen.insert(*key), "Duplicate function key found: {}", key);
        }
    }

    #[test]
    fn shift_function_keys_are_unique() {
        let mut seen = HashSet::new();
        let shift_keys = [SCOPE_AGENT, SCOPE_WORKSPACE, CYCLE_TIME_PRESETS];

        for key in &shift_keys {
            assert!(
                seen.insert(*key),
                "Duplicate shift+function key found: {}",
                key
            );
        }
    }

    #[test]
    fn ctrl_shortcuts_are_unique() {
        let mut seen = HashSet::new();
        let ctrl_keys = [
            CLEAR_FILTERS,
            RESET_STATE,
            REFRESH,
            HISTORY_NEXT,
            HISTORY_PREV,
            HISTORY_CYCLE,
            TOGGLE_SELECT,
        ];

        for key in &ctrl_keys {
            assert!(seen.insert(*key), "Duplicate ctrl shortcut found: {}", key);
        }
    }

    // =========================================================================
    // Format Validation Tests
    // =========================================================================

    #[test]
    fn function_key_format_is_valid() {
        // Function keys should start with "F" followed by a number
        let function_keys = [
            HELP,
            THEME,
            FILTER_AGENT,
            FILTER_WORKSPACE,
            FILTER_DATE_FROM,
            FILTER_DATE_TO,
            CONTEXT_WINDOW,
            EDITOR,
            MATCH_MODE,
            RANKING,
        ];

        for key in &function_keys {
            assert!(
                key.starts_with('F') && key[1..].chars().all(|c| c.is_ascii_digit()),
                "Invalid function key format: {}",
                key
            );
        }
    }

    #[test]
    fn shift_function_key_format_is_valid() {
        let shift_keys = [SCOPE_AGENT, SCOPE_WORKSPACE, CYCLE_TIME_PRESETS];

        for key in &shift_keys {
            assert!(
                key.starts_with("Shift+F"),
                "Shift key should start with 'Shift+F': {}",
                key
            );
        }
    }

    #[test]
    fn modifier_shortcuts_contain_plus_separator() {
        let modifier_keys = [
            SEARCH_MODE,
            CLEAR_FILTERS,
            RESET_STATE,
            REFRESH,
            HISTORY_NEXT,
            HISTORY_PREV,
            HISTORY_CYCLE,
            TOGGLE_SELECT,
        ];

        for key in &modifier_keys {
            assert!(
                key.contains('+'),
                "Modifier shortcut should contain '+': {}",
                key
            );
        }
    }
}
