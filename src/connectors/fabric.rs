//! Connector for Fabric IDE chat history.
//!
//! Fabric is a desktop LLM-based multi-file coding assistant built with Electron.
//! It stores chat history in JSON files under `{project}/.fabric/chats/chat-{id}/`.
//!
//! This connector:
//! - Discovers Fabric projects via window-projects.json and directory scanning
//! - Parses V2 message format with variants and response segments
//! - Normalizes messages for indexing and search

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

pub struct FabricConnector;

impl Default for FabricConnector {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Fabric-specific data structures for deserialization
// ============================================================================

/// window-projects.json structure from Fabric app support
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct WindowProjectsFile {
    #[serde(rename = "windowProjects", default)]
    window_projects: std::collections::HashMap<String, String>,
    #[serde(rename = "lastClosedProjectPath")]
    last_closed_project_path: Option<String>,
}

/// Project metadata from .fabric/project-metadata.json
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ProjectMetadata {
    #[serde(rename = "UUID")]
    uuid: String,
    #[serde(rename = "originalRootPath")]
    original_root_path: Option<String>,
    #[serde(rename = "creationDate")]
    creation_date: Option<String>,
}

/// Chat metadata from chat_metadata.json
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ChatMetadata {
    id: String,
    title: Option<String>,
    #[serde(rename = "lastMessage")]
    last_message: Option<String>,
    timestamp: Option<i64>,
    #[serde(rename = "isActive")]
    is_active: Option<bool>,
    #[serde(rename = "createdAt")]
    created_at: Option<i64>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<i64>,
}

/// V2 message format from chat_messages.json
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct V2Message {
    id: String,
    #[serde(default)]
    format_ver: Option<String>,
    parent_id: Option<String>,
    #[serde(default)]
    children_id: Vec<String>,
    #[serde(rename = "isUser")]
    is_user: bool,
    timestamp: Value, // Can be string (ISO-8601) or number
    #[serde(rename = "messageData")]
    message_data: Value,
}

/// User message data with variants
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct UserMsgData {
    #[serde(default)]
    variants: std::collections::HashMap<String, UserVariant>,
    #[serde(default)]
    images: Vec<String>,
}

/// A single variant of a user message
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct UserVariant {
    text: String,
    #[serde(rename = "isDefault")]
    is_default: Option<bool>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
}

/// Assistant message data with response segments
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AssistantMsgData {
    #[serde(rename = "processingTime")]
    processing_time: Option<i64>,
    cost: Option<CostData>,
    #[serde(rename = "responseContent", default)]
    response_content: Vec<ResponseSegment>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CostData {
    amount: Option<f64>,
}

/// A single response segment from an assistant message
#[derive(Debug, Deserialize)]
struct ResponseSegment {
    #[serde(rename = "type")]
    segment_type: String,
    content: Value,
}

// ============================================================================
// Implementation
// ============================================================================

impl FabricConnector {
    pub fn new() -> Self {
        Self
    }

    /// Get the Fabric app support directory based on platform.
    fn app_support_dir() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            dirs::home_dir().map(|h| h.join("Library/Application Support/Fabric"))
        }
        #[cfg(target_os = "windows")]
        {
            dirs::data_local_dir().map(|d| d.join("Fabric"))
        }
        #[cfg(target_os = "linux")]
        {
            dirs::config_dir().map(|c| c.join("Fabric"))
        }
    }

    /// Check if a directory is a valid Fabric project.
    fn is_fabric_project(path: &PathBuf) -> bool {
        let fabric_dir = path.join(".fabric");
        let metadata_file = fabric_dir.join("project-metadata.json");
        let chats_dir = fabric_dir.join("chats");

        fabric_dir.is_dir() && metadata_file.is_file() && chats_dir.is_dir()
    }

