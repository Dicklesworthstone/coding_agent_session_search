//! Vibe (Mistral) connector for JSONL file-based session storage.
//!
//! Vibe stores sessions at `~/.vibe/logs/session/*/`:
//!   - messages.jsonl - JSONL with role/content per line
//!   - meta.json - Session metadata (title, timestamps, workspace, etc.)

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

pub struct VibeConnector;

impl Default for VibeConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl VibeConnector {
    pub fn new() -> Self {
        Self
    }

    /// Get the Vibe logs directory.
    fn logs_root() -> Option<PathBuf> {
        // Check for env override first (useful for testing)
        if let Ok(path) = dotenvy::var("VIBE_LOGS_ROOT") {
            let p = PathBuf::from(path);
            if p.exists() {
                return Some(p);
            }
        }

        // Primary location: ~/.vibe/logs/session
        if let Some(home) = dirs::home_dir() {
            let session_dir = home.join(".vibe/logs/session");
            if session_dir.exists() {
                return Some(session_dir);
            }
        }

        None
    }
}

// ============================================================================
// JSON Structures for Vibe Storage
// ============================================================================

/// Session metadata from meta.json
#[derive(Debug, Deserialize)]
struct SessionMeta {
    session_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    start_time: Option<String>,
    #[serde(default)]
    end_time: Option<String>,
    #[serde(default)]
    environment: Option<SessionEnvironment>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    stats: Option<SessionStats>,
}

#[derive(Debug, Deserialize)]
struct SessionEnvironment {
    #[serde(default)]
    working_directory: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionStats {
    #[serde(default)]
    steps: Option<i64>,
    #[serde(default)]
    total_messages: Option<i64>,
}

/// Message from messages.jsonl
#[derive(Debug, Deserialize)]
struct VibeMessage {
    role: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    message_id: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
    #[serde(default)]
    tool_call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ToolCall {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type", default)]
    call_type: Option<String>,
    #[serde(default)]
    function: Option<ToolFunction>,
}

#[derive(Debug, Deserialize)]
struct ToolFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

impl Connector for VibeConnector {
    fn detect(&self) -> DetectionResult {
        if let Some(logs_root) = Self::logs_root() {
            DetectionResult {
                detected: true,
                evidence: vec![format!("found {}", logs_root.display())],
                root_paths: vec![logs_root],
            }
        } else {
            DetectionResult::not_found()
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Determine the session root
        let session_root = if ctx.use_default_detection() {
            if ctx.data_dir.exists() && looks_like_vibe_storage(&ctx.data_dir) {
                ctx.data_dir.clone()
            } else {
                match Self::logs_root() {
                    Some(root) => root,
                    None => return Ok(Vec::new()),
                }
            }
        } else if ctx.data_dir.exists() && looks_like_vibe_storage(&ctx.data_dir) {
            ctx.data_dir.clone()
        } else {
            return Ok(Vec::new());
        };

        if !session_root.exists() {
            return Ok(Vec::new());
        }

        // Find all session directories (contain messages.jsonl)
        let session_dirs: Vec<PathBuf> = WalkDir::new(&session_root)
            .max_depth(2)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_dir())
            .filter(|e| e.path().join("messages.jsonl").exists())
            .map(|e| e.path().to_path_buf())
            .collect();

        let mut convs = Vec::new();

        for session_dir in session_dirs {
            let messages_path = session_dir.join("messages.jsonl");
            let meta_path = session_dir.join("meta.json");

            // Check if modified since threshold
            if !session_has_updates(&messages_path, &meta_path, ctx.since_ts) {
                continue;
            }

            // Parse meta.json if it exists
            let meta = if meta_path.exists() {
                parse_meta_file(&meta_path).ok()
            } else {
                None
            };

            // Parse messages.jsonl
            let messages = match load_messages(&messages_path) {
                Ok(msgs) => msgs,
                Err(e) => {
                    tracing::debug!(
                        "vibe: failed to parse messages {}: {e}",
                        messages_path.display()
                    );
                    continue;
                }
            };

            if messages.is_empty() {
                continue;
            }

            // Extract session ID from meta or directory name
            let session_id = meta
                .as_ref()
                .map(|m| m.session_id.clone())
                .unwrap_or_else(|| {
                    session_dir
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(String::from)
                        .unwrap_or_else(|| "unknown".to_string())
                });

            // Extract workspace from meta
            let workspace = meta
                .as_ref()
                .and_then(|m| m.environment.as_ref())
                .and_then(|e| e.working_directory.as_ref())
                .map(PathBuf::from);

            // Extract timestamps
            let started_at = meta
                .as_ref()
                .and_then(|m| m.start_time.as_ref())
                .and_then(|s| parse_iso_timestamp(s));
            let ended_at = meta
                .as_ref()
                .and_then(|m| m.end_time.as_ref())
                .and_then(|s| parse_iso_timestamp(s));

            // Title from meta or first user message
            let title = meta.as_ref().and_then(|m| m.title.clone()).or_else(|| {
                messages
                    .iter()
                    .find(|m| m.role == "user")
                    .and_then(|m| m.content.lines().next())
                    .map(|s| s.chars().take(100).collect())
            });

            convs.push(NormalizedConversation {
                agent_slug: "vibe".into(),
                external_id: Some(session_id.clone()),
                title,
                workspace,
                source_path: messages_path.clone(),
                started_at,
                ended_at,
                metadata: serde_json::json!({
                    "session_id": session_id,
                    "username": meta.as_ref().and_then(|m| m.username.clone()),
                    "steps": meta.as_ref().and_then(|m| m.stats.as_ref()).and_then(|s| s.steps),
                }),
                messages,
            });
        }

        Ok(convs)
    }
}

/// Check if a directory looks like Vibe session storage
fn looks_like_vibe_storage(path: &Path) -> bool {
    let path_str = path.to_string_lossy().to_lowercase();
    path_str.contains(".vibe") || path_str.contains("vibe/logs")
}

/// Check if session has updates since timestamp
fn session_has_updates(messages_path: &Path, meta_path: &Path, since_ts: Option<i64>) -> bool {
    if since_ts.is_none() {
        return true;
    }

    crate::connectors::file_modified_since(messages_path, since_ts)
        || crate::connectors::file_modified_since(meta_path, since_ts)
}

/// Parse meta.json file
fn parse_meta_file(path: &Path) -> Result<SessionMeta> {
    let content =
        fs::read_to_string(path).with_context(|| format!("read meta file {}", path.display()))?;
    let meta: SessionMeta = serde_json::from_str(&content)
        .with_context(|| format!("parse meta JSON {}", path.display()))?;
    Ok(meta)
}

/// Parse ISO timestamp to milliseconds
fn parse_iso_timestamp(s: &str) -> Option<i64> {
    // Try various ISO formats
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    // Try without timezone (assume UTC)
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(dt.and_utc().timestamp_millis());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc().timestamp_millis());
    }
    None
}

