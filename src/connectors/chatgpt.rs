//! Connector for ChatGPT desktop app conversation history.
//!
//! ChatGPT stores conversations in:
//! - macOS: ~/Library/Application Support/com.openai.chat/
//!
//! ## Conversation storage versions:
//! - v1 (legacy): Plain JSON files in `conversations-{uuid}/` (unencrypted)
//! - v2/v3: Encrypted files in `conversations-v2-{uuid}/` or `conversations-v3-{uuid}/`
//!
//! ## Encryption Details (v2/v3):
//! ChatGPT desktop encrypts conversations using AES-256-GCM with a key stored in the
//! macOS Keychain under access group `2DC432GLL2.com.openai.shared`.
//!
//! **Important**: The encryption key is protected by Apple's Keychain Access Groups
//! mechanism, which requires the accessing app to be signed with OpenAI's Team ID
//! (2DC432GLL2). This means third-party apps cannot directly access the key.
//!
//! ## Decryption Options:
//! To decrypt v2/v3 conversations, you can:
//! 1. Set the `CHATGPT_ENCRYPTION_KEY` environment variable to the base64-encoded key
//! 2. Create a key file at `~/.config/cass/chatgpt_key.bin` containing the raw 32-byte key
//!
//! The key can potentially be extracted by:
//! - Using Keychain Access.app to export the key (requires user authorization)
//! - Running a helper tool signed with appropriate entitlements
//!
//! ## File Format:
//! Encrypted files appear to use AES-256-GCM with:
//! - 12-byte nonce at the start
//! - Encrypted JSON data
//! - 16-byte authentication tag at the end

use std::fs;
use std::path::PathBuf;

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use anyhow::{Context, Result};
use base64::prelude::*;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

/// Nonce size for AES-GCM (12 bytes)
const NONCE_SIZE: usize = 12;
/// Authentication tag size for AES-GCM (16 bytes)
const TAG_SIZE: usize = 16;
/// AES-256 key size (32 bytes)
const KEY_SIZE: usize = 32;

pub struct ChatGptConnector {
    /// Optional encryption key for v2/v3 conversations
    encryption_key: Option<[u8; KEY_SIZE]>,
}

impl Default for ChatGptConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatGptConnector {
    pub fn new() -> Self {
        let encryption_key = Self::load_encryption_key();
        if encryption_key.is_some() {
            tracing::info!(
                "chatgpt encryption key loaded, encrypted conversations will be decrypted"
            );
        }
        Self { encryption_key }
    }

    /// Load encryption key from environment variable or key file
    fn load_encryption_key() -> Option<[u8; KEY_SIZE]> {
        // Try environment variable first (base64-encoded)
        if let Ok(key_b64) = dotenvy::var("CHATGPT_ENCRYPTION_KEY") {
            if let Ok(key_bytes) = BASE64_STANDARD.decode(key_b64.trim()) {
                if key_bytes.len() == KEY_SIZE {
                    let mut key = [0u8; KEY_SIZE];
                    key.copy_from_slice(&key_bytes);
                    tracing::debug!(
                        "chatgpt encryption key loaded from CHATGPT_ENCRYPTION_KEY env var"
                    );
                    return Some(key);
                } else {
                    tracing::warn!(
                        "CHATGPT_ENCRYPTION_KEY has wrong length: {} (expected {})",
                        key_bytes.len(),
                        KEY_SIZE
                    );
                }
            } else {
                tracing::warn!("CHATGPT_ENCRYPTION_KEY is not valid base64");
            }
        }

        // Try key file
        let key_file_paths = [
            dirs::config_dir().map(|p| p.join("cass/chatgpt_key.bin")),
            dirs::home_dir().map(|p| p.join(".config/cass/chatgpt_key.bin")),
            dirs::home_dir().map(|p| p.join(".cass/chatgpt_key.bin")),
        ];

        for path_opt in key_file_paths.iter().flatten() {
            if path_opt.exists() {
                match fs::read(path_opt) {
                    Ok(key_bytes) if key_bytes.len() == KEY_SIZE => {
                        let mut key = [0u8; KEY_SIZE];
                        key.copy_from_slice(&key_bytes);
                        tracing::debug!(path = %path_opt.display(), "chatgpt encryption key loaded from file");
                        return Some(key);
                    }
                    Ok(key_bytes) => {
                        tracing::warn!(
                            path = %path_opt.display(),
                            "chatgpt key file has wrong size: {} (expected {})",
                            key_bytes.len(),
                            KEY_SIZE
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path_opt.display(),
                            error = %e,
                            "failed to read chatgpt key file"
                        );
                    }
                }
            }
        }

        None
    }

