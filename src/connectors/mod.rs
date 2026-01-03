//! Connectors for agent histories.

use crate::sources::config::{PathMapping, Platform};
use crate::sources::provenance::Origin;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub mod aider;
pub mod amp;
pub mod chatgpt;
pub mod claude_code;
pub mod cline;
pub mod codex;
pub mod cursor;
pub mod factory;
pub mod gemini;
pub mod opencode;
pub mod pi_agent;

/// High-level detection status for a connector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionResult {
    pub detected: bool,
    pub evidence: Vec<String>,
    pub root_paths: Vec<PathBuf>,
}

impl DetectionResult {
    pub fn not_found() -> Self {
        Self {
            detected: false,
            evidence: Vec::new(),
            root_paths: Vec::new(),
        }
    }
}

/// A root directory to scan with associated provenance.
///
/// Part of P2.1 - multi-root support for remote sources.
#[derive(Debug, Clone)]
pub struct ScanRoot {
    /// Path to scan (e.g., ~/.claude, or /data/remotes/work-laptop/mirror/home/.claude)
    pub path: PathBuf,

    /// Provenance for conversations found under this root.
    /// Injected into every conversation scanned from this root.
    pub origin: Origin,

    /// Optional platform hint (affects path interpretation for workspace mapping).
    pub platform: Option<Platform>,

    /// Optional path rewrite rules.
    /// Used to map remote workspace paths to local equivalents.
    /// Applied at ingest time so filters work across sources.
    pub workspace_rewrites: Vec<PathMapping>,
}

impl ScanRoot {
    /// Create a local scan root with default provenance.
    pub fn local(path: PathBuf) -> Self {
        Self {
            path,
            origin: Origin::local(),
            platform: None,
            workspace_rewrites: Vec::new(),
        }
    }

    /// Create a remote scan root.
    pub fn remote(path: PathBuf, origin: Origin, platform: Option<Platform>) -> Self {
        Self {
            path,
            origin,
            platform,
            workspace_rewrites: Vec::new(),
        }
    }

    /// Add a workspace rewrite rule.
    pub fn with_rewrite(
        mut self,
        src_prefix: impl Into<String>,
        dst_prefix: impl Into<String>,
    ) -> Self {
        self.workspace_rewrites
            .push(PathMapping::new(src_prefix, dst_prefix));
        self
    }

    /// Apply workspace rewriting rules to a path.
    ///
    /// Uses longest-prefix matching for correct handling of nested paths.
    /// Optionally filters by agent name.
    pub fn rewrite_workspace(&self, path: &str, agent: Option<&str>) -> String {
        // Sort by prefix length descending for longest-prefix match
        let mut mappings: Vec<_> = self
            .workspace_rewrites
            .iter()
            .filter(|m| m.applies_to_agent(agent))
            .collect();
        mappings.sort_by(|a, b| b.from.len().cmp(&a.from.len()));

        for mapping in mappings {
            if let Some(rewritten) = mapping.apply(path) {
                return rewritten;
            }
        }

        path.to_string()
    }
}

/// Shared scan context parameters.
#[derive(Debug, Clone)]
pub struct ScanContext {
    /// Primary data directory (cass internal state - where DB and index live).
    pub data_dir: PathBuf,

    /// Scan roots to search for agent logs.
    /// If empty, connectors use their default detection logic (backward compat).
    pub scan_roots: Vec<ScanRoot>,

    /// High-water mark for incremental indexing (milliseconds since epoch).
    pub since_ts: Option<i64>,
}

impl ScanContext {
    /// Create a context for local-only scanning (backward compatible).
    ///
    /// Connectors should use their default detection logic when scan_roots is empty.
    pub fn local_default(data_dir: PathBuf, since_ts: Option<i64>) -> Self {
        Self {
            data_dir,
            scan_roots: Vec::new(),
            since_ts,
        }
    }

    /// Create a context with explicit scan roots.
    pub fn with_roots(data_dir: PathBuf, scan_roots: Vec<ScanRoot>, since_ts: Option<i64>) -> Self {
        Self {
            data_dir,
            scan_roots,
            since_ts,
        }
    }

