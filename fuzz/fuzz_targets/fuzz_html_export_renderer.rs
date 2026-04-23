//! Fuzz target for the HTML export message renderer.
//!
//! Exercises markdown rendering, timestamp/author escaping, collapse previews,
//! and tool-call popovers. The target keeps input sizes bounded so crashes point
//! at renderer behavior instead of unbounded markdown expansion.

#![no_main]

use arbitrary::Arbitrary;
use coding_agent_search::html_export::{
    render_message, Message, RenderOptions, ToolCall, ToolStatus,
};
use libfuzzer_sys::fuzz_target;

const MAX_CONTENT_BYTES: usize = 16 * 1024;
const MAX_FIELD_BYTES: usize = 1024;
const MAX_TOOL_BYTES: usize = 8 * 1024;
const MAX_RENDERED_BYTES: usize = 2 * 1024 * 1024;

#[derive(Arbitrary, Debug)]
enum FuzzRole {
    User,
    Assistant,
    Agent,
    Tool,
    System,
    Other(String),
}

#[derive(Arbitrary, Debug)]
struct FuzzTool {
    name: String,
    input: String,
    output: Option<String>,
    status: Option<u8>,
}

#[derive(Arbitrary, Debug)]
struct FuzzOptions {
    show_timestamps: bool,
    show_tool_calls: bool,
    syntax_highlighting: bool,
    wrap_code: bool,
    collapse_threshold: u16,
    code_preview_lines: u8,
    agent_slug: Option<String>,
}

#[derive(Arbitrary, Debug)]
struct RendererInput {
    role: FuzzRole,
    content: String,
    timestamp: Option<String>,
    author: Option<String>,
    index: Option<u16>,
    tool_call: Option<FuzzTool>,
    options: FuzzOptions,
}

fn bounded_string(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }

    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn fuzz_role_to_string(role: FuzzRole) -> String {
    match role {
        FuzzRole::User => "user".to_string(),
        FuzzRole::Assistant => "assistant".to_string(),
        FuzzRole::Agent => "agent".to_string(),
        FuzzRole::Tool => "tool".to_string(),
        FuzzRole::System => "system".to_string(),
        FuzzRole::Other(value) => bounded_string(value, MAX_FIELD_BYTES),
    }
}

fn fuzz_status_to_tool_status(status: u8) -> ToolStatus {
    match status % 3 {
        0 => ToolStatus::Success,
        1 => ToolStatus::Error,
        _ => ToolStatus::Pending,
    }
}

fn build_message(input: RendererInput) -> (Message, RenderOptions) {
    let mut content = bounded_string(input.content, MAX_CONTENT_BYTES);

    // Fixed probes keep XSS-sensitive markdown and raw HTML paths hot even when
    // the generated corpus mostly contains plain text.
    content.push_str(
        "\n\n<script>alert(1)</script>\n\
         [probe](javascript:alert(1))\n\
         [probe2](vbscript:msgbox(1))\n\
         ![probe3](data:text/html,<svg onload=alert(1)>)\n",
    );

    let tool_call = input.tool_call.map(|tool| ToolCall {
        name: bounded_string(tool.name, MAX_FIELD_BYTES),
        input: bounded_string(tool.input, MAX_TOOL_BYTES),
        output: tool
            .output
            .map(|output| bounded_string(output, MAX_TOOL_BYTES)),
        status: tool.status.map(fuzz_status_to_tool_status),
    });

    let message = Message {
        role: fuzz_role_to_string(input.role),
        content,
        timestamp: input
            .timestamp
            .map(|timestamp| bounded_string(timestamp, MAX_FIELD_BYTES)),
        tool_call,
        index: input.index.map(usize::from),
        author: input
            .author
            .map(|author| bounded_string(author, MAX_FIELD_BYTES)),
    };

    let options = RenderOptions {
        show_timestamps: input.options.show_timestamps,
        show_tool_calls: input.options.show_tool_calls,
        syntax_highlighting: input.options.syntax_highlighting,
        wrap_code: input.options.wrap_code,
        collapse_threshold: usize::from(input.options.collapse_threshold),
        code_preview_lines: usize::from(input.options.code_preview_lines),
        agent_slug: input
            .options
            .agent_slug
            .map(|agent_slug| bounded_string(agent_slug, MAX_FIELD_BYTES)),
    };

    (message, options)
}

fn assert_no_active_script_or_dangerous_url(html: &str) {
    let lower = html.to_ascii_lowercase();

    assert!(!lower.contains("<script"));
    assert!(!lower.contains("href=\"javascript:"));
    assert!(!lower.contains("href='javascript:"));
    assert!(!lower.contains("src=\"javascript:"));
    assert!(!lower.contains("src='javascript:"));
    assert!(!lower.contains("href=\"vbscript:"));
    assert!(!lower.contains("href='vbscript:"));
    assert!(!lower.contains("src=\"vbscript:"));
    assert!(!lower.contains("src='vbscript:"));
    assert!(!lower.contains("href=\"data:"));
    assert!(!lower.contains("href='data:"));
    assert!(!lower.contains("src=\"data:"));
    assert!(!lower.contains("src='data:"));
}

fuzz_target!(|input: RendererInput| {
    let (message, options) = build_message(input);

    if let Ok(html) = render_message(&message, &options) {
        assert!(html.len() <= MAX_RENDERED_BYTES);
        assert_no_active_script_or_dangerous_url(&html);
    }
});