    /// Get the ChatGPT app support directory
    pub fn app_support_dir() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            dirs::home_dir().map(|h| h.join("Library/Application Support/com.openai.chat"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            // ChatGPT desktop is currently macOS only
            None
        }
    }

    /// Find conversation directories (both encrypted and unencrypted)
    fn find_conversation_dirs(base: &PathBuf) -> Vec<(PathBuf, bool)> {
        let mut dirs = Vec::new();

        if !base.exists() {
            return dirs;
        }

        for entry in fs::read_dir(base).into_iter().flatten().flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            // Check for conversation directories
            if name.starts_with("conversations-") {
                // v1 (unencrypted) or v2/v3 (encrypted)
                let is_encrypted = name.contains("-v2-") || name.contains("-v3-");
                dirs.push((path, is_encrypted));
            }
        }

        dirs
    }

    /// Decrypt an encrypted conversation file
    fn decrypt_file(&self, data: &[u8]) -> Result<Vec<u8>> {
        let key = self.encryption_key.ok_or_else(|| {
            anyhow::anyhow!(
                "No encryption key available. Set CHATGPT_ENCRYPTION_KEY env var or create key file."
            )
        })?;

        if data.len() < NONCE_SIZE + TAG_SIZE {
            anyhow::bail!("Encrypted data too short: {} bytes", data.len());
        }

        // Extract nonce from the beginning
        let nonce = Nonce::from_slice(&data[..NONCE_SIZE]);

        // The rest is ciphertext + tag
        let ciphertext = &data[NONCE_SIZE..];

        // Create cipher and decrypt
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|e| anyhow::anyhow!("Failed to create cipher: {}", e))?;

        let plaintext = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow::anyhow!("Decryption failed: {}", e))?;

        Ok(plaintext)
    }

    /// Parse a conversation file (JSON or encrypted data format)
    fn parse_conversation_file(
        &self,
        path: &PathBuf,
        _since_ts: Option<i64>,
        is_encrypted: bool,
    ) -> Result<Option<NormalizedConversation>> {
        let content_bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;

        // Decrypt if necessary
        let content = if is_encrypted {
            let decrypted = self.decrypt_file(&content_bytes)?;
            String::from_utf8(decrypted).with_context(|| {
                format!(
                    "decrypted content is not valid UTF-8 from {}",
                    path.display()
                )
            })?
        } else {
            String::from_utf8(content_bytes)
                .with_context(|| format!("content is not valid UTF-8 from {}", path.display()))?
        };

        let val: Value = serde_json::from_str(&content)
            .with_context(|| format!("parse JSON from {}", path.display()))?;

        let mut messages = Vec::new();
        let mut started_at = None;
        let mut ended_at = None;

        // Extract conversation ID
        let conv_id = val
            .get("id")
            .or_else(|| val.get("conversation_id"))
            .and_then(|v| v.as_str())
            .or_else(|| path.file_stem().and_then(|s| s.to_str()))
            .map(String::from);

        // Extract title
        let title = val.get("title").and_then(|v| v.as_str()).map(String::from);

        // Parse messages from mapping structure (ChatGPT format)
        if let Some(mapping) = val.get("mapping").and_then(|v| v.as_object()) {
            // Collect messages with their parent info for ordering
            let mut msg_nodes: Vec<(Option<String>, String, &Value)> = Vec::new();

            for (node_id, node) in mapping {
                if let Some(msg) = node.get("message") {
                    let parent = node
                        .get("parent")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    msg_nodes.push((parent, node_id.clone(), msg));
                }
            }

            // Simple ordering: sort by create_time if available
            msg_nodes.sort_by(|a, b| {
                let ts_a = a.2.get("create_time").and_then(|v| v.as_f64());
                let ts_b = b.2.get("create_time").and_then(|v| v.as_f64());
                match (ts_a, ts_b) {
                    (Some(a_ts), Some(b_ts)) => a_ts
                        .partial_cmp(&b_ts)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.1.cmp(&b.1)),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => a.1.cmp(&b.1),
                }
            });

            for (_, _, msg) in msg_nodes {
                // Get role
                let role = msg
                    .get("author")
                    .and_then(|a| a.get("role"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("assistant");

                // Skip system messages
                if role == "system" {
                    continue;
                }

                // Get content
                let content_val = msg.get("content");
                let content_str = if let Some(parts) = content_val
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array())
                {
                    parts
                        .iter()
                        .filter_map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                } else if let Some(text) = content_val
                    .and_then(|c| c.get("text"))
                    .and_then(|t| t.as_str())
                {
                    text.to_string()
                } else {
                    continue;
                };

                if content_str.trim().is_empty() {
                    continue;
                }

                // Get timestamp (ChatGPT uses float seconds)
                let created_at = msg
                    .get("create_time")
                    .and_then(|v| v.as_f64())
                    .map(|ts| (ts * 1000.0) as i64);

                // NOTE: Do NOT filter individual messages by timestamp here!
                // The file-level check in file_modified_since() is sufficient.
                // Filtering messages would cause older messages to be lost when
                // the file is re-indexed after new messages are added.

                started_at = started_at.or(created_at);
                ended_at = created_at.or(ended_at);

                // Get model info
                let model = msg
                    .get("metadata")
                    .and_then(|m| m.get("model_slug"))
                    .and_then(|v| v.as_str())
                    .map(String::from);

                messages.push(NormalizedMessage {
                    idx: messages.len() as i64,
                    role: role.to_string(),
                    author: model,
                    created_at,
                    content: content_str,
                    extra: msg.clone(),
                    snippets: Vec::new(),
                });
            }
        }

        // Also try simple messages array format
        if messages.is_empty()
            && let Some(msgs) = val.get("messages").and_then(|v| v.as_array())
        {
            for item in msgs {
                let role = item
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("assistant");

                if role == "system" {
                    continue;
                }

                let content = item.get("content").and_then(|v| v.as_str()).unwrap_or("");

                if content.trim().is_empty() {
                    continue;
                }

                let created_at = item
                    .get("timestamp")
                    .or_else(|| item.get("create_time"))
                    .and_then(crate::connectors::parse_timestamp);

                // NOTE: Do NOT filter individual messages by timestamp here!
                // File-level check is sufficient for incremental indexing.

                started_at = started_at.or(created_at);
                ended_at = created_at.or(ended_at);

                messages.push(NormalizedMessage {
                    idx: messages.len() as i64,
                    role: role.to_string(),
                    author: None,
                    created_at,
                    content: content.to_string(),
                    extra: item.clone(),
                    snippets: Vec::new(),
                });
            }
        }

        if messages.is_empty() {
            return Ok(None);
        }

        Ok(Some(NormalizedConversation {
            agent_slug: "chatgpt".to_string(),
            external_id: conv_id,
            title,
            workspace: None, // ChatGPT doesn't have workspace concept
            source_path: path.clone(),
            started_at,
            ended_at,
            metadata: serde_json::json!({
                "source": if is_encrypted { "chatgpt_desktop_encrypted" } else { "chatgpt_desktop" },
                "model": val.get("model").and_then(|v| v.as_str()),
                "encrypted": is_encrypted,
            }),
            messages,
        }))
    }
}

