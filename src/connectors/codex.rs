use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

pub struct CodexConnector;
impl Default for CodexConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexConnector {
    pub fn new() -> Self {
        Self
    }

    fn home() -> PathBuf {
        dotenvy::var("CODEX_HOME").map_or_else(
            |_| dirs::home_dir().unwrap_or_default().join(".codex"),
            PathBuf::from,
        )
    }

    fn sessions_dir(home: &Path) -> PathBuf {
        let sessions = home.join("sessions");
        if sessions.exists() {
            sessions
        } else {
            home.to_path_buf()
        }
    }

    fn rollout_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let sessions = Self::sessions_dir(root);
        if !sessions.exists() {
            return out;
        }
        for entry in WalkDir::new(sessions).into_iter().flatten() {
            if entry.file_type().is_file() {
                let name = entry.file_name().to_str().unwrap_or("");
                // Match both modern .jsonl and legacy .json formats
                if name.starts_with("rollout-")
                    && (name.ends_with(".jsonl") || name.ends_with(".json"))
                {
                    out.push(entry.path().to_path_buf());
                }
            }
        }
        out
    }

    fn is_token_usage_target_message(message: &NormalizedMessage) -> bool {
        // Attribute token_count usage to concrete assistant turns only.
        // This avoids attaching usage to synthetic reasoning helper messages.
        message.role == "assistant" && message.author.is_none()
    }

    fn token_usage_from_payload(payload: &Value) -> Option<Value> {
        let input_tokens = payload.get("input_tokens").and_then(|v| v.as_i64());
        let output_tokens = payload
            .get("output_tokens")
            .and_then(|v| v.as_i64())
            .or_else(|| payload.get("tokens").and_then(|v| v.as_i64()));

        if input_tokens.is_none() && output_tokens.is_none() {
            return None;
        }

        let mut usage = serde_json::Map::new();
        if let Some(input) = input_tokens {
            usage.insert("input_tokens".to_string(), Value::from(input));
        }
        if let Some(output) = output_tokens {
            usage.insert("output_tokens".to_string(), Value::from(output));
        }
        usage.insert("data_source".to_string(), Value::String("api".to_string()));

        Some(Value::Object(usage))
    }

    fn attach_token_usage_to_latest_assistant(
        messages: &mut [NormalizedMessage],
        token_usage: Value,
        source_path: &Path,
        line_number: usize,
    ) {
        if let Some(target) = messages
            .iter_mut()
            .rev()
            .find(|m| Self::is_token_usage_target_message(m))
        {
            if !target.extra.is_object() {
                target.extra = Value::Object(serde_json::Map::new());
            }

            if let Some(extra) = target.extra.as_object_mut() {
                let cass = extra
                    .entry("cass".to_string())
                    .or_insert_with(|| Value::Object(serde_json::Map::new()));

                if !cass.is_object() {
                    *cass = Value::Object(serde_json::Map::new());
                }

                if let Some(cass_obj) = cass.as_object_mut() {
                    // Multiple token_count events for the same assistant turn:
                    // deterministic rule = last write wins.
                    cass_obj.insert("token_usage".to_string(), token_usage);
                }
            }
        } else {
            tracing::debug!(
                path = %source_path.display(),
                line_number,
                "codex token_count event had no preceding assistant message; skipping"
            );
        }
    }
}

