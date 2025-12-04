 //! Connector for VS Code GitHub Copilot Chat history.
//!
//! VS Code Copilot stores chat sessions as JSON files in:
//! - macOS: ~/Library/Application Support/Code/User/globalStorage/emptyWindowChatSessions/
//! - macOS workspaces: ~/Library/Application Support/Code/User/workspaceStorage/{id}/chatSessions/
//! - Linux: ~/.config/Code/User/globalStorage/emptyWindowChatSessions/
//! - Linux workspaces: ~/.config/Code/User/workspaceStorage/{id}/chatSessions/
//! - Windows: %APPDATA%/Code/User/globalStorage/emptyWindowChatSessions/
//!
//! Each session is a JSON file with:
//! - `version`: Schema version (currently 3)
//! - `sessionId`: UUID identifying the session
//! - `creationDate`: Unix timestamp in milliseconds
//! - `lastMessageDate`: Unix timestamp of last message
//! - `requests`: Array of request/response pairs with `message`, `response`, `timestamp`

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, NormalizedSnippet,
    ScanContext,
};

pub struct CopilotConnector;

impl Default for CopilotConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl CopilotConnector {
    pub fn new() -> Self {
        Self
    }

    /// Get the base VS Code application support directory
    pub fn app_support_dir() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            dirs::home_dir().map(|h| h.join("Library/Application Support/Code/User"))
        }
        #[cfg(target_os = "linux")]
        {
            dirs::home_dir().map(|h| h.join(".config/Code/User"))
        }
        #[cfg(target_os = "windows")]
        {
            dirs::data_dir().map(|d| d.join("Code/User"))
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            None
        }
    }

    /// Find all Copilot chat session JSON files
    fn find_session_files(base: &Path) -> Vec<(PathBuf, Option<PathBuf>)> {
        let mut sessions = Vec::new();

        // Check globalStorage/emptyWindowChatSessions for non-workspace sessions
        let global_sessions = base.join("globalStorage/emptyWindowChatSessions");
        if global_sessions.exists() {
            for entry in WalkDir::new(&global_sessions)
                .max_depth(1)
                .into_iter()
                .flatten()
            {
                let path = entry.path();
                if path.is_file()
                    && path.extension().is_some_and(|ext| ext == "json")
                    && path.file_name().is_some_and(|n| {
                        // Match UUID pattern: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx.json
                        let name = n.to_str().unwrap_or("");
                        name.len() == 41 && name.contains('-') && name.ends_with(".json")
                    })
                {
                    sessions.push((path.to_path_buf(), None));
                }
            }
        }

        // Check workspaceStorage for workspace-specific sessions
        let workspace_storage = base.join("workspaceStorage");
        if workspace_storage.exists() {
            for entry in WalkDir::new(&workspace_storage)
                .max_depth(3)
                .into_iter()
                .flatten()
            {
                let path = entry.path();
                if path.is_file()
                    && path.extension().is_some_and(|ext| ext == "json")
                    && path
                        .parent()
                        .and_then(|p| p.file_name())
                        .is_some_and(|n| n == "chatSessions")
                {
                    // Try to get workspace path from workspace.json
                    let workspace_dir = path.parent().and_then(|p| p.parent());
                    let workspace_path = workspace_dir.and_then(|dir| {
                        let workspace_json = dir.join("workspace.json");
                        if workspace_json.exists() {
                            fs::read_to_string(&workspace_json)
                                .ok()
                                .and_then(|content| serde_json::from_str::<Value>(&content).ok())
                                .and_then(|json| {
                                    json.get("folder")
                                        .and_then(|v| v.as_str())
                                        .map(|s| {
                                            // Handle file:// URI format
                                            let decoded = urlencoding::decode(s).unwrap_or_default();
                                            let path_str = decoded.strip_prefix("file://").unwrap_or(&decoded);
                                            PathBuf::from(path_str)
                                        })
                                })
                        } else {
                            None
                        }
                    });
                    sessions.push((path.to_path_buf(), workspace_path));
                }
            }
        }

        sessions
    }

    /// Parse a single session JSON file into a normalized conversation
    fn parse_session(
        session_path: &Path,
        workspace: Option<PathBuf>,
    ) -> Result<Option<NormalizedConversation>> {
        let content =
            fs::read_to_string(session_path).with_context(|| format!("read {}", session_path.display()))?;

        let val: Value = serde_json::from_str(&content)
            .with_context(|| format!("parse JSON from {}", session_path.display()))?;

        // Extract session metadata
        let session_id = val
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(String::from);

        let creation_date = val.get("creationDate").and_then(|v| v.as_i64());
        let last_message_date = val.get("lastMessageDate").and_then(|v| v.as_i64());

        // Get responder info
        let responder = val
            .get("responderUsername")
            .and_then(|v| v.as_str())
            .unwrap_or("GitHub Copilot");

        // Get user info
        let requester = val
            .get("requesterUsername")
            .and_then(|v| v.as_str())
            .map(String::from);

        // Parse custom title if set
        let custom_title = val
            .get("customTitle")
            .and_then(|v| v.as_str())
            .map(String::from);

        // Parse requests array
        let requests = val.get("requests").and_then(|v| v.as_array());
        let Some(requests) = requests else {
            return Ok(None);
        };

        if requests.is_empty() {
            return Ok(None);
        }

        let mut messages = Vec::new();

        for request in requests {
            let timestamp = request.get("timestamp").and_then(|v| v.as_i64());
            let model_id = request
                .get("modelId")
                .and_then(|v| v.as_str())
                .map(String::from);
            let agent = request
                .get("agent")
                .and_then(|v| v.as_str())
                .map(String::from);

            // Parse user message
            let user_text = request
                .get("message")
                .and_then(|msg| msg.get("text").and_then(|v| v.as_str()))
                .unwrap_or("");

            if !user_text.trim().is_empty() {
                // Extract code snippets from message parts if present
                let snippets = Self::extract_snippets(request.get("message"));

                messages.push(NormalizedMessage {
                    idx: messages.len() as i64,
                    role: "user".to_string(),
                    author: requester.clone(),
                    created_at: timestamp,
                    content: user_text.to_string(),
                    extra: serde_json::json!({
                        "agent": agent,
                        "modelId": model_id,
                    }),
                    snippets,
                });
            }

            // Parse assistant response(s)
            if let Some(response) = request.get("response") {
                let response_parts = if response.is_array() {
                    response.as_array().unwrap()
                } else {
                    // Single response object
                    continue;
                };

                for part in response_parts {
                    // Skip non-content parts (like mcpServersStarting)
                    if part.get("kind").is_some() && part.get("value").is_none() {
                        continue;
                    }

                    let value = part
                        .get("value")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    if value.trim().is_empty() {
                        continue;
                    }

                    messages.push(NormalizedMessage {
                        idx: messages.len() as i64,
                        role: "assistant".to_string(),
                        author: Some(responder.to_string()),
                        created_at: timestamp,
                        content: value.to_string(),
                        extra: serde_json::json!({
                            "modelId": model_id,
                            "agent": agent,
                        }),
                        snippets: Vec::new(),
                    });
                }
            }
        }

        if messages.is_empty() {
            return Ok(None);
        }

        // Generate title from custom title or first user message
        let title = custom_title.or_else(|| {
            messages.iter().find(|m| m.role == "user").map(|m| {
                m.content
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(100)
                    .collect::<String>()
            })
        });

        Ok(Some(NormalizedConversation {
            agent_slug: "copilot".to_string(),
            external_id: session_id,
            title,
            workspace,
            source_path: session_path.to_path_buf(),
            started_at: creation_date,
            ended_at: last_message_date.or(creation_date),
            metadata: serde_json::json!({
                "source": "vscode_copilot",
                "version": val.get("version").and_then(|v| v.as_i64()),
                "requester": requester,
                "responder": responder,
                "initialLocation": val.get("initialLocation").and_then(|v| v.as_str()),
            }),
            messages,
        }))
    }

    /// Extract code snippets from message parts
    fn extract_snippets(message: Option<&Value>) -> Vec<NormalizedSnippet> {
        let mut snippets = Vec::new();

        if let Some(msg) = message {
            if let Some(parts) = msg.get("parts").and_then(|v| v.as_array()) {
                for part in parts {
                    // Check for editor range references (code selections)
                    if let Some(range) = part.get("editorRange") {
                        let start_line = range.get("startLineNumber").and_then(|v| v.as_i64());
                        let end_line = range.get("endLineNumber").and_then(|v| v.as_i64());
                        let text = part.get("text").and_then(|v| v.as_str());

                        if text.is_some() || start_line.is_some() {
                            snippets.push(NormalizedSnippet {
                                file_path: None, // File path not always available
                                start_line,
                                end_line,
                                language: None,
                                snippet_text: text.map(String::from),
                            });
                        }
                    }
                }
            }
        }

        snippets
    }
}

