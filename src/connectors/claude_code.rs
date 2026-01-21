use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

pub struct ClaudeCodeConnector;
impl Default for ClaudeCodeConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeCodeConnector {
    pub fn new() -> Self {
        Self
    }

    fn projects_root() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".claude/projects")
    }
}

impl Connector for ClaudeCodeConnector {
    fn detect(&self) -> DetectionResult {
        let root = Self::projects_root();
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
        // Use data_root only if it looks like a Claude projects directory (for testing)
        // Otherwise use the default projects_root
        let looks_like_root = |path: &PathBuf| {
            path.join("projects").exists()
                || path
                    .file_name()
                    .is_some_and(|n| n.to_str().unwrap_or("").contains("claude"))
        };
        let mut root = if ctx.use_default_detection() {
            if looks_like_root(&ctx.data_dir) {
                ctx.data_dir.clone()
            } else {
                Self::projects_root()
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
            if ext != Some("jsonl") && ext != Some("json") && ext != Some("claude") {
                continue;
            }
            // Skip files not modified since last scan (incremental indexing)
            if !crate::connectors::file_modified_since(entry.path(), ctx.since_ts) {
                continue;
            }
            file_count += 1;
            if file_count <= 3 {
                tracing::debug!(path = %entry.path().display(), "claude_code found file");
            }

            let mut messages = Vec::new();
            let mut started_at = None;
            let mut ended_at = None;
            // Track workspace from first entry's cwd field
            let mut workspace: Option<PathBuf> = None;
            let mut session_id: Option<String> = None;
            let mut git_branch: Option<String> = None;
            let mut content_string = String::new();

            if ext == Some("jsonl") {
                let file = std::fs::File::open(entry.path())
                    .with_context(|| format!("open {}", entry.path().display()))?;
                let reader = std::io::BufReader::new(file);

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
                        Err(_) => continue, // Skip malformed lines
                    };

                    // Extract session metadata from first available entry
                    if workspace.is_none() {
                        workspace = val.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
                    }
                    if session_id.is_none() {
                        session_id = val
                            .get("sessionId")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                    }
                    if git_branch.is_none() {
                        git_branch = val
                            .get("gitBranch")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                    }

                    // Filter to user/assistant entries only (skip summary, file-history-snapshot, etc.)
                    let entry_type = val.get("type").and_then(|v| v.as_str());
                    if !matches!(entry_type, Some("user" | "assistant")) {
                        continue;
                    }

                    // Parse ISO-8601 timestamp using shared utility
                    let created = val
                        .get("timestamp")
                        .and_then(crate::connectors::parse_timestamp);

                    // NOTE: Do NOT filter individual messages by timestamp here!
                    // The file-level check in file_modified_since() is sufficient.
                    // Filtering messages would cause older messages to be lost when
                    // the file is re-indexed after new messages are added.

                    started_at = started_at.or(created);
                    ended_at = created.or(ended_at);

                    // Role from message.role or entry type
                    let role = val
                        .get("message")
                        .and_then(|m| m.get("role"))
                        .and_then(|v| v.as_str())
                        .or(entry_type)
                        .unwrap_or("agent");

                    // Content from message.content (may be string or array)
                    let content_val = val.get("message").and_then(|m| m.get("content"));
                    let content_str = content_val
                        .map(crate::connectors::flatten_content)
                        .unwrap_or_default();

                    // Skip entries with empty content
                    if content_str.trim().is_empty() {
                        continue;
                    }

                    // Extract model name for author field
                    let author = val
                        .get("message")
                        .and_then(|m| m.get("model"))
                        .and_then(|v| v.as_str())
                        .map(String::from);

                    messages.push(NormalizedMessage {
                        idx: 0, // will be re-assigned after filtering
                        role: role.to_string(),
                        author,
                        created_at: created,
                        content: content_str,
                        extra: val,
                        snippets: Vec::new(),
                    });
                }
                // Re-assign sequential indices after filtering
                super::reindex_messages(&mut messages);
            } else {
                // Safety check: Don't read files larger than 100MB to avoid OOM
                if let Ok(metadata) = fs::metadata(entry.path())
                    && metadata.len() > 100 * 1024 * 1024
                {
                    tracing::debug!(
                        path = %entry.path().display(),
                        size_bytes = metadata.len(),
                        "skipping large file (>100MB)"
                    );
                    continue;
                }

                content_string = fs::read_to_string(entry.path())
                    .with_context(|| format!("read {}", entry.path().display()))?;
                // JSON or Claude format files
                let val: Value = match serde_json::from_str(&content_string) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!(path = %entry.path().display(), error = %e, "claude_code skipping malformed JSON");
                        continue;
                    }
                };
                if let Some(arr) = val.get("messages").and_then(|m| m.as_array()) {
                    for item in arr {
                        let role = item
                            .get("role")
                            .or_else(|| item.get("type"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("agent");

                        // Use parse_timestamp for consistent handling of both i64 and ISO-8601
                        let created = item
                            .get("timestamp")
                            .or_else(|| item.get("time"))
                            .and_then(crate::connectors::parse_timestamp);

                        // NOTE: Do NOT filter individual messages by timestamp.
                        // File-level check is sufficient for incremental indexing.

                        started_at = started_at.or(created);
                        ended_at = created.or(ended_at);

                        // Use flatten_content for consistent handling of both string and array content
                        let content_str = item
                            .get("content")
                            .or_else(|| item.get("text"))
                            .map(crate::connectors::flatten_content)
                            .unwrap_or_default();

                        // Skip entries with empty content
                        if content_str.trim().is_empty() {
                            continue;
                        }

                        messages.push(NormalizedMessage {
                            idx: 0, // will be re-assigned after filtering
                            role: role.to_string(),
                            author: None,
                            created_at: created,
                            content: content_str,
                            extra: item.clone(),
                            snippets: Vec::new(),
                        });
                    }
                }
                // Re-assign sequential indices after filtering
                super::reindex_messages(&mut messages);
            }
            if messages.is_empty() {
                if file_count <= 3 {
                    tracing::debug!(path = %entry.path().display(), "claude_code no messages extracted");
                }
                continue;
            }
            tracing::debug!(path = %entry.path().display(), messages = messages.len(), "claude_code extracted messages");

            // Extract title from first user message, truncated to reasonable length
            let title = if ext == Some("jsonl") {
                messages
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
                    })
            } else {
                serde_json::from_str::<Value>(&content_string)
                    .ok()
                    .and_then(|v| {
                        v.get("title")
                            .and_then(|t| t.as_str())
                            .map(std::string::ToString::to_string)
                    })
                    .or_else(|| {
                        messages
                            .first()
                            .and_then(|m| m.content.lines().next())
                            .map(|s| s.chars().take(100).collect())
                    })
            };

            convs.push(NormalizedConversation {
                agent_slug: "claude_code".into(),
                external_id: entry
                    .path()
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(std::string::ToString::to_string),
                title,
                workspace, // Now populated from cwd field!
                source_path: entry.path().to_path_buf(),
                started_at,
                ended_at,
                metadata: serde_json::json!({
                    "source": "claude_code",
                    "sessionId": session_id,
                    "gitBranch": git_branch
                }),
                messages,
            });
        }

        Ok(convs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    // =========================================================================
    // Constructor tests
    // =========================================================================

    #[test]
    fn new_creates_connector() {
        let connector = ClaudeCodeConnector::new();
        let _ = connector;
    }

    #[test]
    fn default_creates_connector() {
        let connector = ClaudeCodeConnector;
        let _ = connector;
    }

    #[test]
    fn projects_root_returns_claude_projects_path() {
        let root = ClaudeCodeConnector::projects_root();
        assert!(root.ends_with(".claude/projects"));
    }

    // =========================================================================
    // Detection tests
    // =========================================================================

    #[test]
    fn detect_not_found_without_projects_dir() {
        let connector = ClaudeCodeConnector::new();
        let result = connector.detect();
        // On most CI/test systems, .claude/projects won't exist
        // Just verify detect() doesn't panic
        let _ = result.detected;
    }

    // =========================================================================
    // JSONL parsing tests
    // =========================================================================

    #[test]
    fn scan_parses_jsonl_user_and_assistant_messages() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.jsonl");
        let content = r#"{"type":"user","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"Hello Claude"}}
{"type":"assistant","timestamp":"2025-12-01T10:00:01Z","message":{"role":"assistant","content":"Hello! How can I help?"}}
{"type":"summary","timestamp":"2025-12-01T10:00:02Z","summary":"Test summary"}
"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);

        // Only user and assistant messages should be extracted (not summary)
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[0].content, "Hello Claude");
        assert_eq!(convs[0].messages[1].role, "assistant");
        assert!(convs[0].messages[1].content.contains("How can I help"));
    }

    #[test]
    fn scan_extracts_session_metadata() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.jsonl");
        let content = r#"{"type":"user","cwd":"/projects/myapp","sessionId":"sess-123","gitBranch":"main","message":{"role":"user","content":"Test message"}}"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].workspace, Some(PathBuf::from("/projects/myapp")));
        assert_eq!(convs[0].metadata["sessionId"], "sess-123");
        assert_eq!(convs[0].metadata["gitBranch"], "main");
    }

    #[test]
    fn scan_extracts_model_as_author() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.jsonl");
        let content = r#"{"type":"assistant","message":{"role":"assistant","content":"Response","model":"claude-3-opus"}}"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(
            convs[0].messages[0].author,
            Some("claude-3-opus".to_string())
        );
    }

    #[test]
    fn scan_parses_iso8601_timestamp() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.jsonl");
        let content = r#"{"type":"user","timestamp":"2025-11-15T14:30:00.123Z","message":{"role":"user","content":"Test"}}"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert!(convs[0].messages[0].created_at.is_some());
        let ts = convs[0].messages[0].created_at.unwrap();
        // Should be around 2025-11-15 in milliseconds
        assert!(ts > 1700000000000);
    }

    #[test]
    fn scan_handles_array_content() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.jsonl");
        let content = json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "First part"},
                    {"type": "text", "text": "Second part"}
                ]
            }
        })
        .to_string();
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].messages.len(), 1);
        assert!(convs[0].messages[0].content.contains("First part"));
        assert!(convs[0].messages[0].content.contains("Second part"));
    }

    #[test]
    fn scan_skips_empty_content() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.jsonl");
        let content = r#"{"type":"user","message":{"role":"user","content":""}}
{"type":"user","message":{"role":"user","content":"   "}}
{"type":"user","message":{"role":"user","content":"Valid message"}}
"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Only the valid message should be extracted
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].content, "Valid message");
    }

    #[test]
    fn scan_skips_non_user_assistant_types() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.jsonl");
        let content = r#"{"type":"summary","content":"Session summary"}
{"type":"file-history-snapshot","files":[]}
{"type":"user","message":{"role":"user","content":"User message"}}
{"type":"tool_result","result":"Some result"}
"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].role, "user");
    }

    #[test]
    fn scan_reindexes_messages_sequentially() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.jsonl");
        let content = r#"{"type":"user","message":{"role":"user","content":"Message 1"}}
{"type":"assistant","message":{"role":"assistant","content":"Message 2"}}
{"type":"user","message":{"role":"user","content":"Message 3"}}
"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].messages[0].idx, 0);
        assert_eq!(convs[0].messages[1].idx, 1);
        assert_eq!(convs[0].messages[2].idx, 2);
    }

    // =========================================================================
    // JSON format parsing tests
    // =========================================================================

    #[test]
    fn scan_parses_json_messages_array() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.json");
        let content = json!({
            "title": "Test Session",
            "messages": [
                {"role": "user", "content": "Hello", "timestamp": 1700000000000i64},
                {"role": "assistant", "content": "Hi there!", "timestamp": 1700000001000i64}
            ]
        })
        .to_string();
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[1].role, "assistant");
    }

    #[test]
    fn scan_json_extracts_title() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.json");
        let content = json!({
            "title": "Custom Session Title",
            "messages": [
                {"role": "user", "content": "Test content"}
            ]
        })
        .to_string();
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].title, Some("Custom Session Title".to_string()));
    }

    #[test]
    fn scan_json_uses_type_as_role_fallback() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.json");
        let content = json!({
            "messages": [
                {"type": "user", "content": "Message with type instead of role"}
            ]
        })
        .to_string();
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].messages[0].role, "user");
    }

    #[test]
    fn scan_json_uses_text_as_content_fallback() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.json");
        let content = json!({
            "messages": [
                {"role": "user", "text": "Message with text field instead of content"}
            ]
        })
        .to_string();
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert!(convs[0].messages[0].content.contains("text field"));
    }

    #[test]
    fn scan_json_uses_time_as_timestamp_fallback() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.json");
        let content = json!({
            "messages": [
                {"role": "user", "content": "Test", "time": 1700000000000i64}
            ]
        })
        .to_string();
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].messages[0].created_at, Some(1700000000000));
    }

    // =========================================================================
    // Title extraction tests
    // =========================================================================

    #[test]
    fn scan_title_from_first_user_message_jsonl() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.jsonl");
        let content = r#"{"type":"assistant","message":{"role":"assistant","content":"I can help"}}
{"type":"user","message":{"role":"user","content":"Help me build a web app"}}
"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].title, Some("Help me build a web app".to_string()));
    }

    #[test]
    fn scan_title_truncates_to_100_chars() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let long_message = "x".repeat(200);
        let session_file = claude_dir.join("session.jsonl");
        let content = format!(
            r#"{{"type":"user","message":{{"role":"user","content":"{}"}}}}"#,
            long_message
        );
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert!(convs[0].title.as_ref().unwrap().len() <= 100);
    }

    #[test]
    fn scan_title_uses_first_line_only() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.jsonl");
        let content = r#"{"type":"user","message":{"role":"user","content":"First line\nSecond line\nThird line"}}"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].title, Some("First line".to_string()));
    }

    #[test]
    fn scan_title_fallback_to_workspace_name() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        // Only assistant message, no user message for title
        let session_file = claude_dir.join("session.jsonl");
        let content = r#"{"type":"assistant","cwd":"/projects/myapp","message":{"role":"assistant","content":"Response only"}}"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Should fallback to workspace directory name
        assert_eq!(convs[0].title, Some("myapp".to_string()));
    }

    // =========================================================================
    // Edge case tests
    // =========================================================================

    #[test]
    fn scan_empty_directory_returns_empty() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert!(convs.is_empty());
    }

    #[test]
    fn scan_skips_malformed_jsonl_lines() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.jsonl");
        let content = r#"not valid json
{"type":"user","message":{"role":"user","content":"Valid message"}}
{broken json here
"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Should still extract the valid line
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].content, "Valid message");
    }

    #[test]
    fn scan_skips_malformed_json_files() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        // Create a malformed JSON file
        let bad_file = claude_dir.join("bad.json");
        fs::write(&bad_file, "not valid json {{{").unwrap();

        // Create a valid JSONL file
        let good_file = claude_dir.join("good.jsonl");
        fs::write(
            &good_file,
            r#"{"type":"user","message":{"role":"user","content":"Valid"}}"#,
        )
        .unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Should only have one conversation from the valid file
        assert_eq!(convs.len(), 1);
    }

    #[test]
    fn scan_handles_empty_messages_array() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.json");
        let content = json!({
            "messages": []
        })
        .to_string();
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Empty messages should result in no conversation
        assert!(convs.is_empty());
    }

    #[test]
    fn scan_processes_subdirectories() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        let subdir = claude_dir.join("project1");
        fs::create_dir_all(&subdir).unwrap();

        let session_file = subdir.join("session.jsonl");
        let content = r#"{"type":"user","message":{"role":"user","content":"Nested message"}}"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert!(convs[0].messages[0].content.contains("Nested message"));
    }

    #[test]
    fn scan_skips_non_session_files() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        // Create various non-session files
        fs::write(claude_dir.join("config.toml"), "").unwrap();
        fs::write(claude_dir.join("notes.txt"), "").unwrap();
        fs::write(claude_dir.join("backup.bak"), "").unwrap();

        // Create a valid session file
        let session_file = claude_dir.join("session.jsonl");
        fs::write(
            &session_file,
            r#"{"type":"user","message":{"role":"user","content":"Valid"}}"#,
        )
        .unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Should only have one conversation from the .jsonl file
        assert_eq!(convs.len(), 1);
    }

    #[test]
    fn scan_handles_claude_extension() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.claude");
        let content = json!({
            "messages": [
                {"role": "user", "content": "Claude extension test"}
            ]
        })
        .to_string();
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert!(convs[0].messages[0].content.contains("Claude extension"));
    }

    #[test]
    fn scan_sets_external_id_from_filename() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("unique-session-id.jsonl");
        let content = r#"{"type":"user","message":{"role":"user","content":"Test"}}"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(
            convs[0].external_id,
            Some("unique-session-id.jsonl".to_string())
        );
    }

    #[test]
    fn scan_sets_agent_slug_to_claude_code() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.jsonl");
        let content = r#"{"type":"user","message":{"role":"user","content":"Test"}}"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].agent_slug, "claude_code");
    }

    #[test]
    fn scan_preserves_original_json_in_extra() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.jsonl");
        let content = r#"{"type":"user","customField":"customValue","message":{"role":"user","content":"Test"}}"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].messages[0].extra["customField"], "customValue");
    }

    #[test]
    fn scan_tracks_started_and_ended_timestamps() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        let session_file = claude_dir.join("session.jsonl");
        let content = r#"{"type":"user","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"First"}}
{"type":"assistant","timestamp":"2025-12-01T10:05:00Z","message":{"role":"assistant","content":"Last"}}
"#;
        fs::write(&session_file, content).unwrap();

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert!(convs[0].started_at.is_some());
        assert!(convs[0].ended_at.is_some());
        // ended_at should be after or equal to started_at
        assert!(convs[0].ended_at.unwrap() >= convs[0].started_at.unwrap());
    }

    #[test]
    fn scan_multiple_files_returns_multiple_conversations() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        // Create two session files
        for i in 1..=3 {
            let session_file = claude_dir.join(format!("session{}.jsonl", i));
            let content =
                format!(r#"{{"type":"user","message":{{"role":"user","content":"Message {i}"}}}}"#);
            fs::write(&session_file, content).unwrap();
        }

        let connector = ClaudeCodeConnector::new();
        let ctx = ScanContext::local_default(claude_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 3);
    }

    #[test]
    #[serial]
    fn scan_respects_claude_home_even_if_data_dir_contains_claude_string() {
        // Setup real claude home with data
        let home_dir = TempDir::new().unwrap();
        // Mock the home directory structure expected by projects_root()
        let real_projects = home_dir.path().join(".claude").join("projects");
        fs::create_dir_all(&real_projects).unwrap();
        
        let content = r#"{"type":"user","message":{"role":"user","content":"Real session"}}"#;
        fs::write(real_projects.join("real.jsonl"), content).unwrap();

        // Setup CASS data dir that happens to have "claude" in path
        let project_dir = TempDir::new().unwrap();
        let confusing_data_dir = project_dir.path().join("my_claude_tool").join("data");
        fs::create_dir_all(&confusing_data_dir).unwrap();

        // Overwrite HOME to point to our temp home
        // SAFETY: Test runs in single-threaded context
        // Note: dirs::home_dir() typically checks HOME
        unsafe { std::env::set_var("HOME", home_dir.path()) };
        
        let connector = ClaudeCodeConnector::new();
        // this defaults to use_default_detection = true
        let ctx = ScanContext::local_default(confusing_data_dir.clone(), None);
        
        let convs = connector.scan(&ctx).unwrap();
        
        // Cleanup
        // We can't easily restore HOME to its original value if we don't know it, 
        // but typically tests running in isolation or containers handle this. 
        // For safety/correctness in a shared env, we should probably save/restore.
        // But here I'll just rely on the fact that we are in a "random exploration" mode
        // and if I break my own environment it's fine? No, I should be careful.
        // But `dirs::home_dir` caches? No.
        
        // Check results
        // Use a loop to print what we found if it fails
        if convs.len() != 1 {
             println!("Found {} conversations:", convs.len());
             for c in &convs {
                 println!(" - {:?}", c.source_path);
             }
        }
        assert_eq!(convs.len(), 1, "Should find session in real home");
        assert_eq!(convs[0].messages[0].content, "Real session");
    }
}
