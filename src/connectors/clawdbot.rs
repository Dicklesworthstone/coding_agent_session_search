//! Clawdbot connector for JSONL session files.
//!
//! Clawdbot (https://clawdbot.com) is a personal AI assistant platform that runs
//! locally and connects to multiple messaging providers (WhatsApp, Telegram, etc.).
//!
//! Sessions are stored at `~/.clawdbot/agents/{agent_name}/sessions/` as JSONL files.
//!
//! Directory structure:
//!   - ~/.clawdbot/agents/main/sessions/{session-uuid}.jsonl
//!   - Each agent (main, etc.) has its own sessions directory
//!
//! JSONL format (version 3):
//!   - First line: {"type":"session","version":3,"id":"...","timestamp":"...","cwd":"..."}
//!   - Messages: {"type":"message","id":"...","parentId":"...","timestamp":"...","message":{...}}
//!   - message.role: "user", "assistant", or "toolResult"
//!   - message.content: array of {type: "text"|"thinking"|"toolCall"|"toolResult", ...}

use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    file_modified_since, flatten_content, parse_timestamp, Connector, DetectionResult,
    NormalizedConversation, NormalizedMessage, ScanContext,
};

pub struct ClawdbotConnector;

impl Default for ClawdbotConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl ClawdbotConnector {
    pub fn new() -> Self {
        Self
    }

    /// Get the Clawdbot agents directory.
    /// Clawdbot stores sessions in ~/.clawdbot/agents/{agent}/sessions/
    fn agents_root() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".clawdbot/agents"))
    }
}

impl Connector for ClawdbotConnector {
    fn detect(&self) -> DetectionResult {
        if let Some(root) = Self::agents_root() {
            if root.exists() {
                return DetectionResult {
                    detected: true,
                    evidence: vec![format!("found {}", root.display())],
                    root_paths: vec![root],
                };
            }
        }
        DetectionResult::not_found()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Determine scan root
        let root = if ctx.use_default_detection() {
            // First check if data_dir looks like clawdbot storage (for testing)
            if looks_like_clawdbot_storage(&ctx.data_dir) && ctx.data_dir.exists() {
                ctx.data_dir.clone()
            } else {
                // Fall back to default agents root
                match Self::agents_root() {
                    Some(r) if r.exists() => r,
                    _ => return Ok(Vec::new()),
                }
            }
        } else {
            // Check scan_roots for clawdbot agents
            let clawdbot_root = ctx.scan_roots.iter().find_map(|sr| {
                let clawdbot_path = sr.path.join(".clawdbot/agents");
                if clawdbot_path.exists() {
                    Some(clawdbot_path)
                } else if looks_like_clawdbot_storage(&sr.path) {
                    Some(sr.path.clone())
                } else {
                    None
                }
            });
            match clawdbot_root {
                Some(r) => r,
                None => return Ok(Vec::new()),
            }
        };

        if !root.exists() {
            return Ok(Vec::new());
        }

        let mut convs = Vec::new();

        for entry in WalkDir::new(&root).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }

            // Only process .jsonl files
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }

            // Skip files not modified since last scan (incremental indexing)
            if !file_modified_since(path, ctx.since_ts) {
                continue;
            }

            match parse_clawdbot_session(path) {
                Ok(Some(conv)) => convs.push(conv),
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(path = %path.display(), error = %e, "clawdbot parse error");
                }
            }
        }

        Ok(convs)
    }
}

/// Check if a directory looks like Clawdbot storage
fn looks_like_clawdbot_storage(path: &Path) -> bool {
    let path_str = path.to_string_lossy().to_lowercase();
    path_str.contains("clawdbot") && path_str.contains("agents")
}

/// Extract agent name from session path.
/// e.g., ~/.clawdbot/agents/main/sessions/uuid.jsonl -> "main"
fn extract_agent_name(path: &Path) -> Option<String> {
    // Walk up path looking for "sessions" directory, agent name is parent of that
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir.file_name().and_then(|n| n.to_str()) == Some("sessions") {
            return dir
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(String::from);
        }
        current = dir.parent();
    }
    None
}

