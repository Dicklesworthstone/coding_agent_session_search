//! Export modal component for HTML session export.
//!
//! Provides a beautiful, keyboard-navigable modal for configuring HTML export options.
//! Features progressive disclosure, smart defaults, and instant visual feedback.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::path::PathBuf;

use super::theme::ThemePalette;
use crate::html_export::{
    ExportOptions, FilenameMetadata, FilenameOptions, generate_filepath, get_downloads_dir,
};
use crate::search::query::SearchHit;
use crate::ui::data::ConversationView;

/// Focus field in the export modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportField {
    #[default]
    OutputDir,
    IncludeTools,
    Encrypt,
    Password,
    ShowTimestamps,
    ExportButton,
}

impl ExportField {
    /// Get next field (Tab navigation).
    pub fn next(self, encrypt_enabled: bool) -> Self {
        match self {
            Self::OutputDir => Self::IncludeTools,
            Self::IncludeTools => Self::Encrypt,
            Self::Encrypt => {
                if encrypt_enabled {
                    Self::Password
                } else {
                    Self::ShowTimestamps
                }
            }
            Self::Password => Self::ShowTimestamps,
            Self::ShowTimestamps => Self::ExportButton,
            Self::ExportButton => Self::OutputDir,
        }
    }

    /// Get previous field (Shift+Tab navigation).
    pub fn prev(self, encrypt_enabled: bool) -> Self {
        match self {
            Self::OutputDir => Self::ExportButton,
            Self::IncludeTools => Self::OutputDir,
            Self::Encrypt => Self::IncludeTools,
            Self::Password => Self::Encrypt,
            Self::ShowTimestamps => {
                if encrypt_enabled {
                    Self::Password
                } else {
                    Self::Encrypt
                }
            }
            Self::ExportButton => Self::ShowTimestamps,
        }
    }
}

/// Export progress states.
#[derive(Debug, Clone, Default)]
pub enum ExportProgress {
    #[default]
    Idle,
    Preparing,
    Encrypting,
    Writing,
    Complete(PathBuf),
    Error(String),
}

impl ExportProgress {
    /// Check if export is in progress.
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Preparing | Self::Encrypting | Self::Writing)
    }
}

/// State for the export modal.
#[derive(Debug, Clone)]
pub struct ExportModalState {
    /// Currently focused field.
    pub focused: ExportField,

    /// Output directory (defaults to Downloads).
    pub output_dir: PathBuf,

    /// Generated filename preview.
    pub filename_preview: String,

    /// Include tool calls in export.
    pub include_tools: bool,

    /// Enable encryption.
    pub encrypt: bool,

    /// Password for encryption (only used if encrypt is true).
    pub password: String,

    /// Show password characters (toggle visibility).
    pub password_visible: bool,

    /// Show message timestamps.
    pub show_timestamps: bool,

    /// Export progress state.
    pub progress: ExportProgress,

    /// Session metadata for display.
    pub agent_name: String,
    pub workspace: String,
    pub timestamp: String,
    pub message_count: usize,
    pub title_preview: String,
}

impl Default for ExportModalState {
    fn default() -> Self {
        Self {
            focused: ExportField::default(),
            output_dir: get_downloads_dir(),
            filename_preview: String::new(),
            include_tools: true,
            encrypt: false,
            password: String::new(),
            password_visible: false,
            show_timestamps: true,
            progress: ExportProgress::default(),
            agent_name: String::new(),
            workspace: String::new(),
            timestamp: String::new(),
            message_count: 0,
            title_preview: String::new(),
        }
    }
}

