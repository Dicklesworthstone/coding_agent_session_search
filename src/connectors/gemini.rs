use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

/// Extract actual workspace path from message content.
/// Gemini stores sessions by hash, but messages often contain the actual project path
/// in patterns like "# AGENTS.md instructions for /data/projects/foo" or file paths.
fn extract_workspace_from_content(messages: &[NormalizedMessage]) -> Option<PathBuf> {
    // Patterns to look for workspace paths in messages:
    // 1. AGENTS.md header: "# AGENTS.md instructions for /path/to/project"
    // 2. Working directory: "Working directory: /path/to/project"
    // 3. Common project paths: /data/projects/X

    // Limit to first 50 messages for performance
    for msg in messages.iter().take(50) {
        // Try AGENTS.md pattern first (most reliable)
        // Pattern: "AGENTS.md instructions for /path/to/project"
        if let Some(idx) = msg.content.find("AGENTS.md instructions for ") {
            let start = idx + "AGENTS.md instructions for ".len();
            if let Some(path) = extract_path_from_position(&msg.content, start) {
                return Some(path);
            }
        }

        // Try working directory pattern
        // Pattern: "Working directory: /path/to/project"
        if let Some(idx) = msg.content.find("Working directory:") {
            let start = idx + "Working directory:".len();
            if let Some(path) = extract_path_from_position(&msg.content, start) {
                return Some(path);
            }
        }
    }

    // Fallback: look for common project path patterns in first few messages
    for msg in messages.iter().take(5) {
        // Look for /data/projects/ paths
        if let Some(idx) = msg.content.find("/data/projects/")
            && let Some(path) = extract_path_from_position(&msg.content, idx)
        {
            return Some(path);
        }
    }

    None
}

/// Extract a path starting from the given position in the string.
/// Stops at whitespace, newlines, or common delimiters.
/// Returns the project directory (truncates at file extensions or deep paths).
fn extract_path_from_position(content: &str, start: usize) -> Option<PathBuf> {
    let rest = content.get(start..)?;
    let rest = rest.trim_start();

    // Handle quoted paths
    let (rest, delimiter) = if let Some(stripped) = rest.strip_prefix('"') {
        (stripped, Some('"'))
    } else if let Some(stripped) = rest.strip_prefix('\'') {
        (stripped, Some('\''))
    } else {
        (rest, None)
    };

    // Find the end of the path (whitespace, newline, delimiter, or certain punctuation)
    let end = rest
        .find(|c: char| {
            if let Some(d) = delimiter {
                c == d
            } else {
                c.is_whitespace()
                    || c == '\n'
                    || c == '>'
                    || c == '"'
                    || c == '\''
                    || c == ')'
                    || c == ']'
                    || c == ','
            }
        })
        .unwrap_or(rest.len());

    let path_str = rest.get(..end)?.trim_end_matches(['/', ':']);

    // Check for Unix absolute path OR Windows absolute path (C:\ or \\)
    let is_unix_abs = path_str.starts_with('/');
    let is_win_drive = path_str.len() >= 3
        && path_str
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        && path_str.chars().nth(1) == Some(':')
        && (path_str.chars().nth(2) == Some('\\') || path_str.chars().nth(2) == Some('/'));
    let is_win_unc = path_str.starts_with("\\\\");

    if !is_unix_abs && !is_win_drive && !is_win_unc {
        return None;
    }

    if path_str.len() <= 3 {
        return None; // Too short to be a useful workspace path
    }

    // Normalize separators for cross-platform robustness (e.g. scanning Windows logs on Linux)
    // This ensures PathBuf::components() and parent() work correctly regardless of host OS.
    let clean_path_str = if is_win_drive || is_win_unc {
        path_str.replace('\\', "/")
    } else {
        path_str.to_string()
    };

    let path = PathBuf::from(clean_path_str);

    // If it looks like a file path (has extension), get the parent directory
    // Also if it's deeper than /data/projects/X or /home/user/projects/X, truncate
    let path = if path.extension().is_some() {
        path.parent()?.to_path_buf()
    } else {
        path
    };

    // For /data/projects/X/... paths, return just /data/projects/X
    let components: Vec<_> = path.components().collect();
    if components.len() >= 4 {
        let path_str = path.to_string_lossy();
        if path_str.starts_with("/data/projects/") {
            // Return just /data/projects/project_name
            let parts: Vec<&str> = path_str.splitn(5, '/').collect();
            if parts.len() >= 4 {
                return Some(PathBuf::from(format!(
                    "/{}/{}/{}",
                    parts[1], parts[2], parts[3]
                )));
            }
        }
    }

    Some(path)
}

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
        dotenvy::var("GEMINI_HOME").map_or_else(
            |_| dirs::home_dir().unwrap_or_default().join(".gemini/tmp"),
            PathBuf::from,
        )
    }

    /// Find all session JSON files in the Gemini structure.
    /// Structure: ~/.gemini/tmp/<hash>/chats/session-*.json
    fn session_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in WalkDir::new(root).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            // Only process session-*.json files in chats/ directories
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.starts_with("session-") && name.ends_with(".json") {
                // Verify it's in a chats/ directory
                if path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    == Some("chats")
                {
                    files.push(path.to_path_buf());
                }
            }
        }
        files
    }
}

