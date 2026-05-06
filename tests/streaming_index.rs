//! Tests for streaming indexing with crossbeam channels (Opt 8.3).
//!
//! Validates that the streaming indexing architecture:
//! - Returns equivalent results to batch indexing
//! - Correctly handles the CASS_STREAMING_INDEX feature flag
//! - Reports progress correctly during streaming
//! - Handles concurrent indexing without data loss
//!
//! Part of bead: coding_agent_session_search-decq (Opt 8.3)

use assert_cmd::cargo::cargo_bin_cmd;
use coding_agent_search::indexer::streaming_index_enabled;
use coding_agent_search::storage::sqlite::SqliteStorage;
use frankensqlite::compat::{ConnectionExt, RowExt};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

mod util;
use util::EnvGuard;

static STREAMING_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Helper to create Codex session with modern envelope format.
fn make_codex_session(root: &Path, date_path: &str, filename: &str, content: &str, ts: u64) {
    let sessions = root.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    let sample = format!(
        r#"{{"type": "event_msg", "timestamp": {ts}, "payload": {{"type": "user_message", "message": "{content}"}}}}
{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "{content}_response"}}}}
"#,
        ts + 1000
    );
    fs::write(file, sample).unwrap();
}

/// Helper to create multiple Codex sessions for a corpus.
fn make_test_corpus(codex_home: &Path, count: usize) {
    for i in 0..count {
        let date_path = format!("2024/11/{:02}", (i % 30) + 1);
        let filename = format!("rollout-{i}.jsonl");
        let content = format!("test message {i} with unique content for corpus item");
        let ts = 1732118400000 + (i as u64 * 1000);
        make_codex_session(codex_home, &date_path, &filename, &content, ts);
    }
}

fn count_messages(db_path: &Path) -> i64 {
    let storage = SqliteStorage::open(db_path).expect("open sqlite");
    storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |r| r.get_typed(0))
        .expect("count messages")
}

fn count_conversations(db_path: &Path) -> i64 {
    let storage = SqliteStorage::open(db_path).expect("open sqlite");
    storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM conversations", &[], |r| {
            r.get_typed(0)
        })
        .expect("count conversations")
}

// =============================================================================
// Feature Flag Tests
// =============================================================================

#[test]
fn test_streaming_enabled_when_var_set_to_1() {
    let _env_lock = STREAMING_ENV_LOCK.lock().unwrap();
    // Explicitly setting to "1" enables streaming
    let _guard = EnvGuard::set("CASS_STREAMING_INDEX", "1");
    assert!(
        streaming_index_enabled(),
        "streaming should be enabled when CASS_STREAMING_INDEX=1"
    );
}

#[test]
fn test_streaming_disabled_via_env_var() {
    let _env_lock = STREAMING_ENV_LOCK.lock().unwrap();
    // Setting to "0" disables streaming
    let _guard = EnvGuard::set("CASS_STREAMING_INDEX", "0");
    assert!(
        !streaming_index_enabled(),
        "streaming should be disabled when CASS_STREAMING_INDEX=0"
    );
}

#[test]
fn test_streaming_enabled_with_non_zero_value() {
    let _env_lock = STREAMING_ENV_LOCK.lock().unwrap();
    // Any value other than "0" enables streaming
    let _guard = EnvGuard::set("CASS_STREAMING_INDEX", "yes");
    assert!(
        streaming_index_enabled(),
        "streaming should be enabled when CASS_STREAMING_INDEX is not '0'"
    );
}

// =============================================================================
// Equivalence Tests: Streaming vs Batch mode should produce identical results
// =============================================================================

