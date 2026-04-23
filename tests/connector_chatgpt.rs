mod util;

use coding_agent_search::connectors::chatgpt::ChatGptConnector;
use coding_agent_search::connectors::{Connector, ScanContext};
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use base64::prelude::*;
use serial_test::serial;
use std::fs::{self, File};
use std::path::Path;
use tempfile::TempDir;
use util::EnvGuard;

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

const CHATGPT_TEST_KEY: [u8; 32] = [
    0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc, 0xfe, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
    0xef, 0xf0, 0xe1, 0xd2, 0xc3, 0xb4, 0xa5, 0x96, 0x87, 0x78, 0x69, 0x5a, 0x4b, 0x3c, 0x2d,
    0x1e, 0x0f,
];

fn load_fixture_bytes(rel_path: &str) -> Vec<u8> {
    fs::read(chatgpt_real_fixture_root().join(rel_path)).unwrap()
}

fn encrypt_chatgpt_payload(plaintext: &[u8], nonce_bytes: [u8; 12]) -> Vec<u8> {
    let cipher = Aes256Gcm::new_from_slice(&CHATGPT_TEST_KEY).unwrap();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher.encrypt(nonce, plaintext).unwrap();
    let mut output = nonce_bytes.to_vec();
    output.extend_from_slice(&ciphertext);
    output
}

// ============================================================================
// Detection tests
// ============================================================================

#[test]
#[serial]
fn detect_does_not_panic() {
    let connector = ChatGptConnector::new();
    let result = connector.detect();
    let _ = result.detected;
}

// ============================================================================
// Scan — mapping format (primary ChatGPT desktop format)
// ============================================================================

#[test]
#[serial]
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
    // Bead 7k7pl: pin the EXACT started/ended timestamps — the
    // mapping fixture seeds create_time = 1700000000.0 and
    // 1700000001.0 (unix seconds), which the connector must convert
    // to ms. A regression that lost precision, swapped order, or
    // dropped the conversion would slip past `.is_some()`.
    let started = convs[0].started_at.expect("started_at from create_time");
    let ended = convs[0].ended_at.expect("ended_at from create_time");
    assert_eq!(
        started, 1_700_000_000_000,
        "started_at must be seeded create_time in ms; got {started}"
    );
    assert_eq!(
        ended, 1_700_000_001_000,
        "ended_at must be seeded create_time in ms; got {ended}"
    );
    assert!(
        started <= ended,
        "started_at must be <= ended_at; got started={started}, ended={ended}"
    );
}

#[test]
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
fn scan_defaults_missing_mapping_role_to_assistant_explicitly() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let conv_dir = root.join("conversations-missing-mapping-role");
    fs::create_dir_all(&conv_dir).unwrap();

    let json = r#"{
        "id": "conv-missing-mapping-role",
        "mapping": {
            "node-1": {
                "parent": null,
                "message": {
                    "author": {},
                    "content": {"parts": ["Role fallback should stay explicit."]},
                    "create_time": 1700000000.0
                }
            }
        }
    }"#;

    write_json(&conv_dir, "missing-role.json", json);

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].role, "assistant");
    assert_eq!(
        convs[0].messages[0].content,
        "Role fallback should stay explicit."
    );
}

#[test]
#[serial]
fn scan_defaults_missing_messages_array_role_to_assistant_explicitly() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let conv_dir = root.join("conversations-missing-array-role");
    fs::create_dir_all(&conv_dir).unwrap();

    let json = r#"{
        "id": "conv-missing-array-role",
        "messages": [
            {"content": "Array-role fallback should stay explicit.", "timestamp": 1700000010000}
        ]
    }"#;

    write_json(&conv_dir, "missing-array-role.json", json);

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].role, "assistant");
    assert_eq!(
        convs[0].messages[0].content,
        "Array-role fallback should stay explicit."
    );
}

#[test]
#[serial]
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

#[test]
#[serial]
fn scan_preserves_string_parts_and_drops_object_only_structured_parts_from_real_fixture() {
    let root = chatgpt_real_fixture_root();
    let expected_path = root.join("conversations-real/conv-structured-parts.json");

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.clone(), None);
    let convs = connector.scan(&ctx).unwrap();
    let conv = convs
        .into_iter()
        .find(|conv| conv.source_path == expected_path)
        .expect("structured-parts fixture should be discovered");

    assert_eq!(conv.external_id.as_deref(), Some("chatgpt-structured-parts-001"));
    assert_eq!(conv.messages.len(), 1);
    assert_eq!(conv.messages[0].role, "user");
    assert_eq!(
        conv.messages[0].content,
        "Leading plain text.\nTrailing plain text."
    );
}