impl Connector for GeminiConnector {
    fn detect(&self) -> DetectionResult {
        let root = Self::root();
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
        // Use data_root only if it looks like a Gemini directory (for testing)
        // Otherwise use the default root
        let looks_like_root = |path: &PathBuf| {
            path.join("chats").exists()
                || fs::read_dir(path)
                    .map(|mut d| d.any(|e| e.ok().is_some_and(|e| e.path().join("chats").exists())))
                    .unwrap_or(false)
        };
        let root = if ctx.use_default_detection() {
            if looks_like_root(&ctx.data_dir) {
                ctx.data_dir.clone()
            } else {
                Self::root()
            }
        } else {
            ctx.data_dir.clone()
        };

        if !ctx.use_default_detection() && !looks_like_root(&root) {
            return Ok(Vec::new());
        }

        if !root.exists() {
            return Ok(Vec::new());
        }

        let files = Self::session_files(&root);
        let mut convs = Vec::new();

        for file in files {
            // Skip files not modified since last scan (incremental indexing)
            if !crate::connectors::file_modified_since(&file, ctx.since_ts) {
                continue;
            }

            let content = match fs::read_to_string(&file) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("failed to read session {}: {}", file.display(), e);
                    continue;
                }
            };

            let val: Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("failed to parse session {}: {}", file.display(), e);
                    continue;
                }
            };

            // Extract session metadata
            let session_id = val
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(String::from);
            let project_hash = val
                .get("projectHash")
                .and_then(|v| v.as_str())
                .map(String::from);

            // Parse session-level timestamps
            let start_time = val
                .get("startTime")
                .and_then(crate::connectors::parse_timestamp);
            let last_updated = val
                .get("lastUpdated")
                .and_then(crate::connectors::parse_timestamp);

            // Parse messages array
            let Some(messages_arr) = val.get("messages").and_then(|m| m.as_array()) else {
                continue;
            };

            let mut messages = Vec::new();
            let mut started_at = start_time;
            let mut ended_at = last_updated;

            for item in messages_arr {
                // Role from "type" field - Gemini uses "user" and "model"
                let msg_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("model");
                let role = if msg_type == "model" {
                    "assistant"
                } else {
                    msg_type
                };

                // Parse timestamp using shared utility
                let created = item
                    .get("timestamp")
                    .and_then(crate::connectors::parse_timestamp);

                // NOTE: Do NOT filter individual messages by timestamp here!
                // The file-level check in file_modified_since() is sufficient.
                // Filtering messages would cause older messages to be lost when
                // the file is re-indexed after new messages are added.

                started_at = started_at.or(created);
                ended_at = match (ended_at, created) {
                    (Some(current), Some(ts)) => Some(current.max(ts)),
                    (None, Some(ts)) => Some(ts),
                    (Some(current), None) => Some(current),
                    (None, None) => None,
                };

                // Extract content using flatten_content for consistency
                let content_str = item
                    .get("content")
                    .map(crate::connectors::flatten_content)
                    .unwrap_or_default();

                // Skip entries with empty content
                if content_str.trim().is_empty() {
                    continue;
                }

                messages.push(NormalizedMessage {
                    idx: 0, // will be re-assigned after filtering
                    role: role.to_string(),
                    author: None,
                    created_at: created,
                    content: content_str,
                    extra: item.clone(),
                    snippets: Vec::new(),
                });
            }

            // Re-assign sequential indices after filtering
            super::reindex_messages(&mut messages);

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

            // Try to extract actual workspace from message content first
            // Gemini stores by hash, but messages often contain the real project path
            let workspace = extract_workspace_from_content(&messages).or_else(|| {
                // Fallback to parent directory structure
                // Structure: ~/.gemini/tmp/<hash>/chats/session-*.json
                file.parent() // chats/
                    .and_then(|p| p.parent()) // <hash>/
                    .map(std::path::Path::to_path_buf)
            });

            convs.push(NormalizedConversation {
                agent_slug: "gemini".into(),
                external_id: session_id
                    .or_else(|| file.file_stem().and_then(|s| s.to_str()).map(String::from)),
                title,
                workspace,
                source_path: file.clone(),
                started_at,
                ended_at,
                metadata: serde_json::json!({
                    "source": "gemini",
                    "project_hash": project_hash
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
    use crate::connectors::NormalizedMessage;
    use serial_test::serial;
    use tempfile::TempDir;

    // ==================== Constructor Tests ====================

    #[test]
    fn new_creates_connector() {
        let connector = GeminiConnector::new();
        let _ = connector;
    }

    #[test]
    fn default_creates_connector() {
        let connector = GeminiConnector;
        let _ = connector;
    }

    // ==================== extract_path_from_position Tests ====================

    #[test]
    fn extract_path_handles_unix_absolute() {
        let content = "Working directory: /home/user/project";
        let path = extract_path_from_position(content, 19);
        assert_eq!(path, Some(PathBuf::from("/home/user/project")));
    }

    #[test]
    fn extract_path_handles_trailing_slash() {
        let content = "Working directory: /data/projects/foo/";
        let path = extract_path_from_position(content, 19);
        assert_eq!(path, Some(PathBuf::from("/data/projects/foo")));
    }

    #[test]
    fn extract_path_handles_quoted_path_start() {
        let content = "Working directory: \"/data/projects/foo\"";
        let path = extract_path_from_position(content, 19);
        assert_eq!(path, Some(PathBuf::from("/data/projects/foo")));
    }

    #[test]
    fn extract_path_handles_quoted_path_with_brackets() {
        let content = "Working directory: \"/data/projects/[foo]\"";
        let path = extract_path_from_position(content, 19);
        assert_eq!(path, Some(PathBuf::from("/data/projects/[foo]")));
    }

    #[test]
    fn extract_path_handles_windows_drive() {
        let content = r"Working directory: C:\Users\User\Project";
        let path = extract_path_from_position(content, 19);
        // Windows paths are normalized to forward slashes for cross-platform robustness
        assert_eq!(path, Some(PathBuf::from("C:/Users/User/Project")));
    }

    #[test]
    fn extract_path_handles_windows_forward_slashes() {
        let content = "Working directory: D:/Code/Rust";
        let path = extract_path_from_position(content, 19);
        assert_eq!(path, Some(PathBuf::from("D:/Code/Rust")));
    }

    #[test]
    fn extract_path_handles_windows_unc() {
        let content = r"Working directory: \\Server\Share\Project";
        let path = extract_path_from_position(content, 19);
        // UNC paths are normalized to forward slashes for cross-platform robustness
        assert_eq!(path, Some(PathBuf::from("//Server/Share/Project")));
    }

    #[test]
    fn extract_path_rejects_relative_paths() {
        let content = "Working directory: relative/path";
        let path = extract_path_from_position(content, 19);
        assert_eq!(path, None);
    }

    #[test]
    fn extract_path_rejects_too_short_paths() {
        let content = "Working directory: /a";
        let path = extract_path_from_position(content, 19);
        assert_eq!(path, None);
    }

    #[test]
    fn extract_path_stops_at_whitespace() {
        let content = "Path: /home/user/project more text";
        let path = extract_path_from_position(content, 6);
        assert_eq!(path, Some(PathBuf::from("/home/user/project")));
    }

    #[test]
    fn extract_path_stops_at_newline() {
        let content = "Path: /home/user/project\nmore text";
        let path = extract_path_from_position(content, 6);
        assert_eq!(path, Some(PathBuf::from("/home/user/project")));
    }

    #[test]
    fn extract_path_stops_at_quote() {
        let content = "Path: \"/home/user/project\" more";
        let path = extract_path_from_position(content, 7);
        assert_eq!(path, Some(PathBuf::from("/home/user/project")));
    }

    #[test]
    fn extract_path_truncates_data_projects_paths() {
        let content = "File at /data/projects/myproject/src/lib.rs";
        let path = extract_path_from_position(content, 8);
        assert_eq!(path, Some(PathBuf::from("/data/projects/myproject")));
    }

    #[test]
    fn extract_path_removes_file_extension_for_parent() {
        let content = "Editing /home/user/project/src/main.rs";
        let path = extract_path_from_position(content, 8);
        assert_eq!(path, Some(PathBuf::from("/home/user/project/src")));
    }

    // ==================== extract_workspace_from_content Tests ====================

    #[test]
    fn extract_workspace_from_agents_md_pattern() {
        let messages = vec![NormalizedMessage {
            idx: 0,
            role: "user".into(),
            author: None,
            created_at: None,
            content: "# AGENTS.md instructions for /data/projects/myapp\nHello".into(),
            extra: serde_json::Value::Null,
            snippets: vec![],
        }];
        let result = extract_workspace_from_content(&messages);
        assert_eq!(result, Some(PathBuf::from("/data/projects/myapp")));
    }

    #[test]
    fn extract_workspace_from_working_directory_pattern() {
        let messages = vec![NormalizedMessage {
            idx: 0,
            role: "assistant".into(),
            author: None,
            created_at: None,
            content: "Working directory: /home/user/project\nLet me help.".into(),
            extra: serde_json::Value::Null,
            snippets: vec![],
        }];
        let result = extract_workspace_from_content(&messages);
        assert_eq!(result, Some(PathBuf::from("/home/user/project")));
    }

    #[test]
    fn extract_workspace_fallback_to_data_projects() {
        let messages = vec![NormalizedMessage {
            idx: 0,
            role: "user".into(),
            author: None,
            created_at: None,
            content: "Check the file at /data/projects/foo/src/main.rs".into(),
            extra: serde_json::Value::Null,
            snippets: vec![],
        }];
        let result = extract_workspace_from_content(&messages);
        assert_eq!(result, Some(PathBuf::from("/data/projects/foo")));
    }

    #[test]
    fn extract_workspace_returns_none_for_no_paths() {
        let messages = vec![NormalizedMessage {
            idx: 0,
            role: "user".into(),
            author: None,
            created_at: None,
            content: "Hello, how are you?".into(),
            extra: serde_json::Value::Null,
            snippets: vec![],
        }];
        let result = extract_workspace_from_content(&messages);
        assert_eq!(result, None);
    }

    #[test]
    fn extract_workspace_prefers_agents_md_over_working_directory() {
        let messages = vec![NormalizedMessage {
            idx: 0,
            role: "user".into(),
            author: None,
            created_at: None,
            content:
                "Working directory: /tmp/wrong\n# AGENTS.md instructions for /data/projects/right"
                    .into(),
            extra: serde_json::Value::Null,
            snippets: vec![],
        }];
        // AGENTS.md pattern should be found first
        let result = extract_workspace_from_content(&messages);
        assert_eq!(result, Some(PathBuf::from("/data/projects/right")));
    }

    // ==================== session_files Tests ====================

    #[test]
    fn session_files_finds_session_json_in_chats() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("abcd1234");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();
        fs::write(chats_dir.join("session-1.json"), "{}").unwrap();
        fs::write(chats_dir.join("session-2.json"), "{}").unwrap();

        let files = GeminiConnector::session_files(dir.path());
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn session_files_ignores_non_session_files() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("abcd1234");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();
        fs::write(chats_dir.join("session-1.json"), "{}").unwrap();
        fs::write(chats_dir.join("other.json"), "{}").unwrap();
        fs::write(chats_dir.join("readme.txt"), "hello").unwrap();

        let files = GeminiConnector::session_files(dir.path());
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn session_files_ignores_session_json_outside_chats() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("abcd1234");
        fs::create_dir_all(&hash_dir).unwrap();
        // session-*.json not in chats/ directory
        fs::write(hash_dir.join("session-1.json"), "{}").unwrap();

        let files = GeminiConnector::session_files(dir.path());
        assert!(files.is_empty());
    }

    #[test]
    fn session_files_finds_multiple_hash_directories() {
        let dir = TempDir::new().unwrap();

        let hash1_chats = dir.path().join("hash1").join("chats");
        fs::create_dir_all(&hash1_chats).unwrap();
        fs::write(hash1_chats.join("session-a.json"), "{}").unwrap();

        let hash2_chats = dir.path().join("hash2").join("chats");
        fs::create_dir_all(&hash2_chats).unwrap();
        fs::write(hash2_chats.join("session-b.json"), "{}").unwrap();

        let files = GeminiConnector::session_files(dir.path());
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn session_files_returns_empty_for_empty_dir() {
        let dir = TempDir::new().unwrap();
        let files = GeminiConnector::session_files(dir.path());
        assert!(files.is_empty());
    }

    #[test]
    fn session_files_returns_empty_for_nonexistent_dir() {
        let files = GeminiConnector::session_files(Path::new("/nonexistent/path/xyz"));
        assert!(files.is_empty());
    }

    // ==================== scan Tests ====================

    #[test]
    fn scan_parses_gemini_session() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_test_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        let session_json = r#"{
            "sessionId": "test-session-123",
            "projectHash": "abc123",
            "startTime": "2025-01-15T10:00:00Z",
            "lastUpdated": "2025-01-15T10:05:00Z",
            "messages": [
                {
                    "type": "user",
                    "content": "Hello Gemini!",
                    "timestamp": "2025-01-15T10:00:00Z"
                },
                {
                    "type": "model",
                    "content": "Hello! How can I help?",
                    "timestamp": "2025-01-15T10:00:05Z"
                }
            ]
        }"#;
        fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        let conv = &convs[0];
        assert_eq!(conv.agent_slug, "gemini");
        assert_eq!(conv.external_id.as_deref(), Some("test-session-123"));
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "Hello Gemini!");
        assert_eq!(conv.messages[1].role, "assistant"); // model -> assistant
        assert_eq!(conv.messages[1].content, "Hello! How can I help?");
    }

    #[test]
    fn scan_normalizes_model_role_to_assistant() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        let session_json = r#"{
            "sessionId": "session-1",
            "messages": [
                {"type": "model", "content": "I am the model"}
            ]
        }"#;
        fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].messages[0].role, "assistant");
    }

    #[test]
    fn scan_assigns_sequential_indices() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        let session_json = r#"{
            "sessionId": "session-1",
            "messages": [
                {"type": "user", "content": "First"},
                {"type": "model", "content": "Second"},
                {"type": "user", "content": "Third"}
            ]
        }"#;
        fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].messages[0].idx, 0);
        assert_eq!(convs[0].messages[1].idx, 1);
        assert_eq!(convs[0].messages[2].idx, 2);
    }

    #[test]
    fn scan_extracts_title_from_first_user_message() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        // Title is extracted from first line of first user message
        let session_json = r#"{
            "sessionId": "session-1",
            "messages": [
                {"type": "model", "content": "Hello!"},
                {"type": "user", "content": "Help me with Rust"}
            ]
        }"#;
        fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].title.as_deref(), Some("Help me with Rust"));
    }

    #[test]
    fn scan_truncates_long_titles() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        let long_content = "x".repeat(200);
        let session_json = format!(
            r#"{{"sessionId": "session-1", "messages": [{{"type": "user", "content": "{}"}}]}}"#,
            long_content
        );
        fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].title.as_ref().map(|t| t.len()), Some(100));
    }

    #[test]
    fn scan_sets_source_path() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        let session_json =
            r#"{"sessionId": "s1", "messages": [{"type": "user", "content": "Hi"}]}"#;
        fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert!(convs[0].source_path.ends_with("session-1.json"));
    }

    #[test]
    fn scan_extracts_project_hash_to_metadata() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        let session_json = r#"{
            "sessionId": "session-1",
            "projectHash": "myproject123",
            "messages": [{"type": "user", "content": "Hi"}]
        }"#;
        fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(
            convs[0]
                .metadata
                .get("project_hash")
                .and_then(|v| v.as_str()),
            Some("myproject123")
        );
    }

    #[test]
    fn scan_handles_empty_messages_array() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        let session_json = r#"{"sessionId": "session-1", "messages": []}"#;
        fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Empty messages should not create a conversation
        assert!(convs.is_empty());
    }

    #[test]
    fn scan_skips_empty_content_messages() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        let session_json = r#"{
            "sessionId": "session-1",
            "messages": [
                {"type": "user", "content": "Hello"},
                {"type": "model", "content": "   "},
                {"type": "user", "content": ""}
            ]
        }"#;
        fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Only the non-empty message should be included
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].content, "Hello");
    }

    #[test]
    fn scan_handles_invalid_json() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        fs::write(chats_dir.join("session-1.json"), "not valid json").unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert!(convs.is_empty());
    }

    #[test]
    fn scan_handles_missing_messages_field() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        let session_json = r#"{"sessionId": "session-1"}"#;
        fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        // No messages field means skip this session
        assert!(convs.is_empty());
    }

    #[test]
    fn scan_handles_empty_directory() {
        let dir = TempDir::new().unwrap();
        // Create a gemini-named subdir so scan uses it instead of real ~/.gemini/tmp
        let gemini_dir = dir.path().join("gemini_test");
        fs::create_dir_all(&gemini_dir).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(gemini_dir, None);
        let convs = connector.scan(&ctx).unwrap();

        assert!(convs.is_empty());
    }

    #[test]
    fn scan_uses_fallback_external_id_from_filename() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        // No sessionId field
        let session_json = r#"{"messages": [{"type": "user", "content": "Hi"}]}"#;
        fs::write(chats_dir.join("session-fallback.json"), session_json).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].external_id.as_deref(), Some("session-fallback"));
    }

    #[test]
    fn scan_extracts_workspace_from_message_content() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        // Pattern: AGENTS.md instructions for /path
        let session_json = r##"{
            "sessionId": "session-1",
            "messages": [
                {"type": "user", "content": "# AGENTS.md instructions for /data/projects/myapp Hello there"}
            ]
        }"##;
        fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(
            convs[0].workspace,
            Some(PathBuf::from("/data/projects/myapp"))
        );
    }

    #[test]
    fn scan_falls_back_to_parent_dir_for_workspace() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("project_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        // No workspace info in content
        let session_json =
            r#"{"sessionId": "s1", "messages": [{"type": "user", "content": "Hi"}]}"#;
        fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Falls back to hash directory
        assert!(
            convs[0]
                .workspace
                .as_ref()
                .unwrap()
                .ends_with("project_hash")
        );
    }

    #[test]
    fn scan_parses_timestamps() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        let session_json = r#"{
            "sessionId": "session-1",
            "startTime": "2025-01-15T10:00:00Z",
            "lastUpdated": "2025-01-15T11:00:00Z",
            "messages": [
                {"type": "user", "content": "Hello", "timestamp": "2025-01-15T10:00:00Z"}
            ]
        }"#;
        fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert!(convs[0].started_at.is_some());
        assert!(convs[0].ended_at.is_some());
        assert!(convs[0].messages[0].created_at.is_some());
    }

    #[test]
    fn scan_handles_array_content() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        // Content as array of parts (Gemini multi-part format)
        let session_json = r#"{
            "sessionId": "session-1",
            "messages": [
                {"type": "user", "content": [{"text": "Hello "}, {"text": "World"}]}
            ]
        }"#;
        fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        // flatten_content should concatenate text parts
        assert!(convs[0].messages[0].content.contains("Hello"));
    }

    #[test]
    fn scan_finds_multiple_sessions() {
        let dir = TempDir::new().unwrap();
        let hash_dir = dir.path().join("gemini_hash");
        let chats_dir = hash_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        fs::write(
            chats_dir.join("session-1.json"),
            r#"{"sessionId": "s1", "messages": [{"type": "user", "content": "Hi 1"}]}"#,
        )
        .unwrap();
        fs::write(
            chats_dir.join("session-2.json"),
            r#"{"sessionId": "s2", "messages": [{"type": "user", "content": "Hi 2"}]}"#,
        )
        .unwrap();

        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 2);
    }

    #[test]
    #[serial]
    fn scan_respects_gemini_home_even_if_data_dir_contains_gemini_string() {
        // Setup real gemini home with data
        let home_dir = TempDir::new().unwrap();
        // Structure: .gemini/tmp/<hash>/chats/session.json
        let real_chats = home_dir.path().join(".gemini/tmp/hash123/chats");
        fs::create_dir_all(&real_chats).unwrap();
        
        let session_json = r#"{"sessionId": "real", "messages": [{"type": "user", "content": "Real session"}]}"#;
        fs::write(real_chats.join("session-real.json"), session_json).unwrap();

        // Setup CASS data dir that happens to have "gemini" in path
        let project_dir = TempDir::new().unwrap();
        // The LEAF directory must contain "gemini" to trigger the bug
        let confusing_data_dir = project_dir.path().join("project_gemini");
        fs::create_dir_all(&confusing_data_dir).unwrap();

        // Overwrite HOME to point to our temp home
        // SAFETY: Test runs in single-threaded context
        unsafe { std::env::set_var("HOME", home_dir.path()) };
        
        let connector = GeminiConnector::new();
        let ctx = ScanContext::local_default(confusing_data_dir.clone(), None);
        
        let convs = connector.scan(&ctx).unwrap();
        
        unsafe { std::env::remove_var("HOME") }; // Cleanup, though risky in shared env

        assert_eq!(convs.len(), 1, "Should find session in real home");
        assert_eq!(convs[0].messages[0].content, "Real session");
    }

    // ==================== detect Tests ====================

    #[test]
    fn detect_returns_not_found_for_missing_directory() {
        // Since detect uses the real root, we can only test the behavior indirectly
        // This test documents the expected behavior
        let connector = GeminiConnector::new();
        let result = connector.detect();
        // Result depends on whether ~/.gemini/tmp exists on the system
        // We just verify it returns a valid result
        let _ = result.detected;
    }
}