    /// Discover Fabric projects from window-projects.json.
    fn discover_projects_from_app_support() -> Vec<PathBuf> {
        let mut projects = HashSet::new();

        if let Some(app_dir) = Self::app_support_dir() {
            let window_projects_path = app_dir.join("window-projects.json");
            if let Ok(content) = fs::read_to_string(&window_projects_path) {
                if let Ok(data) = serde_json::from_str::<WindowProjectsFile>(&content) {
                    // Add projects from windowProjects map
                    for project_path in data.window_projects.values() {
                        let path = PathBuf::from(project_path);
                        if Self::is_fabric_project(&path) {
                            projects.insert(path);
                        }
                    }
                    // Add lastClosedProjectPath
                    if let Some(last) = data.last_closed_project_path {
                        let path = PathBuf::from(&last);
                        if Self::is_fabric_project(&path) {
                            projects.insert(path);
                        }
                    }
                }
            }
        }

        projects.into_iter().collect()
    }

    /// Scan common project directories for Fabric projects as fallback.
    fn scan_common_directories() -> Vec<PathBuf> {
        let mut projects = Vec::new();

        if let Some(home) = dirs::home_dir() {
            let common_dirs = vec![
                home.join("Projects"),
                home.join("Developer"),
                home.join("Code"),
                home.join("dev"),
                home.join("src"),
                home.join("repos"),
                home.join("github"),
            ];

            for dir in common_dirs {
                if dir.is_dir() {
                    // Scan 2 levels deep (projects inside category folders)
                    for entry in WalkDir::new(&dir)
                        .max_depth(2)
                        .into_iter()
                        .filter_map(|e| e.ok())
                    {
                        if entry.file_type().is_dir() {
                            let path = entry.path().to_path_buf();
                            if Self::is_fabric_project(&path) {
                                projects.push(path);
                            }
                        }
                    }
                }
            }
        }

        projects
    }

    /// Parse a single chat directory into a NormalizedConversation.
    fn parse_chat(
        chat_dir: &PathBuf,
        chat_id: &str,
        project_path: &PathBuf,
    ) -> Option<NormalizedConversation> {
        // Read chat metadata (optional but helpful for title)
        let metadata_path = chat_dir.join("chat_metadata.json");
        let chat_metadata: Option<ChatMetadata> = fs::read_to_string(&metadata_path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok());

        // Read messages (required)
        let messages_path = chat_dir.join("chat_messages.json");
        let messages_content = fs::read_to_string(&messages_path).ok()?;
        let raw_messages: Vec<V2Message> = match serde_json::from_str(&messages_content) {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::debug!(
                    path = %messages_path.display(),
                    error = %e,
                    "fabric skipping malformed messages.json"
                );
                return None;
            }
        };

        if raw_messages.is_empty() {
            return None;
        }

        // Parse and normalize messages
        let mut normalized_messages = Vec::new();
        for msg in &raw_messages {
            if let Some(normalized) = Self::normalize_message(msg) {
                normalized_messages.push(normalized);
            }
        }

        if normalized_messages.is_empty() {
            return None;
        }

        // Re-assign sequential indices
        crate::connectors::reindex_messages(&mut normalized_messages);

        // Extract timestamps from first/last messages
        let started_at = raw_messages
            .first()
            .and_then(|m| Self::parse_message_timestamp(&m.timestamp));
        let ended_at = raw_messages
            .last()
            .and_then(|m| Self::parse_message_timestamp(&m.timestamp));

        // Get title from metadata or first user message
        let title = chat_metadata
            .as_ref()
            .and_then(|m| m.title.clone())
            .or_else(|| {
                raw_messages
                    .iter()
                    .find(|m| m.is_user)
                    .and_then(|m| Self::extract_user_text(&m.message_data))
                    .map(|t| {
                        // Take first line, truncate to 100 chars
                        let first_line = t.lines().next().unwrap_or(&t);
                        if first_line.len() > 100 {
                            format!("{}...", &first_line[..97])
                        } else {
                            first_line.to_string()
                        }
                    })
            });

