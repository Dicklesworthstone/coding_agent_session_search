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

            let mut messages = Vec::new();

            for file in [ui_messages_path, api_messages_path] {
                if !file.exists() {
                    continue;
                }
                let data = fs::read_to_string(&file)
                    .with_context(|| format!("read {}", file.display()))?;
                let val: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
                if let Some(arr) = val.as_array() {
                    for (idx, item) in arr.iter().enumerate() {
                        let role = item
                            .get("role")
                            .or_else(|| item.get("type"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("agent");
                        let created = item
                            .get("timestamp")
                            .or_else(|| item.get("created_at"))
                            .and_then(|v| v.as_i64());
                        let content = item
                            .get("content")
                            .or_else(|| item.get("text"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        messages.push(NormalizedMessage {
                            idx: idx as i64,
                            role: role.to_string(),
                            author: None,
                            created_at: created,
                            content: content.to_string(),
                            extra: item.clone(),
                            snippets: Vec::new(),
                        });
                    }
                }
            }

            if messages.is_empty() {
                continue;
            }

            let title = meta_path
                .exists()
                .then(|| fs::read_to_string(&meta_path).ok())
                .flatten()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| {
                    v.get("title")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                });

            convs.push(NormalizedConversation {
                agent_slug: "cline".to_string(),
                external_id: task_id,
                title,
                workspace: None,
                source_path: path.clone(),
                started_at: None,
                ended_at: None,
                metadata: serde_json::json!({"source": "cline"}),
                messages,
            });
        }

        Ok(convs)
    }
}
