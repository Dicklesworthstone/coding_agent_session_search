//! Connector for pi-mono coding-agent (https://github.com/badlogic/pi-mono)
//!
//! Pi-Agent stores sessions in JSONL files under:
//! - `~/.pi/agent/sessions/<safe-path>/` where safe-path is derived from the working directory
//! - Each session file is named `<timestamp>_<uuid>.jsonl`
//!
//! JSONL entry types:
//! - `session`: Header with id, timestamp, cwd, provider, modelId, thinkingLevel
//! - `message`: Contains timestamp and message object with role (user/assistant/toolResult)
//! - `thinking_level_change`: Records thinking level changes
//! - `model_change`: Records model/provider changes

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
    file_modified_since, parse_timestamp,
};

pub struct PiAgentConnector;

impl Default for PiAgentConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl PiAgentConnector {
    pub fn new() -> Self {
        Self
    }

    /// Get the pi-agent home directory.
    /// Checks PI_CODING_AGENT_DIR env var, falls back to ~/.pi/agent/
    fn home() -> PathBuf {
        std::env::var("PI_CODING_AGENT_DIR").map_or_else(
            |_| dirs::home_dir().unwrap_or_default().join(".pi/agent"),
            PathBuf::from,
        )
    }

    /// Find all session JSONL files under the sessions directory.
    fn session_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let sessions = root.join("sessions");
        if !sessions.exists() {
            return out;
        }
        for entry in WalkDir::new(sessions).into_iter().flatten() {
            if entry.file_type().is_file() {
                let name = entry.file_name().to_str().unwrap_or("");
                // Pi-agent session files are named <timestamp>_<uuid>.jsonl
                if name.ends_with(".jsonl") && name.contains('_') {
                    out.push(entry.path().to_path_buf());
                }
            }
        }
        out
    }

    /// Flatten pi-agent message content to a searchable string.
    /// Handles the message.content array which can contain:
    /// - TextContent: {type: "text", text: "..."}
    /// - ThinkingContent: {type: "thinking", thinking: "..."}
    /// - ToolCall: {type: "toolCall", name: "...", arguments: {...}}
    /// - ImageContent: {type: "image", ...} (skip for text extraction)
    fn flatten_message_content(content: &Value) -> String {
        // Direct string content (simple user messages)
        if let Some(s) = content.as_str() {
            return s.to_string();
        }

        // Array of content blocks
        if let Some(arr) = content.as_array() {
            let parts: Vec<String> = arr
                .iter()
                .filter_map(|item| {
                    let item_type = item.get("type").and_then(|v| v.as_str());

                    match item_type {
                        Some("text") => item.get("text").and_then(|v| v.as_str()).map(String::from),
                        Some("thinking") => {
                            // Include thinking content - valuable for search
                            item.get("thinking")
                                .and_then(|v| v.as_str())
                                .map(|t| format!("[Thinking] {t}"))
                        }
                        Some("toolCall") => {
                            // Include tool calls for searchability
                            let name = item
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            let args = item
                                .get("arguments")
                                .map(|a| {
                                    // Extract key argument values for context
                                    if let Some(obj) = a.as_object() {
                                        obj.iter()
                                            .filter_map(|(k, v)| {
                                                v.as_str().map(|s| format!("{k}={s}"))
                                            })
                                            .take(3) // Limit to avoid huge strings
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    } else {
                                        String::new()
                                    }
                                })
                                .unwrap_or_default();
                            if args.is_empty() {
                                Some(format!("[Tool: {name}]"))
                            } else {
                                Some(format!("[Tool: {name}] {args}"))
                            }
                        }
                        Some("image") => None, // Skip image content
                        _ => None,
                    }
                })
                .collect();
            return parts.join("\n");
        }

        String::new()
    }
}

impl Connector for PiAgentConnector {
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
        // Use data_root if it looks like a pi-agent directory (for testing)
        let is_pi_agent_dir = ctx
            .data_root
            .to_str()
            .map(|s| {
                s.contains(".pi/agent") || s.ends_with("/pi-agent") || s.ends_with("\\pi-agent")
            })
            .unwrap_or(false);
        let home = if is_pi_agent_dir {
            ctx.data_root.clone()
        } else {
            Self::home()
        };

        let files = Self::session_files(&home);
        let mut convs = Vec::new();

        for file in files {
            // Skip files not modified since last scan
            if !file_modified_since(&file, ctx.since_ts) {
                continue;
            }

            let source_path = file.clone();

            // Use the parent directory name + filename as external_id
            // e.g., "--Users-foo-project--/2024-01-15T10-30-00_uuid.jsonl"
            let sessions_dir = home.join("sessions");
            let external_id = source_path
                .strip_prefix(&sessions_dir)
                .ok()
                .and_then(|rel| rel.to_str().map(String::from))
                .or_else(|| {
                    source_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(String::from)
                });

            let content = fs::read_to_string(&file)
                .with_context(|| format!("read pi-agent session {}", file.display()))?;

            let mut messages = Vec::new();
            let mut started_at: Option<i64> = None;
            let mut ended_at: Option<i64> = None;
            let mut session_cwd: Option<PathBuf> = None;
            let mut session_id: Option<String> = None;
            let mut provider: Option<String> = None;
            let mut model_id: Option<String> = None;

            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let val: Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let entry_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

                match entry_type {
                    "session" => {
                        // Session header - extract metadata
                        session_id = val.get("id").and_then(|v| v.as_str()).map(String::from);
                        session_cwd = val.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
                        provider = val
                            .get("provider")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                        model_id = val
                            .get("modelId")
                            .and_then(|v| v.as_str())
                            .map(String::from);

                        // Parse timestamp
                        if let Some(ts_val) = val.get("timestamp") {
                            started_at = parse_timestamp(ts_val);
                        }
                    }
                    "message" => {
                        // Message entry - extract the nested message object
                        let created = val.get("timestamp").and_then(parse_timestamp);

                        if let Some(msg) = val.get("message") {
                            let role = msg
                                .get("role")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");

                            // Normalize role names
                            let normalized_role = match role {
                                "user" => "user",
                                "assistant" => "assistant",
                                "toolResult" => "tool",
                                _ => role,
                            };

                            // Extract content
                            let content_str = msg
                                .get("content")
                                .map(Self::flatten_message_content)
                                .unwrap_or_default();

                            if content_str.trim().is_empty() {
                                continue;
                            }

                            // Update timestamps
                            if started_at.is_none() {
                                started_at = created;
                            }
                            ended_at = created.or(ended_at);

                            messages.push(NormalizedMessage {
                                idx: messages.len() as i64,
                                role: normalized_role.to_string(),
                                author: None,
                                created_at: created,
                                content: content_str,
                                extra: val.clone(),
                                snippets: Vec::new(),
                            });
                        }
                    }
                    "model_change" => {
                        // Track model changes (useful metadata)
                        provider = val
                            .get("provider")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                        model_id = val
                            .get("modelId")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                    }
                    _ => {
                        // Skip thinking_level_change and unknown types
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

            // Build metadata
            let metadata = serde_json::json!({
                "source": "pi_agent",
                "session_id": session_id,
                "provider": provider,
                "model_id": model_id,
            });

            convs.push(NormalizedConversation {
                agent_slug: "pi_agent".to_string(),
                external_id,
                title,
                workspace: session_cwd,
                source_path: source_path.clone(),
                started_at,
                ended_at,
                metadata,
                messages,
            });
        }

        Ok(convs)
    }
}
