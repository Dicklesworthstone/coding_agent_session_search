use coding_agent_search::ui::tui::footer_legend;

#[test]
fn help_legend_has_hotkeys() {
    // Simplified footer shows essential keys only
    let short = footer_legend(false);
    assert!(
        short.contains("F1 help"),
        "short footer should show F1 help"
    );
    assert!(
        short.contains("Enter view"),
        "short footer should show Enter view"
    );

    // Expanded footer (when help shown) has more detail
    let long = footer_legend(true);
    assert!(
        long.contains("Esc quit"),
        "long footer should show Esc quit"
    );
    assert!(
        long.contains("Tab focus"),
        "long footer should show Tab focus"
    );
}