impl Connector for ChatGptConnector {
    fn detect(&self) -> DetectionResult {
        if let Some(base) = Self::app_support_dir()
            && base.exists()
        {
            let conv_dirs = Self::find_conversation_dirs(&base);
            if !conv_dirs.is_empty() {
                let encrypted_count = conv_dirs.iter().filter(|(_, enc)| *enc).count();
                let unencrypted_count = conv_dirs.len() - encrypted_count;

                let mut evidence = vec![format!("found ChatGPT at {}", base.display())];

                if unencrypted_count > 0 {
                    evidence.push(format!(
                        "{} unencrypted conversation dir(s) (readable)",
                        unencrypted_count
                    ));
                }
                if encrypted_count > 0 {
                    if self.encryption_key.is_some() {
                        evidence.push(format!(
                            "{} encrypted conversation dir(s) (decryption key available)",
                            encrypted_count
                        ));
                    } else {
                        evidence.push(format!(
                            "{} encrypted conversation dir(s) (set CHATGPT_ENCRYPTION_KEY to decrypt)",
                            encrypted_count
                        ));
                    }
                }

                return DetectionResult {
                    detected: true,
                    evidence,
                    root_paths: vec![base],
                };
            }
        }
        DetectionResult::not_found()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Determine base directory
        let has_conversation_dirs = |path: &PathBuf| {
            fs::read_dir(path)
                .map(|entries| {
                    entries.flatten().any(|entry| {
                        entry.file_name().to_str().is_some_and(|name| {
                            name.starts_with("conversations-") && entry.path().is_dir()
                        })
                    })
                })
                .unwrap_or(false)
        };

        let looks_like_base = |path: &PathBuf| {
            path.file_name()
                .is_some_and(|n| n.to_str().unwrap_or("").contains("openai"))
                || has_conversation_dirs(path)
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

        let conv_dirs = Self::find_conversation_dirs(&base);
        let mut all_convs = Vec::new();

        for (dir_path, is_encrypted) in conv_dirs {
            // Skip encrypted directories if we don't have a key
            if is_encrypted && self.encryption_key.is_none() {
                tracing::debug!(
                    path = %dir_path.display(),
                    "chatgpt skipping encrypted directory (no decryption key)"
                );
                continue;
            }

            // Walk through conversation files
            for entry in WalkDir::new(&dir_path).max_depth(1).into_iter().flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }

                let path = entry.path();
                let ext = path.extension().and_then(|s| s.to_str());

                // Look for JSON or data files
                if ext != Some("json") && ext != Some("data") {
                    continue;
                }

                // Skip files not modified since last scan
                if !crate::connectors::file_modified_since(path, ctx.since_ts) {
                    continue;
                }

                match self.parse_conversation_file(&path.to_path_buf(), ctx.since_ts, is_encrypted)
                {
                    Ok(Some(conv)) => {
                        tracing::debug!(
                            path = %path.display(),
                            messages = conv.messages.len(),
                            encrypted = is_encrypted,
                            "chatgpt extracted conversation"
                        );
                        all_convs.push(conv);
                    }
                    Ok(None) => {
                        tracing::debug!(
                            path = %path.display(),
                            "chatgpt no messages in conversation"
                        );
                    }
                    Err(e) => {
                        if is_encrypted {
                            tracing::warn!(
                                path = %path.display(),
                                error = %e,
                                "chatgpt failed to decrypt/parse conversation (key might be wrong)"
                            );
                        } else {
                            tracing::warn!(
                                path = %path.display(),
                                error = %e,
                                "chatgpt failed to parse conversation"
                            );
                        }
                    }
                }
            }
        }

