//! Conversation to HTML rendering.
//!
//! Converts session messages into semantic HTML markup with proper
//! role-based styling, agent-specific theming, and syntax highlighting support.
//!
//! # Features
//!
//! - **Role-based styling**: User, assistant, tool, and system messages
//! - **Agent-specific theming**: Visual differentiation for 11 supported agents
//! - **Code blocks**: Syntax highlighting with Prism.js language classes
//! - **Tool calls**: Collapsible details with formatted JSON
//! - **Long message collapse**: Optional folding for lengthy content
//! - **XSS prevention**: All user content is properly escaped
//! - **Accessible**: Semantic HTML with ARIA attributes

use std::fmt;
use std::time::Instant;

use super::template::html_escape;
use pulldown_cmark::{Options, Parser, html};
use serde_json;
use tracing::{debug, info, trace};

/// Errors that can occur during rendering.
#[derive(Debug)]
pub enum RenderError {
    /// Invalid message data
    InvalidMessage(String),
    /// Content parsing failed
    ParseError(String),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::InvalidMessage(msg) => write!(f, "invalid message: {}", msg),
            RenderError::ParseError(msg) => write!(f, "parse error: {}", msg),
        }
    }
}

impl std::error::Error for RenderError {}

/// Options for rendering conversations.
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// Show message timestamps
    pub show_timestamps: bool,

    /// Show tool call details
    pub show_tool_calls: bool,

    /// Enable syntax highlighting markers (for Prism.js)
    pub syntax_highlighting: bool,

    /// Wrap long lines in code blocks
    pub wrap_code: bool,

    /// Collapse messages longer than this threshold (characters)
    /// Set to 0 to disable collapsing
    pub collapse_threshold: usize,

    /// Maximum lines to show in collapsed code blocks preview
    pub code_preview_lines: usize,

    /// Agent slug for agent-specific styling
    pub agent_slug: Option<String>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            show_timestamps: true,
            show_tool_calls: true,
            syntax_highlighting: true,
            wrap_code: false,
            collapse_threshold: 0, // Disabled by default
            code_preview_lines: 20,
            agent_slug: None,
        }
    }
}

/// A message to render.
#[derive(Debug, Clone)]
pub struct Message {
    /// Role: user, assistant, tool, system
    pub role: String,

    /// Message content (may contain markdown)
    pub content: String,

    /// Optional timestamp (ISO 8601)
    pub timestamp: Option<String>,

    /// Optional tool call information
    pub tool_call: Option<ToolCall>,

    /// Optional message index for anchoring
    pub index: Option<usize>,

    /// Optional author name (for multi-participant sessions)
    pub author: Option<String>,
}

/// Tool call information.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Tool name (e.g., "Bash", "Read", "Write")
    pub name: String,

    /// Tool input/arguments (usually JSON)
    pub input: String,

    /// Tool output/result
    pub output: Option<String>,

    /// Execution status (success, error, etc.)
    pub status: Option<ToolStatus>,
}

/// Status of a tool execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Success,
    Error,
    Pending,
}

impl ToolStatus {
    fn css_class(&self) -> &'static str {
        match self {
            ToolStatus::Success => "tool-status-success",
            ToolStatus::Error => "tool-status-error",
            ToolStatus::Pending => "tool-status-pending",
        }
    }

    fn icon_svg(&self) -> &'static str {
        match self {
            ToolStatus::Success => ICON_CHECK,
            ToolStatus::Error => ICON_X,
            ToolStatus::Pending => ICON_LOADER,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            ToolStatus::Success => "success",
            ToolStatus::Error => "error",
            ToolStatus::Pending => "pending",
        }
    }
}

// ============================================
// Message Grouping Types for Consolidated Rendering
// ============================================
// NOTE: These types are used by the message grouping algorithm (Task 1c6z)
// and renderer updates (Task 27t2). Allow dead_code until integration.

/// Type of message group for rendering decisions.
///
/// Determines how a group of related messages should be styled and displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum MessageGroupType {
    /// User-initiated message (question, instruction, etc.)
    User,
    /// Assistant/agent response with potential tool calls
    Assistant,
    /// System message (context, instructions)
    System,
    /// Orphan tool calls without a parent assistant message
    ToolOnly,
}

#[allow(dead_code)]
impl MessageGroupType {
    /// Get the CSS class for this group type.
    pub fn css_class(&self) -> &'static str {
        match self {
            MessageGroupType::User => "message-group-user",
            MessageGroupType::Assistant => "message-group-assistant",
            MessageGroupType::System => "message-group-system",
            MessageGroupType::ToolOnly => "message-group-tool",
        }
    }

    /// Get the role icon for this group type.
    pub fn role_icon(&self) -> &'static str {
        match self {
            MessageGroupType::User => "user",
            MessageGroupType::Assistant => "assistant",
            MessageGroupType::System => "system",
            MessageGroupType::ToolOnly => "tool",
        }
    }
}

/// Extended tool result with status and content.
///
/// Represents the output from a tool execution, paired with metadata
/// for correlation and status tracking.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ToolResult {
    /// Tool name this result responds to.
    pub tool_name: String,
    /// Result content (may be truncated for display).
    pub content: String,
    /// Execution status.
    pub status: ToolStatus,
    /// Correlation ID to match with the originating call (e.g., tool_use_id).
    pub correlation_id: Option<String>,
}

#[allow(dead_code)]
impl ToolResult {
    /// Create a new tool result.
    pub fn new(
        tool_name: impl Into<String>,
        content: impl Into<String>,
        status: ToolStatus,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            content: content.into(),
            status,
            correlation_id: None,
        }
    }

    /// Set the correlation ID for matching with tool calls.
    pub fn with_correlation_id(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }

    /// Check if this result indicates an error.
    pub fn is_error(&self) -> bool {
        self.status == ToolStatus::Error
    }
}

/// Tool call paired with its result for correlation.
///
/// Keeps a tool invocation together with its response, enabling
/// consolidated rendering of the complete tool interaction.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ToolCallWithResult {
    /// The original tool call.
    pub call: ToolCall,
    /// The result (if received).
    pub result: Option<ToolResult>,
    /// Correlation ID (tool_use_id in Claude format).
    pub correlation_id: Option<String>,
}

#[allow(dead_code)]
impl ToolCallWithResult {
    /// Create a new tool call without a result yet.
    pub fn new(call: ToolCall) -> Self {
        Self {
            call,
            result: None,
            correlation_id: None,
        }
    }

    /// Set the correlation ID.
    pub fn with_correlation_id(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }

    /// Attach a result to this tool call.
    pub fn with_result(mut self, result: ToolResult) -> Self {
        self.result = Some(result);
        self
    }

    /// Check if this tool call has a result.
    pub fn has_result(&self) -> bool {
        self.result.is_some()
    }

    /// Check if the tool call resulted in an error.
    pub fn is_error(&self) -> bool {
        self.result.as_ref().is_some_and(|r| r.is_error())
    }

    /// Get the effective status (from result or call).
    pub fn effective_status(&self) -> ToolStatus {
        self.result
            .as_ref()
            .map(|r| r.status)
            .or(self.call.status)
            .unwrap_or(ToolStatus::Pending)
    }
}

/// A group of related messages for consolidated rendering.
///
/// Represents a logical unit of conversation: a primary message (user question
/// or assistant response) along with all associated tool calls and their results.
/// This enables rendering an entire interaction as a cohesive block rather than
/// separate messages.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MessageGroup {
    /// Group type for rendering decisions.
    pub group_type: MessageGroupType,
    /// The primary message (user or assistant text).
    pub primary: Message,
    /// Tool calls paired with their results.
    pub tool_calls: Vec<ToolCallWithResult>,
    /// Timestamp of the first message/action in this group.
    pub start_timestamp: Option<String>,
    /// Timestamp of the last message/action in this group.
    pub end_timestamp: Option<String>,
}

