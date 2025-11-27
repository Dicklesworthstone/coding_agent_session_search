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
            }
        } else {
            DetectionResult::not_found()
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Use data_root only if it looks like a Claude projects directory (for testing)
        // Otherwise use the default projects_root
        let root = if ctx.data_root.join("projects").exists()
            || ctx
                .data_root
                .file_name()
                .map(|n| n.to_str().unwrap_or("").contains("claude"))
                .unwrap_or(false)
        {
            ctx.data_root.clone()
        } else {
            Self::projects_root()
        };
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
            file_count += 1;
            if file_count <= 3 {
                tracing::debug!(path = %entry.path().display(), "claude_code found file");
            }
            let content = fs::read_to_string(entry.path())
                .with_context(|| format!("read {}", entry.path().display()))?;
            let mut messages = Vec::new();
            let mut started_at = None;
            let mut ended_at = None;
            // Track workspace from first entry's cwd field
            let mut workspace: Option<PathBuf> = None;
            let mut session_id: Option<String> = None;
            let mut git_branch: Option<String> = None;

            if ext == Some("jsonl") {
                for (idx, line) in content.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let val: Value = match serde_json::from_str(line) {
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
                    if !matches!(entry_type, Some("user") | Some("assistant")) {
                        continue;
                    }

                    // Parse ISO-8601 timestamp using shared utility
                    let created = val
                        .get("timestamp")
                        .and_then(crate::connectors::parse_timestamp);

                    if let (Some(since), Some(ts)) = (ctx.since_ts, created)
                        && ts <= since
                    {
                        continue;
                    }

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
                        idx: idx as i64,
                        role: role.to_string(),
                        author,
                        created_at: created,
                        content: content_str,
                        extra: val,
                        snippets: Vec::new(),
                    });
                }
            } else {
                // JSON or Claude format files
                let val: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
                if let Some(arr) = val.get("messages").and_then(|m| m.as_array()) {
                    for (idx, item) in arr.iter().enumerate() {
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

                        if let (Some(since), Some(ts)) = (ctx.since_ts, created)
                            && ts <= since
                        {
                            continue;
                        }

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
                            idx: idx as i64,
                            role: role.to_string(),
                            author: None,
                            created_at: created,
                            content: content_str,
                            extra: item.clone(),
                            snippets: Vec::new(),
                        });
                    }
                }
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
                serde_json::from_str::<Value>(&content)
                    .ok()
                    .and_then(|v| {
                        v.get("title")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string())
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
                    .map(|s| s.to_string()),
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
