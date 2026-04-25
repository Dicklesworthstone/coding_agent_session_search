//! Export functionality for search results.
//!
//! Provides conversion of search results to various output formats:
//! - Markdown - formatted with headers, code blocks, and metadata
//! - JSON - structured data for programmatic use
//! - Plain Text - simple, copy-paste friendly format

use crate::search::query::SearchHit;
use chrono::{DateTime, Utc};

/// Supported export formats
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportFormat {
    /// Markdown format with headers, code blocks, and rich formatting
    #[default]
    Markdown,
    /// JSON format for programmatic consumption
    Json,
    /// Plain text format for simple copy-paste
    PlainText,
}

impl ExportFormat {
    /// Get the display name for this format
    pub fn name(self) -> &'static str {
        match self {
            Self::Markdown => "Markdown",
            Self::Json => "JSON",
            Self::PlainText => "Plain Text",
        }
    }

    /// Get the file extension for this format
    pub fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Json => "json",
            Self::PlainText => "txt",
        }
    }

    /// Cycle to the next export format
    pub fn next(self) -> Self {
        match self {
            Self::Markdown => Self::Json,
            Self::Json => Self::PlainText,
            Self::PlainText => Self::Markdown,
        }
    }

    /// List all available formats
    pub fn all() -> &'static [Self] {
        &[Self::Markdown, Self::Json, Self::PlainText]
    }
}

/// Options for export customization
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Include full content (not just snippets)
    pub include_content: bool,
    /// Include score in output
    pub include_score: bool,
    /// Include source path
    pub include_path: bool,
    /// Maximum snippet length (0 = unlimited)
    pub max_snippet_len: usize,
    /// Query string (for header/metadata)
    pub query: Option<String>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            include_content: false,
            include_score: true,
            include_path: true,
            max_snippet_len: 500,
            query: None,
        }
    }
}

/// Export search results to the specified format
pub fn export_results(hits: &[SearchHit], format: ExportFormat, options: &ExportOptions) -> String {
    match format {
        ExportFormat::Markdown => export_markdown(hits, options),
        ExportFormat::Json => export_json(hits, options),
        ExportFormat::PlainText => export_plain_text(hits, options),
    }
}

/// Escape special Markdown characters to prevent formatting issues or injection.
fn escape_markdown(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('<', "\\<")
        .replace('>', "\\>")
        .replace('`', "\\`")
        .replace('\n', " ") // Replace newlines with space to prevent breaking tables
        .replace('\r', "") // Remove carriage returns
}

/// Determine the appropriate code block delimiter (e.g., ``` or ````) based on content.
fn get_code_block_delimiter(content: &str) -> String {
    let mut max_backticks = 0;
    let mut current = 0;
    for c in content.chars() {
        if c == '`' {
            current += 1;
        } else {
            max_backticks = max_backticks.max(current);
            current = 0;
        }
    }
    max_backticks = max_backticks.max(current);

    let needed = (max_backticks + 1).max(3);
    "`".repeat(needed)
}

/// Export to Markdown format
fn export_markdown(hits: &[SearchHit], options: &ExportOptions) -> String {
    let mut output = String::new();

    // Header
    output.push_str("# Search Results\n\n");

    if let Some(query) = &options.query {
        output.push_str(&format!("**Query:** `{}`\n\n", query.replace('`', "")));
    }

    output.push_str(&format!(
        "**Results:** {} | **Exported:** {}\n\n",
        hits.len(),
        Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    ));

    output.push_str("---\n\n");

    // Results
    for (i, hit) in hits.iter().enumerate() {
        let safe_title = escape_markdown(&hit.title);
        output.push_str(&format!("## {}. {}\n\n", i + 1, safe_title));

        // Metadata table
        output.push_str("| Field | Value |\n");
        output.push_str("|-------|-------|\n");
        output.push_str(&format!("| Agent | {} |\n", escape_markdown(&hit.agent)));
        output.push_str(&format!(
            "| Workspace | {} |\n",
            escape_markdown(&hit.workspace)
        ));

        if options.include_score {
            output.push_str(&format!("| Score | {:.2} |\n", hit.score));
        }

        if let Some(ts) = hit.created_at
            && let Some(dt) = DateTime::from_timestamp_millis(ts)
        {
            output.push_str(&format!("| Date | {} |\n", dt.format("%Y-%m-%d %H:%M")));
        }

        if options.include_path {
            let path_display = if hit.source_path.chars().count() > 60 {
                let skip = hit.source_path.chars().count() - 57;
                format!(
                    "...{}",
                    hit.source_path.chars().skip(skip).collect::<String>()
                )
            } else {
                hit.source_path.clone()
            };
            output.push_str(&format!(
                "| Source | {} |\n",
                escape_markdown(&path_display)
            ));

            if let Some(line) = hit.line_number {
                output.push_str(&format!("| Line | {line} |\n"));
            }
        }

        output.push('\n');

        // Snippet
        output.push_str("### Snippet\n\n");
        let snippet = truncate_text(&hit.snippet, options.max_snippet_len);
        let delim = get_code_block_delimiter(&snippet);
        output.push_str(&format!("{}\n", delim));
        output.push_str(&snippet);
        if !snippet.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&format!("{}\n\n", delim));

        // Full content (optional)
        if options.include_content && !hit.content.is_empty() {
            output.push_str("<details>\n<summary>Full Content</summary>\n\n");
            let content_delim = get_code_block_delimiter(&hit.content);
            output.push_str(&format!("{}\n", content_delim));
            output.push_str(&hit.content);
            if !hit.content.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&format!("{}\n\n", content_delim));
            output.push_str("</details>\n\n");
        }

        output.push_str("---\n\n");
    }

    output
}