#[test]
fn test_streaming_batch_equivalence_message_count() {
    // Create corpus and index in both modes, verify same message count
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");

    // Create test corpus
    make_test_corpus(&codex_home, 5);

    // Index with streaming mode (default)
    let data_dir_streaming = home.join("cass_streaming");
    fs::create_dir_all(&data_dir_streaming).unwrap();

    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir_streaming)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CASS_STREAMING_INDEX", "1")
        .assert()
        .success();

    let streaming_messages = count_messages(&data_dir_streaming.join("agent_search.db"));
    let streaming_convs = count_conversations(&data_dir_streaming.join("agent_search.db"));

    // Index with batch mode
    let data_dir_batch = home.join("cass_batch");
    fs::create_dir_all(&data_dir_batch).unwrap();

    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir_batch)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CASS_STREAMING_INDEX", "0")
        .assert()
        .success();

    let batch_messages = count_messages(&data_dir_batch.join("agent_search.db"));
    let batch_convs = count_conversations(&data_dir_batch.join("agent_search.db"));

    // Verify equivalence
    assert_eq!(
        streaming_messages, batch_messages,
        "Message counts should match: streaming={} batch={}",
        streaming_messages, batch_messages
    );
    assert_eq!(
        streaming_convs, batch_convs,
        "Conversation counts should match: streaming={} batch={}",
        streaming_convs, batch_convs
    );
}

#[test]
fn test_streaming_batch_equivalence_search_results() {
    // Create corpus, index in both modes, run same search queries, verify results match
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");

    // Create test corpus with searchable content
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-search1.jsonl",
        "authentication handler login",
        1732118400000,
    );
    make_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-search2.jsonl",
        "database configuration setup",
        1732204800000,
    );
    make_codex_session(
        &codex_home,
        "2024/11/22",
        "rollout-search3.jsonl",
        "error handler exception",
        1732291200000,
    );

    // Index with streaming mode
    let data_dir_streaming = home.join("cass_streaming");
    fs::create_dir_all(&data_dir_streaming).unwrap();

    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir_streaming)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CASS_STREAMING_INDEX", "1")
        .assert()
        .success();

    // Index with batch mode
    let data_dir_batch = home.join("cass_batch");
    fs::create_dir_all(&data_dir_batch).unwrap();

    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir_batch)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CASS_STREAMING_INDEX", "0")
        .assert()
        .success();

    // Search in streaming-indexed data
    let streaming_result = cargo_bin_cmd!("cass")
        .args(["search", "handler", "--json", "--data-dir"])
        .arg(&data_dir_streaming)
        .current_dir(home)
        .env("HOME", home)
        .output()
        .expect("search command");

    // Search in batch-indexed data
    let batch_result = cargo_bin_cmd!("cass")
        .args(["search", "handler", "--json", "--data-dir"])
        .arg(&data_dir_batch)
        .current_dir(home)
        .env("HOME", home)
        .output()
        .expect("search command");

    // Parse JSON results
    let streaming_json: serde_json::Value =
        serde_json::from_slice(&streaming_result.stdout).unwrap_or(serde_json::Value::Null);
    let batch_json: serde_json::Value =
        serde_json::from_slice(&batch_result.stdout).unwrap_or(serde_json::Value::Null);

    // Compare hit counts (we expect both to find "handler" in auth and error sessions)
    let streaming_hits = streaming_json
        .get("hits")
        .and_then(|h| h.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let batch_hits = batch_json
        .get("hits")
        .and_then(|h| h.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    assert_eq!(
        streaming_hits, batch_hits,
        "Search hit counts should match: streaming={} batch={}",
        streaming_hits, batch_hits
    );
}

// =============================================================================
// Determinism Tests: Same corpus should produce identical results
// =============================================================================

#[test]
fn test_streaming_indexing_deterministic() {
    // Index same corpus twice with streaming, verify identical results
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");

    // Create test corpus
    make_test_corpus(&codex_home, 10);

    // First streaming index
    let data_dir1 = home.join("cass_run1");
    fs::create_dir_all(&data_dir1).unwrap();

    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir1)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CASS_STREAMING_INDEX", "1")
        .assert()
        .success();

    // Second streaming index (fresh directory)
    let data_dir2 = home.join("cass_run2");
    fs::create_dir_all(&data_dir2).unwrap();

    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir2)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CASS_STREAMING_INDEX", "1")
        .assert()
        .success();

    // Verify both runs produce same counts
    let run1_messages = count_messages(&data_dir1.join("agent_search.db"));
    let run2_messages = count_messages(&data_dir2.join("agent_search.db"));
    let run1_convs = count_conversations(&data_dir1.join("agent_search.db"));
    let run2_convs = count_conversations(&data_dir2.join("agent_search.db"));

    assert_eq!(
        run1_messages, run2_messages,
        "Message counts should be deterministic: run1={} run2={}",
        run1_messages, run2_messages
    );
    assert_eq!(
        run1_convs, run2_convs,
        "Conversation counts should be deterministic: run1={} run2={}",
        run1_convs, run2_convs
    );
}

