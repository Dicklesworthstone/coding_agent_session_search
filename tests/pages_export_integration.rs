//! Integration tests for the pages export pipeline.
//!
//! These tests create real SQLite databases with test data and verify
//! the export engine correctly filters, transforms, and exports data.

use chrono::{TimeZone, Utc};
use coding_agent_search::pages::export::{ExportEngine, ExportFilter, PathMode};
use frankensqlite::compat::{ConnectionExt, RowExt};
use frankensqlite::{Connection, Row as FrankenRow, params as fparams};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tempfile::TempDir;

type TestResult<T> = anyhow::Result<T>;

fn open_db(path: &Path) -> TestResult<Connection> {
    let path_str = path.to_string_lossy();
    Ok(Connection::open(path_str.as_ref())?)
}

fn query_i64(conn: &Connection, sql: &str) -> TestResult<i64> {
    Ok(conn.query_row_map(sql, &[], |row: &FrankenRow| row.get_typed(0))?)
}

fn query_string(conn: &Connection, sql: &str) -> TestResult<String> {
    Ok(conn.query_row_map(sql, &[], |row: &FrankenRow| row.get_typed(0))?)
}

fn query_strings(conn: &Connection, sql: &str) -> TestResult<Vec<String>> {
    Ok(conn.query_map_collect(sql, &[], |row: &FrankenRow| row.get_typed(0))?)
}

fn query_table_columns(conn: &Connection, table_name: &str) -> TestResult<Vec<String>> {
    let sql = format!("PRAGMA table_info({table_name})");
    Ok(conn.query_map_collect(&sql, &[], |row: &FrankenRow| row.get_typed(1))?)
}

fn query_message_pairs(conn: &Connection, sql: &str) -> TestResult<Vec<(i64, String)>> {
    Ok(conn.query_map_collect(sql, &[], |row: &FrankenRow| {
        Ok((row.get_typed(0)?, row.get_typed(1)?))
    })?)
}

/// Create a source database with the schema expected by the indexer.
fn create_source_db(conn: &Connection) -> TestResult<()> {
    Ok(conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS agents (
            id INTEGER PRIMARY KEY,
            slug TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL,
            kind TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS workspaces (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            display_name TEXT
        );

        CREATE TABLE IF NOT EXISTS conversations (
            id INTEGER PRIMARY KEY,
            agent_id INTEGER NOT NULL,
            workspace_id INTEGER,
            title TEXT,
            source_path TEXT NOT NULL,
            started_at INTEGER,
            ended_at INTEGER,
            message_count INTEGER,
            metadata_json TEXT,
            FOREIGN KEY (agent_id) REFERENCES agents(id),
            FOREIGN KEY (workspace_id) REFERENCES workspaces(id)
        );

        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY,
            conversation_id INTEGER NOT NULL,
            idx INTEGER NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at INTEGER,
            attachment_refs TEXT,
            FOREIGN KEY (conversation_id) REFERENCES conversations(id)
        );
        "#,
    )?)
}