        Ok(all_convs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    // =========================================================================
    // Constructor tests
    // =========================================================================

    #[test]
    fn connector_with_key_stores_key() {
        let key_bytes = [42u8; KEY_SIZE];
        let connector = ChatGptConnector {
            encryption_key: Some(key_bytes),
        };
        assert!(connector.encryption_key.is_some());
        assert_eq!(connector.encryption_key.unwrap(), key_bytes);
    }

    #[test]
    fn connector_without_key_stores_none() {
        let connector = ChatGptConnector {
            encryption_key: None,
        };
        assert!(connector.encryption_key.is_none());
    }

    // =========================================================================
    // find_conversation_dirs tests
    // =========================================================================

    #[test]
    fn find_conversation_dirs_empty_for_nonexistent() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("nonexistent");

        let dirs = ChatGptConnector::find_conversation_dirs(&base);
        assert!(dirs.is_empty());
    }

    #[test]
    fn find_conversation_dirs_detects_v1_unencrypted() {
        let dir = TempDir::new().unwrap();
        let conv_dir = dir.path().join("conversations-abc123");
        fs::create_dir_all(&conv_dir).unwrap();

        let dirs = ChatGptConnector::find_conversation_dirs(&dir.path().to_path_buf());

        assert_eq!(dirs.len(), 1);
        assert!(!dirs[0].1); // Not encrypted
    }

