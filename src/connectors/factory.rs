//! Factory Droid connector for JSONL session files.
//!
//! Factory (https://factory.ai) is an AI coding assistant that stores sessions
//! at `~/.factory/sessions/` using a JSONL format similar to Claude Code.
//!
//! Directory structure:
//!   - ~/.factory/sessions/{workspace-path-slug}/{session-uuid}.jsonl
//!   - ~/.factory/sessions/{workspace-path-slug}/{session-uuid}.settings.json
//!
//! The workspace path slug encodes the original working directory path,
//! e.g., `-Users-alice-Dev-myproject` for `/Users/alice/Dev/myproject`.

use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
    file_modified_since, flatten_content, parse_timestamp,
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

    /// Get the Factory sessions directory.
    /// Factory stores sessions in ~/.factory/sessions/
    fn sessions_root() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".factory/sessions"))
    }

    /// Decode a workspace path slug back to a path.
    /// e.g., `-Users-alice-Dev-myproject` -> `/Users/alice/Dev/myproject`
    fn decode_workspace_slug(slug: &str) -> Option<PathBuf> {
        if slug.starts_with('-') {
            // Replace leading dash and internal dashes with path separators
            let path_str = slug.replacen('-', "/", 1).replace('-', "/");
            Some(PathBuf::from(path_str))
        } else {
            None
        }
    }
}

impl Connector for FactoryConnector {
    fn detect(&self) -> DetectionResult {
        if let Some(root) = Self::sessions_root()
            && root.exists()
        {
            return DetectionResult {
                detected: true,
                evidence: vec![format!("found {}", root.display())],
                root_paths: vec![root],
            };
        }
        DetectionResult::not_found()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Determine scan root
        let root = if ctx.use_default_detection() {
            // First check if data_dir looks like factory storage (for testing)
            if looks_like_factory_storage(&ctx.data_dir) && ctx.data_dir.exists() {
                ctx.data_dir.clone()
            } else {
                // Fall back to default sessions root
                match Self::sessions_root() {
                    Some(r) if r.exists() => r,
                    _ => return Ok(Vec::new()),
                }
            }
        } else {
            // Check scan_roots for factory sessions
            let factory_root = ctx.scan_roots.iter().find_map(|sr| {
                let factory_path = sr.path.join(".factory/sessions");
                if factory_path.exists() {
                    Some(factory_path)
                } else if looks_like_factory_storage(&sr.path) {
                    Some(sr.path.clone())
                } else {
                    None
                }
            });
            match factory_root {
                Some(r) => r,
                None => return Ok(Vec::new()),
            }
        };

        if !root.exists() {
            return Ok(Vec::new());
        }

        let mut convs = Vec::new();

        for entry in WalkDir::new(&root).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }

            // Only process .jsonl files (skip .settings.json)
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }

            // Skip files not modified since last scan (incremental indexing)
            if !file_modified_since(path, ctx.since_ts) {
                continue;
            }

            match parse_factory_session(path) {
                Ok(Some(conv)) => convs.push(conv),
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(path = %path.display(), error = %e, "factory parse error");
                }
            }
        }

        Ok(convs)
    }
}

/// Check if a directory looks like Factory storage
fn looks_like_factory_storage(path: &Path) -> bool {
    let path_str = path.to_string_lossy().to_lowercase();
    path_str.contains("factory") && path_str.contains("sessions")
}

