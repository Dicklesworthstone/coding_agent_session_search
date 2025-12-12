//! OpenCode connector - reads from file-based JSON storage.
//!
//! OpenCode stores data at `~/.local/share/opencode/storage/` (or `$XDG_DATA_HOME/opencode/storage/`)
//! in a hierarchical JSON file structure:
//!
//! ```text
//! storage/
//! ├── session/{projectID}/{sessionID}.json    # Session metadata
//! ├── message/{sessionID}/{messageID}.json    # Message metadata
//! ├── part/{messageID}/{partID}.json          # Actual message content (text, tool calls, etc.)
//! ├── project/                                 # Project metadata
//! └── session_diff/                           # Diffs
//! ```

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
    file_modified_since,
};

// ============================================================================
// JSON Schema Structs
// ============================================================================

/// OpenCode session metadata from `session/{projectID}/{sessionID}.json`
#[derive(Debug, Clone, Deserialize)]
struct OpenCodeSession {
    id: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(rename = "projectID")]
    project_id: String,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default, rename = "parentID")]
    parent_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    time: Option<OpenCodeTime>,
    #[serde(default)]
    summary: Option<OpenCodeSessionSummary>,
}

/// Timestamps for sessions and messages
#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
struct OpenCodeTime {
    #[serde(default)]
    created: Option<i64>,
    #[serde(default)]
    updated: Option<i64>,
    #[serde(default)]
    completed: Option<i64>,
    #[serde(default)]
    start: Option<i64>,
    #[serde(default)]
    end: Option<i64>,
}

/// Session summary (git stats)
#[derive(Debug, Clone, Deserialize, Default)]
struct OpenCodeSessionSummary {
    #[serde(default)]
    additions: Option<i64>,
    #[serde(default)]
    deletions: Option<i64>,
    #[serde(default)]
    files: Option<i64>,
}

/// OpenCode message metadata from `message/{sessionID}/{messageID}.json`
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct OpenCodeMessage {
    id: String,
    #[serde(rename = "sessionID")]
    session_id: String,
    role: String,
    #[serde(default)]
    time: Option<OpenCodeTime>,
    #[serde(default, rename = "parentID")]
    parent_id: Option<String>,
    #[serde(default, rename = "modelID")]
    model_id: Option<String>,
    #[serde(default, rename = "providerID")]
    provider_id: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    path: Option<OpenCodePath>,
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default)]
    tokens: Option<OpenCodeTokens>,
    #[serde(default)]
    finish: Option<String>,
    /// User message summary (title, body, diffs)
    #[serde(default)]
    summary: Option<OpenCodeMessageSummary>,
    /// Agent info for user messages
    #[serde(default)]
    agent: Option<String>,
    /// Model info for user messages (alternative location)
    #[serde(default)]
    model: Option<OpenCodeModel>,
}

/// Path info for assistant messages
#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
struct OpenCodePath {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    root: Option<String>,
}

/// Token usage info
#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
struct OpenCodeTokens {
    #[serde(default)]
    input: Option<i64>,
    #[serde(default)]
    output: Option<i64>,
    #[serde(default)]
    reasoning: Option<i64>,
    #[serde(default)]
    cache: Option<OpenCodeCache>,
}

/// Cache token info
#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
struct OpenCodeCache {
    #[serde(default)]
    read: Option<i64>,
    #[serde(default)]
    write: Option<i64>,
}

/// User message summary - can be either a boolean or a struct
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
struct OpenCodeMessageSummary {
    title: Option<String>,
    body: Option<String>,
    diffs: Option<Vec<serde_json::Value>>,
}

impl<'de> Deserialize<'de> for OpenCodeMessageSummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{self, MapAccess, Visitor};

        struct SummaryVisitor;

        impl<'de> Visitor<'de> for SummaryVisitor {
            type Value = OpenCodeMessageSummary;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a boolean or a summary object")
            }

            fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                // When summary is just `true`, return an empty summary
                Ok(OpenCodeMessageSummary::default())
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut title = None;
                let mut body = None;
                let mut diffs = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "title" => title = map.next_value()?,
                        "body" => body = map.next_value()?,
                        "diffs" => diffs = map.next_value()?,
                        _ => {
                            // Skip unknown fields
                            let _ = map.next_value::<serde_json::Value>()?;
                        }
                    }
                }

                Ok(OpenCodeMessageSummary { title, body, diffs })
            }
        }

        deserializer.deserialize_any(SummaryVisitor)
    }
}

/// Model info (alternative location in user messages)
#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
struct OpenCodeModel {
    #[serde(default, rename = "providerID")]
    provider_id: Option<String>,
    #[serde(default, rename = "modelID")]
    model_id: Option<String>,
}