impl Connector for CodexConnector {
    fn detect(&self) -> DetectionResult {
        let home = Self::home();
        // Check for actual sessions directory, not just home existing
        let sessions = home.join("sessions");
        if sessions.exists() && sessions.is_dir() {
            DetectionResult {
                detected: true,
                evidence: vec![format!("found {}", sessions.display())],
                root_paths: vec![sessions],
            }
        } else {
            DetectionResult::not_found()
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Use data_root only if it IS a Codex home directory (for testing).
        // Check for `.codex` in path OR explicit directory name ending in "codex".
        // AND ensure it has a "sessions" subdirectory.
        // This avoids false positives from unrelated directories that happen to have "codex" in the path.
        let is_codex_dir = ctx
            .data_dir
            .to_str()
            .map(|s| s.contains(".codex") || s.ends_with("/codex") || s.ends_with("\\codex"))
            .unwrap_or(false)
            && ctx.data_dir.join("sessions").exists();

        let roots: Vec<PathBuf> = if ctx.use_default_detection() {
            if is_codex_dir {
                vec![ctx.data_dir.clone()]
            } else {
                vec![Self::home()]
            }
        } else {
            // Explicit roots (remote mirrors, etc.)
            ctx.scan_roots.iter().map(|r| r.path.clone()).collect()
        };

        if roots.is_empty() {
            return Ok(Vec::new());
        }

        let mut convs = Vec::new();

        for mut home in roots {
            if home.is_file() {
                home = home.parent().unwrap_or(&home).to_path_buf();
            }
            if !home.exists() {
                continue;
            }

            let files = Self::rollout_files(&home);

            for file in files {
                let source_path = file.clone();
                // Skip files not modified since last scan (incremental indexing)
                if !crate::connectors::file_modified_since(&file, ctx.since_ts) {
                    continue;
                }
                // Use relative path from sessions dir as external_id for uniqueness
                // e.g., "2025/11/20/rollout-1" instead of just "rollout-1"
                let sessions_dir = Self::sessions_dir(&home);
                let external_id = source_path
                    .strip_prefix(&sessions_dir)
                    .ok()
                    .and_then(|rel| {
                        rel.with_extension("")
                            .to_str()
                            .map(std::string::ToString::to_string)
                    })
                    .or_else(|| {
                        source_path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .map(std::string::ToString::to_string)
                    });
                let ext = file.extension().and_then(|e| e.to_str());
                let mut messages = Vec::new();
                let mut started_at = None;
                let mut ended_at = None;
                let mut session_cwd: Option<PathBuf> = None;

                if ext == Some("jsonl") {
                    let f = std::fs::File::open(&file)
                        .with_context(|| format!("open rollout {}", file.display()))?;
                    let reader = std::io::BufReader::new(f);

                    // Modern envelope format: each line has {type, timestamp, payload}
                    for (line_idx, line_res) in std::io::BufRead::lines(reader).enumerate() {
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

                        let entry_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        let created = val
                            .get("timestamp")
                            .and_then(crate::connectors::parse_timestamp);

                        // NOTE: Do NOT filter individual messages by timestamp here!
                        // The file-level check in file_modified_since() is sufficient.
                        // Filtering messages would cause older messages to be lost when
                        // the file is re-indexed after new messages are added.

                        match entry_type {
                            "session_meta" => {
                                // Extract workspace from session metadata
                                if let Some(payload) = val.get("payload") {
                                    session_cwd = payload
                                        .get("cwd")
                                        .and_then(|v| v.as_str())
                                        .map(PathBuf::from);
                                }
                                started_at = started_at.or(created);
                            }
                            "response_item" => {
                                // Main message entries with nested payload
                                if let Some(payload) = val.get("payload") {
                                    let role = payload
                                        .get("role")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("agent");

                                    let content_str = payload
                                        .get("content")
                                        .map(crate::connectors::flatten_content)
                                        .unwrap_or_default();

                                    if content_str.trim().is_empty() {
                                        continue;
                                    }

                                    started_at = started_at.or(created);
                                    ended_at = created.or(ended_at);

                                    messages.push(NormalizedMessage {
                                        idx: 0, // will be re-assigned after filtering
                                        role: role.to_string(),
                                        author: None,
                                        created_at: created,
                                        content: content_str,
                                        extra: val,
                                        snippets: Vec::new(),
                                    });
                                }
                            }
                            "event_msg" => {
                                // Event messages - filter by payload type
                                if let Some(payload) = val.get("payload") {
                                    let event_type = payload.get("type").and_then(|v| v.as_str());

                                    match event_type {
                                        Some("user_message") => {
                                            let text = payload
                                                .get("message")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("");
                                            if !text.is_empty() {
                                                started_at = started_at.or(created);
                                                ended_at = created.or(ended_at);
                                                messages.push(NormalizedMessage {
                                                    idx: 0, // will be re-assigned after filtering
                                                    role: "user".to_string(),
                                                    author: None,
                                                    created_at: created,
                                                    content: text.to_string(),
                                                    extra: val,
                                                    snippets: Vec::new(),
                                                });
                                            }
                                        }
                                        Some("agent_reasoning") => {
                                            // Include reasoning - valuable for search
                                            let text = payload
                                                .get("text")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("");
                                            if !text.is_empty() {
                                                started_at = started_at.or(created);
                                                ended_at = created.or(ended_at);
                                                messages.push(NormalizedMessage {
                                                    idx: 0, // will be re-assigned after filtering
                                                    role: "assistant".to_string(),
                                                    author: Some("reasoning".to_string()),
                                                    created_at: created,
                                                    content: text.to_string(),
                                                    extra: val,
                                                    snippets: Vec::new(),
                                                });
                                            }
                                        }
                                        Some("token_count") => {
                                            if let Some(token_usage) =
                                                Self::token_usage_from_payload(payload)
                                            {
                                                Self::attach_token_usage_to_latest_assistant(
                                                    &mut messages,
                                                    token_usage,
                                                    &source_path,
                                                    line_idx + 1,
                                                );
                                            } else {
                                                tracing::debug!(
                                                    path = %source_path.display(),
                                                    line_number = line_idx + 1,
                                                    "codex token_count event missing token fields; skipping"
                                                );
                                            }
                                        }
                                        _ => {} // Skip turn_aborted and other unknown event types
                                    }
                                }
                            }
                            _ => {} // Skip turn_context and unknown types
                        }
                    }
                    // Re-assign sequential indices after filtering
                    super::reindex_messages(&mut messages);
                } else if ext == Some("json") {
                    let content = fs::read_to_string(&file)
                        .with_context(|| format!("read rollout {}", file.display()))?;
                    // Legacy format: single JSON object with {session, items}
                    let val: Value = match serde_json::from_str(&content) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    // Extract workspace from session.cwd
                    session_cwd = val
                        .get("session")
                        .and_then(|s| s.get("cwd"))
                        .and_then(|v| v.as_str())
                        .map(PathBuf::from);

                    // Parse items array
                    if let Some(items) = val.get("items").and_then(|v| v.as_array()) {
                        for item in items {
                            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("agent");

                            let content_str = item
                                .get("content")
                                .map(crate::connectors::flatten_content)
                                .unwrap_or_default();

                            if content_str.trim().is_empty() {
                                continue;
                            }

                            let created = item
                                .get("timestamp")
                                .and_then(crate::connectors::parse_timestamp);

                            // NOTE: Do NOT filter individual messages by timestamp.
                            // File-level check is sufficient for incremental indexing.

                            started_at = started_at.or(created);
                            ended_at = created.or(ended_at);

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
                    }
                    // Re-assign sequential indices after filtering
                    super::reindex_messages(&mut messages);
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

                convs.push(NormalizedConversation {
                    agent_slug: "codex".to_string(),
                    external_id,
                    title,
                    workspace: session_cwd, // Now populated from session_meta/session.cwd!
                    source_path: source_path.clone(),
                    started_at,
                    ended_at,
                    metadata: serde_json::json!({"source": if ext == Some("json") { "rollout_json" } else { "rollout" }}),
                    messages,
                });
            }
        }

        Ok(convs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    // =====================================================
    // Constructor Tests
    // =====================================================

    #[test]
    fn new_creates_connector() {
        let connector = CodexConnector::new();
        // Just verify it doesn't panic - struct has no fields
        let _ = connector;
    }

    #[test]
    fn default_creates_connector() {
        let connector = CodexConnector;
        let _ = connector;
    }

    // =====================================================
    // home() Tests
    // =====================================================

    #[test]
    #[serial]
    fn home_returns_path_ending_with_codex() {
        // Note: We can't reliably test CODEX_HOME env var due to parallel test execution.
        // Testing that home() returns a valid path structure is sufficient.
        // The function uses CODEX_HOME if set, otherwise defaults to ~/.codex
        let home = CodexConnector::home();
        // Either the env var is set (ends with some path) or default (ends with .codex)
        let path_str = home.to_str().unwrap();
        assert!(
            path_str.ends_with(".codex") || path_str.contains("codex"),
            "home() should return a path related to codex, got: {}",
            path_str
        );
    }

    // =====================================================
    // rollout_files() Tests
    // =====================================================

    #[test]
    fn rollout_files_finds_jsonl_files() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let rollout = sessions.join("rollout-abc123.jsonl");
        fs::write(&rollout, "{}").unwrap();

        let files = CodexConnector::rollout_files(dir.path());
        assert_eq!(files.len(), 1);
        assert!(files[0].to_str().unwrap().contains("rollout-abc123.jsonl"));
    }

    #[test]
    fn rollout_files_finds_json_files() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let rollout = sessions.join("rollout-legacy.json");
        fs::write(&rollout, "{}").unwrap();

        let files = CodexConnector::rollout_files(dir.path());
        assert_eq!(files.len(), 1);
        assert!(files[0].to_str().unwrap().contains("rollout-legacy.json"));
    }

    #[test]
    fn rollout_files_ignores_non_rollout_files() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // Create various non-rollout files
        fs::write(sessions.join("config.json"), "{}").unwrap();
        fs::write(sessions.join("session.jsonl"), "{}").unwrap();
        fs::write(sessions.join("other.txt"), "test").unwrap();

        let files = CodexConnector::rollout_files(dir.path());
        assert_eq!(files.len(), 0);
    }

    #[test]
    fn rollout_files_finds_nested_rollouts() {
        let dir = TempDir::new().unwrap();
        let nested = dir
            .path()
            .join("sessions")
            .join("2025")
            .join("12")
            .join("17");
        fs::create_dir_all(&nested).unwrap();

        let rollout = nested.join("rollout-nested.jsonl");
        fs::write(&rollout, "{}").unwrap();

        let files = CodexConnector::rollout_files(dir.path());
        assert_eq!(files.len(), 1);
        assert!(files[0].to_str().unwrap().contains("rollout-nested.jsonl"));
    }

    #[test]
    fn rollout_files_returns_empty_when_no_sessions_dir() {
        let dir = TempDir::new().unwrap();
        let files = CodexConnector::rollout_files(dir.path());
        assert_eq!(files.len(), 0);
    }

    // =====================================================
    // detect() Tests
    // =====================================================

    #[test]
    #[serial]
    fn detect_returns_true_when_sessions_exists() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // SAFETY: Test runs in single-threaded context
        unsafe { std::env::set_var("CODEX_HOME", dir.path()) };
        let connector = CodexConnector::new();
        let result = connector.detect();
        // SAFETY: Test runs in single-threaded context
        unsafe { std::env::remove_var("CODEX_HOME") };

        assert!(result.detected);
        assert!(!result.evidence.is_empty());
    }

    #[test]
    #[serial]
    fn detect_returns_false_when_no_sessions() {
        let dir = TempDir::new().unwrap();
        // Don't create sessions directory

        // SAFETY: Test runs in single-threaded context
        unsafe { std::env::set_var("CODEX_HOME", dir.path()) };
        let connector = CodexConnector::new();
        let result = connector.detect();
        // SAFETY: Test runs in single-threaded context
        unsafe { std::env::remove_var("CODEX_HOME") };

        assert!(!result.detected);
    }

    // =====================================================
    // scan() JSONL Format Tests
    // =====================================================

    #[test]
    fn scan_parses_jsonl_response_item_messages() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Hello Codex"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"assistant","content":"Hello! How can I help?"}}
"#;
        fs::write(sessions.join("rollout-test.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[0].content, "Hello Codex");
        assert_eq!(convs[0].messages[1].role, "assistant");
    }

    #[test]
    fn scan_parses_event_msg_user_message() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"event_msg","timestamp":"2025-12-01T10:00:00Z","payload":{"type":"user_message","message":"User typed this"}}
"#;
        fs::write(sessions.join("rollout-user.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[0].content, "User typed this");
        assert!(convs[0].started_at.is_some());
        assert!(convs[0].ended_at.is_some());
    }

    #[test]
    fn scan_parses_event_msg_agent_reasoning() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"event_msg","timestamp":"2025-12-01T10:00:00Z","payload":{"type":"agent_reasoning","text":"Let me think about this..."}}
"#;
        fs::write(sessions.join("rollout-reasoning.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].role, "assistant");
        assert_eq!(convs[0].messages[0].author, Some("reasoning".to_string()));
        assert_eq!(convs[0].messages[0].content, "Let me think about this...");
        assert!(convs[0].started_at.is_some());
        assert!(convs[0].ended_at.is_some());
    }

    #[test]
    fn scan_extracts_workspace_from_session_meta() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"session_meta","timestamp":"2025-12-01T10:00:00Z","payload":{"cwd":"/home/user/project"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"user","content":"Test"}}
"#;
        fs::write(sessions.join("rollout-meta.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0].workspace,
            Some(PathBuf::from("/home/user/project"))
        );
    }

    #[test]
    fn scan_skips_empty_lines_in_jsonl() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Message 1"}}

{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"user","content":"Message 2"}}
"#;
        fs::write(sessions.join("rollout-empty-lines.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 2);
    }

    #[test]
    fn scan_skips_invalid_json_lines() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Valid"}}
not valid json at all
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"user","content":"Also valid"}}
"#;
        fs::write(sessions.join("rollout-invalid.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 2);
    }

    #[test]
    fn scan_skips_empty_content_messages() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Has content"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"assistant","content":""}}
{"type":"response_item","timestamp":"2025-12-01T10:00:02Z","payload":{"role":"assistant","content":"   "}}
"#;
        fs::write(sessions.join("rollout-empty-content.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        // Only the message with actual content should be included
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].content, "Has content");
    }

    #[test]
    fn scan_skips_unknown_event_types() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Real message"}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:01Z","payload":{"type":"token_count","tokens":100}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:02Z","payload":{"type":"turn_aborted"}}
{"type":"turn_context","timestamp":"2025-12-01T10:00:03Z","payload":{}}
"#;
        fs::write(sessions.join("rollout-unknown.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        // Only the response_item should be included
        assert_eq!(convs[0].messages.len(), 1);
    }

    #[test]
    fn scan_assigns_sequential_indices() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"First"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"assistant","content":"Second"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:02Z","payload":{"role":"user","content":"Third"}}
"#;
        fs::write(sessions.join("rollout-idx.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].messages[0].idx, 0);
        assert_eq!(convs[0].messages[1].idx, 1);
        assert_eq!(convs[0].messages[2].idx, 2);
    }

    #[test]
    fn scan_attaches_token_count_to_nearest_preceding_assistant() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Question"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"assistant","content":"First answer"}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:02Z","payload":{"type":"token_count","input_tokens":10,"output_tokens":20}}
{"type":"response_item","timestamp":"2025-12-01T10:00:03Z","payload":{"role":"assistant","content":"Second answer"}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:04Z","payload":{"type":"token_count","input_tokens":30,"output_tokens":40}}
"#;
        fs::write(sessions.join("rollout-attach-nearest.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0].messages.len(),
            3,
            "no synthetic token_count messages"
        );

        let first = &convs[0].messages[1];
        assert_eq!(first.content, "First answer");
        assert_eq!(
            first
                .extra
                .pointer("/cass/token_usage/input_tokens")
                .and_then(|v| v.as_i64()),
            Some(10)
        );
        assert_eq!(
            first
                .extra
                .pointer("/cass/token_usage/output_tokens")
                .and_then(|v| v.as_i64()),
            Some(20)
        );

        let second = &convs[0].messages[2];
        assert_eq!(second.content, "Second answer");
        assert_eq!(
            second
                .extra
                .pointer("/cass/token_usage/input_tokens")
                .and_then(|v| v.as_i64()),
            Some(30)
        );
        assert_eq!(
            second
                .extra
                .pointer("/cass/token_usage/output_tokens")
                .and_then(|v| v.as_i64()),
            Some(40)
        );
        assert_eq!(
            second
                .extra
                .pointer("/cass/token_usage/data_source")
                .and_then(|v| v.as_str()),
            Some("api")
        );
    }

    #[test]
    fn scan_ignores_token_count_without_preceding_assistant() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Question"}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:01Z","payload":{"type":"token_count","input_tokens":11,"output_tokens":22}}
{"type":"response_item","timestamp":"2025-12-01T10:00:02Z","payload":{"role":"assistant","content":"Answer later"}}
"#;
        fs::write(sessions.join("rollout-unmatched-token.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 2);
        assert!(
            convs[0].messages[1]
                .extra
                .pointer("/cass/token_usage")
                .is_none(),
            "token_count before first assistant must not attach to future message"
        );
    }

    #[test]
    fn scan_multiple_token_count_for_one_assistant_prefers_last() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Question"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"assistant","content":"Answer"}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:02Z","payload":{"type":"token_count","input_tokens":5,"output_tokens":10}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:03Z","payload":{"type":"token_count","input_tokens":7,"output_tokens":14}}
"#;
        fs::write(sessions.join("rollout-token-last-wins.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 2);
        let assistant = &convs[0].messages[1];
        assert_eq!(
            assistant
                .extra
                .pointer("/cass/token_usage/input_tokens")
                .and_then(|v| v.as_i64()),
            Some(7)
        );
        assert_eq!(
            assistant
                .extra
                .pointer("/cass/token_usage/output_tokens")
                .and_then(|v| v.as_i64()),
            Some(14)
        );
    }

    // =====================================================
    // scan() Legacy JSON Format Tests
    // =====================================================

    #[test]
    fn scan_parses_legacy_json_format() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = json!({
            "session": {"cwd": "/home/user/legacy"},
            "items": [
                {"role": "user", "content": "Legacy user message", "timestamp": "2025-12-01T10:00:00Z"},
                {"role": "assistant", "content": "Legacy assistant response", "timestamp": "2025-12-01T10:00:01Z"}
            ]
        });
        fs::write(sessions.join("rollout-legacy.json"), content.to_string()).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].workspace, Some(PathBuf::from("/home/user/legacy")));
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[0].content, "Legacy user message");
        assert_eq!(convs[0].messages[1].role, "assistant");
    }

    #[test]
    fn scan_legacy_json_skips_empty_content() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = json!({
            "session": {},
            "items": [
                {"role": "user", "content": "Has content"},
                {"role": "assistant", "content": ""},
                {"role": "assistant", "content": "   "}
            ]
        });
        fs::write(
            sessions.join("rollout-empty-legacy.json"),
            content.to_string(),
        )
        .unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 1);
    }

    #[test]
    fn scan_legacy_json_handles_missing_items() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = json!({"session": {}});
        fs::write(sessions.join("rollout-no-items.json"), content.to_string()).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // No messages = conversation is skipped
        assert_eq!(convs.len(), 0);
    }

    #[test]
    fn scan_skips_invalid_legacy_json() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        fs::write(sessions.join("rollout-bad.json"), "not valid json").unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 0);
    }

    // =====================================================
    // Title Extraction Tests
    // =====================================================

    #[test]
    fn scan_extracts_title_from_first_user_message() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","payload":{"role":"assistant","content":"I'm an assistant"}}
{"type":"response_item","payload":{"role":"user","content":"This should be the title"}}
"#;
        fs::write(sessions.join("rollout-title.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].title, Some("This should be the title".to_string()));
    }

    #[test]
    fn scan_truncates_long_titles() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let long_title = "x".repeat(200);
        let content = format!(
            r#"{{"type":"response_item","payload":{{"role":"user","content":"{}"}}}}"#,
            long_title
        );
        fs::write(sessions.join("rollout-long.jsonl"), content + "\n").unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].title.as_ref().unwrap().len(), 100);
    }

    #[test]
    fn scan_uses_first_line_for_multiline_title() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","payload":{"role":"user","content":"First line\nSecond line\nThird line"}}
