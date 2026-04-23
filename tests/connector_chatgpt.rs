use coding_agent_search::connectors::chatgpt::ChatGptConnector;
use coding_agent_search::connectors::{Connector, ScanContext};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

// ============================================================================
// Helper
// ============================================================================

fn write_json(dir: &Path, rel_path: &str, content: &str) -> std::path::PathBuf {
    let path = dir.join(rel_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    path
}

fn chatgpt_real_fixture_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/chatgpt_real")
}

// ============================================================================
// Detection tests
// ============================================================================

#[test]
fn detect_does_not_panic() {
    let connector = ChatGptConnector::new();
    let result = connector.detect();
    let _ = result.detected;
}

// ============================================================================
// Scan — mapping format (primary ChatGPT desktop format)
// ============================================================================

#[test]
fn scan_parses_mapping_format() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // ChatGPT stores conversations in conversations-{uuid}/ directories
    let conv_dir = root.join("conversations-abc123");
    fs::create_dir_all(&conv_dir).unwrap();

    let json = r#"{
        "id": "conv-mapping-001",
        "title": "Sort question",
        "mapping": {
            "node-1": {
                "parent": null,
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["How do I sort?"]},
                    "create_time": 1700000000.0
                }
            },
            "node-2": {
                "parent": "node-1",
                "message": {
                    "author": {"role": "assistant"},
                    "content": {"parts": ["Use .sort() method."]},
                    "create_time": 1700000001.0,
                    "metadata": {"model_slug": "gpt-4"}
                }
            }
        }
    }"#;

    write_json(&conv_dir, "conv-001.json", json);

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].agent_slug, "chatgpt");
    assert_eq!(convs[0].title.as_deref(), Some("Sort question"));
    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].role, "user");
    assert!(convs[0].messages[0].content.contains("sort"));
    assert_eq!(convs[0].messages[1].role, "assistant");
    assert!(convs[0].started_at.is_some());
    assert!(convs[0].ended_at.is_some());
}

#[test]
fn scan_skips_system_messages_in_mapping() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let conv_dir = root.join("conversations-sys");
    fs::create_dir_all(&conv_dir).unwrap();

    let json = r#"{
        "id": "conv-sys",
        "mapping": {
            "node-sys": {
                "parent": null,
                "message": {
                    "author": {"role": "system"},
                    "content": {"parts": ["You are a helpful assistant."]},
                    "create_time": 1700000000.0
                }
            },
            "node-user": {
                "parent": "node-sys",
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Hello"]},
                    "create_time": 1700000001.0
                }
            }
        }
    }"#;

    write_json(&conv_dir, "sys.json", json);

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].messages.len(),
        1,
        "system messages should be skipped"
    );
    assert_eq!(convs[0].messages[0].role, "user");
}

// ============================================================================
// Scan — messages array format
// ============================================================================

#[test]
fn scan_parses_messages_array_format() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let conv_dir = root.join("conversations-simple");
    fs::create_dir_all(&conv_dir).unwrap();

    let json = r#"{
        "id": "conv-simple",
        "title": "Simple chat",
        "messages": [
            {"role": "user", "content": "What is Rust?", "timestamp": 1700000010000},
            {"role": "assistant", "content": "Rust is a systems programming language.", "timestamp": 1700000011000}
        ]
    }"#;

    write_json(&conv_dir, "simple.json", json);

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title.as_deref(), Some("Simple chat"));
    assert_eq!(convs[0].messages.len(), 2);
}

// ============================================================================
// Scan — multiple conversations
// ============================================================================

#[test]
fn scan_parses_multiple_conversation_files() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let conv_dir = root.join("conversations-multi");
    fs::create_dir_all(&conv_dir).unwrap();

    for i in 1..=3 {
        let json = format!(
            r#"{{"id":"conv-{i}","title":"Chat {i}","messages":[{{"role":"user","content":"Message {i}"}}]}}"#
        );
        write_json(&conv_dir, &format!("conv-{i}.json"), &json);
    }

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 3);
}

// ============================================================================
// Edge cases
// ============================================================================

#[test]
fn scan_empty_dir_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // No conversations-* directories at all
    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn scan_skips_empty_content_in_mapping() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let conv_dir = root.join("conversations-empty");
    fs::create_dir_all(&conv_dir).unwrap();

    let json = r#"{
        "id": "conv-empty-parts",
        "mapping": {
            "node-empty": {
                "parent": null,
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": [""]},
                    "create_time": 1700000000.0
                }
            },
            "node-real": {
                "parent": "node-empty",
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Real content"]},
                    "create_time": 1700000001.0
                }
            }
        }
    }"#;

    write_json(&conv_dir, "empty-parts.json", json);

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Real content");
}

#[test]
fn scan_extracts_id_from_filename_when_missing() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let conv_dir = root.join("conversations-fallback");
    fs::create_dir_all(&conv_dir).unwrap();

    // No "id" field; external_id should fall back to filename stem
    let json = r#"{
        "messages": [
            {"role": "user", "content": "Test"}
        ]
    }"#;

    write_json(&conv_dir, "my-fallback-id.json", json);

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some("my-fallback-id"));
}

