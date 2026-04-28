use std::path::{Path, PathBuf};

use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::sources::provenance::{LOCAL_SOURCE_ID, Source, SourceKind};
use coding_agent_search::storage::sqlite::{MigrationError, SqliteStorage};
use frankensqlite::Connection as FrankenConnection;
use frankensqlite::compat::{ConnectionExt, ParamValue, RowExt};

fn open_fixture_db(path: impl AsRef<Path>) -> FrankenConnection {
    let path = path.as_ref().to_string_lossy();
    FrankenConnection::open(path.as_ref()).expect("open frankensqlite fixture database")
}

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
fn schema_version_uses_schema_migrations_after_open() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("store.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    assert_eq!(
        storage.schema_version().unwrap(),
        coding_agent_search::storage::sqlite::CURRENT_SCHEMA_VERSION
    );

    // `_schema_migrations` is authoritative now, so removing the legacy
    // compatibility row must not break schema version reporting.
    storage.raw().execute("DELETE FROM meta").unwrap();
    assert!(
        matches!(
            storage.schema_version(),
            Ok(coding_agent_search::storage::sqlite::CURRENT_SCHEMA_VERSION)
        ),
        "schema_version should continue reading from _schema_migrations after meta cleanup, got: {:?}",
        storage.schema_version()
    );
}

#[test]
fn rebuild_fts_repopulates_rows() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("fts.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

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
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |r| r.get_typed(0))
        .unwrap();
    let mut fts_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM fts_messages", &[], |r| r.get_typed(0))
        .unwrap();
    assert_eq!(fts_count, count_messages);

    storage.raw().execute("DROP TABLE fts_messages").unwrap();
    let fts_table_count: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='fts_messages' AND type='table'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();
    assert_eq!(
        fts_table_count, 0,
        "fts_messages should be absent after drop"
    );

    storage.rebuild_fts().unwrap();
    fts_count = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM fts_messages", &[], |r| r.get_typed(0))
        .unwrap();
    assert_eq!(fts_count, count_messages);
}

#[test]
fn duplicate_idx_within_new_conversation_keeps_first_message() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("rollback.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let conv = sample_conv(None, vec![msg(0, 1), msg(0, 2)]);
    let outcome = storage
        .insert_conversation_tree(agent_id, None, &conv)
        .expect("duplicate idx insert should keep the first canonical message");
    assert_eq!(outcome.inserted_indices, vec![0]);

    let conv_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM conversations", &[], |c| {
            c.get_typed(0)
        })
        .unwrap();
    let msg_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |c| c.get_typed(0))
        .unwrap();

    assert_eq!(conv_count, 1, "conversation should still be inserted");
    assert_eq!(
        msg_count, 1,
        "only the first duplicate idx should be retained"
    );
}

#[test]
fn insert_conversation_tree_succeeds_without_db_resident_fts() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("fts_tree_rollback.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    storage.raw().execute("DROP TABLE fts_messages").unwrap();

    let conv = sample_conv(None, vec![msg(0, 1)]);
    let result = storage.insert_conversation_tree(agent_id, None, &conv);
    assert!(
        result.is_ok(),
        "missing db-resident FTS should not abort authoritative storage inserts"
    );

    let conv_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM conversations", &[], |r| {
            r.get_typed(0)
        })
        .unwrap();
    let msg_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |r| r.get_typed(0))
        .unwrap();

    let fts_table_count: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='fts_messages' AND type='table'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();

    assert_eq!(conv_count, 1, "conversation insert should succeed");
    assert_eq!(msg_count, 1, "message insert should succeed");
    assert_eq!(fts_table_count, 0, "db-resident FTS should remain absent");
}

#[test]
fn insert_conversations_batched_succeeds_without_db_resident_fts() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("fts_batch_rollback.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let convs = [
        sample_conv(Some("batch-rollback-1"), vec![msg(0, 1)]),
        sample_conv(Some("batch-rollback-2"), vec![msg(0, 2)]),
    ];
    let refs: Vec<(i64, Option<i64>, &Conversation)> =
        convs.iter().map(|conv| (agent_id, None, conv)).collect();

    storage.raw().execute("DROP TABLE fts_messages").unwrap();

    let result = storage.insert_conversations_batched(&refs);
    assert!(
        result.is_ok(),
        "batched insert should continue when db-resident FTS maintenance is unavailable"
    );

    let conv_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM conversations", &[], |r| {
            r.get_typed(0)
        })
        .unwrap();
    let msg_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |r| r.get_typed(0))
        .unwrap();

    let fts_table_count: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='fts_messages' AND type='table'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();

    assert_eq!(
        conv_count, 2,
        "batched conversations should still be stored"
    );
    assert_eq!(msg_count, 2, "batched messages should still be stored");
    assert_eq!(fts_table_count, 0, "db-resident FTS should remain absent");
}

#[test]
fn append_only_updates_existing_conversation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("append.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

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
        .query_map_collect(
            "SELECT idx, created_at FROM messages ORDER BY idx",
            &[],
            |r| Ok((r.get_typed(0)?, r.get_typed::<Option<i64>>(1)?.unwrap())),
        )
        .unwrap();
    assert_eq!(rows, vec![(0, 100), (1, 200), (2, 300)]);

    let ended_at: i64 = storage
        .raw()
        .query_row_map(
            "SELECT ended_at FROM conversations WHERE id = ?",
            &[ParamValue::from(outcome1.conversation_id)],
            |r| r.get_typed(0),
        )
        .unwrap();
    assert_eq!(ended_at, 300);
}

#[test]
fn large_batch_insert_keeps_fts_in_sync() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("batch.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

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
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |r| r.get_typed(0))
        .unwrap();
    let fts_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM fts_messages", &[], |r| r.get_typed(0))
        .unwrap();

    assert_eq!(msg_count, 200);
    assert_eq!(fts_count, 200);

    // Spot check a few message rows for correct ordering and timestamps
    let rows: Vec<(i64, i64)> = storage
        .raw()
        .query_map_collect(
            "SELECT idx, created_at FROM messages ORDER BY idx LIMIT 3 OFFSET 197",
            &[],
            |r| Ok((r.get_typed(0)?, r.get_typed::<Option<i64>>(1)?.unwrap())),
        )
        .unwrap();
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
    let storage = SqliteStorage::open(&db_path).expect("open");

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
    let storage = SqliteStorage::open(&db_path).expect("open");

    storage.set_last_scan_ts(10).expect("set ts 10");
    storage.set_last_scan_ts(20).expect("set ts 20");
    assert_eq!(storage.get_last_scan_ts().unwrap(), Some(20));
}