impl ExportModalState {
    /// Create new export modal state from a search hit and conversation view.
    pub fn from_hit(hit: &SearchHit, view: &ConversationView) -> Self {
        let agent = &hit.agent;
        let workspace = &hit.workspace;
        let started_at = view.convo.started_at.unwrap_or(0);
        let message_count = view.messages.len();

        // Extract title from first message or use fallback
        let title_preview = view
            .messages
            .first()
            .map(|m| {
                let content = m.content.trim();
                // Use char_indices to safely truncate at UTF-8 boundary (57 chars + "...")
                if content.chars().count() > 60 {
                    let end_idx = content
                        .char_indices()
                        .nth(56)
                        .map(|(idx, _)| idx)
                        .unwrap_or(content.len());
                    format!("{}...", &content[..end_idx])
                } else {
                    content.to_string()
                }
            })
            .unwrap_or_else(|| "Untitled Session".to_string());

        // Format date for filename
        let date_str = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(started_at)
            .map(|dt| dt.format("%Y-%m-%d").to_string());

        // Generate filename preview
        let metadata = FilenameMetadata {
            agent: Some(agent.clone()),
            date: date_str,
            project: Some(workspace.clone()),
            topic: Some(title_preview.clone()),
            title: None,
        };
        let options = FilenameOptions {
            include_date: true,
            include_agent: true,
            include_project: true,
            include_topic: true,
            ..Default::default()
        };
        let downloads = get_downloads_dir();
        let filepath = generate_filepath(&downloads, &metadata, &options);
        let filename_preview = filepath
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "session.html".to_string());

        // Format timestamp for display
        let timestamp = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(started_at)
            .map(|dt| dt.format("%b %d, %Y at %I:%M %p").to_string())
            .unwrap_or_else(|| "Unknown date".to_string());

        Self {
            output_dir: downloads,
            filename_preview,
            include_tools: true,
            encrypt: false,
            password: String::new(),
            password_visible: false,
            show_timestamps: true,
            focused: ExportField::default(),
            progress: ExportProgress::default(),
            agent_name: agent.clone(),
            workspace: workspace.clone(),
            timestamp,
            message_count,
            title_preview,
        }
    }

    /// Navigate to next field.
    pub fn next_field(&mut self) {
        self.focused = self.focused.next(self.encrypt);
    }

    /// Navigate to previous field.
    pub fn prev_field(&mut self) {
        self.focused = self.focused.prev(self.encrypt);
    }

    /// Toggle the current checkbox field.
    pub fn toggle_current(&mut self) {
        match self.focused {
            ExportField::IncludeTools => self.include_tools = !self.include_tools,
            ExportField::Encrypt => {
                self.encrypt = !self.encrypt;
                if !self.encrypt {
                    self.password.clear();
                }
            }
            ExportField::ShowTimestamps => self.show_timestamps = !self.show_timestamps,
            _ => {}
        }
    }

    /// Toggle password visibility.
    pub fn toggle_password_visibility(&mut self) {
        self.password_visible = !self.password_visible;
    }

    /// Add character to password.
    pub fn password_push(&mut self, c: char) {
        if self.focused == ExportField::Password {
            self.password.push(c);
        }
    }

    /// Remove last character from password.
    pub fn password_pop(&mut self) {
        if self.focused == ExportField::Password {
            self.password.pop();
        }
    }

    /// Check if export is ready (valid configuration).
    pub fn can_export(&self) -> bool {
        !self.progress.is_busy() && (!self.encrypt || !self.password.is_empty())
    }

    /// Get export options from current state.
    pub fn to_export_options(&self) -> ExportOptions {
        ExportOptions {
            title: Some(self.title_preview.clone()),
            include_cdn: true,
            syntax_highlighting: true,
            include_search: true,
            include_theme_toggle: true,
            encrypt: self.encrypt,
            print_styles: true,
            agent_name: Some(self.agent_name.clone()),
            show_timestamps: self.show_timestamps,
            show_tool_calls: self.include_tools,
        }
    }

    /// Get the full output path.
    pub fn output_path(&self) -> PathBuf {
        self.output_dir.join(&self.filename_preview)
    }
}

/// Render the export modal.
pub fn render_export_modal(frame: &mut Frame, state: &ExportModalState, palette: ThemePalette) {
    let area = frame.area();

    // Modal size: 70x24 or smaller if terminal is small
    let modal_width = 70.min(area.width.saturating_sub(4));
    let modal_height = 24.min(area.height.saturating_sub(2));

    let popup_area = centered_rect_fixed(modal_width, modal_height, area);

    // Clear background
    frame.render_widget(Clear, popup_area);

    // Build modal content
    let block = Block::default()
        .title(Span::styled(
            " Export Session as HTML ",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.accent));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    // Layout: session info, options, preview, footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(4), // Session info
            Constraint::Length(1), // Spacer
            Constraint::Length(7), // Options
            Constraint::Length(1), // Spacer
            Constraint::Length(3), // Preview
            Constraint::Min(1),    // Flex
            Constraint::Length(1), // Footer
        ])
        .split(inner);

    // Session info card
    render_session_card(frame, state, chunks[0], palette);

    // Options form
    render_options_form(frame, state, chunks[2], palette);

    // Preview section
    render_preview(frame, state, chunks[4], palette);

    // Footer with keyboard hints
    render_footer(frame, state, chunks[6], palette);
}

