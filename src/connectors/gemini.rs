use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
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

    for msg in messages {
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

    // Find the end of the path (whitespace, newline, or certain punctuation)
    let end = rest
        .find(|c: char| {
            c.is_whitespace()
                || c == '\n'
                || c == '>'
                || c == '"'
                || c == '\''
                || c == ')'
                || c == ']'
                || c == ','
        })
        .unwrap_or(rest.len());

    let path_str = rest.get(..end)?.trim_end_matches(['/', ':', ']', ')']);

    // Check for Unix absolute path OR Windows absolute path (C:\ or \\)
    let is_unix_abs = path_str.starts_with('/');
    let is_win_drive = path_str.len() >= 3
        && path_str
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic())
            .unwrap_or(false)
        && path_str.chars().nth(1) == Some(':')
        && (path_str.chars().nth(2) == Some('\\') || path_str.chars().nth(2) == Some('/'));
    let is_win_unc = path_str.starts_with("\\\\");

    if !is_unix_abs && !is_win_drive && !is_win_unc {
        return None;
    }

    if path_str.len() <= 3 {
        return None; // Too short to be a useful workspace path
    }

    let path = PathBuf::from(path_str);

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
        std::env::var("GEMINI_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".gemini/tmp"))
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
            }
        } else {
            DetectionResult::not_found()
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Use data_root only if it looks like a Gemini directory (for testing)
        // Otherwise use the default root
        let root = if ctx
            .data_root
            .file_name()
            .map(|n| n.to_str().unwrap_or("").contains("gemini"))
            .unwrap_or(false)
            || ctx.data_root.join("chats").exists()
            || fs::read_dir(&ctx.data_root)
                .map(|mut d| {
                    d.any(|e| {
                        e.ok()
                            .map(|e| e.path().join("chats").exists())
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        {
            ctx.data_root.clone()
        } else {
            Self::root()
        };

        if !root.exists() {
            return Ok(Vec::new());
        }

        let files = Self::session_files(&root);
        let mut convs = Vec::new();

        for file in files {
            let content = fs::read_to_string(&file)
                .with_context(|| format!("read session {}", file.display()))?;

            let val: Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => continue,
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

            for (idx, item) in messages_arr.iter().enumerate() {
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

                if let (Some(since), Some(ts)) = (ctx.since_ts, created)
                    && ts <= since
                {
                    continue;
                }

                started_at = started_at.or(created);
                ended_at = created.or(ended_at);

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
                    idx: idx as i64,
                    role: role.to_string(),
                    author: None,
                    created_at: created,
                    content: content_str,
                    extra: item.clone(),
                    snippets: Vec::new(),
                });
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

            // Try to extract actual workspace from message content first
            // Gemini stores by hash, but messages often contain the real project path
            let workspace = extract_workspace_from_content(&messages).or_else(|| {
                // Fallback to parent directory structure
                // Structure: ~/.gemini/tmp/<hash>/chats/session-*.json
                file.parent() // chats/
                    .and_then(|p| p.parent()) // <hash>/
                    .map(|p| p.to_path_buf())
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

    #[test]
    fn extract_path_handles_windows_and_unix() {
        // Unix absolute
        let content = "Working directory: /home/user/project";
        let path = extract_path_from_position(content, 19);
        assert_eq!(path, Some(PathBuf::from("/home/user/project")));

        // Unix absolute with trailing slash
        let content = "Working directory: /data/projects/foo/";
        let path = extract_path_from_position(content, 19);
        assert_eq!(path, Some(PathBuf::from("/data/projects/foo")));

        // Windows drive letter
        let content = r"Working directory: C:\Users\User\Project";
        let path = extract_path_from_position(content, 19);
        assert_eq!(path, Some(PathBuf::from(r"C:\Users\User\Project")));

        // Windows drive with forward slashes (mixed)
        let content = "Working directory: D:/Code/Rust";
        let path = extract_path_from_position(content, 19);
        assert_eq!(path, Some(PathBuf::from("D:/Code/Rust")));

        // Windows UNC
        let content = r"Working directory: \\Server\Share\Project";
        let path = extract_path_from_position(content, 19);
        assert_eq!(path, Some(PathBuf::from(r"\\Server\Share\Project")));

        // Invalid/Relative
        let content = "Working directory: relative/path";
        let path = extract_path_from_position(content, 19);
        assert_eq!(path, None);

        // Too short
        let content = "Working directory: /a";
        let path = extract_path_from_position(content, 19);
        assert_eq!(path, None);
    }
}
