//! OpenCode connector for JSON file-based storage.
//!
//! OpenCode stores data at `~/.local/share/opencode/storage/` using a hierarchical
//! JSON file structure:
//!   - session/{projectID}/{sessionID}.json  - Session metadata
//!   - message/{sessionID}/{messageID}.json  - Message metadata
//!   - part/{messageID}/{partID}.json        - Actual message content

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
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

    /// Get the OpenCode storage directory.
    /// OpenCode stores sessions in ~/.local/share/opencode/storage/
    fn storage_root() -> Option<PathBuf> {
        // Check for env override first (useful for testing)
        if let Ok(path) = dotenvy::var("OPENCODE_STORAGE_ROOT") {
            let p = PathBuf::from(path);
            if p.exists() {
                return Some(p);
            }
        }

        // Primary location: XDG data directory (Linux/macOS)
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

// ============================================================================
// JSON Structures for OpenCode Storage
// ============================================================================

/// Session info from session/{projectID}/{sessionID}.json
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

/// Message info from message/{sessionID}/{messageID}.json
#[derive(Debug, Deserialize)]
struct MessageInfo {
    id: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    time: Option<MessageTime>,
    #[serde(rename = "modelID", default)]
    model_id: Option<String>,
    #[serde(rename = "sessionID", default)]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageTime {
    #[serde(default)]
    created: Option<i64>,
    #[serde(default)]
    #[allow(dead_code)]
    completed: Option<i64>,
}

/// Part info from part/{messageID}/{partID}.json
#[derive(Debug, Clone, Deserialize)]
struct PartInfo {
    #[serde(default)]
    #[allow(dead_code)]
    id: Option<String>,
    #[serde(rename = "messageID", default)]
    message_id: Option<String>,
    #[serde(rename = "type", default)]
    part_type: Option<String>,
    #[serde(default)]
    text: Option<String>,
    // Tool state for tool parts
    #[serde(default)]
    state: Option<ToolState>,
}

#[derive(Debug, Clone, Deserialize)]
struct ToolState {
    #[serde(default)]
    output: Option<String>,
}

impl Connector for OpenCodeConnector {
    fn detect(&self) -> DetectionResult {
        if let Some(storage) = Self::storage_root() {
            DetectionResult {
                detected: true,
                evidence: vec![format!("found {}", storage.display())],
                root_paths: vec![storage],
            }
        } else {
            DetectionResult::not_found()
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Determine the storage root
        let storage_root = if ctx.use_default_detection() {
            if ctx.data_dir.exists() && looks_like_opencode_storage(&ctx.data_dir) {
                ctx.data_dir.clone()
            } else {
                match Self::storage_root() {
                    Some(root) => root,
                    None => return Ok(Vec::new()),
                }
            }
        } else if ctx.data_dir.exists() && looks_like_opencode_storage(&ctx.data_dir) {
            ctx.data_dir.clone()
        } else {
            return Ok(Vec::new());
        };

        let session_dir = storage_root.join("session");
        let message_dir = storage_root.join("message");
        let part_dir = storage_root.join("part");

        if !session_dir.exists() {
            return Ok(Vec::new());
        }

        // Collect all session files
        let session_files: Vec<PathBuf> = WalkDir::new(&session_dir)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "json")
                    .unwrap_or(false)
            })
            .map(|e| e.path().to_path_buf())
            .collect();

        let mut convs = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        for session_file in session_files {
            // Skip files not modified since last scan
            if !crate::connectors::file_modified_since(&session_file, ctx.since_ts) {
                continue;
            }

            // Parse session
            let session = match parse_session_file(&session_file) {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(
                        "opencode: failed to parse session {}: {e}",
                        session_file.display()
                    );
                    continue;
                }
            };

            // Deduplicate by session ID
            if !seen_ids.insert(session.id.clone()) {
                continue;
            }

            // Load messages for this session
            let session_msg_dir = message_dir.join(&session.id);
            let messages = if session_msg_dir.exists() {
                load_messages(&session_msg_dir, &part_dir)?
            } else {
                Vec::new()
            };

            if messages.is_empty() {
                continue;
            }

            // Build normalized conversation
            let started_at = session
                .time
                .as_ref()
                .and_then(|t| t.created)
                .or_else(|| messages.first().and_then(|m| m.created_at));
            let ended_at = session
                .time
                .as_ref()
                .and_then(|t| t.updated)
                .or_else(|| messages.last().and_then(|m| m.created_at));

            let workspace = session.directory.map(PathBuf::from);
            let title = session.title.or_else(|| {
                messages
                    .first()
                    .and_then(|m| m.content.lines().next())
                    .map(|s| s.chars().take(100).collect())
            });

            convs.push(NormalizedConversation {
                agent_slug: "opencode".into(),
                external_id: Some(session.id.clone()),
                title,
                workspace,
                source_path: session_file.clone(),
                started_at,
                ended_at,
                metadata: serde_json::json!({
                    "session_id": session.id,
                    "project_id": session.project_id,
                }),
                messages,
            });
        }

        Ok(convs)
    }
}

/// Check if a directory looks like OpenCode storage
fn looks_like_opencode_storage(path: &std::path::Path) -> bool {
    let path_str = path.to_string_lossy().to_lowercase();
    path_str.contains("opencode")
        || path.join("session").exists()
        || path.join("message").exists()
        || path.join("part").exists()
}

/// Parse a session JSON file
fn parse_session_file(path: &PathBuf) -> Result<SessionInfo> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("read session file {}", path.display()))?;
    let session: SessionInfo = serde_json::from_str(&content)
        .with_context(|| format!("parse session JSON {}", path.display()))?;
    Ok(session)
}