/// Parse a Clawdbot session JSONL file into a NormalizedConversation.
fn parse_clawdbot_session(path: &Path) -> Result<Option<NormalizedConversation>> {
    let file =
        fs::File::open(path).with_context(|| format!("open session file {}", path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut messages = Vec::new();
    let mut session_id: Option<String> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut session_version: Option<i64> = None;
    let mut started_at: Option<i64> = None;
    let mut ended_at: Option<i64> = None;

    // Extract agent name from path
    let agent_name = extract_agent_name(path);

    for line_res in reader.lines() {
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

        let entry_type = val.get("type").and_then(|v| v.as_str());

        match entry_type {
            Some("session") => {
                // Extract session metadata from header line
                session_id = val.get("id").and_then(|v| v.as_str()).map(String::from);
                session_version = val.get("version").and_then(|v| v.as_i64());
                workspace = val.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);

                // Session timestamp as started_at
                if let Some(ts) = val.get("timestamp").and_then(parse_timestamp) {
                    started_at = Some(ts);
                }
            }
            Some("message") => {
                // Parse timestamp
                let created = val.get("timestamp").and_then(parse_timestamp);

                // Track session time bounds
                if started_at.is_none() {
                    started_at = created;
                }
                ended_at = created.or(ended_at);

                // Extract role from message.role
                let role = val
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");

                // Skip toolResult messages - they're tool outputs, not conversation
                if role == "toolResult" {
                    continue;
                }

                // Extract content from message.content
                let content_val = val.get("message").and_then(|m| m.get("content"));
                let content_str = content_val
                    .map(flatten_clawdbot_content)
                    .unwrap_or_default();

                // Skip entries with empty content
                if content_str.trim().is_empty() {
                    continue;
                }

                // Extract model for author field
                let author = val
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(|v| v.as_str())
                    .map(String::from);

                messages.push(NormalizedMessage {
                    idx: 0, // Will be reassigned after collection
                    role: role.to_string(),
                    author,
                    created_at: created,
                    content: content_str,
                    extra: val,
                    snippets: Vec::new(),
                });
            }
            // Skip other types
            _ => {}
        }
    }

    // Reassign sequential indices
    super::reindex_messages(&mut messages);

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
        agent_slug: "clawdbot".into(),
        external_id: session_id
            .clone()
            .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(String::from)),
        title,
        workspace,
        source_path: path.to_path_buf(),
        started_at,
        ended_at,
        metadata: serde_json::json!({
            "source": "clawdbot",
            "sessionId": session_id,
            "version": session_version,
            "agent": agent_name,
        }),
        messages,
    }))
}

/// Flatten Clawdbot content array to text.
///
/// Clawdbot content is an array with items like:
/// - {"type": "text", "text": "..."}
/// - {"type": "thinking", "thinking": "..."}
/// - {"type": "toolCall", "name": "...", "arguments": {...}}
fn flatten_clawdbot_content(val: &Value) -> String {
    // Direct string content
    if let Some(s) = val.as_str() {
        return s.to_string();
    }

    // Array of content blocks
    if let Some(arr) = val.as_array() {
        let mut result = String::new();
        for item in arr {
            if let Some(text) = extract_clawdbot_content_part(item) {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(&text);
            }
        }
        return result;
    }

    // Fallback to generic flatten_content
    flatten_content(val)
}

