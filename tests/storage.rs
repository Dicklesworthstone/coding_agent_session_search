use std::path::PathBuf;

use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::sources::provenance::{LOCAL_SOURCE_ID, Source, SourceKind};
use coding_agent_search::storage::sqlite::SqliteStorage;

fn sample_agent() -> Agent {
    Agent {
        id: None,
        slug: "tester".into(),
        name: "Tester".into(),
        version: Some("1.0".into()),
        kind: AgentKind::Cli,
    }
}

fn sample_conv(external_id: Option<&str>, messages: Vec<Message>) -> Conversation {
    Conversation {
        id: None,
        agent_slug: "tester".into(),
        workspace: Some(PathBuf::from("/workspace/demo")),
        external_id: external_id.map(std::borrow::ToOwned::to_owned),
        title: Some("Demo conversation".into()),
        source_path: PathBuf::from("/logs/demo.jsonl"),
        started_at: Some(1),
        ended_at: Some(2),
        approx_tokens: Some(42),
        metadata_json: serde_json::json!({"k": "v"}),
        messages,
        source_id: "local".to_string(),
        origin_host: None,
    }
}

fn msg(idx: i64, created_at: i64) -> Message {
    Message {
        id: None,
        idx,
        role: MessageRole::User,
        author: Some("user".into()),
        created_at: Some(created_at),
        content: format!("msg-{idx}"),
        extra_json: serde_json::json!({}),
        snippets: vec![],
    }
}

#[test]
fn schema_version_created_on_open() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("store.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    assert_eq!(storage.schema_version().unwrap(), 5);

    // If meta row is removed, the getter surfaces an error.
    storage.raw().execute("DELETE FROM meta", []).unwrap();
    assert!(storage.schema_version().is_err());
}

#[test]
fn rebuild_fts_repopulates_rows() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("fts.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(PathBuf::from("/workspace/demo").as_path(), Some("Demo"))
        .unwrap();

    let conv = sample_conv(Some("ext-1"), vec![msg(0, 10), msg(1, 20)]);
    storage
        .insert_conversation_tree(agent_id, Some(ws_id), &conv)
        .unwrap();

    let count_messages: i64 = storage
        .raw()
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    let mut fts_count: i64 = storage
        .raw()
        .query_row("SELECT COUNT(*) FROM fts_messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fts_count, count_messages);

    storage
        .raw()
        .execute("DELETE FROM fts_messages", [])
        .unwrap();
    fts_count = storage
        .raw()
        .query_row("SELECT COUNT(*) FROM fts_messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fts_count, 0);

    storage.rebuild_fts().unwrap();
    fts_count = storage
        .raw()
        .query_row("SELECT COUNT(*) FROM fts_messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fts_count, count_messages);
}

#[test]
fn transaction_rolls_back_on_duplicate_idx() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("rollback.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();

    // Duplicate idx inside the same conversation should trigger UNIQUE constraint
    // and leave the database unchanged after rollback.
    let conv = sample_conv(None, vec![msg(0, 1), msg(0, 2)]);
    let result = storage.insert_conversation_tree(agent_id, None, &conv);
    assert!(result.is_err());

    let conv_count: i64 = storage
        .raw()
        .query_row("SELECT COUNT(*) FROM conversations", [], |c| c.get(0))
        .unwrap();
    let msg_count: i64 = storage
        .raw()
        .query_row("SELECT COUNT(*) FROM messages", [], |c| c.get(0))
        .unwrap();

    assert_eq!(conv_count, 0);
    assert_eq!(msg_count, 0);
}

#[test]
fn append_only_updates_existing_conversation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("append.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();

    let first = sample_conv(Some("ext-2"), vec![msg(0, 100), msg(1, 200)]);
    let outcome1 = storage
        .insert_conversation_tree(agent_id, None, &first)
        .unwrap();
    assert_eq!(outcome1.inserted_indices, vec![0, 1]);

    let second = sample_conv(Some("ext-2"), vec![msg(0, 100), msg(1, 200), msg(2, 300)]);
    let outcome2 = storage
        .insert_conversation_tree(agent_id, None, &second)
        .unwrap();
    assert_eq!(outcome2.conversation_id, outcome1.conversation_id);
    assert_eq!(outcome2.inserted_indices, vec![2]);

    let rows: Vec<(i64, i64)> = storage
        .raw()
        .prepare("SELECT idx, created_at FROM messages ORDER BY idx")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap()))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(rows, vec![(0, 100), (1, 200), (2, 300)]);

    let ended_at: i64 = storage
        .raw()
        .query_row(
            "SELECT ended_at FROM conversations WHERE id = ?",
            [outcome1.conversation_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ended_at, 300);
}

#[test]
fn large_batch_insert_keeps_fts_in_sync() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("batch.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();

    // Build a conversation with 200 messages
    let mut msgs = Vec::new();
    for idx in 0..200 {
        msgs.push(msg(idx, 1_000 + idx));
    }
    let conv = sample_conv(Some("batch-1"), msgs);

    let outcome = storage
        .insert_conversation_tree(agent_id, None, &conv)
        .expect("batch insert");
    assert_eq!(outcome.inserted_indices.len(), 200);

    let msg_count: i64 = storage
        .raw()
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    let fts_count: i64 = storage
        .raw()
        .query_row("SELECT COUNT(*) FROM fts_messages", [], |r| r.get(0))
        .unwrap();

    assert_eq!(msg_count, 200);
    assert_eq!(fts_count, 200);

    // Spot check a few message rows for correct ordering and timestamps
    let rows: Vec<(i64, i64)> = storage
        .raw()
        .prepare("SELECT idx, created_at FROM messages ORDER BY idx LIMIT 3 OFFSET 197")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap()))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        rows,
        vec![(197, 1_197), (198, 1_198), (199, 1_199)],
        "tail rows should preserve order and timestamps"
    );
}