        Some(NormalizedConversation {
            agent_slug: "fabric".to_string(),
            workspace: Some(project_path.clone()),
            external_id: Some(chat_id.to_string()),
            title,
            source_path: messages_path,
            started_at,
            ended_at,
            metadata: serde_json::json!({}), // Minimal metadata as requested
            messages: normalized_messages,
        })
    }

    /// Normalize a V2 message into a NormalizedMessage.
    fn normalize_message(msg: &V2Message) -> Option<NormalizedMessage> {
        let role = if msg.is_user {
            "user".to_string()
        } else {
            "assistant".to_string()
        };
        let created_at = Self::parse_message_timestamp(&msg.timestamp);

        let content = if msg.is_user {
            Self::extract_user_text(&msg.message_data)?
        } else {
            Self::extract_assistant_text(&msg.message_data)?
        };

        // Skip empty messages
        if content.trim().is_empty() {
            return None;
        }

        Some(NormalizedMessage {
            idx: 0, // Will be re-indexed later
            role,
            author: None, // Could extract model from metadata if needed
            created_at,
            content,
            extra: serde_json::json!({}), // Minimal metadata
            snippets: Vec::new(),
        })
    }

    /// Extract text from user message data with variants.
    fn extract_user_text(message_data: &Value) -> Option<String> {
        let data: UserMsgData = serde_json::from_value(message_data.clone()).ok()?;

        // First try to find the default variant
        if let Some((_, variant)) = data
            .variants
            .iter()
            .find(|(_, v)| v.is_default == Some(true))
        {
            return Some(variant.text.clone());
        }

        // Fallback to "original" key
        if let Some(original) = data.variants.get("original") {
            return Some(original.text.clone());
        }

        // Fallback to first variant
        if let Some((_, variant)) = data.variants.iter().next() {
            return Some(variant.text.clone());
        }

        None
    }

    /// Extract text from assistant message data by flattening response segments.
    fn extract_assistant_text(message_data: &Value) -> Option<String> {
        let data: AssistantMsgData = serde_json::from_value(message_data.clone()).ok()?;

        let mut text_parts = Vec::new();

        for segment in &data.response_content {
            if let Some(text) = Self::flatten_response_segment(segment) {
                if !text.is_empty() {
                    text_parts.push(text);
                }
            }
        }

        if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join("\n\n"))
        }
    }

    /// Flatten a single response segment into searchable text.
    fn flatten_response_segment(segment: &ResponseSegment) -> Option<String> {
        match segment.segment_type.as_str() {
            "raw_text" => {
                // Content is directly a string
                segment.content.as_str().map(|s| s.to_string())
            }
            "reasoning_block" => {
                // Extract reasoning text from nested structure
                let reasoning = segment.content.get("reasoningText")?;
                if let Some(arr) = reasoning.as_array() {
                    let parts: Vec<String> = arr
                        .iter()
                        .filter_map(|item| item.get("body").and_then(|b| b.as_str()))
                        .map(|s| s.to_string())
                        .collect();
                    if !parts.is_empty() {
                        return Some(format!("[Reasoning]\n{}", parts.join("\n")));
                    }
                }
                None
            }
            "code_block" => {
                let text = segment.content.get("text").and_then(|t| t.as_str())?;
                let lang = segment
                    .content
                    .get("language")
                    .and_then(|l| l.as_str())
                    .unwrap_or("");
                Some(format!("```{}\n{}\n```", lang, text))
            }
            "file_edit" => {
                let filename = segment
                    .content
                    .get("filename")
                    .and_then(|f| f.as_str())
                    .unwrap_or("unknown");
                let content = segment
                    .content
                    .get("editContent")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                Some(format!("[File Edit: {}]\n{}", filename, content))
            }
            "terminal_command" | "bash_command" => {
                let text = segment.content.get("text").and_then(|t| t.as_str())?;
                Some(format!("```bash\n{}\n```", text))
            }
            // Skip tool events and other internal segments
            "tool_group" | "tool_event" | "tool" => None,
            _ => {
                // For unknown types, try to extract any text content
                segment.content.as_str().map(|s| s.to_string()).or_else(|| {
                    segment
                        .content
                        .get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                })
            }
        }
    }

    /// Parse a timestamp from either ISO-8601 string or Unix milliseconds.
    fn parse_message_timestamp(value: &Value) -> Option<i64> {
        // Use the shared utility from mod.rs
        crate::connectors::parse_timestamp(value)
    }
}