#[allow(dead_code)]
impl MessageGroup {
    /// Create a new message group with a primary message.
    pub fn new(primary: Message, group_type: MessageGroupType) -> Self {
        let end_timestamp = primary.timestamp.clone();
        let start_timestamp = primary.timestamp.clone();
        Self {
            group_type,
            primary,
            tool_calls: Vec::new(),
            start_timestamp,
            end_timestamp,
        }
    }

    /// Create a user message group.
    pub fn user(primary: Message) -> Self {
        Self::new(primary, MessageGroupType::User)
    }

    /// Create an assistant message group.
    pub fn assistant(primary: Message) -> Self {
        Self::new(primary, MessageGroupType::Assistant)
    }

    /// Create a system message group.
    pub fn system(primary: Message) -> Self {
        Self::new(primary, MessageGroupType::System)
    }

    /// Create a tool-only group (orphan tool calls).
    pub fn tool_only(primary: Message) -> Self {
        Self::new(primary, MessageGroupType::ToolOnly)
    }

    /// Add a tool call to this group.
    pub fn add_tool_call(&mut self, call: ToolCall, correlation_id: Option<String>) {
        tracing::trace!(
            tool_name = %call.name,
            correlation_id = ?correlation_id,
            "Adding tool call to message group"
        );
        let mut tc = ToolCallWithResult::new(call);
        if let Some(id) = correlation_id {
            tc = tc.with_correlation_id(id);
        }
        self.tool_calls.push(tc);
    }

    /// Add a tool result, matching it with an existing call by correlation ID.
    ///
    /// If a matching call is found, the result is attached to it.
    /// If no match is found, the result is dropped with a warning.
    pub fn add_tool_result(&mut self, result: ToolResult) {
        // Try to match by correlation ID first
        if let Some(ref corr_id) = result.correlation_id {
            for tc in &mut self.tool_calls {
                if tc.correlation_id.as_ref() == Some(corr_id) {
                    tracing::trace!(
                        tool_name = %result.tool_name,
                        correlation_id = %corr_id,
                        "Matched tool result to call"
                    );
                    tc.result = Some(result);
                    return;
                }
            }
        }

        // Fall back to matching by tool name (first unmatched call)
        for tc in &mut self.tool_calls {
            if tc.result.is_none() && tc.call.name == result.tool_name {
                tracing::trace!(
                    tool_name = %result.tool_name,
                    "Matched tool result to call by name"
                );
                tc.result = Some(result);
                return;
            }
        }

        tracing::warn!(
            tool_name = %result.tool_name,
            correlation_id = ?result.correlation_id,
            "Could not match tool result to any call"
        );
    }

    /// Update the end timestamp if the given timestamp is later.
    pub fn update_end_timestamp(&mut self, timestamp: Option<String>) {
        if let Some(ts) = timestamp {
            match (&self.end_timestamp, &ts) {
                (Some(existing), new) if new > existing => {
                    self.end_timestamp = Some(ts);
                }
                (None, _) => {
                    self.end_timestamp = Some(ts);
                }
                _ => {}
            }
        }
    }

    /// Get the number of tool calls in this group.
    pub fn tool_count(&self) -> usize {
        self.tool_calls.len()
    }

    /// Check if any tool call in this group resulted in an error.
    pub fn has_errors(&self) -> bool {
        self.tool_calls.iter().any(|tc| tc.is_error())
    }

    /// Check if all tool calls have results.
    pub fn all_tools_complete(&self) -> bool {
        self.tool_calls.iter().all(|tc| tc.has_result())
    }

    /// Get a summary of tool call statuses for display.
    pub fn tool_summary(&self) -> (usize, usize, usize) {
        let mut success = 0;
        let mut error = 0;
        let mut pending = 0;
        for tc in &self.tool_calls {
            match tc.effective_status() {
                ToolStatus::Success => success += 1,
                ToolStatus::Error => error += 1,
                ToolStatus::Pending => pending += 1,
            }
        }
        (success, error, pending)
    }
}

// ============================================
// Lucide SVG Icons (16x16, stroke-width: 2)
// ============================================

/// User icon - for user messages
const ICON_USER: &str = r#"<svg class="lucide-icon" xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M19 21v-2a4 4 0 0 0-4-4H9a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/></svg>"#;

/// Bot icon - for assistant messages
const ICON_BOT: &str = r#"<svg class="lucide-icon" xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 8V4H8"/><rect width="16" height="12" x="4" y="8" rx="2"/><path d="M2 14h2"/><path d="M20 14h2"/><path d="M15 13v2"/><path d="M9 13v2"/></svg>"#;

/// Wrench icon - for tool messages
const ICON_WRENCH: &str = r#"<svg class="lucide-icon" xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"/></svg>"#;

/// Settings icon - for system messages
const ICON_SETTINGS: &str = r#"<svg class="lucide-icon" xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/></svg>"#;

/// Message square icon - fallback
const ICON_MESSAGE: &str = r#"<svg class="lucide-icon" xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/></svg>"#;

/// Terminal icon - for bash/shell
const ICON_TERMINAL: &str = r#"<svg class="lucide-icon" xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="4 17 10 11 4 5"/><line x1="12" x2="20" y1="19" y2="19"/></svg>"#;

/// File text icon - for read
const ICON_FILE_TEXT: &str = r#"<svg class="lucide-icon" xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a2 2 0 0 0 2 2h4"/><path d="M10 9H8"/><path d="M16 13H8"/><path d="M16 17H8"/></svg>"#;

/// Pencil icon - for write/edit
const ICON_PENCIL: &str = r#"<svg class="lucide-icon" xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z"/><path d="m15 5 4 4"/></svg>"#;

/// Search icon - for glob/grep/search
const ICON_SEARCH: &str = r#"<svg class="lucide-icon" xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/></svg>"#;

/// Globe icon - for web fetch
const ICON_GLOBE: &str = r#"<svg class="lucide-icon" xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"/><path d="M2 12h20"/></svg>"#;

/// Check icon - for success status
const ICON_CHECK: &str = r#"<svg class="lucide-icon" xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg>"#;

/// X icon - for error status
const ICON_X: &str = r#"<svg class="lucide-icon" xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>"#;

/// Loader icon - for pending status
const ICON_LOADER: &str = r#"<svg class="lucide-icon lucide-spin" xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 2v4"/><path d="m16.2 7.8 2.9-2.9"/><path d="M18 12h4"/><path d="m16.2 16.2 2.9 2.9"/><path d="M12 18v4"/><path d="m4.9 19.1 2.9-2.9"/><path d="M2 12h4"/><path d="m4.9 4.9 2.9 2.9"/></svg>"#;

/// Mail icon - for MCP agent mail
const ICON_MAIL: &str = r#"<svg class="lucide-icon" xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="20" height="16" x="2" y="4" rx="2"/><path d="m22 7-8.97 5.7a1.94 1.94 0 0 1-2.06 0L2 7"/></svg>"#;

/// Database icon - for data operations
const ICON_DATABASE: &str = r#"<svg class="lucide-icon" xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5V19A9 3 0 0 0 21 19V5"/><path d="M3 12A9 3 0 0 0 21 12"/></svg>"#;

/// Sparkles icon - for AI/task operations
const ICON_SPARKLES: &str = r#"<svg class="lucide-icon" xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9.937 15.5A2 2 0 0 0 8.5 14.063l-6.135-1.582a.5.5 0 0 1 0-.962L8.5 9.936A2 2 0 0 0 9.937 8.5l1.582-6.135a.5.5 0 0 1 .963 0L14.063 8.5A2 2 0 0 0 15.5 9.937l6.135 1.581a.5.5 0 0 1 0 .964L15.5 14.063a2 2 0 0 0-1.437 1.437l-1.582 6.135a.5.5 0 0 1-.963 0z"/><path d="M20 3v4"/><path d="M22 5h-4"/><path d="M4 17v2"/><path d="M5 18H3"/></svg>"#;