// =============================================================================
// Larger Corpus Tests
// =============================================================================

#[test]
fn test_streaming_larger_corpus() {
    // Test streaming with a larger corpus to exercise backpressure
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");

    // Create larger corpus (50 sessions)
    make_test_corpus(&codex_home, 50);

    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Index with streaming mode
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CASS_STREAMING_INDEX", "1")
        .assert()
        .success();

    // Verify all sessions were indexed
    let messages = count_messages(&data_dir.join("agent_search.db"));
    let convs = count_conversations(&data_dir.join("agent_search.db"));

    // Each session has 2 messages (user + assistant)
    assert_eq!(
        messages, 100,
        "Expected 100 messages (50 sessions × 2), got {}",
        messages
    );
    assert_eq!(convs, 50, "Expected 50 conversations, got {}", convs);
}

// =============================================================================
// Incremental Indexing Tests
// =============================================================================

#[test]
fn test_streaming_incremental_indexing() {
    // Test that incremental indexing works with streaming mode
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");

    // Create initial corpus
    make_test_corpus(&codex_home, 5);

    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Initial full index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CASS_STREAMING_INDEX", "1")
        .assert()
        .success();

    let initial_count = count_conversations(&data_dir.join("agent_search.db"));
    assert_eq!(
        initial_count, 5,
        "Initial corpus should have 5 conversations"
    );

    // Add more sessions
    for i in 5..8 {
        let date_path = format!("2024/12/{:02}", i + 1);
        let filename = format!("rollout-{i}.jsonl");
        let content = format!("incremental test message {i}");
        let ts = 1733000000000 + (i as u64 * 1000);
        make_codex_session(&codex_home, &date_path, &filename, &content, ts);
    }

    // Incremental index (no --full flag)
    cargo_bin_cmd!("cass")
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CASS_STREAMING_INDEX", "1")
        .assert()
        .success();

    let final_count = count_conversations(&data_dir.join("agent_search.db"));
    assert_eq!(
        final_count, 8,
        "After incremental index should have 8 conversations, got {}",
        final_count
    );
}

// =============================================================================
// Empty Corpus Tests
// =============================================================================

#[test]
fn test_streaming_empty_corpus() {
    // Test streaming with no data to index
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    fs::create_dir_all(&codex_home).unwrap(); // Empty codex home

    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Index should succeed even with no data
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CASS_STREAMING_INDEX", "1")
        .assert()
        .success();

    // Should create DB with 0 conversations
    let convs = count_conversations(&data_dir.join("agent_search.db"));
    assert_eq!(convs, 0, "Empty corpus should have 0 conversations");
}

// =============================================================================
// Mixed Mode Tests (switch between modes)
// =============================================================================

#[test]
fn test_switch_from_batch_to_streaming() {
    // Index first with batch mode, then add data and reindex with streaming
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");

    // Create initial corpus
    make_test_corpus(&codex_home, 3);

    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Initial batch index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CASS_STREAMING_INDEX", "0")
        .assert()
        .success();

    let batch_count = count_conversations(&data_dir.join("agent_search.db"));
    assert_eq!(batch_count, 3);

    // Add more data
    for i in 3..6 {
        let date_path = format!("2024/12/{:02}", i + 1);
        let filename = format!("rollout-{i}.jsonl");
        let content = format!("new data for streaming {i}");
        let ts = 1733000000000 + (i as u64 * 1000);
        make_codex_session(&codex_home, &date_path, &filename, &content, ts);
    }

    // Reindex with streaming mode
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CASS_STREAMING_INDEX", "1")
        .assert()
        .success();

    let streaming_count = count_conversations(&data_dir.join("agent_search.db"));
    assert_eq!(
        streaming_count, 6,
        "Should have all 6 conversations after streaming reindex"
    );
}
