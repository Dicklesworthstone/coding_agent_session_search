use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

const APPLE_UNIX_OFFSET_SECS: f64 = 978_307_200.0;

pub struct RepoPromptConnector;

impl Default for RepoPromptConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl RepoPromptConnector {
    pub fn new() -> Self {
        Self
    }

    fn home() -> PathBuf {
        std::env::var("REPOPROMPT_HOME").map_or_else(
            |_| {
                dirs::data_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("RepoPrompt")
                    .join("Workspaces")
            },
            PathBuf::from,
        )
    }

    fn session_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in WalkDir::new(root).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");

            if name.starts_with("ChatSession-")
                && name.ends_with(".json")
                && path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    == Some("Chats")
            {
                files.push(path.to_path_buf());
            }
        }
        files
    }

    fn parse_timestamp_apple(val: &Value) -> Option<i64> {
        if let Some(secs) = val.as_f64() {
            let unix_secs = secs + APPLE_UNIX_OFFSET_SECS;
            return Some((unix_secs * 1000.0).round() as i64);
        }

        if let Some(secs) = val.as_i64() {
            let unix_secs = secs as f64 + APPLE_UNIX_OFFSET_SECS;
            return Some((unix_secs * 1000.0).round() as i64);
        }

        crate::connectors::parse_timestamp(val)
    }

    fn derive_workspace(paths: &[PathBuf]) -> Option<PathBuf> {
        if paths.is_empty() {
            return None;
        }

        let mut iter = paths.iter().filter_map(|p| p.parent().map(PathBuf::from));
        let first = iter.next()?;
        let mut prefix_components: Vec<_> = first.components().collect();

        for path in iter {
            let comps: Vec<_> = path.components().collect();
            let mut new_len = 0;
            for (a, b) in prefix_components.iter().zip(&comps) {
                if a == b {
                    new_len += 1;
                } else {
                    break;
                }
            }
            prefix_components.truncate(new_len);
            if prefix_components.is_empty() {
                break;
            }
        }

        if prefix_components.is_empty() {
            return None;
        }

        let mut out = PathBuf::new();
        for c in prefix_components {
            out.push(c);
        }
        Some(out)
    }
}

impl Connector for RepoPromptConnector {
    fn detect(&self) -> DetectionResult {
        let home = Self::home();
        if home.exists() {
            DetectionResult {
                detected: true,
                evidence: vec![format!("found {}", home.display())],
            }
        } else {
            DetectionResult::not_found()
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let root = {
            let looks_like_repoprompt = ctx
                .data_root
                .to_string_lossy()
                .to_lowercase()
                .contains("repoprompt");

            if looks_like_repoprompt || ctx.data_root.join("Chats").exists() {
                ctx.data_root.clone()
            } else {
                Self::home()
            }
        };

        if !root.exists() {
            return Ok(Vec::new());
        }

        let files = Self::session_files(&root);
        let mut convs = Vec::new();

        for file in files {
            if !crate::connectors::file_modified_since(&file, ctx.since_ts) {
                continue;
            }

            let content = fs::read_to_string(&file)
                .with_context(|| format!("read repoprompt chat {}", file.display()))?;

            let val: Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let session_id = val.get("id").and_then(|v| v.as_str()).map(String::from);
            let short_id = val
                .get("shortID")
                .and_then(|v| v.as_str())
                .map(String::from);
            let workspace_id = val
                .get("workspaceID")
                .and_then(|v| v.as_str())
                .map(String::from);
            let name = val.get("name").and_then(|v| v.as_str()).map(String::from);
            let preferred_model = val
                .get("preferredAIModel")
                .and_then(|v| v.as_str())
                .map(String::from);

            let saved_at = val.get("savedAt").and_then(Self::parse_timestamp_apple);

            let mut workspace_paths: Vec<PathBuf> = Vec::new();
            if let Some(arr) = val.get("selectedFilePaths").and_then(|v| v.as_array()) {
                for p in arr.iter().filter_map(|v| v.as_str()) {
                    workspace_paths.push(PathBuf::from(p));
                }
            }

            let Some(messages_arr) = val.get("messages").and_then(|m| m.as_array()) else {
                continue;
            };

            let mut messages = Vec::new();
            let mut started_at = None;
            let mut ended_at = saved_at;

            for item in messages_arr {
                let is_user = item.get("isUser").and_then(|v| v.as_bool()).unwrap_or(true);
                let role = if is_user { "user" } else { "assistant" };

                let created = item.get("timestamp").and_then(Self::parse_timestamp_apple);

                if let Some(ts) = created {
                    started_at = started_at.or(Some(ts));
                    ended_at = Some(ended_at.map_or(ts, |prev| prev.max(ts)));
                }

                let content_str = item
                    .get("rawText")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                if content_str.trim().is_empty() {
                    continue;
                }

                let author = if is_user {
                    Some("user".to_string())
                } else {
                    item.get("modelName")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                };

                if let Some(arr) = item.get("allowedFilePaths").and_then(|v| v.as_array()) {
                    for p in arr.iter().filter_map(|v| v.as_str()) {
                        workspace_paths.push(PathBuf::from(p));
                    }
                }

                messages.push(NormalizedMessage {
                    idx: 0,
                    role: role.to_string(),
                    author,
                    created_at: created,
                    content: content_str,
                    extra: item.clone(),
                    snippets: Vec::new(),
                });
            }

            if messages.is_empty() {
                continue;
            }

            for (i, msg) in messages.iter_mut().enumerate() {
                msg.idx = i as i64;
            }

            let title = name.or_else(|| {
                messages
                    .iter()
                    .find(|m| m.role == "user")
                    .or_else(|| messages.first())
                    .map(|m| {
                        m.content
                            .lines()
                            .next()
                            .unwrap_or(&m.content)
                            .chars()
                            .take(100)
                            .collect()
                    })
            });

            let workspace = Self::derive_workspace(&workspace_paths)
                .or_else(|| file.parent().and_then(|p| p.parent()).map(PathBuf::from));

            let metadata = serde_json::json!({
                "source": "repoprompt",
                "session_id": session_id,
                "short_id": short_id,
                "workspace_id": workspace_id,
                "preferred_model": preferred_model,
                "selected_file_count": workspace_paths.len(),
            });

            convs.push(NormalizedConversation {
                agent_slug: "repoprompt".to_string(),
                external_id: session_id,
                title,
                workspace,
                source_path: file.clone(),
                started_at,
                ended_at,
                metadata,
                messages,
            });
        }

        Ok(convs)
    }
}
