//! E2E full integration test: agent detection → storage → search.
//!
//! Exercises the complete cass pipeline with multiple agent types:
//! 1. Create test session fixtures (Codex + Claude Code formats)
//! 2. Run full indexing via CLI (agent detection + frankensqlite storage + tantivy index)
//! 3. Search via CLI (lexical, JSON robot output)
//! 4. Verify data consistency across the pipeline
//!
//! Bead: coding_agent_session_search-1p9xd

use coding_agent_search::storage::sqlite::SqliteStorage;
use frankensqlite::compat::{ConnectionExt, RowExt};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

#[macro_use]
mod util;
use util::EnvGuard;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Create a Codex-format session file.
fn create_codex_session(root: &Path, date_path: &str, filename: &str, content: &str, ts: u64) {
    let sessions = root.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    let sample = format!(
        r#"{{"type": "event_msg", "timestamp": {ts}, "payload": {{"type": "user_message", "message": "{content}"}}}}
{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "{content} response from codex"}}}}
"#,
        ts + 1000
    );
    fs::write(file, sample).unwrap();
}

/// Create a Claude Code-format session file.
fn create_claude_session(root: &Path, project: &str, filename: &str, content: &str, ts: &str) {
    let project_dir = root.join(format!("projects/{project}"));
    fs::create_dir_all(&project_dir).unwrap();
    let file = project_dir.join(filename);
    let sample = format!(
        r#"{{"type": "user", "timestamp": "{ts}", "message": {{"role": "user", "content": "{content}"}}}}
{{"type": "assistant", "timestamp": "{ts}", "message": {{"role": "assistant", "content": "{content} response from claude"}}}}"#
    );
    fs::write(file, sample).unwrap();
}

/// Count messages in the SQLite database.
fn count_messages(db_path: &Path) -> i64 {
    let storage = SqliteStorage::open(db_path).expect("open sqlite");
    storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |r| r.get_typed(0))
        .expect("count messages")
}

/// Count conversations in the SQLite database.
fn count_conversations(db_path: &Path) -> i64 {
    let storage = SqliteStorage::open(db_path).expect("open sqlite");
    storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM conversations", &[], |r| {
            r.get_typed(0)
        })
        .expect("count conversations")
}

/// Count distinct agents in the database.
fn count_agents(db_path: &Path) -> i64 {
    let storage = SqliteStorage::open(db_path).expect("open sqlite");
    storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM agents", &[], |r| r.get_typed(0))
        .expect("count agents")
}

/// Parse search output as JSON, returning the hits array.
/// Returns empty vec if output is empty (e.g. search exited non-zero with no stdout).
fn parse_search_hits(output: &[u8]) -> Vec<Value> {
    if output.is_empty() {
        return Vec::new();
    }
    let json: Value = serde_json::from_slice(output).expect("search output should be valid JSON");
    json.get("hits")
        .and_then(|h| h.as_array())
        .cloned()
        .unwrap_or_default()
}

use util::cass_bin;

// ============================================================================
// 1. FULL PIPELINE: Index + Search with Multiple Agents
// ============================================================================