/// Render the session info card.
fn render_session_card(
    frame: &mut Frame,
    state: &ExportModalState,
    area: Rect,
    palette: ThemePalette,
) {
    let agent_badge = format!(" {} ", state.agent_name);
    let location = format!("{} | {}", state.workspace, state.timestamp);
    let stats = format!("{} messages", state.message_count);

    let lines = vec![
        Line::from(vec![
            Span::styled(
                agent_badge,
                Style::default()
                    .fg(palette.bg)
                    .bg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(location, Style::default().fg(palette.hint)),
        ]),
        Line::from(Span::styled(
            &state.title_preview,
            Style::default()
                .fg(palette.fg)
                .add_modifier(Modifier::ITALIC),
        )),
        Line::from(Span::styled(stats, Style::default().fg(palette.hint))),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.border))
        .title(Span::styled(" Session ", Style::default().fg(palette.hint)));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Render the options form.
fn render_options_form(
    frame: &mut Frame,
    state: &ExportModalState,
    area: Rect,
    palette: ThemePalette,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.border))
        .title(Span::styled(" Options ", Style::default().fg(palette.hint)));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let option_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Include tools
            Constraint::Length(1), // Encrypt
            Constraint::Length(1), // Password (conditional)
            Constraint::Length(1), // Show timestamps
            Constraint::Min(0),    // Flex
        ])
        .split(inner);

    // Include tools checkbox
    render_checkbox(
        frame,
        "Include tool calls and outputs",
        state.include_tools,
        state.focused == ExportField::IncludeTools,
        option_chunks[0],
        palette,
    );

    // Encrypt checkbox
    render_checkbox(
        frame,
        "Password protection",
        state.encrypt,
        state.focused == ExportField::Encrypt,
        option_chunks[1],
        palette,
    );

    // Password input (only shown if encrypt is enabled)
    if state.encrypt {
        render_password_input(
            frame,
            &state.password,
            state.password_visible,
            state.focused == ExportField::Password,
            option_chunks[2],
            palette,
        );
    }

    // Show timestamps checkbox
    let timestamps_row = if state.encrypt {
        option_chunks[3]
    } else {
        option_chunks[2]
    };
    render_checkbox(
        frame,
        "Show message timestamps",
        state.show_timestamps,
        state.focused == ExportField::ShowTimestamps,
        timestamps_row,
        palette,
    );
}

