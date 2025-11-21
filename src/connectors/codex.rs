use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{Connector, DetectionResult, NormalizedConversation, NormalizedMessage, NormalizedSnippet, ScanContext};

pub struct CodexConnector;

impl CodexConnector {
    pub fn new() -> Self {
        Self
    }

    fn home() -> PathBuf {
        std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".codex"))
    }

    fn rollout_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let sessions = root.join("sessions");
        if !sessions.exists() {
            return out;
        }
        for entry in WalkDir::new(sessions).into_iter().flatten() {
            if entry.file_type().is_file() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                        out.push(entry.path().to_path_buf());
                    }
                }
            }
        }
        out
    }
}

impl Connector for CodexConnector {
    fn detect(&self) -> DetectionResult {
        let home = Self::home();
        if home.join("sessions").exists() {
            DetectionResult {
                detected: true,
                evidence: vec![format!("found {}", home.display())],
            }
        } else {
            DetectionResult::not_found()
        }
    }

    fn scan(&self, _ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let home = Self::home();
        let files = Self::rollout_files(&home);
        let mut convs = Vec::new();
        for file in files {
            let source_path = file.clone();
            let external_id = source_path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string());
            let content = fs::read_to_string(&file)
                .with_context(|| format!("read rollout {}", file.display()))?;
            let mut messages = Vec::new();
            let mut started_at = None;
            let mut ended_at = None;
            for (idx, line) in content.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let val: Value = serde_json::from_str(line).unwrap_or(Value::String(line.to_string()));
                let role_str = val
                    .get("role")
                    .or_else(|| val.get("speaker"))
                    .or_else(|| val.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("agent");
                let created = val
                    .get("timestamp")
                    .or_else(|| val.get("time"))
                    .and_then(|v| v.as_i64());
                if started_at.is_none() {
                    started_at = created;
                }
                ended_at = created.or(ended_at);
                let content_str = val
                    .get("content")
                    .or_else(|| val.get("text"))
                    .or_else(|| val.get("message"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| line.to_string());

                messages.push(NormalizedMessage {
                    idx: idx as i64,
                    role: role_str.to_string(),
                    author: None,
                    created_at: created,
                    content: content_str,
                    extra: val,
                    snippets: Vec::new(),
                });
            }

            if messages.is_empty() {
                continue;
            }

            let title = messages
                .get(0)
                .and_then(|m| m.content.lines().next())
                .map(|s| s.to_string());

            convs.push(NormalizedConversation {
                agent_slug: "codex".to_string(),
                external_id,
                title,
                workspace: None,
                source_path: source_path.clone(),
                started_at,
                ended_at,
                metadata: serde_json::json!({"source": "rollout"}),
                messages,
            });
        }

        Ok(convs)
    }
}
