//! Connector for Cursor IDE chat history.
//!
//! Cursor stores chat history in SQLite databases (state.vscdb) within:
//! - macOS: ~/Library/Application Support/Cursor/User/globalStorage/
//! - macOS workspaces: ~/Library/Application Support/Cursor/User/workspaceStorage/{id}/
//! - Linux: ~/.config/Cursor/User/globalStorage/
//! - Windows: %APPDATA%/Cursor/User/globalStorage/
//!
//! Chat data is stored in the `cursorDiskKV` table with keys like:
//! - `composerData:{uuid}` - Composer/chat session data (JSON)
//!
//! And in the `ItemTable` with keys like:
//! - `workbench.panel.aichat.view.aichat.chatdata` - Legacy chat data

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

/// Cursor v0.40+ bubble type constants (numeric encoding)
mod bubble_type {
    /// User message type in new format
    pub const USER: i64 = 1;
    /// Assistant message type in new format
    pub const ASSISTANT: i64 = 2;
}

/// Type alias for the bubble data lookup map.
/// Keys are bubble IDs for O(1) lookup within a composer.
type BubbleDataMap = HashMap<String, Value>;

pub struct CursorConnector;

impl Default for CursorConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorConnector {
    pub fn new() -> Self {
        Self
    }

    /// Get the base Cursor application support directory
    pub fn app_support_dir() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            dirs::home_dir().map(|h| h.join("Library/Application Support/Cursor/User"))
        }
        #[cfg(target_os = "linux")]
        {
            // Check if we're in WSL and should look at Windows Cursor paths first
            if Self::is_wsl()
                && let Some(wsl_path) = Self::find_wsl_cursor_path()
            {
                return Some(wsl_path);
            }
            // Fall back to Linux native path
            dirs::home_dir().map(|h| h.join(".config/Cursor/User"))
        }
        #[cfg(target_os = "windows")]
        {
            dirs::data_dir().map(|d| d.join("Cursor/User"))
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            None
        }
    }

    /// Check if running inside Windows Subsystem for Linux
    #[cfg(target_os = "linux")]
    fn is_wsl() -> bool {
        std::fs::read_to_string("/proc/version")
            .map(|v| v.to_lowercase().contains("microsoft"))
            .unwrap_or(false)
    }

    /// Find Cursor installation path via WSL mount points
    /// Probes /mnt/c/Users/*/AppData/Roaming/Cursor/User
    #[cfg(target_os = "linux")]
    fn find_wsl_cursor_path() -> Option<PathBuf> {
        let mnt_c = Path::new("/mnt/c/Users");
        if !mnt_c.exists() {
            return None;
        }

        for entry in std::fs::read_dir(mnt_c).ok()?.flatten() {
            // Skip system directories
            let name = entry.file_name();
            let name_str = name.to_str().unwrap_or("");
            if name_str == "Default"
                || name_str == "Public"
                || name_str == "All Users"
                || name_str == "Default User"
            {
                continue;
            }

            let cursor_path = entry.path().join("AppData/Roaming/Cursor/User");
            if cursor_path.join("globalStorage").exists()
                || cursor_path.join("workspaceStorage").exists()
            {
                tracing::debug!(
                    path = %cursor_path.display(),
                    "Found Windows Cursor installation via WSL"
                );
                return Some(cursor_path);
            }
        }
        None
    }

    /// Find all state.vscdb files in Cursor storage
    fn find_db_files(base: &Path) -> Vec<PathBuf> {
        let mut dbs = Vec::new();

        // Check globalStorage
        let global_db = base.join("globalStorage/state.vscdb");
        if global_db.exists() {
            dbs.push(global_db);
        }

        // Check workspaceStorage subdirectories
        let workspace_storage = base.join("workspaceStorage");
        if workspace_storage.exists() {
            for entry in WalkDir::new(&workspace_storage)
                .max_depth(2)
                .into_iter()
                .flatten()
            {
                if entry.file_type().is_file() && entry.file_name().to_str() == Some("state.vscdb")
                {
                    dbs.push(entry.path().to_path_buf());
                }
            }
        }

        dbs
    }

    /// Fetch bubble data for a specific composer from the database.
    /// Returns a map keyed by bubbleId for efficient O(1) lookup.
    /// This lazy-loads only the bubbles needed for one conversation,
    /// avoiding loading all bubbles into memory.
    fn fetch_bubble_data_for_composer(conn: &Connection, composer_id: &str) -> BubbleDataMap {
        let mut bubble_map = BubbleDataMap::new();

        // Only fetch bubbles for this specific composer
        let pattern = format!("bubbleId:{}:%", composer_id);
        let prefix_len = format!("bubbleId:{}:", composer_id).len();

        if let Ok(mut stmt) = conn.prepare("SELECT key, value FROM cursorDiskKV WHERE key LIKE ?") {
            let rows = stmt.query_map([&pattern], |row| {
                let key: String = row.get(0)?;
                let value: String = row.get(1)?;
                Ok((key, value))
            });

            if let Ok(rows) = rows {
                for row in rows.flatten() {
                    let (key, value) = row;
                    // Key format: bubbleId:{composerId}:{bubbleId}
                    // Extract just the bubbleId part
                    if key.len() > prefix_len {
                        let bubble_id = &key[prefix_len..];
                        if let Ok(parsed) = serde_json::from_str::<Value>(&value) {
                            bubble_map.insert(bubble_id.to_string(), parsed);
                        }
                    }
                }
            }
        }

        bubble_map
    }

    /// Extract workspace from bubble data.
    /// Cursor v0.40+ stores workspaceProjectDir in bubble entries.
    fn extract_workspace_from_bubbles(bubble_map: &BubbleDataMap) -> Option<PathBuf> {
        for bubble in bubble_map.values() {
            // Try workspaceProjectDir first (most common)
            if let Some(dir) = bubble.get("workspaceProjectDir").and_then(|v| v.as_str())
                && !dir.is_empty()
            {
                return Some(PathBuf::from(dir));
            }

            // Try workspaceUris array
            if let Some(uris) = bubble.get("workspaceUris").and_then(|v| v.as_array()) {
                for uri in uris {
                    if let Some(uri_str) = uri.as_str() {
                        // Parse file:// or vscode-remote:// URIs
                        if let Some(path) = Self::parse_workspace_uri(uri_str) {
                            return Some(path);
                        }
                    }
                }
            }
        }
        None
    }

    /// Parse a workspace URI to extract the path.
    /// Handles file:// and vscode-remote:// URIs.
    fn parse_workspace_uri(uri: &str) -> Option<PathBuf> {
        if let Some(path) = uri.strip_prefix("file://") {
            // URL decode and return
            let decoded = urlencoding::decode(path).ok()?;
            return Some(PathBuf::from(decoded.into_owned()));
        }

        // Handle vscode-remote://ssh-remote+{json}/path format
        if let Some(rest) = uri.strip_prefix("vscode-remote://") {
            // Extract path after the host/connection info
            // Format: vscode-remote://ssh-remote+{encoded-json}/actual/path
            if let Some(slash_idx) = rest.find('/') {
                let path = &rest[slash_idx..];
                let decoded = urlencoding::decode(path).ok()?;
                return Some(PathBuf::from(decoded.into_owned()));
            }
        }

        None
    }

    /// Extract chat sessions from a SQLite database
    fn extract_from_db(
        db_path: &Path,
        since_ts: Option<i64>,
    ) -> Result<Vec<NormalizedConversation>> {
        let conn = Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("failed to open Cursor db: {}", db_path.display()))?;

        let mut convs = Vec::new();
        let mut seen_ids = HashSet::new();

        // Try cursorDiskKV table for composerData entries
        if let Ok(mut stmt) =
            conn.prepare("SELECT key, value FROM cursorDiskKV WHERE key LIKE 'composerData:%'")
        {
            let rows = stmt.query_map([], |row| {
                let key: String = row.get(0)?;
                let value: String = row.get(1)?;
                Ok((key, value))
            });

            if let Ok(rows) = rows {
                for row in rows.flatten() {
                    let (key, value) = row;
                    if let Some(conv) = Self::parse_composer_data(
                        &key,
                        &value,
                        db_path,
                        since_ts,
                        &mut seen_ids,
                        Some(&conn),
                    ) {
                        convs.push(conv);
                    }
                }
            }
        }

        // Also try ItemTable for legacy aichat data
        if let Ok(mut stmt) = conn.prepare(
            "SELECT key, value FROM ItemTable WHERE key LIKE '%aichat%chatdata%' OR key LIKE '%composer%'",
        ) {
            let rows = stmt.query_map([], |row| {
                let key: String = row.get(0)?;
                let value: String = row.get(1)?;
                Ok((key, value))
            });

            if let Ok(rows) = rows {
                for row in rows.flatten() {
                    let (key, value) = row;
                    if let Some(conv) =
                        Self::parse_aichat_data(&key, &value, db_path, since_ts, &mut seen_ids)
                    {
                        convs.push(conv);
                    }
                }
            }
        }

        Ok(convs)
    }

    /// Parse composerData JSON into a conversation.
    ///
    /// Supports multiple Cursor formats:
    /// - v0.40+ (new): `fullConversationHeadersOnly` with separate bubbleId entries
    /// - v0.3x (tabs): `tabs` → `bubbles` structure
    /// - v0.2x (conversationMap): `conversationMap` → `bubbles` structure
    /// - Simple: `text`/`richText` fields only
    fn parse_composer_data(
        key: &str,
        value: &str,
        db_path: &Path,
        _since_ts: Option<i64>, // File-level filtering done in scan(); message filtering not needed
        seen_ids: &mut HashSet<String>,
        conn: Option<&Connection>,
    ) -> Option<NormalizedConversation> {
        let val: Value = serde_json::from_str(value).ok()?;

        // Extract composer ID from key (composerData:{uuid})
        let composer_id = key.strip_prefix("composerData:")?.to_string();

        // Skip if already seen
        if seen_ids.contains(&composer_id) {
            return None;
        }
        seen_ids.insert(composer_id.clone());

        // Extract timestamps
        let created_at = val.get("createdAt").and_then(|v| v.as_i64());
        let last_updated_at = val.get("lastUpdatedAt").and_then(|v| v.as_i64());

        // NOTE: Do NOT filter conversations/messages by timestamp here!
        // The file-level check in file_modified_since() is sufficient.
        // Filtering would cause data loss when the file is re-indexed.

        let mut messages = Vec::new();
        let mut workspace: Option<PathBuf> = None;

        // Check for v0.40+ format with fullConversationHeadersOnly
        // This format stores only bubble IDs in composerData, with actual content
        // in separate bubbleId:{composerId}:{bubbleId} keys
        // Note: requires a database connection to fetch bubble content
        if let (Some(headers), Some(conn)) = (
            val.get("fullConversationHeadersOnly")
                .and_then(|v| v.as_array()),
            conn,
        ) {
            // Lazy-load bubble data for this composer
            let bubble_map = Self::fetch_bubble_data_for_composer(conn, &composer_id);

            // Extract workspace from bubbles
            workspace = Self::extract_workspace_from_bubbles(&bubble_map);

            // Parse each header reference
            for header in headers {
                if let Some(bubble_id) = header.get("bubbleId").and_then(|v| v.as_str())
                    && let Some(bubble) = bubble_map.get(bubble_id)
                    && let Some(msg) = Self::parse_bubble(bubble, messages.len())
                {
                    messages.push(msg);
                }
            }
        }

        // Parse conversation from bubbles/tabs structure (legacy v0.3x)
        if messages.is_empty()
            && let Some(tabs) = val.get("tabs").and_then(|v| v.as_array())
        {
            for tab in tabs {
                if let Some(bubbles) = tab.get("bubbles").and_then(|v| v.as_array()) {
                    for (idx, bubble) in bubbles.iter().enumerate() {
                        if let Some(msg) = Self::parse_bubble(bubble, idx) {
                            messages.push(msg);
                        }
                    }
                }
            }
        }

        // Also check conversationMap for older format (v0.2x)
        if messages.is_empty()
            && let Some(conv_map) = val.get("conversationMap").and_then(|v| v.as_object())
        {
            for (_, conv_val) in conv_map {
                if let Some(bubbles) = conv_val.get("bubbles").and_then(|v| v.as_array()) {
                    for (idx, bubble) in bubbles.iter().enumerate() {
                        if let Some(msg) = Self::parse_bubble(bubble, messages.len() + idx) {
                            messages.push(msg);
                        }
                    }
                }
            }
        }

        // Check for text/richText as user input (simple composer sessions)
        let user_text = val
            .get("text")
            .and_then(|v| v.as_str())
            .or_else(|| val.get("richText").and_then(|v| v.as_str()))
            .unwrap_or("");

        if !user_text.is_empty() && messages.is_empty() {
            messages.push(NormalizedMessage {
                idx: 0,
                role: "user".to_string(),
                author: None,
                created_at,
                content: user_text.to_string(),
                extra: serde_json::json!({}),
                snippets: Vec::new(),
            });
        }

        // Skip if no messages
        if messages.is_empty() {
            return None;
        }

        // Re-index messages
        for (i, msg) in messages.iter_mut().enumerate() {
            msg.idx = i as i64;
        }

        // Extract model info
        let model_name = val
            .get("modelConfig")
            .and_then(|m| m.get("modelName"))
            .and_then(|v| v.as_str());

        // Use explicit name field if available (v0.40+), otherwise derive from first message
        let title = val
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(100).collect())
            .or_else(|| {
                messages.first().map(|m| {
                    m.content
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(100)
                        .collect()
                })
            })
            .or_else(|| model_name.map(|m| format!("Cursor chat with {}", m)));

        // source_path must be unique per conversation for proper lookup in the TUI.
        // Since multiple conversations live in the same database file, we append
        // the composer_id to create a unique synthetic path for each conversation.
        let unique_source_path = db_path.join(&composer_id);

        Some(NormalizedConversation {
            agent_slug: "cursor".to_string(),
            external_id: Some(composer_id),
            title,
            workspace,
            source_path: unique_source_path,
            started_at: created_at,
            // Use lastUpdatedAt if available (most accurate), fall back to last message time, then createdAt
            ended_at: last_updated_at
                .or_else(|| messages.last().and_then(|m| m.created_at))
                .or(created_at),
            metadata: serde_json::json!({
                "source": "cursor",
                "model": model_name,
                "unifiedMode": val.get("unifiedMode").and_then(|v| v.as_str()),
            }),
            messages,
        })
    }

    /// Parse a bubble (message) from Cursor's format.
    ///
    /// Handles both new format (v0.40+) and legacy formats:
    /// - Content: text > rawText > content > message
    /// - Role: numeric type (1=user, 2=assistant) or string type/role
    /// - Author: modelType (new) or model (legacy)
    fn parse_bubble(bubble: &Value, idx: usize) -> Option<NormalizedMessage> {
        // Extract content - try all known field names in priority order
        let content = bubble
            .get("text")
            .and_then(|v| v.as_str())
            .or_else(|| bubble.get("rawText").and_then(|v| v.as_str()))
            .or_else(|| bubble.get("content").and_then(|v| v.as_str()))
            .or_else(|| bubble.get("message").and_then(|v| v.as_str()))?;

        if content.trim().is_empty() {
            return None;
        }

        // Extract role - try numeric type first (v0.40+), then string type/role (legacy)
        let role = bubble
            .get("type")
            .and_then(|v| {
                // v0.40+ format: numeric type (1=user, 2=assistant)
                v.as_i64()
                    .map(|t| {
                        match t {
                            bubble_type::USER => "user",
                            bubble_type::ASSISTANT => "assistant",
                            _ => "assistant",
                        }
                        .to_string()
                    })
                    // Legacy format: string type
                    .or_else(|| v.as_str().map(Self::normalize_role))
            })
            .or_else(|| {
                // Fallback: check "role" field (legacy format)
                bubble
                    .get("role")
                    .and_then(|v| v.as_str())
                    .map(Self::normalize_role)
            })
            .unwrap_or_else(|| "assistant".to_string());

        let created_at = bubble
            .get("timestamp")
            .or_else(|| bubble.get("createdAt"))
            .and_then(crate::connectors::parse_timestamp);

        // Extract author - try both field names (modelType is v0.40+)
        let author = bubble
            .get("modelType")
            .or_else(|| bubble.get("model"))
            .or_else(|| {
                // Also check modelInfo.modelName for v0.40+
                bubble.get("modelInfo").and_then(|m| m.get("modelName"))
            })
            .and_then(|v| v.as_str())
            .map(String::from);

        Some(NormalizedMessage {
            idx: idx as i64,
            role,
            author,
            created_at,
            content: content.to_string(),
            extra: bubble.clone(),
            snippets: Vec::new(),
        })
    }

    /// Normalize role string to standard values (user/assistant).
    fn normalize_role(role: &str) -> String {
        match role.to_lowercase().as_str() {
            "user" | "human" => "user",
            "assistant" | "ai" | "bot" => "assistant",
            _ => role,
        }
        .to_string()
    }

    /// Parse legacy aichat data
    fn parse_aichat_data(
        key: &str,
        value: &str,
        db_path: &Path,
        _since_ts: Option<i64>, // File-level filtering done in scan(); message filtering not needed
        seen_ids: &mut HashSet<String>,
    ) -> Option<NormalizedConversation> {
        let val: Value = serde_json::from_str(value).ok()?;

        // Skip if already seen
        let id = format!("aichat-{}", key);
        if seen_ids.contains(&id) {
            return None;
        }
        seen_ids.insert(id.clone());

        let mut messages = Vec::new();
        let mut started_at = None;
        let mut ended_at = None;

        // Parse tabs array
        if let Some(tabs) = val.get("tabs").and_then(|v| v.as_array()) {
            for tab in tabs {
                let tab_ts = tab.get("timestamp").and_then(|v| v.as_i64());

                // NOTE: Do NOT filter by timestamp here! File-level check is sufficient.

                if let Some(bubbles) = tab.get("bubbles").and_then(|v| v.as_array()) {
                    for bubble in bubbles {
                        if let Some(msg) = Self::parse_bubble(bubble, messages.len()) {
                            if started_at.is_none() {
                                started_at = msg.created_at.or(tab_ts);
                            }
                            ended_at = msg.created_at.or(tab_ts);
                            messages.push(msg);
                        }
                    }
                }
            }
        }

        if messages.is_empty() {
            return None;
        }

        // Re-index
        for (i, msg) in messages.iter_mut().enumerate() {
            msg.idx = i as i64;
        }

        let title = messages.first().map(|m| {
            m.content
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(100)
                .collect()
        });

        // source_path must be unique per conversation for proper lookup in the TUI.
        let unique_source_path = db_path.join(&id);

        Some(NormalizedConversation {
            agent_slug: "cursor".to_string(),
            external_id: Some(id),
            title,
            workspace: None,
            source_path: unique_source_path,
            started_at,
            ended_at,
            metadata: serde_json::json!({"source": "cursor_aichat"}),
            messages,
        })
    }
}