    /// Legacy accessor for backward compatibility.
    /// Returns data_dir as the "data_root" connectors were using before.
    #[deprecated(note = "Use data_dir directly or check scan_roots for explicit roots")]
    pub fn data_root(&self) -> &PathBuf {
        &self.data_dir
    }

    /// Check if we should use default detection logic (no explicit roots).
    pub fn use_default_detection(&self) -> bool {
        self.scan_roots.is_empty()
    }
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
                // Handle plain strings in array (e.g., ["Hello", "World"])
                if let Some(text) = item.as_str() {
                    return Some(text.to_string());
                }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_root_local_creates_with_defaults() {
        let root = ScanRoot::local(PathBuf::from("/test/path"));
        assert_eq!(root.path, PathBuf::from("/test/path"));
        assert_eq!(root.origin.source_id, "local");
        assert!(root.platform.is_none());
        assert!(root.workspace_rewrites.is_empty());
    }

    #[test]
    fn scan_root_remote_sets_origin() {
        let origin = Origin {
            source_id: "work-laptop".to_string(),
            kind: crate::sources::provenance::SourceKind::Ssh,
            host: Some("work.local".to_string()),
        };
        let root = ScanRoot::remote(
            PathBuf::from("/data/remotes/work"),
            origin.clone(),
            Some(Platform::Linux),
        );
        assert_eq!(root.origin.source_id, "work-laptop");
        assert_eq!(root.platform, Some(Platform::Linux));
    }

    #[test]
    fn scan_root_with_rewrite_adds_rule() {
        let root =
            ScanRoot::local(PathBuf::from("/test")).with_rewrite("/home/user", "/Users/local");
        assert_eq!(root.workspace_rewrites.len(), 1);
        assert_eq!(root.workspace_rewrites[0].from, "/home/user");
        assert_eq!(root.workspace_rewrites[0].to, "/Users/local");
    }

    #[test]
    fn scan_root_rewrite_workspace_applies_rules() {
        let root = ScanRoot::local(PathBuf::from("/test"))
            .with_rewrite("/home/user/projects", "/Users/me/projects")
            .with_rewrite("/home/user", "/Users/me");

        // Longest prefix match
        assert_eq!(
            root.rewrite_workspace("/home/user/projects/myapp", None),
            "/Users/me/projects/myapp"
        );

        // Shorter prefix match
        assert_eq!(
            root.rewrite_workspace("/home/user/other", None),
            "/Users/me/other"
        );

        // No match
        assert_eq!(root.rewrite_workspace("/opt/data", None), "/opt/data");
    }

    #[test]
    fn scan_root_rewrite_with_agent_filter() {
        let mut root = ScanRoot::local(PathBuf::from("/test"));
        root.workspace_rewrites
            .push(PathMapping::new("/home/user", "/Users/me"));
        root.workspace_rewrites.push(PathMapping::with_agents(
            "/home/user/projects",
            "/Volumes/Work",
            vec!["claude-code".into()],
        ));

        // With claude-code agent, uses agent-specific mapping
        assert_eq!(
            root.rewrite_workspace("/home/user/projects/app", Some("claude-code")),
            "/Volumes/Work/app"
        );

        // With other agent, falls back to general mapping
        assert_eq!(
            root.rewrite_workspace("/home/user/projects/app", Some("cursor")),
            "/Users/me/projects/app"
        );
    }

    #[test]
    fn scan_context_local_default_has_empty_roots() {
        let ctx = ScanContext::local_default(PathBuf::from("/data"), None);
        assert_eq!(ctx.data_dir, PathBuf::from("/data"));
        assert!(ctx.scan_roots.is_empty());
        assert!(ctx.use_default_detection());
    }

    #[test]
    fn scan_context_with_roots_sets_roots() {
        let roots = vec![ScanRoot::local(PathBuf::from("/test"))];
        let ctx = ScanContext::with_roots(PathBuf::from("/data"), roots, Some(1000));
        assert_eq!(ctx.scan_roots.len(), 1);
        assert!(!ctx.use_default_detection());
        assert_eq!(ctx.since_ts, Some(1000));
    }