#[test]
fn last_scan_ts_roundtrip() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("scan.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open");

    // Initially None
    assert_eq!(storage.get_last_scan_ts().unwrap(), None);

    storage.set_last_scan_ts(1234).expect("set ts");
    assert_eq!(storage.get_last_scan_ts().unwrap(), Some(1234));

    // Reopen and ensure persisted
    drop(storage);
    let storage2 = SqliteStorage::open(&db_path).expect("reopen");
    assert_eq!(storage2.get_last_scan_ts().unwrap(), Some(1234));
}

#[test]
fn last_scan_ts_overwrite() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("scan_over.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open");

    storage.set_last_scan_ts(10).expect("set ts 10");
    storage.set_last_scan_ts(20).expect("set ts 20");
    assert_eq!(storage.get_last_scan_ts().unwrap(), Some(20));
}

#[test]
fn unsupported_schema_version_errors() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("schema.db");

    // First open initializes schema to current version
    let storage = SqliteStorage::open(&db_path).expect("initial open");
    // Poison the schema_version to an unsupported future value
    storage
        .raw()
        .execute(
            "UPDATE meta SET value = '999' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    drop(storage); // Close connection before reopening

    let reopen = SqliteStorage::open(&db_path);
    assert!(
        reopen.is_err(),
        "opening with unsupported schema_version should error"
    );
}

// =============================================================================
// Schema Migration Tests (tst.sto.schema)
// Tests for database schema creation and migrations
// =============================================================================

#[test]
fn fresh_db_creates_all_tables() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("fresh.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    // Query sqlite_master for table names
    let tables: Vec<String> = storage
        .raw()
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert!(tables.contains(&"meta".to_string()), "meta table exists");
    assert!(
        tables.contains(&"agents".to_string()),
        "agents table exists"
    );
    assert!(
        tables.contains(&"workspaces".to_string()),
        "workspaces table exists"
    );
    assert!(
        tables.contains(&"conversations".to_string()),
        "conversations table exists"
    );
    assert!(
        tables.contains(&"messages".to_string()),
        "messages table exists"
    );
    assert!(
        tables.contains(&"snippets".to_string()),
        "snippets table exists"
    );
    assert!(tables.contains(&"tags".to_string()), "tags table exists");
    assert!(
        tables.contains(&"conversation_tags".to_string()),
        "conversation_tags table exists"
    );
    // FTS5 virtual table
    assert!(
        tables.contains(&"fts_messages".to_string()),
        "fts_messages virtual table exists"
    );
    // Sources table (v4)
    assert!(
        tables.contains(&"sources".to_string()),
        "sources table exists"
    );
}

#[test]
fn fresh_db_creates_all_indexes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("indexes.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let indexes: Vec<String> = storage
        .raw()
        .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert!(
        indexes.contains(&"idx_conversations_agent_started".to_string()),
        "idx_conversations_agent_started index exists"
    );
    assert!(
        indexes.contains(&"idx_messages_conv_idx".to_string()),
        "idx_messages_conv_idx index exists"
    );
    assert!(
        indexes.contains(&"idx_messages_created".to_string()),
        "idx_messages_created index exists"
    );
}