/// Insert test data into the source database.
fn insert_test_data(conn: &Connection) -> TestResult<()> {
    // Insert agents
    conn.execute("INSERT INTO agents (id, slug, name, kind) VALUES (1, 'claude', 'Claude', 'ai')")?;
    conn.execute("INSERT INTO agents (id, slug, name, kind) VALUES (2, 'codex', 'Codex', 'ai')")?;
    conn.execute("INSERT INTO agents (id, slug, name, kind) VALUES (3, 'gemini', 'Gemini', 'ai')")?;

    // Insert workspaces
    conn.execute(
        "INSERT INTO workspaces (id, path, display_name) VALUES (1, '/home/user/project-a', 'Project A')"
    )?;
    conn.execute(
        "INSERT INTO workspaces (id, path, display_name) VALUES (2, '/home/user/project-b', 'Project B')"
    )?;

    // Insert conversations with different agents, workspaces, and timestamps
    let base_ts = Utc.with_ymd_and_hms(2024, 6, 15, 10, 0, 0).unwrap();

    // Conversation 1: claude, project-a, June 15
    conn.execute_compat(
        "INSERT INTO conversations (id, agent_id, workspace_id, title, source_path, started_at, ended_at, message_count)
         VALUES (1, 1, 1, 'Auth debugging', '/home/user/project-a/sessions/auth.jsonl', ?1, ?2, 3)",
        fparams![base_ts.timestamp_millis(), (base_ts + chrono::Duration::hours(1)).timestamp_millis()],
    )?;

    // Conversation 2: codex, project-a, June 16
    let ts2 = base_ts + chrono::Duration::days(1);
    conn.execute_compat(
        "INSERT INTO conversations (id, agent_id, workspace_id, title, source_path, started_at, ended_at, message_count)
         VALUES (2, 2, 1, 'API refactoring', '/home/user/project-a/sessions/api.jsonl', ?1, ?2, 2)",
        fparams![ts2.timestamp_millis(), (ts2 + chrono::Duration::hours(2)).timestamp_millis()],
    )?;

    // Conversation 3: claude, project-b, June 17
    let ts3 = base_ts + chrono::Duration::days(2);
    conn.execute_compat(
        "INSERT INTO conversations (id, agent_id, workspace_id, title, source_path, started_at, ended_at, message_count)
         VALUES (3, 1, 2, 'UI design', '/home/user/project-b/sessions/ui.jsonl', ?1, ?2, 4)",
        fparams![ts3.timestamp_millis(), (ts3 + chrono::Duration::hours(3)).timestamp_millis()],
    )?;

    // Conversation 4: gemini, project-b, June 18
    let ts4 = base_ts + chrono::Duration::days(3);
    conn.execute_compat(
        "INSERT INTO conversations (id, agent_id, workspace_id, title, source_path, started_at, ended_at, message_count)
         VALUES (4, 3, 2, 'Database optimization', '/home/user/project-b/sessions/db.jsonl', ?1, ?2, 5)",
        fparams![ts4.timestamp_millis(), (ts4 + chrono::Duration::hours(1)).timestamp_millis()],
    )?;

    // Insert messages for each conversation
    let messages = vec![
        // Conv 1 messages
        (1, 0, "user", "Help me debug the auth flow"),
        (
            1,
            1,
            "assistant",
            "I'll help analyze the authentication code",
        ),
        (1, 2, "user", "The token is expiring too fast"),
        // Conv 2 messages
        (2, 0, "user", "Refactor the API endpoints"),
        (2, 1, "assistant", "Let me review the current structure"),
        // Conv 3 messages
        (3, 0, "user", "Design a new dashboard"),
        (3, 1, "assistant", "I'll create a mockup"),
        (3, 2, "user", "Add dark mode support"),
        (3, 3, "assistant", "Implementing dark mode theme"),
        // Conv 4 messages
        (4, 0, "user", "Optimize the queries"),
        (4, 1, "assistant", "Analyzing query performance"),
        (4, 2, "user", "Add indexes"),
        (4, 3, "assistant", "Creating optimized indexes"),
        (4, 4, "user", "Test the changes"),
    ];

    for (conv_id, idx, role, content) in messages {
        conn.execute_compat(
            "INSERT INTO messages (conversation_id, idx, role, content, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            fparams![
                conv_id as i64,
                idx as i64,
                role,
                content,
                base_ts.timestamp_millis() + (idx as i64 * 60000)
            ],
        )?;
    }

    Ok(())
}

/// Verify exported database has correct schema.
fn verify_export_schema(conn: &Connection) -> TestResult<()> {
    // Check conversations table exists and has expected columns
    let _: i64 = query_i64(conn, "SELECT COUNT(*) FROM conversations")?;

    // Check messages table
    let _: i64 = query_i64(conn, "SELECT COUNT(*) FROM messages")?;

    // Check FTS tables are present in schema
    let fts_exists = query_i64(
        conn,
        "SELECT COUNT(*) FROM sqlite_master WHERE name = 'messages_fts'",
    )?;
    let code_fts_exists = query_i64(
        conn,
        "SELECT COUNT(*) FROM sqlite_master WHERE name = 'messages_code_fts'",
    )?;
    assert_eq!(fts_exists, 1);
    assert_eq!(code_fts_exists, 1);

    // Check export_meta
    let schema_version = query_string(
        conn,
        "SELECT value FROM export_meta WHERE key = 'schema_version'",
    )?;
    assert_eq!(schema_version, "1");

    let message_columns = query_table_columns(conn, "messages")?;
    assert!(message_columns.contains(&"updated_at".to_string()));
    assert!(message_columns.contains(&"model".to_string()));
    assert!(message_columns.contains(&"attachment_refs".to_string()));

    Ok(())
}

// =============================================================================
// Basic Export Tests
// =============================================================================

