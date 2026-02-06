//! Snapshot infrastructure smoke tests for FrankenTUI harness integration.
//!
//! These tests establish baseline snapshots under `tests/snapshots/` and
//! validate the BLESS workflow for future ftui UI migration work.

mod util;

use ftui::Style;
use ftui::text::{Span, Text, WrapMode};
use ftui::widgets::block::Block;
use ftui::widgets::borders::Borders;
use ftui::widgets::list::{List, ListItem, ListState};
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::{StatefulWidget, Widget};
use util::{assert_ftui_snapshot, assert_ftui_snapshot_ansi};

#[test]
fn ftui_snapshot_block_paragraph_baseline() {
    assert_ftui_snapshot("ftui_block_paragraph_baseline", 34, 7, |area, frame| {
        let paragraph = Paragraph::new(Text::raw(
            "Cass ftui harness integration\nSnapshot baseline for migration",
        ))
        .block(Block::default().borders(Borders::ALL).title("cass"));
        paragraph.render(area, frame);
    });
}

#[test]
fn ftui_snapshot_list_selection_baseline() {
    assert_ftui_snapshot("ftui_list_selection_baseline", 28, 6, |area, frame| {
        let items = vec![
            ListItem::new("search mode: lexical"),
            ListItem::new("ranking: balanced"),
            ListItem::new("context: medium"),
        ];
        let list = List::new(items).highlight_symbol("> ");
        let mut state = ListState::default();
        state.select(Some(1));
        StatefulWidget::render(&list, area, frame, &mut state);
    });
}

#[test]
fn ftui_snapshot_ansi_styled_text_baseline() {
    assert_ftui_snapshot_ansi("ftui_styled_text_baseline", 30, 4, |area, frame| {
        let text = Text::from_spans([
            Span::styled("cass ", Style::new().bold()),
            Span::styled("ftui", Style::new().italic()),
            Span::raw(" snapshot"),
        ]);
        let paragraph = Paragraph::new(text).wrap(WrapMode::Word);
        paragraph.render(area, frame);
    });
}
