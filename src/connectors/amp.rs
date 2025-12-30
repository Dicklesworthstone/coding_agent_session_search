use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

pub struct AmpConnector;
impl Default for AmpConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl AmpConnector {
    pub fn new() -> Self {
        Self
    }

    fn cache_root() -> PathBuf {
        // Check XDG_DATA_HOME first (important for testing and cross-platform consistency)
        // Note: dirs::data_dir() on macOS ignores XDG_DATA_HOME
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            return PathBuf::from(xdg).join("amp");
        }
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("amp")
    }

    fn vscode_global_storage() -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join(".config/Code/User/globalStorage/sourcegraph.amp"));
            roots.push(
                home.join("Library/Application Support/Code/User/globalStorage/sourcegraph.amp"),
            );
            roots.push(home.join("AppData/Roaming/Code/User/globalStorage/sourcegraph.amp"));
        }
        roots
    }

    pub fn candidate_roots() -> Vec<PathBuf> {
        let mut roots = vec![Self::cache_root()];
        roots.extend(Self::vscode_global_storage());
        roots
    }
}

impl Connector for AmpConnector {
    fn detect(&self) -> DetectionResult {
        let existing_roots: Vec<PathBuf> = Self::candidate_roots()
            .into_iter()
            .filter(|r| r.exists())
            .collect();

        let evidence: Vec<String> = existing_roots
            .iter()
            .map(|r| format!("found {}", r.display()))
            .collect();

        if evidence.is_empty() {
            DetectionResult::not_found()
        } else {
            DetectionResult {
                detected: true,
                evidence,
                root_paths: existing_roots,
            }
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut convs = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        let looks_like_root = |path: &PathBuf| {
            path.file_name()
                .is_some_and(|n| n.to_str().unwrap_or("").contains("amp"))
                || std::fs::read_dir(path)
                    .map(|mut d| d.any(|e| e.ok().is_some_and(|e| is_amp_log_file(&e.path()))))
                    .unwrap_or(false)
        };

        // allow tests to override via ctx.data_dir
        let roots = if ctx.use_default_detection() {
            if looks_like_root(&ctx.data_dir) {
                vec![ctx.data_dir.clone()]
            } else {
                Self::candidate_roots()
            }
        } else {
            if !looks_like_root(&ctx.data_dir) {
                return Ok(Vec::new());
            }
            vec![ctx.data_dir.clone()]
        };

        for root in roots {
            if !root.exists() {
                continue;
            }

            for entry in WalkDir::new(&root).into_iter().flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                if !is_amp_log_file(path) {
                    continue;
                }
                let text = match std::fs::read_to_string(path) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let val: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                // For incremental indexing, check both file mtime AND message timestamps.
                // amp files may not update their mtime when new messages are added,
                // so we also check if any message in the file is newer than last scan.
                let should_process = if let Some(ts) = ctx.since_ts {
                    crate::connectors::file_modified_since(path, ctx.since_ts) 
                        || has_message_since(&val, Some(ts))
                } else {
                    true
                };

                if !should_process {
                    continue;
                }

                if let Some(messages) = extract_messages(&val, ctx.since_ts) {
                    if messages.is_empty() {
                        continue;
                    }
                    let title = val
                        .get("title")
                        .and_then(|v| v.as_str())
                        .map(std::string::ToString::to_string)
                        .or_else(|| {
                            messages
                                .first()
                                .and_then(|m| m.content.lines().next())
                                .map(std::string::ToString::to_string)
                        });

                    let workspace = infer_workspace(&val).or_else(|| {
                        messages.iter().find_map(|m| {
                            m.extra
                                .get("workspace")
                                .and_then(|w| w.as_str())
                                .map(PathBuf::from)
                        })
                    });

                    let external_id = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(std::string::ToString::to_string)
                        .or_else(|| {
                            val.get("id")
                                .and_then(|v| v.as_str())
                                .map(std::string::ToString::to_string)
                        });

                    let key = external_id.clone().map_or_else(
                        || format!("amp:{}", path.display()),
                        |id| format!("amp:{id}"),
                    );
                    if seen_ids.insert(key) {
                        convs.push(NormalizedConversation {
                            agent_slug: "amp".into(),
                            external_id,
                            title,
                            workspace,
                            source_path: path.to_path_buf(),
                            started_at: messages.first().and_then(|m| m.created_at),
                            ended_at: messages.last().and_then(|m| m.created_at),
                            metadata: val.clone(),
                            messages,
                        });
                        tracing::info!(
                            target: "connector::amp",
                            source = %path.display(),
                            messages = convs.last().map_or(0, |c| c.messages.len()),
                            since_ts = ctx.since_ts,
                            "amp_scan"
                        );
                    }
                }
            }
        }

        Ok(convs)
    }
}

