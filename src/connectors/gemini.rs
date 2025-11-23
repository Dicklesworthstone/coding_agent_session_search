use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

pub struct GeminiConnector;
impl Default for GeminiConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl GeminiConnector {
    pub fn new() -> Self {
        Self
    }

    fn root() -> PathBuf {
        std::env::var("GEMINI_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".gemini/tmp"))
    }
}

impl Connector for GeminiConnector {
    fn detect(&self) -> DetectionResult {
        let root = Self::root();
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
            Self::root()
        };
        if !root.exists() {
            return Ok(Vec::new());
        }

        let mut convs = Vec::new();
        for dir in fs::read_dir(&root)? {
            let dir = dir?;
            let path = dir.path();
            if !path.is_dir() {
                continue;
            }
            let mut messages = Vec::new();
            for entry in WalkDir::new(&path).into_iter().flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }
                if let Some(ext) = entry.path().extension().and_then(|s| s.to_str())
                    && ext != "json"
                    && ext != "jsonl"
                {
                    continue;
                }
                let data = fs::read_to_string(entry.path())
                    .with_context(|| format!("read {}", entry.path().display()))?;
                if entry.path().extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    for (idx, line) in data.lines().enumerate() {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let val: Value =
                            serde_json::from_str(line).unwrap_or(Value::String(line.to_string()));
                        let content = val
                            .get("content")
                            .or_else(|| val.get("text"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(line);
                        messages.push(NormalizedMessage {
                            idx: idx as i64,
                            role: val
                                .get("role")
                                .and_then(|v| v.as_str())
                                .unwrap_or("agent")
                                .to_string(),
                            author: None,
                            created_at: val.get("timestamp").and_then(|v| v.as_i64()),
                            content: content.to_string(),
                            extra: val,
                            snippets: Vec::new(),
                        });
                    }
                } else {
                    let val: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
                    if let Some(arr) = val.as_array() {
                        for (idx, item) in arr.iter().enumerate() {
                            let content = item
                                .get("content")
                                .or_else(|| item.get("text"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            messages.push(NormalizedMessage {
                                idx: idx as i64,
                                role: item
                                    .get("role")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("agent")
                                    .to_string(),
                                author: None,
                                created_at: item.get("timestamp").and_then(|v| v.as_i64()),
                                content: content.to_string(),
                                extra: item.clone(),
                                snippets: Vec::new(),
                            });
                        }
                    }
                }
            }
            if messages.is_empty() {
                continue;
            }
            convs.push(NormalizedConversation {
                agent_slug: "gemini".into(),
                external_id: path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string()),
                title: messages
                    .first()
                    .and_then(|m| m.content.lines().next())
                    .map(|s| s.to_string()),
                workspace: None,
                source_path: path.clone(),
                started_at: messages.first().and_then(|m| m.created_at),
                ended_at: messages.last().and_then(|m| m.created_at),
                metadata: serde_json::json!({"source": "gemini"}),
                messages,
            });
        }

        Ok(convs)
    }
}