#[test]
fn agents_table_has_correct_columns() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("agents_cols.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let columns: Vec<String> = storage
        .raw()
        .prepare("PRAGMA table_info(agents)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert!(columns.contains(&"id".to_string()));
    assert!(columns.contains(&"slug".to_string()));
    assert!(columns.contains(&"name".to_string()));
    assert!(columns.contains(&"version".to_string()));
    assert!(columns.contains(&"kind".to_string()));
    assert!(columns.contains(&"created_at".to_string()));
    assert!(columns.contains(&"updated_at".to_string()));
}

#[test]
fn conversations_table_has_correct_columns() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("convs_cols.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let columns: Vec<String> = storage
        .raw()
        .prepare("PRAGMA table_info(conversations)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert!(columns.contains(&"id".to_string()));
    assert!(columns.contains(&"agent_id".to_string()));
    assert!(columns.contains(&"workspace_id".to_string()));
    assert!(columns.contains(&"external_id".to_string()));
    assert!(columns.contains(&"title".to_string()));
    assert!(columns.contains(&"source_path".to_string()));
    assert!(columns.contains(&"started_at".to_string()));
    assert!(columns.contains(&"ended_at".to_string()));
    assert!(columns.contains(&"approx_tokens".to_string()));
    assert!(columns.contains(&"metadata_json".to_string()));
}

#[test]
fn messages_table_has_correct_columns() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("msgs_cols.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let columns: Vec<String> = storage
        .raw()
        .prepare("PRAGMA table_info(messages)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert!(columns.contains(&"id".to_string()));
    assert!(columns.contains(&"conversation_id".to_string()));
    assert!(columns.contains(&"idx".to_string()));
    assert!(columns.contains(&"role".to_string()));
    assert!(columns.contains(&"author".to_string()));
    assert!(columns.contains(&"created_at".to_string()));
    assert!(columns.contains(&"content".to_string()));
    assert!(columns.contains(&"extra_json".to_string()));
}

#[test]
fn fts_messages_is_fts5_virtual_table() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("fts5.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    // Check that fts_messages is an FTS5 virtual table
    let sql: String = storage
        .raw()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='fts_messages' AND type='table'",
            [],
            |r| r.get(0),
        )
        .expect("fts_messages should exist");

    assert!(
        sql.contains("fts5"),
        "fts_messages should be FTS5 virtual table"
    );
    assert!(sql.contains("content"), "fts_messages should have content");
    assert!(sql.contains("title"), "fts_messages should have title");
    assert!(sql.contains("agent"), "fts_messages should have agent");
    assert!(
        sql.contains("workspace"),
        "fts_messages should have workspace"
    );
    assert!(
        sql.contains("porter"),
        "fts_messages should use porter tokenizer"
    );
}

#[test]
fn migration_from_v1_applies_v2_and_v3() {
    use rusqlite::Connection;

    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("migrate_v1.db");

    // Manually create a v1 database
    {
        let conn = Connection::open(&db_path).expect("create v1 db");
        conn.execute_batch(
            r"
            PRAGMA foreign_keys = ON;

            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO meta(key, value) VALUES('schema_version', '1');

            CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                version TEXT,
                kind TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE workspaces (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                display_name TEXT
            );

            CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent_id INTEGER NOT NULL REFERENCES agents(id),
                workspace_id INTEGER REFERENCES workspaces(id),
                external_id TEXT,
                title TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER,
                ended_at INTEGER,
                approx_tokens INTEGER,
                metadata_json TEXT,
                UNIQUE(agent_id, external_id)
            );

            CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                author TEXT,
                created_at INTEGER,
                content TEXT NOT NULL,
                extra_json TEXT,
                UNIQUE(conversation_id, idx)
            );

            CREATE TABLE snippets (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
                file_path TEXT,
                start_line INTEGER,
                end_line INTEGER,
                language TEXT,
                snippet_text TEXT
            );

            CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);

            CREATE TABLE conversation_tags (
                conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
                PRIMARY KEY (conversation_id, tag_id)
            );

            CREATE INDEX idx_conversations_agent_started ON conversations(agent_id, started_at DESC);
            CREATE INDEX idx_messages_conv_idx ON messages(conversation_id, idx);
            CREATE INDEX idx_messages_created ON messages(created_at);
            ",
        )
        .expect("create v1 schema");
    }

    // Open with SqliteStorage - should apply v2, v3, and v4 migrations
    let storage = SqliteStorage::open(&db_path).expect("open v1 db");

    // Verify migration completed
    assert_eq!(storage.schema_version().unwrap(), 5, "should migrate to v5");

    // Verify FTS5 table was created
    let tables: Vec<String> = storage
        .raw()
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='fts_messages'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(tables.len(), 1, "fts_messages should exist after migration");
}

#[test]
fn migration_from_v2_applies_v3() {
    use rusqlite::Connection;

    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("migrate_v2.db");

    // Manually create a v2 database with FTS5 table
    {
        let conn = Connection::open(&db_path).expect("create v2 db");
        conn.execute_batch(
            r"
            PRAGMA foreign_keys = ON;

            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO meta(key, value) VALUES('schema_version', '2');

            CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                version TEXT,
                kind TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE workspaces (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                display_name TEXT
            );

            CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent_id INTEGER NOT NULL REFERENCES agents(id),
                workspace_id INTEGER REFERENCES workspaces(id),
                external_id TEXT,
                title TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER,
                ended_at INTEGER,
                approx_tokens INTEGER,
                metadata_json TEXT,
                UNIQUE(agent_id, external_id)
            );

            CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                author TEXT,
                created_at INTEGER,
                content TEXT NOT NULL,
                extra_json TEXT,
                UNIQUE(conversation_id, idx)
            );

            CREATE TABLE snippets (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
                file_path TEXT,
                start_line INTEGER,
                end_line INTEGER,
                language TEXT,
                snippet_text TEXT
            );

            CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);

            CREATE TABLE conversation_tags (
                conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
                PRIMARY KEY (conversation_id, tag_id)
            );

            CREATE INDEX idx_conversations_agent_started ON conversations(agent_id, started_at DESC);
            CREATE INDEX idx_messages_conv_idx ON messages(conversation_id, idx);
            CREATE INDEX idx_messages_created ON messages(created_at);

            -- V2 FTS5 table
            CREATE VIRTUAL TABLE fts_messages USING fts5(
                content,
                title,
                agent,
                workspace,
                source_path,
                created_at UNINDEXED,
                message_id UNINDEXED,
                tokenize='porter'
            );
            ",
        )
        .expect("create v2 schema");
    }

    // Open with SqliteStorage - should apply v3 and v4 migrations
    let storage = SqliteStorage::open(&db_path).expect("open v2 db");

    // Verify migration completed
    assert_eq!(storage.schema_version().unwrap(), 5, "should migrate to v5");
}

