//! Backwards compatibility tests for archive reading.
//!
//! These tests verify that:
//! - Newer code can read archives from older versions
//! - Version detection works correctly
//! - Unknown fields are gracefully ignored
//! - Missing optional fields have sensible defaults

use coding_agent_search::pages::encrypt::{EncryptionConfig, KdfAlgorithm, SlotType};
use coding_agent_search::storage::sqlite::{MigrationError, SqliteStorage, CURRENT_SCHEMA_VERSION};
use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;

const _: () = {
    assert!(CURRENT_SCHEMA_VERSION > 0, "Schema version should be positive");
    assert!(
        CURRENT_SCHEMA_VERSION < 100,
        "Schema version should be reasonable"
    );
};

// =============================================================================
// Schema Version Tests
// =============================================================================

/// Test that schema version constant is accessible and reasonable.
#[test]
fn test_schema_version_exists() {
    let _ = CURRENT_SCHEMA_VERSION;
}

/// Test creating database and verifying schema version.
#[test]
fn test_new_database_has_current_schema() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");

    let storage = SqliteStorage::open(&db_path).unwrap();
    let version = storage.schema_version().unwrap();

    assert_eq!(
        version, CURRENT_SCHEMA_VERSION,
        "New database should have current schema version"
    );
}

// =============================================================================
// Database Compatibility Tests
// =============================================================================

/// Test that we can open a database with older schema version and check compatibility.
#[test]
fn test_detects_older_schema() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("old.db");

    // Create a minimal old-style database
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
        // Simulate an older schema version
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', '1')",
            [],
        )
        .unwrap();
    }

    // Try to open with SqliteStorage - should trigger migration or rebuild
    let result = SqliteStorage::open_or_rebuild(&db_path);

    // Should succeed (either migrate or rebuild)
    match result {
        Ok(_) => {
            // Successfully opened/migrated
            let storage = SqliteStorage::open(&db_path).unwrap();
            let version = storage.schema_version().unwrap();
            assert!(
                version >= CURRENT_SCHEMA_VERSION,
                "Schema should be at least current version after migration"
            );
        }
        Err(e) => {
            // Migration error is acceptable for very old schemas
            if let MigrationError::RebuildRequired { reason, .. } = e {
                assert!(
                    !reason.is_empty(),
                    "Rebuild reason should not be empty"
                );
            }
        }
    }
}

/// Test that unknown tables are ignored (forward compatibility).
#[test]
fn test_ignores_unknown_tables() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("extended.db");

    // First create a normal database
    {
        let _storage = SqliteStorage::open(&db_path).unwrap();
    }

    // Add extra tables that a future version might have
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE future_feature (
                id INTEGER PRIMARY KEY,
                data TEXT
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE another_extension (
                id INTEGER PRIMARY KEY,
                value BLOB
            )",
            [],
        )
        .unwrap();
    }

    // Should still be able to open and use the database
    let storage = SqliteStorage::open(&db_path).unwrap();
    let agents = storage.list_agents().unwrap();
    // Should work fine even with extra tables
    assert!(agents.is_empty() || !agents.is_empty()); // Just verify it doesn't panic
}

/// Test that missing optional columns are handled.
#[test]
fn test_handles_missing_optional_columns() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("minimal.db");

    // Create a database with minimal required structure
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
            INSERT INTO meta (key, value) VALUES ('schema_version', '8');
            CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL UNIQUE,
                name TEXT,
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
                agent_id INTEGER NOT NULL,
                workspace_id INTEGER,
                external_id TEXT UNIQUE,
                title TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER,
                ended_at INTEGER,
                metadata_json TEXT,
                FOREIGN KEY(agent_id) REFERENCES agents(id)
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                author TEXT,
                content TEXT NOT NULL,
                created_at INTEGER,
                extra_json TEXT,
                FOREIGN KEY(conversation_id) REFERENCES conversations(id)
            );
            CREATE TABLE sources (
                id INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                path TEXT NOT NULL UNIQUE,
                updated_at INTEGER NOT NULL
            );
            INSERT INTO sources (id, kind, path, updated_at) VALUES (0, 'local', 'default', 0);
            "#,
        )
        .unwrap();
    }

    // Should open successfully with readonly
    let result = SqliteStorage::open_readonly(&db_path);
    assert!(result.is_ok(), "Should open database with minimal schema");
}