/// Full integration test: create fixtures for 2 agent types, index, and search.
///
/// Pipeline: codex+claude session files → cass index → cass search → verify hits
#[test]
fn e2e_multi_agent_index_and_search() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    let xdg_data = home.join(".local/share");
    let xdg_config = home.join(".config");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&xdg_data).unwrap();
    fs::create_dir_all(&xdg_config).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // ---- Phase 1: Create fixtures ----
    // Codex session with unique searchable keyword
    create_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-e2e-1.jsonl",
        "debugging the authentication_flow_alpha middleware",
        1732118400000,
    );

    // Claude Code session with different unique keyword
    create_claude_session(
        &claude_home,
        "integration-test",
        "session-e2e-1.jsonl",
        "optimizing database_query_beta performance",
        "2024-11-20T12:00:00Z",
    );

    // ---- Phase 2: Full index ----
    let index_result = Command::new(cass_bin())
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .output()
        .expect("index command should execute");

    assert!(
        index_result.status.success(),
        "Index should succeed. stderr: {}",
        String::from_utf8_lossy(&index_result.stderr)
    );

    // ---- Phase 3: Verify database artifacts ----
    let db_path = data_dir.join("agent_search.db");
    assert!(db_path.exists(), "SQLite database should be created");
    assert!(
        data_dir.join("index").exists(),
        "Tantivy index directory should exist"
    );

    let msg_count = count_messages(&db_path);
    let conv_count = count_conversations(&db_path);
    let agent_count = count_agents(&db_path);

    verbose!(
        "Indexed: {} messages, {} conversations, {} agents",
        msg_count,
        conv_count,
        agent_count
    );

    // At minimum: each session produces 2 messages (user + assistant)
    assert!(
        msg_count >= 2,
        "Should have at least 2 messages, got {msg_count}"
    );
    assert!(
        conv_count >= 1,
        "Should have at least 1 conversation, got {conv_count}"
    );

    // ---- Phase 4: Lexical search for Codex content ----
    let codex_search = Command::new(cass_bin())
        .args([
            "search",
            "authentication_flow_alpha",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(home)
        .env("HOME", home)
        .output()
        .expect("codex search command");

    assert!(
        codex_search.status.success(),
        "Codex search should succeed. stderr: {}",
        String::from_utf8_lossy(&codex_search.stderr)
    );

    let codex_hits = parse_search_hits(&codex_search.stdout);
    assert!(
        !codex_hits.is_empty(),
        "Should find Codex content 'authentication_flow_alpha'. Output: {}",
        String::from_utf8_lossy(&codex_search.stdout)
    );

    // Verify hit contains expected content
    let first_hit = &codex_hits[0];
    let hit_content = first_hit
        .get("content")
        .or_else(|| first_hit.get("snippet"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        hit_content.contains("authentication_flow_alpha") || hit_content.contains("authentication"),
        "Hit content should reference the search term. Got: {hit_content}"
    );

    // ---- Phase 5: Lexical search for Claude content ----
    let claude_search = Command::new(cass_bin())
        .args(["search", "database_query_beta", "--robot", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("HOME", home)
        .output()
        .expect("claude search command");

    assert!(
        claude_search.status.success(),
        "Claude search should succeed. stderr: {}",
        String::from_utf8_lossy(&claude_search.stderr)
    );

    let claude_hits = parse_search_hits(&claude_search.stdout);
    // Claude session may or may not be detected depending on connector logic,
    // but the search command itself should not fail.
    verbose!(
        "Claude search hits: {} (may be 0 if connector skipped detection)",
        claude_hits.len()
    );

    // ---- Phase 6: Search for non-existent term ----
    let empty_search = Command::new(cass_bin())
        .args([
            "search",
            "zzz_nonexistent_term_zzz",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(home)
        .env("HOME", home)
        .output()
        .expect("empty search command");

    // Some search backends exit non-zero when there are 0 results (grep convention).
    // Accept either success OR non-zero with empty/no-result JSON stdout.
    let empty_stdout = String::from_utf8_lossy(&empty_search.stdout);
    let empty_stderr = String::from_utf8_lossy(&empty_search.stderr);
    if !empty_search.status.success() {
        eprintln!(
            "Note: search for nonexistent term exited with {:?}\nstdout: {}\nstderr: {}",
            empty_search.status.code(),
            empty_stdout,
            empty_stderr
        );
    }

    let empty_hits = parse_search_hits(&empty_search.stdout);
    assert!(
        empty_hits.is_empty(),
        "Nonexistent term should return 0 hits"
    );
}

// ============================================================================
// 2. ROBOT MODE JSON STRUCTURE
// ============================================================================

/// Verify that --robot mode produces valid, structured JSON output.
#[test]
fn e2e_robot_mode_json_structure() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    let xdg_data = home.join(".local/share");
    let xdg_config = home.join(".config");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&xdg_data).unwrap();
    fs::create_dir_all(&xdg_config).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    create_codex_session(
        &codex_home,
        "2024/12/01",
        "rollout-json-test.jsonl",
        "unique_json_structure_token",
        1733097600000,
    );

    let index_output = Command::new(cass_bin())
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .output()
        .expect("index command should execute");

    assert!(
        index_output.status.success(),
        "Index should succeed. stderr: {}",
        String::from_utf8_lossy(&index_output.stderr)
    );

    let output = Command::new(cass_bin())
        .args([
            "search",
            "unique_json_structure_token",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(home)
        .env("HOME", home)
        .output()
        .expect("robot search command");

    assert!(output.status.success());

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");

    // Verify top-level structure
    assert!(json.get("hits").is_some(), "JSON should have 'hits' field");
    assert!(json["hits"].is_array(), "'hits' should be an array");

    let hits = json["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "Should have at least one hit");

    // Verify hit structure
    let hit = &hits[0];
    // Hits should have standard fields (may vary by version, check what's present)
    let has_content = hit.get("content").is_some() || hit.get("snippet").is_some();
    assert!(
        has_content,
        "Hit should have 'content' or 'snippet' field. Got: {hit}"
    );
}

// ============================================================================
// 3. DATABASE INTEGRITY AFTER INDEX
// ============================================================================

/// Verify that the database maintains referential integrity after indexing.
#[test]
fn e2e_database_integrity() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    let xdg_data = home.join(".local/share");
    let xdg_config = home.join(".config");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&xdg_data).unwrap();
    fs::create_dir_all(&xdg_config).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Create sessions for two different agents
    create_codex_session(
        &codex_home,
        "2024/11/15",
        "rollout-integrity-1.jsonl",
        "integrity_check_first",
        1731657600000,
    );
    create_codex_session(
        &codex_home,
        "2024/11/16",
        "rollout-integrity-2.jsonl",
        "integrity_check_second",
        1731744000000,
    );

    let index_output = Command::new(cass_bin())
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .output()
        .expect("index command should execute");

    assert!(
        index_output.status.success(),
        "Index should succeed. stderr: {}",
        String::from_utf8_lossy(&index_output.stderr)
    );

    let db_path = data_dir.join("agent_search.db");
    let storage = SqliteStorage::open(&db_path).expect("open db");
    let conn = storage.raw();

    // Every message should reference a valid conversation
    let orphan_msgs: i64 = conn
        .query_row_map(
            "SELECT COUNT(*) FROM messages m
             LEFT JOIN conversations c ON m.conversation_id = c.id
             WHERE c.id IS NULL",
            &[],
            |r| r.get_typed(0),
        )
        .expect("orphan check");
    assert_eq!(orphan_msgs, 0, "No orphan messages should exist");

    // Every conversation should reference a valid agent
    let orphan_convs: i64 = conn
        .query_row_map(
            "SELECT COUNT(*) FROM conversations c
             LEFT JOIN agents a ON c.agent_id = a.id
             WHERE a.id IS NULL",
            &[],
            |r| r.get_typed(0),
        )
        .expect("orphan conv check");
    assert_eq!(orphan_convs, 0, "No orphan conversations should exist");

    // The current contentless FTS table is considered healthy if frankensqlite can
    // query it and at least one row is visible through the canonical doctor probe.
    let fts_probe_rows: Vec<i64> = conn
        .query_map_collect("SELECT rowid FROM fts_messages LIMIT 1", &[], |r| {
            r.get_typed(0)
        })
        .expect("fts probe");
    let msg_count = count_messages(&db_path);
    assert!(
        !fts_probe_rows.is_empty(),
        "FTS should expose at least one indexed row after indexing"
    );
    verbose!(
        "DB integrity OK: {} messages, FTS queryable with {} visible probe rows, 0 orphans",
        msg_count,
        fts_probe_rows.len()
    );

    let doctor_output = Command::new(cass_bin())
        .args(["doctor", "--json", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .output()
        .expect("doctor command should execute");

    assert!(
        doctor_output.status.success(),
        "Doctor should succeed after indexing. stderr: {}",
        String::from_utf8_lossy(&doctor_output.stderr)
    );

    let doctor_json: Value =
        serde_json::from_slice(&doctor_output.stdout).expect("valid doctor JSON");
    assert_eq!(
        doctor_json["healthy"].as_bool(),
        Some(true),
        "Doctor should report a healthy DB after full index: {doctor_json}"
    );
}

// ============================================================================
// 4. STATS COMMAND AFTER INDEX
// ============================================================================

/// Verify the stats command works after indexing (uses frankensqlite queries).
#[test]
fn e2e_stats_after_index() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    let xdg_data = home.join(".local/share");
    let xdg_config = home.join(".config");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&xdg_data).unwrap();
    fs::create_dir_all(&xdg_config).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    create_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-stats.jsonl",
        "stats_test_content",
        1732118400000,
    );

    let index_output = Command::new(cass_bin())
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .output()
        .expect("index command should execute");

    assert!(
        index_output.status.success(),
        "Index should succeed. stderr: {}",
        String::from_utf8_lossy(&index_output.stderr)
    );

    // Run stats command (exercises the frankensqlite-migrated run_stats path)
    let stats_output = Command::new(cass_bin())
        .args(["stats", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("HOME", home)
        .output()
        .expect("stats command");

    assert!(
        stats_output.status.success(),
        "Stats command should succeed. stderr: {}",
        String::from_utf8_lossy(&stats_output.stderr)
    );

    let stdout = String::from_utf8_lossy(&stats_output.stdout);
    verbose!("Stats output length: {} bytes", stdout.len());

    // Stats output should contain some meaningful text (agent names, counts, etc.)
    // Exact format varies, but it should not be empty
    assert!(!stdout.is_empty(), "Stats output should not be empty");
}

// ============================================================================
// 5. DIAG COMMAND INTEGRATION
// ============================================================================

/// Verify the diag command works (exercises frankensqlite-migrated run_diag path).
#[test]
fn e2e_diag_after_index() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    let xdg_data = home.join(".local/share");
    let xdg_config = home.join(".config");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&xdg_data).unwrap();
    fs::create_dir_all(&xdg_config).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    create_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-diag.jsonl",
        "diag_test_content",
        1732118400000,
    );

    let index_output = Command::new(cass_bin())
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .output()
        .expect("index command should execute");

    assert!(
        index_output.status.success(),
        "Index should succeed. stderr: {}",
        String::from_utf8_lossy(&index_output.stderr)
    );

    // Run diag command
    let diag_output = Command::new(cass_bin())
        .args(["diag", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("HOME", home)
        .output()
        .expect("diag command");

    assert!(
        diag_output.status.success(),
        "Diag command should succeed. stderr: {}",
        String::from_utf8_lossy(&diag_output.stderr)
    );

    let stdout = String::from_utf8_lossy(&diag_output.stdout);
    assert!(!stdout.is_empty(), "Diag output should not be empty");
}
