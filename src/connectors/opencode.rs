use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

pub struct OpenCodeConnector;
impl Default for OpenCodeConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenCodeConnector {
    pub fn new() -> Self {
        Self
    }

    /// Get the OpenCode global storage directory.
    /// OpenCode stores sessions in ~/.local/share/opencode/storage/
    fn storage_root() -> Option<PathBuf> {
        // Primary location: XDG data directory
        if let Some(data) = dirs::data_local_dir() {
            let storage_dir = data.join("opencode/storage");
            if storage_dir.exists() {
                return Some(storage_dir);
            }
        }

        // Fallback: ~/.local/share/opencode/storage
        if let Some(home) = dirs::home_dir() {
            let storage_dir = home.join(".local/share/opencode/storage");
            if storage_dir.exists() {
                return Some(storage_dir);
            }
        }

        None
    }
}

/// OpenCode session info from session/<project_hash>/<session_id>.json
#[derive(Debug, Deserialize)]
struct SessionInfo {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    directory: Option<String>,
    #[serde(rename = "projectID", default)]
    project_id: Option<String>,
    #[serde(default)]
    time: Option<SessionTime>,
}

#[derive(Debug, Deserialize)]
struct SessionTime {
    #[serde(default)]
    created: Option<i64>,
    #[serde(default)]
    updated: Option<i64>,
}

/// OpenCode message from message/<session_id>/<msg_id>.json
#[derive(Debug, Deserialize)]
struct MessageInfo {
    id: String,
    role: String,
    #[serde(default)]
    time: Option<MessageTime>,
    #[serde(rename = "modelID", default)]
    model_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageTime {
    #[serde(default)]
    created: Option<i64>,
    #[serde(default)]
    completed: Option<i64>,
}

/// OpenCode part (content) from part/<session_id>/<msg_id>/<part_id>.json
#[derive(Debug, Deserialize)]
struct PartInfo {
    #[serde(rename = "type", default)]
    part_type: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    state: Option<ToolState>,
}

/// Tool state for tool parts
#[derive(Debug, Deserialize)]
struct ToolState {
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    metadata: Option<ToolMetadata>,
}

#[derive(Debug, Deserialize)]
struct ToolMetadata {
    #[serde(default)]
    preview: Option<String>,
}

impl Connector for OpenCodeConnector {
    fn detect(&self) -> DetectionResult {
        if let Some(root) = Self::storage_root() {
            DetectionResult {
                detected: true,
                evidence: vec![format!("found {}", root.display())],
            }
        } else {
            DetectionResult::not_found()
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Determine storage root directory
        let storage_root = if ctx.data_root.exists()
            && (ctx
                .data_root
                .to_str()
                .is_some_and(|s| s.contains("opencode"))
                || ctx.data_root.join("session").exists())
        {
            // Test mode or custom path with opencode storage structure
            ctx.data_root.clone()
        } else if let Some(root) = Self::storage_root() {
            root
        } else {
            return Ok(Vec::new());
        };

        tracing::debug!(root = %storage_root.display(), "opencode scanning storage root");

        let session_root = storage_root.join("session");
        let message_root = storage_root.join("message");
        let part_root = storage_root.join("part");

        if !session_root.exists() {
            tracing::debug!("opencode session directory does not exist");
            return Ok(Vec::new());
        }

        let mut convs = Vec::new();
        let mut seen_sessions = std::collections::HashSet::new();

        // Walk through all project directories under session/
        // Structure: session/<project_hash>/<session_id>.json
        for project_entry in WalkDir::new(&session_root)
            .min_depth(1)
            .max_depth(1)
            .into_iter()
            .flatten()
        {
            if !project_entry.file_type().is_dir() {
                continue;
            }

            let project_dir = project_entry.path();
            tracing::debug!(project = %project_dir.display(), "opencode scanning project");

            // Read all session files in this project directory
            for session_entry in WalkDir::new(project_dir)
                .min_depth(1)
                .max_depth(1)
                .into_iter()
                .flatten()
            {
                if !session_entry.file_type().is_file() {
                    continue;
                }

                let session_path = session_entry.path();
                if session_path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }

                // Skip files not modified since last scan
                if !crate::connectors::file_modified_since(session_path, ctx.since_ts) {
                    continue;
                }

                // Parse session info
                let content = match fs::read_to_string(session_path) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::debug!(path = %session_path.display(), error = %e, "opencode failed to read session");
                        continue;
                    }
                };

                let session_info: SessionInfo = match serde_json::from_str(&content) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!(path = %session_path.display(), error = %e, "opencode failed to parse session");
                        continue;
                    }
                };

                // Deduplicate sessions
                if !seen_sessions.insert(session_info.id.clone()) {
                    continue;
                }

                // Get workspace from session directory field
                let workspace = session_info.directory.as_ref().map(PathBuf::from);

                // Find and read messages for this session
                // Messages are in: message/<session_id>/<msg_id>.json
                // Parts are in: part/<msg_id>/<part_id>.json (NOTE: no session_id in path!)
                let session_message_dir = message_root.join(&session_info.id);