/// Load all messages for a session
fn load_messages(session_msg_dir: &PathBuf, part_dir: &PathBuf) -> Result<Vec<NormalizedMessage>> {
    let mut messages = Vec::new();

    // Find all message files for this session
    let msg_files: Vec<PathBuf> = WalkDir::new(session_msg_dir)
        .max_depth(1)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "json")
                .unwrap_or(false)
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    // Build a map of message_id -> parts
    let mut parts_by_msg: HashMap<String, Vec<PartInfo>> = HashMap::new();

    // Scan part directory for all parts
    if part_dir.exists() {
        for entry in WalkDir::new(part_dir).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false)
                && let Ok(content) = fs::read_to_string(path)
                && let Ok(part) = serde_json::from_str::<PartInfo>(&content)
                && let Some(msg_id) = &part.message_id
            {
                parts_by_msg.entry(msg_id.clone()).or_default().push(part);
            }
        }
    }

    for msg_file in msg_files {
        let content = match fs::read_to_string(&msg_file) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let msg_info: MessageInfo = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // Get parts for this message
        let parts = parts_by_msg.get(&msg_info.id).cloned().unwrap_or_default();

        // Assemble message content from parts
        let content_text = assemble_content_from_parts(&parts);
        if content_text.trim().is_empty() {
            continue;
        }

        // Determine role
        let role = msg_info
            .role
            .clone()
            .unwrap_or_else(|| "assistant".to_string());

        // Determine timestamp
        let created_at = msg_info.time.as_ref().and_then(|t| t.created);

        // Author from model_id for assistant messages
        let author = if role == "assistant" {
            msg_info.model_id.clone()
        } else {
            Some("user".to_string())
        };

        messages.push(NormalizedMessage {
            idx: 0, // Will be assigned later
            role,
            author,
            created_at,
            content: content_text,
            extra: serde_json::json!({
                "message_id": msg_info.id,
                "session_id": msg_info.session_id,
            }),
            snippets: Vec::new(),
        });
    }

    // Sort by timestamp and assign indices
    messages.sort_by_key(|m| m.created_at.unwrap_or(i64::MAX));
    super::reindex_messages(&mut messages);

    Ok(messages)
}

/// Assemble message content from parts
fn assemble_content_from_parts(parts: &[PartInfo]) -> String {
    let mut content_pieces: Vec<String> = Vec::new();

    for part in parts {
        match part.part_type.as_deref() {
            Some("text") => {
                if let Some(text) = &part.text
                    && !text.trim().is_empty()
                {
                    content_pieces.push(text.clone());
                }
            }
            Some("tool") => {
                // Include tool output if available
                if let Some(state) = &part.state
                    && let Some(output) = &state.output
                    && !output.trim().is_empty()
                {
                    content_pieces.push(format!("[Tool Output]\n{}", output));
                }
            }
            Some("reasoning") => {
                if let Some(text) = &part.text
                    && !text.trim().is_empty()
                {
                    content_pieces.push(format!("[Reasoning]\n{}", text));
                }
            }
            Some("patch") => {
                if let Some(text) = &part.text
                    && !text.trim().is_empty()
                {
                    content_pieces.push(format!("[Patch]\n{}", text));
                }
            }
            // Ignore step-start, step-finish, and other control parts
            _ => {}
        }
    }

    content_pieces.join("\n\n")
}

// ============================================================================
// Export Adapter for cass export command
// ============================================================================

/// Result of attempting to load an OpenCode session for export.
pub enum OpenCodeExportResult {
    /// Successfully loaded messages
    Messages(Vec<serde_json::Value>),
    /// Path is an OpenCode session file but message directory is missing
    MissingMessageDir {
        session_id: String,
        expected_path: PathBuf,
    },
    /// Path matches OpenCode layout but session JSON is invalid
    InvalidSessionJson { path: PathBuf, error: String },
    /// Path is not an OpenCode session file
    NotOpenCode,
}

#[derive(Default)]
struct ExportReadStats {
    unreadable_messages: usize,
    invalid_messages: usize,
    unreadable_parts: usize,
    invalid_parts: usize,
    unreadable_part_dirs: usize,
}

impl ExportReadStats {
    fn has_issues(&self) -> bool {
        self.unreadable_messages > 0
            || self.invalid_messages > 0
            || self.unreadable_parts > 0
            || self.invalid_parts > 0
            || self.unreadable_part_dirs > 0
    }
}

fn emit_export_warnings(stats: &ExportReadStats, session_msg_dir: &std::path::Path) {
    if !stats.has_issues() {
        return;
    }
    let skipped_messages = stats.unreadable_messages + stats.invalid_messages;
    let skipped_parts = stats.unreadable_parts + stats.invalid_parts;
    eprintln!(
        "Warning: OpenCode export skipped {} message file(s) ({} unreadable, {} invalid JSON), {} part file(s) ({} unreadable, {} invalid JSON), and failed to read {} part directories under {}",
        skipped_messages,
        stats.unreadable_messages,
        stats.invalid_messages,
        skipped_parts,
        stats.unreadable_parts,
        stats.invalid_parts,
        stats.unreadable_part_dirs,
        session_msg_dir.display()
    );
}

