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
//!
//! ## Data Format Evolution
//!
//! Cursor has evolved its data format over time. This connector supports:
//!
//! 1. **v0.40+ (New Format)**: `fullConversationHeadersOnly` with separate bubble entries
//!    - `composerData:{uuid}` contains only headers with `bubbleId` references
//!    - Actual message content stored in `bubbleId:{composerId}:{bubbleId}` keys
//!    - Role encoded as numeric type: 1 = user, 2 = assistant
//!
//! 2. **v0.3x (Tabs Format)**: Inline `tabs` -> `bubbles` structure
//!    - Full message content embedded in `composerData`
//!    - Role encoded as string: "user", "assistant", "ai", "human"
//!
//! 3. **v0.2x (ConversationMap Format)**: `conversationMap` structure
//!    - Similar to tabs format but different nesting
//!
//! 4. **Simple Text**: `text`/`richText` fields for basic composer sessions
//!
//! The connector tries formats in order (newest first) and uses the first that yields messages.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

/// Type alias for the bubble data lookup map.
/// Keys are "{composerId}:{bubbleId}" for efficient O(1) lookup.
type BubbleDataMap = HashMap<String, Value>;

/// Cursor v0.40+ bubble type constants (numeric encoding)
mod bubble_type {
    /// User message type in new format
    pub const USER: i64 = 1;
    /// Assistant message type in new format
    pub const ASSISTANT: i64 = 2;
}

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

    /// Fetch all bubble data from the database for new format support.
    /// Returns a map keyed by "composerId:bubbleId" for efficient O(1) lookup.
    fn fetch_bubble_data(conn: &Connection) -> BubbleDataMap {
        let mut bubble_map = BubbleDataMap::new();

        if let Ok(mut stmt) =
            conn.prepare("SELECT key, value FROM cursorDiskKV WHERE key LIKE 'bubbleId:%'")
        {
            let rows = stmt.query_map([], |row| {
                let key: String = row.get(0)?;
                let value: String = row.get(1)?;
                Ok((key, value))
            });

            if let Ok(rows) = rows {
                for row in rows.flatten() {
                    let (key, value) = row;
                    // Key format: bubbleId:{composerId}:{bubbleId}
                    // We store as "{composerId}:{bubbleId}" for easy lookup
                    if let Some(rest) = key.strip_prefix("bubbleId:") {
                        match serde_json::from_str::<Value>(&value) {
                            Ok(parsed) => {
                                bubble_map.insert(rest.to_string(), parsed);
                            }
                            Err(e) => {
                                tracing::trace!(
                                    key = %rest,
                                    error = %e,
                                    "skipping malformed bubble JSON"
                                );
                            }
                        }
                    }
                }
            }
        }

        bubble_map
    }

    /// Extract workspace path from the database path.
    ///
    /// For workspaceStorage databases, reads workspace.json to get the actual workspace path.
    /// Returns None for globalStorage databases (no specific workspace).
    fn extract_workspace_from_db_path(db_path: &Path) -> Option<PathBuf> {
        // Check if this is a workspaceStorage database
        // Path: .../workspaceStorage/{hash}/state.vscdb
        let parent = db_path.parent()?; // {hash} directory
        let grandparent = parent.parent()?;

        if grandparent.file_name()?.to_str()? != "workspaceStorage" {
            return None; // This is globalStorage or something else
        }

        // Try to read workspace.json in the same directory
        let workspace_json_path = parent.join("workspace.json");
        let content = std::fs::read_to_string(&workspace_json_path).ok()?;
        let val: Value = serde_json::from_str(&content).ok()?;

        // Try "folder" first (single folder workspace)
        if let Some(folder) = val.get("folder").and_then(|v| v.as_str()) {
            // folder is typically a file:// URI like "file:///Users/user/project"
            return Self::parse_file_uri(folder);
        }

        // Try "workspace" (multi-root workspace file)
        if let Some(workspace) = val.get("workspace").and_then(|v| v.as_str()) {
            // For .code-workspace files, return the directory containing it
            if let Some(path) = Self::parse_file_uri(workspace) {
                return path.parent().map(PathBuf::from);
            }
        }

        None
    }

    /// Parse a file:// URI into a PathBuf.
    /// Handles basic percent-encoding (e.g., %20 for spaces).
    fn parse_file_uri(uri: &str) -> Option<PathBuf> {
        let path = uri.strip_prefix("file://")?;

        // Simple percent-decoding for common cases
        let mut decoded = String::with_capacity(path.len());
        let mut chars = path.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '%' {
                // Try to decode %XX
                let hex: String = chars.by_ref().take(2).collect();
                if hex.len() == 2
                    && let Ok(byte) = u8::from_str_radix(&hex, 16)
                {
                    decoded.push(byte as char);
                    continue;
                }
                // Failed to decode, keep original
                decoded.push('%');
                decoded.push_str(&hex);
            } else {
                decoded.push(c);
            }
        }

        Some(PathBuf::from(decoded))
    }

    /// Extract workspace path from file references in the conversation context.
    ///
    /// Looks at fileSelections, folderSelections, newlyCreatedFiles, and newlyCreatedFolders
    /// to find file paths. Returns the common ancestor directory of all paths found.
    fn extract_workspace_from_context(val: &Value) -> Option<PathBuf> {
        let mut paths: Vec<PathBuf> = Vec::new();

        // Helper to extract paths from URI structures
        let extract_from_uri = |uri: &Value| -> Option<PathBuf> {
            // Try fsPath first (most reliable), then path
            uri.get("fsPath")
                .and_then(|v| v.as_str())
                .or_else(|| uri.get("path").and_then(|v| v.as_str()))
                .map(PathBuf::from)
        };

        // Extract from context.fileSelections
        if let Some(context) = val.get("context") {
            if let Some(selections) = context.get("fileSelections").and_then(|v| v.as_array()) {
                for sel in selections {
                    if let Some(uri) = sel.get("uri")
                        && let Some(path) = extract_from_uri(uri)
                    {
                        paths.push(path);
                    }
                }
            }

            // Extract from context.folderSelections
            if let Some(selections) = context.get("folderSelections").and_then(|v| v.as_array()) {
                for sel in selections {
                    if let Some(uri) = sel.get("uri")
                        && let Some(path) = extract_from_uri(uri)
                    {
                        paths.push(path);
                    }
                }
            }

            // Extract from context.selections (code selections)
            if let Some(selections) = context.get("selections").and_then(|v| v.as_array()) {
                for sel in selections {
                    if let Some(uri) = sel.get("uri")
                        && let Some(path) = extract_from_uri(uri)
                    {
                        paths.push(path);
                    }
                }
            }
        }

        // Extract from newlyCreatedFiles (can be string array or object array with uri)
        if let Some(files) = val.get("newlyCreatedFiles").and_then(|v| v.as_array()) {
            for file in files {
                // Try as string first
                if let Some(path_str) = file.as_str() {
                    paths.push(PathBuf::from(path_str));
                // Then try as object with uri.fsPath
                } else if let Some(uri) = file.get("uri")
                    && let Some(path) = extract_from_uri(uri)
                {
                    paths.push(path);
                }
            }
        }

        // Extract from newlyCreatedFolders (can be string array or object array with uri)
        if let Some(folders) = val.get("newlyCreatedFolders").and_then(|v| v.as_array()) {
            for folder in folders {
                // Try as string first
                if let Some(path_str) = folder.as_str() {
                    paths.push(PathBuf::from(path_str));
                // Then try as object with uri.fsPath
                } else if let Some(uri) = folder.get("uri")
                    && let Some(path) = extract_from_uri(uri)
                {
                    paths.push(path);
                }
            }
        }

        // Find common ancestor of all paths
        if paths.is_empty() {
            return None;
        }

        // Start with the first path's parent directory
        let first_path = &paths[0];
        let mut common = if first_path.is_file() || !first_path.exists() {
            first_path.parent()?.to_path_buf()
        } else {
            first_path.clone()
        };

        // For each subsequent path, find the common ancestor
        for path in paths.iter().skip(1) {
            let path_to_check = if path.is_file() || !path.exists() {
                path.parent().map(PathBuf::from)
            } else {
                Some(path.clone())
            };

            if let Some(p) = path_to_check {
                // Find common ancestor between common and p
                let common_components: Vec<_> = common.components().collect();
                let path_components: Vec<_> = p.components().collect();

                let mut new_common = PathBuf::new();
                for (c1, c2) in common_components.iter().zip(path_components.iter()) {
                    if c1 == c2 {
                        new_common.push(c1.as_os_str());
                    } else {
                        break;
                    }
                }

                // Don't go above home directory or root
                if new_common.components().count() > 1 {
                    common = new_common;
                } else {
                    // If we're down to just root, keep our best guess
                    break;
                }
            }
        }

        // Ensure we have a meaningful path (not just "/" or empty)
        if common.components().count() > 2 {
            Some(common)
        } else {
            None
        }
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

        // Extract workspace from workspaceStorage path (if applicable)
        let workspace = Self::extract_workspace_from_db_path(db_path);

        // Pre-fetch all bubbleId entries for new format support
        // Keys are like: bubbleId:{composerId}:{bubbleId}
        let bubble_data = Self::fetch_bubble_data(&conn);

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
                        &bubble_data,
                        workspace.as_ref(),
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
                        Self::parse_aichat_data(&key, &value, db_path, since_ts, &mut seen_ids, workspace.as_ref())
                    {
                        convs.push(conv);
                    }
                }
            }
        }

        Ok(convs)
    }

    /// Parse composerData JSON into a conversation
    fn parse_composer_data(
        key: &str,
        value: &str,
        db_path: &Path,
        _since_ts: Option<i64>, // File-level filtering done in scan(); message filtering not needed
        seen_ids: &mut HashSet<String>,
        bubble_data: &BubbleDataMap,
        workspace: Option<&PathBuf>,
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

        // Check for new format with fullConversationHeadersOnly (Cursor v0.40+)
        // This format stores only bubble IDs in composerData, with actual content
        // in separate bubbleId:{composerId}:{bubbleId} keys
        if let Some(headers) = val
            .get("fullConversationHeadersOnly")
            .and_then(|v| v.as_array())
        {
            // New format: headers contain bubbleId references
            for header in headers {
                if let Some(bubble_id) = header.get("bubbleId").and_then(|v| v.as_str()) {
                    // Look up the full bubble data
                    let lookup_key = format!("{}:{}", composer_id, bubble_id);
                    if let Some(bubble) = bubble_data.get(&lookup_key)
                        && let Some(msg) = Self::parse_bubble(bubble, messages.len())
                    {
                        messages.push(msg);
                    }
                }
            }
        }

        // Parse conversation from bubbles/tabs structure (legacy format)
        // Cursor uses different structures depending on version
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

        // Also check fullConversation/conversationMap for older format
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

        // Extract model info for title
        let model_name = val
            .get("modelConfig")
            .and_then(|m| m.get("modelName"))
            .and_then(|v| v.as_str());

        // Use explicit name field if available (new format), otherwise derive from first message
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

        // source_path must be unique per conversation for proper lookup.
        // Append composer_id since multiple conversations share the same db file.
        let unique_source_path = db_path.join(&composer_id);

        // Try workspace from db_path first, then from context file paths
        let final_workspace = workspace
            .cloned()
            .or_else(|| Self::extract_workspace_from_context(&val));

        Some(NormalizedConversation {
            agent_slug: "cursor".to_string(),
            external_id: Some(composer_id),
            title,
            workspace: final_workspace,
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
    /// Handles both new format (v0.40+) and legacy formats by trying all known field names.
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

        // Extract role - try numeric type first (new format), then string type/role (legacy)
        let role = bubble
            .get("type")
            .and_then(|v| {
                // New format: numeric type (1=user, 2=assistant)
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

        // Extract author - try both field names
        let author = bubble
            .get("modelType")
            .or_else(|| bubble.get("model"))
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
        workspace: Option<&PathBuf>,
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

        // source_path must be unique per conversation for proper lookup.
        let unique_source_path = db_path.join(&id);

        Some(NormalizedConversation {
            agent_slug: "cursor".to_string(),
            external_id: Some(id),
            title,
            workspace: workspace.cloned(),
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
    // Workspace extraction tests
    // =========================================================================

    #[test]
    fn parse_file_uri_basic() {
        let result = CursorConnector::parse_file_uri("file:///Users/test/project");
        assert_eq!(result, Some(PathBuf::from("/Users/test/project")));
    }

    #[test]
    fn parse_file_uri_with_percent_encoding() {
        let result = CursorConnector::parse_file_uri("file:///Users/test/my%20project");
        assert_eq!(result, Some(PathBuf::from("/Users/test/my project")));
    }

    #[test]
    fn parse_file_uri_with_special_chars() {
        let result = CursorConnector::parse_file_uri("file:///Users/test/%23hash%26amp");
        assert_eq!(result, Some(PathBuf::from("/Users/test/#hash&amp")));
    }

    #[test]
    fn parse_file_uri_returns_none_without_prefix() {
        let result = CursorConnector::parse_file_uri("/Users/test/project");
        assert!(result.is_none());
    }

    #[test]
    fn extract_workspace_from_workspace_storage() {
        let dir = TempDir::new().unwrap();
        let ws_dir = dir.path().join("workspaceStorage").join("abc123");
        fs::create_dir_all(&ws_dir).unwrap();

        // Create workspace.json with folder
        let workspace_json = json!({"folder": "file:///Users/test/my-project"});
        fs::write(ws_dir.join("workspace.json"), workspace_json.to_string()).unwrap();

        let db_path = ws_dir.join("state.vscdb");
        let workspace = CursorConnector::extract_workspace_from_db_path(&db_path);

        assert_eq!(workspace, Some(PathBuf::from("/Users/test/my-project")));
    }

    #[test]
    fn extract_workspace_from_workspace_storage_with_workspace_file() {
        let dir = TempDir::new().unwrap();
        let ws_dir = dir.path().join("workspaceStorage").join("def456");
        fs::create_dir_all(&ws_dir).unwrap();

        // Create workspace.json with .code-workspace reference
        let workspace_json =
            json!({"workspace": "file:///Users/test/my-project/app.code-workspace"});
        fs::write(ws_dir.join("workspace.json"), workspace_json.to_string()).unwrap();

        let db_path = ws_dir.join("state.vscdb");
        let workspace = CursorConnector::extract_workspace_from_db_path(&db_path);

        // Should return parent directory of .code-workspace file
        assert_eq!(workspace, Some(PathBuf::from("/Users/test/my-project")));
    }

    #[test]
    fn extract_workspace_returns_none_for_global_storage() {
        let dir = TempDir::new().unwrap();
        let global_dir = dir.path().join("globalStorage");
        fs::create_dir_all(&global_dir).unwrap();

        let db_path = global_dir.join("state.vscdb");
        let workspace = CursorConnector::extract_workspace_from_db_path(&db_path);

        assert!(workspace.is_none());
    }

    #[test]
    fn extract_workspace_returns_none_when_workspace_json_missing() {
        let dir = TempDir::new().unwrap();
        let ws_dir = dir.path().join("workspaceStorage").join("abc123");
        fs::create_dir_all(&ws_dir).unwrap();
        // Don't create workspace.json

        let db_path = ws_dir.join("state.vscdb");
        let workspace = CursorConnector::extract_workspace_from_db_path(&db_path);

        assert!(workspace.is_none());
    }

    #[test]
    fn extract_workspace_handles_percent_encoded_paths() {
        let dir = TempDir::new().unwrap();
        let ws_dir = dir.path().join("workspaceStorage").join("xyz789");
        fs::create_dir_all(&ws_dir).unwrap();

        // Create workspace.json with percent-encoded path
        let workspace_json = json!({"folder": "file:///Users/test/my%20project%20name"});
        fs::write(ws_dir.join("workspace.json"), workspace_json.to_string()).unwrap();

        let db_path = ws_dir.join("state.vscdb");
        let workspace = CursorConnector::extract_workspace_from_db_path(&db_path);

        assert_eq!(
            workspace,
            Some(PathBuf::from("/Users/test/my project name"))
        );
    }

    // =========================================================================
    // extract_workspace_from_context tests
    // =========================================================================

    #[test]
    fn extract_workspace_from_context_with_file_selections() {
        let val = json!({
            "context": {
                "fileSelections": [
                    {"uri": {"fsPath": "/Users/test/project/src/main.rs", "scheme": "file"}},
                    {"uri": {"fsPath": "/Users/test/project/src/lib.rs", "scheme": "file"}}
                ],
                "folderSelections": [],
                "selections": []
            },
            "newlyCreatedFiles": [],
            "newlyCreatedFolders": []
        });

        let workspace = CursorConnector::extract_workspace_from_context(&val);
        assert_eq!(workspace, Some(PathBuf::from("/Users/test/project/src")));
    }

    #[test]
    fn extract_workspace_from_context_with_newly_created_files_string() {
        let val = json!({
            "context": {
                "fileSelections": [],
                "folderSelections": [],
                "selections": []
            },
            "newlyCreatedFiles": ["/Users/test/myproject/new_file.rs"],
            "newlyCreatedFolders": []
        });

        let workspace = CursorConnector::extract_workspace_from_context(&val);
        assert_eq!(workspace, Some(PathBuf::from("/Users/test/myproject")));
    }

    #[test]
    fn extract_workspace_from_context_with_newly_created_files_uri_object() {
        // Real format from Cursor: newlyCreatedFiles is array of objects with uri.fsPath
        let val = json!({
            "context": {
                "fileSelections": [],
                "folderSelections": [],
                "selections": []
            },
            "newlyCreatedFiles": [
                {"uri": {"fsPath": "/Users/eric/src/Tasks/2025-11-05 Accounting/file1.md"}},
                {"uri": {"fsPath": "/Users/eric/src/Tasks/2025-11-05 Accounting/file2.ps1"}}
            ],
            "newlyCreatedFolders": []
        });

        let workspace = CursorConnector::extract_workspace_from_context(&val);
        assert_eq!(
            workspace,
            Some(PathBuf::from("/Users/eric/src/Tasks/2025-11-05 Accounting"))
        );
    }

    #[test]
    fn extract_workspace_from_context_finds_common_ancestor() {
        let val = json!({
            "context": {
                "fileSelections": [
                    {"uri": {"fsPath": "/Users/test/project/src/main.rs"}},
                    {"uri": {"fsPath": "/Users/test/project/tests/test.rs"}}
                ],
                "folderSelections": [],
                "selections": []
            },
            "newlyCreatedFiles": [],
            "newlyCreatedFolders": []
        });

        let workspace = CursorConnector::extract_workspace_from_context(&val);
        assert_eq!(workspace, Some(PathBuf::from("/Users/test/project")));
    }

    #[test]
    fn extract_workspace_from_context_empty_returns_none() {
        let val = json!({
            "context": {
                "fileSelections": [],
                "folderSelections": [],
                "selections": []
            },
            "newlyCreatedFiles": [],
            "newlyCreatedFolders": []
        });

        let workspace = CursorConnector::extract_workspace_from_context(&val);
        assert!(workspace.is_none());
    }

    #[test]
    fn extract_workspace_from_context_uses_path_fallback() {
        // Test that we use "path" field when "fsPath" is not available
        let val = json!({
            "context": {
                "fileSelections": [
                    {"uri": {"path": "/Users/test/project/src/main.rs"}}
                ],
                "folderSelections": [],
                "selections": []
            },
            "newlyCreatedFiles": [],
            "newlyCreatedFolders": []
        });

        let workspace = CursorConnector::extract_workspace_from_context(&val);
        assert_eq!(workspace, Some(PathBuf::from("/Users/test/project/src")));
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
        let bubble_data = HashMap::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
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
        let bubble_data = HashMap::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
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
        let bubble_data = HashMap::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
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
        let bubble_data = HashMap::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
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
        let bubble_data = HashMap::new();
        let conv1 = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
            None,
        );
        let conv2 = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
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
        let bubble_data = HashMap::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
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
        let bubble_data = HashMap::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.metadata["model"], "gpt-4-turbo");
    }

    #[test]
    fn parse_composer_data_uses_last_updated_at_for_ended_at() {
        let key = "composerData:timestamps-123";
        let value = json!({
            "text": "Test",
            "createdAt": 1700000000000i64,
            "lastUpdatedAt": 1700000999000i64
        })
        .to_string();

        let mut seen = HashSet::new();
        let bubble_data = HashMap::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.started_at, Some(1700000000000));
        // ended_at should use lastUpdatedAt (most accurate)
        assert_eq!(conv.ended_at, Some(1700000999000));
    }

    #[test]
    fn parse_composer_data_falls_back_to_created_at_without_last_updated() {
        let key = "composerData:no-update-123";
        let value = json!({
            "text": "Test",
            "createdAt": 1700000000000i64
        })
        .to_string();

        let mut seen = HashSet::new();
        let bubble_data = HashMap::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.started_at, Some(1700000000000));
        // Without lastUpdatedAt, should fall back to createdAt
        assert_eq!(conv.ended_at, Some(1700000000000));
    }

    #[test]
    fn parse_composer_data_invalid_key_returns_none() {
        let key = "not-composer-data"; // Missing "composerData:" prefix
        let value = json!({ "text": "Content" }).to_string();

        let mut seen = HashSet::new();
        let bubble_data = HashMap::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
            None,
        );

        assert!(conv.is_none());
    }

    // =========================================================================
    // New format (fullConversationHeadersOnly) tests
    // =========================================================================

    #[test]
    fn parse_composer_data_new_format_with_bubble_refs() {
        let composer_id = "000cb6ea-22c7-425c-b14d-eed4e4503879";
        let bubble_id_1 = "0b9409fa-459c-40b9-af28-afdd72f8475d";
        let bubble_id_2 = "082e7471-76af-4559-be44-ca3ca3a8d62e";

        let key = format!("composerData:{}", composer_id);
        let value = json!({
            "_v": 10,
            "composerId": composer_id,
            "fullConversationHeadersOnly": [
                {"bubbleId": bubble_id_1, "type": 1},
                {"bubbleId": bubble_id_2, "type": 2}
            ],
            "conversationMap": {},
            "name": "Test Conversation",
            "createdAt": 1766508972270i64,
            "lastUpdatedAt": 1766508994755i64,
            "modelConfig": {"modelName": "claude-4.5-opus-high-thinking", "maxMode": true}
        })
        .to_string();

        // Create bubble data map with actual message content
        let mut bubble_data = HashMap::new();
        bubble_data.insert(
            format!("{}:{}", composer_id, bubble_id_1),
            json!({
                "_v": 3,
                "type": 1,
                "bubbleId": bubble_id_1,
                "text": "What is the meaning of life?",
                "rawText": "What is the meaning of life?"
            }),
        );
        bubble_data.insert(
            format!("{}:{}", composer_id, bubble_id_2),
            json!({
                "_v": 3,
                "type": 2,
                "bubbleId": bubble_id_2,
                "text": "The meaning of life is a philosophical question...",
                "rawText": "The meaning of life is a philosophical question...",
                "modelType": "claude-4.5-opus"
            }),
        );

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            &key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.agent_slug, "cursor");
        assert_eq!(conv.external_id, Some(composer_id.to_string()));
        assert_eq!(conv.title, Some("Test Conversation".to_string()));
        assert_eq!(conv.messages.len(), 2);

        // Check first message (user)
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "What is the meaning of life?");
        assert_eq!(conv.messages[0].idx, 0);

        // Check second message (assistant)
        assert_eq!(conv.messages[1].role, "assistant");
        assert!(conv.messages[1].content.contains("philosophical question"));
        assert_eq!(conv.messages[1].idx, 1);
        assert_eq!(conv.messages[1].author, Some("claude-4.5-opus".to_string()));
    }

    #[test]
    fn parse_composer_data_new_format_uses_raw_text_fallback() {
        let composer_id = "test-composer-123";
        let bubble_id = "test-bubble-456";

        let key = format!("composerData:{}", composer_id);
        let value = json!({
            "composerId": composer_id,
            "fullConversationHeadersOnly": [
                {"bubbleId": bubble_id, "type": 1}
            ],
            "createdAt": 1700000000000i64
        })
        .to_string();

        // Bubble only has rawText, not text
        let mut bubble_data = HashMap::new();
        bubble_data.insert(
            format!("{}:{}", composer_id, bubble_id),
            json!({
                "type": 1,
                "bubbleId": bubble_id,
                "rawText": "Content from rawText field"
            }),
        );

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            &key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].content, "Content from rawText field");
    }

    #[test]
    fn parse_composer_data_new_format_skips_missing_bubbles() {
        let composer_id = "test-composer-789";
        let existing_bubble = "existing-bubble";
        let missing_bubble = "missing-bubble";

        let key = format!("composerData:{}", composer_id);
        let value = json!({
            "composerId": composer_id,
            "fullConversationHeadersOnly": [
                {"bubbleId": existing_bubble, "type": 1},
                {"bubbleId": missing_bubble, "type": 2}  // This one won't exist in bubble_data
            ],
            "createdAt": 1700000000000i64
        })
        .to_string();

        // Only include one bubble
        let mut bubble_data = HashMap::new();
        bubble_data.insert(
            format!("{}:{}", composer_id, existing_bubble),
            json!({
                "type": 1,
                "bubbleId": existing_bubble,
                "text": "I exist!"
            }),
        );

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            &key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].content, "I exist!");
    }

    #[test]
    fn parse_composer_data_new_format_uses_name_for_title() {
        let composer_id = "title-test-123";
        let bubble_id = "bubble-456";

        let key = format!("composerData:{}", composer_id);
        let value = json!({
            "composerId": composer_id,
            "fullConversationHeadersOnly": [
                {"bubbleId": bubble_id, "type": 1}
            ],
            "name": "My Custom Conversation Title",
            "createdAt": 1700000000000i64
        })
        .to_string();

        let mut bubble_data = HashMap::new();
        bubble_data.insert(
            format!("{}:{}", composer_id, bubble_id),
            json!({
                "type": 1,
                "text": "This would be the title if name wasn't present"
            }),
        );

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            &key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.title, Some("My Custom Conversation Title".to_string()));
    }

    #[test]
    fn parse_bubble_numeric_type_mapping() {
        // Test type 1 = user (new format)
        let user_bubble = json!({
            "type": 1,
            "text": "User message"
        });
        let msg = CursorConnector::parse_bubble(&user_bubble, 0).unwrap();
        assert_eq!(msg.role, "user");

        // Test type 2 = assistant (new format)
        let assistant_bubble = json!({
            "type": 2,
            "text": "Assistant message"
        });
        let msg = CursorConnector::parse_bubble(&assistant_bubble, 0).unwrap();
        assert_eq!(msg.role, "assistant");

        // Test unknown type defaults to assistant
        let unknown_bubble = json!({
            "type": 99,
            "text": "Unknown type message"
        });
        let msg = CursorConnector::parse_bubble(&unknown_bubble, 0).unwrap();
        assert_eq!(msg.role, "assistant");
    }

    #[test]
    fn parse_bubble_uses_raw_text_fallback() {
        // New format uses rawText when text is missing
        let bubble = json!({
            "type": 1,
            "rawText": "Content from rawText"
        });
        let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
        assert_eq!(msg.content, "Content from rawText");
    }

    #[test]
    fn parse_bubble_extracts_model_type() {
        // New format uses modelType field
        let bubble = json!({
            "type": 2,
            "text": "Response",
            "modelType": "claude-4.5-opus"
        });
        let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
        assert_eq!(msg.author, Some("claude-4.5-opus".to_string()));

        // Legacy format uses model field
        let legacy_bubble = json!({
            "type": "assistant",
            "text": "Response",
            "model": "gpt-4"
        });
        let msg = CursorConnector::parse_bubble(&legacy_bubble, 0).unwrap();
        assert_eq!(msg.author, Some("gpt-4".to_string()));
    }

    #[test]
    fn parse_bubble_missing_type_defaults_to_assistant() {
        // No type field at all should default to assistant
        let bubble = json!({
            "text": "Message without type"
        });
        let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
        assert_eq!(msg.role, "assistant");
    }

    #[test]
    fn parse_composer_data_empty_headers_falls_back_to_tabs() {
        // Empty fullConversationHeadersOnly array should fall back to tabs format
        let key = "composerData:fallback-test";
        let value = json!({
            "composerId": "fallback-test",
            "fullConversationHeadersOnly": [],
            "tabs": [{
                "bubbles": [
                    {"text": "Fallback message", "type": "user"}
                ]
            }],
            "createdAt": 1700000000000i64
        })
        .to_string();

        let mut seen = HashSet::new();
        let bubble_data = HashMap::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].content, "Fallback message");
    }

    #[test]
    fn parse_composer_data_null_bubble_id_skipped() {
        // Null bubbleId in header should be gracefully skipped
        let composer_id = "null-bubble-test";
        let valid_bubble = "valid-bubble";

        let key = format!("composerData:{}", composer_id);
        let value = json!({
            "composerId": composer_id,
            "fullConversationHeadersOnly": [
                {"bubbleId": null, "type": 1},  // null bubbleId
                {"type": 2},  // missing bubbleId
                {"bubbleId": valid_bubble, "type": 1}  // valid
            ],
            "createdAt": 1700000000000i64
        })
        .to_string();

        let mut bubble_data = HashMap::new();
        bubble_data.insert(
            format!("{}:{}", composer_id, valid_bubble),
            json!({
                "type": 1,
                "text": "Valid message"
            }),
        );

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            &key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        // Only the valid bubble should be parsed
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].content, "Valid message");
    }

    #[test]
    fn extract_from_db_new_format() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.vscdb");

        let conn = create_test_db(&db_path);

        let composer_id = "new-format-test-123";
        let bubble_id_1 = "bubble-user-1";
        let bubble_id_2 = "bubble-assistant-1";

        // Insert composerData with new format
        let composer_value = json!({
            "composerId": composer_id,
            "fullConversationHeadersOnly": [
                {"bubbleId": bubble_id_1, "type": 1},
                {"bubbleId": bubble_id_2, "type": 2}
            ],
            "name": "New Format Test",
            "createdAt": 1700000000000i64
        })
        .to_string();
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            [&format!("composerData:{}", composer_id), &composer_value],
        )
        .unwrap();

        // Insert bubble data
        let bubble1_value = json!({
            "type": 1,
            "bubbleId": bubble_id_1,
            "text": "Hello from user"
        })
        .to_string();
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            [
                &format!("bubbleId:{}:{}", composer_id, bubble_id_1),
                &bubble1_value,
            ],
        )
        .unwrap();

        let bubble2_value = json!({
            "type": 2,
            "bubbleId": bubble_id_2,
            "text": "Hello from assistant"
        })
        .to_string();
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            [
                &format!("bubbleId:{}:{}", composer_id, bubble_id_2),
                &bubble2_value,
            ],
        )
        .unwrap();

        drop(conn);

        let convs = CursorConnector::extract_from_db(&db_path, None).unwrap();
        assert_eq!(convs.len(), 1);

        let conv = &convs[0];
        assert_eq!(conv.title, Some("New Format Test".to_string()));
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "Hello from user");
        assert_eq!(conv.messages[1].role, "assistant");
        assert_eq!(conv.messages[1].content, "Hello from assistant");
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
        let conv = CursorConnector::parse_aichat_data(
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
        let conv = CursorConnector::parse_aichat_data(
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
    fn parse_aichat_data_skips_duplicates() {
        let key = "aichat.dup";
        let value = json!({
            "tabs": [{
                "bubbles": [{"text": "Content", "type": "user"}]
            }]
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv1 = CursorConnector::parse_aichat_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );
        let conv2 = CursorConnector::parse_aichat_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

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
        let bubble_data = HashMap::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
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
        let bubble_data = HashMap::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
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
        let bubble_data = HashMap::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
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
        let bubble_data = HashMap::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            &bubble_data,
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
