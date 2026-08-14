use coding_agent_search::franken_sync::Connection;
use coding_agent_search::storage::sqlite::{MigrationError, SqliteStorage};
use std::path::Path;
use tempfile::TempDir;

// Helper to create a V1 database with some data
fn create_v1_db(path: &Path) {
    let conn = Connection::open(path.to_string_lossy().as_ref()).expect("create v1 db");
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

        -- Insert sample data
        INSERT INTO agents(slug, name, kind, created_at, updated_at)
        VALUES ('claude', 'Claude', 'cli', 1000, 1000);

        INSERT INTO conversations(agent_id, source_path, title, started_at)
        VALUES (1, '/logs/v1.jsonl', 'V1 Conversation', 2000);

        INSERT INTO messages(conversation_id, idx, role, content, created_at)
        VALUES (1, 0, 'user', 'Hello from V1', 2000);
        ",
    )
    .expect("setup v1 schema/data");
}

#[test]
fn test_migration_v1_requires_rebuild() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("v1_to_curr.db");

    create_v1_db(&db_path);

    match SqliteStorage::open_or_rebuild(&db_path) {
        Err(MigrationError::RebuildRequired {
            reason,
            backup_path,
        }) => {
            assert!(reason.contains("too old for in-place migration"));

            let backup_path = backup_path.expect("legacy db should be backed up");
            assert!(backup_path.exists());
            assert!(!db_path.exists());
        }
        Ok(_) => panic!("expected rebuild-required result for V1 schema, got Ok(_)"),
        Err(err) => panic!("expected rebuild-required result for V1 schema, got {err}"),
    }
}

#[test]
fn test_rebuild_safety_on_corruption() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("corrupt.db");

    // Create a corrupted file
    std::fs::write(&db_path, "Not a SQLite file").unwrap();

    // open_or_rebuild should fail with RebuildRequired
    let result = SqliteStorage::open_or_rebuild(&db_path);

    match result {
        Err(MigrationError::RebuildRequired {
            reason,
            backup_path,
        }) => {
            println!("Rebuild required as expected: {}", reason);
            assert!(backup_path.is_some());
            let backup = backup_path.unwrap();
            assert!(backup.exists());

            // Verify backup contains original corrupted data
            let content = std::fs::read_to_string(&backup).unwrap();
            assert_eq!(content, "Not a SQLite file");

            // The original file should be gone (or replaced? logic says remove_database_files called)
            assert!(!db_path.exists());

            // Now we can "rebuild" by opening fresh
            let _new_storage = SqliteStorage::open(&db_path).expect("open fresh");
            assert!(db_path.exists());
        }
        _ => panic!("Should have required rebuild"),
    }
}

#[test]
fn test_missing_meta_triggers_rebuild() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("no_meta.db");

    // Create a valid SQLite DB but without meta table (simulating very old or broken state)
    {
        let conn = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();
        conn.execute("CREATE TABLE some_table (id INTEGER)")
            .unwrap();
    }

    let result = SqliteStorage::open_or_rebuild(&db_path);
    match result {
        Err(MigrationError::RebuildRequired { reason, .. }) => {
            assert!(reason.contains("metadata"));
        }
        _ => panic!("Should have required rebuild due to missing meta"),
    }
}

#[test]
fn test_future_schema_triggers_rebuild() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("future.db");

    {
        let conn = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();
        conn.execute("CREATE TABLE meta (key TEXT, value TEXT)")
            .unwrap();
        conn.execute("INSERT INTO meta VALUES ('schema_version', '9999')")
            .unwrap();
    }

    let result = SqliteStorage::open_or_rebuild(&db_path);
    match result {
        Err(MigrationError::RebuildRequired { reason, .. }) => {
            assert!(reason.contains("newer than supported"));
        }
        _ => panic!("Should have required rebuild due to future version"),
    }
}
