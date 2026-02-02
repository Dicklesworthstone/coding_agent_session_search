//! Connector for OpenClaw session logs.
//!
//! OpenClaw stores JSONL sessions at:
//! - ~/.openclaw/agents/openclaw/sessions/*.jsonl
//!
//! Each line has a `type` discriminator: "session", "message", "model_change",
//! "thinking_level_change", "custom". Messages are wrapped:
//! {"type":"message","id":"...","message":{"role":"user","content":[...],...}}

use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
    file_modified_since, flatten_content, parse_timestamp,
};

pub struct OpenClawConnector;

impl Default for OpenClawConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenClawConnector {
    pub fn new() -> Self {
        Self
    }

    fn sessions_root() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".openclaw")
            .join("agents")
            .join("openclaw")
            .join("sessions")
    }

    fn looks_like_openclaw_storage(path: &Path) -> bool {
        let path_str = path.to_string_lossy().to_lowercase();
        path_str.contains("openclaw") && path_str.contains("sessions")
    }

    fn session_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if !root.exists() {
            return out;
        }

        for entry in WalkDir::new(root).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|s| s.to_str()) == Some("jsonl") {
                out.push(entry.path().to_path_buf());
            }
        }

        out
    }

    /// Flatten OpenClaw content blocks into a single string.
    /// Content is an array of blocks: text, toolCall, thinking.
    fn flatten_openclaw_content(content: &Value) -> String {
        match content {
            Value::String(s) => s.clone(),
            Value::Array(arr) => {
                let parts: Vec<String> = arr
                    .iter()
                    .filter_map(|block| {
                        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match block_type {
                            "text" => block.get("text").and_then(|t| t.as_str()).map(String::from),
                            "toolCall" => {
                                let name = block
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("tool_call");
                                Some(format!("[tool: {name}]"))
                            }
                            "thinking" => {
                                block.get("text").and_then(|t| t.as_str()).map(String::from)
                            }
                            _ => block.get("text").and_then(|t| t.as_str()).map(String::from),
                        }
                    })
                    .collect();
                parts.join("\n")
            }
            _ => flatten_content(content),
        }
    }
}

impl Connector for OpenClawConnector {
    fn detect(&self) -> DetectionResult {
        let root = Self::sessions_root();
        if root.exists() && root.is_dir() {
            DetectionResult {
                detected: true,
                evidence: vec![format!("found {}", root.display())],
                root_paths: vec![root],
            }
        } else {
            // Also check parent dir in case sessions dir hasn't been created yet
            let parent = dirs::home_dir().unwrap_or_default().join(".openclaw");
            if parent.exists() {
                DetectionResult {
                    detected: true,
                    evidence: vec![format!("found {}", parent.display())],
                    root_paths: vec![Self::sessions_root()],
                }
            } else {
                DetectionResult::not_found()
            }
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut roots: Vec<PathBuf> = Vec::new();

        if ctx.use_default_detection() {
            if Self::looks_like_openclaw_storage(&ctx.data_dir) && ctx.data_dir.exists() {
                roots.push(ctx.data_dir.clone());
            } else {
                let root = Self::sessions_root();
                if root.exists() {
                    roots.push(root);
                }
            }
        } else {
            for root in &ctx.scan_roots {
                let candidate = root
                    .path
                    .join(".openclaw")
                    .join("agents")
                    .join("openclaw")
                    .join("sessions");
                if candidate.exists() {
                    roots.push(candidate);
                } else if Self::looks_like_openclaw_storage(&root.path) && root.path.exists() {
                    roots.push(root.path.clone());
                }
            }
        }

        if roots.is_empty() {
            return Ok(Vec::new());
        }

        let mut convs = Vec::new();

        for mut root in roots {
            if root.is_file() {
                root = root.parent().unwrap_or(&root).to_path_buf();
            }

            let files = Self::session_files(&root);
            for file in files {
                if !file_modified_since(&file, ctx.since_ts) {
                    continue;
                }

                let source_path = file.clone();
                let external_id = source_path
                    .strip_prefix(&root)
                    .ok()
                    .and_then(|rel| {
                        rel.with_extension("")
                            .to_str()
                            .map(std::string::ToString::to_string)
                    })
                    .or_else(|| {
                        source_path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .map(std::string::ToString::to_string)
                    });

                let file_handle = match fs::File::open(&file) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::debug!(path = %file.display(), error = %e, "openclaw: skipping unreadable session");
                        continue;
                    }
                };
                let reader = std::io::BufReader::new(file_handle);

                let mut messages = Vec::new();
                let mut started_at: Option<i64> = None;
                let mut ended_at: Option<i64> = None;
                let mut session_cwd: Option<String> = None;

                for line_res in reader.lines() {
                    let line = match line_res {
                        Ok(l) => l,
                        Err(_) => continue,
                    };
                    if line.trim().is_empty() {
                        continue;
                    }

                    let val: Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    let line_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

                    match line_type {
                        "session" => {
                            // Extract session metadata
                            session_cwd = val.get("cwd").and_then(|v| v.as_str()).map(String::from);
                            if let Some(ts) = val.get("timestamp").and_then(parse_timestamp) {
                                started_at = Some(ts);
                            }
                        }
                        "message" => {
                            // Messages are wrapped: {type:"message", message:{role, content, ...}}
                            let msg = match val.get("message") {
                                Some(m) => m,
                                None => continue,
                            };

                            let role = msg
                                .get("role")
                                .and_then(|v| v.as_str())
                                .unwrap_or("assistant");

                            let content = msg
                                .get("content")
                                .map(Self::flatten_openclaw_content)
                                .unwrap_or_default();

                            if content.trim().is_empty() {
                                continue;
                            }

                            // Timestamps can be on the wrapper or inner message
                            let created = val
                                .get("timestamp")
                                .and_then(parse_timestamp)
                                .or_else(|| msg.get("timestamp").and_then(parse_timestamp));

                            started_at = started_at.or(created);
                            ended_at = created.or(ended_at);

                            messages.push(NormalizedMessage {
                                idx: messages.len() as i64,
                                role: role.to_string(),
                                author: msg.get("model").and_then(|v| v.as_str()).map(String::from),
                                created_at: created,
                                content,
                                extra: val,
                                snippets: Vec::new(),
                            });
                        }
                        // Skip model_change, thinking_level_change, custom, etc.
                        _ => continue,
                    }
                }

                if messages.is_empty() {
                    continue;
                }

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

                let workspace = session_cwd.as_ref().map(PathBuf::from);

                let metadata = serde_json::json!({
                    "source": "openclaw",
                    "cwd": session_cwd,
                });

                convs.push(NormalizedConversation {
                    agent_slug: "openclaw".to_string(),
                    external_id,
                    title,
                    workspace,
                    source_path,
                    started_at,
                    ended_at,
                    metadata,
                    messages,
                });
            }
        }