#[test]
fn export_engine_exports_all_conversations_with_no_filter() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    // Create and populate source DB
    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    // Export with no filter
    let filter = ExportFilter {
        agents: None,
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Full,
    };

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    let stats = engine.execute(|_, _| {}, None).unwrap();

    // Should export all 4 conversations and 14 messages
    assert_eq!(stats.conversations_processed, 4);
    assert_eq!(stats.messages_processed, 14);

    // Verify exported database
    let out_conn = open_db(&output_path).unwrap();
    verify_export_schema(&out_conn).unwrap();

    let conv_count = query_i64(&out_conn, "SELECT COUNT(*) FROM conversations").unwrap();
    assert_eq!(conv_count, 4);

    let msg_count = query_i64(&out_conn, "SELECT COUNT(*) FROM messages").unwrap();
    assert_eq!(msg_count, 14);
}

#[test]
fn export_engine_filters_by_single_agent() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    // Filter to only claude conversations
    let filter = ExportFilter {
        agents: Some(vec!["claude".to_string()]),
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Full,
    };

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    let stats = engine.execute(|_, _| {}, None).unwrap();

    // Claude has conversations 1 and 3 (3 + 4 = 7 messages)
    assert_eq!(stats.conversations_processed, 2);
    assert_eq!(stats.messages_processed, 7);

    let out_conn = open_db(&output_path).unwrap();
    let agents = query_strings(&out_conn, "SELECT DISTINCT agent FROM conversations").unwrap();
    assert_eq!(agents, vec!["claude"]);
}

#[test]
fn export_engine_filters_by_multiple_agents() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    // Filter to claude and codex
    let filter = ExportFilter {
        agents: Some(vec!["claude".to_string(), "codex".to_string()]),
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Full,
    };

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    let stats = engine.execute(|_, _| {}, None).unwrap();

    // Claude (2 convs, 7 msgs) + Codex (1 conv, 2 msgs) = 3 convs, 9 msgs
    assert_eq!(stats.conversations_processed, 3);
    assert_eq!(stats.messages_processed, 9);
}

#[test]
fn export_engine_filters_by_workspace() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    // Filter to project-a only
    let filter = ExportFilter {
        agents: None,
        workspaces: Some(vec![PathBuf::from("/home/user/project-a")]),
        since: None,
        until: None,
        path_mode: PathMode::Full,
    };

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    let stats = engine.execute(|_, _| {}, None).unwrap();

    // project-a has conversations 1 and 2 (3 + 2 = 5 messages)
    assert_eq!(stats.conversations_processed, 2);
    assert_eq!(stats.messages_processed, 5);

    let out_conn = open_db(&output_path).unwrap();
    let workspaces =
        query_strings(&out_conn, "SELECT DISTINCT workspace FROM conversations").unwrap();
    assert_eq!(workspaces, vec!["/home/user/project-a"]);
}

#[test]
fn export_engine_filters_by_time_range() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    // Filter to June 16-17 only (conversations 2 and 3)
    let since = Utc.with_ymd_and_hms(2024, 6, 16, 0, 0, 0).unwrap();
    let until = Utc.with_ymd_and_hms(2024, 6, 17, 23, 59, 59).unwrap();

    let filter = ExportFilter {
        agents: None,
        workspaces: None,
        since: Some(since),
        until: Some(until),
        path_mode: PathMode::Full,
    };

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    let stats = engine.execute(|_, _| {}, None).unwrap();

    // Conversations 2 (2 msgs) and 3 (4 msgs) = 2 convs, 6 msgs
    assert_eq!(stats.conversations_processed, 2);
    assert_eq!(stats.messages_processed, 6);
}

#[test]
fn export_engine_combined_filters() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    // Filter: claude only, project-b workspace
    let filter = ExportFilter {
        agents: Some(vec!["claude".to_string()]),
        workspaces: Some(vec![PathBuf::from("/home/user/project-b")]),
        since: None,
        until: None,
        path_mode: PathMode::Full,
    };

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    let stats = engine.execute(|_, _| {}, None).unwrap();

    // Only conversation 3 matches (claude + project-b)
    assert_eq!(stats.conversations_processed, 1);
    assert_eq!(stats.messages_processed, 4);
}

// =============================================================================
// Path Transformation Tests
// =============================================================================

#[test]
fn export_engine_transforms_paths_with_full_mode() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    let filter = ExportFilter {
        agents: Some(vec!["claude".to_string()]),
        workspaces: Some(vec![PathBuf::from("/home/user/project-a")]),
        since: None,
        until: None,
        path_mode: PathMode::Full,
    };

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    engine.execute(|_, _| {}, None).unwrap();

    let out_conn = open_db(&output_path).unwrap();
    let path = query_string(&out_conn, "SELECT source_path FROM conversations LIMIT 1").unwrap();

    // Full mode preserves the complete path
    assert_eq!(path, "/home/user/project-a/sessions/auth.jsonl");
}