/// OpenCode part from `part/{messageID}/{partID}.json`
///
/// Parts can be different types: text, tool, step-start, step-finish, reasoning, patch
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct OpenCodePart {
    id: String,
    #[serde(rename = "sessionID")]
    session_id: String,
    #[serde(rename = "messageID")]
    message_id: String,
    #[serde(rename = "type")]
    part_type: String,
    /// Text content (for text and reasoning parts)
    #[serde(default)]
    text: Option<String>,
    /// Tool call ID (for tool parts)
    #[serde(default, rename = "callID")]
    call_id: Option<String>,
    /// Tool name (for tool parts)
    #[serde(default)]
    tool: Option<String>,
    /// Tool state (for tool parts)
    #[serde(default)]
    state: Option<OpenCodeToolState>,
    /// Git snapshot hash (for step-start/step-finish)
    #[serde(default)]
    snapshot: Option<String>,
    /// Finish reason (for step-finish)
    #[serde(default)]
    reason: Option<String>,
    /// Cost (for step-finish)
    #[serde(default)]
    cost: Option<f64>,
    /// Tokens (for step-finish)
    #[serde(default)]
    tokens: Option<OpenCodeTokens>,
    /// Patch hash (for patch parts)
    #[serde(default)]
    hash: Option<String>,
    /// Files affected (for patch parts)
    #[serde(default)]
    files: Option<Vec<String>>,
    /// Time info (for reasoning parts)
    #[serde(default)]
    time: Option<OpenCodeTime>,
    /// Metadata (for reasoning parts)
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    /// Filename (for file parts)
    #[serde(default)]
    filename: Option<String>,
    /// URL (for file parts)
    #[serde(default)]
    url: Option<String>,
    /// MIME type (for file parts)
    #[serde(default)]
    mime: Option<String>,
}

/// Tool state for tool parts
#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
struct OpenCodeToolState {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

// ============================================================================
// Connector Implementation
// ============================================================================

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

