use ratatui::layout::Alignment;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::ui::components::theme::ThemePalette;

pub fn search_bar(query: &str, palette: ThemePalette, focused: bool) -> Paragraph<'static> {
    let title = Span::styled("Search", palette.title());
    let style = if focused {
        Style::default().fg(palette.accent)
    } else {
        Style::default().fg(palette.hint)
    };

    let body = vec![
        Line::from(Span::styled(format!("/ {}", query), style)),
        Line::from(vec![
            Span::styled("Tips: ", palette.title()),
            Span::raw("a/w/f/t filters • A/W/F clear • x clear all • o open"),
        ]),
    ];

    Paragraph::new(body)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(palette.accent_alt)),
        )
        .style(Style::default())
        .alignment(Alignment::Left)
}