impl Connector for CopilotConnector {
    fn detect(&self) -> DetectionResult {
        if let Some(base) = Self::app_support_dir() {
            let global_sessions = base.join("globalStorage/emptyWindowChatSessions");
            let has_global = global_sessions.exists()
                && fs::read_dir(&global_sessions)
                    .map(|d| d.count() > 0)
                    .unwrap_or(false);

            // Also check workspace storage for chatSessions directories
            let workspace_storage = base.join("workspaceStorage");
            let has_workspace = workspace_storage.exists()
                && WalkDir::new(&workspace_storage)
                    .max_depth(3)
                    .into_iter()
                    .flatten()
                    .any(|e| {
                        e.path().is_dir()
                            && e.file_name().to_str() == Some("chatSessions")
                    });

            if has_global || has_workspace {
                let sessions = Self::find_session_files(&base);
                return DetectionResult {
                    detected: true,
                    evidence: vec![
                        format!("found VS Code Copilot at {}", base.display()),
                        format!("found {} session file(s)", sessions.len()),
                    ],
                };
            }
        }
        DetectionResult::not_found()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Determine base directory
        let base = if ctx.data_root.join("globalStorage").exists()
            || ctx.data_root.join("workspaceStorage").exists()
            || ctx
                .data_root
                .file_name()
                .is_some_and(|n| n.to_str().unwrap_or("").contains("Code"))
        {
            ctx.data_root.clone()
        } else if let Some(default_base) = Self::app_support_dir() {
            default_base
        } else {
            return Ok(Vec::new());
        };

        if !base.exists() {
            return Ok(Vec::new());
        }

        let session_files = Self::find_session_files(&base);
        let mut all_convs = Vec::new();

        for (session_path, workspace) in session_files {
            // Skip files not modified since last scan
            if !crate::connectors::file_modified_since(&session_path, ctx.since_ts) {
                continue;
            }

            match Self::parse_session(&session_path, workspace) {
                Ok(Some(conv)) => {
                    tracing::debug!(
                        path = %session_path.display(),
                        messages = conv.messages.len(),
                        "copilot parsed session"
                    );
                    all_convs.push(conv);
                }
                Ok(None) => {
                    tracing::debug!(
                        path = %session_path.display(),
                        "copilot session has no messages, skipping"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        path = %session_path.display(),
                        error = %e,
                        "copilot failed to parse session"
                    );
                }
            }
        }

        Ok(all_convs)
    }
}
