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

    /// Get candidate directories where OpenCode stores its data.
    /// OpenCode uses ~/.local/share/opencode/project/ on Linux/macOS.
    fn data_root() -> Option<PathBuf> {
        // Primary location: XDG data directory
        if let Some(data) = dirs::data_local_dir() {
            let opencode_dir = data.join("opencode/project");
            if opencode_dir.exists() {
                return Some(opencode_dir);
            }
        }

        // Fallback: ~/.local/share/opencode/project
        if let Some(home) = dirs::home_dir() {
            let opencode_dir = home.join(".local/share/opencode/project");
            if opencode_dir.exists() {
                return Some(opencode_dir);
            }
        }

        None
    }

    /// Find all project directories containing session storage.
    fn find_project_dirs(root: &PathBuf) -> Vec<PathBuf> {
        let mut projects = Vec::new();

        // Walk the root looking for directories with storage/session subdirectories
        for entry in WalkDir::new(root)
            .max_depth(10) // Limit depth to avoid infinite recursion in nested project/project dirs
            .into_iter()
            .flatten()
        {
            if entry.file_type().is_dir() {
                let session_dir = entry.path().join("storage/session");
                if session_dir.exists() && session_dir.is_dir() {
                    projects.push(entry.path().to_path_buf());
                }
            }
        }

        projects
    }
}

/// OpenCode session info from info/*.json
#[derive(Debug, Deserialize)]
struct SessionInfo {
    id: String,
    #[serde(default)]
    title: Option<String>,
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

/// OpenCode message from message/<session>/<msg>.json
#[derive(Debug, Deserialize)]
struct MessageInfo {
    id: String,
    role: String,
    #[serde(default)]
    path: Option<MessagePath>,
    #[serde(default)]
    time: Option<MessageTime>,
    #[serde(rename = "modelID", default)]
    model_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessagePath {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    root: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageTime {
    #[serde(default)]
    created: Option<i64>,
    #[serde(default)]
    completed: Option<i64>,
}

/// OpenCode part (content) from part/<session>/<msg>/<part>.json
#[derive(Debug, Deserialize)]
struct PartInfo {
    #[serde(rename = "type", default)]
    part_type: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

impl Connector for OpenCodeConnector {
    fn detect(&self) -> DetectionResult {
        if let Some(root) = Self::data_root() {
            DetectionResult {
                detected: true,
                evidence: vec![format!("found {}", root.display())],
            }
        } else {
            DetectionResult::not_found()
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Determine root directory to scan
        let root = if ctx.data_root.exists()
            && (ctx
                .data_root
                .to_str()
                .is_some_and(|s| s.contains("opencode"))
                || ctx.data_root.join("storage/session").exists())
        {
            // Test mode or custom path with opencode structure
            ctx.data_root.clone()
        } else if let Some(data_root) = Self::data_root() {
            data_root
        } else {
            return Ok(Vec::new());
        };

        tracing::debug!(root = %root.display(), "opencode scanning root");

        let mut convs = Vec::new();
        let mut seen_sessions = std::collections::HashSet::new();

        // Find all project directories
        let project_dirs = if root.join("storage/session").exists() {
            // The root itself is a project directory
            vec![root.clone()]
        } else {
            Self::find_project_dirs(&root)
        };

        tracing::debug!(count = project_dirs.len(), "opencode found project directories");

        for project_dir in project_dirs {
            let session_root = project_dir.join("storage/session");
            if !session_root.exists() {
                continue;
            }

            let info_dir = session_root.join("info");
            let message_dir = session_root.join("message");
            let part_dir = session_root.join("part");

            if !info_dir.exists() {
                continue;
            }

            // Read all session info files
            for entry in WalkDir::new(&info_dir)
                .max_depth(1)
                .into_iter()
                .flatten()
            {
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }

                // Skip files not modified since last scan
                if !crate::connectors::file_modified_since(path, ctx.since_ts) {
                    continue;
                }

                // Parse session info
                let content = match fs::read_to_string(path) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::debug!(path = %path.display(), error = %e, "opencode failed to read session info");
                        continue;
                    }
                };

                let session_info: SessionInfo = match serde_json::from_str(&content) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!(path = %path.display(), error = %e, "opencode failed to parse session info");
                        continue;
                    }
                };

                // Deduplicate sessions across nested project directories
                if !seen_sessions.insert(session_info.id.clone()) {
                    continue;
                }

                // Find and read messages for this session
                let session_message_dir = message_dir.join(&session_info.id);
                let session_part_dir = part_dir.join(&session_info.id);

                let mut messages = Vec::new();
                let mut workspace: Option<PathBuf> = None;
                let mut started_at = session_info.time.as_ref().and_then(|t| t.created);
                let mut ended_at = session_info.time.as_ref().and_then(|t| t.updated);

                if session_message_dir.exists() {
                    // Collect message files
                    let mut message_files: Vec<_> = WalkDir::new(&session_message_dir)
                        .max_depth(1)
                        .into_iter()
                        .flatten()
                        .filter(|e| {
                            e.file_type().is_file()
                                && e.path().extension().and_then(|x| x.to_str()) == Some("json")
                        })
                        .collect();

                    // Sort by filename to maintain order
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

                        // Extract workspace from first message with path info
                        if workspace.is_none() && msg_info.path.is_some() {
                            let path_info = msg_info.path.as_ref().unwrap();
                            workspace = path_info
                                .root
                                .as_ref()
                                .or(path_info.cwd.as_ref())
                                .map(PathBuf::from);
                        }

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
                        let mut content_parts = Vec::new();
                        let msg_part_dir = session_part_dir.join(&msg_info.id);

                        if msg_part_dir.exists() {
                            let mut part_files: Vec<_> = WalkDir::new(&msg_part_dir)
                                .max_depth(1)
                                .into_iter()
                                .flatten()
                                .filter(|e| {
                                    e.file_type().is_file()
                                        && e.path().extension().and_then(|x| x.to_str())
                                            == Some("json")
                                })
                                .collect();

                            // Sort parts by filename
                            part_files.sort_by_key(|e| e.file_name().to_os_string());

                            for part_entry in part_files {
                                let part_content = match fs::read_to_string(part_entry.path()) {
                                    Ok(c) => c,
                                    Err(_) => continue,
                                };

                                let part_info: PartInfo = match serde_json::from_str(&part_content)
                                {
                                    Ok(p) => p,
                                    Err(_) => continue,
                                };

                                // Include text parts
                                if let Some(text) = part_info.text.filter(|t| !t.trim().is_empty()) {
                                    // For tool_result or other special types, add context
                                    if part_info.part_type.as_deref() == Some("tool_result") {
                                        content_parts.push(format!("[Tool Result]\n{text}"));
                                    } else {
                                        content_parts.push(text);
                                    }
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

                // Determine source path - use the session info file
                let source_path = path.to_path_buf();

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
                    source_path,
                    started_at,
                    ended_at,
                    metadata: serde_json::json!({
                        "source": "opencode",
                        "session_id": session_info.id,
                        "project_dir": project_dir.display().to_string(),
                    }),
                    messages,
                });
            }
        }

        Ok(convs)
    }
}
