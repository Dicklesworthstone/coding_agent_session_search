//! Conversation to HTML rendering.
//!
//! Converts session messages into semantic HTML markup with proper
//! role-based styling and syntax highlighting support.

use std::fmt;

use super::template::html_escape;

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
#[derive(Debug, Clone, Default)]
pub struct RenderOptions {
    /// Show message timestamps
    pub show_timestamps: bool,

    /// Show tool call details
    pub show_tool_calls: bool,

    /// Enable syntax highlighting markers (for Prism.js)
    pub syntax_highlighting: bool,

    /// Wrap long lines in code blocks
    pub wrap_code: bool,
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
}

/// Render a list of messages to HTML.
pub fn render_conversation(messages: &[Message], options: &RenderOptions) -> Result<String, RenderError> {
    let mut html = String::new();

    for message in messages {
        html.push_str(&render_message(message, options)?);
        html.push('\n');
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

    let timestamp_html = if options.show_timestamps {
        if let Some(ts) = &message.timestamp {
            format!(
                r#"<time class="message-time" datetime="{}">{}</time>"#,
                html_escape(ts),
                format_timestamp(ts)
            )
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let content_html = render_content(&message.content, options);

    let tool_call_html = if options.show_tool_calls {
        if let Some(tc) = &message.tool_call {
            render_tool_call(tc, options)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    Ok(format!(
        r#"            <article class="message {role_class}" role="article">
                <header class="message-header">
                    <span class="message-role">{role}</span>
                    {timestamp}
                </header>
                <div class="message-content">
                    {content}
                </div>
                {tool_call}
            </article>"#,
        role_class = role_class,
        role = html_escape(&message.role),
        timestamp = timestamp_html,
        content = content_html,
        tool_call = tool_call_html,
    ))
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
    let mut chars = text.chars().peekable();
    let mut in_code = false;
    let mut code = String::new();

    while let Some(c) = chars.next() {
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
                if next.is_whitespace() || next == '<' || next == '>' || next == '"' {
                    break;
                }
                url.push(chars.next().unwrap());
            }

            // Escape URL for safe inclusion in href attribute
            let escaped_url = html_escape(&url);
            result.push_str(&format!(
                r#"<a href="{url}" target="_blank" rel="noopener noreferrer">{url}</a>"#,
                url = escaped_url
            ));

            buffer.clear();
        }
    }

    result.push_str(&buffer);
    result
}

/// Render a tool call section.
fn render_tool_call(tool_call: &ToolCall, _options: &RenderOptions) -> String {
    let output_html = if let Some(output) = &tool_call.output {
        format!(
            r#"
                    <div class="tool-output">
                        <pre><code>{}</code></pre>
                    </div>"#,
            html_escape(output)
        )
    } else {
        String::new()
    };

    format!(
        r#"
                <details class="tool-call">
                    <summary class="tool-call-header">
                        <span class="tool-call-name">{name}</span>
                        <span class="tool-call-toggle">▼</span>
                    </summary>
                    <div class="tool-call-body">
                        <div class="tool-input">
                            <pre><code>{input}</code></pre>
                        </div>{output}
                    </div>
                </details>"#,
        name = html_escape(&tool_call.name),
        input = html_escape(&tool_call.input),
        output = output_html,
    )
}

/// Format a timestamp for display.
fn format_timestamp(ts: &str) -> String {
    // Simple formatting - could be enhanced with chrono
    // For now, just return a shortened version
    if ts.len() > 19 {
        ts[..19].replace('T', " ")
    } else {
        ts.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_message_user() {
        let msg = Message {
            role: "user".to_string(),
            content: "Hello, world!".to_string(),
            timestamp: None,
            tool_call: None,
        };

        let opts = RenderOptions::default();
        let html = render_message(&msg, &opts).unwrap();

        assert!(html.contains("message-user"));
        assert!(html.contains("Hello, world!"));
    }

    #[test]
    fn test_render_message_with_code() {
        let msg = Message {
            role: "assistant".to_string(),
            content: "Here's code:\n```rust\nfn main() {}\n```".to_string(),
            timestamp: None,
            tool_call: None,
        };

        let mut opts = RenderOptions::default();
        opts.syntax_highlighting = true;
        let html = render_message(&msg, &opts).unwrap();

        assert!(html.contains("<pre>"));
        assert!(html.contains("language-rust"));
        assert!(html.contains("fn main()"));
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
    fn test_html_escape_in_content() {
        let msg = Message {
            role: "user".to_string(),
            content: "<script>alert('xss')</script>".to_string(),
            timestamp: None,
            tool_call: None,
        };

        let html = render_message(&msg, &RenderOptions::default()).unwrap();
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
