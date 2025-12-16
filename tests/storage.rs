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

    assert_eq!(storage.schema_version().unwrap(), 4);

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
    assert_eq!(storage.schema_version().unwrap(), 4, "should migrate to v4");

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
    assert_eq!(storage.schema_version().unwrap(), 4, "should migrate to v4");
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
    assert_eq!(storage.schema_version().unwrap(), 4, "should migrate to v4");

    // Verify sources table was created with local source
    let sources = storage.list_sources().expect("list_sources");
    assert!(
        sources.iter().any(|s| s.id == LOCAL_SOURCE_ID),
        "local source should exist after migration"
    );
}