#[test]
fn foreign_keys_are_enforced() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("fk.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    // Try to insert a conversation with non-existent agent_id
    let result = storage.raw().execute(
        "INSERT INTO conversations(agent_id, source_path) VALUES(999, '/test')",
        [],
    );

    assert!(
        result.is_err(),
        "foreign key constraint should prevent invalid agent_id"
    );
}

#[test]
fn unique_constraints_work() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("unique.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    // Insert an agent
    storage
        .raw()
        .execute(
            "INSERT INTO agents(slug, name, kind, created_at, updated_at) VALUES('test', 'Test', 'cli', 0, 0)",
            [],
        )
        .expect("first insert");

    // Try to insert duplicate slug
    let result = storage.raw().execute(
        "INSERT INTO agents(slug, name, kind, created_at, updated_at) VALUES('test', 'Test2', 'cli', 0, 0)",
        [],
    );

    assert!(
        result.is_err(),
        "unique constraint should prevent duplicate slug"
    );
}

#[test]
fn pragmas_are_applied() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("pragmas.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    // Check journal_mode is WAL
    let journal_mode: String = storage
        .raw()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal_mode, "wal", "journal_mode should be WAL");

    // Check foreign_keys is ON
    let fk: i64 = storage
        .raw()
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk, 1, "foreign_keys should be ON");
}

// =============================================================================
// Source CRUD Tests (tst.sto.sources)
// Tests for source table operations
// =============================================================================

#[test]
fn local_source_auto_created_on_init() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("sources.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    // Local source should be auto-created
    let local = storage.get_source(LOCAL_SOURCE_ID).expect("get_source");
    assert!(local.is_some(), "local source should exist");

    let local = local.unwrap();
    assert_eq!(local.id, LOCAL_SOURCE_ID);
    assert_eq!(local.kind, SourceKind::Local);
    assert!(local.host_label.is_none());
}

#[test]
fn list_sources_includes_local() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("sources_list.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let sources = storage.list_sources().expect("list_sources");
    assert!(!sources.is_empty(), "should have at least local source");
    assert!(
        sources.iter().any(|s| s.id == LOCAL_SOURCE_ID),
        "local source should be in list"
    );
}

#[test]
fn upsert_and_get_source() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("sources_upsert.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    // Create a new remote source
    let source = Source {
        id: "work-laptop".to_string(),
        kind: SourceKind::Ssh,
        host_label: Some("user@laptop.local".to_string()),
        machine_id: Some("abc123".to_string()),
        platform: Some("linux".to_string()),
        config_json: Some(serde_json::json!({"port": 22})),
        created_at: None,
        updated_at: None,
    };

    storage.upsert_source(&source).expect("upsert_source");

    // Retrieve it
    let retrieved = storage
        .get_source("work-laptop")
        .expect("get_source")
        .expect("source should exist");

    assert_eq!(retrieved.id, "work-laptop");
    assert_eq!(retrieved.kind, SourceKind::Ssh);
    assert_eq!(retrieved.host_label, Some("user@laptop.local".to_string()));
    assert_eq!(retrieved.machine_id, Some("abc123".to_string()));
    assert_eq!(retrieved.platform, Some("linux".to_string()));
    assert!(retrieved.config_json.is_some());
    assert!(retrieved.created_at.is_some());
    assert!(retrieved.updated_at.is_some());
}

#[test]
fn upsert_updates_existing_source() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("sources_update.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    // Create initial source
    let source1 = Source {
        id: "remote-1".to_string(),
        kind: SourceKind::Ssh,
        host_label: Some("old-label".to_string()),
        machine_id: None,
        platform: None,
        config_json: None,
        created_at: None,
        updated_at: None,
    };
    storage.upsert_source(&source1).expect("first upsert");

    let first = storage
        .get_source("remote-1")
        .expect("get")
        .expect("exists");
    let first_created = first.created_at;

    // Update the source
    let source2 = Source {
        id: "remote-1".to_string(),
        kind: SourceKind::Ssh,
        host_label: Some("new-label".to_string()),
        machine_id: Some("machine-id".to_string()),
        platform: Some("macos".to_string()),
        config_json: None,
        created_at: first_created, // Preserve original created_at
        updated_at: None,
    };
    storage.upsert_source(&source2).expect("second upsert");

    let updated = storage
        .get_source("remote-1")
        .expect("get")
        .expect("exists");

    assert_eq!(updated.host_label, Some("new-label".to_string()));
    assert_eq!(updated.machine_id, Some("machine-id".to_string()));
    assert_eq!(updated.platform, Some("macos".to_string()));
    // created_at should be preserved, updated_at should change
    assert_eq!(updated.created_at, first_created);
    assert!(updated.updated_at >= first.updated_at);
}

#[test]
fn delete_source_removes_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("sources_delete.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    // Create a source
    let source = Source::remote("to-delete", "host.local");
    storage.upsert_source(&source).expect("upsert");

    // Verify it exists
    assert!(storage.get_source("to-delete").unwrap().is_some());

    // Delete it
    let deleted = storage.delete_source("to-delete", false).expect("delete");
    assert!(deleted, "should return true for successful deletion");

    // Verify it's gone
    assert!(storage.get_source("to-delete").unwrap().is_none());
}

#[test]
fn delete_nonexistent_source_returns_false() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("sources_delete_none.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let deleted = storage.delete_source("nonexistent", false).expect("delete");
    assert!(!deleted, "should return false for nonexistent source");
}