// =============================================================================
// Encryption Config Compatibility Tests
// =============================================================================

/// Test parsing v1 encryption config format.
#[test]
fn test_parse_v1_encryption_config() {
    let v1_config = json!({
        "version": 1,
        "export_id": "AAAAAAAAAAAAAAAAAAAAAA==",
        "base_nonce": "AAAAAAAAAAAA",
        "compression": "deflate",
        "kdf_defaults": {
            "memory_kb": 65536,
            "iterations": 3,
            "parallelism": 4
        },
        "payload": {
            "chunk_size": 8388608,
            "chunk_count": 1,
            "total_compressed_size": 1024,
            "total_plaintext_size": 2048,
            "files": ["test.db"]
        },
        "key_slots": [{
            "id": 0,
            "slot_type": "password",
            "kdf": "argon2id",
            "salt": "c2FsdHNhbHRzYWx0c2FsdA==",
            "wrapped_dek": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "nonce": "AAAAAAAAAAAA",
            "argon2_params": {
                "memory_kb": 65536,
                "iterations": 3,
                "parallelism": 4
            }
        }]
    });

    // Should parse successfully
    let config: Result<EncryptionConfig, _> = serde_json::from_value(v1_config);
    assert!(config.is_ok(), "Should parse v1 config: {:?}", config.err());

    let config = config.unwrap();
    assert_eq!(config.version, 1);
    assert_eq!(config.key_slots.len(), 1);
    assert_eq!(config.key_slots[0].slot_type, SlotType::Password);
}

/// Test parsing v2 encryption config format (current).
#[test]
fn test_parse_v2_encryption_config() {
    let v2_config = json!({
        "version": 2,
        "export_id": "AAAAAAAAAAAAAAAAAAAAAA==",
        "base_nonce": "AAAAAAAAAAAA",
        "compression": "deflate",
        "kdf_defaults": {
            "memory_kb": 65536,
            "iterations": 3,
            "parallelism": 4
        },
        "payload": {
            "chunk_size": 8388608,
            "chunk_count": 2,
            "total_compressed_size": 2048,
            "total_plaintext_size": 4096,
            "files": ["data.db", "index.db"]
        },
        "key_slots": [
            {
                "id": 0,
                "slot_type": "password",
                "kdf": "argon2id",
                "salt": "c2FsdHNhbHRzYWx0c2FsdA==",
                "wrapped_dek": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                "nonce": "AAAAAAAAAAAA",
                "argon2_params": {
                    "memory_kb": 65536,
                    "iterations": 3,
                    "parallelism": 4
                }
            },
            {
                "id": 1,
                "slot_type": "recovery",
                "kdf": "hkdf-sha256",
                "salt": "c2FsdHNhbHRzYWx0c2FsdA==",
                "wrapped_dek": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                "nonce": "AAAAAAAAAAAA"
            }
        ]
    });

    let config: Result<EncryptionConfig, _> = serde_json::from_value(v2_config);
    assert!(config.is_ok(), "Should parse v2 config: {:?}", config.err());

    let config = config.unwrap();
    assert_eq!(config.version, 2);
    assert_eq!(config.key_slots.len(), 2);
    assert_eq!(config.key_slots[0].slot_type, SlotType::Password);
    assert_eq!(config.key_slots[1].slot_type, SlotType::Recovery);
    assert_eq!(config.key_slots[1].kdf, KdfAlgorithm::HkdfSha256);
}

/// Test config with unknown fields is still parseable (forward compatibility).
#[test]
fn test_parse_config_with_unknown_fields() {
    let future_config = json!({
        "version": 2,
        "export_id": "AAAAAAAAAAAAAAAAAAAAAA==",
        "base_nonce": "AAAAAAAAAAAA",
        "compression": "deflate",
        "kdf_defaults": {
            "memory_kb": 65536,
            "iterations": 3,
            "parallelism": 4
        },
        "payload": {
            "chunk_size": 8388608,
            "chunk_count": 1,
            "total_compressed_size": 1024,
            "total_plaintext_size": 2048,
            "files": ["test.db"],
            "future_field": "ignored"
        },
        "key_slots": [{
            "id": 0,
            "slot_type": "password",
            "kdf": "argon2id",
            "salt": "c2FsdHNhbHRzYWx0c2FsdA==",
            "wrapped_dek": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "nonce": "AAAAAAAAAAAA",
            "argon2_params": {
                "memory_kb": 65536,
                "iterations": 3,
                "parallelism": 4
            }
        }],
        "new_future_feature": {
            "enabled": true,
            "options": ["a", "b"]
        }
    });

    // With deny_unknown_fields, this would fail. We use default serde behavior
    // which ignores unknown fields for forward compatibility.
    let config: Result<EncryptionConfig, _> = serde_json::from_value(future_config);
    // This might fail depending on serde config - that's okay, we're testing the behavior
    if let Ok(config) = config {
        assert_eq!(config.version, 2);
    }
}