/// Get the CSS class for an agent slug.
///
/// Maps agent identifiers to their visual styling class.
pub fn agent_css_class(slug: &str) -> &'static str {
    match slug {
        "claude_code" | "claude" => "agent-claude",
        "codex" | "codex_cli" => "agent-codex",
        "cursor" | "cursor_ai" => "agent-cursor",
        "chatgpt" | "openai" => "agent-chatgpt",
        "gemini" | "google" => "agent-gemini",
        "aider" => "agent-aider",
        "copilot" | "github_copilot" => "agent-copilot",
        "cody" | "sourcegraph" => "agent-cody",
        "windsurf" => "agent-windsurf",
        "amp" => "agent-amp",
        "grok" => "agent-grok",
        _ => "agent-default",
    }
}

/// Get human-readable agent name.
pub fn agent_display_name(slug: &str) -> &'static str {
    match slug {
        "claude_code" | "claude" => "Claude",
        "codex" | "codex_cli" => "Codex",
        "cursor" | "cursor_ai" => "Cursor",
        "chatgpt" | "openai" => "ChatGPT",
        "gemini" | "google" => "Gemini",
        "aider" => "Aider",
        "copilot" | "github_copilot" => "GitHub Copilot",
        "cody" | "sourcegraph" => "Cody",
        "windsurf" => "Windsurf",
        "amp" => "Amp",
        "grok" => "Grok",
        _ => "AI Assistant",
    }
}

/// Render a list of messages to HTML.
pub fn render_conversation(
    messages: &[Message],
    options: &RenderOptions,
) -> Result<String, RenderError> {
    let started = Instant::now();
    let mut html = String::with_capacity(messages.len() * 2000);

    // Add agent-specific class to conversation wrapper if specified
    let agent_class = options
        .agent_slug
        .as_ref()
        .map(|s| agent_css_class(s))
        .unwrap_or("");

    info!(
        component = "renderer",
        operation = "render_conversation",
        message_count = messages.len(),
        agent_slug = options.agent_slug.as_deref().unwrap_or(""),
        "Rendering conversation"
    );

    if !agent_class.is_empty() {
        html.push_str(&format!(
            r#"<div class="conversation-messages {}">"#,
            agent_class
        ));
        html.push('\n');
    }

    for (idx, message) in messages.iter().enumerate() {
        // Allow message to have its own index, or use enumeration
        let msg_with_index = if message.index.is_some() {
            message.clone()
        } else {
            let mut m = message.clone();
            m.index = Some(idx);
            m
        };
        html.push_str(&render_message(&msg_with_index, options)?);
        html.push('\n');
    }

    if !agent_class.is_empty() {
        html.push_str("</div>\n");
    }

    debug!(
        component = "renderer",
        operation = "render_conversation_complete",
        duration_ms = started.elapsed().as_millis(),
        bytes = html.len(),
        "Conversation rendered"
    );

    Ok(html)
}

// ============================================================================
// MessageGroup Rendering (Consolidated Tool Calls)
// ============================================================================

/// Maximum number of tool badges to show before overflow indicator.
const MAX_VISIBLE_BADGES: usize = 6;

/// Render a list of message groups to HTML (consolidated rendering).
///
/// This is the preferred rendering method when messages have been grouped
/// via `group_messages_for_export()`. Each group renders as a single article
/// with all associated tool calls shown as compact badges.
pub fn render_message_groups(
    groups: &[MessageGroup],
    options: &RenderOptions,
) -> Result<String, RenderError> {
    let started = Instant::now();
    let mut html = String::with_capacity(groups.len() * 3000);

    // Add agent-specific class to conversation wrapper if specified
    let agent_class = options
        .agent_slug
        .as_ref()
        .map(|s| agent_css_class(s))
        .unwrap_or("");

    info!(
        component = "renderer",
        operation = "render_message_groups",
        group_count = groups.len(),
        agent_slug = options.agent_slug.as_deref().unwrap_or(""),
        "Rendering conversation from message groups"
    );

    if !agent_class.is_empty() {
        html.push_str(&format!(
            r#"<div class="conversation-messages {}">"#,
            agent_class
        ));
        html.push('\n');
    }

    for (idx, group) in groups.iter().enumerate() {
        html.push_str(&render_message_group(group, idx, options)?);
        html.push('\n');
    }

    if !agent_class.is_empty() {
        html.push_str("</div>\n");
    }

    debug!(
        component = "renderer",
        operation = "render_message_groups_complete",
        duration_ms = started.elapsed().as_millis(),
        bytes = html.len(),
        groups = groups.len(),
        "Message groups rendered"
    );

    Ok(html)
}

