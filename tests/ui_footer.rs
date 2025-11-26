use coding_agent_search::ui::tui::footer_legend;

#[test]
fn footer_legend_toggles_help() {
    let hidden = footer_legend(false);
    assert!(
        hidden.contains("F1 help"),
        "hidden footer should show F1 help"
    );
    assert!(
        hidden.contains("Enter view"),
        "hidden footer should show Enter view"
    );
    assert!(
        hidden.contains("Esc quit"),
        "hidden footer should show Esc quit"
    );

    let shown = footer_legend(true);
    assert!(
        shown.contains("Esc quit"),
        "shown footer should show Esc quit"
    );
    assert!(
        shown.contains("F1-F9 commands"),
        "shown footer should mention F1-F9 commands"
    );
}