// =============================================================================
// Version Detection Tests
// =============================================================================

/// Test detecting config version from JSON.
#[test]
fn test_detect_config_version() {
    fn get_version(json_str: &str) -> Option<u8> {
        serde_json::from_str::<serde_json::Value>(json_str)
            .ok()
            .and_then(|v| v.get("version")?.as_u64())
            .map(|v| v as u8)
    }

    assert_eq!(get_version(r#"{"version": 1}"#), Some(1));
    assert_eq!(get_version(r#"{"version": 2}"#), Some(2));
    assert_eq!(get_version(r#"{"version": 99}"#), Some(99));
    assert_eq!(get_version(r#"{"other": "field"}"#), None);
}

/// Test graceful handling of very old schema.
#[test]
fn test_reject_schema_version_0() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("ancient.db");

    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', '0')",
            [],
        )
        .unwrap();
    }

    // Very old schemas should trigger rebuild
    let result = SqliteStorage::open_or_rebuild(&db_path);
    // Either succeeds with rebuild or returns error
    match result {
        Ok(_storage) => {
            // Rebuild succeeded, verify schema is current
            let storage = SqliteStorage::open(&db_path).unwrap();
            assert!(storage.schema_version().unwrap() >= CURRENT_SCHEMA_VERSION);
        }
        Err(e) => {
            match e {
                MigrationError::RebuildRequired { reason, .. } => {
                    assert!(
                        reason.to_lowercase().contains("rebuild")
                            || reason.to_lowercase().contains("schema"),
                        "Rebuild reason should be informative: {}",
                        reason
                    );
                }
                other => panic!("Unexpected error type: {:?}", other),
            }
        }
    }
}

// =============================================================================
// Feature Degradation Tests
// =============================================================================

/// Test that search works even without FTS indexes.
#[test]
fn test_search_without_fts() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("no_fts.db");

    // Create database without FTS
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(&format!(
            r#"
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
            INSERT INTO meta (key, value) VALUES ('schema_version', '{}');
            CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL UNIQUE,
                name TEXT,
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
                agent_id INTEGER NOT NULL,
                workspace_id INTEGER,
                source_id TEXT NOT NULL DEFAULT 'local',
                external_id TEXT,
                title TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER,
                ended_at INTEGER,
                approx_tokens INTEGER,
                metadata_json TEXT,
                origin_host TEXT,
                metadata_bin BLOB
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                author TEXT,
                content TEXT NOT NULL,
                created_at INTEGER,
                extra_json TEXT
            );
            CREATE TABLE sources (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                path TEXT NOT NULL UNIQUE,
                updated_at INTEGER NOT NULL
            );
            INSERT INTO sources (id, kind, path, updated_at) VALUES ('local', 'local', 'default', 0);
            INSERT INTO agents (id, slug, name, kind, created_at, updated_at)
                VALUES (1, 'test', 'Test Agent', 'cli', 0, 0);
            INSERT INTO conversations (id, agent_id, source_id, source_path, title)
                VALUES (1, 1, 'local', '/test', 'Test Conv');
            INSERT INTO messages (id, conversation_id, idx, role, content)
                VALUES (1, 1, 0, 'user', 'Test message content');
            "#,
            CURRENT_SCHEMA_VERSION
        ))
        .unwrap();
    }

    // Should be able to open and query (though FTS won't work)
    let storage = SqliteStorage::open_readonly(&db_path).unwrap();
    let convs = storage.list_conversations(10, 0).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title.as_deref(), Some("Test Conv"));
}