    // =========================================================================
    // Timestamp parsing edge cases (bead yln.4)
    // =========================================================================

    #[test]
    fn parse_timestamp_i64_milliseconds() {
        let val = serde_json::json!(1700000000000_i64);
        assert_eq!(super::parse_timestamp(&val), Some(1700000000000));
    }

    #[test]
    fn parse_timestamp_iso8601_with_fractional() {
        let val = serde_json::json!("2025-11-12T18:31:32.217Z");
        let result = super::parse_timestamp(&val);
        assert!(result.is_some());
        // Should be in milliseconds range for 2025
        assert!(result.unwrap() > 1700000000000);
    }

    #[test]
    fn parse_timestamp_iso8601_without_fractional() {
        let val = serde_json::json!("2025-11-12T18:31:32Z");
        let result = super::parse_timestamp(&val);
        assert!(result.is_some());
        assert!(result.unwrap() > 1700000000000);
    }

    #[test]
    fn parse_timestamp_rfc3339_with_offset() {
        let val = serde_json::json!("2025-11-12T18:31:32+00:00");
        let result = super::parse_timestamp(&val);
        assert!(result.is_some());
    }

    #[test]
    fn parse_timestamp_null_returns_none() {
        let val = serde_json::json!(null);
        assert_eq!(super::parse_timestamp(&val), None);
    }

    #[test]
    fn parse_timestamp_invalid_string_returns_none() {
        let val = serde_json::json!("not a timestamp");
        assert_eq!(super::parse_timestamp(&val), None);
    }

    #[test]
    fn parse_timestamp_empty_string_returns_none() {
        let val = serde_json::json!("");
        assert_eq!(super::parse_timestamp(&val), None);
    }

    #[test]
    fn parse_timestamp_object_returns_none() {
        let val = serde_json::json!({"timestamp": 1234});
        assert_eq!(super::parse_timestamp(&val), None);
    }

    #[test]
    fn parse_timestamp_negative_i64() {
        // Negative timestamps (before 1970) should still work
        let val = serde_json::json!(-1000_i64);
        assert_eq!(super::parse_timestamp(&val), Some(-1000));
    }

    #[test]
    fn parse_timestamp_zero() {
        let val = serde_json::json!(0_i64);
        assert_eq!(super::parse_timestamp(&val), Some(0));
    }

    // =========================================================================
    // Content flattening edge cases (bead yln.4)
    // =========================================================================

    #[test]
    fn flatten_content_plain_string() {
        let val = serde_json::json!("Hello world");
        assert_eq!(super::flatten_content(&val), "Hello world");
    }

    #[test]
    fn flatten_content_text_block_array() {
        let val = serde_json::json!([
            {"type": "text", "text": "Part 1"},
            {"type": "text", "text": "Part 2"}
        ]);
        let result = super::flatten_content(&val);
        assert!(result.contains("Part 1"));
        assert!(result.contains("Part 2"));
    }

    #[test]
    fn flatten_content_tool_use_block() {
        let val = serde_json::json!([
            {"type": "tool_use", "name": "Read", "input": {"path": "/test"}}
        ]);
        let result = super::flatten_content(&val);
        assert!(result.contains("Read"));
    }

    #[test]
    fn flatten_content_mixed_blocks() {
        let val = serde_json::json!([
            {"type": "text", "text": "I'll read the file"},
            {"type": "tool_use", "name": "Read", "input": {"path": "/test"}}
        ]);
        let result = super::flatten_content(&val);
        assert!(result.contains("I'll read the file"));
        assert!(result.contains("Read"));
    }

    #[test]
    fn flatten_content_input_text_block() {
        let val = serde_json::json!([
            {"type": "input_text", "text": "User input here"}
        ]);
        assert!(super::flatten_content(&val).contains("User input here"));
    }

    #[test]
    fn flatten_content_null_returns_empty() {
        let val = serde_json::json!(null);
        assert!(super::flatten_content(&val).is_empty());
    }

    #[test]
    fn flatten_content_empty_array() {
        let val = serde_json::json!([]);
        assert!(super::flatten_content(&val).is_empty());
    }