#[test]
fn scan_extracts_conversation_id_from_real_fixture() {
    let root = chatgpt_real_fixture_root();
    let expected_path = root.join("conversations-real/conv-conversation-id.json");

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.clone(), None);
    let convs = connector.scan(&ctx).unwrap();
    let conv = convs
        .into_iter()
        .find(|conv| conv.source_path == expected_path)
        .expect("conversation_id fixture should be discovered");

    assert_eq!(
        conv.external_id.as_deref(),
        Some("chatgpt-desktop-conv-alt-001")
    );
    assert_eq!(conv.title.as_deref(), Some("Conversation ID Fixture"));
    assert_eq!(conv.messages.len(), 2);
    assert_eq!(
        conv.messages[0].content,
        "Use conversation_id as the stable external id."
    );
    assert_eq!(conv.messages[1].author.as_deref(), Some("gpt-4o"));
}

#[test]
fn scan_handles_content_text_field() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let conv_dir = root.join("conversations-textfield");
    fs::create_dir_all(&conv_dir).unwrap();

    // Use content.text instead of content.parts
    let json = r#"{
        "id": "conv-text",
        "mapping": {
            "node-1": {
                "parent": null,
                "message": {
                    "author": {"role": "user"},
                    "content": {"text": "Via text field"},
                    "create_time": 1700000000.0
                }
            }
        }
    }"#;

    write_json(&conv_dir, "text.json", json);

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Via text field");
}

#[test]
fn scan_joins_multipart_content_from_real_fixture() {
    let root = chatgpt_real_fixture_root();
    let expected_path = root.join("conversations-real/conv-multipart.json");

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.clone(), None);
    let convs = connector.scan(&ctx).unwrap();
    let conv = convs
        .into_iter()
        .find(|conv| conv.source_path == expected_path)
        .expect("multipart fixture should be discovered");

    assert_eq!(conv.external_id.as_deref(), Some("chatgpt-multipart-001"));
    assert_eq!(conv.messages.len(), 1);
    assert_eq!(
        conv.messages[0].content,
        "First paragraph.\nSecond paragraph.\n```rust\nfn main() {}\n```"
    );
    assert_eq!(conv.messages[0].role, "user");
}

// ============================================================================
// Incremental scanning (since_ts)
// ============================================================================

#[test]
fn scan_respects_since_ts() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let conv_dir = root.join("conversations-old");
    fs::create_dir_all(&conv_dir).unwrap();

    write_json(
        &conv_dir,
        "old.json",
        r#"{"id":"old","messages":[{"role":"user","content":"old msg"}]}"#,
    );

    let connector = ChatGptConnector::new();
    let far_future = chrono::Utc::now().timestamp_millis() + 86_400_000;
    let ctx = ScanContext::local_default(root.to_path_buf(), Some(far_future));
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

// ============================================================================
// Encrypted directory detection
// ============================================================================

#[test]
fn scan_skips_encrypted_dir_without_key() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Create an encrypted conversations directory (v2)
    let enc_dir = root.join("conversations-v2-abc123");
    fs::create_dir_all(&enc_dir).unwrap();
    // Write some binary data pretending to be encrypted
    fs::write(enc_dir.join("conv.data"), b"fake-encrypted-data").unwrap();

    // Also create an unencrypted directory
    let plain_dir = root.join("conversations-plain123");
    fs::create_dir_all(&plain_dir).unwrap();
    write_json(
        &plain_dir,
        "conv.json",
        r#"{"id":"plain","messages":[{"role":"user","content":"Unencrypted"}]}"#,
    );

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    // Should only get the unencrypted conversation (encrypted is skipped without key)
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some("plain"));
}

// ============================================================================
// Message ordering
// ============================================================================

#[test]
fn scan_orders_messages_by_create_time() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let conv_dir = root.join("conversations-ordered");
    fs::create_dir_all(&conv_dir).unwrap();

    // Nodes deliberately out of order in the mapping
    let json = r#"{
        "id": "conv-ordered",
        "mapping": {
            "node-3": {
                "parent": "node-2",
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Third"]},
                    "create_time": 1700000002.0
                }
            },
            "node-1": {
                "parent": null,
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["First"]},
                    "create_time": 1700000000.0
                }
            },
            "node-2": {
                "parent": "node-1",
                "message": {
                    "author": {"role": "assistant"},
                    "content": {"parts": ["Second"]},
                    "create_time": 1700000001.0
                }
            }
        }
    }"#;

    write_json(&conv_dir, "ordered.json", json);

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages[0].content, "First");
    assert_eq!(convs[0].messages[1].content, "Second");
    assert_eq!(convs[0].messages[2].content, "Third");
}

// ============================================================================
// Model extraction
// ============================================================================

#[test]
fn scan_extracts_model_slug() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let conv_dir = root.join("conversations-model");
    fs::create_dir_all(&conv_dir).unwrap();

    let json = r#"{
        "id": "conv-model",
        "mapping": {
            "n1": {
                "parent": null,
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Hello"]},
                    "create_time": 1700000000.0
                }
            },
            "n2": {
                "parent": "n1",
                "message": {
                    "author": {"role": "assistant"},
                    "content": {"parts": ["Hi there!"]},
                    "create_time": 1700000001.0,
                    "metadata": {"model_slug": "gpt-4o"}
                }
            }
        }
    }"#;

    write_json(&conv_dir, "model.json", json);

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages[1].author.as_deref(), Some("gpt-4o"));
}
