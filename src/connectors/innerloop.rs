//! InnerLoop SDK connector.
//!
//! Reads sessions from InnerLoop's v2 JSONL format stored in XDG-compliant directories.
//! Default location: ~/.local/share/innerloop/sessions/

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

pub struct InnerLoopConnector;

impl Default for InnerLoopConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl InnerLoopConnector {
    pub fn new() -> Self {
        Self
    }

    /// Get the default InnerLoop sessions directory (XDG-compliant).
    fn sessions_root() -> PathBuf {
        // Check INNERLOOP_DATA_DIR first
        if let Ok(data_dir) = std::env::var("INNERLOOP_DATA_DIR") {
            return PathBuf::from(data_dir).join("sessions");
        }
        // Fall back to XDG_DATA_HOME
        let xdg_data = std::env::var("XDG_DATA_HOME")
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(".local/share")
                    .to_string_lossy()
                    .to_string()
            });
        PathBuf::from(xdg_data).join("innerloop").join("sessions")
    }
}

impl Connector for InnerLoopConnector {
    fn detect(&self) -> DetectionResult {
        let root = Self::sessions_root();
        if root.exists() {
            DetectionResult {
                detected: true,
                evidence: vec![format!("found {}", root.display())],
                root_paths: vec![root],
            }
        } else {
            DetectionResult::not_found()
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Determine root directory
        let looks_like_root = |path: &PathBuf| {
            path.join("sessions").exists()
                || path
                    .file_name()
                    .is_some_and(|n| n.to_str().unwrap_or("").contains("innerloop"))
        };

        let mut root = if ctx.use_default_detection() {
            if looks_like_root(&ctx.data_dir) {
                ctx.data_dir.clone()
            } else {
                Self::sessions_root()
            }
        } else {
            ctx.data_dir.clone()
        };

        if root.is_file() {
            root = root.parent().unwrap_or(&root).to_path_buf();
        }
        if !ctx.use_default_detection() && !looks_like_root(&root) {
            return Ok(Vec::new());
        }
        if !root.exists() {
            return Ok(Vec::new());
        }

        let mut convs = Vec::new();
        let mut file_count = 0;

        for entry in WalkDir::new(&root).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let ext = entry.path().extension().and_then(|s| s.to_str());
            if ext != Some("jsonl") {
                continue;
            }
            // Skip files not modified since last scan
            if !crate::connectors::file_modified_since(entry.path(), ctx.since_ts) {
                continue;
            }
            file_count += 1;
            if file_count <= 3 {
                tracing::debug!(path = %entry.path().display(), "innerloop found file");
            }

            // Parse the session file
            match parse_innerloop_session(entry.path()) {
                Ok(Some(conv)) => convs.push(conv),
                Ok(None) => {
                    if file_count <= 3 {
                        tracing::debug!(path = %entry.path().display(), "innerloop no messages extracted");
                    }
                }
                Err(e) => {
                    tracing::debug!(path = %entry.path().display(), error = %e, "innerloop parse error");
                }
            }
        }

        Ok(convs)
    }
}