/// Extract message timestamp from a message object.
/// amp stores message timestamps in meta.sentAt.
fn extract_message_timestamp(m: &Value) -> Option<i64> {
    m.get("meta")
        .and_then(|v| v.as_object())
        .and_then(|meta| meta.get("sentAt"))
        .and_then(crate::connectors::parse_timestamp)
        .or_else(|| m.get("created_at").and_then(crate::connectors::parse_timestamp))
        .or_else(|| m.get("createdAt").and_then(crate::connectors::parse_timestamp))
        .or_else(|| m.get("timestamp").and_then(crate::connectors::parse_timestamp))
        .or_else(|| m.get("ts").and_then(crate::connectors::parse_timestamp))
}

/// Check if a file contains any message with a timestamp newer than since_ts.
/// This is used for incremental indexing when file mtime may not reflect content updates.
fn has_message_since(val: &Value, since_ts: Option<i64>) -> bool {
    let since_ts = match since_ts {
        None => return true, // No filter
        Some(ts) => ts,
    };

    let msgs = val
        .get("messages")
        .and_then(|m| m.as_array())
        .or_else(|| {
            val.get("thread")
                .and_then(|t| t.get("messages"))
                .and_then(|m| m.as_array())
        });

    if let Some(messages) = msgs {
        for m in messages {
            if let Some(ts) = extract_message_timestamp(m) {
                // Check with 1-second slack window like file_modified_since does
                if ts >= (since_ts.saturating_sub(1_000)) {
                    return true;
                }
            }
        }
    }

    false
}

