//! Connectors for agent histories.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub mod aider;
pub mod amp;
pub mod chatgpt;
pub mod claude_code;
pub mod cline;
pub mod codex;
pub mod cursor;
pub mod gemini;
pub mod opencode;
pub mod pi_agent;

/// High-level detection status for a connector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionResult {
    pub detected: bool,
    pub evidence: Vec<String>,
}

impl DetectionResult {
    pub fn not_found() -> Self {
        Self {
            detected: false,
            evidence: Vec::new(),
        }
    }
}

/// Shared scan context parameters.
#[derive(Debug, Clone)]
pub struct ScanContext {
    pub data_root: PathBuf,
    pub since_ts: Option<i64>,
}

/// Normalized conversation emitted by connectors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedConversation {
    pub agent_slug: String,
    pub external_id: Option<String>,
    pub title: Option<String>,
    pub workspace: Option<PathBuf>,
    pub source_path: PathBuf,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub metadata: serde_json::Value,
    pub messages: Vec<NormalizedMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedMessage {
    pub idx: i64,
    pub role: String,
    pub author: Option<String>,
    pub created_at: Option<i64>,
    pub content: String,
    pub extra: serde_json::Value,
    pub snippets: Vec<NormalizedSnippet>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedSnippet {
    pub file_path: Option<PathBuf>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub language: Option<String>,
    pub snippet_text: Option<String>,
}

pub trait Connector {
    fn detect(&self) -> DetectionResult;
    fn scan(&self, ctx: &ScanContext) -> anyhow::Result<Vec<NormalizedConversation>>;
}

/// Check if a file was modified since the given timestamp.
/// Returns true if file should be processed (modified since timestamp or no timestamp given).
/// Uses file modification time (mtime) for comparison.
pub fn file_modified_since(path: &std::path::Path, since_ts: Option<i64>) -> bool {
    match since_ts {
        None => true, // No timestamp filter, process all files
        Some(ts) => {
            // Provide a small slack window to account for filesystem mtime granularity.
            // Some filesystems store mtime with 1s resolution, which can cause updates
            // that happen shortly after a scan to be missed if we compare exact millis.
            let threshold = ts.saturating_sub(1_000);
            // Get file modification time
            std::fs::metadata(path)
                .and_then(|m| m.modified())
                .map(|mt| {
                    mt.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| (d.as_millis() as i64) >= threshold)
                        .unwrap_or(true) // On time error, process the file
                })
                .unwrap_or(true) // On metadata error, process the file
        }
    }
}

/// Parse a timestamp from either i64 milliseconds or ISO-8601 string.
/// Returns milliseconds since Unix epoch, or None if unparseable.
///
/// Handles both legacy integer timestamps and modern ISO-8601 strings like:
/// - `1700000000000` (i64 milliseconds)
/// - `"2025-11-12T18:31:32.217Z"` (ISO-8601 string)
pub fn parse_timestamp(val: &serde_json::Value) -> Option<i64> {
    // Try direct i64 first (legacy format)
    if let Some(ts) = val.as_i64() {
        return Some(ts);
    }
    // Try ISO-8601 string (modern format)
    if let Some(s) = val.as_str() {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
            return Some(dt.timestamp_millis());
        }
        // Fallback: try parsing with explicit UTC format
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.fZ") {
            return Some(dt.and_utc().timestamp_millis());
        }
        // Fallback: try without fractional seconds
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ") {
            return Some(dt.and_utc().timestamp_millis());
        }
    }
    None
}

/// Flatten content that may be a string or array of content blocks.
/// Extracts text from text blocks and tool names from `tool_use` blocks.
///
/// Handles:
/// - Direct string content (e.g., user messages)
/// - Array of content blocks with `{"type": "text", "text": "..."}`
/// - Tool use blocks: `{"type": "tool_use", "name": "Read", "input": {...}}`
/// - Codex `input_text` blocks: `{"type": "input_text", "text": "..."}`
pub fn flatten_content(val: &serde_json::Value) -> String {
    // Direct string content (user messages in Claude Code)
    if let Some(s) = val.as_str() {
        return s.to_string();
    }

    // Array of content blocks (assistant messages)
    if let Some(arr) = val.as_array() {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|item| {
                let item_type = item.get("type").and_then(|v| v.as_str());

                // Standard text block: {"type": "text", "text": "..."}
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    // Only include if it's a text type or has no type (plain text)
                    if item_type.is_none()
                        || item_type == Some("text")
                        || item_type == Some("input_text")
                    {
                        return Some(text.to_string());
                    }
                }

                // Tool use block - include tool name for searchability
                if item_type == Some("tool_use") {
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let desc = item
                        .get("input")
                        .and_then(|i| i.get("description"))
                        .and_then(|v| v.as_str())
                        .or_else(|| {
                            item.get("input")
                                .and_then(|i| i.get("file_path"))
                                .and_then(|v| v.as_str())
                        })
                        .unwrap_or("");
                    if desc.is_empty() {
                        return Some(format!("[Tool: {name}]"));
                    }
                    return Some(format!("[Tool: {name} - {desc}]"));
                }

                None
            })
            .collect();
        return parts.join("\n");
    }

    String::new()
}