/// Parse an InnerLoop v2 JSONL session file into a NormalizedConversation.
fn parse_innerloop_session(path: &std::path::Path) -> Result<Option<NormalizedConversation>> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut messages = Vec::new();
    let mut session_id: Option<String> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut started_at: Option<i64> = None;
    let mut ended_at: Option<i64> = None;
    let mut model: Option<String> = None;
    let mut agent_name: Option<String> = None;
    let mut task_id: Option<String> = None;

    for line_res in std::io::BufRead::lines(reader) {
        let line = match line_res {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let val: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Extract session metadata from first record
        if session_id.is_none() {
            session_id = val.get("session_id").and_then(|v| v.as_str()).map(String::from);
        }

        // Get timestamp (ts_ms in milliseconds)
        let ts = val.get("ts_ms").and_then(|v| v.as_i64());
        started_at = started_at.or(ts);
        ended_at = ts.or(ended_at);

        // Get record type
        let record_type = val.get("type").and_then(|v| v.as_str());

        match record_type {
            Some("meta") => {
                // Extract metadata from meta record
                if let Some(data) = val.get("data") {
                    model = data.get("model").and_then(|v| v.as_str()).map(String::from);

                    // Extract from context fields
                    if let Some(agent_ctx) = data.get("agent") {
                        agent_name = agent_ctx.get("name").and_then(|v| v.as_str()).map(String::from);
                    }
                    if let Some(task_ctx) = data.get("task") {
                        task_id = task_ctx.get("task_id").and_then(|v| v.as_str()).map(String::from);
                    }
                    if let Some(dirs_ctx) = data.get("directories") {
                        workspace = dirs_ctx.get("task_workdir").and_then(|v| v.as_str()).map(PathBuf::from);
                    }
                }
            }
            Some("message") => {
                // Extract message data
                if let Some(data) = val.get("data") {
                    let role = data.get("role").and_then(|v| v.as_str()).unwrap_or("agent");

                    // Only include user and assistant messages
                    if role != "user" && role != "assistant" {
                        continue;
                    }

                    // Flatten content array to string
                    let content_str = if let Some(content) = data.get("content") {
                        flatten_innerloop_content(content)
                    } else {
                        String::new()
                    };

                    if content_str.trim().is_empty() {
                        continue;
                    }

                    // Extract model from message if present
                    if model.is_none() {
                        model = data.get("model").and_then(|v| v.as_str()).map(String::from);
                    }

                    messages.push(NormalizedMessage {
                        idx: messages.len() as i64,
                        role: role.to_string(),
                        author: model.clone(),
                        created_at: ts,
                        content: content_str,
                        extra: val.clone(),
                        snippets: Vec::new(),
                    });
                }
            }
            Some("tool_call") => {
                // Include tool calls for searchability
                if let Some(data) = val.get("data") {
                    let tool_name = data.get("tool").and_then(|v| v.as_str()).unwrap_or("unknown");
                    let input = data.get("input");

                    // Create a searchable representation
                    let content_str = if let Some(inp) = input {
                        format!("[Tool: {} - {}]", tool_name, inp.to_string().chars().take(200).collect::<String>())
                    } else {
                        format!("[Tool: {}]", tool_name)
                    };

                    messages.push(NormalizedMessage {
                        idx: messages.len() as i64,
                        role: "assistant".to_string(),
                        author: model.clone(),
                        created_at: ts,
                        content: content_str,
                        extra: val.clone(),
                        snippets: Vec::new(),
                    });
                }
            }
            Some("tool_result") => {
                // Include tool results for searchability (truncated)
                if let Some(data) = val.get("data") {
                    let tool_name = data.get("tool").and_then(|v| v.as_str()).unwrap_or("unknown");
                    let output = data
                        .get("output")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    // Truncate long outputs
                    let truncated = if output.len() > 500 {
                        format!("{}...", &output[..500])
                    } else {
                        output.to_string()
                    };

                    let content_str = format!("[Tool Result: {}]\n{}", tool_name, truncated);

                    messages.push(NormalizedMessage {
                        idx: messages.len() as i64,
                        role: "tool".to_string(),
                        author: None,
                        created_at: ts,
                        content: content_str,
                        extra: val.clone(),
                        snippets: Vec::new(),
                    });
                }
            }
            _ => {
                // Skip other record types (usage, done, etc.)
            }
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }

    // Generate title from first user message
    let title = messages
        .iter()
        .find(|m| m.role == "user")
        .map(|m| {
            m.content
                .lines()
                .next()
                .unwrap_or(&m.content)
                .chars()
                .take(100)
                .collect::<String>()
        })
        .or_else(|| {
            // Fallback to workspace directory name
            workspace
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(String::from)
        });

    Ok(Some(NormalizedConversation {
        agent_slug: agent_name.unwrap_or_else(|| "innerloop".to_string()),
        external_id: session_id.clone(),
        title,
        workspace,
        source_path: path.to_path_buf(),
        started_at,
        ended_at,
        metadata: serde_json::json!({
            "source": "innerloop",
            "session_id": session_id,
            "task_id": task_id,
            "model": model
        }),
        messages,
    }))
}

/// Flatten InnerLoop content array to a searchable string.
fn flatten_innerloop_content(val: &Value) -> String {
    // Direct string content
    if let Some(s) = val.as_str() {
        return s.to_string();
    }

    // Array of content blocks
    if let Some(arr) = val.as_array() {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|item| {
                let item_type = item.get("type").and_then(|v| v.as_str());

                match item_type {
                    Some("text") => item.get("text").and_then(|v| v.as_str()).map(String::from),
                    Some("tool_use") => {
                        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                        Some(format!("[Tool: {}]", name))
                    }
                    Some("tool_result") => {
                        let content = item
                            .get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        // Truncate long tool results
                        let truncated = if content.len() > 200 {
                            format!("{}...", &content[..200])
                        } else {
                            content.to_string()
                        };
                        Some(format!("[Tool Result: {}]", truncated))
                    }
                    _ => None,
                }
            })
            .collect();
        return parts.join("\n");
    }

    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn new_creates_connector() {
        let connector = InnerLoopConnector::new();
        let _ = connector;
    }

    #[test]
    fn default_creates_connector() {
        let connector = InnerLoopConnector::default();
        let _ = connector;
    }

    #[test]
    fn sessions_root_returns_xdg_path() {
        let root = InnerLoopConnector::sessions_root();
        // Should end with innerloop/sessions
        assert!(root.to_string_lossy().contains("innerloop"));
        assert!(root.to_string_lossy().ends_with("sessions"));
    }

    #[test]
    fn detect_not_found_without_sessions_dir() {
        let connector = InnerLoopConnector::new();
        let result = connector.detect();
        // Just verify it doesn't panic
        let _ = result.detected;
    }

    #[test]
    fn scan_parses_v2_messages() {
        let dir = TempDir::new().unwrap();
        let innerloop_dir = dir.path().join("innerloop").join("sessions");
        fs::create_dir_all(&innerloop_dir).unwrap();

        let session_file = innerloop_dir.join("test-session.jsonl");
        let content = format!(
            "{}\n{}\n{}\n",
            json!({
                "schema_version": "2.0",
                "session_id": "TEST123",
                "event_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "seq": 0,
                "turn": 0,
                "ts_ms": 1700000000000_i64,
                "type": "meta",
                "data": {
                    "model": "claude-3-opus",
                    "agent": {"name": "test-agent"},
                    "directories": {"task_workdir": "/projects/myapp"}
                }
            }),
            json!({
                "schema_version": "2.0",
                "session_id": "TEST123",
                "event_id": "01ARZ3NDEKTSV4RRFFQ69G5FAW",
                "seq": 1,
                "turn": 1,
                "ts_ms": 1700000001000_i64,
                "type": "message",
                "data": {
                    "role": "user",
                    "content": [{"type": "text", "text": "Hello InnerLoop"}]
                }
            }),
            json!({
                "schema_version": "2.0",
                "session_id": "TEST123",
                "event_id": "01ARZ3NDEKTSV4RRFFQ69G5FAX",
                "seq": 2,
                "turn": 1,
                "ts_ms": 1700000002000_i64,
                "type": "message",
                "data": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "Hello! How can I help?"}]
                }
            })
        );
        fs::write(&session_file, content).unwrap();

        let connector = InnerLoopConnector::new();
        let parent = dir.path().join("innerloop");
        let ctx = ScanContext::local_default(parent, None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);

        let conv = &convs[0];
        assert_eq!(conv.agent_slug, "test-agent");
        assert_eq!(conv.external_id, Some("TEST123".to_string()));
        assert_eq!(conv.workspace, Some(PathBuf::from("/projects/myapp")));
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user");
        assert!(conv.messages[0].content.contains("Hello InnerLoop"));
        assert_eq!(conv.messages[1].role, "assistant");
    }

    #[test]
    fn scan_includes_tool_calls() {
        let dir = TempDir::new().unwrap();
        let innerloop_dir = dir.path().join("innerloop").join("sessions");
        fs::create_dir_all(&innerloop_dir).unwrap();

        let session_file = innerloop_dir.join("tools-session.jsonl");
        let content = format!(
            "{}\n{}\n",
            json!({
                "schema_version": "2.0",
                "session_id": "TOOLS123",
                "seq": 0,
                "turn": 1,
                "ts_ms": 1700000000000_i64,
                "type": "message",
                "data": {"role": "user", "content": [{"type": "text", "text": "Read the file"}]}
            }),
            json!({
                "schema_version": "2.0",
                "session_id": "TOOLS123",
                "seq": 1,
                "turn": 1,
                "ts_ms": 1700000001000_i64,
                "type": "tool_call",
                "data": {"tool": "read_file", "input": {"path": "/test.py"}}
            })
        );
        fs::write(&session_file, content).unwrap();

        let connector = InnerLoopConnector::new();
        let parent = dir.path().join("innerloop");
        let ctx = ScanContext::local_default(parent, None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 2);
        assert!(convs[0].messages[1].content.contains("read_file"));
    }

    #[test]
    fn flatten_content_handles_text_blocks() {
        let content = json!([
            {"type": "text", "text": "First part"},
            {"type": "text", "text": "Second part"}
        ]);
        let result = flatten_innerloop_content(&content);
        assert!(result.contains("First part"));
        assert!(result.contains("Second part"));
    }

    #[test]
    fn flatten_content_handles_string() {
        let content = json!("Simple string content");
        let result = flatten_innerloop_content(&content);
        assert_eq!(result, "Simple string content");
    }

    #[test]
    fn flatten_content_handles_tool_use() {
        let content = json!([
            {"type": "text", "text": "I'll read the file"},
            {"type": "tool_use", "name": "read_file", "input": {"path": "/test"}}
        ]);
        let result = flatten_innerloop_content(&content);
        assert!(result.contains("I'll read the file"));
        assert!(result.contains("[Tool: read_file]"));
    }
}