impl Connector for FabricConnector {
    fn detect(&self) -> DetectionResult {
        let mut evidence = Vec::new();
        let mut project_set = HashSet::new();

        // Check app support directory
        if let Some(app_dir) = Self::app_support_dir()
            && app_dir.is_dir()
        {
            evidence.push(format!("Found Fabric app directory: {}", app_dir.display()));

            let window_projects = app_dir.join("window-projects.json");
            if window_projects.is_file() {
                evidence.push("Found window-projects.json".to_string());
            }
        }

        // Discover projects from app support (window-projects.json)
        let discovered = Self::discover_projects_from_app_support();
        for project in discovered {
            evidence.push(format!("Found Fabric project (config): {}", project.display()));
            project_set.insert(project);
        }

        // ALWAYS scan common directories to find additional projects
        // This catches projects not listed in window-projects.json
        let scanned = Self::scan_common_directories();
        for project in scanned {
            if project_set.insert(project.clone()) {
                evidence.push(format!(
                    "Found Fabric project (scanned): {}",
                    project.display()
                ));
            }
        }

        let root_paths: Vec<PathBuf> = project_set.into_iter().collect();

        DetectionResult {
            detected: !root_paths.is_empty(),
            evidence,
            root_paths,
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = Vec::new();

        // Determine which roots to scan
        let roots: Vec<PathBuf> = if ctx.use_default_detection() {
            // Use automatic detection - combine BOTH methods
            let mut project_set = HashSet::new();
            for p in Self::discover_projects_from_app_support() {
                project_set.insert(p);
            }
            for p in Self::scan_common_directories() {
                project_set.insert(p);
            }
            project_set.into_iter().collect()
        } else {
            // Use explicit scan roots from config
            ctx.scan_roots.iter().map(|r| r.path.clone()).collect()
        };

        let mut project_count = 0;
        for project_path in roots {
            if !Self::is_fabric_project(&project_path) {
                continue;
            }

            project_count += 1;
            if project_count <= 3 {
                tracing::debug!(path = %project_path.display(), "fabric scanning project");
            }

            let chats_dir = project_path.join(".fabric").join("chats");
            if !chats_dir.is_dir() {
                continue;
            }

            // Iterate over chat directories
            let entries = match fs::read_dir(&chats_dir) {
                Ok(e) => e,
                Err(e) => {
                    tracing::debug!(
                        path = %chats_dir.display(),
                        error = %e,
                        "fabric failed to read chats directory"
                    );
                    continue;
                }
            };

            for entry in entries.filter_map(|e| e.ok()) {
                let chat_dir = entry.path();

                if !chat_dir.is_dir() {
                    continue;
                }

                // Extract chat ID from directory name (chat-{uuid})
                let dir_name = chat_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !dir_name.starts_with("chat-") {
                    continue;
                }
                let chat_id = dir_name.strip_prefix("chat-").unwrap_or(dir_name);

                // Check modification time for incremental indexing
                let messages_path = chat_dir.join("chat_messages.json");
                if !crate::connectors::file_modified_since(&messages_path, ctx.since_ts) {
                    continue;
                }

                // Parse the chat
                if let Some(conv) = Self::parse_chat(&chat_dir, chat_id, &project_path) {
                    conversations.push(conv);
                }
            }
        }

        tracing::info!(
            projects = project_count,
            conversations = conversations.len(),
            "fabric scan complete"
        );

        Ok(conversations)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_project(temp: &TempDir) -> PathBuf {
        let project = temp.path().join("test-project");
        let fabric_dir = project.join(".fabric");
        let chats_dir = fabric_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();

        // Create project metadata
        let metadata = serde_json::json!({
            "UUID": "test-uuid-1234",
            "originalRootPath": project.to_str().unwrap(),
            "creationDate": "2025-01-01T00:00:00.000Z"
        });
        fs::write(
            fabric_dir.join("project-metadata.json"),
            serde_json::to_string(&metadata).unwrap(),
        )
        .unwrap();

        project
    }

    fn create_test_chat(chats_dir: &PathBuf, chat_id: &str, messages: &[Value]) {
        let chat_dir = chats_dir.join(format!("chat-{}", chat_id));
        fs::create_dir_all(&chat_dir).unwrap();

        let metadata = serde_json::json!({
            "id": chat_id,
            "title": "Test Chat",
            "lastMessage": "Last message preview...",
            "timestamp": 1704067200000_i64,
            "isActive": false,
            "createdAt": 1704067200000_i64,
            "updatedAt": 1704067200000_i64
        });
        fs::write(
            chat_dir.join("chat_metadata.json"),
            serde_json::to_string(&metadata).unwrap(),
        )
        .unwrap();

        fs::write(
            chat_dir.join("chat_messages.json"),
            serde_json::to_string(&messages).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn test_is_fabric_project() {
        let temp = TempDir::new().unwrap();
        let project = create_test_project(&temp);

        assert!(FabricConnector::is_fabric_project(&project));
        assert!(!FabricConnector::is_fabric_project(
            &temp.path().to_path_buf()
        ));
    }

    #[test]
    fn test_extract_user_text_with_default_variant() {
        let message_data = serde_json::json!({
            "variants": {
                "original": {
                    "text": "Hello world",
                    "isDefault": true,
                    "createdAt": "2025-01-01T00:00:00.000Z"
                }
            },
            "images": []
        });

        let text = FabricConnector::extract_user_text(&message_data);
        assert_eq!(text, Some("Hello world".to_string()));
    }

    #[test]
    fn test_extract_user_text_with_multiple_variants() {
        let message_data = serde_json::json!({
            "variants": {
                "original": {
                    "text": "Original text",
                    "isDefault": false,
                    "createdAt": "2025-01-01T00:00:00.000Z"
                },
                "edited": {
                    "text": "Edited text",
                    "isDefault": true,
                    "createdAt": "2025-01-01T00:01:00.000Z"
                }
            },
            "images": []
        });

        let text = FabricConnector::extract_user_text(&message_data);
        assert_eq!(text, Some("Edited text".to_string()));
    }

    #[test]
    fn test_extract_user_text_fallback_to_original() {
        let message_data = serde_json::json!({
            "variants": {
                "original": {
                    "text": "Original text",
                    "createdAt": "2025-01-01T00:00:00.000Z"
                }
            },
            "images": []
        });

        let text = FabricConnector::extract_user_text(&message_data);
        assert_eq!(text, Some("Original text".to_string()));
    }

    #[test]
    fn test_extract_assistant_text_raw() {
        let message_data = serde_json::json!({
            "processingTime": 1000,
            "responseContent": [
                { "type": "raw_text", "content": "Hello from assistant" }
            ]
        });

        let text = FabricConnector::extract_assistant_text(&message_data);
        assert_eq!(text, Some("Hello from assistant".to_string()));
    }

    #[test]
    fn test_extract_assistant_text_with_reasoning() {
        let message_data = serde_json::json!({
            "processingTime": 1000,
            "responseContent": [
                {
                    "type": "reasoning_block",
                    "content": {
                        "reasoningText": [
                            { "body": "Thinking about this..." }
                        ],
                        "reasoningTime": 5
                    }
                },
                { "type": "raw_text", "content": "Here is my response" }
            ]
        });

        let text = FabricConnector::extract_assistant_text(&message_data);
        assert!(text.is_some());
        let text = text.unwrap();
        assert!(text.contains("[Reasoning]"));
        assert!(text.contains("Thinking about this"));
        assert!(text.contains("Here is my response"));
    }

    #[test]
    fn test_extract_assistant_text_with_code_block() {
        let message_data = serde_json::json!({
            "processingTime": 1000,
            "responseContent": [
                {
                    "type": "code_block",
                    "content": {
                        "language": "rust",
                        "text": "fn main() {}"
                    }
                }
            ]
        });

        let text = FabricConnector::extract_assistant_text(&message_data);
        assert!(text.is_some());
        let text = text.unwrap();
        assert!(text.contains("```rust"));
        assert!(text.contains("fn main() {}"));
    }

    #[test]
    fn test_flatten_tool_group_returns_none() {
        let segment = ResponseSegment {
            segment_type: "tool_group".to_string(),
            content: serde_json::json!({ "tools": [] }),
        };

        assert!(FabricConnector::flatten_response_segment(&segment).is_none());
    }

    #[test]
    fn test_parse_timestamp_string() {
        let ts = serde_json::json!("2025-01-01T00:00:00.000Z");
        let parsed = FabricConnector::parse_message_timestamp(&ts);
        assert!(parsed.is_some());
    }

    #[test]
    fn test_parse_timestamp_number() {
        let ts = serde_json::json!(1704067200000_i64);
        let parsed = FabricConnector::parse_message_timestamp(&ts);
        assert_eq!(parsed, Some(1704067200000));
    }

    #[test]
    fn test_full_chat_parsing() {
        let temp = TempDir::new().unwrap();
        let project = create_test_project(&temp);
        let chats_dir = project.join(".fabric").join("chats");

        let messages = vec![
            serde_json::json!({
                "id": "msg1",
                "format_ver": "v2",
                "parent_id": null,
                "children_id": ["msg2"],
                "isUser": true,
                "timestamp": "2025-01-01T00:00:00.000Z",
                "messageData": {
                    "variants": {
                        "original": {
                            "text": "Hello",
                            "isDefault": true,
                            "createdAt": "2025-01-01T00:00:00.000Z"
                        }
                    },
                    "images": []
                }
            }),
            serde_json::json!({
                "id": "msg2",
                "format_ver": "v2",
                "parent_id": "msg1",
                "children_id": [],
                "isUser": false,
                "timestamp": "2025-01-01T00:00:01.000Z",
                "messageData": {
                    "processingTime": 1000,
                    "responseContent": [
                        { "type": "raw_text", "content": "Hi there!" }
                    ]
                }
            }),
        ];

        create_test_chat(&chats_dir, "test-chat-id", &messages);

        let chat_dir = chats_dir.join("chat-test-chat-id");
        let conv = FabricConnector::parse_chat(&chat_dir, "test-chat-id", &project);

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.agent_slug, "fabric");
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].content, "Hello");
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[1].content, "Hi there!");
        assert_eq!(conv.messages[1].role, "assistant");
    }

    #[test]
    fn test_empty_chat_returns_none() {
        let temp = TempDir::new().unwrap();
        let project = create_test_project(&temp);
        let chats_dir = project.join(".fabric").join("chats");

        create_test_chat(&chats_dir, "empty-chat", &[]);

        let chat_dir = chats_dir.join("chat-empty-chat");
        let conv = FabricConnector::parse_chat(&chat_dir, "empty-chat", &project);

        assert!(conv.is_none());
    }

    #[test]
    fn test_detection_result_not_found() {
        // This tests when no Fabric projects exist
        let result = DetectionResult::not_found();
        assert!(!result.detected);
        assert!(result.evidence.is_empty());
    }
}