"#;
        fs::write(sessions.join("rollout-multiline.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].title, Some("First line".to_string()));
    }

    #[test]
    fn scan_falls_back_to_first_message_for_title() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // No user messages, only assistant
        let content = r#"{"type":"response_item","payload":{"role":"assistant","content":"Assistant speaks first"}}
"#;
        fs::write(sessions.join("rollout-assistant-only.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].title, Some("Assistant speaks first".to_string()));
    }

    // =====================================================
    // External ID Tests
    // =====================================================

    #[test]
    fn scan_uses_relative_path_as_external_id() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir
            .join("sessions")
            .join("2025")
            .join("12")
            .join("17");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","payload":{"role":"user","content":"Test"}}
"#;
        fs::write(sessions.join("rollout-nested-id.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // External ID should be the relative path from sessions dir
        assert!(convs[0].external_id.is_some());
        let ext_id = convs[0].external_id.as_ref().unwrap();
        assert!(ext_id.contains("2025") || ext_id.contains("rollout-nested-id"));
    }

    // =====================================================
    // Metadata Tests
    // =====================================================

    #[test]
    fn scan_sets_metadata_source_for_jsonl() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","payload":{"role":"user","content":"Test"}}
"#;
        fs::write(sessions.join("rollout-meta-jsonl.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].metadata["source"], "rollout");
    }

    #[test]
    fn scan_sets_metadata_source_for_json() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = json!({
            "session": {},
            "items": [{"role": "user", "content": "Test"}]
        });
        fs::write(sessions.join("rollout-meta-json.json"), content.to_string()).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].metadata["source"], "rollout_json");
    }

    // =====================================================
    // Agent Slug Tests
    // =====================================================

    #[test]
    fn scan_sets_agent_slug_to_codex() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","payload":{"role":"user","content":"Test"}}
"#;
        fs::write(sessions.join("rollout-slug.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].agent_slug, "codex");
    }

    // =====================================================
    // Timestamp Tests
    // =====================================================

    #[test]
    fn scan_parses_timestamps() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"First"}}
{"type":"response_item","timestamp":"2025-12-01T11:00:00Z","payload":{"role":"user","content":"Last"}}
"#;
        fs::write(sessions.join("rollout-ts.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert!(convs[0].started_at.is_some());
        assert!(convs[0].ended_at.is_some());
        assert!(convs[0].messages[0].created_at.is_some());
    }

    // =====================================================
    // Edge Cases
    // =====================================================

    #[test]
    fn scan_handles_empty_sessions_dir() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        // No files in sessions directory

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 0);
    }

    #[test]
    fn scan_handles_multiple_rollout_files() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content1 = r#"{"type":"response_item","payload":{"role":"user","content":"Session 1"}}