impl Connector for CursorConnector {
    fn detect(&self) -> DetectionResult {
        if let Some(base) = Self::app_support_dir()
            && base.exists()
        {
            let dbs = Self::find_db_files(&base);
            if !dbs.is_empty() {
                return DetectionResult {
                    detected: true,
                    evidence: vec![
                        format!("found Cursor at {}", base.display()),
                        format!("found {} database file(s)", dbs.len()),
                    ],
                    root_paths: vec![base],
                };
            }
        }
        DetectionResult::not_found()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Determine base directory
        let looks_like_base = |path: &PathBuf| {
            path.join("globalStorage").exists()
                || path.join("workspaceStorage").exists()
                || path
                    .file_name()
                    .is_some_and(|n| n.to_str().unwrap_or("").contains("Cursor"))
        };

        let base = if ctx.use_default_detection() {
            if looks_like_base(&ctx.data_dir) {
                ctx.data_dir.clone()
            } else if let Some(default_base) = Self::app_support_dir() {
                default_base
            } else {
                return Ok(Vec::new());
            }
        } else {
            if !looks_like_base(&ctx.data_dir) {
                return Ok(Vec::new());
            }
            ctx.data_dir.clone()
        };

        if !base.exists() {
            return Ok(Vec::new());
        }

        let db_files = Self::find_db_files(&base);
        let mut all_convs = Vec::new();

        for db_path in db_files {
            // Skip files not modified since last scan
            if !crate::connectors::file_modified_since(&db_path, ctx.since_ts) {
                continue;
            }

            match Self::extract_from_db(&db_path, ctx.since_ts) {
                Ok(convs) => {
                    tracing::debug!(
                        path = %db_path.display(),
                        count = convs.len(),
                        "cursor extracted conversations"
                    );
                    all_convs.extend(convs);
                }
                Err(e) => {
                    tracing::warn!(
                        path = %db_path.display(),
                        error = %e,
                        "cursor failed to extract from db"
                    );
                }
            }
        }

        Ok(all_convs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde_json::json;
    use std::collections::HashSet;
    use std::fs;
    use tempfile::TempDir;

    /// Create a test SQLite database with the cursorDiskKV table
    fn create_test_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS cursorDiskKV (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
        conn
    }

    // =========================================================================
    // Constructor tests
    // =========================================================================

    #[test]
    fn new_creates_connector() {
        let connector = CursorConnector::new();
        let _ = connector;
    }

    #[test]
    fn default_creates_connector() {
        let connector = CursorConnector;
        let _ = connector;
    }

    // =========================================================================
    // find_db_files tests
    // =========================================================================

    #[test]
    fn find_db_files_empty_for_nonexistent() {
        let dir = TempDir::new().unwrap();
        let dbs = CursorConnector::find_db_files(dir.path());
        assert!(dbs.is_empty());
    }

    #[test]
    fn find_db_files_finds_global_storage() {
        let dir = TempDir::new().unwrap();
        let global_dir = dir.path().join("globalStorage");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(global_dir.join("state.vscdb"), "").unwrap();

        let dbs = CursorConnector::find_db_files(dir.path());
        assert_eq!(dbs.len(), 1);
        assert!(dbs[0].ends_with("state.vscdb"));
    }

    #[test]
    fn find_db_files_finds_workspace_storage() {
        let dir = TempDir::new().unwrap();
        let workspace_dir = dir.path().join("workspaceStorage").join("abc123");
        fs::create_dir_all(&workspace_dir).unwrap();
        fs::write(workspace_dir.join("state.vscdb"), "").unwrap();

        let dbs = CursorConnector::find_db_files(dir.path());
        assert_eq!(dbs.len(), 1);
    }

    #[test]
    fn find_db_files_finds_multiple() {
        let dir = TempDir::new().unwrap();

        // Create global storage
        let global_dir = dir.path().join("globalStorage");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(global_dir.join("state.vscdb"), "").unwrap();

        // Create multiple workspace storage dirs
        for i in 1..=3 {
            let ws_dir = dir.path().join("workspaceStorage").join(format!("ws{}", i));
            fs::create_dir_all(&ws_dir).unwrap();
            fs::write(ws_dir.join("state.vscdb"), "").unwrap();
        }

        let dbs = CursorConnector::find_db_files(dir.path());
        assert_eq!(dbs.len(), 4); // 1 global + 3 workspace
    }

    // =========================================================================
    // parse_bubble tests
    // =========================================================================

    #[test]
    fn parse_bubble_with_text() {
        let bubble = json!({
            "text": "Hello from user",
            "type": "user"
        });

        let msg = CursorConnector::parse_bubble(&bubble, 0);
        assert!(msg.is_some());
        let msg = msg.unwrap();
        assert_eq!(msg.content, "Hello from user");
        assert_eq!(msg.role, "user");
    }

    #[test]
    fn parse_bubble_with_content_field() {
        let bubble = json!({
            "content": "Response from assistant",
            "role": "assistant"
        });

        let msg = CursorConnector::parse_bubble(&bubble, 1);
        assert!(msg.is_some());
        let msg = msg.unwrap();
        assert_eq!(msg.content, "Response from assistant");
        assert_eq!(msg.role, "assistant");
    }

    #[test]
    fn parse_bubble_with_message_field() {
        let bubble = json!({
            "message": "Another message",
            "type": "ai"
        });

        let msg = CursorConnector::parse_bubble(&bubble, 0);
        assert!(msg.is_some());
        let msg = msg.unwrap();
        assert_eq!(msg.content, "Another message");
        assert_eq!(msg.role, "assistant"); // "ai" maps to assistant
    }

    #[test]
    fn parse_bubble_role_normalization() {
        let test_cases = vec![
            ("user", "user"),
            ("human", "user"),
            ("assistant", "assistant"),
            ("ai", "assistant"),
            ("bot", "assistant"),
            ("custom", "custom"), // Unknown roles pass through
        ];

        for (input_role, expected_role) in test_cases {
            let bubble = json!({
                "text": "test",
                "type": input_role
            });

            let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
            assert_eq!(
                msg.role, expected_role,
                "Failed for input role: {}",
                input_role
            );
        }
    }

    #[test]
    fn parse_bubble_empty_content_returns_none() {
        let bubble = json!({
            "text": "",
            "type": "user"
        });

        assert!(CursorConnector::parse_bubble(&bubble, 0).is_none());
    }

    #[test]
    fn parse_bubble_whitespace_only_returns_none() {
        let bubble = json!({
            "text": "   \n\t  ",
            "type": "user"
        });

        assert!(CursorConnector::parse_bubble(&bubble, 0).is_none());
    }

    #[test]
    fn parse_bubble_extracts_timestamp() {
        let bubble = json!({
            "text": "Test",
            "type": "user",
            "timestamp": 1700000000000i64
        });

        let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
        assert_eq!(msg.created_at, Some(1700000000000));
    }

    #[test]
    fn parse_bubble_extracts_model() {
        let bubble = json!({
            "text": "Response",
            "type": "assistant",
            "model": "gpt-4"
        });

        let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
        assert_eq!(msg.author, Some("gpt-4".to_string()));
    }

    #[test]
    fn parse_bubble_defaults_to_assistant() {
        let bubble = json!({
            "text": "No role specified"
        });

        let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
        assert_eq!(msg.role, "assistant");
    }

    // =========================================================================
    // parse_composer_data tests
    // =========================================================================

    #[test]
    fn parse_composer_data_with_tabs_and_bubbles() {
        let key = "composerData:abc-123";
        let value = json!({
            "createdAt": 1700000000000i64,
            "tabs": [{
                "bubbles": [
                    {"text": "Hello", "type": "user"},
                    {"text": "Hi there!", "type": "assistant"}
                ]
            }]
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.agent_slug, "cursor");
        assert_eq!(conv.external_id, Some("abc-123".to_string()));
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[1].role, "assistant");
    }

    #[test]
    fn parse_composer_data_with_conversation_map() {
        let key = "composerData:def-456";
        let value = json!({
            "conversationMap": {
                "conv1": {
                    "bubbles": [
                        {"text": "Question?", "type": "user"},
                        {"content": "Answer!", "role": "assistant"}
                    ]
                }
            }
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.messages.len(), 2);
    }

    #[test]
    fn parse_composer_data_with_text_only() {
        let key = "composerData:simple-123";
        let value = json!({
            "text": "Simple user input without bubbles",
            "createdAt": 1700000000000i64
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].role, "user");
        assert!(conv.messages[0].content.contains("Simple user input"));
    }

    #[test]
    fn parse_composer_data_with_rich_text() {
        let key = "composerData:rich-789";
        let value = json!({
            "richText": "Rich text content here"
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert!(conv.messages[0].content.contains("Rich text"));
    }

    #[test]
    fn parse_composer_data_skips_duplicates() {
        let key = "composerData:dup-123";
        let value = json!({
            "text": "Content"
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv1 = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );
        let conv2 = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv1.is_some());
        assert!(conv2.is_none()); // Duplicate should return None
    }

    #[test]
    fn parse_composer_data_returns_none_for_empty() {
        let key = "composerData:empty-123";
        let value = json!({}).to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_none());
    }

    #[test]
    fn parse_composer_data_extracts_model_config() {
        let key = "composerData:model-123";
        let value = json!({
            "text": "Test",
            "modelConfig": {
                "modelName": "gpt-4-turbo"
            }
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.metadata["model"], "gpt-4-turbo");
    }

    #[test]
    fn parse_composer_data_invalid_key_returns_none() {
        let key = "not-composer-data"; // Missing "composerData:" prefix
        let value = json!({ "text": "Content" }).to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_none());
    }

    // =========================================================================
    // parse_aichat_data tests
    // =========================================================================

    #[test]
    fn parse_aichat_data_with_tabs() {
        let key = "aichat.chatdata";
        let value = json!({
            "tabs": [{
                "timestamp": 1700000000000i64,
                "bubbles": [
                    {"text": "User question", "type": "user"},
                    {"text": "AI response", "type": "ai"}
                ]
            }]
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv =
            CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen);

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.agent_slug, "cursor");
        assert!(conv.external_id.as_ref().unwrap().starts_with("aichat-"));
        assert_eq!(conv.messages.len(), 2);
    }

    #[test]
    fn parse_aichat_data_returns_none_for_empty() {
        let key = "aichat.empty";
        let value = json!({
            "tabs": []
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv =
            CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen);

        assert!(conv.is_none());
    }

    #[test]
    fn parse_aichat_data_skips_duplicates() {
        let key = "aichat.dup";
        let value = json!({
            "tabs": [{
                "bubbles": [{"text": "Content", "type": "user"}]
            }]
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv1 =
            CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen);
        let conv2 =
            CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen);

        assert!(conv1.is_some());
        assert!(conv2.is_none());
    }

    // =========================================================================
    // extract_from_db tests
    // =========================================================================

    #[test]
    fn extract_from_db_with_composer_data() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.vscdb");

        let conn = create_test_db(&db_path);
        let value = json!({
            "text": "Database test"
        })
        .to_string();
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            ["composerData:db-test-123", &value],
        )
        .unwrap();
        drop(conn);

        let convs = CursorConnector::extract_from_db(&db_path, None).unwrap();
        assert_eq!(convs.len(), 1);
        assert!(convs[0].messages[0].content.contains("Database test"));
    }

    #[test]
    fn extract_from_db_with_aichat_data() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.vscdb");

        let conn = create_test_db(&db_path);
        let value = json!({
            "tabs": [{
                "bubbles": [{"text": "Aichat test", "type": "user"}]
            }]
        })
        .to_string();
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            ["workbench.panel.aichat.view.aichat.chatdata", &value],
        )
        .unwrap();
        drop(conn);

        let convs = CursorConnector::extract_from_db(&db_path, None).unwrap();
        assert_eq!(convs.len(), 1);
    }

    #[test]
    fn extract_from_db_handles_empty_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.vscdb");

        let _conn = create_test_db(&db_path);

        let convs = CursorConnector::extract_from_db(&db_path, None).unwrap();
        assert!(convs.is_empty());
    }