#[test]
fn open_ignores_stale_meta_schema_version_once_schema_migrations_exist() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("schema.db");

    // First open initializes schema to current version
    let storage = SqliteStorage::open(&db_path).expect("initial open");
    // Poison the schema_version to an unsupported future value
    storage
        .raw()
        .execute("UPDATE meta SET value = '999' WHERE key = 'schema_version'")
        .unwrap();
    drop(storage); // Close connection before reopening

    let reopen = SqliteStorage::open(&db_path);
    assert!(
        reopen.is_ok(),
        "open() should rely on _schema_migrations, not the legacy meta mirror"
    );
    assert_eq!(
        reopen.unwrap().schema_version().unwrap(),
        coding_agent_search::storage::sqlite::CURRENT_SCHEMA_VERSION
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
        .query_map_collect(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();

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
        .query_map_collect("SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%' ORDER BY name", &[], |r| r.get_typed(0))
        .unwrap();

    assert!(
        indexes.contains(&"idx_conversations_agent_started".to_string()),
        "idx_conversations_agent_started index exists"
    );
    assert!(
        indexes.contains(&"idx_messages_created".to_string()),
        "idx_messages_created index exists"
    );

    let message_indexes: Vec<String> = storage
        .raw()
        .query_map_collect("PRAGMA index_list(messages)", &[], |r| r.get_typed(1))
        .unwrap();
    assert!(
        message_indexes.contains(&"sqlite_autoindex_messages_1".to_string()),
        "messages UNIQUE(conversation_id, idx) autoindex should exist, found: {message_indexes:?}"
    );
    assert!(
        message_indexes.contains(&"idx_messages_conv_idx".to_string()),
        "fresh schema should create the named message index, found: {message_indexes:?}"
    );
}

#[test]
fn agents_table_has_correct_columns() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("agents_cols.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let columns: Vec<String> = storage
        .raw()
        .query_map_collect("PRAGMA table_info(agents)", &[], |r| {
            r.get_typed::<String>(1)
        })
        .unwrap();

    let missing: Vec<&str> = [
        "id",
        "slug",
        "name",
        "version",
        "kind",
        "created_at",
        "updated_at",
    ]
    .iter()
    .filter(|&&col| !columns.contains(&col.to_string()))
    .copied()
    .collect();
    assert!(
        missing.is_empty(),
        "agents table missing columns: {:?}, found: {:?}",
        missing,
        columns
    );
}

#[test]
fn conversations_table_has_correct_columns() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("convs_cols.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let columns: Vec<String> = storage
        .raw()
        .query_map_collect("PRAGMA table_info(conversations)", &[], |r| {
            r.get_typed::<String>(1)
        })
        .unwrap();

    let expected = [
        "id",
        "agent_id",
        "workspace_id",
        "external_id",
        "title",
        "source_path",
        "started_at",
        "ended_at",
        "approx_tokens",
        "metadata_json",
    ];
    let missing: Vec<&str> = expected
        .iter()
        .filter(|&&col| !columns.contains(&col.to_string()))
        .copied()
        .collect();
    assert!(
        missing.is_empty(),
        "conversations table missing columns: {:?}, found: {:?}",
        missing,
        columns
    );
}

#[test]
fn messages_table_has_correct_columns() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("msgs_cols.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let columns: Vec<String> = storage
        .raw()
        .query_map_collect("PRAGMA table_info(messages)", &[], |r| {
            r.get_typed::<String>(1)
        })
        .unwrap();

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
        .query_row_map(
            "SELECT sql FROM sqlite_master WHERE name='fts_messages' AND type='table'",
            &[],
            |r| r.get_typed(0),
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
        sql.contains("content=''") || sql.contains("content = ''"),
        "fts_messages should use contentless storage"
    );
    assert!(
        !sql.contains("message_id UNINDEXED"),
        "fts_messages should no longer store legacy message_id payloads"
    );
    assert!(
        sql.contains("porter"),
        "fts_messages should use porter tokenizer"
    );
}

#[test]
fn fresh_database_fts_messages_is_queryable_via_frankensqlite() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("fresh-fts.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM fts_messages", &[], |row| {
            row.get_typed(0)
        })
        .expect("fresh FTS table should be queryable via frankensqlite");
    assert_eq!(count, 0, "fresh FTS table should start empty");
}

#[test]
fn open_disables_frankensqlite_autocommit_retain() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("autocommit-retain.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let namespaced: i64 = storage
        .raw()
        .query_row_map("PRAGMA fsqlite.autocommit_retain;", &[], |row| {
            row.get_typed(0)
        })
        .expect("fsqlite.autocommit_retain pragma should be queryable");
    assert_eq!(
        namespaced, 0,
        "storage open should disable retained autocommit"
    );

    let alias: i64 = storage
        .raw()
        .query_row_map("PRAGMA autocommit_retain;", &[], |row| row.get_typed(0))
        .expect("autocommit_retain pragma alias should be queryable");
    assert_eq!(alias, 0, "autocommit_retain alias should also be disabled");
}

#[test]
fn migration_from_v1_requires_rebuild() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("migrate_v1.db");

    // Manually create a v1 database
    {
        let conn = open_fixture_db(&db_path);
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

    let result = SqliteStorage::open_or_rebuild(&db_path);
    match result {
        Err(MigrationError::RebuildRequired {
            reason,
            backup_path,
        }) => {
            assert!(
                reason.contains("too old for in-place migration"),
                "unexpected rebuild reason: {reason}"
            );
            assert!(backup_path.is_some(), "legacy schema should be backed up");
        }
        Ok(_) => panic!("expected rebuild requirement for v1 schema"),
        Err(err) => panic!("expected rebuild requirement for v1 schema, got {err}"),
    }
    assert!(
        !db_path.exists(),
        "legacy v1 database should be removed after rebuild requirement"
    );
}

#[test]
fn migration_from_v2_requires_rebuild() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("migrate_v2.db");

    // Manually create a v2 database with FTS5 table
    {
        let conn = open_fixture_db(&db_path);
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

    let result = SqliteStorage::open_or_rebuild(&db_path);
    match result {
        Err(MigrationError::RebuildRequired {
            reason,
            backup_path,
        }) => {
            assert!(
                reason.contains("too old for in-place migration"),
                "unexpected rebuild reason: {reason}"
            );
            assert!(backup_path.is_some(), "legacy schema should be backed up");
        }
        Ok(_) => panic!("expected rebuild requirement for v2 schema"),
        Err(err) => panic!("expected rebuild requirement for v2 schema, got {err}"),
    }
    assert!(
        !db_path.exists(),
        "legacy v2 database should be removed after rebuild requirement"
    );
}