/// Render a single message group to HTML.
///
/// A message group consists of:
/// - A primary message (user/assistant/system)
/// - Zero or more associated tool calls with their results
///
/// The output is a single `<article>` element with tool badges in the header.
fn render_message_group(
    group: &MessageGroup,
    index: usize,
    options: &RenderOptions,
) -> Result<String, RenderError> {
    let started = Instant::now();
    trace!(
        component = "renderer",
        operation = "render_message_group",
        index = index,
        group_type = ?group.group_type,
        tool_count = group.tool_count(),
        "Rendering message group"
    );

    // Role class based on group type
    let role_class = match group.group_type {
        MessageGroupType::User => "message-user",
        MessageGroupType::Assistant => "message-assistant",
        MessageGroupType::System => "message-system",
        MessageGroupType::ToolOnly => "message-tool",
    };

    // Role icon
    let role_icon = match group.group_type {
        MessageGroupType::User => ICON_USER,
        MessageGroupType::Assistant => ICON_BOT,
        MessageGroupType::System => ICON_SETTINGS,
        MessageGroupType::ToolOnly => ICON_WRENCH,
    };

    // Author display
    let author_display = group
        .primary
        .author
        .as_ref()
        .map(|a| super::template::html_escape(a))
        .unwrap_or_else(|| match group.group_type {
            MessageGroupType::User => "You".to_string(),
            MessageGroupType::Assistant => "Assistant".to_string(),
            MessageGroupType::System => "System".to_string(),
            MessageGroupType::ToolOnly => "Tool".to_string(),
        });

    // Message anchor
    let anchor_id = group
        .primary
        .index
        .or(Some(index))
        .map(|idx| format!(r#" id="msg-{}""#, idx))
        .unwrap_or_default();

    // Timestamp
    let timestamp_html = if options.show_timestamps {
        if let Some(ts) = &group.start_timestamp {
            format!(
                r#"<time class="message-time" datetime="{}">{}</time>"#,
                super::template::html_escape(ts),
                super::template::html_escape(&format_timestamp(ts))
            )
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Render content
    let content_html = render_content(&group.primary.content, options);

    // Render tool badges with overflow handling
    let (tool_badges_html, overflow_count) =
        if options.show_tool_calls && !group.tool_calls.is_empty() {
            render_tool_badges_with_overflow(&group.tool_calls, options)
        } else {
            (String::new(), 0)
        };

    // ARIA label for the article
    let aria_label = if group.tool_calls.is_empty() {
        format!("{} message", group.group_type.role_icon())
    } else {
        format!(
            "{} message with {} tool call{}",
            group.group_type.role_icon(),
            group.tool_calls.len(),
            if group.tool_calls.len() == 1 { "" } else { "s" }
        )
    };

    // Check for content collapse
    let (content_wrapper_start, content_wrapper_end) = if options.collapse_threshold > 0
        && group.primary.content.len() > options.collapse_threshold
    {
        let preview_len = options.collapse_threshold.min(500);
        let safe_len = truncate_to_char_boundary(&group.primary.content, preview_len);
        let preview = &group.primary.content[..safe_len];
        (
            format!(
                r#"<details class="message-collapse">
                    <summary>
                        <span class="message-preview">{}</span>
                        <span class="message-expand-hint">Click to expand ({} chars)</span>
                    </summary>
                    <div class="message-expanded">"#,
                super::template::html_escape(preview),
                group.primary.content.len()
            ),
            "</div></details>".to_string(),
        )
    } else {
        (String::new(), String::new())
    };

    // Only render content div if there's actual content
    let content_section = if content_html.trim().is_empty() {
        String::new()
    } else {
        format!(
            r#"
                <div class="message-content">
                    {wrapper_start}{content}{wrapper_end}
                </div>"#,
            wrapper_start = content_wrapper_start,
            content = content_html,
            wrapper_end = content_wrapper_end,
        )
    };

    // Tool badges container with accessibility
    let tool_container = if !tool_badges_html.is_empty() {
        format!(
            r#"<div class="message-header-right" role="group" aria-label="Tool calls{}">
                        {badges}
                    </div>"#,
            if overflow_count > 0 {
                format!(" ({} shown, {} more)", MAX_VISIBLE_BADGES, overflow_count)
            } else {
                String::new()
            },
            badges = tool_badges_html,
        )
    } else {
        r#"<div class="message-header-right"></div>"#.to_string()
    };

    let rendered = format!(
        r#"            <article class="message {role_class}"{anchor} role="article" aria-label="{aria_label}">
                <header class="message-header">
                    <div class="message-header-left">
                        <span class="message-icon" aria-hidden="true">{role_icon}</span>
                        <span class="message-author">{author}</span>
                        {timestamp}
                    </div>
                    {tool_container}
                </header>{content_section}
            </article>"#,
        role_class = role_class,
        anchor = anchor_id,
        aria_label = super::template::html_escape(&aria_label),
        role_icon = role_icon,
        author = author_display,
        timestamp = timestamp_html,
        tool_container = tool_container,
        content_section = content_section,
    );

    debug!(
        component = "renderer",
        operation = "render_message_group_complete",
        index = index,
        duration_ms = started.elapsed().as_millis(),
        bytes = rendered.len(),
        "Message group rendered"
    );

    Ok(rendered)
}

/// Render tool badges with overflow handling.
///
/// When there are more than `MAX_VISIBLE_BADGES` tool calls, shows the first N
/// badges plus a "+X more" overflow indicator.
fn render_tool_badges_with_overflow(
    tools: &[ToolCallWithResult],
    _options: &RenderOptions,
) -> (String, usize) {
    if tools.is_empty() {
        return (String::new(), 0);
    }

    if tools.len() <= MAX_VISIBLE_BADGES {
        // Render all badges
        let badges: String = tools
            .iter()
            .map(render_single_tool_badge)
            .collect::<Vec<_>>()
            .join("\n                        ");
        (badges, 0)
    } else {
        // Render first N badges + overflow indicator
        let visible: String = tools[..MAX_VISIBLE_BADGES]
            .iter()
            .map(render_single_tool_badge)
            .collect::<Vec<_>>()
            .join("\n                        ");

        let overflow_count = tools.len() - MAX_VISIBLE_BADGES;
        let overflow_badge = format!(
            r#"<button class="tool-badge tool-overflow"
                    aria-label="{count} more tool{s}"
                    aria-expanded="false"
                    data-overflow-count="{count}">
                <span class="tool-badge-text">+{count}</span>
            </button>"#,
            count = overflow_count,
            s = if overflow_count == 1 { "" } else { "s" },
        );

        (
            format!("{}\n                        {}", visible, overflow_badge),
            overflow_count,
        )
    }
}

/// Render a single tool badge as a button with Lucide SVG icon.
fn render_single_tool_badge(tool: &ToolCallWithResult) -> String {
    let icon = get_tool_lucide_icon(&tool.call.name);
    let status = tool.effective_status();
    let status_class = status.css_class();
    let status_label = status.label();
    let status_icon = status.icon_svg();

    // Escape input for data attribute
    let input_preview = if tool.call.input.len() > 200 {
        let safe_len = truncate_to_char_boundary(&tool.call.input, 200);
        format!("{}...", &tool.call.input[..safe_len])
    } else {
        tool.call.input.clone()
    };

    // Build output preview for popover
    let output_preview = tool
        .result
        .as_ref()
        .map(|r| {
            if r.content.len() > 200 {
                let safe_len = truncate_to_char_boundary(&r.content, 200);
                format!("{}...", &r.content[..safe_len])
            } else {
                r.content.clone()
            }
        })
        .unwrap_or_default();

    format!(
        r#"<button class="tool-badge {status_class}"
                aria-label="{name}: {status_label}"
                aria-expanded="false"
                data-tool-name="{name}"
                data-tool-input="{input}"
                data-tool-output="{output}">
            <span class="tool-badge-icon">{icon}</span>
            <span class="tool-badge-name">{name}</span>
            <span class="tool-badge-status">{status_icon}</span>
        </button>"#,
        status_class = status_class,
        name = super::template::html_escape(&tool.call.name),
        status_label = status_label,
        icon = icon,
        input = super::template::html_escape(&input_preview),
        output = super::template::html_escape(&output_preview),
        status_icon = status_icon,
    )
}

/// Get the appropriate Lucide SVG icon for a tool by name.
fn get_tool_lucide_icon(tool_name: &str) -> &'static str {
    match tool_name.to_lowercase().as_str() {
        "bash" | "shell" | "terminal" => ICON_TERMINAL,
        "read" | "read_file" | "readfile" => ICON_FILE_TEXT,
        "write" | "write_file" | "writefile" | "edit" => ICON_PENCIL,
        "glob" | "find" | "grep" | "search" | "websearch" => ICON_SEARCH,
        "webfetch" | "fetch" | "http" | "curl" => ICON_GLOBE,
        "task" | "agent" => ICON_SPARKLES,
        n if n.starts_with("mcp__mcp-agent-mail") => ICON_MAIL,
        n if n.contains("sql") || n.contains("db") || n.contains("database") => ICON_DATABASE,
        _ => ICON_WRENCH,
    }
}

/// Render a single message to HTML.
pub fn render_message(message: &Message, options: &RenderOptions) -> Result<String, RenderError> {
    let started = Instant::now();
    trace!(
        component = "renderer",
        operation = "render_message",
        message_index = message.index.unwrap_or(0),
        has_index = message.index.is_some(),
        role = message.role.as_str(),
        content_len = message.content.len(),
        "Rendering message"
    );

    // Role class for semantic styling (matches styles.rs)
    let role_class = match message.role.as_str() {
        "user" => "message-user",
        "assistant" | "agent" => "message-assistant",
        "tool" => "message-tool",
        "system" => "message-system",
        _ => "",
    };

    // Message anchor for deep linking
    let anchor_id = message
        .index
        .map(|idx| format!(r#" id="msg-{}""#, idx))
        .unwrap_or_default();

    // Author display (falls back to role)
    let author_display = message
        .author
        .as_ref()
        .map(|a| html_escape(a))
        .unwrap_or_else(|| format_role_display(&message.role));

    let timestamp_html = if options.show_timestamps {
        if let Some(ts) = &message.timestamp {
            format!(
                r#"<time class="message-time" datetime="{}">{}</time>"#,
                html_escape(ts),
                html_escape(&format_timestamp(ts))
            )
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let content_html = render_content(&message.content, options);

    // Check if message should be collapsed
    let (content_wrapper_start, content_wrapper_end) =
        if options.collapse_threshold > 0 && message.content.len() > options.collapse_threshold {
            debug!(
                component = "renderer",
                operation = "collapse_message",
                message_index = message.index.unwrap_or(0),
                content_len = message.content.len(),
                collapse_threshold = options.collapse_threshold,
                "Collapsing long message"
            );
            let preview_len = options.collapse_threshold.min(500);
            // Safe truncation at char boundary to avoid panic on multi-byte UTF-8
            let safe_len = truncate_to_char_boundary(&message.content, preview_len);
            let preview = &message.content[..safe_len];
            (
                format!(
                    r#"<details class="message-collapse">
                    <summary>
                        <span class="message-preview">{}</span>
                        <span class="message-expand-hint">Click to expand ({} chars)</span>
                    </summary>
                    <div class="message-expanded">"#,
                    html_escape(preview),
                    message.content.len()
                ),
                "</div></details>".to_string(),
            )
        } else {
            (String::new(), String::new())
        };

    // Tool badges rendered as compact icons in header (upper-right)
    let tool_badges_html = if options.show_tool_calls {
        if let Some(tc) = &message.tool_call {
            render_tool_badge(tc, options)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Role icon for visual differentiation - using Lucide SVG icons
    let role_icon = match message.role.as_str() {
        "user" => ICON_USER,
        "assistant" | "agent" => ICON_BOT,
        "tool" => ICON_WRENCH,
        "system" => ICON_SETTINGS,
        _ => ICON_MESSAGE,
    };

    // Only render content div if there's actual content
    let content_section = if content_html.trim().is_empty() {
        String::new()
    } else {
        format!(
            r#"
                <div class="message-content">
                    {wrapper_start}{content}{wrapper_end}
                </div>"#,
            wrapper_start = content_wrapper_start,
            content = content_html,
            wrapper_end = content_wrapper_end,
        )
    };

    let rendered = format!(
        r#"            <article class="message {role_class}"{anchor} role="article" aria-label="{role} message">
                <header class="message-header">
                    <div class="message-header-left">
                        <span class="message-icon" aria-hidden="true">{role_icon}</span>
                        <span class="message-author">{author}</span>
                        {timestamp}
                    </div>
                    <div class="message-header-right">
                        {tool_badges}
                    </div>
                </header>{content_section}
            </article>"#,
        role_class = role_class,
        anchor = anchor_id,
        role = html_escape(&message.role),
        role_icon = role_icon,
        author = author_display,
        timestamp = timestamp_html,
        content_section = content_section,
        tool_badges = tool_badges_html,
    );

    debug!(
        component = "renderer",
        operation = "render_message_complete",
        message_index = message.index.unwrap_or(0),
        duration_ms = started.elapsed().as_millis(),
        bytes = rendered.len(),
        "Message rendered"
    );

    Ok(rendered)
}

/// Format role for display.
fn format_role_display(role: &str) -> String {
    match role {
        "user" => "You".to_string(),
        "assistant" | "agent" => "Assistant".to_string(),
        "tool" => "Tool".to_string(),
        "system" => "System".to_string(),
        other => other.to_string(),
    }
}

/// Render message content, converting markdown to HTML using pulldown-cmark.
/// Raw HTML in the input is escaped for security (XSS prevention).
fn render_content(content: &str, _options: &RenderOptions) -> String {
    use pulldown_cmark::Event;

    // Configure pulldown-cmark with all common extensions
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_SMART_PUNCTUATION);

    // Parse markdown and filter out raw HTML for security
    let parser = Parser::new_ext(content, opts).map(|event| match event {
        // Convert raw HTML to escaped text (XSS prevention)
        Event::Html(html) => Event::Text(html),
        Event::InlineHtml(html) => Event::Text(html),
        // Pass through all other events
        other => other,
    });

    let mut html_output = String::new();
    html::push_html(&mut html_output, parser);

    html_output
}

/// Render a code block with optional syntax highlighting.
#[allow(dead_code)]
fn render_code_block(content: &str, lang: &str, options: &RenderOptions) -> String {
    trace!(
        component = "renderer",
        operation = "render_code_block",
        language = lang,
        lines = content.lines().count(),
        content_len = content.len(),
        "Rendering code block"
    );
    let lang_class = if options.syntax_highlighting && !lang.is_empty() {
        format!(r#" class="language-{}""#, html_escape(lang))
    } else {
        String::new()
    };

    let wrap_class = if options.wrap_code {
        r#" style="white-space: pre-wrap;""#
    } else {
        ""
    };

    format!(
        r#"<pre{wrap}><code{lang}>{}</code></pre>"#,
        html_escape(content),
        wrap = wrap_class,
        lang = lang_class,
    )
}

/// Render inline code (backticks).
#[allow(dead_code)]
fn render_inline_code(text: &str) -> String {
    let mut result = String::new();
    let chars = text.chars();
    let mut in_code = false;
    let mut code = String::new();

    for c in chars {
        if c == '`' {
            if in_code {
                result.push_str("<code>");
                result.push_str(&code);
                result.push_str("</code>");
                code.clear();
                in_code = false;
            } else {
                in_code = true;
            }
        } else if in_code {
            code.push(c);
        } else {
            result.push(c);
        }
    }

    // Handle unclosed inline code
    if in_code {
        result.push('`');
        result.push_str(&code);
    }

    result
}

/// Render URLs as clickable links.
///
/// NOTE: This function expects already HTML-escaped text as input (from render_content).
/// The URL is NOT re-escaped since it's already safe. The browser will decode HTML
/// entities in href attributes after parsing, so `&amp;` becomes `&` in the actual URL.
#[allow(dead_code)]
fn render_links(text: &str) -> String {
    // Simple URL detection - matches http:// and https://
    let mut result = String::new();
    let mut chars = text.chars().peekable();
    let mut buffer = String::new();

    while let Some(c) = chars.next() {
        buffer.push(c);

        // Check for URL pattern
        if buffer.ends_with("http://") || buffer.ends_with("https://") {
            // Found URL start, capture the rest
            let prefix = if buffer.ends_with("https://") {
                "https://"
            } else {
                "http://"
            };

            result.push_str(&buffer[..buffer.len() - prefix.len()]);

            let mut url = prefix.to_string();
            while let Some(&next) = chars.peek() {
                // Stop at whitespace. Note: raw <, >, " would already be escaped
                // to &lt;, &gt;, &quot; at this point, so we only check whitespace.
                if next.is_whitespace() {
                    break;
                }
                url.push(chars.next().unwrap());
            }

            // URL is already HTML-escaped (from the earlier html_escape call in render_content).
            // Do NOT re-escape, or &amp; becomes &amp;amp; (broken links).
            result.push_str(&format!(
                r#"<a href="{url}" target="_blank" rel="noopener noreferrer">{url}</a>"#,
                url = url
            ));

            buffer.clear();
        }
    }

    result.push_str(&buffer);
    result
}

/// Render a compact tool badge with hover popover for the message header.
fn render_tool_badge(tool_call: &ToolCall, options: &RenderOptions) -> String {
    let started = Instant::now();
    trace!(
        component = "renderer",
        operation = "render_tool_badge",
        tool = tool_call.name.as_str(),
        input_len = tool_call.input.len(),
        output_len = tool_call.output.as_ref().map(|s| s.len()).unwrap_or(0),
        "Rendering tool badge"
    );

    // Status indicator - get CSS class and SVG icon
    let (status_class, status_icon_svg, status_label) = tool_call
        .status
        .as_ref()
        .map(|s| (s.css_class(), s.icon_svg(), s.label()))
        .unwrap_or(("", "", ""));

    // Format input as pretty JSON if possible
    let formatted_input = format_json_or_raw(&tool_call.input);

    // Tool icon based on name - using Lucide SVG icons
    let tool_icon = match tool_call.name.to_lowercase().as_str() {
        "bash" | "shell" => ICON_TERMINAL,
        "read" | "read_file" => ICON_FILE_TEXT,
        "write" | "write_file" | "edit" => ICON_PENCIL,
        "glob" | "find" | "grep" | "search" | "websearch" => ICON_SEARCH,
        "webfetch" | "fetch" | "http" => ICON_GLOBE,
        "task" => ICON_SPARKLES,
        n if n.starts_with("mcp__mcp-agent-mail") => ICON_MAIL,
        n if n.contains("sql") || n.contains("db") => ICON_DATABASE,
        _ => ICON_WRENCH,
    };

    // Suppress unused warning for options - may be used for future customization
    let _ = options;

    // Truncate input/output for popover display
    let input_preview = if formatted_input.len() > 500 {
        let safe_len = truncate_to_char_boundary(&formatted_input, 500);
        format!("{}…", &formatted_input[..safe_len])
    } else {
        formatted_input.clone()
    };

    let output_preview = if let Some(output) = &tool_call.output {
        let formatted = format_json_or_raw(output);
        if formatted.len() > 500 {
            let safe_len = truncate_to_char_boundary(&formatted, 500);
            format!("{}…", &formatted[..safe_len])
        } else {
            formatted
        }
    } else {
        String::new()
    };

    // Build popover content
    let popover_input = if !input_preview.trim().is_empty() {
        format!(
            r#"<div class="tool-popover-section"><span class="tool-popover-label">Input</span><pre><code>{}</code></pre></div>"#,
            html_escape(&input_preview)
        )
    } else {
        String::new()
    };

    let popover_output = if !output_preview.is_empty() {
        format!(
            r#"<div class="tool-popover-section"><span class="tool-popover-label">Output</span><pre><code>{}</code></pre></div>"#,
            html_escape(&output_preview)
        )
    } else {
        String::new()
    };

    // Compact badge with hover popover - using SVG icons
    let rendered = format!(
        r#"<span class="tool-badge {status_class}" tabindex="0" role="button" aria-label="{name} tool call">
            <span class="tool-badge-icon">{icon}</span>
            <span class="tool-badge-name">{name}</span>
            {status_badge}
            <div class="tool-popover" role="tooltip">
                <div class="tool-popover-header">{icon} <span>{name}</span> {status_badge}</div>
                {input}{output}
            </div>
        </span>"#,
        icon = tool_icon,
        name = html_escape(&tool_call.name),
        status_class = status_class,
        status_badge = if !status_label.is_empty() {
            format!(
                r#"<span class="tool-badge-status {}">{}</span>"#,
                status_label, status_icon_svg
            )
        } else {
            String::new()
        },
        input = popover_input,
        output = popover_output,
    );

    debug!(
        component = "renderer",
        operation = "render_tool_badge_complete",
        tool = tool_call.name.as_str(),
        duration_ms = started.elapsed().as_millis(),
        bytes = rendered.len(),
        "Tool call rendered"
    );

    rendered
}

/// Try to format as pretty JSON, otherwise return raw.
fn format_json_or_raw(s: &str) -> String {
    // Try to parse as JSON and pretty print
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(s)
        && let Ok(pretty) = serde_json::to_string_pretty(&value)
    {
        return pretty;
    }
    s.to_string()
}

/// Format a timestamp for display.
fn format_timestamp(ts: &str) -> String {
    // Simple formatting - could be enhanced with chrono
    // For now, just return a shortened version
    if ts.len() > 19 {
        // Safe truncation at char boundary
        let end = truncate_to_char_boundary(ts, 19);
        ts[..end].replace('T', " ")
    } else {
        ts.to_string()
    }
}

/// Find the largest byte index <= `max_bytes` that is on a UTF-8 char boundary.
fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> usize {
    if max_bytes >= s.len() {
        return s.len();
    }
    // Walk backwards from max_bytes to find a char boundary
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_message(role: &str, content: &str) -> Message {
        Message {
            role: role.to_string(),
            content: content.to_string(),
            timestamp: None,
            tool_call: None,
            index: None,
            author: None,
        }
    }

    #[test]
    fn test_render_message_user() {
        let msg = test_message("user", "Hello, world!");
        let opts = RenderOptions::default();
        let html = render_message(&msg, &opts).unwrap();

        assert!(html.contains("message-user"));
        assert!(html.contains("Hello, world!"));
        assert!(html.contains("lucide-icon")); // SVG Lucide icon
        assert!(html.contains("M19 21v-2")); // User icon path
    }

    #[test]
    fn test_render_message_with_code() {
        let msg = test_message("assistant", "Here's code:\n```rust\nfn main() {}\n```");
        let opts = RenderOptions {
            syntax_highlighting: true,
            ..Default::default()
        };
        let html = render_message(&msg, &opts).unwrap();

        assert!(html.contains("<pre>"));
        assert!(html.contains("language-rust"));
        assert!(html.contains("fn main()"));
        assert!(html.contains("lucide-icon")); // SVG Lucide icon (bot)
    }

    #[test]
    fn test_render_inline_code() {
        let result = render_inline_code("Use `println!` to print");
        assert!(result.contains("<code>println!</code>"));
    }

    #[test]
    fn test_render_links() {
        let result = render_links("Visit https://example.com for more");
        assert!(result.contains(r#"<a href="https://example.com""#));
        assert!(result.contains("target=\"_blank\""));
    }

    #[test]
    fn test_url_with_query_params_not_double_escaped() {
        // Test that URLs with & in query params are correctly escaped once, not twice.
        // The render_content function HTML-escapes first, then render_links processes.
        // If render_links re-escapes, &amp; becomes &amp;amp; (broken).
        let msg = test_message("user", "Visit https://example.com?a=1&b=2 for info");
        let html = render_message(&msg, &RenderOptions::default()).unwrap();

        // Should contain &amp; (single escape), NOT &amp;amp; (double escape)
        assert!(
            html.contains("https://example.com?a=1&amp;b=2"),
            "URL should have single-escaped ampersand. HTML: {}",
            html
        );
        assert!(
            !html.contains("&amp;amp;"),
            "URL should NOT be double-escaped. HTML: {}",
            html
        );
    }

    #[test]
    fn test_html_escape_in_content() {
        let msg = test_message("user", "<script>alert('xss')</script>");
        let html = render_message(&msg, &RenderOptions::default()).unwrap();
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn test_agent_css_class() {
        assert_eq!(agent_css_class("claude_code"), "agent-claude");
        assert_eq!(agent_css_class("codex"), "agent-codex");
        assert_eq!(agent_css_class("cursor"), "agent-cursor");
        assert_eq!(agent_css_class("gemini"), "agent-gemini");
        assert_eq!(agent_css_class("unknown"), "agent-default");
    }

    #[test]
    fn test_agent_display_name() {
        assert_eq!(agent_display_name("claude_code"), "Claude");
        assert_eq!(agent_display_name("codex"), "Codex");
        assert_eq!(agent_display_name("github_copilot"), "GitHub Copilot");
        assert_eq!(agent_display_name("unknown"), "AI Assistant");
    }

    #[test]
    fn test_tool_status_rendering() {
        let msg = Message {
            role: "tool".to_string(),
            content: "Tool executed".to_string(),
            timestamp: None,
            tool_call: Some(ToolCall {
                name: "Bash".to_string(),
                input: r#"{"command": "ls -la"}"#.to_string(),
                output: Some("file1.txt\nfile2.txt".to_string()),
                status: Some(ToolStatus::Success),
            }),
            index: None,
            author: None,
        };

        let html = render_message(&msg, &RenderOptions::default()).unwrap();
        assert!(html.contains("tool-status-success"));
        assert!(html.contains("lucide-icon")); // SVG icon
        assert!(html.contains("M20 6 9 17l-5-5")); // Check icon path (success)
        assert!(html.contains("polyline points=\"4 17 10 11 4 5\"")); // Terminal icon path (bash)
    }

    #[test]
    fn test_message_with_index() {
        let msg = Message {
            role: "user".to_string(),
            content: "Test message".to_string(),
            timestamp: None,
            tool_call: None,
            index: Some(42),
            author: None,
        };

        let html = render_message(&msg, &RenderOptions::default()).unwrap();
        assert!(html.contains(r#"id="msg-42""#));
    }

    #[test]
    fn test_message_with_author() {
        let msg = Message {
            role: "user".to_string(),
            content: "Test message".to_string(),
            timestamp: None,
            tool_call: None,
            index: None,
            author: Some("Alice".to_string()),
        };

        let html = render_message(&msg, &RenderOptions::default()).unwrap();
        assert!(html.contains("Alice"));
    }

    #[test]
    fn test_conversation_with_agent_class() {
        let messages = vec![test_message("user", "Hello")];
        let opts = RenderOptions {
            agent_slug: Some("claude_code".to_string()),
            ..Default::default()
        };

        let html = render_conversation(&messages, &opts).unwrap();
        assert!(html.contains("agent-claude"));
    }

    #[test]
    fn test_format_json_or_raw() {
        // Valid JSON gets pretty printed
        let json_input = r#"{"key":"value"}"#;
        let formatted = format_json_or_raw(json_input);
        assert!(formatted.contains('\n')); // Pretty printed has newlines

        // Invalid JSON passes through unchanged
        let raw_input = "not json at all";
        let formatted = format_json_or_raw(raw_input);
        assert_eq!(formatted, raw_input);
    }

    #[test]
    fn test_long_message_collapse() {
        let long_content = "x".repeat(2000);
        let msg = test_message("user", &long_content);
        let opts = RenderOptions {
            collapse_threshold: 1000,
            ..Default::default()
        };

        let html = render_message(&msg, &opts).unwrap();
        assert!(html.contains("<details"));
        assert!(html.contains("Click to expand"));
    }

    #[test]
    fn test_tool_icons_for_different_tools() {
        // Check that different tools get appropriate Lucide SVG icons
        let tools_and_svg_markers = vec![
            ("Read", "M15 2H6a2 2 0 0 0-2 2v16"), // FileText icon path
            ("Write", "M21.174 6.812"),           // Pencil icon path
            ("Bash", "polyline points=\"4 17 10 11 4 5\""), // Terminal icon
            ("Grep", "circle cx=\"11\" cy=\"11\" r=\"8\""), // Search icon
            ("WebFetch", "circle cx=\"12\" cy=\"12\" r=\"10\""), // Globe icon
        ];

        for (tool_name, svg_marker) in tools_and_svg_markers {
            let tc = ToolCall {
                name: tool_name.to_string(),
                input: "{}".to_string(),
                output: None,
                status: None,
            };
            let html = render_tool_badge(&tc, &RenderOptions::default());
            assert!(
                html.contains("lucide-icon"),
                "Tool {} should have lucide-icon class",
                tool_name
            );
            assert!(
                html.contains(svg_marker),
                "Tool {} should have SVG marker '{}', got: {}",
                tool_name,
                svg_marker,
                html
            );
        }
    }

    // ========================================================================
    // UTF-8 boundary safety tests
    // ========================================================================

    #[test]
    fn test_truncate_to_char_boundary() {
        // ASCII string
        assert_eq!(truncate_to_char_boundary("hello", 3), 3);
        assert_eq!(truncate_to_char_boundary("hello", 10), 5);

        // UTF-8 multi-byte characters
        // "日本語" = 3 chars, 9 bytes (each char is 3 bytes)
        let japanese = "日本語";
        assert_eq!(japanese.len(), 9);
        // Truncating at byte 4 should back up to byte 3 (end of first char)
        assert_eq!(truncate_to_char_boundary(japanese, 4), 3);
        // Truncating at byte 6 should stay at 6 (end of second char)
        assert_eq!(truncate_to_char_boundary(japanese, 6), 6);
    }

    #[test]
    fn test_long_message_collapse_utf8_safe() {
        // Create a message with multi-byte UTF-8 content that would panic if sliced incorrectly
        let content_with_emoji = "This is a message with emoji 🎉🎊🎈 ".repeat(50);
        let msg = test_message("user", &content_with_emoji);
        let opts = RenderOptions {
            collapse_threshold: 100,
            ..Default::default()
        };

        // Should not panic even though the emoji may be at the slice boundary
        let html = render_message(&msg, &opts).unwrap();
        assert!(html.contains("<details"));
        // The preview should be valid UTF-8 (this would fail if we sliced incorrectly)
        assert!(!html.is_empty());
    }

    #[test]
    fn test_tool_output_truncation_utf8_safe() {
        // Create a very long tool output with multi-byte chars
        let long_output_with_unicode = "结果: ".repeat(5000); // Chinese characters

        let msg = Message {
            role: "tool".to_string(),
            content: "Tool result".to_string(),
            timestamp: None,
            tool_call: Some(ToolCall {
                name: "Test".to_string(),
                input: "{}".to_string(),
                output: Some(long_output_with_unicode),
                status: Some(ToolStatus::Success),
            }),
            index: None,
            author: None,
        };

        // Should not panic even though we're truncating the long output
        // The new badge format truncates at 500 chars with ellipsis
        let html = render_message(&msg, &RenderOptions::default()).unwrap();
        // Verify we have a tool badge (new compact format)
        assert!(html.contains("tool-badge"));
        // Verify output was truncated (ends with ellipsis in popover)
        assert!(html.contains("…"));
    }

    #[test]
    fn test_format_timestamp_utf8_safe() {
        // Malformed timestamp with multi-byte chars (edge case)
        let weird_ts = "2026-01-25T12:30:00日本語";
        let formatted = format_timestamp(weird_ts);
        // Should not panic and should produce valid output
        assert!(!formatted.is_empty());
    }

    // ========================================================================
    // MessageGroup Rendering Tests
    // ========================================================================

    fn test_tool_call(name: &str) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            input: r#"{"test": "input"}"#.to_string(),
            output: Some("test output".to_string()),
            status: Some(ToolStatus::Success),
        }
    }

    fn test_tool_call_with_result(name: &str, status: ToolStatus) -> ToolCallWithResult {
        let call = test_tool_call(name);
        let result = ToolResult::new(name, "test output", status);
        ToolCallWithResult::new(call).with_result(result)
    }

    #[test]
    fn test_render_message_group_user() {
        let msg = test_message("user", "Hello, assistant!");
        let group = MessageGroup::user(msg);
        let opts = RenderOptions::default();
        let html = render_message_group(&group, 0, &opts).unwrap();

        assert!(html.contains("message-user"));
        assert!(html.contains("Hello, assistant!"));
        assert!(html.contains(r#"role="article""#));
        assert!(html.contains("lucide-icon")); // Has role icon
    }

    #[test]
    fn test_render_message_group_assistant_with_tools() {
        let msg = test_message("assistant", "Let me read that file.");
        let mut group = MessageGroup::assistant(msg);

        // Add tool calls
        group.add_tool_call(test_tool_call("Read"), Some("toolu_abc123".to_string()));
        group.add_tool_result(
            ToolResult::new("Read", "file contents here", ToolStatus::Success)
                .with_correlation_id("toolu_abc123"),
        );

        let opts = RenderOptions::default();
        let html = render_message_group(&group, 0, &opts).unwrap();

        assert!(html.contains("message-assistant"));
        assert!(html.contains("Let me read that file."));
        assert!(html.contains("tool-badge")); // Has tool badge
        assert!(html.contains("Read")); // Tool name in badge
        assert!(html.contains(r#"role="group""#)); // Accessibility for tool container
        assert!(html.contains("aria-label")); // Accessible
    }

    #[test]
    fn test_render_message_group_multiple_tools() {
        let msg = test_message("assistant", "I'll run several commands.");
        let mut group = MessageGroup::assistant(msg);

        // Add multiple tool calls
        let tools = ["Bash", "Read", "Write"];
        for (i, name) in tools.iter().enumerate() {
            group.add_tool_call(test_tool_call(name), Some(format!("toolu_{}", i)));
        }

        let opts = RenderOptions::default();
        let html = render_message_group(&group, 0, &opts).unwrap();

        // Should have all tool badges
        for tool_name in tools {
            assert!(
                html.contains(tool_name),
                "Should contain badge for {}",
                tool_name
            );
        }
        assert!(html.contains("with 3 tool calls")); // Aria label mentions count
    }

    #[test]
    fn test_render_tool_badges_overflow() {
        // Create more tools than MAX_VISIBLE_BADGES
        let tool_names = [
            "Read", "Write", "Bash", "Glob", "Grep", "WebFetch", "Task", "Search",
        ];
        let tools: Vec<ToolCallWithResult> = tool_names
            .iter()
            .map(|name| test_tool_call_with_result(name, ToolStatus::Success))
            .collect();

        let opts = RenderOptions::default();
        let (html, overflow) = render_tool_badges_with_overflow(&tools, &opts);

        // Should show MAX_VISIBLE_BADGES badges
        assert!(overflow > 0, "Should have overflow");
        assert_eq!(overflow, tools.len() - MAX_VISIBLE_BADGES);

        // Should have overflow badge
        assert!(html.contains("tool-overflow"));
        assert!(html.contains(&format!("+{}", overflow)));
    }

    #[test]
    fn test_render_tool_badges_no_overflow() {
        let tools: Vec<ToolCallWithResult> = ["Read", "Write", "Bash"]
            .iter()
            .map(|name| test_tool_call_with_result(name, ToolStatus::Success))
            .collect();

        let opts = RenderOptions::default();
        let (html, overflow) = render_tool_badges_with_overflow(&tools, &opts);

        assert_eq!(overflow, 0);
        assert!(!html.contains("tool-overflow"));
        assert!(html.contains("Read"));
        assert!(html.contains("Write"));
        assert!(html.contains("Bash"));
    }

    #[test]
    fn test_render_single_tool_badge_success() {
        let tool = test_tool_call_with_result("Bash", ToolStatus::Success);
        let html = render_single_tool_badge(&tool);

        assert!(html.contains("tool-badge"));
        assert!(html.contains("tool-status-success"));
        assert!(html.contains("Bash"));
        assert!(html.contains(r#"aria-label="Bash: success""#));
        assert!(html.contains("lucide-icon")); // Has SVG icon
    }

    #[test]
    fn test_render_single_tool_badge_error() {
        let tool = test_tool_call_with_result("Bash", ToolStatus::Error);
        let html = render_single_tool_badge(&tool);

        assert!(html.contains("tool-status-error"));
        assert!(html.contains(r#"aria-label="Bash: error""#));
    }

    #[test]
    fn test_render_single_tool_badge_with_data_attributes() {
        let tool = test_tool_call_with_result("Read", ToolStatus::Success);
        let html = render_single_tool_badge(&tool);

        assert!(html.contains(r#"data-tool-name="Read""#));
        assert!(html.contains("data-tool-input="));
        assert!(html.contains("data-tool-output="));
    }

    #[test]
    fn test_get_tool_lucide_icon() {
        // Check icon mappings
        assert!(get_tool_lucide_icon("Bash").contains("polyline")); // Terminal
        assert!(get_tool_lucide_icon("Read").contains("M15 2H6")); // FileText
        assert!(get_tool_lucide_icon("Write").contains("M21.174")); // Pencil
        assert!(get_tool_lucide_icon("Glob").contains("circle cx=\"11\"")); // Search
        assert!(get_tool_lucide_icon("WebFetch").contains("circle cx=\"12\" cy=\"12\" r=\"10\"")); // Globe
        assert!(get_tool_lucide_icon("mcp__mcp-agent-mail__send").contains("rect width=\"20\"")); // Mail
        assert!(get_tool_lucide_icon("unknown_tool").contains("path d=\"M14.7 6.3")); // Wrench fallback
    }

    #[test]
    fn test_render_message_groups_empty() {
        let groups: Vec<MessageGroup> = vec![];
        let opts = RenderOptions::default();
        let html = render_message_groups(&groups, &opts).unwrap();

        // Should just have the wrapper if agent class is set
        assert!(html.is_empty() || !html.contains("conversation-messages"));
    }

    #[test]
    fn test_render_message_groups_with_agent_class() {
        let groups = vec![
            MessageGroup::user(test_message("user", "Hello")),
            MessageGroup::assistant(test_message("assistant", "Hi there")),
        ];
        let opts = RenderOptions {
            agent_slug: Some("claude_code".to_string()),
            ..Default::default()
        };
        let html = render_message_groups(&groups, &opts).unwrap();

        assert!(html.contains("agent-claude"));
        assert!(html.contains("conversation-messages"));
        assert!(html.contains("message-user"));
        assert!(html.contains("message-assistant"));
    }

    #[test]
    fn test_render_message_group_system() {
        let msg = test_message("system", "You are a helpful assistant.");
        let group = MessageGroup::system(msg);
        let opts = RenderOptions::default();
        let html = render_message_group(&group, 0, &opts).unwrap();

        assert!(html.contains("message-system"));
        assert!(html.contains("System")); // Author display
        assert!(html.contains("You are a helpful assistant."));
    }

    #[test]
    fn test_render_message_group_tool_only() {
        let msg = test_message("tool", "Tool result content");
        let group = MessageGroup::tool_only(msg);
        let opts = RenderOptions::default();
        let html = render_message_group(&group, 0, &opts).unwrap();

        assert!(html.contains("message-tool"));
    }

    #[test]
    fn test_render_message_group_with_timestamp() {
        let mut msg = test_message("user", "Test message");
        msg.timestamp = Some("2026-01-25T14:30:00Z".to_string());
        let group = MessageGroup::user(msg);

        let opts = RenderOptions {
            show_timestamps: true,
            ..Default::default()
        };
        let html = render_message_group(&group, 0, &opts).unwrap();

        assert!(html.contains("<time"));
        assert!(html.contains("datetime="));
        assert!(html.contains("2026-01-25"));
    }

    #[test]
    fn test_render_message_group_without_timestamps() {
        let mut msg = test_message("user", "Test message");
        msg.timestamp = Some("2026-01-25T14:30:00Z".to_string());
        let group = MessageGroup::user(msg);

        let opts = RenderOptions {
            show_timestamps: false,
            ..Default::default()
        };
        let html = render_message_group(&group, 0, &opts).unwrap();

        assert!(!html.contains("<time"));
    }

    #[test]
    fn test_render_message_group_tool_badges_hidden_when_disabled() {
        let msg = test_message("assistant", "Let me check that file.");
        let mut group = MessageGroup::assistant(msg);
        group.add_tool_call(test_tool_call("Read"), None);

        let opts = RenderOptions {
            show_tool_calls: false,
            ..Default::default()
        };
        let html = render_message_group(&group, 0, &opts).unwrap();

        assert!(!html.contains("tool-badge"));
    }

    #[test]
    fn test_render_message_group_with_collapse() {
        let long_content = "x".repeat(2000);
        let msg = test_message("user", &long_content);
        let group = MessageGroup::user(msg);

        let opts = RenderOptions {
            collapse_threshold: 1000,
            ..Default::default()
        };
        let html = render_message_group(&group, 0, &opts).unwrap();

        assert!(html.contains("<details"));
        assert!(html.contains("message-collapse"));
        assert!(html.contains("Click to expand"));
    }

    #[test]
    fn test_render_message_group_anchors() {
        let mut msg = test_message("user", "Test message");
        msg.index = Some(42);
        let group = MessageGroup::user(msg);
        let opts = RenderOptions::default();
        let html = render_message_group(&group, 0, &opts).unwrap();

        assert!(html.contains(r#"id="msg-42""#));
    }

    #[test]
    fn test_render_message_group_uses_fallback_index() {
        // No message index, should use the group index
        let msg = test_message("user", "Test message");
        let group = MessageGroup::user(msg);
        let opts = RenderOptions::default();
        let html = render_message_group(&group, 5, &opts).unwrap();

        assert!(html.contains(r#"id="msg-5""#));
    }

    #[test]
    fn test_tool_badge_truncates_long_input() {
        let long_input = r#"{"command": ""#.to_owned() + &"x".repeat(500) + r#""}"#;
        let mut call = test_tool_call("Bash");
        call.input = long_input;
        let tool = ToolCallWithResult::new(call);
        let html = render_single_tool_badge(&tool);

        // Should contain truncated input with ellipsis
        assert!(html.contains("..."));
    }

    #[test]
    fn test_tool_badge_accessibility() {
        let tool = test_tool_call_with_result("Read", ToolStatus::Success);
        let html = render_single_tool_badge(&tool);

        // Must be a button (keyboard accessible)
        assert!(html.contains("<button"));
        assert!(html.contains("</button>"));
        // Must have aria-label
        assert!(html.contains("aria-label="));
        // Must have aria-expanded for popover
        assert!(html.contains("aria-expanded="));
    }

    #[test]
    fn test_render_message_groups_all_roles() {
        let groups = vec![
            MessageGroup::user(test_message("user", "User message")),
            MessageGroup::assistant(test_message("assistant", "Assistant response")),
            MessageGroup::system(test_message("system", "System context")),
            MessageGroup::tool_only(test_message("tool", "Tool result")),
        ];
        let opts = RenderOptions::default();
        let html = render_message_groups(&groups, &opts).unwrap();

        assert!(html.contains("message-user"));
        assert!(html.contains("message-assistant"));
        assert!(html.contains("message-system"));
        assert!(html.contains("message-tool"));
    }
}