    #[test]
    fn find_conversation_dirs_detects_v2_encrypted() {
        let dir = TempDir::new().unwrap();
        let conv_dir = dir.path().join("conversations-v2-abc123");
        fs::create_dir_all(&conv_dir).unwrap();

        let dirs = ChatGptConnector::find_conversation_dirs(&dir.path().to_path_buf());

        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].1); // Encrypted
    }

    #[test]
    fn find_conversation_dirs_detects_v3_encrypted() {
        let dir = TempDir::new().unwrap();
        let conv_dir = dir.path().join("conversations-v3-abc123");
        fs::create_dir_all(&conv_dir).unwrap();

        let dirs = ChatGptConnector::find_conversation_dirs(&dir.path().to_path_buf());

        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].1); // Encrypted
    }

    #[test]
    fn find_conversation_dirs_mixed_versions() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("conversations-old")).unwrap();
        fs::create_dir_all(dir.path().join("conversations-v2-new")).unwrap();
        fs::create_dir_all(dir.path().join("other-folder")).unwrap();

        let dirs = ChatGptConnector::find_conversation_dirs(&dir.path().to_path_buf());

        assert_eq!(dirs.len(), 2);
        // One encrypted, one not
        let encrypted_count = dirs.iter().filter(|(_, enc)| *enc).count();
        let unencrypted_count = dirs.iter().filter(|(_, enc)| !*enc).count();
        assert_eq!(encrypted_count, 1);
        assert_eq!(unencrypted_count, 1);
    }

    // =========================================================================
    // decrypt_file tests
    // =========================================================================

    #[test]
    fn decrypt_file_fails_without_key() {
        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let data = vec![0u8; 100];

        let result = connector.decrypt_file(&data);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No encryption key")
        );
    }

    #[test]
    fn decrypt_file_fails_for_too_short_data() {
        let connector = ChatGptConnector {
            encryption_key: Some([0u8; KEY_SIZE]),
        };
        // Less than NONCE_SIZE + TAG_SIZE
        let data = vec![0u8; 10];

        let result = connector.decrypt_file(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too short"));
    }

    #[test]
    fn decrypt_file_fails_with_wrong_key() {
        let connector = ChatGptConnector {
            encryption_key: Some([0u8; KEY_SIZE]),
        };
        // Valid-length but garbage data
        let data = vec![0u8; NONCE_SIZE + 50 + TAG_SIZE];

        let result = connector.decrypt_file(&data);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Decryption failed")
        );
    }

    // =========================================================================
    // parse_conversation_file tests (mapping format)
    // =========================================================================

    #[test]
    fn parse_mapping_format_conversation() {
        let dir = TempDir::new().unwrap();
        let conv_file = dir.path().join("conv1.json");

        let conv_json = json!({
            "id": "conv-123",
            "title": "Test Conversation",
            "mapping": {
                "node1": {
                    "parent": null,
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["Hello, ChatGPT!"]},
                        "create_time": 1700000000.123
                    }
                },
                "node2": {
                    "parent": "node1",
                    "message": {
                        "author": {"role": "assistant"},
                        "content": {"parts": ["Hello! How can I help you?"]},
                        "create_time": 1700000001.456,
                        "metadata": {"model_slug": "gpt-4"}
                    }
                }
            }
        });

        fs::write(&conv_file, conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let result = connector.parse_conversation_file(&conv_file, None, false);

        assert!(result.is_ok());
        let conv = result.unwrap().unwrap();

        assert_eq!(conv.agent_slug, "chatgpt");
        assert_eq!(conv.external_id, Some("conv-123".to_string()));
        assert_eq!(conv.title, Some("Test Conversation".to_string()));
        assert_eq!(conv.messages.len(), 2);

        // Check first message (user)
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "Hello, ChatGPT!");

        // Check second message (assistant with model)
        assert_eq!(conv.messages[1].role, "assistant");
        assert!(conv.messages[1].content.contains("How can I help"));
        assert_eq!(conv.messages[1].author, Some("gpt-4".to_string()));
    }

    #[test]
    fn parse_mapping_preserves_ended_at_with_missing_timestamp() {
        let dir = TempDir::new().unwrap();
        let conv_file = dir.path().join("conv_missing_ts.json");

        let conv_json = json!({
            "id": "conv-missing-ts",
            "mapping": {
                "node1": {
                    "parent": null,
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["First message"]},
                        "create_time": 1700000000.0
                    }
                },
                "node2": {
                    "parent": "node1",
                    "message": {
                        "author": {"role": "assistant"},
                        "content": {"parts": ["Second message without timestamp"]}
                    }
                }
            }
        });

        fs::write(&conv_file, conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let result = connector.parse_conversation_file(&conv_file, None, false);

        let conv = result.unwrap().unwrap();
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].content, "First message");
        assert_eq!(conv.messages[1].content, "Second message without timestamp");
        assert_eq!(conv.started_at, Some(1_700_000_000_000));
        assert_eq!(conv.ended_at, Some(1_700_000_000_000));
    }

    #[test]
    fn parse_skips_system_messages() {
        let dir = TempDir::new().unwrap();
        let conv_file = dir.path().join("conv.json");

        let conv_json = json!({
            "id": "conv-sys",
            "mapping": {
                "sys": {
                    "parent": null,
                    "message": {
                        "author": {"role": "system"},
                        "content": {"parts": ["You are a helpful assistant."]},
                        "create_time": 1700000000.0
                    }
                },
                "user": {
                    "parent": "sys",
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["Hi!"]},
                        "create_time": 1700000001.0
                    }
                }
            }
        });

        fs::write(&conv_file, conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let result = connector.parse_conversation_file(&conv_file, None, false);

        let conv = result.unwrap().unwrap();
        // System message should be skipped
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].role, "user");
    }

    #[test]
    fn parse_skips_empty_content() {
        let dir = TempDir::new().unwrap();
        let conv_file = dir.path().join("conv.json");

        let conv_json = json!({
            "id": "conv-empty",
            "mapping": {
                "empty": {
                    "parent": null,
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": [""]},
                        "create_time": 1700000000.0
                    }
                },
                "whitespace": {
                    "parent": "empty",
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["   \n\t  "]},
                        "create_time": 1700000001.0
                    }
                },
                "valid": {
                    "parent": "whitespace",
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["Valid message"]},
                        "create_time": 1700000002.0
                    }
                }
            }
        });

        fs::write(&conv_file, conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let result = connector.parse_conversation_file(&conv_file, None, false);

        let conv = result.unwrap().unwrap();
        // Empty/whitespace messages should be skipped
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].content, "Valid message");
    }

    // =========================================================================
    // parse_conversation_file tests (simple messages array format)
    // =========================================================================

    #[test]
    fn parse_simple_messages_array_format() {
        let dir = TempDir::new().unwrap();
        let conv_file = dir.path().join("conv.json");

        let conv_json = json!({
            "id": "simple-conv",
            "title": "Simple Format",
            "messages": [
                {
                    "role": "user",
                    "content": "Question?",
                    "timestamp": 1700000000000i64
                },
                {
                    "role": "assistant",
                    "content": "Answer!",
                    "timestamp": 1700000001000i64
                }
            ]
        });

        fs::write(&conv_file, conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let result = connector.parse_conversation_file(&conv_file, None, false);

        let conv = result.unwrap().unwrap();
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "Question?");
        assert_eq!(conv.messages[1].role, "assistant");
        assert_eq!(conv.messages[1].content, "Answer!");
    }

    #[test]
    fn parse_content_with_text_field() {
        let dir = TempDir::new().unwrap();
        let conv_file = dir.path().join("conv.json");

        let conv_json = json!({
            "id": "text-content",
            "mapping": {
                "node1": {
                    "message": {
                        "author": {"role": "user"},
                        "content": {"text": "Using text field instead of parts"},
                        "create_time": 1700000000.0
                    }
                }
            }
        });

        fs::write(&conv_file, conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let result = connector.parse_conversation_file(&conv_file, None, false);

        let conv = result.unwrap().unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert!(conv.messages[0].content.contains("text field"));
    }

    // =========================================================================
    // Edge case tests
    // =========================================================================

    #[test]
    fn parse_returns_none_for_empty_conversation() {
        let dir = TempDir::new().unwrap();
        let conv_file = dir.path().join("empty.json");

        let conv_json = json!({
            "id": "empty-conv",
            "mapping": {}
        });

        fs::write(&conv_file, conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let result = connector.parse_conversation_file(&conv_file, None, false);

        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn parse_handles_missing_optional_fields() {
        let dir = TempDir::new().unwrap();
        let conv_file = dir.path().join("minimal.json");

        // Minimal conversation - no id, no title, no timestamps
        let conv_json = json!({
            "mapping": {
                "node1": {
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["Minimal message"]}
                    }
                }
            }
        });

        fs::write(&conv_file, conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let result = connector.parse_conversation_file(&conv_file, None, false);

        let conv = result.unwrap().unwrap();
        assert_eq!(conv.messages.len(), 1);
        // external_id falls back to filename stem
        assert!(conv.external_id.is_some());
        assert!(conv.title.is_none());
        assert!(conv.started_at.is_none());
    }

    #[test]
    fn parse_extracts_id_from_conversation_id_field() {
        let dir = TempDir::new().unwrap();
        let conv_file = dir.path().join("conv.json");

        let conv_json = json!({
            "conversation_id": "alt-id-123",
            "mapping": {
                "node1": {
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["Test"]}
                    }
                }
            }
        });

        fs::write(&conv_file, conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let result = connector.parse_conversation_file(&conv_file, None, false);

        let conv = result.unwrap().unwrap();
        assert_eq!(conv.external_id, Some("alt-id-123".to_string()));
    }

    #[test]
    fn parse_fails_gracefully_for_invalid_json() {
        let dir = TempDir::new().unwrap();
        let conv_file = dir.path().join("invalid.json");

        fs::write(&conv_file, "not valid json {{{").unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let result = connector.parse_conversation_file(&conv_file, None, false);

        assert!(result.is_err());
    }

    #[test]
    fn parse_multipart_content_joins_with_newlines() {
        let dir = TempDir::new().unwrap();
        let conv_file = dir.path().join("multipart.json");

        let conv_json = json!({
            "mapping": {
                "node1": {
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["Part 1", "Part 2", "Part 3"]},
                        "create_time": 1700000000.0
                    }
                }
            }
        });

        fs::write(&conv_file, conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let result = connector.parse_conversation_file(&conv_file, None, false);

        let conv = result.unwrap().unwrap();
        assert_eq!(conv.messages[0].content, "Part 1\nPart 2\nPart 3");
    }

    #[test]
    fn parse_sets_metadata_correctly() {
        let dir = TempDir::new().unwrap();
        let conv_file = dir.path().join("meta.json");

        let conv_json = json!({
            "model": "gpt-4-turbo",
            "mapping": {
                "node1": {
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["Test"]}
                    }
                }
            }
        });

        fs::write(&conv_file, conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let result = connector.parse_conversation_file(&conv_file, None, false);

        let conv = result.unwrap().unwrap();
        assert_eq!(conv.metadata["source"], "chatgpt_desktop");
        assert_eq!(conv.metadata["model"], "gpt-4-turbo");
        assert_eq!(conv.metadata["encrypted"], false);
    }

    #[test]
    fn parse_encrypted_flag_in_metadata() {
        let dir = TempDir::new().unwrap();
        let conv_file = dir.path().join("enc.json");

        // This will fail decryption but we can test the metadata flag
        let conv_json = json!({
            "mapping": {
                "node1": {
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["Test"]}
                    }
                }
            }
        });

        fs::write(&conv_file, conv_json.to_string()).unwrap();

        // Parse as unencrypted first to test flag
        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let result = connector.parse_conversation_file(&conv_file, None, false);
        let conv = result.unwrap().unwrap();
        assert_eq!(conv.metadata["encrypted"], false);
    }

    // =========================================================================
    // Timestamps and ordering tests
    // =========================================================================

    #[test]
    fn parse_orders_messages_by_create_time() {
        let dir = TempDir::new().unwrap();
        let conv_file = dir.path().join("ordered.json");

        // Messages in reverse order in JSON
        let conv_json = json!({
            "mapping": {
                "late": {
                    "message": {
                        "author": {"role": "assistant"},
                        "content": {"parts": ["Second"]},
                        "create_time": 1700000002.0
                    }
                },
                "early": {
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["First"]},
                        "create_time": 1700000001.0
                    }
                }
            }
        });

        fs::write(&conv_file, conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let result = connector.parse_conversation_file(&conv_file, None, false);

        let conv = result.unwrap().unwrap();
        // Should be ordered by create_time
        assert_eq!(conv.messages[0].content, "First");
        assert_eq!(conv.messages[1].content, "Second");
    }

    #[test]
    fn parse_converts_float_timestamp_to_millis() {
        let dir = TempDir::new().unwrap();
        let conv_file = dir.path().join("ts.json");

        let conv_json = json!({
            "mapping": {
                "node1": {
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["Test"]},
                        "create_time": 1700000000.5  // .5 seconds
                    }
                }
            }
        });

        fs::write(&conv_file, conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };
        let result = connector.parse_conversation_file(&conv_file, None, false);

        let conv = result.unwrap().unwrap();
        // Should be converted to milliseconds
        assert_eq!(conv.messages[0].created_at, Some(1700000000500));
    }

    // =========================================================================
    // Detection tests
    // =========================================================================

    #[test]
    fn detect_not_found_without_app_dir() {
        // This test will pass on systems without ChatGPT installed
        let connector = ChatGptConnector {
            encryption_key: None,
        };

        // On most CI/test systems, ChatGPT won't be installed
        // Just verify detect() doesn't panic and returns a valid result
        let result = connector.detect();
        // Result could be either found or not found depending on system
        // Just verify detect() returned without panicking
        let _ = result.detected;
    }

    // =========================================================================
    // Scan tests
    // =========================================================================

    #[test]
    fn scan_empty_directory_returns_empty() {
        let dir = TempDir::new().unwrap();
        let connector = ChatGptConnector {
            encryption_key: None,
        };

        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn scan_skips_encrypted_without_key() {
        let dir = TempDir::new().unwrap();

        // Create encrypted directory structure
        let conv_dir = dir.path().join("conversations-v2-abc123");
        fs::create_dir_all(&conv_dir).unwrap();

        // Create a conversation file (will be skipped as encrypted)
        let conv_json = json!({
            "id": "test",
            "mapping": {
                "node1": {
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["Test"]}
                    }
                }
            }
        });
        fs::write(conv_dir.join("conv.json"), conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };

        // Use a context that will make it look for conversations in our dir
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        // Encrypted dirs are skipped without key
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn scan_processes_unencrypted_conversations() {
        let dir = TempDir::new().unwrap();

        // Create a directory structure that mimics com.openai.chat
        let openai_dir = dir.path().join("com.openai.chat");
        fs::create_dir_all(&openai_dir).unwrap();

        // Create unencrypted directory structure
        let conv_dir = openai_dir.join("conversations-uuid123");
        fs::create_dir_all(&conv_dir).unwrap();

        let conv_json = json!({
            "id": "test-conv",
            "title": "Test Title",
            "mapping": {
                "node1": {
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["Hello!"]},
                        "create_time": 1700000000.0
                    }
                }
            }
        });
        fs::write(conv_dir.join("conv.json"), conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };

        // Pass the openai directory so scan recognizes it
        let ctx = ScanContext::local_default(openai_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].title, Some("Test Title".to_string()));
    }

    #[test]
    fn scan_accepts_base_with_conversations_dir() {
        let dir = TempDir::new().unwrap();

        let conv_dir = dir.path().join("conversations-uuid123");
        fs::create_dir_all(&conv_dir).unwrap();

        let conv_json = json!( {
            "id": "test-conv",
            "title": "Direct Base",
            "mapping": {
                "node1": {
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["Hello!"]},
                        "create_time": 1700000000.0
                    }
                }
            }
        });
        fs::write(conv_dir.join("conv.json"), conv_json.to_string()).unwrap();

        let connector = ChatGptConnector {
            encryption_key: None,
        };

        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].title, Some("Direct Base".to_string()));
    }
}