#[test]
fn cannot_delete_local_source() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("sources_local_delete.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    // Try to delete local source
    let result = storage.delete_source(LOCAL_SOURCE_ID, false);
    assert!(result.is_err(), "should not be able to delete local source");

    // Verify local source still exists
    assert!(storage.get_source(LOCAL_SOURCE_ID).unwrap().is_some());
}

#[test]
fn sources_table_has_correct_columns() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("sources_cols.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let columns: Vec<String> = storage
        .raw()
        .prepare("PRAGMA table_info(sources)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert!(columns.contains(&"id".to_string()));
    assert!(columns.contains(&"kind".to_string()));
    assert!(columns.contains(&"host_label".to_string()));
    assert!(columns.contains(&"machine_id".to_string()));
    assert!(columns.contains(&"platform".to_string()));
    assert!(columns.contains(&"config_json".to_string()));
    assert!(columns.contains(&"created_at".to_string()));
    assert!(columns.contains(&"updated_at".to_string()));
}

#[test]
fn migration_from_v3_creates_sources_table() {
    use rusqlite::Connection;

    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("migrate_v3.db");

    // Manually create a v3 database (without sources table)
    {
        let conn = Connection::open(&db_path).expect("create v3 db");
        conn.execute_batch(
            r"
            PRAGMA foreign_keys = ON;

            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO meta(key, value) VALUES('schema_version', '3');

            CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                version TEXT,
                kind TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE workspaces (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                display_name TEXT
            );

            CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent_id INTEGER NOT NULL REFERENCES agents(id),
                workspace_id INTEGER REFERENCES workspaces(id),
                external_id TEXT,
                title TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER,
                ended_at INTEGER,
                approx_tokens INTEGER,
                metadata_json TEXT,
                UNIQUE(agent_id, external_id)
            );

            CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                author TEXT,
                created_at INTEGER,
                content TEXT NOT NULL,
                extra_json TEXT,
                UNIQUE(conversation_id, idx)
            );

            CREATE TABLE snippets (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
                file_path TEXT,
                start_line INTEGER,
                end_line INTEGER,
                language TEXT,
                snippet_text TEXT
            );

            CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);

            CREATE TABLE conversation_tags (
                conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
                PRIMARY KEY (conversation_id, tag_id)
            );

            CREATE INDEX idx_conversations_agent_started ON conversations(agent_id, started_at DESC);
            CREATE INDEX idx_messages_conv_idx ON messages(conversation_id, idx);
            CREATE INDEX idx_messages_created ON messages(created_at);

            CREATE VIRTUAL TABLE fts_messages USING fts5(
                content,
                title,
                agent,
                workspace,
                source_path,
                created_at UNINDEXED,
                message_id UNINDEXED,
                tokenize='porter'
            );
            ",
        )
        .expect("create v3 schema");
    }

    // Open with SqliteStorage - should apply v4 migration
    let storage = SqliteStorage::open(&db_path).expect("open v3 db");

    // Verify migration completed
    assert_eq!(storage.schema_version().unwrap(), 5, "should migrate to v5");

    // Verify sources table was created with local source
    let sources = storage.list_sources().expect("list_sources");
    assert!(
        sources.iter().any(|s| s.id == LOCAL_SOURCE_ID),
        "local source should exist after migration"
    );
}

// -------------------------------------------------------------------------
// P1.5 Migration Safety Tests
// -------------------------------------------------------------------------

use coding_agent_search::storage::sqlite::{
    MigrationError, CURRENT_SCHEMA_VERSION, cleanup_old_backups, create_backup, is_user_data_file,
};

#[test]
fn is_user_data_file_detects_protected_files() {
    use std::path::Path;

    // Protected files
    assert!(is_user_data_file(Path::new("/data/bookmarks.db")));
    assert!(is_user_data_file(Path::new("/data/tui_state.json")));
    assert!(is_user_data_file(Path::new("/data/sources.toml")));
    assert!(is_user_data_file(Path::new("/data/.env")));

    // Not protected
    assert!(!is_user_data_file(Path::new("/data/agent_search.db")));
    assert!(!is_user_data_file(Path::new("/data/index")));
    assert!(!is_user_data_file(Path::new("/data/something.txt")));
}

#[test]
fn current_schema_version_matches_internal() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("version.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    assert_eq!(
        storage.schema_version().unwrap(),
        CURRENT_SCHEMA_VERSION,
        "CURRENT_SCHEMA_VERSION should match actual schema version"
    );
}

#[test]
fn create_backup_creates_timestamped_copy() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("backup_test.db");

    // Create a database file
    std::fs::write(&db_path, b"test database content").unwrap();

    // Create backup
    let backup = create_backup(&db_path).expect("create_backup");
    assert!(backup.is_some(), "backup should be created");

    let backup_path = backup.unwrap();
    assert!(backup_path.exists(), "backup file should exist");
    assert!(
        backup_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("backup_test.db.backup."),
        "backup should have correct name pattern"
    );

    // Verify content matches
    let original = std::fs::read(&db_path).unwrap();
    let backed_up = std::fs::read(&backup_path).unwrap();
    assert_eq!(original, backed_up, "backup content should match original");
}