/// Attempt to load messages from an OpenCode session file for export.
///
/// This is the adapter between OpenCode's split storage layout and the export
/// command's expected `Vec<serde_json::Value>` format.
///
/// # Arguments
/// * `session_file` - Path to the session JSON file (e.g., `session/{projectID}/{sessionID}.json`)
/// * `include_tools` - Whether to include tool outputs in the content
///
/// # Returns
/// * `OpenCodeExportResult::Messages` - Successfully loaded messages
/// * `OpenCodeExportResult::MissingMessageDir` - Session file found but no messages directory
/// * `OpenCodeExportResult::InvalidSessionJson` - Session file exists but is invalid JSON
/// * `OpenCodeExportResult::NotOpenCode` - Path doesn't look like an OpenCode session
pub fn load_session_for_export(
    session_file: &std::path::Path,
    include_tools: bool,
) -> OpenCodeExportResult {
    // Derive storage root from session file path
    // Expected: storage/session/{projectID}/{sessionID}.json
    // We need: storage/message/{sessionID}/ and storage/part/
    let session_dir = match session_file.parent().and_then(|p| p.parent()) {
        Some(dir) => dir,
        None => return OpenCodeExportResult::NotOpenCode,
    };
    if session_dir.file_name().and_then(|name| name.to_str()) != Some("session") {
        return OpenCodeExportResult::NotOpenCode;
    }
    let storage_root = match session_dir.parent() {
        Some(root) => root.to_path_buf(),
        None => return OpenCodeExportResult::NotOpenCode,
    };
    if !looks_like_opencode_storage(&storage_root) {
        return OpenCodeExportResult::NotOpenCode;
    }

    // Try to parse as OpenCode session file
    let session_info = match parse_session_file(&session_file.to_path_buf()) {
        Ok(s) => s,
        Err(err) => {
            return OpenCodeExportResult::InvalidSessionJson {
                path: session_file.to_path_buf(),
                error: err.to_string(),
            };
        }
    };

    let message_dir = storage_root.join("message").join(&session_info.id);
    let part_dir = storage_root.join("part");

    // Check if message directory exists
    if !message_dir.exists() {
        return OpenCodeExportResult::MissingMessageDir {
            session_id: session_info.id,
            expected_path: message_dir,
        };
    }

    // Load messages using the internal function
    let messages = match load_messages_for_export(&message_dir, &part_dir, include_tools) {
        Ok(msgs) => msgs,
        Err(_) => return OpenCodeExportResult::NotOpenCode,
    };

    OpenCodeExportResult::Messages(messages)
}

/// Load parts for a single message from the OpenCode part directory.
fn load_parts_for_message(
    part_dir: &std::path::Path,
    message_id: &str,
    stats: &mut ExportReadStats,
) -> Vec<PartInfo> {
    let message_part_dir = part_dir.join(message_id);
    if !message_part_dir.exists() {
        return Vec::new();
    }
    let entries = match fs::read_dir(&message_part_dir) {
        Ok(entries) => entries,
        Err(_) => {
            stats.unreadable_part_dirs += 1;
            return Vec::new();
        }
    };

    let mut part_files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().map(|ext| ext == "json").unwrap_or(false))
        .collect();

    part_files.sort_by_key(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    });

    let mut parts = Vec::new();
    for path in part_files {
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(_) => {
                stats.unreadable_parts += 1;
                continue;
            }
        };
        let part = match serde_json::from_str::<PartInfo>(&content) {
            Ok(part) => part,
            Err(_) => {
                stats.invalid_parts += 1;
                continue;
            }
        };
        parts.push(part);
    }

    parts
}

/// Load messages for export, returning them as JSON values shaped for the exporter.
fn load_messages_for_export(
    session_msg_dir: &std::path::Path,
    part_dir: &std::path::Path,
    include_tools: bool,
) -> Result<Vec<serde_json::Value>> {
    let mut messages = Vec::new();
    let mut stats = ExportReadStats::default();

    // Find all message files for this session
    let mut msg_files: Vec<PathBuf> = WalkDir::new(session_msg_dir)
        .max_depth(1)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "json")
                .unwrap_or(false)
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    msg_files.sort_by_key(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    });

    for (file_index, msg_file) in msg_files.into_iter().enumerate() {
        let content = match fs::read_to_string(&msg_file) {
            Ok(c) => c,
            Err(_) => {
                stats.unreadable_messages += 1;
                continue;
            }
        };

        let msg_info: MessageInfo = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(_) => {
                stats.invalid_messages += 1;
                continue;
            }
        };

        // Get parts for this message
        let parts = load_parts_for_message(part_dir, &msg_info.id, &mut stats);
        let has_tool_parts = parts
            .iter()
            .any(|part| part.part_type.as_deref() == Some("tool"));

        // Assemble message content from parts (with tool filtering)
        let content_text = assemble_content_for_export(&parts, include_tools);
        if content_text.trim().is_empty() && (include_tools || !has_tool_parts) {
            continue;
        }

        // Determine role
        let role = msg_info
            .role
            .clone()
            .unwrap_or_else(|| "assistant".to_string());

        // Determine timestamp
        let timestamp = msg_info.time.as_ref().and_then(|t| t.created);

        // Build export-ready JSON value
        let mut msg_json = serde_json::json!({
            "role": role,
            "content": content_text,
        });

        if let Some(ts) = timestamp {
            msg_json["timestamp"] = serde_json::json!(ts);
        }

        if let Some(model) = &msg_info.model_id {
            msg_json["model"] = serde_json::json!(model);
        }

        messages.push((timestamp.unwrap_or(i64::MAX), file_index, msg_json));
    }

    emit_export_warnings(&stats, session_msg_dir);

    // Sort by timestamp, then by file order for stability
    messages.sort_by_key(|(ts, file_index, _)| (*ts, *file_index));

    Ok(messages.into_iter().map(|(_, _, msg)| msg).collect())
}