/// Load all messages from messages.jsonl
fn load_messages(path: &Path) -> Result<Vec<NormalizedMessage>> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut messages = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    for (line_num, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        if line.trim().is_empty() {
            continue;
        }

        let msg: VibeMessage = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // Deduplicate by message_id (vibe sometimes has duplicate lines)
        if let Some(ref id) = msg.message_id {
            if !seen_ids.insert(id.clone()) {
                continue;
            }
        }

        // Build content from message
        let content = build_message_content(&msg);
        if content.trim().is_empty() {
            continue;
        }

        // Map role
        let role = match msg.role.as_str() {
            "user" => "user".to_string(),
            "assistant" => "assistant".to_string(),
            "tool" => "tool".to_string(),
            "system" => "system".to_string(),
            other => other.to_string(),
        };

        // Author for tool messages
        let author = if role == "tool" {
            msg.name.clone()
        } else {
            None
        };

        messages.push(NormalizedMessage {
            idx: line_num as i64,
            role,
            author,
            created_at: None, // Vibe doesn't store per-message timestamps
            content,
            extra: serde_json::json!({
                "message_id": msg.message_id,
                "tool_call_id": msg.tool_call_id,
            }),
            snippets: Vec::new(),
        });
    }

    // Reindex after deduplication
    super::reindex_messages(&mut messages);

    Ok(messages)
}

