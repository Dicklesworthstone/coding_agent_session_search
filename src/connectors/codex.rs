use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

pub struct CodexConnector;
impl Default for CodexConnector {
    fn default() -> Self {
        Self::new()
    }
}

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
                let name = entry.file_name().to_str().unwrap_or("");
                // Match both modern .jsonl and legacy .json formats
                if name.starts_with("rollout-")
                    && (name.ends_with(".jsonl") || name.ends_with(".json"))
                {
                    out.push(entry.path().to_path_buf());
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

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Use data_root only if it looks like a Codex home directory (for testing)
        // Otherwise use the default home
        let home = if ctx.data_root.join("sessions").exists()
            || ctx
                .data_root
                .file_name()
                .map(|n| n.to_str().unwrap_or("").contains("codex"))
                .unwrap_or(false)
        {
            ctx.data_root.clone()
        } else {
            Self::home()
        };
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

            let ext = file.extension().and_then(|e| e.to_str());
            let mut messages = Vec::new();
            let mut started_at = None;
            let mut ended_at = None;
            let mut session_cwd: Option<PathBuf> = None;

            if ext == Some("jsonl") {
                // Modern envelope format: each line has {type, timestamp, payload}
                for (idx, line) in content.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let val: Value = match serde_json::from_str(line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    let entry_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let created = val
                        .get("timestamp")
                        .and_then(crate::connectors::parse_timestamp);

                    if let (Some(since), Some(ts)) = (ctx.since_ts, created)
                        && ts <= since
                    {
                        continue;
                    }

                    match entry_type {
                        "session_meta" => {
                            // Extract workspace from session metadata
                            if let Some(payload) = val.get("payload") {
                                session_cwd = payload
                                    .get("cwd")
                                    .and_then(|v| v.as_str())
                                    .map(PathBuf::from);
                            }
                            started_at = started_at.or(created);
                        }
                        "response_item" => {
                            // Main message entries with nested payload
                            if let Some(payload) = val.get("payload") {
                                let role = payload
                                    .get("role")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("agent");

                                let content_str = payload
                                    .get("content")
                                    .map(crate::connectors::flatten_content)
                                    .unwrap_or_default();

                                if content_str.trim().is_empty() {
                                    continue;
                                }

                                started_at = started_at.or(created);
                                ended_at = created.or(ended_at);

                                messages.push(NormalizedMessage {
                                    idx: idx as i64,
                                    role: role.to_string(),
                                    author: None,
                                    created_at: created,
                                    content: content_str,
                                    extra: val,
                                    snippets: Vec::new(),
                                });
                            }
                        }
                        "event_msg" => {
                            // Event messages - filter by payload type
                            if let Some(payload) = val.get("payload") {
                                let event_type = payload.get("type").and_then(|v| v.as_str());

                                match event_type {
                                    Some("user_message") => {
                                        let text = payload
                                            .get("message")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        if !text.is_empty() {
                                            ended_at = created.or(ended_at);
                                            messages.push(NormalizedMessage {
                                                idx: idx as i64,
                                                role: "user".to_string(),
                                                author: None,
                                                created_at: created,
                                                content: text.to_string(),
                                                extra: val,
                                                snippets: Vec::new(),
                                            });
                                        }
                                    }
                                    Some("agent_reasoning") => {
                                        // Include reasoning - valuable for search
                                        let text = payload
                                            .get("text")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        if !text.is_empty() {
                                            ended_at = created.or(ended_at);
                                            messages.push(NormalizedMessage {
                                                idx: idx as i64,
                                                role: "assistant".to_string(),
                                                author: Some("reasoning".to_string()),
                                                created_at: created,
                                                content: text.to_string(),
                                                extra: val,
                                                snippets: Vec::new(),
                                            });
                                        }
                                    }
                                    _ => {} // Skip token_count, turn_aborted, etc.
                                }
                            }
                        }
                        _ => {} // Skip turn_context and unknown types
                    }
                }
            } else if ext == Some("json") {
                // Legacy format: single JSON object with {session, items}
                let val: Value = match serde_json::from_str(&content) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                // Extract workspace from session.cwd
                session_cwd = val
                    .get("session")
                    .and_then(|s| s.get("cwd"))
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from);

                // Parse items array
                if let Some(items) = val.get("items").and_then(|v| v.as_array()) {
                    for (idx, item) in items.iter().enumerate() {
                        let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("agent");

                        let content_str = item
                            .get("content")
                            .map(crate::connectors::flatten_content)
                            .unwrap_or_default();

                        if content_str.trim().is_empty() {
                            continue;
                        }

                        let created = item
                            .get("timestamp")
                            .and_then(crate::connectors::parse_timestamp);

                        if let (Some(since), Some(ts)) = (ctx.since_ts, created)
                            && ts <= since
                        {
                            continue;
                        }

                        started_at = started_at.or(created);
                        ended_at = created.or(ended_at);

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
                continue;
            }

            // Extract title from first user message
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
                    messages
                        .first()
                        .and_then(|m| m.content.lines().next())
                        .map(|s| s.chars().take(100).collect())
                });

            convs.push(NormalizedConversation {
                agent_slug: "codex".to_string(),
                external_id,
                title,
                workspace: session_cwd, // Now populated from session_meta/session.cwd!
                source_path: source_path.clone(),
                started_at,
                ended_at,
                metadata: serde_json::json!({"source": if ext == Some("json") { "rollout_json" } else { "rollout" }}),
                messages,
            });
        }

        Ok(convs)
    }
}
