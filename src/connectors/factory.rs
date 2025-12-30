use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

pub struct FactoryConnector;

impl Default for FactoryConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl FactoryConnector {
    pub fn new() -> Self {
        Self
    }

    fn sessions_root() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".factory/sessions")
    }
}

impl Connector for FactoryConnector {
    fn detect(&self) -> DetectionResult {
        let root = Self::sessions_root();
        if root.exists() {
            DetectionResult {
                detected: true,
                evidence: vec![format!("found {}", root.display())],
                root_paths: vec![root],
            }
        } else {
            DetectionResult::not_found()
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let looks_like_root = |path: &PathBuf| {
            path.join("sessions").exists()
                || path
                    .file_name()
                    .is_some_and(|n| n.to_str().unwrap_or("").contains("factory"))
                || path
                    .file_name()
                    .is_some_and(|n| n.to_str().unwrap_or("") == "sessions")
        };

        let mut root = if ctx.use_default_detection() {
            if looks_like_root(&ctx.data_dir) {
                ctx.data_dir.clone()
            } else {
                Self::sessions_root()
            }
        } else {
            ctx.data_dir.clone()
        };

        if root.is_file() {
            root = root.parent().unwrap_or(&root).to_path_buf();
        }

        if !ctx.use_default_detection() && !looks_like_root(&root) {
            return Ok(Vec::new());
        }

        if !root.exists() {
            return Ok(Vec::new());
        }

        let mut convs = Vec::new();
        let mut file_count = 0;

        for entry in WalkDir::new(&root).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }

            let ext = entry.path().extension().and_then(|s| s.to_str());
            if ext != Some("jsonl") {
                continue;
            }

            if !crate::connectors::file_modified_since(entry.path(), ctx.since_ts) {
                continue;
            }

            file_count += 1;
            if file_count <= 3 {
                tracing::debug!(path = %entry.path().display(), "factory found file");
            }

            let mut messages = Vec::new();
            let mut started_at = None;
            let mut ended_at = None;
            let mut workspace: Option<PathBuf> = None;
            let mut session_id: Option<String> = None;
            let mut session_title: Option<String> = None;

            let file = fs::File::open(entry.path())
                .with_context(|| format!("open {}", entry.path().display()))?;
            let reader = std::io::BufReader::new(file);

            for line_res in std::io::BufRead::lines(reader) {
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

                let entry_type = val.get("type").and_then(|v| v.as_str());

                // Handle session_start metadata
                if entry_type == Some("session_start") {
                    session_id = val.get("id").and_then(|v| v.as_str()).map(String::from);
                    session_title = val.get("title").and_then(|v| v.as_str()).map(String::from);
                    continue;
                }

                // Only process message entries
                if entry_type != Some("message") {
                    continue;
                }

                let created = val
                    .get("timestamp")
                    .and_then(crate::connectors::parse_timestamp);

                started_at = started_at.or(created);
                ended_at = created.or(ended_at);

                // Extract role from message.role
                let role = val
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("agent");

                // Extract content from message.content array
                let content_val = val.get("message").and_then(|m| m.get("content"));
                let content_str = content_val
                    .map(crate::connectors::flatten_content)
                    .unwrap_or_default();

                if content_str.trim().is_empty() {
                    continue;
                }

                // Extract model name for author field
                let author = val
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(|v| v.as_str())
                    .map(String::from);

                messages.push(NormalizedMessage {
                    idx: 0,
                    role: role.to_string(),
                    author,
                    created_at: created,
                    content: content_str,
                    extra: val,
                    snippets: Vec::new(),
                });
            }

            // Re-assign sequential indices after filtering
            for (i, msg) in messages.iter_mut().enumerate() {
                msg.idx = i as i64;
            }

            if messages.is_empty() {
                if file_count <= 3 {
                    tracing::debug!(path = %entry.path().display(), "factory no messages extracted");
                }
                continue;
            }

            tracing::debug!(path = %entry.path().display(), messages = messages.len(), "factory extracted messages");

            // Try to extract workspace from directory structure
            // Factory stores sessions in directories named after workspace paths
            // e.g., ~/.factory/sessions/-Users-username-Git-Project/uuid.jsonl
            if workspace.is_none() {
                if let Some(parent) = entry.path().parent() {
                    let parent_name = parent.file_name().and_then(|n| n.to_str());
                    if let Some(name) = parent_name {
                        if name.starts_with('-') && name != "sessions" {
                            // Convert -Users-username-Git-Project back to /Users/username/Git/Project
                            let path_str = name.replace('-', "/");
                            workspace = Some(PathBuf::from(path_str));
                        }
                    }
                }
            }

            // Use session title or first user message for title
            let title = session_title.clone().or_else(|| {
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
                            .collect::<String>()
                    })
                    .or_else(|| {
                        workspace
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .and_then(|n| n.to_str())
                            .map(String::from)
                    })
            });

            convs.push(NormalizedConversation {
                agent_slug: "factory".into(),
                external_id: session_id.or_else(|| {
                    entry
                        .path()
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(std::string::ToString::to_string)
                }),
                title,
                workspace,
                source_path: entry.path().to_path_buf(),
                started_at,
                ended_at,
                metadata: serde_json::json!({
                    "source": "factory"
                }),
                messages,
            });
        }

        Ok(convs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn new_creates_connector() {
        let connector = FactoryConnector::new();
        let _ = connector;
    }

    #[test]
    fn default_creates_connector() {
        let connector = FactoryConnector;
        let _ = connector;
    }

    #[test]
    fn sessions_root_returns_factory_sessions_path() {
        let root = FactoryConnector::sessions_root();
        assert!(root.ends_with(".factory/sessions"));
    }

    #[test]
    fn detect_not_found_without_sessions_dir() {
        let connector = FactoryConnector::new();
        let result = connector.detect();
        let _ = result.detected;
    }

    #[test]
    fn scan_parses_factory_jsonl_format() {
        let dir = TempDir::new().unwrap();
        let factory_dir = dir.path().join(".factory/sessions");
        fs::create_dir_all(&factory_dir).unwrap();

        let session_file = factory_dir.join("test-session.jsonl");
        let content = r#"{"type":"session_start","id":"abc-123","title":"Test Session","owner":"testuser","version":2}
{"type":"message","id":"msg-1","timestamp":"2025-12-01T10:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"Hello Factory"}]}}
{"type":"message","id":"msg-2","timestamp":"2025-12-01T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Hello! How can I help you today?"}]}}
"#;
        fs::write(&session_file, content).unwrap();

        let connector = FactoryConnector::new();
        let ctx = ScanContext::local_default(factory_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].agent_slug, "factory");
        assert_eq!(convs[0].external_id, Some("abc-123".to_string()));
        assert_eq!(convs[0].title, Some("Test Session".to_string()));
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[0].content, "Hello Factory");
        assert_eq!(convs[0].messages[1].role, "assistant");
        assert!(convs[0].messages[1].content.contains("How can I help"));
    }

    #[test]
    fn scan_extracts_workspace_from_directory_name() {
        let dir = TempDir::new().unwrap();
        let workspace_dir = dir.path().join(".factory/sessions/-Users-test-Git-Project");
        fs::create_dir_all(&workspace_dir).unwrap();

        let session_file = workspace_dir.join("session.jsonl");
        let content = r#"{"type":"session_start","id":"xyz","title":"Project Session"}
{"type":"message","id":"msg-1","timestamp":"2025-12-01T10:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"Test message"}]}}
"#;
        fs::write(&session_file, content).unwrap();

        let connector = FactoryConnector::new();
        let ctx = ScanContext::local_default(dir.path().join(".factory/sessions"), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0].workspace,
            Some(PathBuf::from("/Users/test/Git/Project"))
        );
    }

    #[test]
    fn scan_skips_non_message_entries() {
        let dir = TempDir::new().unwrap();
        let factory_dir = dir.path().join(".factory/sessions");
        fs::create_dir_all(&factory_dir).unwrap();

        let session_file = factory_dir.join("session.jsonl");
        let content = r#"{"type":"session_start","id":"abc"}
{"type":"tool_result","id":"tool-1","content":"some result"}
{"type":"message","id":"msg-1","timestamp":"2025-12-01T10:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"Hello"}]}}
{"type":"system","id":"sys-1","content":"system message"}
"#;
        fs::write(&session_file, content).unwrap();

        let connector = FactoryConnector::new();
        let ctx = ScanContext::local_default(factory_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].role, "user");
    }

    #[test]
    fn scan_handles_empty_sessions_directory() {
        let dir = TempDir::new().unwrap();
        let factory_dir = dir.path().join(".factory/sessions");
        fs::create_dir_all(&factory_dir).unwrap();

        let connector = FactoryConnector::new();
        let ctx = ScanContext::local_default(factory_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert!(convs.is_empty());
    }

    #[test]
    fn scan_skips_malformed_json_lines() {
        let dir = TempDir::new().unwrap();
        let factory_dir = dir.path().join(".factory/sessions");
        fs::create_dir_all(&factory_dir).unwrap();

        let session_file = factory_dir.join("session.jsonl");
        let content = r#"{"type":"session_start","id":"abc"}
not valid json
{"type":"message","id":"msg-1","timestamp":"2025-12-01T10:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"Hello"}]}}
"#;
        fs::write(&session_file, content).unwrap();

        let connector = FactoryConnector::new();
        let ctx = ScanContext::local_default(factory_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 1);
    }

    #[test]
    fn scan_uses_first_user_message_for_title_if_no_session_title() {
        let dir = TempDir::new().unwrap();
        let factory_dir = dir.path().join(".factory/sessions");
        fs::create_dir_all(&factory_dir).unwrap();

        let session_file = factory_dir.join("session.jsonl");
        let content = r#"{"type":"session_start","id":"abc"}
{"type":"message","id":"msg-1","timestamp":"2025-12-01T10:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"This is my question about coding"}]}}
"#;
        fs::write(&session_file, content).unwrap();

        let connector = FactoryConnector::new();
        let ctx = ScanContext::local_default(factory_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0].title,
            Some("This is my question about coding".to_string())
        );
    }

    #[test]
    fn scan_skips_messages_with_empty_content() {
        let dir = TempDir::new().unwrap();
        let factory_dir = dir.path().join(".factory/sessions");
        fs::create_dir_all(&factory_dir).unwrap();

        let session_file = factory_dir.join("session.jsonl");
        let content = r#"{"type":"session_start","id":"abc"}
{"type":"message","id":"msg-1","timestamp":"2025-12-01T10:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":""}]}}
{"type":"message","id":"msg-2","timestamp":"2025-12-01T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"Real message"}]}}
"#;
        fs::write(&session_file, content).unwrap();

        let connector = FactoryConnector::new();
        let ctx = ScanContext::local_default(factory_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].content, "Real message");
    }

    #[test]
    fn scan_handles_sessions_without_messages() {
        let dir = TempDir::new().unwrap();
        let factory_dir = dir.path().join(".factory/sessions");
        fs::create_dir_all(&factory_dir).unwrap();

        let session_file = factory_dir.join("session.jsonl");
        let content = r#"{"type":"session_start","id":"abc","title":"Empty Session"}
"#;
        fs::write(&session_file, content).unwrap();

        let connector = FactoryConnector::new();
        let ctx = ScanContext::local_default(factory_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert!(convs.is_empty());
    }
}