/// Extract text content from a single Clawdbot content block.
fn extract_clawdbot_content_part(item: &Value) -> Option<String> {
    let item_type = item.get("type").and_then(|v| v.as_str());

    match item_type {
        Some("text") => item.get("text").and_then(|v| v.as_str()).map(String::from),
        Some("thinking") => {
            // Include thinking content for searchability
            item.get("thinking")
                .and_then(|v| v.as_str())
                .map(|t| format!("[Thinking: {}]", t.chars().take(200).collect::<String>()))
        }
        Some("toolCall") => {
            // Include tool name for searchability
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            Some(format!("[Tool: {name}]"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // =========================================================================
    // Constructor tests
    // =========================================================================

    #[test]
    fn new_creates_connector() {
        let connector = ClawdbotConnector::new();
        let _ = connector;
    }

    #[test]
    fn default_creates_connector() {
        let connector = ClawdbotConnector;
        let _ = connector;
    }

    #[test]
    fn agents_root_returns_clawdbot_agents_path() {
        if let Some(root) = ClawdbotConnector::agents_root() {
            assert!(root.ends_with(".clawdbot/agents"));
        }
    }

    // =========================================================================
    // Agent name extraction tests
    // =========================================================================

    #[test]
    fn extract_agent_name_from_path() {
        let path = PathBuf::from("/home/user/.clawdbot/agents/main/sessions/uuid.jsonl");
        assert_eq!(extract_agent_name(&path), Some("main".to_string()));
    }

    #[test]
    fn extract_agent_name_custom_agent() {
        let path = PathBuf::from("/home/user/.clawdbot/agents/work/sessions/abc.jsonl");
        assert_eq!(extract_agent_name(&path), Some("work".to_string()));
    }

    #[test]
    fn extract_agent_name_no_sessions() {
        let path = PathBuf::from("/home/user/other/path/file.jsonl");
        assert_eq!(extract_agent_name(&path), None);
    }

    // =========================================================================
    // Detection tests
    // =========================================================================

    #[test]
    fn detect_not_found_without_agents_dir() {
        let connector = ClawdbotConnector::new();
        let result = connector.detect();
        // Just verify detect() doesn't panic
        let _ = result.detected;
    }

    // =========================================================================
    // JSONL parsing tests
    // =========================================================================

    fn create_clawdbot_storage(dir: &TempDir) -> PathBuf {
        let storage = dir.path().join(".clawdbot").join("agents");
        fs::create_dir_all(&storage).unwrap();
        storage
    }

    fn write_session_file(storage: &Path, agent: &str, session_id: &str, lines: &[&str]) {
        let session_dir = storage.join(agent).join("sessions");
        fs::create_dir_all(&session_dir).unwrap();
        let file_path = session_dir.join(format!("{session_id}.jsonl"));
        fs::write(&file_path, lines.join("\n")).unwrap();
    }

    #[test]
    fn scan_parses_session_header_and_messages() {
        let dir = TempDir::new().unwrap();
        let storage = create_clawdbot_storage(&dir);

        let lines = vec![
            r#"{"type":"session","version":3,"id":"sess-001","timestamp":"2025-12-01T10:00:00Z","cwd":"/home/user/project"}"#,
            r#"{"type":"message","id":"msg1","timestamp":"2025-12-01T10:00:05Z","message":{"role":"user","content":[{"type":"text","text":"Hello Clawdbot"}]}}"#,
            r#"{"type":"message","id":"msg2","timestamp":"2025-12-01T10:00:10Z","message":{"role":"assistant","content":[{"type":"text","text":"Hello! How can I help?"}],"model":"claude-opus-4-5"}}"#,
        ];
        write_session_file(&storage, "main", "sess-001", &lines);

        let connector = ClawdbotConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].agent_slug, "clawdbot");
        assert_eq!(
            convs[0].workspace,
            Some(PathBuf::from("/home/user/project"))
        );
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[0].content, "Hello Clawdbot");
        assert_eq!(convs[0].messages[1].role, "assistant");
        assert_eq!(
            convs[0].messages[1].author,
            Some("claude-opus-4-5".to_string())
        );
    }

    #[test]
    fn scan_extracts_session_metadata() {
        let dir = TempDir::new().unwrap();
        let storage = create_clawdbot_storage(&dir);

        let lines = vec![
            r#"{"type":"session","version":3,"id":"sess-meta","timestamp":"2025-12-01T10:00:00Z","cwd":"/projects/app"}"#,
            r#"{"type":"message","id":"msg1","timestamp":"2025-12-01T10:00:05Z","message":{"role":"user","content":[{"type":"text","text":"Test"}]}}"#,
        ];
        write_session_file(&storage, "main", "sess-meta", &lines);

        let connector = ClawdbotConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].metadata["sessionId"], "sess-meta");
        assert_eq!(convs[0].metadata["version"], 3);
        assert_eq!(convs[0].metadata["agent"], "main");
        assert_eq!(convs[0].external_id, Some("sess-meta".to_string()));
    }

    #[test]
    fn scan_handles_thinking_content() {
        let dir = TempDir::new().unwrap();
        let storage = create_clawdbot_storage(&dir);

        let lines = vec![
            r#"{"type":"session","version":3,"id":"sess-think","cwd":"/test"}"#,
            r#"{"type":"message","id":"msg1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me analyze this problem"},{"type":"text","text":"Here is my response"}]}}"#,
        ];
        write_session_file(&storage, "main", "sess-think", &lines);

        let connector = ClawdbotConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        let content = &convs[0].messages[0].content;
        assert!(content.contains("Thinking:"));
        assert!(content.contains("Here is my response"));
    }

    #[test]
    fn scan_handles_tool_calls() {
        let dir = TempDir::new().unwrap();
        let storage = create_clawdbot_storage(&dir);

        let lines = vec![
            r#"{"type":"session","version":3,"id":"sess-tool","cwd":"/test"}"#,
            r#"{"type":"message","id":"msg1","message":{"role":"assistant","content":[{"type":"text","text":"Let me read that file"},{"type":"toolCall","name":"read","arguments":{"path":"/test.txt"}}]}}"#,
        ];
        write_session_file(&storage, "main", "sess-tool", &lines);

        let connector = ClawdbotConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        let content = &convs[0].messages[0].content;
        assert!(content.contains("Let me read that file"));
        assert!(content.contains("[Tool: read]"));
    }

    #[test]
    fn scan_skips_tool_result_messages() {
        let dir = TempDir::new().unwrap();
        let storage = create_clawdbot_storage(&dir);

        let lines = vec![
            r#"{"type":"session","version":3,"id":"sess-result","cwd":"/test"}"#,
            r#"{"type":"message","id":"msg1","message":{"role":"user","content":[{"type":"text","text":"Read the file"}]}}"#,
            r#"{"type":"message","id":"msg2","message":{"role":"toolResult","content":[{"type":"text","text":"file contents here"}]}}"#,
            r#"{"type":"message","id":"msg3","message":{"role":"assistant","content":[{"type":"text","text":"The file contains..."}]}}"#,
        ];
        write_session_file(&storage, "main", "sess-result", &lines);

        let connector = ClawdbotConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Should only have user and assistant messages, not toolResult
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[1].role, "assistant");
    }

    #[test]
    fn scan_generates_title_from_first_user_message() {
        let dir = TempDir::new().unwrap();
        let storage = create_clawdbot_storage(&dir);

        let lines = vec![
            r#"{"type":"session","version":3,"id":"sess-title","cwd":"/test"}"#,
            r#"{"type":"message","id":"msg1","message":{"role":"user","content":[{"type":"text","text":"Help me build a web app\nWith authentication"}]}}"#,
        ];
        write_session_file(&storage, "main", "sess-title", &lines);

        let connector = ClawdbotConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Title should be first line only
        assert_eq!(convs[0].title, Some("Help me build a web app".to_string()));
    }

    #[test]
    fn scan_empty_messages_returns_none() {
        let dir = TempDir::new().unwrap();
        let storage = create_clawdbot_storage(&dir);

        let lines = vec![r#"{"type":"session","version":3,"id":"sess-empty","cwd":"/test"}"#];
        write_session_file(&storage, "main", "sess-empty", &lines);

        let connector = ClawdbotConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert!(convs.is_empty());
    }

    #[test]
    fn scan_multiple_agents() {
        let dir = TempDir::new().unwrap();
        let storage = create_clawdbot_storage(&dir);

        let lines = vec![
            r#"{"type":"session","version":3,"id":"sess-main","cwd":"/main"}"#,
            r#"{"type":"message","id":"msg1","message":{"role":"user","content":[{"type":"text","text":"Main agent message"}]}}"#,
        ];
        write_session_file(&storage, "main", "sess-main", &lines);

        let lines2 = vec![
            r#"{"type":"session","version":3,"id":"sess-work","cwd":"/work"}"#,
            r#"{"type":"message","id":"msg1","message":{"role":"user","content":[{"type":"text","text":"Work agent message"}]}}"#,
        ];
        write_session_file(&storage, "work", "sess-work", &lines2);

        let connector = ClawdbotConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 2);
        // Check different agents are captured
        let agents: Vec<_> = convs
            .iter()
            .map(|c| c.metadata["agent"].as_str().unwrap())
            .collect();
        assert!(agents.contains(&"main"));
        assert!(agents.contains(&"work"));
    }
}
