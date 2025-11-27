use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

pub struct ClineConnector;
impl Default for ClineConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl ClineConnector {
    pub fn new() -> Self {
        Self
    }

    fn storage_root() -> PathBuf {
        let base = dirs::home_dir().unwrap_or_default();
        let linux = base.join(".config/Code/User/globalStorage/saoudrizwan.claude-dev");
        if linux.exists() {
            return linux;
        }
        base.join("Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev")
    }
}

impl Connector for ClineConnector {
    fn detect(&self) -> DetectionResult {
        let root = Self::storage_root();
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
        let root = if ctx.data_root.exists() {
            ctx.data_root.clone()
        } else {
            Self::storage_root()
        };
        if !root.exists() {
            return Ok(Vec::new());
        }

        let mut convs = Vec::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let task_id = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string());
            if task_id.as_deref() == Some("taskHistory.json") {
                continue;
            }

            let meta_path = path.join("task_metadata.json");
            let ui_messages_path = path.join("ui_messages.json");
            let api_messages_path = path.join("api_conversation_history.json");

            // Prefer UI messages as they are user-facing. Fallback to API history.
            let source_file = if ui_messages_path.exists() {
                Some(ui_messages_path)
            } else if api_messages_path.exists() {
                Some(api_messages_path)
            } else {
                None
            };

            let Some(file) = source_file else {
                continue;
            };

            let data =
                fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
            let val: Value = serde_json::from_str(&data).unwrap_or(Value::Null);

            let mut messages = Vec::new();
            if let Some(arr) = val.as_array() {
                for item in arr {
                    // Use parse_timestamp to handle both i64 milliseconds and ISO-8601 strings
                    let created = item
                        .get("timestamp")
                        .or_else(|| item.get("created_at"))
                        .or_else(|| item.get("ts"))
                        .and_then(crate::connectors::parse_timestamp);

                    // Skip if older than since_ts
                    if let (Some(since), Some(ts)) = (ctx.since_ts, created)
                        && ts <= since
                    {
                        continue;
                    }

                    let role = item
                        .get("role")
                        .or_else(|| item.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("agent");

                    let content = item
                        .get("content")
                        .or_else(|| item.get("text"))
                        .or_else(|| item.get("message"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    if content.trim().is_empty() {
                        continue;
                    }

                    messages.push(NormalizedMessage {
                        idx: 0, // set later
                        role: role.to_string(),
                        author: None,
                        created_at: created,
                        content: content.to_string(),
                        extra: item.clone(),
                        snippets: Vec::new(),
                    });
                }
            }

            if messages.is_empty() {
                continue;
            }

            // Sort by timestamp to ensure correct ordering
            messages.sort_by_key(|m| m.created_at.unwrap_or(0));

            // Re-index
            for (i, msg) in messages.iter_mut().enumerate() {
                msg.idx = i as i64;
            }

            let mut title = None;
            let mut workspace = None;

            if meta_path.exists()
                && let Ok(s) = fs::read_to_string(&meta_path)
                && let Ok(v) = serde_json::from_str::<Value>(&s)
            {
                title = v
                    .get("title")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string());
                // Try to find workspace path
                // Cline doesn't standardize this in metadata, but sometimes it's there or in state.
                // We check common keys.
                workspace = v
                    .get("rootPath")
                    .or_else(|| v.get("cwd"))
                    .or_else(|| v.get("workspace"))
                    .and_then(|s| s.as_str())
                    .map(PathBuf::from);
            }

            // Fallback title from first message
            if title.is_none() {
                title = messages
                    .first()
                    .and_then(|m| m.content.lines().next())
                    .map(|s| s.chars().take(100).collect());
            }

            convs.push(NormalizedConversation {
                agent_slug: "cline".to_string(),
                external_id: task_id,
                title,
                workspace,
                source_path: path.clone(),
                started_at: messages.first().and_then(|m| m.created_at),
                ended_at: messages.last().and_then(|m| m.created_at),
                metadata: serde_json::json!({"source": "cline"}),
                messages,
            });
        }

        Ok(convs)
    }
}