/// Export to JSON format
fn export_json(hits: &[SearchHit], options: &ExportOptions) -> String {
    let export_data = serde_json::json!({
        "query": options.query,
        "count": hits.len(),
        "exported_at": Utc::now().to_rfc3339(),
        "hits": hits
            .iter()
            .map(|hit| export_hit_json(hit, options))
            .collect::<Vec<_>>()
    });

    serde_json::to_string_pretty(&export_data).unwrap_or_else(|_| "{}".to_string())
}

fn export_hit_json(hit: &SearchHit, options: &ExportOptions) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "title": hit.title,
        "agent": hit.agent,
        "workspace": hit.workspace,
        "snippet": truncate_text(&hit.snippet, options.max_snippet_len),
    });

    if options.include_score {
        let score = if hit.score.is_finite() {
            hit.score
        } else {
            0.0
        };
        obj["score"] = serde_json::json!(score);
    }

    if options.include_path {
        obj["source_path"] = serde_json::json!(hit.source_path);
        if let Some(line) = hit.line_number {
            obj["line_number"] = serde_json::json!(line);
        }
    }

    if let Some(ts) = hit.created_at {
        obj["created_at"] = serde_json::json!(ts);
        if let Some(dt) = DateTime::from_timestamp_millis(ts) {
            obj["created_at_formatted"] = serde_json::json!(dt.to_rfc3339());
        }
    }

    if options.include_content && !hit.content.is_empty() {
        obj["content"] = serde_json::json!(hit.content);
    }

    obj
}

/// Export to plain text format
fn export_plain_text(hits: &[SearchHit], options: &ExportOptions) -> String {
    let mut output = String::new();

    // Header
    output.push_str("SEARCH RESULTS\n");
    output.push_str(&"=".repeat(60));
    output.push('\n');

    if let Some(query) = &options.query {
        output.push_str(&format!("Query: {query}\n"));
    }

    output.push_str(&format!(
        "Results: {} | Exported: {}\n",
        hits.len(),
        Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    ));

    output.push_str(&"=".repeat(60));
    output.push_str("\n\n");

    // Results
    for (i, hit) in hits.iter().enumerate() {
        output.push_str(&format!("[{}] {}\n", i + 1, hit.title));
        output.push_str(&"-".repeat(60));
        output.push('\n');

        output.push_str(&format!("Agent: {}\n", hit.agent));
        output.push_str(&format!("Workspace: {}\n", hit.workspace));

        if options.include_score {
            output.push_str(&format!("Score: {:.2}\n", hit.score));
        }

        if let Some(ts) = hit.created_at
            && let Some(dt) = DateTime::from_timestamp_millis(ts)
        {
            output.push_str(&format!("Date: {}\n", dt.format("%Y-%m-%d %H:%M")));
        }

        if options.include_path {
            output.push_str(&format!("Source: {}\n", hit.source_path));
            if let Some(line) = hit.line_number {
                output.push_str(&format!("Line: {line}\n"));
            }
        }

        output.push('\n');
        output.push_str("Snippet:\n");
        let snippet = truncate_text(&hit.snippet, options.max_snippet_len);
        for line in snippet.lines() {
            output.push_str(&format!("  {line}\n"));
        }

        if options.include_content && !hit.content.is_empty() {
            output.push_str("\nFull Content:\n");
            for line in hit.content.lines() {
                output.push_str(&format!("  {line}\n"));
            }
        }

        output.push('\n');
    }

    output
}