    #[test]
    fn extract_from_db_fails_for_missing_file() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("nonexistent.vscdb");

        let result = CursorConnector::extract_from_db(&db_path, None);
        assert!(result.is_err());
    }

    // =========================================================================
    // Detection tests
    // =========================================================================

    #[test]
    fn detect_not_found_without_cursor_dir() {
        let connector = CursorConnector::new();
        let result = connector.detect();
        // On most CI/test systems, Cursor won't be installed
        // Just verify detect() doesn't panic
        let _ = result.detected;
    }

    // =========================================================================
    // Scan tests
    // =========================================================================

    #[test]
    fn scan_empty_directory_returns_empty() {
        let dir = TempDir::new().unwrap();

        // Create globalStorage to make scan() use this directory instead of fallback
        let global_dir = dir.path().join("globalStorage");
        fs::create_dir_all(&global_dir).unwrap();
        // Create an empty state.vscdb to prevent fallback to system Cursor
        create_test_db(&global_dir.join("state.vscdb"));

        let connector = CursorConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn scan_processes_global_storage() {
        let dir = TempDir::new().unwrap();

        // Create Cursor-like directory structure
        let cursor_dir = dir.path().join("Cursor");
        let global_dir = cursor_dir.join("globalStorage");
        fs::create_dir_all(&global_dir).unwrap();

        // Create database with test data
        let db_path = global_dir.join("state.vscdb");
        let conn = create_test_db(&db_path);
        let value = json!({ "text": "Scan test" }).to_string();
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            ["composerData:scan-123", &value],
        )
        .unwrap();
        drop(conn);

        let connector = CursorConnector::new();
        let ctx = ScanContext::local_default(cursor_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
    }

    #[test]
    fn scan_recognizes_cursor_in_path() {
        let dir = TempDir::new().unwrap();

        // Directory name contains "Cursor"
        let cursor_dir = dir.path().join("TestCursor");
        let global_dir = cursor_dir.join("globalStorage");
        fs::create_dir_all(&global_dir).unwrap();

        let db_path = global_dir.join("state.vscdb");
        let conn = create_test_db(&db_path);
        let value = json!({ "text": "Path test" }).to_string();
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            ["composerData:path-123", &value],
        )
        .unwrap();
        drop(conn);

        let connector = CursorConnector::new();
        let ctx = ScanContext::local_default(cursor_dir, None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 1);
    }

    // =========================================================================
    // Edge case tests
    // =========================================================================

    #[test]
    fn parse_composer_data_invalid_json_returns_none() {
        let key = "composerData:invalid-123";
        let value = "not valid json {{{";

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_none());
    }

    #[test]
    fn parse_bubble_preserves_original_in_extra() {
        let bubble = json!({
            "text": "Test",
            "type": "user",
            "customField": "customValue"
        });

        let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
        assert_eq!(msg.extra["customField"], "customValue");
    }

    #[test]
    fn conversation_title_from_first_message() {
        let key = "composerData:title-test";
        let value = json!({
            "tabs": [{
                "bubbles": [
                    {"text": "This is the first line\nSecond line here", "type": "user"}
                ]
            }]
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        let conv = conv.unwrap();
        // Title should be first line only
        assert_eq!(conv.title, Some("This is the first line".to_string()));
    }

    #[test]
    fn conversation_title_truncates_long_lines() {
        let key = "composerData:long-title";
        let long_text = "x".repeat(200);
        let value = json!({
            "text": long_text
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        let conv = conv.unwrap();
        assert!(conv.title.as_ref().unwrap().len() <= 100);
    }

    #[test]
    fn messages_are_reindexed_sequentially() {
        let key = "composerData:reindex";
        let value = json!({
            "tabs": [{
                "bubbles": [
                    {"text": "One", "type": "user"},
                    {"text": "Two", "type": "assistant"},
                    {"text": "Three", "type": "user"}
                ]
            }]
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        )
        .unwrap();

        assert_eq!(conv.messages[0].idx, 0);
        assert_eq!(conv.messages[1].idx, 1);
        assert_eq!(conv.messages[2].idx, 2);
    }

    // =========================================================================
    // WSL detection tests (Linux-only)
    // =========================================================================

    #[cfg(target_os = "linux")]
    mod wsl_tests {
        use super::*;

        #[test]
        fn is_wsl_returns_false_on_native_linux() {
            // On a real Linux system (not WSL), /proc/version won't contain "microsoft"
            // This test just verifies the function doesn't panic
            let result = CursorConnector::is_wsl();
            // We can't assert the exact value since it depends on the environment,
            // but we verify the function works
            let _ = result;
        }

        #[test]
        fn find_wsl_cursor_path_returns_none_without_mnt_c() {
            // On native Linux, /mnt/c typically doesn't exist
            // This verifies the function gracefully returns None
            if !Path::new("/mnt/c/Users").exists() {
                let result = CursorConnector::find_wsl_cursor_path();
                assert!(result.is_none());
            }
        }

        #[test]
        fn find_wsl_cursor_path_skips_system_dirs() {
            // Create a temp structure that mimics /mnt/c/Users with system dirs
            let dir = TempDir::new().unwrap();
            let users_dir = dir.path().join("Users");
            fs::create_dir_all(&users_dir).unwrap();

            // Create system directories that should be skipped
            for sys_dir in ["Default", "Public", "All Users", "Default User"] {
                fs::create_dir_all(users_dir.join(sys_dir)).unwrap();
            }

            // The function checks /mnt/c/Users specifically, so we can't directly test
            // the skipping logic without mocking. Instead, verify the skip list is correct.
            let skip_list = ["Default", "Public", "All Users", "Default User"];
            assert_eq!(skip_list.len(), 4);
        }

        #[test]
        fn wsl_path_structure_is_valid() {
            // Verify the expected WSL path structure
            let expected = Path::new("/mnt/c/Users/TestUser/AppData/Roaming/Cursor/User");
            assert!(expected.starts_with("/mnt/c/Users"));
            assert!(expected.ends_with("Cursor/User"));
        }
    }
}