"#;
        let content2 = r#"{"type":"response_item","payload":{"role":"user","content":"Session 2"}}
"#;
        fs::write(sessions.join("rollout-1.jsonl"), content1).unwrap();
        fs::write(sessions.join("rollout-2.jsonl"), content2).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 2);
    }

    #[test]
    fn scan_skips_conversations_with_no_messages() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // Only metadata, no actual messages
        let content = r#"{"type":"session_meta","payload":{"cwd":"/test"}}
{"type":"turn_context","payload":{}}
"#;
        fs::write(sessions.join("rollout-no-msgs.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        // Should be skipped because no actual messages
        assert_eq!(convs.len(), 0);
    }

    #[test]
    fn scan_handles_array_content_in_response_item() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // Content as array of text blocks (like Claude API format)
        let content = json!({
            "type": "response_item",
            "payload": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Part one."},
                    {"type": "text", "text": " Part two."}
                ]
            }
        });
        fs::write(
            sessions.join("rollout-array.jsonl"),
            content.to_string() + "\n",
        )
        .unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        // flatten_content should combine the parts
        assert!(convs[0].messages[0].content.contains("Part one"));
    }

    #[test]
    fn scan_uses_default_role_when_missing() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // No role specified in payload
        let content = r#"{"type":"response_item","payload":{"content":"No role specified"}}