#[test]
fn create_backup_returns_none_for_nonexistent_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("nonexistent.db");

    let backup = create_backup(&db_path).expect("create_backup");
    assert!(backup.is_none(), "backup should be None for nonexistent file");
}

#[test]
fn cleanup_old_backups_keeps_recent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("cleanup_test.db");

    // Create 5 backup files with different timestamps
    for i in 0..5 {
        let backup_name = format!("cleanup_test.db.backup.{}", 1000 + i);
        let backup_path = tmp.path().join(&backup_name);
        std::fs::write(&backup_path, format!("backup {}", i)).unwrap();
        // Add small delay to ensure different mtimes
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Keep only 2
    cleanup_old_backups(&db_path, 2).expect("cleanup");

    // Count remaining backups
    let remaining: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with("cleanup_test.db.backup."))
                .unwrap_or(false)
        })
        .collect();

    assert_eq!(remaining.len(), 2, "should keep only 2 backups");
}

#[test]
fn open_or_rebuild_creates_fresh_db() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("fresh.db");

    // Open fresh database
    let storage = SqliteStorage::open_or_rebuild(&db_path).expect("open_or_rebuild");

    assert_eq!(
        storage.schema_version().unwrap(),
        CURRENT_SCHEMA_VERSION,
        "fresh db should have current schema version"
    );
}

#[test]
fn open_or_rebuild_migrates_compatible_schema() {
    use rusqlite::Connection;

    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("migrate.db");

    // Create a v4 database (without provenance columns in conversations)
    {
        let conn = Connection::open(&db_path).expect("create v4 db");
        conn.execute_batch(
            r"
            PRAGMA foreign_keys = ON;
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO meta(key, value) VALUES('schema_version', '4');

            CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                version TEXT,
                kind TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE workspaces (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                display_name TEXT
            );

            CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent_id INTEGER NOT NULL REFERENCES agents(id),
                workspace_id INTEGER REFERENCES workspaces(id),
                external_id TEXT,
                title TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER,
                ended_at INTEGER,
                approx_tokens INTEGER,
                metadata_json TEXT,
                UNIQUE(agent_id, external_id)
            );

            CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                author TEXT,
                created_at INTEGER,
                content TEXT NOT NULL,
                extra_json TEXT,
                UNIQUE(conversation_id, idx)
            );

            CREATE TABLE snippets (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
                file_path TEXT,
                start_line INTEGER,
                end_line INTEGER,
                language TEXT,
                snippet_text TEXT
            );

            CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);

            CREATE TABLE conversation_tags (
                conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
                PRIMARY KEY (conversation_id, tag_id)
            );

            CREATE TABLE sources (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                host_label TEXT,
                machine_id TEXT,
                platform TEXT,
                config_json TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            INSERT INTO sources (id, kind, host_label, created_at, updated_at)
            VALUES ('local', 'local', NULL, 0, 0);

            CREATE VIRTUAL TABLE fts_messages USING fts5(
                content, title, agent, workspace, source_path,
                created_at UNINDEXED, message_id UNINDEXED,
                tokenize='porter'
            );
            ",
        )
        .expect("create v4 schema");
    }

    // Open with open_or_rebuild - should migrate successfully
    let storage = SqliteStorage::open_or_rebuild(&db_path).expect("open_or_rebuild");

    assert_eq!(
        storage.schema_version().unwrap(),
        CURRENT_SCHEMA_VERSION,
        "should migrate to current version"
    );
}

#[test]
fn open_or_rebuild_triggers_rebuild_for_future_version() {
    use rusqlite::Connection;

    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("future.db");

    // Create a database with a future schema version
    {
        let conn = Connection::open(&db_path).expect("create future db");
        conn.execute_batch(
            r"
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO meta(key, value) VALUES('schema_version', '999');
            ",
        )
        .expect("create future schema");
    }

    // Open with open_or_rebuild - should trigger rebuild
    let result = SqliteStorage::open_or_rebuild(&db_path);

    match result {
        Err(MigrationError::RebuildRequired { reason, backup_path }) => {
            assert!(
                reason.contains("999"),
                "reason should mention future version: {}",
                reason
            );
            assert!(backup_path.is_some(), "backup should be created");
            let backup = backup_path.unwrap();
            assert!(backup.exists(), "backup file should exist");
        }
        Ok(_) => panic!("should have triggered rebuild for future version"),
        Err(e) => panic!("unexpected error: {}", e),
    }

    // Original database should be deleted
    assert!(!db_path.exists(), "original db should be deleted");
}

#[test]
fn open_or_rebuild_handles_corrupted_db() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("corrupt.db");

    // Create a corrupted database file
    std::fs::write(&db_path, b"this is not a valid sqlite database").unwrap();

    // Open with open_or_rebuild - should trigger rebuild
    let result = SqliteStorage::open_or_rebuild(&db_path);

    match result {
        Err(MigrationError::RebuildRequired { backup_path, .. }) => {
            assert!(backup_path.is_some(), "backup should be created");
        }
        Err(_) => {
            // Also acceptable - database error during check
        }
        Ok(_) => panic!("should have failed for corrupted db"),
    }
}

// =============================================================================
// Timeline Source Filtering Tests (P7.8)
// Tests for --source filtering in timeline command
// =============================================================================