    #[test]
    fn flatten_content_plain_string_array() {
        // Handle arrays of plain strings (e.g., ["Hello", "World"])
        let val = serde_json::json!(["Hello", "World"]);
        let result = super::flatten_content(&val);
        assert!(result.contains("Hello"));
        assert!(result.contains("World"));
    }

    #[test]
    fn flatten_content_empty_string() {
        let val = serde_json::json!("");
        assert!(super::flatten_content(&val).is_empty());
    }

    #[test]
    fn flatten_content_number_returns_empty() {
        let val = serde_json::json!(42);
        assert!(super::flatten_content(&val).is_empty());
    }

    #[test]
    fn flatten_content_whitespace_only() {
        let val = serde_json::json!("   \n\t  ");
        assert_eq!(super::flatten_content(&val), "   \n\t  ");
    }

    // =========================================================================
    // NormalizedMessage construction (bead yln.4)
    // =========================================================================

    #[test]
    fn normalized_message_default_fields() {
        let msg = NormalizedMessage {
            idx: 0,
            role: "user".into(),
            author: None,
            created_at: None,
            content: "test".into(),
            extra: serde_json::json!({}),
            snippets: vec![],
        };
        assert_eq!(msg.role, "user");
        assert!(msg.author.is_none());
        assert!(msg.created_at.is_none());
    }

    #[test]
    fn normalized_message_with_all_fields() {
        let msg = NormalizedMessage {
            idx: 5,
            role: "assistant".into(),
            author: Some("claude".into()),
            created_at: Some(1700000000000),
            content: "Response text".into(),
            extra: serde_json::json!({"model": "claude-3"}),
            snippets: vec![NormalizedSnippet {
                file_path: Some("test.rs".into()),
                start_line: Some(10),
                end_line: Some(20),
                language: Some("rust".into()),
                snippet_text: Some("fn test()".into()),
            }],
        };
        assert_eq!(msg.idx, 5);
        assert_eq!(msg.author, Some("claude".into()));
        assert_eq!(msg.snippets.len(), 1);
    }

    // =========================================================================
    // NormalizedConversation construction (bead yln.4)
    // =========================================================================

    #[test]
    fn normalized_conversation_minimal() {
        let conv = NormalizedConversation {
            agent_slug: "test_agent".into(),
            external_id: None,
            title: None,
            workspace: None,
            source_path: PathBuf::from("/test/session.jsonl"),
            started_at: None,
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![],
        };
        assert_eq!(conv.agent_slug, "test_agent");
        assert!(conv.messages.is_empty());
    }

    #[test]
    fn normalized_conversation_with_messages() {
        let conv = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: Some("session-123".into()),
            title: Some("Test session".into()),
            workspace: Some(PathBuf::from("/home/user/project")),
            source_path: PathBuf::from("/data/session.jsonl"),
            started_at: Some(1700000000000),
            ended_at: Some(1700000060000),
            metadata: serde_json::json!({"tokens": 500}),
            messages: vec![
                NormalizedMessage {
                    idx: 0,
                    role: "user".into(),
                    author: None,
                    created_at: Some(1700000000000),
                    content: "Hello".into(),
                    extra: serde_json::json!({}),
                    snippets: vec![],
                },
                NormalizedMessage {
                    idx: 1,
                    role: "assistant".into(),
                    author: None,
                    created_at: Some(1700000010000),
                    content: "Hi there".into(),
                    extra: serde_json::json!({}),
                    snippets: vec![],
                },
            ],
        };
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.workspace, Some(PathBuf::from("/home/user/project")));
    }

    // =========================================================================
    // DetectionResult (bead yln.4)
    // =========================================================================

    #[test]
    fn detection_result_not_found() {
        let result = DetectionResult::not_found();
        assert!(!result.detected);
        assert!(result.evidence.is_empty());
    }

    #[test]
    fn detection_result_found() {
        let result = DetectionResult {
            detected: true,
            evidence: vec!["found ~/.codex".into(), "has sessions/".into()],
            root_paths: vec![],
        };
        assert!(result.detected);
        assert_eq!(result.evidence.len(), 2);
    }
}