/// Render a checkbox option.
fn render_checkbox(
    frame: &mut Frame,
    label: &str,
    checked: bool,
    focused: bool,
    area: Rect,
    palette: ThemePalette,
) {
    let checkbox = if checked { "[x]" } else { "[ ]" };
    let style = if focused {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.fg)
    };

    let line = Line::from(vec![
        Span::styled(format!(" {} ", checkbox), style),
        Span::styled(label, style),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

/// Render password input field.
fn render_password_input(
    frame: &mut Frame,
    password: &str,
    visible: bool,
    focused: bool,
    area: Rect,
    palette: ThemePalette,
) {
    let display = if visible {
        password.to_string()
    } else {
        "*".repeat(password.len())
    };

    let style = if focused {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.fg)
    };

    let visibility_hint = if visible {
        "(Ctrl+H hide)"
    } else {
        "(Ctrl+H show)"
    };
    let cursor = if focused { "_" } else { "" };

    let line = Line::from(vec![
        Span::styled("     Password: ", style),
        Span::styled(format!("{}{}", display, cursor), style),
        Span::styled(
            format!(" {}", visibility_hint),
            Style::default().fg(palette.hint),
        ),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

/// Render the preview section.
fn render_preview(frame: &mut Frame, state: &ExportModalState, area: Rect, palette: ThemePalette) {
    let mut features = vec!["Dark/Light themes", "Print-friendly", "Search enabled"];
    if state.encrypt {
        features.push("Encrypted");
    }

    // Estimate file size (rough: ~2KB per message + overhead)
    let estimated_kb = (state.message_count * 2 + 15).max(20);
    let size_str = if estimated_kb > 1024 {
        format!("~{:.1}MB", estimated_kb as f64 / 1024.0)
    } else {
        format!("~{}KB", estimated_kb)
    };

    let features_str = features.join(" | ");
    let preview_line = format!(
        "{} messages | {} estimated | {}",
        state.message_count, size_str, features_str
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.border))
        .title(Span::styled(" Preview ", Style::default().fg(palette.hint)));

    let lines = vec![
        Line::from(Span::styled(
            &state.filename_preview,
            Style::default().fg(palette.fg),
        )),
        Line::from(Span::styled(
            preview_line,
            Style::default().fg(palette.hint),
        )),
    ];

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Render the footer with keyboard hints.
fn render_footer(frame: &mut Frame, state: &ExportModalState, area: Rect, palette: ThemePalette) {
    let can_export = state.can_export();
    let export_style = if can_export && state.focused == ExportField::ExportButton {
        Style::default()
            .fg(palette.bg)
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else if can_export {
        Style::default().fg(palette.accent)
    } else {
        Style::default().fg(palette.hint)
    };

    let hints = vec![
        Span::styled(" Tab ", Style::default().fg(palette.hint)),
        Span::styled("Navigate  ", Style::default().fg(palette.fg)),
        Span::styled(" Space ", Style::default().fg(palette.hint)),
        Span::styled("Toggle  ", Style::default().fg(palette.fg)),
        Span::styled(" Enter ", export_style),
        Span::styled("Export  ", export_style),
        Span::styled(" Esc ", Style::default().fg(palette.hint)),
        Span::styled("Cancel", Style::default().fg(palette.fg)),
    ];

    frame.render_widget(
        Paragraph::new(Line::from(hints)).alignment(Alignment::Center),
        area,
    );
}

/// Create a centered rect with fixed dimensions.
fn centered_rect_fixed(width: u16, height: u16, r: Rect) -> Rect {
    let actual_width = width.min(r.width.saturating_sub(4));
    let actual_height = height.min(r.height.saturating_sub(2));

    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(r.height.saturating_sub(actual_height) / 2),
            Constraint::Length(actual_height),
            Constraint::Length(r.height.saturating_sub(actual_height) / 2),
        ])
        .split(r);

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(r.width.saturating_sub(actual_width) / 2),
            Constraint::Length(actual_width),
            Constraint::Length(r.width.saturating_sub(actual_width) / 2),
        ])
        .split(popup_layout[1]);

    horizontal[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_export_field_navigation() {
        // Test Tab navigation without encryption
        let mut field = ExportField::OutputDir;
        field = field.next(false);
        assert_eq!(field, ExportField::IncludeTools);
        field = field.next(false);
        assert_eq!(field, ExportField::Encrypt);
        field = field.next(false);
        assert_eq!(field, ExportField::ShowTimestamps); // Skips password
        field = field.next(false);
        assert_eq!(field, ExportField::ExportButton);
        field = field.next(false);
        assert_eq!(field, ExportField::OutputDir); // Wraps

        // Test Tab navigation with encryption
        let mut field = ExportField::Encrypt;
        field = field.next(true);
        assert_eq!(field, ExportField::Password); // Includes password
    }

    #[test]
    fn test_export_field_prev_navigation() {
        // Test Shift+Tab without encryption
        let mut field = ExportField::ShowTimestamps;
        field = field.prev(false);
        assert_eq!(field, ExportField::Encrypt); // Skips password

        // Test Shift+Tab with encryption
        let mut field = ExportField::ShowTimestamps;
        field = field.prev(true);
        assert_eq!(field, ExportField::Password); // Includes password
    }

    #[test]
    fn test_can_export() {
        let state = ExportModalState::default();
        assert!(state.can_export());

        let state = ExportModalState {
            encrypt: true,
            ..Default::default()
        };
        assert!(!state.can_export());

        let state = ExportModalState {
            encrypt: true,
            password: "secret".to_string(),
            ..Default::default()
        };
        assert!(state.can_export());
    }

    #[test]
    fn test_toggle_encryption_clears_password() {
        let mut state = ExportModalState {
            encrypt: true,
            password: "secret".to_string(),
            focused: ExportField::Encrypt,
            ..Default::default()
        };

        // Toggling encryption off should clear password
        state.toggle_current();
        assert!(!state.encrypt);
        assert!(state.password.is_empty());
    }
}