/// Assemble message content from parts for export, with tool filtering.
fn assemble_content_for_export(parts: &[PartInfo], include_tools: bool) -> String {
    let mut content_pieces: Vec<String> = Vec::new();

    for part in parts {
        match part.part_type.as_deref() {
            Some("text") => {
                if let Some(text) = &part.text
                    && !text.trim().is_empty()
                {
                    content_pieces.push(text.clone());
                }
            }
            Some("tool") => {
                // Only include tool output if include_tools is true
                if include_tools
                    && let Some(state) = &part.state
                    && let Some(output) = &state.output
                    && !output.trim().is_empty()
                {
                    content_pieces.push(format!("[Tool Output]\n{}", output));
                }
            }
            Some("reasoning") => {
                if let Some(text) = &part.text
                    && !text.trim().is_empty()
                {
                    content_pieces.push(format!("[Reasoning]\n{}", text));
                }
            }
            Some("patch") => {
                if let Some(text) = &part.text
                    && !text.trim().is_empty()
                {
                    content_pieces.push(format!("[Patch]\n{}", text));
                }
            }
            // Ignore step-start, step-finish, and other control parts
            _ => {}
        }
    }

    content_pieces.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    // =====================================================
    // Constructor Tests
    // =====================================================

    #[test]
    fn new_creates_connector() {
        let connector = OpenCodeConnector::new();
        let _ = connector;
    }

    #[test]
    fn default_creates_connector() {
        let connector = OpenCodeConnector;
        let _ = connector;
    }

    // =====================================================
    // looks_like_opencode_storage() Tests
    // =====================================================

    #[test]
    fn looks_like_opencode_storage_with_opencode_in_path() {
        let dir = TempDir::new().unwrap();
        let opencode_path = dir.path().join("opencode").join("test");
        fs::create_dir_all(&opencode_path).unwrap();
        assert!(looks_like_opencode_storage(&opencode_path));
    }

    #[test]
    fn looks_like_opencode_storage_with_session_dir() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("session")).unwrap();
        assert!(looks_like_opencode_storage(dir.path()));
    }

    #[test]
    fn looks_like_opencode_storage_with_message_dir() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("message")).unwrap();
        assert!(looks_like_opencode_storage(dir.path()));
    }

    #[test]
    fn looks_like_opencode_storage_with_part_dir() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("part")).unwrap();
        assert!(looks_like_opencode_storage(dir.path()));
    }

    #[test]
    fn looks_like_opencode_storage_returns_false_for_random_dir() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("random")).unwrap();
        assert!(!looks_like_opencode_storage(dir.path()));
    }

    // =====================================================
    // assemble_content_from_parts() Tests
    // =====================================================

    #[test]
    fn assemble_content_from_text_parts() {
        let parts = vec![
            PartInfo {
                id: Some("p1".into()),
                message_id: Some("m1".into()),
                part_type: Some("text".into()),
                text: Some("Hello, world!".into()),
                state: None,
            },
            PartInfo {
                id: Some("p2".into()),
                message_id: Some("m1".into()),
                part_type: Some("text".into()),
                text: Some("Second part".into()),
                state: None,
            },
        ];
        let content = assemble_content_from_parts(&parts);
        assert!(content.contains("Hello, world!"));
        assert!(content.contains("Second part"));
    }

    #[test]
    fn assemble_content_from_tool_parts() {
        let parts = vec![PartInfo {
            id: Some("p1".into()),
            message_id: Some("m1".into()),
            part_type: Some("tool".into()),
            text: None,
            state: Some(ToolState {
                output: Some("Tool executed successfully".into()),
            }),
        }];
        let content = assemble_content_from_parts(&parts);
        assert!(content.contains("[Tool Output]"));
        assert!(content.contains("Tool executed successfully"));
    }

    #[test]
    fn assemble_content_from_reasoning_parts() {
        let parts = vec![PartInfo {
            id: Some("p1".into()),
            message_id: Some("m1".into()),
            part_type: Some("reasoning".into()),
            text: Some("Let me think about this...".into()),
            state: None,
        }];
        let content = assemble_content_from_parts(&parts);
        assert!(content.contains("[Reasoning]"));
        assert!(content.contains("Let me think about this..."));
    }

    #[test]
    fn assemble_content_from_patch_parts() {
        let parts = vec![PartInfo {
            id: Some("p1".into()),
            message_id: Some("m1".into()),
            part_type: Some("patch".into()),
            text: Some("@@ -1,3 +1,4 @@".into()),
            state: None,
        }];
        let content = assemble_content_from_parts(&parts);
        assert!(content.contains("[Patch]"));
        assert!(content.contains("@@ -1,3 +1,4 @@"));
    }

    #[test]
    fn assemble_content_skips_empty_text() {
        let parts = vec![
            PartInfo {
                id: Some("p1".into()),
                message_id: Some("m1".into()),
                part_type: Some("text".into()),
                text: Some("".into()),
                state: None,
            },
            PartInfo {
                id: Some("p2".into()),
                message_id: Some("m1".into()),
                part_type: Some("text".into()),
                text: Some("   ".into()),
                state: None,
            },
            PartInfo {
                id: Some("p3".into()),
                message_id: Some("m1".into()),
                part_type: Some("text".into()),
                text: Some("Actual content".into()),
                state: None,
            },
        ];
        let content = assemble_content_from_parts(&parts);
        assert_eq!(content, "Actual content");
    }

    #[test]
    fn assemble_content_skips_unknown_part_types() {
        let parts = vec![
            PartInfo {
                id: Some("p1".into()),
                message_id: Some("m1".into()),
                part_type: Some("step-start".into()),
                text: Some("Starting...".into()),
                state: None,
            },
            PartInfo {
                id: Some("p2".into()),
                message_id: Some("m1".into()),
                part_type: Some("step-finish".into()),
                text: Some("Done".into()),
                state: None,
            },
        ];
        let content = assemble_content_from_parts(&parts);
        assert!(content.is_empty());
    }

    #[test]
    fn assemble_content_mixed_parts() {
        let parts = vec![
            PartInfo {
                id: Some("p1".into()),
                message_id: Some("m1".into()),
                part_type: Some("text".into()),
                text: Some("Here's my analysis:".into()),
                state: None,
            },
            PartInfo {
                id: Some("p2".into()),
                message_id: Some("m1".into()),
                part_type: Some("reasoning".into()),
                text: Some("Thinking...".into()),
                state: None,
            },
            PartInfo {
                id: Some("p3".into()),
                message_id: Some("m1".into()),
                part_type: Some("tool".into()),
                text: None,
                state: Some(ToolState {
                    output: Some("Result: 42".into()),
                }),
            },
        ];
        let content = assemble_content_from_parts(&parts);
        assert!(content.contains("Here's my analysis:"));
        assert!(content.contains("[Reasoning]"));
        assert!(content.contains("[Tool Output]"));
    }

    // =====================================================
    // Helper: Create OpenCode storage structure
    // =====================================================

    fn create_opencode_storage(dir: &TempDir) -> PathBuf {
        let storage = dir.path().join("opencode").join("storage");
        fs::create_dir_all(storage.join("session")).unwrap();
        fs::create_dir_all(storage.join("message")).unwrap();
        fs::create_dir_all(storage.join("part")).unwrap();
        storage
    }

    fn write_session(storage: &Path, project_id: &str, session: &serde_json::Value) {
        let session_id = session["id"].as_str().unwrap();
        let session_dir = storage.join("session").join(project_id);
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join(format!("{session_id}.json")),
            session.to_string(),
        )
        .unwrap();
    }

    fn write_message(storage: &Path, session_id: &str, message: &serde_json::Value) {
        let message_id = message["id"].as_str().unwrap();
        let message_dir = storage.join("message").join(session_id);
        fs::create_dir_all(&message_dir).unwrap();
        fs::write(
            message_dir.join(format!("{message_id}.json")),
            message.to_string(),
        )
        .unwrap();
    }

    fn write_part(storage: &Path, message_id: &str, part: &serde_json::Value) {
        let part_id = part["id"].as_str().unwrap();
        let part_dir = storage.join("part").join(message_id);
        fs::create_dir_all(&part_dir).unwrap();
        fs::write(part_dir.join(format!("{part_id}.json")), part.to_string()).unwrap();
    }

    // =====================================================
    // scan() Tests
    // =====================================================

    #[test]
    fn scan_parses_simple_conversation() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        // Create session
        let session = json!({
            "id": "sess-001",
            "title": "Test Session",
            "directory": "/home/user/project",
            "projectID": "proj-001",
            "time": {
                "created": 1733000000,
                "updated": 1733000100
            }
        });
        write_session(&storage, "proj-001", &session);

        // Create message
        let message = json!({
            "id": "msg-001",
            "role": "user",
            "sessionID": "sess-001",
            "time": {
                "created": 1733000000,
                "completed": 1733000001
            }
        });
        write_message(&storage, "sess-001", &message);

        // Create part
        let part = json!({
            "id": "part-001",
            "messageID": "msg-001",
            "type": "text",
            "text": "Hello, OpenCode!"
        });
        write_part(&storage, "msg-001", &part);

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].title, Some("Test Session".to_string()));
        assert_eq!(
            convs[0].workspace,
            Some(PathBuf::from("/home/user/project"))
        );
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].role, "user");
        assert!(convs[0].messages[0].content.contains("Hello, OpenCode!"));
    }

    #[test]
    fn scan_parses_multiple_messages() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        let session = json!({
            "id": "sess-002",
            "projectID": "proj-001"
        });
        write_session(&storage, "proj-001", &session);

        // User message
        let user_msg = json!({
            "id": "msg-u1",
            "role": "user",
            "sessionID": "sess-002",
            "time": {"created": 1733000000}
        });
        write_message(&storage, "sess-002", &user_msg);
        write_part(
            &storage,
            "msg-u1",
            &json!({
                "id": "p1",
                "messageID": "msg-u1",
                "type": "text",
                "text": "What is 2+2?"
            }),
        );

        // Assistant message
        let assistant_msg = json!({
            "id": "msg-a1",
            "role": "assistant",
            "sessionID": "sess-002",
            "modelID": "gpt-4",
            "time": {"created": 1733000001}
        });
        write_message(&storage, "sess-002", &assistant_msg);
        write_part(
            &storage,
            "msg-a1",
            &json!({
                "id": "p2",
                "messageID": "msg-a1",
                "type": "text",
                "text": "2 + 2 = 4"
            }),
        );

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[1].role, "assistant");
        assert_eq!(convs[0].messages[1].author, Some("gpt-4".to_string()));
    }

    #[test]
    fn scan_handles_empty_storage() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 0);
    }

    #[test]
    fn scan_skips_sessions_without_messages() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        let session = json!({
            "id": "sess-empty",
            "title": "Empty Session",
            "projectID": "proj-001"
        });
        write_session(&storage, "proj-001", &session);
        // Don't create any messages

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 0);
    }

    #[test]
    fn scan_extracts_title_from_first_message_if_no_session_title() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        let session = json!({
            "id": "sess-no-title",
            "projectID": "proj-001"
            // No title field
        });
        write_session(&storage, "proj-001", &session);

        let message = json!({
            "id": "msg-001",
            "role": "user",
            "sessionID": "sess-no-title",
            "time": {"created": 1733000000}
        });
        write_message(&storage, "sess-no-title", &message);
        write_part(
            &storage,
            "msg-001",
            &json!({
                "id": "p1",
                "messageID": "msg-001",
                "type": "text",
                "text": "This is the first line\nSecond line\nThird line"
            }),
        );

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].title, Some("This is the first line".to_string()));
    }

    #[test]
    fn scan_sets_agent_slug_to_opencode() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        let session = json!({
            "id": "sess-slug",
            "projectID": "proj-001"
        });
        write_session(&storage, "proj-001", &session);

        let message = json!({
            "id": "msg-001",
            "role": "user",
            "sessionID": "sess-slug",
            "time": {"created": 1733000000}
        });
        write_message(&storage, "sess-slug", &message);
        write_part(
            &storage,
            "msg-001",
            &json!({"id": "p1", "messageID": "msg-001", "type": "text", "text": "Test"}),
        );

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].agent_slug, "opencode");
    }

    #[test]
    fn scan_sets_metadata_with_session_and_project_id() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        let session = json!({
            "id": "sess-meta",
            "projectID": "proj-meta-001"
        });
        write_session(&storage, "proj-meta-001", &session);

        let message = json!({
            "id": "msg-001",
            "role": "user",
            "sessionID": "sess-meta",
            "time": {"created": 1733000000}
        });
        write_message(&storage, "sess-meta", &message);
        write_part(
            &storage,
            "msg-001",
            &json!({"id": "p1", "messageID": "msg-001", "type": "text", "text": "Test"}),
        );

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].metadata["session_id"], "sess-meta");
        assert_eq!(convs[0].metadata["project_id"], "proj-meta-001");
    }

    #[test]
    fn scan_sorts_messages_by_timestamp() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        let session = json!({
            "id": "sess-sort",
            "projectID": "proj-001"
        });
        write_session(&storage, "proj-001", &session);

        // Create messages out of order
        let msg_later = json!({
            "id": "msg-later",
            "role": "assistant",
            "sessionID": "sess-sort",
            "time": {"created": 1733000100}
        });
        let msg_earlier = json!({
            "id": "msg-earlier",
            "role": "user",
            "sessionID": "sess-sort",
            "time": {"created": 1733000000}
        });
        write_message(&storage, "sess-sort", &msg_later);
        write_message(&storage, "sess-sort", &msg_earlier);

        write_part(
            &storage,
            "msg-later",
            &json!({"id": "p1", "messageID": "msg-later", "type": "text", "text": "Later"}),
        );
        write_part(
            &storage,
            "msg-earlier",
            &json!({"id": "p2", "messageID": "msg-earlier", "type": "text", "text": "Earlier"}),
        );

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].messages.len(), 2);
        // Earlier message should be first due to sorting
        assert!(convs[0].messages[0].content.contains("Earlier"));
        assert!(convs[0].messages[1].content.contains("Later"));
    }

    #[test]
    fn scan_assigns_sequential_indices() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        let session = json!({
            "id": "sess-idx",
            "projectID": "proj-001"
        });
        write_session(&storage, "proj-001", &session);

        for i in 0..3 {
            let msg = json!({
                "id": format!("msg-{i}"),
                "role": "user",
                "sessionID": "sess-idx",
                "time": {"created": 1733000000 + i}
            });
            write_message(&storage, "sess-idx", &msg);
            write_part(
                &storage,
                &format!("msg-{i}"),
                &json!({
                    "id": format!("p{i}"),
                    "messageID": format!("msg-{i}"),
                    "type": "text",
                    "text": format!("Message {i}")
                }),
            );
        }

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].messages[0].idx, 0);
        assert_eq!(convs[0].messages[1].idx, 1);
        assert_eq!(convs[0].messages[2].idx, 2);
    }

    #[test]
    fn scan_handles_messages_without_parts() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        let session = json!({
            "id": "sess-no-parts",
            "projectID": "proj-001"
        });
        write_session(&storage, "proj-001", &session);

        let message = json!({
            "id": "msg-no-parts",
            "role": "user",
            "sessionID": "sess-no-parts",
            "time": {"created": 1733000000}
        });
        write_message(&storage, "sess-no-parts", &message);
        // Don't create any parts

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Session should be skipped because message has no content
        assert_eq!(convs.len(), 0);
    }

    #[test]
    fn scan_deduplicates_sessions_by_id() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        // Create same session in two project directories
        let session = json!({
            "id": "sess-dupe",
            "title": "Duplicate Session",
            "projectID": "proj-001"
        });
        write_session(&storage, "proj-001", &session);
        write_session(&storage, "proj-002", &session);

        let message = json!({
            "id": "msg-001",
            "role": "user",
            "sessionID": "sess-dupe",
            "time": {"created": 1733000000}
        });
        write_message(&storage, "sess-dupe", &message);
        write_part(
            &storage,
            "msg-001",
            &json!({"id": "p1", "messageID": "msg-001", "type": "text", "text": "Test"}),
        );

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Should only have one conversation (deduplicated)
        assert_eq!(convs.len(), 1);
    }

    #[test]
    fn scan_uses_default_role_when_missing() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        let session = json!({
            "id": "sess-no-role",
            "projectID": "proj-001"
        });
        write_session(&storage, "proj-001", &session);

        // Message without role field
        let message = json!({
            "id": "msg-no-role",
            "sessionID": "sess-no-role",
            "time": {"created": 1733000000}
        });
        write_message(&storage, "sess-no-role", &message);
        write_part(
            &storage,
            "msg-no-role",
            &json!({"id": "p1", "messageID": "msg-no-role", "type": "text", "text": "Test"}),
        );

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Default role should be "assistant"
        assert_eq!(convs[0].messages[0].role, "assistant");
    }

    #[test]
    fn scan_handles_multiple_parts_per_message() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        let session = json!({
            "id": "sess-multi-part",
            "projectID": "proj-001"
        });
        write_session(&storage, "proj-001", &session);

        let message = json!({
            "id": "msg-multi",
            "role": "assistant",
            "sessionID": "sess-multi-part",
            "time": {"created": 1733000000}
        });
        write_message(&storage, "sess-multi-part", &message);

        // Multiple parts for one message
        write_part(
            &storage,
            "msg-multi",
            &json!({"id": "p1", "messageID": "msg-multi", "type": "text", "text": "First part"}),
        );
        write_part(
            &storage,
            "msg-multi",
            &json!({"id": "p2", "messageID": "msg-multi", "type": "reasoning", "text": "Reasoning part"}),
        );
        write_part(
            &storage,
            "msg-multi",
            &json!({"id": "p3", "messageID": "msg-multi", "type": "text", "text": "Third part"}),
        );

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        let content = &convs[0].messages[0].content;
        assert!(content.contains("First part"));
        assert!(content.contains("[Reasoning]"));
        assert!(content.contains("Third part"));
    }

    #[test]
    fn scan_extracts_timestamps() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        let session = json!({
            "id": "sess-ts",
            "projectID": "proj-001",
            "time": {
                "created": 1733000000,
                "updated": 1733000200
            }
        });
        write_session(&storage, "proj-001", &session);

        let message = json!({
            "id": "msg-ts",
            "role": "user",
            "sessionID": "sess-ts",
            "time": {"created": 1733000050}
        });
        write_message(&storage, "sess-ts", &message);
        write_part(
            &storage,
            "msg-ts",
            &json!({"id": "p1", "messageID": "msg-ts", "type": "text", "text": "Test"}),
        );

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].started_at, Some(1733000000));
        assert_eq!(convs[0].ended_at, Some(1733000200));
        assert_eq!(convs[0].messages[0].created_at, Some(1733000050));
    }

    #[test]
    fn scan_uses_external_id_from_session_id() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        let session = json!({
            "id": "unique-session-id-123",
            "projectID": "proj-001"
        });
        write_session(&storage, "proj-001", &session);

        let message = json!({
            "id": "msg-001",
            "role": "user",
            "sessionID": "unique-session-id-123",
            "time": {"created": 1733000000}
        });
        write_message(&storage, "unique-session-id-123", &message);
        write_part(
            &storage,
            "msg-001",
            &json!({"id": "p1", "messageID": "msg-001", "type": "text", "text": "Test"}),
        );

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(
            convs[0].external_id,
            Some("unique-session-id-123".to_string())
        );
    }

    #[test]
    fn scan_skips_invalid_session_json() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        // Create invalid session file
        let session_dir = storage.join("session").join("proj-001");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("invalid.json"), "not valid json").unwrap();

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 0);
    }

    #[test]
    fn scan_skips_invalid_message_json() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        let session = json!({
            "id": "sess-invalid-msg",
            "projectID": "proj-001"
        });
        write_session(&storage, "proj-001", &session);

        // Create invalid message file
        let msg_dir = storage.join("message").join("sess-invalid-msg");
        fs::create_dir_all(&msg_dir).unwrap();
        fs::write(msg_dir.join("bad.json"), "not valid json").unwrap();

        let connector = OpenCodeConnector::new();
        let ctx = ScanContext::local_default(storage.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Should skip the session because no valid messages
        assert_eq!(convs.len(), 0);
    }

    // =====================================================
    // parse_session_file() Tests
    // =====================================================

    #[test]
    fn parse_session_file_parses_complete_session() {
        let dir = TempDir::new().unwrap();
        let session = json!({
            "id": "sess-parse",
            "title": "Parse Test",
            "directory": "/test/dir",
            "projectID": "proj-parse",
            "time": {
                "created": 1733000000,
                "updated": 1733000100
            }
        });
        let path = dir.path().join("session.json");
        fs::write(&path, session.to_string()).unwrap();

        let result = parse_session_file(&path).unwrap();
        assert_eq!(result.id, "sess-parse");
        assert_eq!(result.title, Some("Parse Test".to_string()));
        assert_eq!(result.directory, Some("/test/dir".to_string()));
        assert_eq!(result.project_id, Some("proj-parse".to_string()));
        assert!(result.time.is_some());
    }

    #[test]
    fn parse_session_file_handles_minimal_session() {
        let dir = TempDir::new().unwrap();
        let session = json!({"id": "minimal"});
        let path = dir.path().join("minimal.json");
        fs::write(&path, session.to_string()).unwrap();

        let result = parse_session_file(&path).unwrap();
        assert_eq!(result.id, "minimal");
        assert!(result.title.is_none());
        assert!(result.directory.is_none());
    }

    // =====================================================
    // load_session_for_export() Tests
    // =====================================================

    #[test]
    fn export_loads_opencode_session_messages() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        // Create session
        let session = json!({
            "id": "sess-export",
            "title": "Export Test",
            "projectID": "proj-export",
            "time": { "created": 1733000000, "updated": 1733000100 }
        });
        write_session(&storage, "proj-export", &session);

        // Create message
        let message = json!({
            "id": "msg-export",
            "role": "user",
            "sessionID": "sess-export",
            "time": { "created": 1733000000 }
        });
        write_message(&storage, "sess-export", &message);

        // Create text part
        let part = json!({
            "id": "part-export",
            "messageID": "msg-export",
            "type": "text",
            "text": "Hello from export!"
        });
        write_part(&storage, "msg-export", &part);

        let session_path = storage
            .join("session")
            .join("proj-export")
            .join("sess-export.json");

        match load_session_for_export(&session_path, true) {
            OpenCodeExportResult::Messages(messages) => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0]["role"], "user");
                assert!(
                    messages[0]["content"]
                        .as_str()
                        .unwrap()
                        .contains("Hello from export!")
                );
                assert!(messages[0]["timestamp"].as_i64().is_some());
            }
            other => panic!(
                "Expected Messages, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn export_loads_session_without_project_id() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        // Create session with no projectID field
        let session = json!({
            "id": "sess-no-projid",
            "title": "Export Test"
        });
        write_session(&storage, "proj-no-projid", &session);

        // Create message
        let message = json!({
            "id": "msg-no-projid",
            "role": "user",
            "sessionID": "sess-no-projid",
            "time": { "created": 1733000000 }
        });
        write_message(&storage, "sess-no-projid", &message);

        // Create text part
        let part = json!({
            "id": "part-no-projid",
            "messageID": "msg-no-projid",
            "type": "text",
            "text": "Hello without projectID!"
        });
        write_part(&storage, "msg-no-projid", &part);

        let session_path = storage
            .join("session")
            .join("proj-no-projid")
            .join("sess-no-projid.json");

        match load_session_for_export(&session_path, true) {
            OpenCodeExportResult::Messages(messages) => {
                assert_eq!(messages.len(), 1);
                assert!(
                    messages[0]["content"]
                        .as_str()
                        .unwrap()
                        .contains("Hello without projectID!")
                );
                assert_eq!(messages[0]["timestamp"].as_i64(), Some(1733000000));
            }
            other => panic!(
                "Expected Messages, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn export_preserves_file_order_for_equal_timestamps() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        let session = json!({
            "id": "sess-order",
            "title": "Order Test",
            "projectID": "proj-order"
        });
        write_session(&storage, "proj-order", &session);

        let message_a = json!({
            "id": "msg-a",
            "role": "user",
            "sessionID": "sess-order",
            "time": { "created": 1733000000 }
        });
        let message_b = json!({
            "id": "msg-b",
            "role": "assistant",
            "sessionID": "sess-order",
            "time": { "created": 1733000000 }
        });

        // Write message_b first to ensure file order tie-break uses filename ordering.
        write_message(&storage, "sess-order", &message_b);
        write_message(&storage, "sess-order", &message_a);

        let part_a = json!({
            "id": "part-a",
            "messageID": "msg-a",
            "type": "text",
            "text": "First"
        });
        let part_b = json!({
            "id": "part-b",
            "messageID": "msg-b",
            "type": "text",
            "text": "Second"
        });
        write_part(&storage, "msg-a", &part_a);
        write_part(&storage, "msg-b", &part_b);

        let session_path = storage
            .join("session")
            .join("proj-order")
            .join("sess-order.json");

        match load_session_for_export(&session_path, true) {
            OpenCodeExportResult::Messages(messages) => {
                assert_eq!(messages.len(), 2);
                let first = messages[0]["content"].as_str().unwrap();
                let second = messages[1]["content"].as_str().unwrap();
                assert_eq!(first, "First");
                assert_eq!(second, "Second");
            }
            other => panic!(
                "Expected Messages, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn export_excludes_tools_when_include_tools_false() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        // Create session
        let session = json!({
            "id": "sess-tools",
            "title": "Tools Test",
            "projectID": "proj-tools"
        });
        write_session(&storage, "proj-tools", &session);

        // Create assistant message
        let message = json!({
            "id": "msg-tools",
            "role": "assistant",
            "sessionID": "sess-tools",
            "time": { "created": 1733000000 }
        });
        write_message(&storage, "sess-tools", &message);

        // Create text part
        let text_part = json!({
            "id": "part-text",
            "messageID": "msg-tools",
            "type": "text",
            "text": "Let me check that file."
        });
        write_part(&storage, "msg-tools", &text_part);

        // Create tool part
        let tool_part = json!({
            "id": "part-tool",
            "messageID": "msg-tools",
            "type": "tool",
            "state": { "output": "file.txt contents here" }
        });
        write_part(&storage, "msg-tools", &tool_part);

        let session_path = storage
            .join("session")
            .join("proj-tools")
            .join("sess-tools.json");

        // With include_tools=false, tool output should be excluded
        match load_session_for_export(&session_path, false) {
            OpenCodeExportResult::Messages(messages) => {
                assert_eq!(messages.len(), 1);
                let content = messages[0]["content"].as_str().unwrap();
                assert!(content.contains("Let me check that file."));
                assert!(!content.contains("Tool Output"));
                assert!(!content.contains("file.txt contents here"));
            }
            other => panic!(
                "Expected Messages, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        // With include_tools=true, tool output should be included
        match load_session_for_export(&session_path, true) {
            OpenCodeExportResult::Messages(messages) => {
                let content = messages[0]["content"].as_str().unwrap();
                assert!(content.contains("Let me check that file."));
                assert!(content.contains("[Tool Output]"));
                assert!(content.contains("file.txt contents here"));
            }
            other => panic!(
                "Expected Messages, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn export_keeps_tool_only_message_when_tools_excluded() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        // Create session
        let session = json!({
            "id": "sess-tool-only",
            "title": "Tool Only",
            "projectID": "proj-tool-only"
        });
        write_session(&storage, "proj-tool-only", &session);

        // Create assistant message
        let message = json!({
            "id": "msg-tool-only",
            "role": "assistant",
            "sessionID": "sess-tool-only",
            "time": { "created": 1733000000 }
        });
        write_message(&storage, "sess-tool-only", &message);

        // Create tool part only
        let tool_part = json!({
            "id": "part-tool-only",
            "messageID": "msg-tool-only",
            "type": "tool",
            "state": { "output": "hidden output" }
        });
        write_part(&storage, "msg-tool-only", &tool_part);

        let session_path = storage
            .join("session")
            .join("proj-tool-only")
            .join("sess-tool-only.json");

        match load_session_for_export(&session_path, false) {
            OpenCodeExportResult::Messages(messages) => {
                assert_eq!(messages.len(), 1);
                let content = messages[0]["content"].as_str().unwrap_or("");
                assert!(content.is_empty());
            }
            other => panic!(
                "Expected Messages, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn export_returns_invalid_session_json_for_opencode_layout() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);
        let session_dir = storage.join("session").join("proj-invalid");
        fs::create_dir_all(&session_dir).unwrap();
        let session_path = session_dir.join("sess-invalid.json");
        fs::write(&session_path, "{not-json").unwrap();

        match load_session_for_export(&session_path, true) {
            OpenCodeExportResult::InvalidSessionJson { path, .. } => {
                assert_eq!(path, session_path);
            }
            other => panic!(
                "Expected InvalidSessionJson, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn export_returns_not_opencode_for_non_session_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("random.json");
        fs::write(&path, r#"{"some": "json"}"#).unwrap();

        match load_session_for_export(&path, true) {
            OpenCodeExportResult::NotOpenCode => {}
            other => panic!(
                "Expected NotOpenCode, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn export_returns_missing_message_dir_when_no_messages() {
        let dir = TempDir::new().unwrap();
        let storage = create_opencode_storage(&dir);

        // Create session but NO message directory for this session
        let session = json!({
            "id": "sess-nomsg",
            "title": "No Messages",
            "projectID": "proj-nomsg"
        });
        write_session(&storage, "proj-nomsg", &session);
        // Don't create the message directory

        let session_path = storage
            .join("session")
            .join("proj-nomsg")
            .join("sess-nomsg.json");

        match load_session_for_export(&session_path, true) {
            OpenCodeExportResult::MissingMessageDir { session_id, .. } => {
                assert_eq!(session_id, "sess-nomsg");
            }
            other => panic!(
                "Expected MissingMessageDir, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }
}