#[test]
fn export_engine_transforms_paths_with_basename_mode() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    let filter = ExportFilter {
        agents: Some(vec!["claude".to_string()]),
        workspaces: Some(vec![PathBuf::from("/home/user/project-a")]),
        since: None,
        until: None,
        path_mode: PathMode::Basename,
    };

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    engine.execute(|_, _| {}, None).unwrap();

    let out_conn = open_db(&output_path).unwrap();
    let path = query_string(&out_conn, "SELECT source_path FROM conversations LIMIT 1").unwrap();

    // Basename mode extracts just the filename
    assert_eq!(path, "auth.jsonl");
}

#[test]
fn export_engine_transforms_paths_with_relative_mode() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    let filter = ExportFilter {
        agents: Some(vec!["claude".to_string()]),
        workspaces: Some(vec![PathBuf::from("/home/user/project-a")]),
        since: None,
        until: None,
        path_mode: PathMode::Relative,
    };

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    engine.execute(|_, _| {}, None).unwrap();

    let out_conn = open_db(&output_path).unwrap();
    let path = query_string(&out_conn, "SELECT source_path FROM conversations LIMIT 1").unwrap();

    // Relative mode strips workspace prefix
    assert_eq!(path, "sessions/auth.jsonl");
}

#[test]
fn export_engine_transforms_paths_with_hash_mode() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    let filter = ExportFilter {
        agents: Some(vec!["claude".to_string()]),
        workspaces: Some(vec![PathBuf::from("/home/user/project-a")]),
        since: None,
        until: None,
        path_mode: PathMode::Hash,
    };

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    engine.execute(|_, _| {}, None).unwrap();

    let out_conn = open_db(&output_path).unwrap();
    let path = query_string(&out_conn, "SELECT source_path FROM conversations LIMIT 1").unwrap();

    // Hash mode produces 16 hex characters
    assert_eq!(path.len(), 16);
    assert!(path.chars().all(|c| c.is_ascii_hexdigit()));
}

// =============================================================================
// Edge Case Tests
// =============================================================================

#[test]
fn export_engine_handles_empty_filter_results() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    // Filter to non-existent agent
    let filter = ExportFilter {
        agents: Some(vec!["nonexistent".to_string()]),
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Full,
    };

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    let stats = engine.execute(|_, _| {}, None).unwrap();

    assert_eq!(stats.conversations_processed, 0);
    assert_eq!(stats.messages_processed, 0);

    // Output DB should still be valid
    let out_conn = open_db(&output_path).unwrap();
    verify_export_schema(&out_conn).unwrap();
}

#[test]
fn export_engine_handles_empty_agents_list() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    // Empty agents list should match nothing
    let filter = ExportFilter {
        agents: Some(vec![]),
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Full,
    };

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    let stats = engine.execute(|_, _| {}, None).unwrap();

    assert_eq!(stats.conversations_processed, 0);
    assert_eq!(stats.messages_processed, 0);
}

#[test]
fn export_engine_cancellation_via_running_flag() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    let filter = ExportFilter {
        agents: None,
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Full,
    };

    // Set running flag to false immediately
    let running = Arc::new(AtomicBool::new(false));

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    let result = engine.execute(|_, _| {}, Some(running));

    // Should return cancellation error
    assert!(result.is_err());
    let err = result.err().unwrap();
    assert!(err.to_string().contains("cancelled"));
}

#[test]
fn export_engine_rejects_same_source_and_output() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("source.db");

    let src_conn = open_db(&db_path).unwrap();
    create_source_db(&src_conn).unwrap();
    drop(src_conn);

    let filter = ExportFilter {
        agents: None,
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Full,
    };

    // Same path for source and output
    let engine = ExportEngine::new(&db_path, &db_path, filter);
    let result = engine.execute(|_, _| {}, None);

    assert!(result.is_err());
    let err = result.err().unwrap();
    assert!(err.to_string().contains("different"));
}

#[test]
fn export_engine_rejects_output_directory() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    drop(src_conn);

    let filter = ExportFilter {
        agents: None,
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Full,
    };

    // Output path is a directory
    let engine = ExportEngine::new(&source_path, tmp.path(), filter);
    let result = engine.execute(|_, _| {}, None);

    assert!(result.is_err());
    let err = result.err().unwrap();
    assert!(err.to_string().contains("directory"));
}