        Ok(convs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_session(root: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let path = root.join(name);
        let content = lines.join("\n");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn scan_parses_openclaw_wrapped_messages() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
        fs::create_dir_all(&sessions).unwrap();

        write_session(
            &sessions,
            "session.jsonl",
            &[
                r#"{"type":"session","id":"abc","timestamp":"2026-02-01T16:00:00.000Z","cwd":"/home/user/project","version":"0.1.0"}"#,
                r#"{"type":"message","id":"m1","parentId":"abc","timestamp":"2026-02-01T16:00:00.828Z","message":{"role":"user","content":[{"type":"text","text":"Hello OpenClaw"}],"timestamp":1769961600827}}"#,
                r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-02-01T16:00:06.672Z","message":{"role":"assistant","content":[{"type":"text","text":"Hi there!"},{"type":"toolCall","id":"tc1","name":"exec","arguments":{}}],"api":"anthropic-messages","provider":"anthropic","model":"claude-opus-4-5"}}"#,
            ],
        );

        let connector = OpenClawConnector::new();
        let ctx = ScanContext::local_default(sessions.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].agent_slug, "openclaw");
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].title, Some("Hello OpenClaw".to_string()));
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[1].role, "assistant");
        assert!(convs[0].messages[1].content.contains("Hi there!"));
        assert!(convs[0].messages[1].content.contains("[tool: exec]"));
        assert_eq!(
            convs[0].messages[1].author,
            Some("claude-opus-4-5".to_string())
        );
        assert!(convs[0].workspace.is_some());
        assert!(convs[0].started_at.is_some());
    }

    #[test]
    fn scan_skips_non_message_types() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
        fs::create_dir_all(&sessions).unwrap();

        write_session(
            &sessions,
            "session2.jsonl",
            &[
                r#"{"type":"session","id":"s1","timestamp":"2026-02-01T16:00:00.000Z","cwd":"/"}"#,
                r#"{"type":"model_change","model":"gpt-5"}"#,
                r#"{"type":"thinking_level_change","level":"high"}"#,
                r#"{"type":"message","id":"m1","timestamp":"2026-02-01T16:00:01.000Z","message":{"role":"user","content":"Only message"}}"#,
                r#"{"type":"custom","data":"something"}"#,
            ],
        );

        let connector = OpenClawConnector::new();
        let ctx = ScanContext::local_default(sessions.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].content, "Only message");
    }

    #[test]
    fn scan_handles_empty_and_invalid_lines() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
        fs::create_dir_all(&sessions).unwrap();

        write_session(
            &sessions,
            "bad.jsonl",
            &[
                "",
                "not-json",
                r#"{"type":"message","id":"m1","timestamp":"2026-02-01T16:00:00.000Z","message":{"role":"user","content":"Valid"}}"#,
                r#"{"type":"message","id":"m2","message":{"role":"assistant","content":""}}"#,
            ],
        );

        let connector = OpenClawConnector::new();
        let ctx = ScanContext::local_default(sessions.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        // Only the valid non-empty message should appear
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].content, "Valid");
    }
}