/// Create a conversation with a specific source_id for testing timeline filtering
fn sample_conv_with_source(
    external_id: &str,
    source_id: &str,
    started_at: i64,
    messages: Vec<Message>,
) -> Conversation {
    Conversation {
        id: None,
        agent_slug: "tester".into(),
        workspace: Some(PathBuf::from("/workspace/demo")),
        external_id: Some(external_id.to_string()),
        title: Some(format!("Conv from {}", source_id)),
        source_path: PathBuf::from(format!("/logs/{}.jsonl", external_id)),
        started_at: Some(started_at),
        ended_at: Some(started_at + 100),
        approx_tokens: Some(42),
        metadata_json: serde_json::json!({}),
        messages,
        source_id: source_id.to_string(),
        origin_host: if source_id != "local" {
            Some(format!("{}.local", source_id))
        } else {
            None
        },
    }
}

#[test]
fn timeline_source_filter_local_only() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("timeline.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open");

    // Setup: Create agent
    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(PathBuf::from("/workspace/demo").as_path(), Some("Demo"))
        .unwrap();

    // Ensure sources exist
    storage
        .upsert_source(&Source::local())
        .expect("ensure local source");
    storage
        .upsert_source(&Source::remote("laptop", "laptop.local"))
        .expect("ensure remote source");

    // Insert conversations: 2 local, 1 remote
    let now = 1700000000i64;
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("local-1", "local", now, vec![msg(0, now)]),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("local-2", "local", now + 1000, vec![msg(0, now + 1000)]),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("remote-1", "laptop", now + 2000, vec![msg(0, now + 2000)]),
        )
        .unwrap();

    // Query with source_id = 'local' filter
    let local_only: Vec<String> = storage
        .raw()
        .prepare(
            "SELECT c.external_id FROM conversations c
             WHERE c.source_id = 'local'
             ORDER BY c.started_at DESC",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(local_only.len(), 2, "should return 2 local conversations");
    assert!(local_only.contains(&"local-1".to_string()));
    assert!(local_only.contains(&"local-2".to_string()));
    assert!(
        !local_only.contains(&"remote-1".to_string()),
        "should not include remote"
    );
}

#[test]
fn timeline_source_filter_remote_only() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("timeline_remote.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(PathBuf::from("/workspace/demo").as_path(), Some("Demo"))
        .unwrap();

    storage
        .upsert_source(&Source::local())
        .expect("ensure local source");
    storage
        .upsert_source(&Source::remote("laptop", "laptop.local"))
        .expect("ensure remote source");
    storage
        .upsert_source(&Source::remote("server", "server.example.com"))
        .expect("ensure second remote source");

    let now = 1700000000i64;
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("local-1", "local", now, vec![msg(0, now)]),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("laptop-1", "laptop", now + 1000, vec![msg(0, now + 1000)]),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("server-1", "server", now + 2000, vec![msg(0, now + 2000)]),
        )
        .unwrap();

    // Query with source_id != 'local' (remote filter)
    let remote_only: Vec<String> = storage
        .raw()
        .prepare(
            "SELECT c.external_id FROM conversations c
             WHERE c.source_id != 'local'
             ORDER BY c.started_at DESC",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(remote_only.len(), 2, "should return 2 remote conversations");
    assert!(remote_only.contains(&"laptop-1".to_string()));
    assert!(remote_only.contains(&"server-1".to_string()));
    assert!(
        !remote_only.contains(&"local-1".to_string()),
        "should not include local"
    );
}

#[test]
fn timeline_source_filter_specific_source() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("timeline_specific.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(PathBuf::from("/workspace/demo").as_path(), Some("Demo"))
        .unwrap();

    storage
        .upsert_source(&Source::local())
        .expect("ensure local source");
    storage
        .upsert_source(&Source::remote("laptop", "laptop.local"))
        .expect("ensure remote source");
    storage
        .upsert_source(&Source::remote("server", "server.example.com"))
        .expect("ensure second remote source");

    let now = 1700000000i64;
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("local-1", "local", now, vec![msg(0, now)]),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("laptop-1", "laptop", now + 1000, vec![msg(0, now + 1000)]),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("laptop-2", "laptop", now + 2000, vec![msg(0, now + 2000)]),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("server-1", "server", now + 3000, vec![msg(0, now + 3000)]),
        )
        .unwrap();

    // Query with source_id = 'laptop' (specific source)
    let laptop_only: Vec<String> = storage
        .raw()
        .prepare(
            "SELECT c.external_id FROM conversations c
             WHERE c.source_id = 'laptop'
             ORDER BY c.started_at DESC",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(
        laptop_only.len(),
        2,
        "should return 2 laptop conversations"
    );
    assert!(laptop_only.contains(&"laptop-1".to_string()));
    assert!(laptop_only.contains(&"laptop-2".to_string()));
    assert!(
        !laptop_only.contains(&"local-1".to_string()),
        "should not include local"
    );
    assert!(
        !laptop_only.contains(&"server-1".to_string()),
        "should not include server"
    );
}

// =============================================================================
// Timeline JSON Provenance Fields Tests (P7.10)
// Tests for provenance fields (source_id, origin_kind, origin_host) in timeline output
// =============================================================================