"#;
        fs::write(sessions.join("rollout-no-role.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        // Default role should be "agent"
        assert_eq!(convs[0].messages[0].role, "agent");
    }

    #[test]
    fn scan_stores_source_path() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = r#"{"type":"response_item","payload":{"role":"user","content":"Test"}}
"#;
        let file_path = sessions.join("rollout-path.jsonl");
        fs::write(&file_path, content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].source_path, file_path);
    }

    #[test]
    #[serial]
    fn scan_respects_codex_home_even_if_data_dir_contains_codex_string() {
        // Setup real codex home with data
        let real_home = TempDir::new().unwrap();
        let real_sessions = real_home.path().join("sessions");
        fs::create_dir_all(&real_sessions).unwrap();
        let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Real session"}}"#;
        fs::write(real_sessions.join("rollout-real.jsonl"), content).unwrap();

        // Setup CASS data dir that happens to have ".codex" in path which triggers the heuristic
        // e.g., /tmp/.../fake.codex/data
        let project_dir = TempDir::new().unwrap();
        let confusing_data_dir = project_dir.path().join("fake.codex").join("data");
        fs::create_dir_all(&confusing_data_dir).unwrap();

        // SAFETY: Test runs in single-threaded context
        unsafe { std::env::set_var("CODEX_HOME", real_home.path()) };

        let connector = CodexConnector::new();
        // this defaults to use_default_detection = true
        let ctx = ScanContext::local_default(confusing_data_dir.clone(), None);

        let convs = connector.scan(&ctx).unwrap();

        // Cleanup
        unsafe { std::env::remove_var("CODEX_HOME") };

        // Should find the real session
        assert_eq!(
            convs.len(),
            1,
            "Should find session in CODEX_HOME, but found {}",
            convs.len()
        );
        assert_eq!(convs[0].messages[0].content, "Real session");
    }

    // =====================================================
    // Edge case tests — malformed input robustness (br-fiiv)
    // =====================================================

    #[test]
    fn truncated_jsonl_mid_json_returns_partial_results() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // First line valid, second truncated mid-JSON
        let content = b"{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"Valid\"}}\n{\"type\":\"response_item\",\"payload\":{\"role\":\"assistant\",\"con";
        fs::write(sessions.join("rollout-truncated.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok(), "truncated file should not cause an error");
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0].messages.len(),
            1,
            "should yield only the 1 valid message from truncated file"
        );
        assert_eq!(convs[0].messages[0].content, "Valid");
    }

    #[test]
    fn truncated_mid_utf8_does_not_panic() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(
            b"{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"OK\"}}\n",
        );
        // Incomplete 4-byte UTF-8 sequence (U+1F600 = F0 9F 98 80, only 2 bytes)
        bytes.extend_from_slice(b"\xF0\x9F");

        fs::write(sessions.join("rollout-utf8trunc.jsonl"), &bytes).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok(), "truncated mid-UTF8 should not panic");
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages[0].content, "OK");
    }

    #[test]
    fn invalid_utf8_skips_corrupted_lines() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(
            b"{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"Before\"}}\n",
        );
        bytes.extend_from_slice(b"\xFF\xFE invalid utf8 line\n");
        bytes.extend_from_slice(
            b"{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"After\"}}\n",
        );

        fs::write(sessions.join("rollout-badbytes.jsonl"), &bytes).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok(), "invalid UTF-8 should not cause a panic");
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0].messages.len(),
            2,
            "should extract valid messages around invalid UTF-8"
        );
        assert_eq!(convs[0].messages[0].content, "Before");
        assert_eq!(convs[0].messages[1].content, "After");
    }

    #[test]
    fn empty_file_returns_no_conversations() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        fs::write(sessions.join("rollout-empty.jsonl"), b"").unwrap();
        fs::write(sessions.join("rollout-empty.json"), b"").unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok(), "empty files should not cause errors");
        let convs = result.unwrap();
        assert!(
            convs.is_empty(),
            "empty files should produce no conversations"
        );
    }

    #[test]
    fn whitespace_only_file_returns_no_conversations() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        fs::write(sessions.join("rollout-ws.jsonl"), "  \n\n  \t\n").unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(
            result.is_ok(),
            "whitespace-only file should not cause errors"
        );
        let convs = result.unwrap();
        assert!(
            convs.is_empty(),
            "whitespace-only file should produce no conversations"
        );
    }

    #[test]
    fn json_type_mismatch_skips_gracefully() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = concat!(
            // payload is a string instead of object
            "{\"type\":\"response_item\",\"payload\":\"not an object\"}\n",
            // type is a number
            "{\"type\":123,\"payload\":{\"role\":\"user\",\"content\":\"num type\"}}\n",
            // content is a number
            "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":99}}\n",
            // Correct entry
            "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"Correct\"}}\n",
        );
        fs::write(sessions.join("rollout-types.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok(), "type mismatches should not cause errors");
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert!(
            convs[0].messages.iter().any(|m| m.content == "Correct"),
            "should extract the correctly typed entry"
        );
    }

    #[test]
    fn deeply_nested_json_does_not_stack_overflow() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // serde_json has a recursion limit of 128; 200 levels will trigger parse error
        let mut nested = String::new();
        for _ in 0..200 {
            nested.push_str("{\"a\":");
        }
        nested.push('1');
        for _ in 0..200 {
            nested.push('}');
        }

        let content = format!(
            "{}\n{}\n",
            nested,
            r#"{"type":"response_item","payload":{"role":"user","content":"After nesting"}}"#
        );
        fs::write(sessions.join("rollout-deep.jsonl"), &content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(
            result.is_ok(),
            "deeply nested JSON should not cause stack overflow"
        );
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages[0].content, "After nesting");
    }

    #[test]
    fn large_message_body_handled_without_oom() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let large_content = "x".repeat(1_000_000);
        let line = format!(
            r#"{{"type":"response_item","payload":{{"role":"user","content":"{}"}}}}"#,
            large_content
        );
        fs::write(sessions.join("rollout-large.jsonl"), &line).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok(), "large message body should not cause OOM");
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages[0].content.len(), 1_000_000);
    }

    #[test]
    fn null_bytes_embedded_in_content_handled() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = concat!(
            r#"{"type":"response_item","payload":{"role":"user","content":"before\u0000after"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"role":"user","content":"Clean"}}"#,
            "\n"
        );
        fs::write(sessions.join("rollout-null.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(
            result.is_ok(),
            "null bytes in content should not cause errors"
        );
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert!(!convs[0].messages.is_empty());
    }

    #[test]
    fn bom_marker_at_file_start_handled() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\xEF\xBB\xBF"); // UTF-8 BOM
        bytes.extend_from_slice(
            b"{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"BOM line\"}}\n",
        );
        bytes.extend_from_slice(
            b"{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"Second\"}}\n",
        );
        fs::write(sessions.join("rollout-bom.jsonl"), &bytes).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok(), "BOM marker should not cause errors");
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert!(
            !convs[0].messages.is_empty(),
            "should extract at least the second line after BOM"
        );
        assert!(
            convs[0].messages.iter().any(|m| m.content == "Second"),
            "second line should parse correctly regardless of BOM"
        );
    }

    // =====================================================
    // Codex-specific edge cases (br-fiiv)
    // =====================================================

    #[test]
    fn missing_payload_field_skipped() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // response_item and event_msg without payload field
        let content = concat!(
            "{\"type\":\"response_item\",\"timestamp\":\"2025-12-01T10:00:00Z\"}\n",
            "{\"type\":\"event_msg\",\"timestamp\":\"2025-12-01T10:00:01Z\"}\n",
            "{\"type\":\"session_meta\",\"timestamp\":\"2025-12-01T10:00:02Z\"}\n",
            "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"Has payload\"}}\n",
        );
        fs::write(sessions.join("rollout-nopayload.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok(), "missing payload should not cause errors");
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].content, "Has payload");
    }

    #[test]
    fn timestamp_parsing_edge_cases() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let content = concat!(
            // ISO 8601 with milliseconds
            "{\"type\":\"response_item\",\"timestamp\":\"2025-12-01T10:00:00.123Z\",\"payload\":{\"role\":\"user\",\"content\":\"ms precision\"}}\n",
            // ISO 8601 with timezone offset
            "{\"type\":\"response_item\",\"timestamp\":\"2025-12-01T10:00:00+05:30\",\"payload\":{\"role\":\"user\",\"content\":\"tz offset\"}}\n",
            // Unix epoch milliseconds as number
            "{\"type\":\"response_item\",\"timestamp\":1700000000000,\"payload\":{\"role\":\"user\",\"content\":\"epoch millis\"}}\n",
            // No timestamp at all
            "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"no timestamp\"}}\n",
            // Null timestamp
            "{\"type\":\"response_item\",\"timestamp\":null,\"payload\":{\"role\":\"user\",\"content\":\"null ts\"}}\n",
        );
        fs::write(sessions.join("rollout-timestamps.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(
            result.is_ok(),
            "varied timestamp formats should not cause errors"
        );
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0].messages.len(),
            5,
            "all 5 messages should be extracted regardless of timestamp format"
        );
        // Messages with valid timestamps should have created_at set
        assert!(convs[0].messages[0].created_at.is_some());
        assert!(convs[0].messages[1].created_at.is_some());
        assert!(convs[0].messages[2].created_at.is_some());
    }

    #[test]
    fn workspace_path_encoding_edge_cases() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // Test various workspace path formats in session_meta
        let content = concat!(
            // Path with spaces
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/home/user/my project/src\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"Spaces path\"}}\n",
        );
        fs::write(sessions.join("rollout-spaces.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0].workspace,
            Some(PathBuf::from("/home/user/my project/src"))
        );

        // Unicode workspace path
        let content2 = concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/home/\u{00FC}ser/projekt\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"Unicode path\"}}\n",
        );
        fs::write(sessions.join("rollout-unicode.jsonl"), content2).unwrap();

        let convs2 = connector.scan(&ctx).unwrap();
        assert!(
            !convs2.is_empty(),
            "unicode workspace paths should be handled"
        );
    }

    #[test]
    fn event_msg_with_unknown_subtypes_skipped() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // Various event_msg subtypes that should be gracefully skipped
        let content = concat!(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"streaming_start\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"streaming_delta\",\"delta\":\"partial\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"streaming_end\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"tool_call\",\"name\":\"bash\",\"input\":{\"cmd\":\"ls\"}}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"tool_result\",\"output\":\"file.txt\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Real user input\"}}\n",
        );
        fs::write(sessions.join("rollout-events.jsonl"), content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(
            result.is_ok(),
            "unknown event subtypes should not cause errors"
        );
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        // Only user_message event should produce a message
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].content, "Real user input");
        assert_eq!(convs[0].messages[0].role, "user");
    }

    #[test]
    fn tool_call_format_variations() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        let sessions = codex_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // response_item with tool_use content blocks (like Claude API format)
        let content = json!({
            "type": "response_item",
            "payload": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Let me check that."},
                    {"type": "tool_use", "name": "read_file", "input": {"path": "/etc/hosts"}}
                ]
            }
        })
        .to_string()
            + "\n"
            + &json!({
                "type": "response_item",
                "payload": {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "name": "bash", "input": {"command": "ls -la"}},
                        {"type": "text", "text": "Here are the results."}
                    ]
                }
            })
            .to_string()
            + "\n";

        fs::write(sessions.join("rollout-tools.jsonl"), &content).unwrap();

        let connector = CodexConnector::new();
        let ctx = ScanContext::local_default(codex_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(
            result.is_ok(),
            "tool call format variations should not cause errors"
        );
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 2);
        // flatten_content should handle tool_use blocks
        assert!(convs[0].messages[0].content.contains("Let me check"));
        assert!(
            convs[0].messages[1]
                .content
                .contains("Here are the results")
        );
    }
}