fn extract_messages(val: &Value, _since_ts: Option<i64>) -> Option<Vec<NormalizedMessage>> {
    let msgs = val
        .get("messages")
        .and_then(|m| m.as_array().cloned())
        .or_else(|| {
            val.get("thread")
                .and_then(|t| t.get("messages"))
                .and_then(|m| m.as_array().cloned())
        })?;

    let mut out = Vec::new();
    for m in msgs {
        let role = m
            .get("role")
            .or_else(|| m.get("speaker"))
            .or_else(|| m.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("agent")
            .to_string();
        // Handle both array format (content: [{type: "text", text: "..."}]) and string format
        let content = if let Some(content_array) = m.get("content").and_then(|v| v.as_array()) {
            content_array
                .iter()
                .filter_map(|block| {
                    if block.get("type") == Some(&serde_json::Value::String("text".to_string())) {
                        block
                            .get("text")
                            .or_else(|| block.get("body"))
                            .and_then(|t| t.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("")
        } else {
            m.get("content")
                .or_else(|| m.get("text"))
                .or_else(|| m.get("body"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };

        if content.trim().is_empty() {
            continue;
        }

        let created_at = extract_message_timestamp(&m);
        let author = m
            .get("author")
            .or_else(|| m.get("sender"))
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string);

        // NOTE: Do NOT filter individual messages by timestamp here!
        // The file-level check in file_modified_since() is sufficient.
        // Filtering messages would cause older messages to be lost when
        // the file is re-indexed after new messages are added.

        out.push(NormalizedMessage {
            idx: 0, // Will be re-assigned after filtering
            role,
            author,
            created_at,
            content,
            extra: m.clone(),
            snippets: Vec::new(),
        });
    }

    // Re-assign indices after filtering to maintain sequential order
    for (i, msg) in out.iter_mut().enumerate() {
        msg.idx = i as i64;
    }

    if out.is_empty() { None } else { Some(out) }
}

fn infer_workspace(val: &Value) -> Option<PathBuf> {
    let keys = ["workspace", "cwd", "path", "project_path", "repo", "root"];
    for k in keys {
        if let Some(p) = val.get(k).and_then(|v| v.as_str()) {
            return Some(PathBuf::from(p));
        }
    }
    None
}

fn is_amp_log_file(path: &std::path::Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("json") {
        return false;
    }
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        let stem_lower = stem.to_ascii_lowercase();
        // Match known patterns: thread, conversation, chat
        if stem_lower.contains("thread")
            || stem_lower.contains("conversation")
            || stem_lower.contains("chat")
        {
            return true;
        }
        // Match Amp's T-{uuid}.json format (e.g., T-01872a67-152b-46af-a1af-4de6fce3d2b3.json)
        if stem_lower.starts_with("t-") && looks_like_uuid(&stem[2..]) {
            return true;
        }
    }
    // Also match any .json file in a "threads" directory
    if let Some(parent) = path.parent()
        && let Some(dir_name) = parent.file_name().and_then(|n| n.to_str())
        && dir_name == "threads"
    {
        return true;
    }
    false
}

/// Check if a string looks like a UUID (8-4-4-4-12 hex pattern)
fn looks_like_uuid(s: &str) -> bool {
    // UUID format: 8-4-4-4-12 (32 hex chars + 4 dashes = 36 chars)
    if s.len() != 36 {
        return false;
    }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let expected_lens = [8, 4, 4, 4, 12];
    for (part, &expected_len) in parts.iter().zip(expected_lens.iter()) {
        if part.len() != expected_len || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    // =====================================================
    // Constructor Tests
    // =====================================================

    #[test]
    fn new_creates_connector() {
        let connector = AmpConnector::new();
        let _ = connector;
    }

    #[test]
    fn default_creates_connector() {
        let connector = AmpConnector;
        let _ = connector;
    }

    // =====================================================
    // is_amp_log_file() Tests
    // =====================================================

    #[test]
    fn is_amp_log_file_matches_thread_json() {
        assert!(is_amp_log_file(std::path::Path::new("thread.json")));
        assert!(is_amp_log_file(std::path::Path::new("my-thread.json")));
        assert!(is_amp_log_file(std::path::Path::new("Thread_123.json")));
    }

    #[test]
    fn is_amp_log_file_matches_conversation_json() {
        assert!(is_amp_log_file(std::path::Path::new("conversation.json")));
        assert!(is_amp_log_file(std::path::Path::new(
            "conversation-2025-12-17.json"
        )));
        assert!(is_amp_log_file(std::path::Path::new("CONVERSATION.json")));
    }

    #[test]
    fn is_amp_log_file_matches_chat_json() {
        assert!(is_amp_log_file(std::path::Path::new("chat.json")));
        assert!(is_amp_log_file(std::path::Path::new("chat-session.json")));
        assert!(is_amp_log_file(std::path::Path::new("Chat_Log.json")));
    }

    #[test]
    fn is_amp_log_file_rejects_non_json() {
        assert!(!is_amp_log_file(std::path::Path::new("thread.txt")));
        assert!(!is_amp_log_file(std::path::Path::new("conversation.xml")));
        assert!(!is_amp_log_file(std::path::Path::new("chat")));
    }

    #[test]
    fn is_amp_log_file_rejects_wrong_stems() {
        assert!(!is_amp_log_file(std::path::Path::new("config.json")));
        assert!(!is_amp_log_file(std::path::Path::new("settings.json")));
        assert!(!is_amp_log_file(std::path::Path::new("data.json")));
    }

    #[test]
    fn is_amp_log_file_matches_uuid_format() {
        // Amp stores files as T-{uuid}.json
        assert!(is_amp_log_file(std::path::Path::new(
            "T-01872a67-152b-46af-a1af-4de6fce3d2b3.json"
        )));
        assert!(is_amp_log_file(std::path::Path::new(
            "t-abcdef12-3456-7890-abcd-ef1234567890.json"
        )));
    }

    #[test]
    fn is_amp_log_file_rejects_invalid_uuid() {
        // T- prefix but not a valid UUID
        assert!(!is_amp_log_file(std::path::Path::new("T-not-a-uuid.json")));
        assert!(!is_amp_log_file(std::path::Path::new("T-12345.json")));
    }

    #[test]
    fn is_amp_log_file_matches_threads_directory() {
        // Any .json in a "threads" directory should match
        assert!(is_amp_log_file(std::path::Path::new(
            "/home/user/.local/share/amp/threads/random-file.json"
        )));
        assert!(is_amp_log_file(std::path::Path::new(
            "threads/any-name.json"
        )));
    }

    #[test]
    fn looks_like_uuid_valid_uuids() {
        assert!(looks_like_uuid("01872a67-152b-46af-a1af-4de6fce3d2b3"));
        assert!(looks_like_uuid("abcdef12-3456-7890-abcd-ef1234567890"));
        assert!(looks_like_uuid("00000000-0000-0000-0000-000000000000"));
        assert!(looks_like_uuid("ABCDEF12-3456-7890-ABCD-EF1234567890"));
    }

    #[test]
    fn looks_like_uuid_invalid() {
        assert!(!looks_like_uuid("not-a-uuid"));
        assert!(!looks_like_uuid("12345"));
        assert!(!looks_like_uuid(""));
        assert!(!looks_like_uuid("01872a67-152b-46af-a1af-4de6fce3d2b")); // too short
        assert!(!looks_like_uuid("01872a67-152b-46af-a1af-4de6fce3d2b33")); // too long
        assert!(!looks_like_uuid("0187zzzz-152b-46af-a1af-4de6fce3d2b3")); // non-hex
    }

    // =====================================================
    // infer_workspace() Tests
    // =====================================================

    #[test]
    fn infer_workspace_from_workspace_key() {
        let val = json!({"workspace": "/home/user/project"});
        assert_eq!(
            infer_workspace(&val),
            Some(PathBuf::from("/home/user/project"))
        );
    }

    #[test]
    fn infer_workspace_from_cwd_key() {
        let val = json!({"cwd": "/home/user/cwd-project"});
        assert_eq!(
            infer_workspace(&val),
            Some(PathBuf::from("/home/user/cwd-project"))
        );
    }

    #[test]
    fn infer_workspace_from_path_key() {
        let val = json!({"path": "/home/user/path-project"});
        assert_eq!(
            infer_workspace(&val),
            Some(PathBuf::from("/home/user/path-project"))
        );
    }

    #[test]
    fn infer_workspace_from_project_path_key() {
        let val = json!({"project_path": "/home/user/proj"});
        assert_eq!(
            infer_workspace(&val),
            Some(PathBuf::from("/home/user/proj"))
        );
    }

    #[test]
    fn infer_workspace_from_repo_key() {
        let val = json!({"repo": "/home/user/repo"});
        assert_eq!(
            infer_workspace(&val),
            Some(PathBuf::from("/home/user/repo"))
        );
    }

    #[test]
    fn infer_workspace_from_root_key() {
        let val = json!({"root": "/home/user/root"});
        assert_eq!(
            infer_workspace(&val),
            Some(PathBuf::from("/home/user/root"))
        );
    }

    #[test]
    fn infer_workspace_returns_none_when_no_match() {
        let val = json!({"title": "Test", "id": "123"});
        assert!(infer_workspace(&val).is_none());
    }

    #[test]
    fn infer_workspace_prefers_workspace_key() {
        let val = json!({
            "workspace": "/workspace",
            "cwd": "/cwd",
            "path": "/path"
        });
        assert_eq!(infer_workspace(&val), Some(PathBuf::from("/workspace")));
    }

    // =====================================================
    // extract_messages() Tests
    // =====================================================

    #[test]
    fn extract_messages_from_messages_array() {
        let val = json!({
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": "Hi there!"}
            ]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content, "Hello");
        assert_eq!(msgs[1].role, "assistant");
    }

    #[test]
    fn extract_messages_from_thread_messages() {
        let val = json!({
            "thread": {
                "messages": [
                    {"role": "user", "content": "Question?"},
                    {"role": "assistant", "content": "Answer!"}
                ]
            }
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "Question?");
    }

    #[test]
    fn extract_messages_uses_speaker_as_role() {
        let val = json!({
            "messages": [{"speaker": "human", "content": "Test"}]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].role, "human");
    }

    #[test]
    fn extract_messages_uses_type_as_role() {
        let val = json!({
            "messages": [{"type": "userMessage", "content": "Test"}]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].role, "userMessage");
    }

    #[test]
    fn extract_messages_uses_text_as_content() {
        let val = json!({
            "messages": [{"role": "user", "text": "Text content"}]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].content, "Text content");
    }

    #[test]
    fn extract_messages_uses_body_as_content() {
        let val = json!({
            "messages": [{"role": "user", "body": "Body content"}]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].content, "Body content");
    }

    #[test]
    fn extract_messages_handles_content_array_format() {
        let val = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Hello "},
                    {"type": "text", "text": "World"}
                ]
            }]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].content, "Hello World");
    }

    #[test]
    fn extract_messages_handles_content_array_with_body_fallback() {
        let val = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "body": "Text from body"}
                ]
            }]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].content, "Text from body");
    }

    #[test]
    fn extract_messages_skips_non_text_blocks_in_array() {
        let val = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "code", "text": "ignored code"},
                    {"type": "text", "text": "kept text"}
                ]
            }]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].content, "kept text");
    }

    #[test]
    fn extract_messages_skips_empty_content() {
        let val = json!({
            "messages": [
                {"role": "user", "content": "Valid"},
                {"role": "assistant", "content": ""},
                {"role": "assistant", "content": "   "}
            ]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "Valid");
    }

    #[test]
    fn extract_messages_parses_sent_at_from_meta() {
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "meta": {"sentAt": 1733000000}}]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].created_at, Some(1733000000));
    }

    #[test]
    fn extract_messages_parses_created_at() {
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "created_at": 1733000000}]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].created_at, Some(1733000000));
    }

    #[test]
    fn extract_messages_parses_created_at_camel_case() {
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "createdAt": 1733000001}]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].created_at, Some(1733000001));
    }

    #[test]
    fn extract_messages_parses_timestamp() {
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "timestamp": 1733000002}]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].created_at, Some(1733000002));
    }

    #[test]
    fn extract_messages_parses_ts() {
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "ts": 1733000003}]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].created_at, Some(1733000003));
    }

    #[test]
    fn extract_messages_parses_author() {
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "author": "john"}]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].author, Some("john".to_string()));
    }

    #[test]
    fn extract_messages_parses_sender_as_author() {
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "sender": "jane"}]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].author, Some("jane".to_string()));
    }

    #[test]
    fn extract_messages_assigns_sequential_indices() {
        let val = json!({
            "messages": [
                {"role": "user", "content": "First"},
                {"role": "assistant", "content": "Second"},
                {"role": "user", "content": "Third"}
            ]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].idx, 0);
        assert_eq!(msgs[1].idx, 1);
        assert_eq!(msgs[2].idx, 2);
    }

    #[test]
    fn extract_messages_defaults_role_to_agent() {
        let val = json!({
            "messages": [{"content": "No role"}]
        });
        let msgs = extract_messages(&val, None).unwrap();
        assert_eq!(msgs[0].role, "agent");
    }

    #[test]
    fn extract_messages_returns_none_for_empty() {
        let val = json!({"messages": []});
        assert!(extract_messages(&val, None).is_none());
    }

    #[test]
    fn extract_messages_returns_none_for_missing() {
        let val = json!({"title": "No messages"});
        assert!(extract_messages(&val, None).is_none());
    }

    // =====================================================
    // scan() Tests
    // =====================================================

    fn create_amp_dir(dir: &TempDir) -> PathBuf {
        let amp_dir = dir.path().join("amp");
        fs::create_dir_all(&amp_dir).unwrap();
        amp_dir
    }

    #[test]
    fn scan_parses_simple_conversation() {
        let dir = TempDir::new().unwrap();
        let amp_dir = create_amp_dir(&dir);

        let content = json!({
            "title": "Test Thread",
            "workspace": "/home/user/project",
            "messages": [
                {"role": "user", "content": "Hello Amp!"},
                {"role": "assistant", "content": "Hello! How can I help?"}
            ]
        });
        fs::write(amp_dir.join("thread.json"), content.to_string()).unwrap();

        let connector = AmpConnector::new();
        let ctx = ScanContext::local_default(amp_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].title, Some("Test Thread".to_string()));
        assert_eq!(
            convs[0].workspace,
            Some(PathBuf::from("/home/user/project"))
        );
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[0].content, "Hello Amp!");
    }

    #[test]
    fn scan_handles_multiple_files() {
        let dir = TempDir::new().unwrap();
        let amp_dir = create_amp_dir(&dir);

        let content1 = json!({
            "messages": [{"role": "user", "content": "Session 1"}]
        });
        let content2 = json!({
            "messages": [{"role": "user", "content": "Session 2"}]
        });
        fs::write(amp_dir.join("thread-1.json"), content1.to_string()).unwrap();
        fs::write(amp_dir.join("conversation-2.json"), content2.to_string()).unwrap();

        let connector = AmpConnector::new();
        let ctx = ScanContext::local_default(amp_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 2);
    }

    #[test]
    fn scan_handles_empty_directory() {
        let dir = TempDir::new().unwrap();
        let amp_dir = create_amp_dir(&dir);

        let connector = AmpConnector::new();
        let ctx = ScanContext::local_default(amp_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 0);
    }

    #[test]
    fn scan_skips_non_matching_files() {
        let dir = TempDir::new().unwrap();
        let amp_dir = create_amp_dir(&dir);

        let content = json!({"messages": [{"role": "user", "content": "Test"}]});
        fs::write(amp_dir.join("config.json"), content.to_string()).unwrap();
        fs::write(amp_dir.join("settings.json"), content.to_string()).unwrap();

        let connector = AmpConnector::new();
        let ctx = ScanContext::local_default(amp_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 0);
    }

    #[test]
    fn scan_extracts_title_from_first_message_if_missing() {
        let dir = TempDir::new().unwrap();
        let amp_dir = create_amp_dir(&dir);

        let content = json!({
            "messages": [
                {"role": "user", "content": "First line\nSecond line"},
                {"role": "assistant", "content": "Response"}
            ]
        });
        fs::write(amp_dir.join("chat.json"), content.to_string()).unwrap();

        let connector = AmpConnector::new();
        let ctx = ScanContext::local_default(amp_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].title, Some("First line".to_string()));
    }

    #[test]
    fn scan_sets_agent_slug_to_amp() {
        let dir = TempDir::new().unwrap();
        let amp_dir = create_amp_dir(&dir);

        let content = json!({"messages": [{"role": "user", "content": "Test"}]});
        fs::write(amp_dir.join("thread.json"), content.to_string()).unwrap();

        let connector = AmpConnector::new();
        let ctx = ScanContext::local_default(amp_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].agent_slug, "amp");
    }

    #[test]
    fn scan_uses_file_stem_as_external_id() {
        let dir = TempDir::new().unwrap();
        let amp_dir = create_amp_dir(&dir);

        let content = json!({"messages": [{"role": "user", "content": "Test"}]});
        fs::write(amp_dir.join("my-thread-123.json"), content.to_string()).unwrap();

        let connector = AmpConnector::new();
        let ctx = ScanContext::local_default(amp_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].external_id, Some("my-thread-123".to_string()));
    }

    #[test]
    fn scan_extracts_timestamps_from_messages() {
        let dir = TempDir::new().unwrap();
        let amp_dir = create_amp_dir(&dir);

        let content = json!({
            "messages": [
                {"role": "user", "content": "First", "meta": {"sentAt": 1733000000}},
                {"role": "assistant", "content": "Last", "meta": {"sentAt": 1733000100}}
            ]
        });
        fs::write(amp_dir.join("thread.json"), content.to_string()).unwrap();

        let connector = AmpConnector::new();
        let ctx = ScanContext::local_default(amp_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].started_at, Some(1733000000));
        assert_eq!(convs[0].ended_at, Some(1733000100));
    }

    #[test]
    fn scan_skips_invalid_json() {
        let dir = TempDir::new().unwrap();
        let amp_dir = create_amp_dir(&dir);

        fs::write(amp_dir.join("thread.json"), "not valid json").unwrap();

        let connector = AmpConnector::new();
        let ctx = ScanContext::local_default(amp_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 0);
    }

    #[test]
    fn scan_skips_files_without_messages() {
        let dir = TempDir::new().unwrap();
        let amp_dir = create_amp_dir(&dir);

        let content = json!({"title": "Empty Thread"});
        fs::write(amp_dir.join("thread.json"), content.to_string()).unwrap();

        let connector = AmpConnector::new();
        let ctx = ScanContext::local_default(amp_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 0);
    }

    #[test]
    fn scan_handles_thread_nested_messages() {
        let dir = TempDir::new().unwrap();
        let amp_dir = create_amp_dir(&dir);

        let content = json!({
            "thread": {
                "messages": [
                    {"role": "user", "content": "Nested message"}
                ]
            }
        });
        fs::write(amp_dir.join("thread.json"), content.to_string()).unwrap();

        let connector = AmpConnector::new();
        let ctx = ScanContext::local_default(amp_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages[0].content, "Nested message");
    }

    #[test]
    fn scan_deduplicates_by_external_id() {
        let dir = TempDir::new().unwrap();
        let amp_dir = create_amp_dir(&dir);

        // Create same file in nested directory
        let nested = amp_dir.join("nested");
        fs::create_dir_all(&nested).unwrap();

        let content = json!({
            "id": "same-id",
            "messages": [{"role": "user", "content": "Test"}]
        });
        fs::write(amp_dir.join("thread-same-id.json"), content.to_string()).unwrap();
        fs::write(nested.join("thread-same-id.json"), content.to_string()).unwrap();

        let connector = AmpConnector::new();
        let ctx = ScanContext::local_default(amp_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Should have at least 1 (deduplication happens by external_id)
        assert!(!convs.is_empty());
    }

    #[test]
    fn scan_stores_source_path() {
        let dir = TempDir::new().unwrap();
        let amp_dir = create_amp_dir(&dir);

        let content = json!({"messages": [{"role": "user", "content": "Test"}]});
        let file_path = amp_dir.join("thread.json");
        fs::write(&file_path, content.to_string()).unwrap();

        let connector = AmpConnector::new();
        let ctx = ScanContext::local_default(amp_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].source_path, file_path);
    }

    #[test]
    fn scan_infers_workspace_from_message_extra() {
        let dir = TempDir::new().unwrap();
        let amp_dir = create_amp_dir(&dir);

        let content = json!({
            "messages": [
                {"role": "user", "content": "Test", "workspace": "/msg/workspace"}
            ]
        });
        fs::write(amp_dir.join("thread.json"), content.to_string()).unwrap();

        let connector = AmpConnector::new();
        let ctx = ScanContext::local_default(amp_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].workspace, Some(PathBuf::from("/msg/workspace")));
    }

    #[test]
    fn scan_stores_full_json_as_metadata() {
        let dir = TempDir::new().unwrap();
        let amp_dir = create_amp_dir(&dir);

        let content = json!({
            "title": "Meta Test",
            "custom_field": "custom_value",
            "messages": [{"role": "user", "content": "Test"}]
        });
        fs::write(amp_dir.join("thread.json"), content.to_string()).unwrap();

        let connector = AmpConnector::new();
        let ctx = ScanContext::local_default(amp_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].metadata["title"], "Meta Test");
        assert_eq!(convs[0].metadata["custom_field"], "custom_value");
    }

    // =====================================================
    // candidate_roots() Tests
    // =====================================================

    #[test]
    fn candidate_roots_returns_non_empty_list() {
        let roots = AmpConnector::candidate_roots();
        assert!(!roots.is_empty());
    }

    #[test]
    fn candidate_roots_includes_cache_root() {
        let roots = AmpConnector::candidate_roots();
        let cache = AmpConnector::cache_root();
        assert!(roots.contains(&cache));
    }

    // =====================================================
    // has_message_since() Tests
    // =====================================================

    #[test]
    fn has_message_since_returns_true_when_no_filter() {
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "timestamp": 1000}]
        });
        assert!(has_message_since(&val, None));
    }

    #[test]
    fn has_message_since_detects_newer_message() {
        let now = crate::storage::sqlite::SqliteStorage::now_millis();
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "meta": {"sentAt": now}}]
        });
        assert!(has_message_since(&val, Some(now - 10_000))); // message is newer than filter
    }

    #[test]
    fn has_message_since_ignores_older_messages() {
        let now = crate::storage::sqlite::SqliteStorage::now_millis();
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "meta": {"sentAt": now - 100_000}}]
        });
        assert!(!has_message_since(&val, Some(now))); // message is older than filter
    }

    #[test]
    fn has_message_since_with_slack_window() {
        let now = crate::storage::sqlite::SqliteStorage::now_millis();
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "meta": {"sentAt": now - 500}}]
        });
        // Should return true due to 1-second slack window
        assert!(has_message_since(&val, Some(now)));
    }

    #[test]
    fn has_message_since_checks_sent_at_in_meta() {
        let now = crate::storage::sqlite::SqliteStorage::now_millis();
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "meta": {"sentAt": now}}]
        });
        assert!(has_message_since(&val, Some(now - 10_000)));
    }

    #[test]
    fn has_message_since_checks_created_at_field() {
        let now = crate::storage::sqlite::SqliteStorage::now_millis();
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "created_at": now}]
        });
        assert!(has_message_since(&val, Some(now - 10_000)));
    }

    #[test]
    fn has_message_since_checks_created_at_camel_case() {
        let now = crate::storage::sqlite::SqliteStorage::now_millis();
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "createdAt": now}]
        });
        assert!(has_message_since(&val, Some(now - 10_000)));
    }

    #[test]
    fn has_message_since_checks_timestamp_field() {
        let now = crate::storage::sqlite::SqliteStorage::now_millis();
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "timestamp": now}]
        });
        assert!(has_message_since(&val, Some(now - 10_000)));
    }

    #[test]
    fn has_message_since_checks_ts_field() {
        let now = crate::storage::sqlite::SqliteStorage::now_millis();
        let val = json!({
            "messages": [{"role": "user", "content": "Test", "ts": now}]
        });
        assert!(has_message_since(&val, Some(now - 10_000)));
    }

    #[test]
    fn has_message_since_returns_false_for_empty_messages() {
        let val = json!({"messages": []});
        assert!(!has_message_since(&val, Some(1000)));
    }

    #[test]
    fn has_message_since_handles_nested_thread_messages() {
        let now = crate::storage::sqlite::SqliteStorage::now_millis();
        let val = json!({
            "thread": {
                "messages": [{"role": "user", "content": "Test", "meta": {"sentAt": now}}]
            }
        });
        assert!(has_message_since(&val, Some(now - 10_000)));
    }

    #[test]
    fn has_message_since_returns_false_without_timestamp_fields() {
        let val = json!({
            "messages": [{"role": "user", "content": "Test"}]
        });
        assert!(!has_message_since(&val, Some(1000)));
    }
}