#[test]
fn timeline_json_includes_source_id_field() {
    // P7.10: Verify timeline SQL returns source_id field
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("timeline_json.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(PathBuf::from("/workspace/demo").as_path(), Some("Demo"))
        .unwrap();

    storage
        .upsert_source(&Source::local())
        .expect("upsert local source");

    let now = 1700000000i64;
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("test-1", "local", now, vec![msg(0, now)]),
        )
        .unwrap();

    // Query with source_id field selection (simulates timeline JSON output)
    let result: Vec<(i64, String)> = storage
        .raw()
        .prepare(
            "SELECT c.id, c.source_id FROM conversations c
             WHERE c.source_id IS NOT NULL",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert!(!result.is_empty(), "should have at least one conversation");
    let (_, source_id) = &result[0];
    assert_eq!(source_id, "local", "source_id should be 'local'");
}

#[test]
fn timeline_json_includes_origin_kind_field() {
    // P7.10: Verify timeline SQL returns origin_kind from sources table
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("timeline_kind.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(PathBuf::from("/workspace/demo").as_path(), Some("Demo"))
        .unwrap();

    // Create both local and remote sources
    storage
        .upsert_source(&Source::local())
        .expect("upsert local source");
    storage
        .upsert_source(&Source::remote("laptop", "laptop.local"))
        .expect("upsert remote source");

    let now = 1700000000i64;
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("local-conv", "local", now, vec![msg(0, now)]),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("remote-conv", "laptop", now + 1000, vec![msg(0, now + 1000)]),
        )
        .unwrap();

    // Query with origin_kind from sources table (matches timeline SQL)
    let results: Vec<(String, String, String)> = storage
        .raw()
        .prepare(
            "SELECT c.source_id, c.origin_host, s.kind as origin_kind
             FROM conversations c
             LEFT JOIN sources s ON c.source_id = s.id
             ORDER BY c.source_id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?.unwrap_or_else(|| "local".into()),
            ))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(results.len(), 2, "should have 2 conversations");

    // Find local and remote results
    let local = results.iter().find(|(id, _, _)| id == "local").expect("local conv");
    let remote = results.iter().find(|(id, _, _)| id == "laptop").expect("remote conv");

    // Verify origin_kind is correct
    assert_eq!(local.2, "local", "local source should have kind 'local'");
    assert_eq!(remote.2, "ssh", "remote source should have kind 'ssh'");
}

#[test]
fn timeline_json_includes_origin_host_field() {
    // P7.10: Verify timeline SQL returns origin_host for remote sessions
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("timeline_host.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(PathBuf::from("/workspace/demo").as_path(), Some("Demo"))
        .unwrap();

    storage
        .upsert_source(&Source::local())
        .expect("upsert local source");
    storage
        .upsert_source(&Source::remote("work", "user@work.example.com"))
        .expect("upsert remote source");

    let now = 1700000000i64;

    // Local conversation - origin_host should be null
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("local-conv", "local", now, vec![msg(0, now)]),
        )
        .unwrap();

    // Remote conversation - origin_host set via sample_conv_with_source
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("remote-conv", "work", now + 1000, vec![msg(0, now + 1000)]),
        )
        .unwrap();

    // Query origin_host field
    let results: Vec<(String, Option<String>)> = storage
        .raw()
        .prepare(
            "SELECT c.source_id, c.origin_host FROM conversations c ORDER BY c.source_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(results.len(), 2, "should have 2 conversations");

    let local = results.iter().find(|(id, _)| id == "local").expect("local conv");
    let remote = results.iter().find(|(id, _)| id == "work").expect("remote conv");

    // Local should have null origin_host
    assert!(
        local.1.is_none(),
        "local source should have null origin_host"
    );

    // Remote should have origin_host set
    assert!(
        remote.1.is_some(),
        "remote source should have origin_host set"
    );
    assert_eq!(
        remote.1.as_deref(),
        Some("work.local"),
        "origin_host should match the pattern from sample_conv_with_source"
    );
}

#[test]
fn timeline_json_grouped_output_includes_provenance() {
    // P7.10: Verify provenance fields are present when timeline is grouped
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("timeline_grouped.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(PathBuf::from("/workspace/demo").as_path(), Some("Demo"))
        .unwrap();

    storage.upsert_source(&Source::local()).expect("upsert local");
    storage
        .upsert_source(&Source::remote("server", "server.example.com"))
        .expect("upsert remote");

    let now = 1700000000i64;
    // Same day, different sources
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("local-1", "local", now, vec![msg(0, now)]),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &sample_conv_with_source("server-1", "server", now + 100, vec![msg(0, now + 100)]),
        )
        .unwrap();

    // Query all provenance fields as timeline JSON would
    let results: Vec<(i64, String, Option<String>, Option<String>)> = storage
        .raw()
        .prepare(
            "SELECT c.id, c.source_id, c.origin_host, s.kind as origin_kind
             FROM conversations c
             LEFT JOIN sources s ON c.source_id = s.id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    // All entries should have source_id
    for (id, source_id, _, _) in &results {
        assert!(
            !source_id.is_empty(),
            "Entry {} should have non-empty source_id",
            id
        );
    }

    // Verify we have both local and remote entries with correct kinds
    let has_local = results
        .iter()
        .any(|(_, sid, _, kind)| sid == "local" && kind.as_deref() == Some("local"));
    let has_remote = results
        .iter()
        .any(|(_, sid, _, kind)| sid == "server" && kind.as_deref() == Some("ssh"));

    assert!(has_local, "should have local entry with kind='local'");
    assert!(has_remote, "should have remote entry with kind='ssh'");
}