// ============================================================================
// Incremental scanning (since_ts)
// ============================================================================

#[test]
#[serial]
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
#[serial]
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

#[test]
#[serial]
fn scan_parses_encrypted_conversation_id_fixture_with_env_key() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let enc_dir = root.join("conversations-v2-success");
    fs::create_dir_all(&enc_dir).unwrap();

    let ciphertext = encrypt_chatgpt_payload(
        &load_fixture_bytes("conversations-real/conv-conversation-id.json"),
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
    );
    fs::write(enc_dir.join("conv.data"), ciphertext).unwrap();

    let _key_guard = EnvGuard::set(
        "CHATGPT_ENCRYPTION_KEY",
        BASE64_STANDARD.encode(CHATGPT_TEST_KEY),
    );

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(
        conv.external_id.as_deref(),
        Some("chatgpt-desktop-conv-alt-001")
    );
    assert_eq!(conv.title.as_deref(), Some("Conversation ID Fixture"));
    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].role, "user");
    assert_eq!(
        conv.messages[0].content,
        "Use conversation_id as the stable external id."
    );
    assert_eq!(conv.messages[1].author.as_deref(), Some("gpt-4o"));
    assert_eq!(conv.metadata["source"], "chatgpt_desktop_encrypted");
    assert_eq!(conv.metadata["encrypted"], true);
}

#[test]
#[serial]
fn scan_parses_encrypted_multipart_fixture_with_env_key() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let enc_dir = root.join("conversations-v3-multipart");
    fs::create_dir_all(&enc_dir).unwrap();

    let ciphertext = encrypt_chatgpt_payload(
        &load_fixture_bytes("conversations-real/conv-multipart.json"),
        [12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1],
    );
    fs::write(enc_dir.join("conv.data"), ciphertext).unwrap();

    let _key_guard = EnvGuard::set(
        "CHATGPT_ENCRYPTION_KEY",
        BASE64_STANDARD.encode(CHATGPT_TEST_KEY),
    );

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.external_id.as_deref(), Some("chatgpt-multipart-001"));
    assert_eq!(conv.messages.len(), 1);
    assert_eq!(conv.messages[0].role, "user");
    assert_eq!(
        conv.messages[0].content,
        "First paragraph.\nSecond paragraph.\n```rust\nfn main() {}\n```"
    );
    assert_eq!(conv.metadata["source"], "chatgpt_desktop_encrypted");
}

#[test]
#[serial]
fn scan_continues_past_malformed_encrypted_file_with_valid_key() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let enc_dir = root.join("conversations-v2-bad");
    fs::create_dir_all(&enc_dir).unwrap();
    fs::write(enc_dir.join("conv.data"), b"too-short").unwrap();

    let plain_dir = root.join("conversations-plain");
    fs::create_dir_all(&plain_dir).unwrap();
    write_json(
        &plain_dir,
        "conv.json",
        r#"{"id":"plain","messages":[{"role":"user","content":"Recovered plain conversation"}]}"#,
    );

    let _key_guard = EnvGuard::set(
        "CHATGPT_ENCRYPTION_KEY",
        BASE64_STANDARD.encode(CHATGPT_TEST_KEY),
    );

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some("plain"));
    assert_eq!(convs[0].messages[0].content, "Recovered plain conversation");
}

#[test]
#[serial]
fn scan_skips_oversized_encrypted_file_even_with_key() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let enc_dir = root.join("conversations-v2-huge");
    fs::create_dir_all(&enc_dir).unwrap();
    let huge_file = enc_dir.join("conv.data");
    let file = File::create(&huge_file).unwrap();
    file.set_len(100 * 1024 * 1024 + 1).unwrap();

    let plain_dir = root.join("conversations-plain");
    fs::create_dir_all(&plain_dir).unwrap();
    write_json(
        &plain_dir,
        "conv.json",
        r#"{"id":"plain","messages":[{"role":"user","content":"Small sibling conversation"}]}"#,
    );

    let _key_guard = EnvGuard::set(
        "CHATGPT_ENCRYPTION_KEY",
        BASE64_STANDARD.encode(CHATGPT_TEST_KEY),
    );

    let connector = ChatGptConnector::new();
    let ctx = ScanContext::local_default(root.to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some("plain"));
    assert_eq!(convs[0].messages[0].content, "Small sibling conversation");
}

// ============================================================================
// Message ordering
// ============================================================================

#[test]
#[serial]
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
#[serial]
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