#[test]
fn export_engine_preserves_existing_output_on_cancelled_rerun() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    let filter = ExportFilter {
        agents: None,
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Full,
    };

    let engine = ExportEngine::new(&source_path, &output_path, filter.clone());
    let stats = engine.execute(|_, _| {}, None).unwrap();
    assert_eq!(stats.conversations_processed, 4);
    assert_eq!(stats.messages_processed, 14);

    let original_size = std::fs::metadata(&output_path).unwrap().len();
    assert!(
        original_size > 0,
        "initial export should create a non-empty database"
    );

    let cancelled = Arc::new(AtomicBool::new(false));
    let rerun = ExportEngine::new(&source_path, &output_path, filter);
    let err = rerun.execute(|_, _| {}, Some(cancelled)).err().unwrap();
    assert!(
        err.to_string().contains("cancelled"),
        "expected cancellation error, got: {err}"
    );

    let preserved_size = std::fs::metadata(&output_path).unwrap().len();
    assert_eq!(
        preserved_size, original_size,
        "cancelled rerun should preserve the previous export file"
    );

    let preserved_conn = open_db(&output_path).unwrap();
    let schema_version = query_string(
        &preserved_conn,
        "SELECT value FROM export_meta WHERE key = 'schema_version'",
    )
    .unwrap();
    assert_eq!(schema_version, "1");
    let conv_count = query_i64(&preserved_conn, "SELECT COUNT(*) FROM conversations").unwrap();
    let msg_count = query_i64(&preserved_conn, "SELECT COUNT(*) FROM messages").unwrap();
    assert_eq!(conv_count, 4);
    assert_eq!(msg_count, 14);
}

// =============================================================================
// FTS Verification Tests
// =============================================================================

#[test]
fn export_engine_populates_fts_indexes() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    let filter = ExportFilter {
        agents: None,
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Full,
    };

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    engine.execute(|_, _| {}, None).unwrap();

    let out_conn = open_db(&output_path).unwrap();

    let messages_count = query_i64(&out_conn, "SELECT COUNT(*) FROM messages").unwrap();
    assert!(messages_count > 0, "Export should contain indexed messages");

    let fts_exists = query_i64(
        &out_conn,
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'messages_fts'",
    )
    .unwrap();
    let code_fts_exists = query_i64(
        &out_conn,
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'messages_code_fts'",
    )
    .unwrap();
    assert_eq!(fts_exists, 1, "Export should create prose FTS index");
    assert_eq!(code_fts_exists, 1, "Export should create code FTS index");

    let fts_sql = query_string(
        &out_conn,
        "SELECT sql FROM sqlite_master WHERE name = 'messages_fts'",
    )
    .unwrap();
    assert!(
        fts_sql.contains("fts5"),
        "messages_fts should be an FTS5 virtual table"
    );
}

#[test]
fn export_engine_preserves_message_order() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    let filter = ExportFilter {
        agents: Some(vec!["claude".to_string()]),
        workspaces: Some(vec![PathBuf::from("/home/user/project-a")]),
        since: None,
        until: None,
        path_mode: PathMode::Full,
    };

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    engine.execute(|_, _| {}, None).unwrap();

    let out_conn = open_db(&output_path).unwrap();

    // Get messages in idx order
    let messages = query_message_pairs(
        &out_conn,
        "SELECT idx, content FROM messages WHERE conversation_id = 1 ORDER BY idx",
    )
    .unwrap();

    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].0, 0);
    assert!(messages[0].1.contains("debug"));
    assert_eq!(messages[1].0, 1);
    assert_eq!(messages[2].0, 2);
}

// =============================================================================
// Progress Callback Tests
// =============================================================================

#[test]
fn export_engine_calls_progress_callback() {
    let tmp = TempDir::new().unwrap();
    let source_path = tmp.path().join("source.db");
    let output_path = tmp.path().join("export.db");

    let src_conn = open_db(&source_path).unwrap();
    create_source_db(&src_conn).unwrap();
    insert_test_data(&src_conn).unwrap();
    drop(src_conn);

    let filter = ExportFilter {
        agents: None,
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Full,
    };

    let progress_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let progress_clone = progress_calls.clone();

    let engine = ExportEngine::new(&source_path, &output_path, filter);
    engine
        .execute(
            move |current, total| {
                progress_clone.lock().unwrap().push((current, total));
            },
            None,
        )
        .unwrap();

    let calls = progress_calls.lock().unwrap();
    assert!(!calls.is_empty(), "Progress callback should be called");

    // Last call should have current == total
    let last = calls.last().unwrap();
    assert_eq!(last.0, last.1);
    assert_eq!(last.1, 4); // 4 total conversations
}