/// Parse a Factory session JSONL file into a NormalizedConversation.
fn parse_factory_session(path: &Path) -> Result<Option<NormalizedConversation>> {
    let file =
        fs::File::open(path).with_context(|| format!("open session file {}", path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut messages = Vec::new();
    let mut session_id: Option<String> = None;
    let mut title: Option<String> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut owner: Option<String> = None;
    let mut started_at: Option<i64> = None;
    let mut ended_at: Option<i64> = None;

    // Try to infer workspace from parent directory name if not in session_start
    let parent_dir_name = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());

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

        let entry_type = val.get("type").and_then(|v| v.as_str());

        match entry_type {
            Some("session_start") => {
                // Extract session metadata
                session_id = val.get("id").and_then(|v| v.as_str()).map(String::from);
                title = val.get("title").and_then(|v| v.as_str()).map(String::from);
                owner = val.get("owner").and_then(|v| v.as_str()).map(String::from);
                workspace = val
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from)
                    .or_else(|| {
                        // Fallback: decode workspace from parent directory name
                        parent_dir_name.and_then(FactoryConnector::decode_workspace_slug)
                    });
            }
            Some("message") => {
                // Parse timestamp
                let created = val.get("timestamp").and_then(parse_timestamp);

                // Track session time bounds
                if started_at.is_none() {
                    started_at = created;
                }
                ended_at = created.or(ended_at);

                // Extract role from message.role
                let role = val
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");

                // Extract content from message.content
                let content_val = val.get("message").and_then(|m| m.get("content"));
                let content_str = content_val.map(flatten_content).unwrap_or_default();

                // Skip entries with empty content
                if content_str.trim().is_empty() {
                    continue;
                }

                // Extract model for author field (from message if present)
                let author = val
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(|v| v.as_str())
                    .map(String::from);

                messages.push(NormalizedMessage {
                    idx: 0, // Will be reassigned after collection
                    role: role.to_string(),
                    author,
                    created_at: created,
                    content: content_str,
                    extra: val,
                    snippets: Vec::new(),
                });
            }
            // Skip other types: todo_state, tool_result, etc.
            _ => {}
        }
    }

    // Reassign sequential indices
    for (i, msg) in messages.iter_mut().enumerate() {
        msg.idx = i as i64;
    }

    if messages.is_empty() {
        return Ok(None);
    }

    // Infer workspace from parent directory name if not set by session_start
    if workspace.is_none() {
        workspace = parent_dir_name.and_then(FactoryConnector::decode_workspace_slug);
    }

    // Generate title from first user message if not in session_start
    let final_title = title.or_else(|| {
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
                // Fallback to workspace directory name
                workspace
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(String::from)
            })
    });

    // Load settings file if it exists for additional metadata
    let settings_path = path.with_extension("settings.json");
    let model_info = if settings_path.exists() {
        fs::read_to_string(&settings_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from))
    } else {
        None
    };

    Ok(Some(NormalizedConversation {
        agent_slug: "factory".into(),
        external_id: session_id
            .clone()
            .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(String::from)),
        title: final_title,
        workspace,
        source_path: path.to_path_buf(),
        started_at,
        ended_at,
        metadata: serde_json::json!({
            "source": "factory",
            "sessionId": session_id,
            "owner": owner,
            "model": model_info,
        }),
        messages,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // =========================================================================
    // Constructor tests
    // =========================================================================

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
        if let Some(root) = FactoryConnector::sessions_root() {
            assert!(root.ends_with(".factory/sessions"));
        }
    }

    // =========================================================================
    // Workspace slug decoding tests
    // =========================================================================

    #[test]
    fn decode_workspace_slug_basic() {
        let result = FactoryConnector::decode_workspace_slug("-Users-alice-Dev-myproject");
        assert_eq!(result, Some(PathBuf::from("/Users/alice/Dev/myproject")));
    }

    #[test]
    fn decode_workspace_slug_deep_path() {
        let result = FactoryConnector::decode_workspace_slug("-Users-bob-Dev-sites-example.com");
        assert_eq!(
            result,
            Some(PathBuf::from("/Users/bob/Dev/sites/example.com"))
        );
    }

    #[test]
    fn decode_workspace_slug_no_leading_dash() {
        let result = FactoryConnector::decode_workspace_slug("invalid-path");
        assert_eq!(result, None);
    }

    #[test]
    fn decode_workspace_slug_empty() {
        let result = FactoryConnector::decode_workspace_slug("");
        assert_eq!(result, None);
    }

    // =========================================================================
    // Detection tests
    // =========================================================================

    #[test]
    fn detect_not_found_without_sessions_dir() {
        let connector = FactoryConnector::new();
        let result = connector.detect();
        // Just verify detect() doesn't panic
        let _ = result.detected;
    }

    // =========================================================================
    // JSONL parsing tests
    // =========================================================================

    fn create_factory_storage(dir: &TempDir) -> PathBuf {
        let storage = dir.path().join(".factory").join("sessions");
        fs::create_dir_all(&storage).unwrap();
        storage
    }

    fn write_session_file(storage: &Path, workspace_slug: &str, session_id: &str, lines: &[&str]) {
        let session_dir = storage.join(workspace_slug);
        fs::create_dir_all(&session_dir).unwrap();
        let file_path = session_dir.join(format!("{session_id}.jsonl"));
        fs::write(&file_path, lines.join("\n")).unwrap();
    }

    #[test]
    fn scan_parses_session_start_and_messages() {
        let dir = TempDir::new().unwrap();
        let storage = create_factory_storage(&dir);

        let lines = vec![
            r#"{"type":"session_start","id":"sess-001","title":"Test Session","owner":"testuser","cwd":"/home/user/project"}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"Hello Factory"}}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:05Z","message":{"role":"assistant","content":"Hello! How can I help?"}}"#,
        ];
        write_session_file(&storage, "-home-user-project", "sess-001", &lines);

        let connector = FactoryConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].title, Some("Test Session".to_string()));
        assert_eq!(
            convs[0].workspace,
            Some(PathBuf::from("/home/user/project"))
        );
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[0].content, "Hello Factory");
        assert_eq!(convs[0].messages[1].role, "assistant");
    }

    #[test]
    fn scan_extracts_session_metadata() {
        let dir = TempDir::new().unwrap();
        let storage = create_factory_storage(&dir);

        let lines = vec![
            r#"{"type":"session_start","id":"sess-meta","title":"Metadata Test","owner":"alice","cwd":"/projects/app"}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"Test"}}"#,
        ];
        write_session_file(&storage, "-projects-app", "sess-meta", &lines);

        let connector = FactoryConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].metadata["sessionId"], "sess-meta");
        assert_eq!(convs[0].metadata["owner"], "alice");
        assert_eq!(convs[0].external_id, Some("sess-meta".to_string()));
    }

    #[test]
    fn scan_handles_array_content() {
        let dir = TempDir::new().unwrap();
        let storage = create_factory_storage(&dir);

        let lines = vec![
            r#"{"type":"session_start","id":"sess-arr","cwd":"/test"}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":"First part"},{"type":"tool_use","name":"Read"},{"type":"text","text":"Second part"}]}}"#,
        ];
        write_session_file(&storage, "-test", "sess-arr", &lines);

        let connector = FactoryConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].messages.len(), 1);
        let msg_content = &convs[0].messages[0].content;
        assert!(msg_content.contains("First part"));
        assert!(msg_content.contains("Read"));
    }

    #[test]
    fn scan_infers_workspace_from_directory() {
        let dir = TempDir::new().unwrap();
        let storage = create_factory_storage(&dir);

        // Session without cwd field
        let lines = vec![
            r#"{"type":"session_start","id":"sess-no-cwd"}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"Test"}}"#,
        ];
        write_session_file(&storage, "-Users-test-myproject", "sess-no-cwd", &lines);

        let connector = FactoryConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(
            convs[0].workspace,
            Some(PathBuf::from("/Users/test/myproject"))
        );
    }

    #[test]
    fn scan_empty_messages_returns_none() {
        let dir = TempDir::new().unwrap();
        let storage = create_factory_storage(&dir);

        let lines = vec![r#"{"type":"session_start","id":"sess-empty","cwd":"/test"}"#];
        write_session_file(&storage, "-test", "sess-empty", &lines);

        let connector = FactoryConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert!(convs.is_empty());
    }

    #[test]
    fn scan_reads_model_from_settings() {
        let dir = TempDir::new().unwrap();
        let storage = create_factory_storage(&dir);

        let lines = vec![
            r#"{"type":"session_start","id":"sess-model","cwd":"/test"}"#,
            r#"{"type":"message","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"Hello"}}"#,
        ];
        write_session_file(&storage, "-test", "sess-model", &lines);

        // Write settings file
        let settings_path = storage.join("-test").join("sess-model.settings.json");
        fs::write(&settings_path, r#"{"model":"claude-opus-4-5-20251101"}"#).unwrap();

        let connector = FactoryConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].metadata["model"], "claude-opus-4-5-20251101");
    }
}