#[test]
fn foreign_keys_are_enforced() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("fk.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    // Try to insert a conversation with non-existent agent_id
    let result = storage
        .raw()
        .execute("INSERT INTO conversations(agent_id, source_path) VALUES(999, '/test')");

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
        )
        .expect("first insert");

    // Try to insert duplicate slug
    let result = storage.raw().execute(
        "INSERT INTO agents(slug, name, kind, created_at, updated_at) VALUES('test', 'Test2', 'cli', 0, 0)",
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
        .query_row_map("PRAGMA journal_mode", &[], |r| r.get_typed(0))
        .unwrap();
    assert_eq!(journal_mode, "wal", "journal_mode should be WAL");

    // Check foreign_keys is ON
    let fk: i64 = storage
        .raw()
        .query_row_map("PRAGMA foreign_keys", &[], |r| r.get_typed(0))
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
    // Bead 7k7pl: pin the EXACT seeded config_json value and check
    // both timestamps are plausible ms-epoch values (not 0 / MIN). A
    // regression that stored `null` config, clock-zeroed timestamps,
    // or 1970-epoch would slip past `.is_some()` while breaking
    // downstream time-based queries.
    assert_eq!(
        retrieved.config_json,
        Some(serde_json::json!({"port": 22})),
        "seeded config_json must round-trip unchanged"
    );
    let created = retrieved
        .created_at
        .expect("created_at must be set after upsert");
    let updated = retrieved
        .updated_at
        .expect("updated_at must be set after upsert");
    // ms-epoch sanity floor: 2001-09-09 (1_000_000_000_000 ms).
    assert!(
        created >= 1_000_000_000_000,
        "created_at must be a plausible ms-epoch value; got {created}"
    );
    assert!(
        updated >= created,
        "updated_at must be >= created_at; got created={created}, updated={updated}"
    );
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
        .query_map_collect("PRAGMA table_info(sources)", &[], |r| {
            r.get_typed::<String>(1)
        })
        .unwrap();

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
fn migration_from_v3_requires_rebuild() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("migrate_v3.db");

    // Manually create a v3 database (without sources table)
    {
        let conn = open_fixture_db(&db_path);
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

    let result = SqliteStorage::open_or_rebuild(&db_path);
    match result {
        Err(MigrationError::RebuildRequired {
            reason,
            backup_path,
        }) => {
            assert!(
                reason.contains("too old for in-place migration"),
                "unexpected rebuild reason: {reason}"
            );
            assert!(backup_path.is_some(), "legacy schema should be backed up");
        }
        Ok(_) => panic!("expected rebuild requirement for v3 schema"),
        Err(err) => panic!("expected rebuild requirement for v3 schema, got {err}"),
    }
    assert!(
        !db_path.exists(),
        "legacy v3 database should be removed after rebuild requirement"
    );
}

// -------------------------------------------------------------------------
// P1.5 Migration Safety Tests
// -------------------------------------------------------------------------

use coding_agent_search::storage::sqlite::{
    CURRENT_SCHEMA_VERSION, cleanup_old_backups, create_backup, is_user_data_file,
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
fn create_backup_creates_named_copy() {
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
    assert!(
        backup.is_none(),
        "backup should be None for nonexistent file"
    );
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
fn open_or_rebuild_requires_rebuild_for_legacy_v4_schema() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("migrate.db");

    // Create a v4 database (without provenance columns in conversations)
    {
        let conn = open_fixture_db(&db_path);
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
    let result = SqliteStorage::open_or_rebuild(&db_path);
    match result {
        Err(MigrationError::RebuildRequired {
            reason,
            backup_path,
        }) => {
            assert!(
                reason.contains("too old for in-place migration"),
                "unexpected rebuild reason: {reason}"
            );
            assert!(backup_path.is_some(), "legacy schema should be backed up");
        }
        Ok(_) => panic!("expected rebuild requirement for v4 schema"),
        Err(err) => panic!("expected rebuild requirement for v4 schema, got {err}"),
    }
    assert!(
        !db_path.exists(),
        "legacy v4 database should be removed after rebuild requirement"
    );
}

#[test]
fn open_or_rebuild_triggers_rebuild_for_future_version() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("future.db");

    // Create a database with a future schema version
    {
        let conn = open_fixture_db(&db_path);
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
        Err(MigrationError::RebuildRequired {
            reason,
            backup_path,
        }) => {
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
    let storage = SqliteStorage::open(&db_path).expect("open");

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
        .query_map_collect(
            "SELECT c.external_id FROM conversations c
             WHERE c.source_id = 'local'
             ORDER BY c.started_at DESC",
            &[],
            |row| row.get_typed(0),
        )
        .unwrap();

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
    let storage = SqliteStorage::open(&db_path).expect("open");

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
        .query_map_collect(
            "SELECT c.external_id FROM conversations c
             WHERE c.source_id != 'local'
             ORDER BY c.started_at DESC",
            &[],
            |row| row.get_typed(0),
        )
        .unwrap();

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
    let storage = SqliteStorage::open(&db_path).expect("open");

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
        .query_map_collect(
            "SELECT c.external_id FROM conversations c
             WHERE c.source_id = 'laptop'
             ORDER BY c.started_at DESC",
            &[],
            |row| row.get_typed(0),
        )
        .unwrap();

    assert_eq!(laptop_only.len(), 2, "should return 2 laptop conversations");
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
    let storage = SqliteStorage::open(&db_path).expect("open");

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
        .query_map_collect(
            "SELECT c.id, c.source_id FROM conversations c
             WHERE c.source_id IS NOT NULL",
            &[],
            |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
        )
        .unwrap();

    // Bead 7k7pl: pin the EXACT count (1). The test seeds exactly
    // one conversation; a regression that duplicated inserts or
    // leaked prior state would slip past `!is_empty()` while
    // silently skewing downstream timeline aggregates.
    assert_eq!(
        result.len(),
        1,
        "exactly one conversation expected; got {result:?}"
    );
    let (_, source_id) = &result[0];
    assert_eq!(source_id, "local", "source_id should be 'local'");
}

#[test]
fn timeline_json_includes_origin_kind_field() {
    // P7.10: Verify timeline SQL returns origin_kind from sources table
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("timeline_kind.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

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
            &sample_conv_with_source(
                "remote-conv",
                "laptop",
                now + 1000,
                vec![msg(0, now + 1000)],
            ),
        )
        .unwrap();

    // Query with origin_kind from sources table (matches timeline SQL)
    let results: Vec<(String, String, String)> = storage
        .raw()
        .query_map_collect(
            "SELECT c.source_id, c.origin_host, s.kind as origin_kind
             FROM conversations c
             LEFT JOIN sources s ON c.source_id = s.id
             ORDER BY c.source_id",
            &[],
            |row| {
                Ok((
                    row.get_typed::<String>(0)?,
                    row.get_typed::<Option<String>>(1)?.unwrap_or_default(),
                    row.get_typed::<Option<String>>(2)?
                        .unwrap_or_else(|| "local".into()),
                ))
            },
        )
        .unwrap();

    assert_eq!(results.len(), 2, "should have 2 conversations");

    // Find local and remote results
    let local = results
        .iter()
        .find(|(id, _, _)| id == "local")
        .expect("local conv");
    let remote = results
        .iter()
        .find(|(id, _, _)| id == "laptop")
        .expect("remote conv");

    // Verify origin_kind is correct
    assert_eq!(local.2, "local", "local source should have kind 'local'");
    assert_eq!(remote.2, "ssh", "remote source should have kind 'ssh'");
}

#[test]
fn timeline_json_includes_origin_host_field() {
    // P7.10: Verify timeline SQL returns origin_host for remote sessions
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("timeline_host.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

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
        .query_map_collect(
            "SELECT c.source_id, c.origin_host FROM conversations c ORDER BY c.source_id",
            &[],
            |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
        )
        .unwrap();

    assert_eq!(results.len(), 2, "should have 2 conversations");

    let local = results
        .iter()
        .find(|(id, _)| id == "local")
        .expect("local conv");
    let remote = results
        .iter()
        .find(|(id, _)| id == "work")
        .expect("remote conv");

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
    let storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(PathBuf::from("/workspace/demo").as_path(), Some("Demo"))
        .unwrap();

    storage
        .upsert_source(&Source::local())
        .expect("upsert local");
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
        .query_map_collect(
            "SELECT c.id, c.source_id, c.origin_host, s.kind as origin_kind
             FROM conversations c
             LEFT JOIN sources s ON c.source_id = s.id",
            &[],
            |row| {
                Ok((
                    row.get_typed::<i64>(0)?,
                    row.get_typed::<String>(1)?,
                    row.get_typed::<Option<String>>(2)?,
                    row.get_typed::<Option<String>>(3)?,
                ))
            },
        )
        .unwrap();

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

// =============================================================================
// Daily Stats Tests (Opt 3.2 - tst.sto.daily_stats)
// Tests for materialized time-range aggregates
// =============================================================================

#[test]
fn daily_stats_table_created_on_fresh_db() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("daily_stats.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    // Check that daily_stats table exists
    let tables: Vec<String> = storage
        .raw()
        .query_map_collect(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='daily_stats'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();

    assert_eq!(tables.len(), 1, "daily_stats table should exist");

    // Check columns
    let columns: Vec<String> = storage
        .raw()
        .query_map_collect("PRAGMA table_info(daily_stats)", &[], |r| {
            r.get_typed::<String>(1)
        })
        .unwrap();

    assert!(columns.contains(&"day_id".to_string()));
    assert!(columns.contains(&"agent_slug".to_string()));
    assert!(columns.contains(&"source_id".to_string()));
    assert!(columns.contains(&"session_count".to_string()));
    assert!(columns.contains(&"message_count".to_string()));
    assert!(columns.contains(&"total_chars".to_string()));
    assert!(columns.contains(&"last_updated".to_string()));
}

#[test]
fn daily_stats_day_id_conversion() {
    // Test day_id conversion: 2024-01-01 00:00:00 UTC = 1704067200 seconds
    // Days since 2020-01-01 (1577836800) = (1704067200 - 1577836800) / 86400 = 1461
    let ts_ms = 1704067200 * 1000; // 2024-01-01 in milliseconds
    let day_id = SqliteStorage::day_id_from_millis(ts_ms);
    assert_eq!(
        day_id, 1461,
        "2024-01-01 should be day 1461 since 2020-01-01"
    );

    // Test round-trip: day_id -> timestamp -> day_id
    let ts_back = SqliteStorage::millis_from_day_id(day_id);
    let day_id_back = SqliteStorage::day_id_from_millis(ts_back);
    assert_eq!(day_id, day_id_back, "day_id should round-trip correctly");
}

#[test]
fn daily_stats_rebuild_from_conversations() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("daily_rebuild.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    // Insert some conversations
    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(PathBuf::from("/workspace/demo").as_path(), Some("Demo"))
        .unwrap();

    // Insert 3 conversations on different days
    let base_ts = 1704067200000_i64; // 2024-01-01 00:00:00 UTC in ms
    for i in 0..3 {
        let started_at = base_ts + (i * 86400 * 1000); // Each day
        let conv = Conversation {
            id: None,
            agent_slug: "tester".into(),
            workspace: Some(PathBuf::from("/workspace/demo")),
            external_id: Some(format!("conv-{}", i)),
            title: Some(format!("Conversation {}", i)),
            source_path: PathBuf::from(format!("/logs/conv{}.jsonl", i)),
            started_at: Some(started_at),
            ended_at: Some(started_at + 3600000),
            approx_tokens: Some(100),
            metadata_json: serde_json::json!({}),
            messages: vec![msg(0, started_at)],
            source_id: "local".to_string(),
            origin_host: None,
        };
        storage
            .insert_conversation_tree(agent_id, Some(ws_id), &conv)
            .unwrap();
    }

    // Rebuild daily stats
    let result = storage.rebuild_daily_stats().expect("rebuild_daily_stats");
    assert!(result.rows_created > 0, "should create daily_stats rows");
    assert_eq!(result.total_sessions, 3, "should count 3 sessions");

    // Verify health check
    let health = storage.daily_stats_health().expect("daily_stats_health");
    assert!(health.populated, "daily_stats should be populated");
    assert_eq!(health.conversation_count, 3);
    assert_eq!(health.materialized_total, 3);
    assert_eq!(health.drift, 0, "no drift after rebuild");
}

#[test]
fn daily_stats_count_sessions_in_range() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("daily_count.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    // Insert conversations
    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let base_ts = 1704067200000_i64;

    for i in 0..5 {
        let started_at = base_ts + (i * 86400 * 1000);
        let conv = Conversation {
            id: None,
            agent_slug: "tester".into(),
            workspace: None,
            external_id: Some(format!("sess-{}", i)),
            title: None,
            source_path: PathBuf::from(format!("/logs/s{}.jsonl", i)),
            started_at: Some(started_at),
            ended_at: None,
            approx_tokens: None,
            metadata_json: serde_json::json!({}),
            messages: vec![],
            source_id: "local".to_string(),
            origin_host: None,
        };
        storage
            .insert_conversation_tree(agent_id, None, &conv)
            .unwrap();
    }

    // Rebuild stats
    storage.rebuild_daily_stats().expect("rebuild");

    // Query range: days 1-3 (should get 3 sessions)
    let start = base_ts + (86400 * 1000);
    let end = base_ts + (3 * 86400 * 1000);
    let (count, from_cache) = storage
        .count_sessions_in_range(Some(start), Some(end), None, None)
        .expect("count_sessions_in_range");

    assert!(from_cache, "should use materialized stats");
    assert_eq!(count, 3, "should count 3 sessions in range");

    // Query all time
    let (total, _) = storage
        .count_sessions_in_range(None, None, None, None)
        .expect("count all");
    assert_eq!(total, 5, "should count all 5 sessions");
}

#[test]
fn daily_stats_histogram() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("daily_hist.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let base_ts = 1704067200000_i64;

    // Insert multiple conversations on day 0 and day 2
    for i in 0..3 {
        let started_at = base_ts; // Day 0
        let conv = Conversation {
            id: None,
            agent_slug: "tester".into(),
            workspace: None,
            external_id: Some(format!("d0-{}", i)),
            title: None,
            source_path: PathBuf::from(format!("/logs/d0-{}.jsonl", i)),
            started_at: Some(started_at),
            ended_at: None,
            approx_tokens: None,
            metadata_json: serde_json::json!({}),
            messages: vec![msg(0, started_at)],
            source_id: "local".to_string(),
            origin_host: None,
        };
        storage
            .insert_conversation_tree(agent_id, None, &conv)
            .unwrap();
    }

    for i in 0..2 {
        let started_at = base_ts + (2 * 86400 * 1000); // Day 2
        let conv = Conversation {
            id: None,
            agent_slug: "tester".into(),
            workspace: None,
            external_id: Some(format!("d2-{}", i)),
            title: None,
            source_path: PathBuf::from(format!("/logs/d2-{}.jsonl", i)),
            started_at: Some(started_at),
            ended_at: None,
            approx_tokens: None,
            metadata_json: serde_json::json!({}),
            messages: vec![msg(0, started_at)],
            source_id: "local".to_string(),
            origin_host: None,
        };
        storage
            .insert_conversation_tree(agent_id, None, &conv)
            .unwrap();
    }

    storage.rebuild_daily_stats().expect("rebuild");

    // Get histogram for days 0-2
    let histogram = storage
        .get_daily_histogram(base_ts, base_ts + (2 * 86400 * 1000), None, None)
        .expect("get_daily_histogram");

    // Should have entries for day 0 and day 2 (day 1 has no sessions)
    assert!(
        histogram.len() >= 2,
        "should have at least 2 days with data"
    );

    // Find day 0 entry
    let day0_id = SqliteStorage::day_id_from_millis(base_ts);
    let day0 = histogram.iter().find(|d| d.day_id == day0_id);
    // Bead 7k7pl: collapse `.is_some()` + `.unwrap().sessions == N`
    // into a single pin that fails loudly on BOTH missing entry and
    // wrong-count regressions.
    assert_eq!(
        day0.map(|d| d.sessions),
        Some(3),
        "day 0 must exist with 3 sessions; got {day0:?}"
    );

    // Find day 2 entry
    let day2_id = SqliteStorage::day_id_from_millis(base_ts + (2 * 86400 * 1000));
    let day2 = histogram.iter().find(|d| d.day_id == day2_id);
    assert_eq!(
        day2.map(|d| d.sessions),
        Some(2),
        "day 2 must exist with 2 sessions; got {day2:?}"
    );
}

#[test]
fn daily_stats_uses_materialized_after_insert() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("daily_materialized.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let base_ts = 1704067200000_i64;

    // Insert conversation - stats are now updated inline
    let conv = Conversation {
        id: None,
        agent_slug: "tester".into(),
        workspace: None,
        external_id: Some("mat-1".to_string()),
        title: None,
        source_path: PathBuf::from("/logs/mat.jsonl"),
        started_at: Some(base_ts),
        ended_at: None,
        approx_tokens: None,
        metadata_json: serde_json::json!({}),
        messages: vec![],
        source_id: "local".to_string(),
        origin_host: None,
    };
    storage
        .insert_conversation_tree(agent_id, None, &conv)
        .unwrap();

    // daily_stats is now populated after insert, should use materialized stats
    let (count, from_cache) = storage
        .count_sessions_in_range(None, None, None, None)
        .expect("count from cache");

    assert!(from_cache, "should use materialized stats (from cache)");
    assert_eq!(count, 1, "should count 1 session via materialized stats");
}

#[test]
fn daily_stats_health_no_drift_after_inserts() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("daily_health.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let base_ts = 1704067200000_i64;

    // Insert first conversation - stats updated inline
    let conv1 = Conversation {
        id: None,
        agent_slug: "tester".into(),
        workspace: None,
        external_id: Some("health-1".to_string()),
        title: None,
        source_path: PathBuf::from("/logs/health1.jsonl"),
        started_at: Some(base_ts),
        ended_at: None,
        approx_tokens: None,
        metadata_json: serde_json::json!({}),
        messages: vec![],
        source_id: "local".to_string(),
        origin_host: None,
    };
    storage
        .insert_conversation_tree(agent_id, None, &conv1)
        .unwrap();

    // Health should show no drift after insert
    let health1 = storage.daily_stats_health().expect("health");
    assert_eq!(health1.drift, 0, "no drift after first insert");
    assert_eq!(health1.conversation_count, 1);
    assert_eq!(health1.materialized_total, 1);

    // Insert another conversation - stats also updated inline
    let conv2 = Conversation {
        id: None,
        agent_slug: "tester".into(),
        workspace: None,
        external_id: Some("health-2".to_string()),
        title: None,
        source_path: PathBuf::from("/logs/health2.jsonl"),
        started_at: Some(base_ts + 3600000),
        ended_at: None,
        approx_tokens: None,
        metadata_json: serde_json::json!({}),
        messages: vec![],
        source_id: "local".to_string(),
        origin_host: None,
    };
    storage
        .insert_conversation_tree(agent_id, None, &conv2)
        .unwrap();

    // Health should still show no drift after second insert
    let health2 = storage
        .daily_stats_health()
        .expect("health after second insert");
    assert_eq!(health2.drift, 0, "no drift after second insert");
    assert_eq!(health2.conversation_count, 2);
    assert_eq!(health2.materialized_total, 2);

    // Rebuild should be a no-op (stats are already correct)
    let rebuild_result = storage.rebuild_daily_stats().expect("rebuild");
    assert_eq!(
        rebuild_result.total_sessions, 2,
        "rebuild should count same sessions"
    );

    let health3 = storage.daily_stats_health().expect("health after rebuild");
    assert_eq!(health3.drift, 0, "still no drift after rebuild");
}

#[test]
fn daily_stats_null_timestamp_consistency() {
    // Regression test: Ensure NULL started_at timestamps are handled
    // consistently between incremental updates and full rebuilds.
    // Both should map NULL -> day_id=0 (not a large negative number).
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("daily_null_ts.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();

    // Insert conversation with NULL started_at
    let conv = Conversation {
        id: None,
        agent_slug: "tester".into(),
        workspace: None,
        external_id: Some("null-ts-1".to_string()),
        title: None,
        source_path: PathBuf::from("/logs/null_ts.jsonl"),
        started_at: None, // NULL timestamp!
        ended_at: None,
        approx_tokens: None,
        metadata_json: serde_json::json!({}),
        messages: vec![],
        source_id: "local".to_string(),
        origin_host: None,
    };
    storage
        .insert_conversation_tree(agent_id, None, &conv)
        .unwrap();

    // Rebuild daily stats
    let result = storage.rebuild_daily_stats().expect("rebuild");
    assert_eq!(result.total_sessions, 1, "should count 1 session");

    // Check that the session was placed at day_id=0, not a negative day_id
    let day_ids: Vec<i64> = storage
        .raw()
        .query_map_collect("SELECT DISTINCT day_id FROM daily_stats WHERE agent_slug = 'all' AND source_id = 'all'", &[], |r| r.get_typed(0))
        .unwrap();

    assert_eq!(day_ids.len(), 1, "should have exactly 1 day_id");
    assert_eq!(
        day_ids[0], 0,
        "NULL started_at should map to day_id=0, not negative"
    );

    // Verify the count at day_id=0
    let count_at_zero: i64 = storage
        .raw()
        .query_row_map(
            "SELECT session_count FROM daily_stats WHERE day_id = 0 AND agent_slug = 'all' AND source_id = 'all'",
            &[],
            |r| r.get_typed(0),
        )
        .expect("query day_id=0");
    assert_eq!(count_at_zero, 1, "day_id=0 should have 1 session");
}

/// Verify that insert_conversations_batched updates daily_stats correctly without rebuild.
#[test]
fn daily_stats_batched_insert_no_drift() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("batched_stats.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(&PathBuf::from("/workspace/demo"), None)
        .unwrap();
    let base_ts = 1704067200000_i64; // 2024-01-01 00:00:00 UTC

    // Create 3 conversations for batched insert
    let convs: Vec<Conversation> = (0..3)
        .map(|i| {
            let started_at = base_ts + (i * 3600000); // Spread 1 hour apart, same day
            Conversation {
                id: None,
                agent_slug: "tester".into(),
                workspace: Some(PathBuf::from("/workspace/demo")),
                external_id: Some(format!("batch-conv-{}", i)),
                title: Some(format!("Batched conversation {}", i)),
                source_path: PathBuf::from(format!("/logs/batch{}.jsonl", i)),
                started_at: Some(started_at),
                ended_at: Some(started_at + 1800000),
                approx_tokens: Some(50),
                metadata_json: serde_json::json!({}),
                messages: vec![msg(0, started_at), msg(1, started_at + 60000)],
                source_id: "local".to_string(),
                origin_host: None,
            }
        })
        .collect();

    // Build references for batched insert
    let refs: Vec<(i64, Option<i64>, &Conversation)> =
        convs.iter().map(|c| (agent_id, Some(ws_id), c)).collect();

    // Use batched insert (should update daily_stats automatically)
    let outcomes = storage
        .insert_conversations_batched(&refs)
        .expect("batched insert");
    assert_eq!(outcomes.len(), 3, "should insert 3 conversations");

    // Check daily_stats health WITHOUT calling rebuild_daily_stats
    let health = storage.daily_stats_health().expect("daily_stats_health");
    assert!(
        health.populated,
        "daily_stats should be populated after batched insert"
    );
    assert_eq!(health.conversation_count, 3, "should have 3 conversations");
    assert_eq!(
        health.materialized_total, 3,
        "should have 3 sessions in materialized stats"
    );
    assert_eq!(
        health.drift, 0,
        "should have NO drift after batched insert (stats updated inline)"
    );
}

/// Verify that insert_conversation_tree updates daily_stats correctly (fixed path).
#[test]
fn daily_stats_tree_insert_no_drift() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("tree_stats.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let base_ts = 1704067200000_i64;

    // Insert using insert_conversation_tree path
    for i in 0..3 {
        let started_at = base_ts + (i * 3600000);
        let conv = Conversation {
            id: None,
            agent_slug: "tester".into(),
            workspace: None,
            external_id: Some(format!("tree-conv-{}", i)),
            title: None,
            source_path: PathBuf::from(format!("/logs/tree{}.jsonl", i)),
            started_at: Some(started_at),
            ended_at: None,
            approx_tokens: None,
            metadata_json: serde_json::json!({}),
            messages: vec![msg(0, started_at)],
            source_id: "local".to_string(),
            origin_host: None,
        };
        storage
            .insert_conversation_tree(agent_id, None, &conv)
            .unwrap();
    }

    // Check daily_stats health WITHOUT calling rebuild
    let health = storage.daily_stats_health().expect("daily_stats_health");
    assert_eq!(health.conversation_count, 3, "should have 3 conversations");
    assert_eq!(
        health.materialized_total, 3,
        "should have 3 sessions in materialized stats"
    );
    assert_eq!(
        health.drift, 0,
        "should have NO drift after insert (stats updated inline)"
    );
}

// =============================================================================
// SQLite ID Caching Equivalence Tests (16pz / Opt 7.3)
// =============================================================================
// These tests verify that IndexingCache produces identical database state
// compared to direct ensure_* calls. The cache is an optimization that should
// not change observable behavior.

use coding_agent_search::storage::sqlite::IndexingCache;

/// Helper to dump database state for comparison.
#[allow(clippy::type_complexity)]
fn dump_agent_workspace_state(storage: &SqliteStorage) -> (Vec<(i64, String)>, Vec<(i64, String)>) {
    let agents: Vec<(i64, String)> = storage
        .raw()
        .query_map_collect("SELECT id, slug FROM agents ORDER BY slug", &[], |r| {
            Ok((r.get_typed(0)?, r.get_typed(1)?))
        })
        .unwrap();

    let workspaces: Vec<(i64, String)> = storage
        .raw()
        .query_map_collect("SELECT id, path FROM workspaces ORDER BY path", &[], |r| {
            Ok((r.get_typed(0)?, r.get_typed(1)?))
        })
        .unwrap();

    (agents, workspaces)
}

/// Test that cached agent lookups return the same ID as direct ensure_agent calls.
#[test]
fn cache_agent_id_consistency() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("cache_agent.db");
    let storage = SqliteStorage::open(&db_path).expect("open");
    let mut cache = IndexingCache::new();

    let agent = Agent {
        id: None,
        slug: "claude_code".into(),
        name: "Claude Code".into(),
        version: Some("1.0".into()),
        kind: AgentKind::Cli,
    };

    // First lookup - should be a miss (goes to DB)
    let id1 = cache.get_or_insert_agent(&storage, &agent).unwrap();

    // Second lookup - should be a hit (from cache)
    let id2 = cache.get_or_insert_agent(&storage, &agent).unwrap();

    // Direct DB lookup should match
    let id3 = storage.ensure_agent(&agent).unwrap();

    assert_eq!(id1, id2, "cached lookups should return same ID");
    assert_eq!(id1, id3, "cached ID should match direct DB lookup");
    assert!(id1 > 0, "ID should be positive");
}

/// Test that cached workspace lookups return the same ID as direct ensure_workspace calls.
#[test]
fn cache_workspace_id_consistency() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("cache_workspace.db");
    let storage = SqliteStorage::open(&db_path).expect("open");
    let mut cache = IndexingCache::new();

    let path = std::path::Path::new("/home/user/projects/myapp");

    // First lookup - miss
    let id1 = cache
        .get_or_insert_workspace(&storage, path, Some("My App"))
        .unwrap();

    // Second lookup - hit
    let id2 = cache
        .get_or_insert_workspace(&storage, path, Some("My App"))
        .unwrap();

    // Direct DB lookup
    let id3 = storage.ensure_workspace(path, Some("My App")).unwrap();

    assert_eq!(id1, id2, "cached lookups should return same ID");
    assert_eq!(id1, id3, "cached ID should match direct DB lookup");
    assert!(id1 > 0, "ID should be positive");
}

/// Test cache hit/miss statistics are tracked correctly.
#[test]
fn cache_statistics_tracking() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("cache_stats.db");
    let storage = SqliteStorage::open(&db_path).expect("open");
    let mut cache = IndexingCache::new();

    // Initial stats should be zero
    let (hits, misses, hit_rate) = cache.stats();
    assert_eq!(hits, 0);
    assert_eq!(misses, 0);
    assert_eq!(hit_rate, 0.0);

    let agents = ["codex", "claude_code", "cline"];
    let workspaces = ["/ws/a", "/ws/b"];

    // First round - all misses
    for slug in &agents {
        let agent = Agent {
            id: None,
            slug: (*slug).into(),
            name: (*slug).into(),
            version: None,
            kind: AgentKind::Cli,
        };
        cache.get_or_insert_agent(&storage, &agent).unwrap();
    }
    for ws in &workspaces {
        cache
            .get_or_insert_workspace(&storage, std::path::Path::new(ws), None)
            .unwrap();
    }

    let (hits, misses, _) = cache.stats();
    assert_eq!(misses, 5, "5 unique lookups = 5 misses");
    assert_eq!(hits, 0, "no hits on first round");

    // Second round - all hits
    for slug in &agents {
        let agent = Agent {
            id: None,
            slug: (*slug).into(),
            name: (*slug).into(),
            version: None,
            kind: AgentKind::Cli,
        };
        cache.get_or_insert_agent(&storage, &agent).unwrap();
    }
    for ws in &workspaces {
        cache
            .get_or_insert_workspace(&storage, std::path::Path::new(ws), None)
            .unwrap();
    }

    let (hits, misses, hit_rate) = cache.stats();
    assert_eq!(hits, 5, "5 repeated lookups = 5 hits");
    assert_eq!(misses, 5, "misses unchanged");
    assert!((hit_rate - 0.5).abs() < 0.01, "50% hit rate");
}

/// Test that cache.clear() resets all state.
#[test]
fn cache_clear_resets_state() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("cache_clear.db");
    let storage = SqliteStorage::open(&db_path).expect("open");
    let mut cache = IndexingCache::new();

    let agent = Agent {
        id: None,
        slug: "test_agent".into(),
        name: "Test".into(),
        version: None,
        kind: AgentKind::Cli,
    };

    // Populate cache
    cache.get_or_insert_agent(&storage, &agent).unwrap();
    cache
        .get_or_insert_workspace(&storage, std::path::Path::new("/ws"), None)
        .unwrap();

    assert_eq!(cache.agent_count(), 1);
    assert_eq!(cache.workspace_count(), 1);
    let (_, misses, _) = cache.stats();
    assert_eq!(misses, 2);

    // Clear cache
    cache.clear();

    assert_eq!(cache.agent_count(), 0, "agents cleared");
    assert_eq!(cache.workspace_count(), 0, "workspaces cleared");
    let (hits, misses, _) = cache.stats();
    assert_eq!(hits, 0, "hits reset");
    assert_eq!(misses, 0, "misses reset");

    // After clear, next lookup is a miss again
    cache.get_or_insert_agent(&storage, &agent).unwrap();
    let (hits, misses, _) = cache.stats();
    assert_eq!(hits, 0);
    assert_eq!(misses, 1, "lookup after clear is a miss");
}

/// Test that multiple unique agents/workspaces are all cached correctly.
#[test]
fn cache_multiple_unique_entries() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("cache_multi.db");
    let storage = SqliteStorage::open(&db_path).expect("open");
    let mut cache = IndexingCache::new();

    let agent_slugs: Vec<String> = (0..20).map(|i| format!("agent_{}", i)).collect();
    let workspace_paths: Vec<String> = (0..15).map(|i| format!("/workspace/{}", i)).collect();

    // Insert all agents
    let mut agent_ids: Vec<i64> = Vec::new();
    for slug in &agent_slugs {
        let agent = Agent {
            id: None,
            slug: slug.clone(),
            name: slug.clone(),
            version: None,
            kind: AgentKind::Cli,
        };
        let id = cache.get_or_insert_agent(&storage, &agent).unwrap();
        agent_ids.push(id);
    }

    // Insert all workspaces
    let mut workspace_ids: Vec<i64> = Vec::new();
    for ws in &workspace_paths {
        let id = cache
            .get_or_insert_workspace(&storage, std::path::Path::new(ws), None)
            .unwrap();
        workspace_ids.push(id);
    }

    // Verify counts
    assert_eq!(cache.agent_count(), 20);
    assert_eq!(cache.workspace_count(), 15);

    // Verify IDs are unique
    let unique_agent_ids: std::collections::HashSet<_> = agent_ids.iter().collect();
    let unique_ws_ids: std::collections::HashSet<_> = workspace_ids.iter().collect();
    assert_eq!(unique_agent_ids.len(), 20, "all agent IDs unique");
    assert_eq!(unique_ws_ids.len(), 15, "all workspace IDs unique");

    // Verify cache hit on second lookup
    for (i, slug) in agent_slugs.iter().enumerate() {
        let agent = Agent {
            id: None,
            slug: slug.clone(),
            name: slug.clone(),
            version: None,
            kind: AgentKind::Cli,
        };
        let id = cache.get_or_insert_agent(&storage, &agent).unwrap();
        assert_eq!(id, agent_ids[i], "cached ID matches original");
    }

    let (hits, misses, _) = cache.stats();
    assert_eq!(misses, 35, "35 unique entries = 35 misses");
    assert_eq!(hits, 20, "20 agent re-lookups = 20 hits");
}

/// Test CASS_SQLITE_CACHE environment variable control.
#[test]
fn cache_env_var_control() {
    // Default: cache enabled
    assert!(
        IndexingCache::is_enabled(),
        "cache should be enabled by default"
    );

    // With CASS_SQLITE_CACHE=0, cache is disabled
    unsafe { std::env::set_var("CASS_SQLITE_CACHE", "0") };
    assert!(
        !IndexingCache::is_enabled(),
        "cache should be disabled with CASS_SQLITE_CACHE=0"
    );

    // With CASS_SQLITE_CACHE=false, cache is disabled
    unsafe { std::env::set_var("CASS_SQLITE_CACHE", "false") };
    assert!(
        !IndexingCache::is_enabled(),
        "cache should be disabled with CASS_SQLITE_CACHE=false"
    );

    // With CASS_SQLITE_CACHE=1, cache is enabled
    unsafe { std::env::set_var("CASS_SQLITE_CACHE", "1") };
    assert!(
        IndexingCache::is_enabled(),
        "cache should be enabled with CASS_SQLITE_CACHE=1"
    );

    // Cleanup
    unsafe { std::env::remove_var("CASS_SQLITE_CACHE") };
}

/// Stress test: large corpus with many unique agents/workspaces.
/// Verifies cache produces identical state to direct calls.
#[test]
fn cache_stress_test_large_corpus() {
    let tmp = tempfile::TempDir::new().unwrap();

    // Test with cache enabled
    let db_cached = tmp.path().join("cached.db");
    let storage_cached = SqliteStorage::open(&db_cached).expect("open cached");
    let mut cache = IndexingCache::new();

    // Test without cache (direct calls)
    let db_direct = tmp.path().join("direct.db");
    let storage_direct = SqliteStorage::open(&db_direct).expect("open direct");

    // Generate test data: 100 conversations across 10 agents and 50 workspaces
    let agents: Vec<String> = (0..10).map(|i| format!("agent_{}", i)).collect();
    let workspaces: Vec<String> = (0..50)
        .map(|i| format!("/workspace/project_{}", i))
        .collect();

    // Insert with cache
    for i in 0..100 {
        let slug = &agents[i % agents.len()];
        let ws = &workspaces[i % workspaces.len()];

        let agent = Agent {
            id: None,
            slug: slug.clone(),
            name: slug.clone(),
            version: None,
            kind: AgentKind::Cli,
        };

        cache.get_or_insert_agent(&storage_cached, &agent).unwrap();
        cache
            .get_or_insert_workspace(&storage_cached, std::path::Path::new(ws), None)
            .unwrap();
    }

    // Insert without cache (direct calls)
    for i in 0..100 {
        let slug = &agents[i % agents.len()];
        let ws = &workspaces[i % workspaces.len()];

        let agent = Agent {
            id: None,
            slug: slug.clone(),
            name: slug.clone(),
            version: None,
            kind: AgentKind::Cli,
        };

        storage_direct.ensure_agent(&agent).unwrap();
        storage_direct
            .ensure_workspace(std::path::Path::new(ws), None)
            .unwrap();
    }

    // Compare database states
    let (cached_agents, cached_workspaces) = dump_agent_workspace_state(&storage_cached);
    let (direct_agents, direct_workspaces) = dump_agent_workspace_state(&storage_direct);

    // Agent slugs should match (IDs may differ due to insertion order timing)
    let cached_slugs: Vec<_> = cached_agents.iter().map(|(_, s)| s.clone()).collect();
    let direct_slugs: Vec<_> = direct_agents.iter().map(|(_, s)| s.clone()).collect();
    assert_eq!(cached_slugs, direct_slugs, "agent slugs should match");
    assert_eq!(cached_slugs.len(), 10, "should have 10 unique agents");

    // Workspace paths should match
    let cached_paths: Vec<_> = cached_workspaces.iter().map(|(_, p)| p.clone()).collect();
    let direct_paths: Vec<_> = direct_workspaces.iter().map(|(_, p)| p.clone()).collect();
    assert_eq!(cached_paths, direct_paths, "workspace paths should match");
    assert_eq!(cached_paths.len(), 50, "should have 50 unique workspaces");

    // Verify cache statistics
    let (hits, misses, hit_rate) = cache.stats();
    assert_eq!(misses, 60, "10 agents + 50 workspaces = 60 misses");
    assert_eq!(
        hits, 140,
        "100 iterations - 60 unique = 140 hits from repeats"
    );
    assert!(hit_rate > 0.6, "hit rate should be >60%");
}

/// Test that IDs are stable across multiple indexing runs.
#[test]
fn cache_id_stability_across_runs() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("stability.db");

    let agent = Agent {
        id: None,
        slug: "stable_agent".into(),
        name: "Stable Agent".into(),
        version: None,
        kind: AgentKind::Cli,
    };
    let ws_path = std::path::Path::new("/stable/workspace");

    // First run
    let agent_id_1;
    let ws_id_1;
    {
        let storage = SqliteStorage::open(&db_path).expect("open");
        let mut cache = IndexingCache::new();
        agent_id_1 = cache.get_or_insert_agent(&storage, &agent).unwrap();
        ws_id_1 = cache
            .get_or_insert_workspace(&storage, ws_path, Some("Stable WS"))
            .unwrap();
    }

    // Second run (new cache, same DB)
    let agent_id_2;
    let ws_id_2;
    {
        let storage = SqliteStorage::open(&db_path).expect("reopen");
        let mut cache = IndexingCache::new();
        agent_id_2 = cache.get_or_insert_agent(&storage, &agent).unwrap();
        ws_id_2 = cache
            .get_or_insert_workspace(&storage, ws_path, Some("Stable WS"))
            .unwrap();
    }

    assert_eq!(agent_id_1, agent_id_2, "agent ID stable across runs");
    assert_eq!(ws_id_1, ws_id_2, "workspace ID stable across runs");
}

// =============================================================================
// SQLite ID Caching Benchmark Tests (1tmi / Opt 7.4)
// =============================================================================
// These tests measure the performance improvement from IndexingCache.

/// Benchmark: measure time for cached vs direct ID lookups.
/// This test verifies that caching provides significant speedup.
#[test]
fn cache_benchmark_speedup() {
    use std::time::Instant;

    let tmp = tempfile::TempDir::new().unwrap();
    let iterations = 500;
    let agents: Vec<String> = (0..10).map(|i| format!("agent_{}", i)).collect();
    let workspaces: Vec<String> = (0..20)
        .map(|i| format!("/workspace/project_{}", i))
        .collect();

    // Benchmark with cache
    let db_cached = tmp.path().join("bench_cached.db");
    let storage_cached = SqliteStorage::open(&db_cached).expect("open");
    let mut cache = IndexingCache::new();

    let start_cached = Instant::now();
    for i in 0..iterations {
        let slug = &agents[i % agents.len()];
        let ws = &workspaces[i % workspaces.len()];

        let agent = Agent {
            id: None,
            slug: slug.clone(),
            name: slug.clone(),
            version: None,
            kind: AgentKind::Cli,
        };

        cache.get_or_insert_agent(&storage_cached, &agent).unwrap();
        cache
            .get_or_insert_workspace(&storage_cached, std::path::Path::new(ws), None)
            .unwrap();
    }
    let elapsed_cached = start_cached.elapsed();

    // Benchmark without cache (direct DB calls)
    let db_direct = tmp.path().join("bench_direct.db");
    let storage_direct = SqliteStorage::open(&db_direct).expect("open");

    let start_direct = Instant::now();
    for i in 0..iterations {
        let slug = &agents[i % agents.len()];
        let ws = &workspaces[i % workspaces.len()];

        let agent = Agent {
            id: None,
            slug: slug.clone(),
            name: slug.clone(),
            version: None,
            kind: AgentKind::Cli,
        };

        storage_direct.ensure_agent(&agent).unwrap();
        storage_direct
            .ensure_workspace(std::path::Path::new(ws), None)
            .unwrap();
    }
    let elapsed_direct = start_direct.elapsed();

    // Log results for manual verification
    let cached_ms = elapsed_cached.as_secs_f64() * 1000.0;
    let direct_ms = elapsed_direct.as_secs_f64() * 1000.0;
    let speedup = direct_ms / cached_ms;

    println!("\n[Opt 7.4] SQLite ID Caching Benchmark Results:");
    println!("  Iterations: {iterations}");
    println!(
        "  Agents: {}, Workspaces: {}",
        agents.len(),
        workspaces.len()
    );
    println!(
        "  Cached:  {:.2}ms ({:.4}ms/iter)",
        cached_ms,
        cached_ms / iterations as f64
    );
    println!(
        "  Direct:  {:.2}ms ({:.4}ms/iter)",
        direct_ms,
        direct_ms / iterations as f64
    );
    println!("  Speedup: {:.1}x", speedup);

    // Verify cache stats
    let (hits, misses, hit_rate) = cache.stats();
    println!(
        "  Cache hits: {}, misses: {}, hit_rate: {:.1}%",
        hits,
        misses,
        hit_rate * 100.0
    );

    // Assertions: cache should provide speedup
    assert!(
        speedup > 1.0,
        "cached path should be faster than direct (speedup: {:.2}x)",
        speedup
    );

    // With 500 iterations over 10 agents and 20 workspaces, expect high hit rate
    // First 30 are misses (10 agents + 20 workspaces), rest are hits
    assert!(
        hit_rate > 0.85,
        "expected >85% hit rate, got {:.1}%",
        hit_rate * 100.0
    );
}

/// Test that cache hit ratio meets expected targets for real-world patterns.
/// From the task description:
/// - Expected: >90% hit ratio for agent_ids
/// - Expected: >80% hit ratio for workspace_ids
#[test]
fn cache_hit_ratio_targets() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("hit_ratio.db");
    let storage = SqliteStorage::open(&db_path).expect("open");

    // Simulate real-world indexing: 500 conversations from 5 agents across 30 workspaces
    let agents: Vec<String> = (0..5).map(|i| format!("agent_{}", i)).collect();
    let workspaces: Vec<String> = (0..30)
        .map(|i| format!("/workspace/project_{}", i))
        .collect();

    let mut agent_cache = IndexingCache::new();
    let mut workspace_cache = IndexingCache::new();

    for i in 0..500 {
        let slug = &agents[i % agents.len()];
        let ws = &workspaces[i % workspaces.len()];

        let agent = Agent {
            id: None,
            slug: slug.clone(),
            name: slug.clone(),
            version: None,
            kind: AgentKind::Cli,
        };

        agent_cache.get_or_insert_agent(&storage, &agent).unwrap();
        workspace_cache
            .get_or_insert_workspace(&storage, std::path::Path::new(ws), None)
            .unwrap();
    }

    let (agent_hits, agent_misses, agent_rate) = agent_cache.stats();
    let (ws_hits, ws_misses, ws_rate) = workspace_cache.stats();

    println!("\n[Opt 7.4] Cache Hit Ratio Analysis:");
    println!(
        "  Agent cache: {} hits, {} misses, {:.1}% hit rate",
        agent_hits,
        agent_misses,
        agent_rate * 100.0
    );
    println!(
        "  Workspace cache: {} hits, {} misses, {:.1}% hit rate",
        ws_hits,
        ws_misses,
        ws_rate * 100.0
    );

    // Verify targets from task description
    // Agent: 500 lookups, 5 unique = 495 hits, 5 misses = 99% hit rate
    assert!(
        agent_rate > 0.90,
        "Expected >90% agent hit ratio, got {:.1}%",
        agent_rate * 100.0
    );

    // Workspace: 500 lookups, 30 unique = 470 hits, 30 misses = 94% hit rate
    assert!(
        ws_rate > 0.80,
        "Expected >80% workspace hit ratio, got {:.1}%",
        ws_rate * 100.0
    );

    // With these specific numbers, we can compute exact expected values
    assert_eq!(
        agent_misses, 5,
        "should have 5 agent misses (unique agents)"
    );
    assert_eq!(
        ws_misses, 30,
        "should have 30 workspace misses (unique workspaces)"
    );
}

/// Large-scale benchmark: 3000+ conversations (as specified in task).
/// This simulates the benchmark scenario from the task description.
#[test]
fn cache_benchmark_large_corpus() {
    use std::time::Instant;

    let tmp = tempfile::TempDir::new().unwrap();

    // Parameters from task: generate_corpus(3000)
    let corpus_size = 3000;
    let agent_count = 15; // Realistic variety
    let workspace_count = 100; // Many workspaces

    let agents: Vec<String> = (0..agent_count).map(|i| format!("agent_{}", i)).collect();
    let workspaces: Vec<String> = (0..workspace_count)
        .map(|i| format!("/workspace/project_{}", i))
        .collect();

    // With cache
    let db_cached = tmp.path().join("large_cached.db");
    let storage_cached = SqliteStorage::open(&db_cached).expect("open");
    let mut cache = IndexingCache::new();

    let start_cached = Instant::now();
    for i in 0..corpus_size {
        let slug = &agents[i % agents.len()];
        let ws = &workspaces[i % workspaces.len()];

        let agent = Agent {
            id: None,
            slug: slug.clone(),
            name: slug.clone(),
            version: None,
            kind: AgentKind::Cli,
        };

        cache.get_or_insert_agent(&storage_cached, &agent).unwrap();
        cache
            .get_or_insert_workspace(&storage_cached, std::path::Path::new(ws), None)
            .unwrap();
    }
    let elapsed_cached = start_cached.elapsed();

    // Without cache
    let db_direct = tmp.path().join("large_direct.db");
    let storage_direct = SqliteStorage::open(&db_direct).expect("open");

    let start_direct = Instant::now();
    for i in 0..corpus_size {
        let slug = &agents[i % agents.len()];
        let ws = &workspaces[i % workspaces.len()];

        let agent = Agent {
            id: None,
            slug: slug.clone(),
            name: slug.clone(),
            version: None,
            kind: AgentKind::Cli,
        };

        storage_direct.ensure_agent(&agent).unwrap();
        storage_direct
            .ensure_workspace(std::path::Path::new(ws), None)
            .unwrap();
    }
    let elapsed_direct = start_direct.elapsed();

    let (hits, misses, hit_rate) = cache.stats();
    let cached_ms = elapsed_cached.as_secs_f64() * 1000.0;
    let direct_ms = elapsed_direct.as_secs_f64() * 1000.0;
    let speedup = direct_ms / cached_ms;

    println!(
        "\n[Opt 7.4] Large Corpus Benchmark ({} conversations):",
        corpus_size
    );
    println!("  Agents: {}, Workspaces: {}", agent_count, workspace_count);
    println!(
        "  Cached:  {:.2}ms total, {:.4}ms/conv",
        cached_ms,
        cached_ms / corpus_size as f64
    );
    println!(
        "  Direct:  {:.2}ms total, {:.4}ms/conv",
        direct_ms,
        direct_ms / corpus_size as f64
    );
    println!("  Speedup: {:.1}x", speedup);
    println!(
        "  Cache: {} hits, {} misses, {:.1}% hit rate",
        hits,
        misses,
        hit_rate * 100.0
    );

    // Success criteria from task: indexing time reduction
    assert!(
        speedup > 1.5,
        "Expected >1.5x speedup for large corpus, got {:.2}x",
        speedup
    );

    // Expected misses: agent_count + workspace_count = 115
    // Expected hits: (corpus_size * 2) - 115 = 5885
    assert_eq!(
        misses,
        (agent_count + workspace_count) as u64,
        "misses should equal unique entries"
    );
}

/// Verify no memory overhead concerns (cache is small).
#[test]
fn cache_memory_overhead_acceptable() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("memory.db");
    let storage = SqliteStorage::open(&db_path).expect("open");
    let mut cache = IndexingCache::new();

    // Populate with reasonable upper bound: 100 agents, 1000 workspaces
    for i in 0..100 {
        let agent = Agent {
            id: None,
            slug: format!("agent_{}", i),
            name: format!("Agent {}", i),
            version: None,
            kind: AgentKind::Cli,
        };
        cache.get_or_insert_agent(&storage, &agent).unwrap();
    }

    for i in 0..1000 {
        cache
            .get_or_insert_workspace(
                &storage,
                std::path::Path::new(&format!("/workspace/project_{}", i)),
                None,
            )
            .unwrap();
    }

    // Verify cache contains expected counts
    assert_eq!(cache.agent_count(), 100);
    assert_eq!(cache.workspace_count(), 1000);

    // Cache size estimation:
    // - 100 agents: ~100 * (slug ~20 bytes + id 8 bytes) ≈ 2.8 KB
    // - 1000 workspaces: ~1000 * (path ~40 bytes + id 8 bytes) ≈ 48 KB
    // Total: ~50 KB - well under any reasonable memory budget
    //
    // We can't directly measure memory, but we verify the counts are as expected
    // and the operations complete without issues.
    println!("\n[Opt 7.4] Memory overhead check:");
    println!("  Cached agents: {}", cache.agent_count());
    println!("  Cached workspaces: {}", cache.workspace_count());
    println!("  Estimated cache size: ~50KB (100 agents + 1000 workspaces)");
}