/// Build message content from VibeMessage
fn build_message_content(msg: &VibeMessage) -> String {
    let mut parts = Vec::new();

    // Add main content
    if let Some(ref content) = msg.content {
        if !content.trim().is_empty() {
            parts.push(content.clone());
        }
    }

    // Add tool calls for assistant messages
    if let Some(ref tool_calls) = msg.tool_calls {
        for tc in tool_calls {
            if let Some(ref func) = tc.function {
                let name = func.name.as_deref().unwrap_or("unknown");
                let args = func.arguments.as_deref().unwrap_or("");
                // Truncate long arguments
                let args_preview = if args.len() > 200 {
                    format!("{}...", &args[..200])
                } else {
                    args.to_string()
                };
                parts.push(format!("[Tool: {} - {}]", name, args_preview));
            }
        }
    }

    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn create_vibe_storage(dir: &TempDir) -> PathBuf {
        let session_dir = dir.path().join("session_test");
        fs::create_dir_all(&session_dir).unwrap();
        session_dir
    }

    fn write_messages(session_dir: &Path, messages: &[serde_json::Value]) {
        let path = session_dir.join("messages.jsonl");
        let content: String = messages
            .iter()
            .map(|m| m.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(path, content).unwrap();
    }

    fn write_meta(session_dir: &Path, meta: &serde_json::Value) {
        let path = session_dir.join("meta.json");
        fs::write(path, meta.to_string()).unwrap();
    }

    #[test]
    fn new_creates_connector() {
        let connector = VibeConnector::new();
        let _ = connector;
    }

    #[test]
    fn scan_parses_simple_conversation() {
        let dir = TempDir::new().unwrap();
        let session_dir = create_vibe_storage(&dir);

        let messages = vec![
            json!({"role": "user", "content": "Hello!", "message_id": "msg-1"}),
            json!({"role": "assistant", "content": "Hi there!", "message_id": "msg-2"}),
        ];
        write_messages(&session_dir, &messages);

        let meta = json!({
            "session_id": "test-session-123",
            "title": "Test Session",
            "start_time": "2026-01-28T10:00:00",
            "end_time": "2026-01-28T10:30:00",
            "environment": {
                "working_directory": "/home/user/project"
            }
        });
        write_meta(&session_dir, &meta);

        let connector = VibeConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].agent_slug, "vibe");
        assert_eq!(convs[0].title, Some("Test Session".to_string()));
        assert_eq!(
            convs[0].workspace,
            Some(PathBuf::from("/home/user/project"))
        );
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[1].role, "assistant");
    }

    #[test]
    fn scan_handles_tool_messages() {
        let dir = TempDir::new().unwrap();
        let session_dir = create_vibe_storage(&dir);

        let messages = vec![
            json!({"role": "user", "content": "Run a command", "message_id": "msg-1"}),
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{\"command\": \"ls\"}"}
                }],
                "message_id": "msg-2"
            }),
            json!({
                "role": "tool",
                "content": "file1.txt\nfile2.txt",
                "name": "bash",
                "tool_call_id": "call-1"
            }),
        ];
        write_messages(&session_dir, &messages);

        let meta = json!({"session_id": "test-tools"});
        write_meta(&session_dir, &meta);

        let connector = VibeConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 3);
        assert_eq!(convs[0].messages[1].role, "assistant");
        assert!(convs[0].messages[1].content.contains("[Tool: bash"));
        assert_eq!(convs[0].messages[2].role, "tool");
        assert_eq!(convs[0].messages[2].author, Some("bash".to_string()));
    }

    #[test]
    fn scan_deduplicates_messages() {
        let dir = TempDir::new().unwrap();
        let session_dir = create_vibe_storage(&dir);

        // Same message_id appears twice (vibe quirk)
        let messages = vec![
            json!({"role": "user", "content": "Hello", "message_id": "msg-1"}),
            json!({"role": "assistant", "content": "Response", "message_id": "msg-2"}),
            json!({"role": "assistant", "content": "Response", "message_id": "msg-2"}),
        ];
        write_messages(&session_dir, &messages);

        let meta = json!({"session_id": "test-dedup"});
        write_meta(&session_dir, &meta);

        let connector = VibeConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].messages.len(), 2); // Deduplicated
    }

    #[test]
    fn scan_handles_missing_meta() {
        let dir = TempDir::new().unwrap();
        let session_dir = create_vibe_storage(&dir);

        let messages = vec![
            json!({"role": "user", "content": "First line\nSecond line", "message_id": "msg-1"}),
        ];
        write_messages(&session_dir, &messages);
        // No meta.json

        let connector = VibeConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        // Title should come from first user message
        assert_eq!(convs[0].title, Some("First line".to_string()));
    }

    #[test]
    fn scan_handles_empty_storage() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("empty_session")).unwrap();

        let connector = VibeConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 0);
    }

    #[test]
    fn parse_iso_timestamp_works() {
        // With fractional seconds
        let ts = parse_iso_timestamp("2026-01-28T10:54:03.875672");
        assert!(ts.is_some());

        // Without fractional
        let ts = parse_iso_timestamp("2026-01-28T10:54:03");
        assert!(ts.is_some());

        // RFC3339
        let ts = parse_iso_timestamp("2026-01-28T10:54:03+00:00");
        assert!(ts.is_some());
    }

    #[test]
    fn build_message_content_handles_tool_calls() {
        let msg = VibeMessage {
            role: "assistant".to_string(),
            content: Some("Let me run that".to_string()),
            message_id: None,
            tool_calls: Some(vec![ToolCall {
                id: Some("call-1".to_string()),
                call_type: Some("function".to_string()),
                function: Some(ToolFunction {
                    name: Some("bash".to_string()),
                    arguments: Some("{\"command\": \"ls\"}".to_string()),
                }),
            }]),
            tool_call_id: None,
            name: None,
        };

        let content = build_message_content(&msg);
        assert!(content.contains("Let me run that"));
        assert!(content.contains("[Tool: bash"));
    }
}