    /// Get candidate directories where OpenCode data might be stored.
    fn dir_candidates() -> Vec<PathBuf> {
        let mut dirs = Vec::new();

        // XDG_DATA_HOME or ~/.local/share
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            dirs.push(PathBuf::from(xdg).join("opencode/storage"));
        }

        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join(".local/share/opencode/storage"));
        }

        // Also check data_dir() which handles platform differences
        if let Some(data) = dirs::data_dir() {
            dirs.push(data.join("opencode/storage"));
        }

        // Legacy locations (in case OpenCode ever used these)
        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join(".opencode/storage"));
        }

        dirs
    }

    /// Find the OpenCode storage root directory.
    fn find_storage_root() -> Option<PathBuf> {
        Self::dir_candidates()
            .into_iter()
            .find(|candidate| Self::is_valid_storage_dir(candidate))
    }

    /// Check if a directory is a valid OpenCode storage directory.
    fn is_valid_storage_dir(path: &Path) -> bool {
        if !path.exists() || !path.is_dir() {
            return false;
        }
        // Must have session/, message/, and part/ subdirectories
        let session_dir = path.join("session");
        let message_dir = path.join("message");
        let part_dir = path.join("part");

        session_dir.is_dir() && message_dir.is_dir() && part_dir.is_dir()
    }

    /// Scan all sessions from the storage directory.
    fn scan_sessions(storage_root: &Path) -> Result<Vec<(PathBuf, OpenCodeSession)>> {
        let session_dir = storage_root.join("session");
        let mut sessions = Vec::new();

        for entry in WalkDir::new(&session_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "json") {
                match std::fs::read_to_string(path) {
                    Ok(content) => match serde_json::from_str::<OpenCodeSession>(&content) {
                        Ok(session) => sessions.push((path.to_path_buf(), session)),
                        Err(e) => {
                            tracing::debug!(
                                "opencode: failed to parse session {}: {e}",
                                path.display()
                            );
                        }
                    },
                    Err(e) => {
                        tracing::debug!("opencode: failed to read session {}: {e}", path.display());
                    }
                }
            }
        }

        Ok(sessions)
    }

    /// Load all messages for a session.
    fn load_messages(storage_root: &Path, session_id: &str) -> Result<Vec<OpenCodeMessage>> {
        let message_dir = storage_root.join("message").join(session_id);
        let mut messages = Vec::new();

        if !message_dir.exists() {
            return Ok(messages);
        }

        for entry in std::fs::read_dir(&message_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "json") {
                match std::fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str::<OpenCodeMessage>(&content) {
                        Ok(msg) => messages.push(msg),
                        Err(e) => {
                            tracing::debug!(
                                "opencode: failed to parse message {}: {e}",
                                path.display()
                            );
                        }
                    },
                    Err(e) => {
                        tracing::debug!("opencode: failed to read message {}: {e}", path.display());
                    }
                }
            }
        }

        // Sort by creation time
        messages.sort_by_key(|m| m.time.as_ref().and_then(|t| t.created).unwrap_or(0));

        Ok(messages)
    }

    /// Load all parts for a message.
    fn load_parts(storage_root: &Path, message_id: &str) -> Result<Vec<OpenCodePart>> {
        let part_dir = storage_root.join("part").join(message_id);
        let mut parts = Vec::new();

        if !part_dir.exists() {
            return Ok(parts);
        }

        for entry in std::fs::read_dir(&part_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "json") {
                match std::fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str::<OpenCodePart>(&content) {
                        Ok(part) => parts.push(part),
                        Err(e) => {
                            tracing::debug!(
                                "opencode: failed to parse part {}: {e}",
                                path.display()
                            );
                        }
                    },
                    Err(e) => {
                        tracing::debug!("opencode: failed to read part {}: {e}", path.display());
                    }
                }
            }
        }

        // Sort by part ID (they're lexicographically ordered by creation)
        parts.sort_by(|a, b| a.id.cmp(&b.id));

        Ok(parts)
    }

    /// Assemble message content from parts.
    fn assemble_content(parts: &[OpenCodePart]) -> String {
        let mut content_parts = Vec::new();

        for part in parts {
            match part.part_type.as_str() {
                "text" => {
                    if let Some(text) = &part.text
                        && !text.is_empty()
                    {
                        content_parts.push(text.clone());
                    }
                }
                "reasoning" => {
                    if let Some(text) = &part.text
                        && !text.is_empty()
                    {
                        content_parts.push(format!("[Reasoning]\n{text}"));
                    }
                }
                "tool" => {
                    let tool_name = part.tool.as_deref().unwrap_or("unknown");
                    let mut tool_desc = format!("[Tool: {tool_name}]");

                    // Add tool input description if available
                    if let Some(state) = &part.state {
                        if let Some(title) = &state.title {
                            tool_desc = format!("[Tool: {tool_name} - {title}]");
                        } else if let Some(input) = &state.input {
                            // Try to extract a description from the input
                            if let Some(desc) = input.get("description").and_then(|v| v.as_str()) {
                                tool_desc = format!("[Tool: {tool_name} - {desc}]");
                            } else if let Some(path) =
                                input.get("filePath").and_then(|v| v.as_str())
                            {
                                tool_desc = format!("[Tool: {tool_name} - {path}]");
                            } else if let Some(cmd) = input.get("command").and_then(|v| v.as_str())
                            {
                                // Truncate long commands
                                let cmd_short = if cmd.len() > 100 {
                                    format!("{}...", &cmd[..100])
                                } else {
                                    cmd.to_string()
                                };
                                tool_desc = format!("[Tool: {tool_name} - {cmd_short}]");
                            }
                        }

                        // Include tool output if it's meaningful
                        if let Some(output) = &state.output
                            && !output.is_empty()
                            && output.len() < 500
                        {
                            tool_desc.push_str(&format!("\n{output}"));
                        }
                    }

                    content_parts.push(tool_desc);
                }
                "patch" => {
                    if let Some(files) = &part.files {
                        let file_list = files.join(", ");
                        content_parts.push(format!("[Patch: {file_list}]"));
                    }
                }
                "step-start" | "step-finish" | "compaction" => {
                    // These are metadata parts, skip for content
                }
                "file" => {
                    // File attachment - include filename if available
                    if let Some(filename) = &part.filename {
                        content_parts.push(format!("[File: {filename}]"));
                    }
                }
                other => {
                    tracing::debug!("opencode: unknown part type: {other}");
                }
            }
        }

        content_parts.join("\n")
    }

    /// Get the author/model string for a message.
    fn get_author(msg: &OpenCodeMessage) -> Option<String> {
        // Try model_id first (assistant messages)
        if let Some(model_id) = &msg.model_id {
            return Some(model_id.clone());
        }
        // Try model.model_id (user messages)
        if let Some(model) = &msg.model
            && let Some(model_id) = &model.model_id
        {
            return Some(model_id.clone());
        }
        // Try agent field
        if let Some(agent) = &msg.agent {
            return Some(agent.clone());
        }
        None
    }

    /// Convert an OpenCode message to a normalized message.
    fn normalize_message(
        storage_root: &Path,
        msg: &OpenCodeMessage,
        idx: i64,
    ) -> Result<NormalizedMessage> {
        // Load parts and assemble content
        let parts = Self::load_parts(storage_root, &msg.id)?;
        let mut content = Self::assemble_content(&parts);

        // For user messages, also include the summary body if available
        if msg.role == "user"
            && let Some(summary) = &msg.summary
            && let Some(body) = &summary.body
            && !body.is_empty()
        {
            if content.is_empty() {
                content = body.clone();
            } else {
                content = format!("{body}\n\n{content}");
            }
        }

        let created_at = msg.time.as_ref().and_then(|t| t.created);

        // Build extra metadata
        let mut extra = serde_json::Map::new();
        if let Some(provider) = &msg.provider_id {
            extra.insert(
                "provider".into(),
                serde_json::Value::String(provider.clone()),
            );
        }
        if let Some(mode) = &msg.mode {
            extra.insert("mode".into(), serde_json::Value::String(mode.clone()));
        }
        if let Some(cost) = msg.cost {
            extra.insert("cost".into(), serde_json::json!(cost));
        }
        if let Some(tokens) = &msg.tokens {
            extra.insert(
                "tokens".into(),
                serde_json::json!({
                    "input": tokens.input,
                    "output": tokens.output,
                    "reasoning": tokens.reasoning,
                }),
            );
        }
        if let Some(finish) = &msg.finish {
            extra.insert("finish".into(), serde_json::Value::String(finish.clone()));
        }
        if let Some(parent) = &msg.parent_id {
            extra.insert(
                "parent_id".into(),
                serde_json::Value::String(parent.clone()),
            );
        }

        Ok(NormalizedMessage {
            idx,
            role: msg.role.clone(),
            author: Self::get_author(msg),
            created_at,
            content,
            extra: serde_json::Value::Object(extra),
            snippets: Vec::new(),
        })
    }
}