                let mut messages = Vec::new();
                let mut started_at = session_info.time.as_ref().and_then(|t| t.created);
                let mut ended_at = session_info.time.as_ref().and_then(|t| t.updated);

                if session_message_dir.exists() {
                    // Collect and sort message files by filename (contains timestamp)
                    let mut message_files: Vec<_> = WalkDir::new(&session_message_dir)
                        .min_depth(1)
                        .max_depth(1)
                        .into_iter()
                        .flatten()
                        .filter(|e| {
                            e.file_type().is_file()
                                && e.path().extension().and_then(|x| x.to_str()) == Some("json")
                        })
                        .collect();

                    message_files.sort_by_key(|e| e.file_name().to_os_string());

                    for msg_entry in message_files {
                        let msg_path = msg_entry.path();
                        let msg_content = match fs::read_to_string(msg_path) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };

                        let msg_info: MessageInfo = match serde_json::from_str(&msg_content) {
                            Ok(m) => m,
                            Err(_) => continue,
                        };

                        // Get message timestamp
                        let created = msg_info
                            .time
                            .as_ref()
                            .and_then(|t| t.created.or(t.completed));
                        if started_at.is_none() {
                            started_at = created;
                        }
                        if let Some(ts) = created {
                            ended_at = Some(ended_at.map_or(ts, |e| e.max(ts)));
                        }

                        // Get content from part files
                        // Parts are in: part/<msg_id>/<part_id>.json
                        let mut content_parts = Vec::new();
                        let msg_part_dir = part_root.join(&msg_info.id);

                        if msg_part_dir.exists() {
                            let mut part_files: Vec<_> = WalkDir::new(&msg_part_dir)
                                .min_depth(1)
                                .max_depth(1)
                                .into_iter()
                                .flatten()
                                .filter(|e| {
                                    e.file_type().is_file()
                                        && e.path().extension().and_then(|x| x.to_str())
                                            == Some("json")
                                })
                                .collect();

                            part_files.sort_by_key(|e| e.file_name().to_os_string());

                            for part_entry in part_files {
                                let part_content = match fs::read_to_string(part_entry.path()) {
                                    Ok(c) => c,
                                    Err(_) => continue,
                                };

                                let part_info: PartInfo = match serde_json::from_str(&part_content) {
                                    Ok(p) => p,
                                    Err(_) => continue,
                                };

                                // Extract content based on part type
                                match part_info.part_type.as_deref() {
                                    Some("text") => {
                                        if let Some(text) =
                                            part_info.text.filter(|t| !t.trim().is_empty())
                                        {
                                            content_parts.push(text);
                                        }
                                    }
                                    Some("tool") => {
                                        // Include tool output/preview for searchability
                                        if let Some(state) = &part_info.state {
                                            if let Some(preview) = state
                                                .metadata
                                                .as_ref()
                                                .and_then(|m| m.preview.as_ref())
                                            {
                                                if !preview.trim().is_empty() {
                                                    content_parts
                                                        .push(format!("[Tool Output]\n{preview}"));
                                                }
                                            } else if let Some(output) = &state.output {
                                                // Truncate very long tool outputs (UTF-8 safe)
                                                let truncated: String = if output.chars().count() > 500 {
                                                    format!("{}...", output.chars().take(500).collect::<String>())
                                                } else {
                                                    output.clone()
                                                };
                                                if !truncated.trim().is_empty() {
                                                    content_parts
                                                        .push(format!("[Tool Output]\n{truncated}"));
                                                }
                                            }
                                        }
                                    }
                                    // Skip step-start, step-finish, snapshot, patch, etc.
                                    _ => {}
                                }
                            }
                        }

                        let content = content_parts.join("\n\n");
                        if content.trim().is_empty() {
                            continue;
                        }

                        messages.push(NormalizedMessage {
                            idx: messages.len() as i64,
                            role: msg_info.role.clone(),
                            author: msg_info.model_id.clone(),
                            created_at: created,
                            content,
                            extra: serde_json::json!({
                                "message_id": msg_info.id,
                                "model": msg_info.model_id,
                            }),
                            snippets: Vec::new(),
                        });
                    }
                }

                if messages.is_empty() {
                    tracing::debug!(
                        session_id = %session_info.id,
                        "opencode no messages found for session"
                    );
                    continue;
                }

                tracing::debug!(
                    session_id = %session_info.id,
                    message_count = messages.len(),
                    "opencode extracted session"
                );

                convs.push(NormalizedConversation {
                    agent_slug: "opencode".into(),
                    external_id: Some(session_info.id.clone()),
                    title: session_info.title.or_else(|| {
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
                                    .collect()
                            })
                    }),
                    workspace,
                    source_path: session_path.to_path_buf(),
                    started_at,
                    ended_at,
                    metadata: serde_json::json!({
                        "source": "opencode",
                        "session_id": session_info.id,
                        "project_id": session_info.project_id,
                    }),
                    messages,
                });
            }
        }

        tracing::info!(
            conversations = convs.len(),
            "opencode scan complete"
        );

        Ok(convs)
    }
}
