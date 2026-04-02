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
    unique_filename,
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

fn timestamp_to_utc(ts: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    if ts.unsigned_abs() >= 10_000_000_000 {
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ts)
    } else {
        chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
    }
}

impl ExportModalState {
    /// Create new export modal state from a search hit and conversation view.
    pub fn from_hit(hit: &SearchHit, view: &ConversationView) -> Self {
        let agent = if view.convo.agent_slug.trim().is_empty() {
            hit.agent.trim().to_string()
        } else {
            view.convo.agent_slug.trim().to_string()
        };
        let workspace = view
            .workspace
            .as_ref()
            .map(|ws| ws.path.display().to_string())
            .or_else(|| {
                view.convo
                    .workspace
                    .as_ref()
                    .map(|path| path.display().to_string())
            })
            .filter(|workspace| !workspace.trim().is_empty())
            .unwrap_or_else(|| hit.workspace.trim().to_string());
        let started_at = view
            .convo
            .started_at
            .or_else(|| view.messages.iter().filter_map(|m| m.created_at).min())
            .or(hit.created_at);
        let message_count = view.messages.len();

        // Prefer stable session metadata over first-message text so export titles and
        // filenames do not drift when the indexed conversation already has a real title.
        let title_preview = view
            .convo
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let hit_title = hit.title.trim();
                (!hit_title.is_empty()).then(|| hit_title.to_string())
            })
            .or_else(|| {
                view.messages.first().map(|m| {
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
            })
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| "Untitled Session".to_string());

        // Format date for filename
        let started_dt = started_at.and_then(timestamp_to_utc);
        let date_str = started_dt.map(|dt| dt.format("%Y-%m-%d").to_string());

        // Generate filename preview
        let metadata = FilenameMetadata {
            agent: (!agent.is_empty()).then(|| agent.clone()),
            date: date_str,
            project: (!workspace.is_empty()).then(|| workspace.clone()),
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
        let base_filename = filepath
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("session.html");
        let filename_preview = unique_filename(&downloads, base_filename)
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| base_filename.to_string());

        // Format timestamp for display
        let timestamp = started_at
            .and_then(timestamp_to_utc)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{Conversation, Message, MessageRole};
    use crate::search::query::MatchType;
    use crate::ui::data::ConversationView;
    use std::path::PathBuf;

    fn make_hit(created_at: Option<i64>) -> SearchHit {
        SearchHit {
            title: "t".to_string(),
            snippet: "s".to_string(),
            content: "content".to_string(),
            content_hash: 1,
            conversation_id: None,
            score: 1.0,
            source_path: "/tmp/session.jsonl".to_string(),
            agent: "codex".to_string(),
            workspace: "/tmp/ws".to_string(),
            workspace_original: None,
            created_at,
            line_number: Some(1),
            match_type: MatchType::Exact,
            source_id: "local".to_string(),
            origin_kind: "local".to_string(),
            origin_host: None,
        }
    }

    fn make_view(started_at: Option<i64>, message_ts: Option<i64>) -> ConversationView {
        ConversationView {
            convo: Conversation {
                id: Some(1),
                agent_slug: "codex".to_string(),
                workspace: Some(PathBuf::from("/tmp/ws")),
                external_id: Some("ext-1".to_string()),
                title: Some("session".to_string()),
                source_path: PathBuf::from("/tmp/session.jsonl"),
                started_at,
                ended_at: started_at,
                approx_tokens: None,
                metadata_json: serde_json::json!({}),
                messages: Vec::new(),
                source_id: "local".to_string(),
                origin_host: None,
            },
            messages: vec![Message {
                id: Some(1),
                idx: 0,
                role: MessageRole::User,
                author: Some("user".to_string()),
                created_at: message_ts,
                content: "hello export".to_string(),
                extra_json: serde_json::json!({}),
                snippets: Vec::new(),
            }],
            workspace: None,
        }
    }

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

    #[test]
    fn from_hit_prefers_conversation_agent_and_workspace_metadata() {
        let mut hit = make_hit(None);
        hit.agent = "stale-agent".to_string();
        hit.workspace = "/stale/ws".to_string();

        let mut view = make_view(None, Some(1_700_000_000));
        view.convo.agent_slug = "cursor".to_string();
        view.convo.workspace = Some(PathBuf::from("/canonical/ws"));

        let state = ExportModalState::from_hit(&hit, &view);

        assert_eq!(state.agent_name, "cursor");
        assert_eq!(state.workspace, "/canonical/ws");
        assert!(
            state.filename_preview.contains("cursor") || state.filename_preview.contains("Cursor"),
            "filename should use canonical agent metadata"
        );
        assert!(
            state.filename_preview.contains("canonical-ws")
                || state.filename_preview.contains("canonical_ws")
                || state.filename_preview.contains("canonical"),
            "filename should use canonical workspace metadata"
        );
    }

    #[test]
    fn from_hit_prefers_conversation_title_for_title_preview() {
        let hit = make_hit(None);
        let mut view = make_view(None, Some(1_700_000_000));
        view.convo.title = Some("Canonical Session Title".to_string());
        view.messages[0].content = "hello export".to_string();

        let state = ExportModalState::from_hit(&hit, &view);

        assert_eq!(state.title_preview, "Canonical Session Title");
        assert!(
            state.filename_preview.contains("canonical-session-title")
                || state.filename_preview.contains("Canonical-Session-Title")
                || state.filename_preview.contains("canonical_session_title"),
            "filename should derive from the canonical conversation title"
        );
    }

    #[test]
    fn from_hit_trims_whitespace_hit_agent_and_workspace_when_view_metadata_missing() {
        let mut hit = make_hit(None);
        hit.agent = "   codex   ".to_string();
        hit.workspace = "   /tmp/ws   ".to_string();

        let mut view = make_view(None, Some(1_700_000_000));
        view.convo.agent_slug.clear();
        view.convo.workspace = None;
        view.workspace = None;

        let state = ExportModalState::from_hit(&hit, &view);

        assert_eq!(state.agent_name, "codex");
        assert_eq!(state.workspace, "/tmp/ws");
    }

    #[test]
    fn from_hit_falls_back_to_search_hit_title_when_conversation_title_missing() {
        let mut hit = make_hit(None);
        hit.title = "Search Hit Title".to_string();
        let mut view = make_view(None, Some(1_700_000_000));
        view.convo.title = None;
        view.messages[0].content = "first user message body".to_string();

        let state = ExportModalState::from_hit(&hit, &view);

        assert_eq!(state.title_preview, "Search Hit Title");
    }

    #[test]
    fn from_hit_ignores_whitespace_first_message_when_deriving_title_preview() {
        let mut hit = make_hit(None);
        hit.title = "   ".to_string();
        let mut view = make_view(None, Some(1_700_000_000));
        view.convo.title = None;
        view.messages[0].content = "   
	  "
        .to_string();

        let state = ExportModalState::from_hit(&hit, &view);

        assert_eq!(state.title_preview, "Untitled Session");
    }

    #[test]
    fn from_hit_uses_message_timestamp_when_conversation_start_missing() {
        let hit = make_hit(None);
        let view = make_view(None, Some(1_700_000_000));
        let state = ExportModalState::from_hit(&hit, &view);

        assert_ne!(state.timestamp, "Unknown date");
        assert!(!state.filename_preview.contains("1970"));
    }

    #[test]
    fn from_hit_with_no_timestamps_does_not_fabricate_epoch_date() {
        let hit = make_hit(None);
        let mut view = make_view(None, None);
        view.messages.clear();
        let state = ExportModalState::from_hit(&hit, &view);

        assert_eq!(state.timestamp, "Unknown date");
        assert!(!state.filename_preview.contains("1970"));
    }
}