impl Connector for OpenCodeConnector {
    fn detect(&self) -> DetectionResult {
        if let Some(root) = Self::find_storage_root() {
            return DetectionResult {
                detected: true,
                evidence: vec![format!("found OpenCode storage at {}", root.display())],
            };
        }
        DetectionResult::not_found()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Determine storage root
        let storage_root = if ctx.data_root.exists() && Self::is_valid_storage_dir(&ctx.data_root) {
            ctx.data_root.clone()
        } else if let Some(root) = Self::find_storage_root() {
            root
        } else {
            return Ok(Vec::new());
        };

        let mut convs = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        // Scan all sessions
        let sessions = Self::scan_sessions(&storage_root)?;

        for (session_path, session) in sessions {
            // Skip if session file hasn't been modified since last scan
            if !file_modified_since(&session_path, ctx.since_ts) {
                continue;
            }

            // Deduplicate by session ID
            if !seen_ids.insert(session.id.clone()) {
                continue;
            }

            // Load messages for this session
            let messages = Self::load_messages(&storage_root, &session.id)?;
            if messages.is_empty() {
                continue;
            }

            // Normalize messages
            let mut normalized_messages = Vec::new();
            for (idx, msg) in messages.iter().enumerate() {
                match Self::normalize_message(&storage_root, msg, idx as i64) {
                    Ok(norm_msg) => normalized_messages.push(norm_msg),
                    Err(e) => {
                        tracing::debug!("opencode: failed to normalize message {}: {e}", msg.id);
                    }
                }
            }

            if normalized_messages.is_empty() {
                continue;
            }

            // Apply since_ts filter to messages
            if let Some(since) = ctx.since_ts {
                normalized_messages.retain(|m| m.created_at.is_some_and(|ts| ts > since));
                if normalized_messages.is_empty() {
                    continue;
                }
                // Re-index after filtering
                for (i, msg) in normalized_messages.iter_mut().enumerate() {
                    msg.idx = i as i64;
                }
            }

            // Extract timestamps
            let started_at = session
                .time
                .as_ref()
                .and_then(|t| t.created)
                .or_else(|| normalized_messages.first().and_then(|m| m.created_at));
            let ended_at = session
                .time
                .as_ref()
                .and_then(|t| t.updated.or(t.completed))
                .or_else(|| normalized_messages.last().and_then(|m| m.created_at));

            // Get workspace from session directory
            let workspace = session.directory.map(PathBuf::from);

            // Build metadata
            let metadata = serde_json::json!({
                "session_id": session.id,
                "project_id": session.project_id,
                "version": session.version,
                "parent_session": session.parent_id,
                "summary": session.summary.as_ref().map(|s| serde_json::json!({
                    "additions": s.additions,
                    "deletions": s.deletions,
                    "files": s.files,
                })),
            });

            convs.push(NormalizedConversation {
                agent_slug: "opencode".into(),
                external_id: Some(session.id.clone()),
                title: session.title.or_else(|| {
                    // Fallback: use first user message summary title
                    messages
                        .iter()
                        .find_map(|m| m.summary.as_ref().and_then(|s| s.title.clone()))
                }),
                workspace,
                source_path: session_path,
                started_at,
                ended_at,
                metadata,
                messages: normalized_messages,
            });
        }

        Ok(convs)
    }
}
