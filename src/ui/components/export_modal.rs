//! Export modal component for HTML session export.
//!
//! Provides a beautiful, keyboard-navigable modal for configuring HTML export options.
//! Features progressive disclosure, smart defaults, and instant visual feedback.
//!
//! State and logic live here; rendering is done in [`super::super::app::CassApp::render_export_overlay`]
//! using ftui widgets.

use std::path::PathBuf;

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

    /// Output directory (defaults to cwd).
    pub output_dir: PathBuf,

    /// User is editing the output directory path.
    pub output_dir_editing: bool,

    /// Temporary edit buffer for output directory.
    pub output_dir_buffer: String,

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
        let output_dir = get_downloads_dir();
        let output_dir_buffer = output_dir.display().to_string();
        Self {
            focused: ExportField::default(),
            output_dir,
            output_dir_editing: false,
            output_dir_buffer,
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

        let output_dir_buffer = downloads.display().to_string();
        Self {
            output_dir: downloads,
            output_dir_editing: false,
            output_dir_buffer,
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

    /// Toggle the current checkbox field or start editing text fields.
    pub fn toggle_current(&mut self) {
        match self.focused {
            ExportField::OutputDir => {
                self.output_dir_editing = !self.output_dir_editing;
                if self.output_dir_editing {
                    self.output_dir_buffer = self.output_dir.display().to_string();
                } else {
                    // Commit the edit
                    self.commit_output_dir();
                }
            }
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

    /// Commit the output directory edit buffer.
    fn commit_output_dir(&mut self) {
        let path = PathBuf::from(&self.output_dir_buffer);
        if path.is_dir() || !path.exists() {
            self.output_dir = path;
        }
        self.output_dir_editing = false;
    }

    /// Add character to output directory buffer.
    pub fn output_dir_push(&mut self, c: char) {
        if self.focused == ExportField::OutputDir && self.output_dir_editing {
            self.output_dir_buffer.push(c);
        }
    }

    /// Remove last character from output directory buffer.
    pub fn output_dir_pop(&mut self) {
        if self.focused == ExportField::OutputDir && self.output_dir_editing {
            self.output_dir_buffer.pop();
        }
    }

    /// Check if currently editing a text field.
    pub fn is_editing_text(&self) -> bool {
        (self.focused == ExportField::OutputDir && self.output_dir_editing)
            || self.focused == ExportField::Password
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

// =========================================================================
// Legacy ratatui rendering (used by src/ui/tui.rs — the old TUI path)
// =========================================================================

/// Render the export modal using ratatui widgets (legacy path).
pub fn render_export_modal(
    frame: &mut ratatui::Frame,
    state: &ExportModalState,
    palette: super::theme::ThemePalette,
) {
    use ratatui::{
        layout::{Alignment, Constraint, Direction, Layout, Rect},
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::{Block, Borders, Clear, Paragraph},
    };

    let area = frame.area();
    let modal_width = 70u16.min(area.width.saturating_sub(4));
    let modal_height = 24u16.min(area.height.saturating_sub(2));
    let popup_area = super::widgets::centered_rect_fixed(modal_width, modal_height, area);

    frame.render_widget(Clear, popup_area);

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

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(1),
            Constraint::Length(7),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    // Session info card.
    {
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
        let blk = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(palette.border))
            .title(Span::styled(" Session ", Style::default().fg(palette.hint)));
        frame.render_widget(Paragraph::new(lines).block(blk), chunks[0]);
    }

    // Options form.
    {
        let blk = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(palette.border))
            .title(Span::styled(" Options ", Style::default().fg(palette.hint)));
        let opts_inner = blk.inner(chunks[2]);
        frame.render_widget(blk, chunks[2]);

        let opt_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(opts_inner);

        // Output dir.
        let focused = state.focused == ExportField::OutputDir;
        let editing = state.output_dir_editing;
        let dir_style = if focused {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.fg)
        };
        let display_path = if editing {
            &state.output_dir_buffer
        } else {
            &state.output_dir.display().to_string()
        };
        let max_path_len = opt_chunks[0].width.saturating_sub(18) as usize;
        let truncated = if max_path_len < 4 {
            display_path.to_string()
        } else if display_path.chars().count() > max_path_len {
            let tail_len = max_path_len.saturating_sub(3);
            let skip = display_path.chars().count().saturating_sub(tail_len);
            let tail: String = display_path.chars().skip(skip).collect();
            format!("...{tail}")
        } else {
            display_path.to_string()
        };
        let cursor = if editing { "_" } else { "" };
        let hint = if focused && !editing {
            " (Enter to edit)"
        } else if editing {
            " (Enter to confirm)"
        } else {
            ""
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" Output: ", dir_style),
                Span::styled(format!("{truncated}{cursor}"), dir_style),
                Span::styled(hint, Style::default().fg(palette.hint)),
            ])),
            opt_chunks[0],
        );

        // Helper to render checkbox.
        let render_cb =
            |f: &mut ratatui::Frame, label: &str, checked: bool, foc: bool, area: Rect| {
                let mark = if checked { "[x]" } else { "[ ]" };
                let s = if foc {
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(palette.fg)
                };
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(format!(" {mark} "), s),
                        Span::styled(label, s),
                    ])),
                    area,
                );
            };
        render_cb(
            frame,
            "Include tool calls and outputs",
            state.include_tools,
            state.focused == ExportField::IncludeTools,
            opt_chunks[1],
        );
        render_cb(
            frame,
            "Password protection",
            state.encrypt,
            state.focused == ExportField::Encrypt,
            opt_chunks[2],
        );

        if state.encrypt {
            let pfoc = state.focused == ExportField::Password;
            let ps = if pfoc {
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.fg)
            };
            let display = if state.password_visible {
                state.password.clone()
            } else {
                "*".repeat(state.password.len())
            };
            let pc = if pfoc { "_" } else { "" };
            let vh = if state.password_visible {
                "(Ctrl+H hide)"
            } else {
                "(Ctrl+H show)"
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("     Password: ", ps),
                    Span::styled(format!("{display}{pc}"), ps),
                    Span::styled(format!(" {vh}"), Style::default().fg(palette.hint)),
                ])),
                opt_chunks[3],
            );
        }

        let ts_row = if state.encrypt {
            opt_chunks[4]
        } else {
            opt_chunks[3]
        };
        render_cb(
            frame,
            "Show message timestamps",
            state.show_timestamps,
            state.focused == ExportField::ShowTimestamps,
            ts_row,
        );
    }

    // Preview.
    {
        let mut features = vec!["Dark/Light themes", "Print-friendly", "Search enabled"];
        if state.encrypt {
            features.push("Encrypted");
        }
        let est_kb = (state.message_count * 2 + 15).max(20);
        let size_str = if est_kb > 1024 {
            format!("~{:.1}MB", est_kb as f64 / 1024.0)
        } else {
            format!("~{est_kb}KB")
        };
        let preview_line = format!(
            "{} messages | {} estimated | {}",
            state.message_count,
            size_str,
            features.join(" | ")
        );
        let blk = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(palette.border))
            .title(Span::styled(" Preview ", Style::default().fg(palette.hint)));
        let mut lines = vec![
            Line::from(Span::styled(
                &state.filename_preview,
                Style::default().fg(palette.fg),
            )),
            Line::from(Span::styled(
                preview_line,
                Style::default().fg(palette.hint),
            )),
        ];
        let (ptext, ps) = match &state.progress {
            ExportProgress::Idle => (None, Style::default()),
            ExportProgress::Preparing => (
                Some("Preparing export...".to_string()),
                Style::default().fg(palette.accent),
            ),
            ExportProgress::Encrypting => (
                Some("Encrypting content...".to_string()),
                Style::default().fg(palette.accent),
            ),
            ExportProgress::Writing => (
                Some("Writing HTML file...".to_string()),
                Style::default().fg(palette.accent),
            ),
            ExportProgress::Complete(p) => {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| p.display().to_string());
                (
                    Some(format!("Exported: {name}")),
                    Style::default().fg(palette.user),
                )
            }
            ExportProgress::Error(m) => (
                Some(format!("Error: {m}")),
                Style::default().fg(Color::Rgb(247, 118, 142)),
            ),
        };
        if let Some(t) = ptext {
            lines.push(Line::from(Span::styled(t, ps)));
        }
        frame.render_widget(Paragraph::new(lines).block(blk), chunks[4]);
    }

    // Footer.
    {
        let can_export = state.can_export();
        let es = if can_export && state.focused == ExportField::ExportButton {
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
            Span::styled(" Enter ", es),
            Span::styled("Export  ", es),
            Span::styled(" Esc ", Style::default().fg(palette.hint)),
            Span::styled("Cancel", Style::default().fg(palette.fg)),
        ];
        frame.render_widget(
            Paragraph::new(Line::from(hints)).alignment(Alignment::Center),
            chunks[6],
        );
    }
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
