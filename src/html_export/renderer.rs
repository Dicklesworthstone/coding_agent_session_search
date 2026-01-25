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

use super::template::html_escape;
use serde_json;

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

    fn icon(&self) -> &'static str {
        match self {
            ToolStatus::Success => "✓",
            ToolStatus::Error => "✗",
            ToolStatus::Pending => "⋯",
        }
    }
}

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
    let mut html = String::with_capacity(messages.len() * 2000);

    // Add agent-specific class to conversation wrapper if specified
    let agent_class = options
        .agent_slug
        .as_ref()
        .map(|s| agent_css_class(s))
        .unwrap_or("");

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

    Ok(html)
}

/// Render a single message to HTML.
pub fn render_message(message: &Message, options: &RenderOptions) -> Result<String, RenderError> {
    let role_class = match message.role.as_str() {
        "user" => "message-user",
        "assistant" | "agent" => "message-assistant",
        "tool" => "message-tool",
        "system" => "message-system",
        _ => "message-user",
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
            let preview_len = options.collapse_threshold.min(500);
            // Safe truncation at char boundary to avoid panic on multi-byte UTF-8
            let safe_len = truncate_to_char_boundary(&message.content, preview_len);
            let preview = &message.content[..safe_len];
            (
                format!(
                    r#"<details class="message-collapsed">
                    <summary class="message-preview">
                        <span class="preview-text">{}</span>
                        <span class="expand-hint">Click to expand ({} chars)</span>
                    </summary>
                    <div class="message-full">"#,
                    html_escape(preview),
                    message.content.len()
                ),
                "</div></details>".to_string(),
            )
        } else {
            (String::new(), String::new())
        };

    let tool_call_html = if options.show_tool_calls {
        if let Some(tc) = &message.tool_call {
            render_tool_call(tc, options)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Role icon for visual differentiation
    let role_icon = match message.role.as_str() {
        "user" => r#"<span class="role-icon" aria-hidden="true">👤</span>"#,
        "assistant" | "agent" => r#"<span class="role-icon" aria-hidden="true">🤖</span>"#,
        "tool" => r#"<span class="role-icon" aria-hidden="true">🔧</span>"#,
        "system" => r#"<span class="role-icon" aria-hidden="true">⚙️</span>"#,
        _ => "",
    };

    Ok(format!(
        r#"            <article class="message {role_class}"{anchor} role="article" aria-label="{role} message">
                <header class="message-header">
                    {role_icon}
                    <span class="message-author">{author}</span>
                    {timestamp}
                </header>
                <div class="message-content">
                    {wrapper_start}{content}{wrapper_end}
                </div>
                {tool_call}
            </article>"#,
        role_class = role_class,
        anchor = anchor_id,
        role = html_escape(&message.role),
        role_icon = role_icon,
        author = author_display,
        timestamp = timestamp_html,
        wrapper_start = content_wrapper_start,
        content = content_html,
        wrapper_end = content_wrapper_end,
        tool_call = tool_call_html,
    ))
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

/// Render message content, converting markdown to HTML.
fn render_content(content: &str, options: &RenderOptions) -> String {
    let mut result = String::new();
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_content = String::new();

    for line in content.lines() {
        if line.starts_with("```") {
            if in_code_block {
                // End code block
                result.push_str(&render_code_block(&code_content, &code_lang, options));
                code_content.clear();
                code_lang.clear();
                in_code_block = false;
            } else {
                // Start code block
                code_lang = line.trim_start_matches('`').trim().to_string();
                in_code_block = true;
            }
        } else if in_code_block {
            if !code_content.is_empty() {
                code_content.push('\n');
            }
            code_content.push_str(line);
        } else {
            // Regular paragraph
            let escaped = html_escape(line);
            let with_inline_code = render_inline_code(&escaped);
            let with_links = render_links(&with_inline_code);

            if !with_links.is_empty() {
                result.push_str("<p>");
                result.push_str(&with_links);
                result.push_str("</p>\n");
            }
        }
    }

    // Handle unclosed code block
    if in_code_block && !code_content.is_empty() {
        result.push_str(&render_code_block(&code_content, &code_lang, options));
    }

    result
}

/// Render a code block with optional syntax highlighting.
fn render_code_block(content: &str, lang: &str, options: &RenderOptions) -> String {
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

/// Render a tool call section.
fn render_tool_call(tool_call: &ToolCall, options: &RenderOptions) -> String {
    // Status indicator
    let (status_class, status_icon) = tool_call
        .status
        .as_ref()
        .map(|s| (s.css_class(), s.icon()))
        .unwrap_or(("", ""));

    // Format input as pretty JSON if possible
    let formatted_input = format_json_or_raw(&tool_call.input);

    // Format output with truncation for very long outputs
    let output_html = if let Some(output) = &tool_call.output {
        let formatted = format_json_or_raw(output);
        let (display_output, is_truncated) = if formatted.len() > 10000 {
            // Safe truncation at char boundary to avoid panic on multi-byte UTF-8
            let safe_len = truncate_to_char_boundary(&formatted, 10000);
            let truncated = &formatted[..safe_len];
            (truncated.to_string(), true)
        } else {
            (formatted, false)
        };

        let truncate_notice = if is_truncated {
            r#"<p class="truncate-notice">Output truncated (10,000+ chars)</p>"#
        } else {
            ""
        };

        format!(
            r#"
                        <div class="tool-output">
                            <div class="tool-section-header">Output</div>
                            <pre><code class="language-json">{}</code></pre>
                            {}
                        </div>"#,
            html_escape(&display_output),
            truncate_notice
        )
    } else {
        String::new()
    };

    // Tool icon based on name
    let tool_icon = match tool_call.name.to_lowercase().as_str() {
        "bash" | "shell" => "💻",
        "read" | "read_file" => "📖",
        "write" | "write_file" => "📝",
        "edit" => "✏️",
        "glob" | "find" => "🔍",
        "grep" | "search" => "🔎",
        "webfetch" | "fetch" | "http" => "🌐",
        "websearch" => "🔍",
        _ => "🔧",
    };

    // Code preview lines option
    let input_class = if options.code_preview_lines > 0
        && formatted_input.lines().count() > options.code_preview_lines
    {
        "tool-input-long"
    } else {
        "tool-input"
    };

    format!(
        r#"
                <details class="tool-call">
                    <summary class="tool-call-header">
                        <span class="tool-icon" aria-hidden="true">{icon}</span>
                        <span class="tool-call-name">{name}</span>
                        {status_badge}
                        <span class="tool-call-toggle" aria-hidden="true">▼</span>
                    </summary>
                    <div class="tool-call-body">
                        <div class="{input_class}">
                            <div class="tool-section-header">Input</div>
                            <pre><code class="language-json">{input}</code></pre>
                        </div>{output}
                    </div>
                </details>"#,
        icon = tool_icon,
        name = html_escape(&tool_call.name),
        status_badge = if !status_class.is_empty() {
            format!(
                r#"<span class="tool-status {}">{}</span>"#,
                status_class, status_icon
            )
        } else {
            String::new()
        },
        input_class = input_class,
        input = html_escape(&formatted_input),
        output = output_html,
    )
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
        assert!(html.contains("👤")); // User icon
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
        assert!(html.contains("🤖")); // Assistant icon
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
        assert!(html.contains("✓")); // Success icon
        assert!(html.contains("💻")); // Bash icon
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
        assert!(html.contains("message-collapsed"));
        assert!(html.contains("Click to expand"));
    }

    #[test]
    fn test_tool_icons_for_different_tools() {
        let tools_and_icons = vec![
            ("Read", "📖"),
            ("Write", "📝"),
            ("Bash", "💻"),
            ("Grep", "🔎"),
            ("WebFetch", "🌐"),
        ];

        for (tool_name, expected_icon) in tools_and_icons {
            let tc = ToolCall {
                name: tool_name.to_string(),
                input: "{}".to_string(),
                output: None,
                status: None,
            };
            let html = render_tool_call(&tc, &RenderOptions::default());
            assert!(
                html.contains(expected_icon),
                "Tool {} should have icon {}",
                tool_name,
                expected_icon
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
        assert!(html.contains("message-collapsed"));
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

        // Should not panic even though we're truncating at 10000 bytes
        let html = render_message(&msg, &RenderOptions::default()).unwrap();
        assert!(html.contains("Output truncated"));
    }

    #[test]
    fn test_format_timestamp_utf8_safe() {
        // Malformed timestamp with multi-byte chars (edge case)
        let weird_ts = "2026-01-25T12:30:00日本語";
        let formatted = format_timestamp(weird_ts);
        // Should not panic and should produce valid output
        assert!(!formatted.is_empty());
    }
}