/// Truncate text to max length (in characters), adding ellipsis if needed.
///
/// When max_len <= 3, truncates without ellipsis to avoid exceeding max_len.
fn truncate_text(text: &str, max_len: usize) -> String {
    if max_len == 0 {
        return text.to_string();
    }

    let mut chars = text.chars();
    let mut preview: String = chars.by_ref().take(max_len).collect();

    if chars.next().is_none() {
        return preview;
    }

    // For very small max_len (≤3), truncate without ellipsis to avoid exceeding limit
    if max_len <= 3 {
        return preview;
    }

    let take = max_len.saturating_sub(3);
    preview.truncate(preview.chars().take(take).map(|c| c.len_utf8()).sum());
    preview.push_str("...");
    preview
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hit() -> SearchHit {
        SearchHit {
            title: "Test Result".to_string(),
            snippet: "This is a test snippet".to_string(),
            content: "Full content here".to_string(),
            content_hash: crate::search::query::stable_content_hash("Full content here"),
            conversation_id: None,
            score: 8.5,
            source_path: "/path/to/file.jsonl".to_string(),
            agent: "claude_code".to_string(),
            workspace: "/projects/test".to_string(),
            workspace_original: None,
            created_at: Some(1700000000000),
            line_number: Some(42),
            match_type: crate::search::query::MatchType::Exact,
            source_id: "local".to_string(),
            origin_kind: "local".to_string(),
            origin_host: None,
        }
    }

    fn assert_json_field(value: &serde_json::Value, key: &str, expected: serde_json::Value) {
        assert_eq!(
            value.get(key),
            Some(&expected),
            "unexpected JSON field `{key}` in {value}"
        );
    }

    #[test]
    fn test_export_format_cycle() {
        let format = ExportFormat::Markdown;
        assert_eq!(format.next(), ExportFormat::Json);
        assert_eq!(format.next().next(), ExportFormat::PlainText);
        assert_eq!(format.next().next().next(), ExportFormat::Markdown);
    }

    #[test]
    fn test_export_format_extension() {
        assert_eq!(ExportFormat::Markdown.extension(), "md");
        assert_eq!(ExportFormat::Json.extension(), "json");
        assert_eq!(ExportFormat::PlainText.extension(), "txt");
    }

    #[test]
    fn test_truncate_text() {
        assert_eq!(truncate_text("short", 100), "short");
        assert_eq!(truncate_text("this is long text", 10), "this is...");
        assert_eq!(truncate_text("any", 0), "any");

        // Edge case: very small max_len should not exceed limit
        assert_eq!(truncate_text("hello", 3), "hel"); // No ellipsis for max_len <= 3
        assert_eq!(truncate_text("hello", 2), "he");
        assert_eq!(truncate_text("hello", 1), "h");
        assert_eq!(truncate_text("hello", 4), "h..."); // max_len > 3 gets ellipsis
    }

    #[test]
    fn test_export_markdown() {
        let hits = vec![sample_hit()];
        let options = ExportOptions::default();
        let output = export_markdown(&hits, &options);

        assert!(output.contains("# Search Results"));
        assert!(output.contains("Test Result"));
        // underscores are escaped in markdown
        assert!(output.contains("claude\\_code"));
        assert!(output.contains("```"));
    }

    #[test]
    fn test_export_json() {
        let hits = vec![sample_hit()];
        let options = ExportOptions::default();
        let output = export_json(&hits, &options);

        assert!(output.contains("\"count\": 1"));
        assert!(output.contains("\"agent\": \"claude_code\""));
    }

    #[test]
    fn test_export_hit_json_shape() {
        let mut hit = sample_hit();
        hit.score = f32::NAN;
        let options = ExportOptions {
            include_content: true,
            include_score: true,
            include_path: true,
            max_snippet_len: 10,
            query: Some("ignored by hit projection".to_string()),
        };

        let projected = export_hit_json(&hit, &options);

        assert_json_field(&projected, "title", serde_json::json!("Test Result"));
        assert_json_field(&projected, "agent", serde_json::json!("claude_code"));
        assert_json_field(&projected, "workspace", serde_json::json!("/projects/test"));
        assert_json_field(&projected, "snippet", serde_json::json!("This is..."));
        assert_json_field(&projected, "score", serde_json::json!(0.0));
        assert_json_field(
            &projected,
            "source_path",
            serde_json::json!("/path/to/file.jsonl"),
        );
        assert_json_field(&projected, "line_number", serde_json::json!(42));
        assert_json_field(
            &projected,
            "created_at",
            serde_json::json!(1700000000000i64),
        );
        assert_json_field(
            &projected,
            "created_at_formatted",
            serde_json::json!("2023-11-14T22:13:20+00:00"),
        );
        assert_json_field(
            &projected,
            "content",
            serde_json::json!("Full content here"),
        );
        assert_eq!(projected.as_object().expect("object").len(), 10);
    }

    #[test]
    fn test_export_plain_text() {
        let hits = vec![sample_hit()];
        let options = ExportOptions::default();
        let output = export_plain_text(&hits, &options);

        assert!(output.contains("SEARCH RESULTS"));
        assert!(output.contains("[1] Test Result"));
        assert!(output.contains("Agent: claude_code"));
    }

    #[test]
    fn test_export_markdown_escapes_special_chars() {
        let mut hit = sample_hit();
        hit.title = "[Link](javascript:alert(1))".to_string();
        hit.agent = "agent|pipe".to_string();
        hit.content = "Contains ``` backticks".to_string();

        let options = ExportOptions {
            include_content: true,
            ..ExportOptions::default()
        };
        let output = export_markdown(&[hit], &options);

        assert!(output.contains("\\[Link\\](javascript:alert(1))"));
        assert!(output.contains("agent\\|pipe"));
        // Should use 4 backticks because content has 3
        assert!(output.contains("````\nContains ``` backticks"));
    }
}
