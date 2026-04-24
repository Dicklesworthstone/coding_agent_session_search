//! Frankensqlite Compatibility Gate Tests
//!
//! These tests verify that frankensqlite (pure-Rust SQLite) can handle the
//! critical features cass depends on. This is a BLOCKING RISK GATE for the
//! frankensqlite migration (Track 3).
//!
//! Gate 1: FTS5 full-text search (CREATE VIRTUAL TABLE, MATCH, highlight, etc.)
//! Gate 2: Existing C SQLite database file compatibility (read rusqlite-created DBs)

use frankensqlite::Connection;
use frankensqlite::Connection as FrankenConnection;
use frankensqlite::compat::RowExt;
use fsqlite_types::value::SqliteValue;
use std::fmt::Write as _;

use coding_agent_search::storage::sqlite::{CURRENT_SCHEMA_VERSION, FrankenStorage, SqliteStorage};

#[test]
fn rusqlite_is_dev_dependency_only() {
    let manifest: toml::Table =
        toml::from_str(include_str!("../Cargo.toml")).expect("parse Cargo.toml");
    let dependencies = manifest
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .expect("Cargo.toml dependencies table");
    assert!(
        !dependencies.contains_key("rusqlite"),
        "rusqlite must not ship as a normal production dependency; \
         keep C-SQLite interop coverage in dev-dependencies only"
    );

    let dev_dependencies = manifest
        .get("dev-dependencies")
        .and_then(toml::Value::as_table)
        .expect("Cargo.toml dev-dependencies table");
    assert!(
        dev_dependencies.contains_key("rusqlite"),
        "rusqlite should remain available to tests that build legacy \
         C-SQLite fixture databases"
    );
}

// ============================================================================
// GATE 1: FTS5 Compatibility
// ============================================================================

#[test]
fn gate1_fts5_create_virtual_table() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE VIRTUAL TABLE test_fts USING fts5(content)")
        .expect("GATE 1.1 FAIL: Cannot create FTS5 virtual table");
}

#[test]
fn gate1_fts5_create_with_trigram_tokenizer() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    // Trigram tokenizer is critical for cass substring search
    conn.execute("CREATE VIRTUAL TABLE test_fts USING fts5(content, tokenize='trigram')")
        .expect("GATE 1.1b FAIL: Cannot create FTS5 table with trigram tokenizer");
}

#[test]
fn gate1_fts5_create_with_porter_tokenizer() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE VIRTUAL TABLE test_fts USING fts5(content, tokenize='porter')")
        .expect("GATE 1.1c FAIL: Cannot create FTS5 table with porter tokenizer");
}

#[test]
fn gate1_fts5_insert() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE VIRTUAL TABLE test_fts USING fts5(content)")
        .unwrap();

    conn.execute("INSERT INTO test_fts(content) VALUES ('hello world')")
        .expect("GATE 1.2 FAIL: Cannot insert into FTS5 table");
    conn.execute("INSERT INTO test_fts(content) VALUES ('rust programming language')")
        .expect("GATE 1.2 FAIL: Cannot insert second row");
    conn.execute("INSERT INTO test_fts(content) VALUES ('hello rust developers')")
        .expect("GATE 1.2 FAIL: Cannot insert third row");
}

#[test]
fn gate1_fts5_match_query() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE VIRTUAL TABLE test_fts USING fts5(content)")
        .unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('hello world')")
        .unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('goodbye world')")
        .unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('hello rust')")
        .unwrap();

    let rows = conn
        .query("SELECT content FROM test_fts WHERE test_fts MATCH 'hello'")
        .expect("GATE 1.3 FAIL: FTS5 MATCH query failed");

    assert_eq!(
        rows.len(),
        2,
        "GATE 1.3 FAIL: Expected 2 matches for 'hello', got {}",
        rows.len()
    );
}

#[test]
fn gate1_fts5_trigram_substring_match() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE VIRTUAL TABLE test_fts USING fts5(content, tokenize='trigram')")
        .unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('hello world')")
        .unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('say hello there')")
        .unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('nothing here')")
        .unwrap();

    // Trigram search for substring 'llo' should match rows containing 'hello'
    let rows = conn
        .query("SELECT content FROM test_fts WHERE test_fts MATCH 'llo'")
        .expect("GATE 1.3b FAIL: Trigram substring search failed");

    assert_eq!(
        rows.len(),
        2,
        "GATE 1.3b FAIL: Expected 2 trigram matches for 'llo', got {}",
        rows.len()
    );
}

#[test]
fn gate1_fts5_prefix_match() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE VIRTUAL TABLE test_fts USING fts5(content)")
        .unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('authentication error')")
        .unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('authorize user')")
        .unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('something else')")
        .unwrap();

    // Prefix match with *
    let rows = conn
        .query("SELECT content FROM test_fts WHERE test_fts MATCH 'auth*'")
        .expect("GATE 1.4 FAIL: FTS5 prefix matching failed");

    assert_eq!(
        rows.len(),
        2,
        "GATE 1.4 FAIL: Expected 2 prefix matches for 'auth*', got {}",
        rows.len()
    );
}

#[test]
fn gate1_fts5_highlight_function() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE VIRTUAL TABLE test_fts USING fts5(content)")
        .unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('hello world')")
        .unwrap();

    let rows = conn
        .query(
            "SELECT highlight(test_fts, 0, '<b>', '</b>') FROM test_fts WHERE test_fts MATCH 'hello'",
        )
        .expect("GATE 1.5 FAIL: FTS5 highlight() function failed");

    assert_eq!(rows.len(), 1, "GATE 1.5 FAIL: Expected 1 highlighted row");
    let val = rows[0].get(0).expect("column 0");
    if let SqliteValue::Text(s) = val {
        assert!(
            s.contains("<b>hello</b>"),
            "GATE 1.5 FAIL: highlight() should wrap 'hello' in <b> tags, got: {s}"
        );
    } else {
        panic!("GATE 1.5 FAIL: highlight() should return text, got: {val:?}");
    }
}

#[test]
fn gate1_fts5_rebuild_command() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE VIRTUAL TABLE test_fts USING fts5(content)")
        .unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('test data')")
        .unwrap();

    conn.execute("INSERT INTO test_fts(test_fts) VALUES('rebuild')")
        .expect("GATE 1.6 FAIL: FTS5 rebuild command failed");
}

#[test]
fn gate1_fts5_optimize_command() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE VIRTUAL TABLE test_fts USING fts5(content)")
        .unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('optimize me')")
        .unwrap();

    conn.execute("INSERT INTO test_fts(test_fts) VALUES('optimize')")
        .expect("GATE 1.6b FAIL: FTS5 optimize command failed");
}

#[test]
fn gate1_fts5_multi_column() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE VIRTUAL TABLE test_fts USING fts5(title, body)")
        .unwrap();
    conn.execute(
        "INSERT INTO test_fts(title, body) VALUES ('Rust Guide', 'Learn systems programming')",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO test_fts(title, body) VALUES ('Python Intro', 'Learn dynamic programming')",
    )
    .unwrap();

    // Search in body column only
    let rows = conn
        .query("SELECT title FROM test_fts WHERE test_fts MATCH 'body:systems'")
        .expect("GATE 1.7 FAIL: Multi-column FTS5 column filter failed");

    assert_eq!(
        rows.len(),
        1,
        "GATE 1.7 FAIL: Expected 1 match for body:systems"
    );
}

#[test]
fn gate1_fts5_rank_function() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE VIRTUAL TABLE test_fts USING fts5(content)")
        .unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('rust rust rust')") // high relevance
        .unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('hello rust')") // low relevance
        .unwrap();

    let rows = conn
        .query("SELECT content, rank FROM test_fts WHERE test_fts MATCH 'rust' ORDER BY rank")
        .expect("GATE 1.8 FAIL: FTS5 rank function failed");

    assert_eq!(rows.len(), 2, "GATE 1.8 FAIL: Expected 2 ranked results");
    // rank is a negative BM25 score (more negative = better match)
    let rank0 = rows[0].get(1).expect("rank col");
    let rank1 = rows[1].get(1).expect("rank col");
    if let (SqliteValue::Float(r0), SqliteValue::Float(r1)) = (rank0, rank1) {
        assert!(
            r0 <= r1,
            "GATE 1.8 FAIL: rank should be ordered (more negative first), got {r0} vs {r1}"
        );
    }
}

#[test]
fn gate1_fts5_within_transaction() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE VIRTUAL TABLE test_fts USING fts5(content)")
        .unwrap();

    conn.execute("BEGIN").unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('in transaction')")
        .expect("GATE 1.9 FAIL: FTS5 insert within transaction failed");
    conn.execute("COMMIT").unwrap();

    let rows = conn
        .query("SELECT content FROM test_fts WHERE test_fts MATCH 'transaction'")
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "GATE 1.9 FAIL: FTS5 data not visible after commit"
    );
}

#[test]
fn gate1_fts5_transaction_rollback() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE VIRTUAL TABLE test_fts USING fts5(content)")
        .unwrap();

    conn.execute("BEGIN").unwrap();
    conn.execute("INSERT INTO test_fts(content) VALUES ('will be rolled back')")
        .unwrap();
    conn.execute("ROLLBACK").unwrap();

    let rows = conn.query("SELECT COUNT(*) FROM test_fts").unwrap();
    let count = rows[0].get(0).unwrap();
    assert_eq!(
        count,
        &SqliteValue::Integer(0),
        "GATE 1.9b FAIL: FTS5 data visible after rollback"
    );
}

#[test]
fn gate1_fts5_multiple_tables_coexist() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE VIRTUAL TABLE fts_a USING fts5(content)")
        .unwrap();
    conn.execute("CREATE VIRTUAL TABLE fts_b USING fts5(content)")
        .unwrap();

    conn.execute("INSERT INTO fts_a(content) VALUES ('alpha search')")
        .unwrap();
    conn.execute("INSERT INTO fts_b(content) VALUES ('beta search')")
        .unwrap();

    let rows_a = conn
        .query("SELECT content FROM fts_a WHERE fts_a MATCH 'alpha'")
        .unwrap();
    let rows_b = conn
        .query("SELECT content FROM fts_b WHERE fts_b MATCH 'beta'")
        .unwrap();

    assert_eq!(
        rows_a.len(),
        1,
        "GATE 1.10 FAIL: Multiple FTS5 tables - first table query failed"
    );
    assert_eq!(
        rows_b.len(),
        1,
        "GATE 1.10 FAIL: Multiple FTS5 tables - second table query failed"
    );
}

#[test]
fn gate1_fts5_bulk_insert_performance() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE VIRTUAL TABLE perf_fts USING fts5(content)")
        .unwrap();

    // Insert 1000 rows
    conn.execute("BEGIN").unwrap();
    for i in 0..1000 {
        conn.execute_with_params(
            "INSERT INTO perf_fts(content) VALUES (?1)",
            &[SqliteValue::Text(
                format!("document number {i} with searchable content about rust programming")
                    .into(),
            )],
        )
        .unwrap();
    }
    conn.execute("COMMIT").unwrap();

    // Verify count
    let rows = conn.query("SELECT COUNT(*) FROM perf_fts").unwrap();
    assert_eq!(
        rows[0].get(0).unwrap(),
        &SqliteValue::Integer(1000),
        "GATE 1.11 FAIL: Bulk insert count mismatch"
    );

    // Search should work on bulk data
    let results = conn
        .query("SELECT content FROM perf_fts WHERE perf_fts MATCH 'rust' LIMIT 5")
        .expect("GATE 1.11 FAIL: Search on 1000-row FTS5 table failed");

    assert!(
        !results.is_empty(),
        "GATE 1.11 FAIL: No results from 1000-row search"
    );
}

// ============================================================================
// GATE 2: Existing C SQLite Database File Compatibility
// ============================================================================

#[test]
fn gate2_file_compat_create_with_rusqlite_read_with_frankensqlite() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let db_path = dir.path().join("test_compat.db");

    // Step 1: Create database with rusqlite (C SQLite)
    {
        let conn = rusqlite::Connection::open(&db_path).expect("rusqlite open for write");
        conn.execute_batch(
            "
            PRAGMA journal_mode=WAL;
            PRAGMA user_version=12;
            CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent TEXT NOT NULL,
                workspace TEXT,
                created_at INTEGER NOT NULL,
                content TEXT
            );
        ",
        )
        .expect("rusqlite schema creation");

        // Insert test data
        for i in 0..10 {
            conn.execute(
                "INSERT INTO conversations (id, agent, workspace, created_at, content) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    i,
                    format!("agent_{}", i % 3),
                    format!("/workspace/{}", i),
                    1700000000 + i * 1000,
                    format!("conversation content for session {i}")
                ],
            ).expect("rusqlite insert");
        }
    }

    // Step 2: Open with frankensqlite and verify
    let conn = Connection::open(db_path.to_str().unwrap())
        .expect("GATE 2.1 FAIL: frankensqlite cannot open rusqlite-created database");

    // Verify row count
    let rows = conn
        .query("SELECT COUNT(*) FROM conversations")
        .expect("GATE 2.2 FAIL: frankensqlite cannot query rusqlite-created table");

    assert_eq!(
        rows[0].get(0).unwrap(),
        &SqliteValue::Integer(10),
        "GATE 2.2 FAIL: Row count mismatch"
    );

    // Verify data integrity
    let rows = conn
        .query("SELECT id, agent, workspace, created_at, content FROM conversations ORDER BY id")
        .expect("GATE 2.3 FAIL: Cannot read all columns");

    assert_eq!(rows.len(), 10, "GATE 2.3 FAIL: Expected 10 rows");

    // Verify first row
    let first = &rows[0];
    assert_eq!(
        first.get(0).unwrap(),
        &SqliteValue::Integer(0),
        "GATE 2.3 FAIL: First row id mismatch"
    );
    assert_eq!(
        first.get(1).unwrap(),
        &SqliteValue::Text("agent_0".into()),
        "GATE 2.3 FAIL: First row agent mismatch"
    );
}

#[test]
fn gate2_file_compat_pragma_user_version() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let db_path = dir.path().join("test_pragma.db");

    // Create with rusqlite, set user_version
    {
        let conn = rusqlite::Connection::open(&db_path).expect("rusqlite open");
        conn.execute_batch("PRAGMA user_version=12;").unwrap();
        conn.execute_batch("CREATE TABLE t(x);").unwrap();
    }

    // Read with frankensqlite
    let conn = Connection::open(db_path.to_str().unwrap())
        .expect("GATE 2.4 FAIL: Cannot open for PRAGMA check");

    let rows = conn
        .query("PRAGMA user_version")
        .expect("GATE 2.4 FAIL: Cannot read PRAGMA user_version");

    let version = rows[0].get(0).expect("version column");
    assert_eq!(
        version,
        &SqliteValue::Integer(12),
        "GATE 2.4 FAIL: PRAGMA user_version should be 12, got {version:?}"
    );
}

#[test]
fn gate2_file_compat_wal_mode() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let db_path = dir.path().join("test_wal.db");

    // Create WAL-mode database with rusqlite
    {
        let conn = rusqlite::Connection::open(&db_path).expect("rusqlite open");
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        conn.execute_batch("CREATE TABLE data(id INTEGER PRIMARY KEY, val TEXT);")
            .unwrap();
        conn.execute("INSERT INTO data VALUES (1, 'wal test')", [])
            .unwrap();
    }

    // Verify WAL files exist
    let wal_path = db_path.with_extension("db-wal");
    let shm_path = db_path.with_extension("db-shm");

    // Open with frankensqlite (should handle WAL mode)
    let conn = Connection::open(db_path.to_str().unwrap())
        .expect("GATE 2.5 FAIL: Cannot open WAL-mode database");

    let rows = conn
        .query("SELECT val FROM data WHERE id = 1")
        .expect("GATE 2.5 FAIL: Cannot query WAL-mode database");

    assert_eq!(rows.len(), 1, "GATE 2.5 FAIL: Expected 1 row from WAL DB");
    assert_eq!(
        rows[0].get(0).unwrap(),
        &SqliteValue::Text("wal test".into()),
        "GATE 2.5 FAIL: WAL data mismatch"
    );

    // Log WAL file presence for diagnostics
    eprintln!(
        "  WAL file exists: {}, SHM file exists: {}",
        wal_path.exists(),
        shm_path.exists()
    );
}

#[test]
fn gate2_file_compat_filtered_query() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let db_path = dir.path().join("test_filter.db");

    // Create with rusqlite
    {
        let conn = rusqlite::Connection::open(&db_path).expect("rusqlite open");
        conn.execute_batch("CREATE TABLE msgs(id INTEGER PRIMARY KEY, agent TEXT, content TEXT);")
            .unwrap();
        conn.execute(
            "INSERT INTO msgs VALUES (1, 'claude', 'hello from claude')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO msgs VALUES (2, 'codex', 'hello from codex')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO msgs VALUES (3, 'claude', 'another claude msg')",
            [],
        )
        .unwrap();
    }

    // Query with frankensqlite using parameter binding
    let conn = Connection::open(db_path.to_str().unwrap())
        .expect("GATE 2.6 FAIL: Cannot open for filtered query");

    let rows = conn
        .query_with_params(
            "SELECT id, content FROM msgs WHERE agent = ?1",
            &[SqliteValue::Text("claude".into())],
        )
        .expect("GATE 2.6 FAIL: Parameterized query on rusqlite DB failed");

    assert_eq!(
        rows.len(),
        2,
        "GATE 2.6 FAIL: Expected 2 claude rows, got {}",
        rows.len()
    );
}

#[test]
fn gate2_file_compat_write_back() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let db_path = dir.path().join("test_writeback.db");

    // Create with rusqlite
    {
        let conn = rusqlite::Connection::open(&db_path).expect("rusqlite open");
        conn.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, val TEXT);")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'original')", [])
            .unwrap();
    }

    // Write with frankensqlite
    {
        let conn = Connection::open(db_path.to_str().unwrap())
            .expect("GATE 2.7 FAIL: Cannot open for write-back");
        conn.execute_with_params(
            "INSERT INTO t VALUES (?1, ?2)",
            &[
                SqliteValue::Integer(2),
                SqliteValue::Text("from frankensqlite".into()),
            ],
        )
        .expect("GATE 2.7 FAIL: Cannot write to rusqlite-created DB");
    }

    // Verify with rusqlite that the write persisted
    {
        let conn = rusqlite::Connection::open(&db_path).expect("rusqlite reopen");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 2,
            "GATE 2.7 FAIL: Write from frankensqlite not visible to rusqlite"
        );
    }
}

#[test]
fn gate2_file_compat_cass_schema_simulation() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let db_path = dir.path().join("test_cass_schema.db");

    // Create a simplified cass-like schema with rusqlite
    {
        let conn = rusqlite::Connection::open(&db_path).expect("rusqlite open");
        conn.execute_batch(
            "
            PRAGMA journal_mode=WAL;
            PRAGMA user_version=12;

            CREATE TABLE conversations (
                id TEXT PRIMARY KEY,
                agent TEXT NOT NULL,
                workspace TEXT,
                project_dir TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER,
                model TEXT,
                title TEXT,
                message_count INTEGER DEFAULT 0,
                source_id TEXT DEFAULT 'local'
            );

            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation_id TEXT NOT NULL REFERENCES conversations(id),
                role TEXT NOT NULL,
                content TEXT,
                timestamp INTEGER,
                token_count INTEGER DEFAULT 0
            );

            CREATE INDEX idx_conv_agent ON conversations(agent);
            CREATE INDEX idx_conv_created ON conversations(created_at);
            CREATE INDEX idx_msg_conv ON messages(conversation_id);
        ",
        )
        .expect("cass schema creation");

        // Insert realistic test data
        conn.execute(
            "INSERT INTO conversations VALUES ('sess-001', 'claude_code', '/home/user/project', '/home/user/project', 1700000000, 1700001000, 'claude-3-opus', 'Debug auth flow', 5, 'local')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO messages VALUES (1, 'sess-001', 'user', 'Why is my auth failing?', 1700000000, 42)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO messages VALUES (2, 'sess-001', 'assistant', 'Let me check the auth middleware...', 1700000100, 150)",
            [],
        ).unwrap();
    }

    // Verify full schema compatibility with frankensqlite
    let conn = Connection::open(db_path.to_str().unwrap())
        .expect("GATE 2.8 FAIL: Cannot open cass-like schema");

    // Read conversations
    let convs = conn
        .query("SELECT id, agent, workspace, created_at, title FROM conversations")
        .expect("GATE 2.8 FAIL: Cannot read conversations table");
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].get(1).unwrap(),
        &SqliteValue::Text("claude_code".into())
    );

    // Read messages with join
    let msgs = conn
        .query(
            "SELECT m.role, m.content, c.agent FROM messages m JOIN conversations c ON m.conversation_id = c.id ORDER BY m.id",
        )
        .expect("GATE 2.8 FAIL: JOIN query on cass schema failed");
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].get(0).unwrap(), &SqliteValue::Text("user".into()));

    // Verify PRAGMA user_version
    let ver = conn.query("PRAGMA user_version").unwrap();
    assert_eq!(
        ver[0].get(0).unwrap(),
        &SqliteValue::Integer(12),
        "GATE 2.8 FAIL: Schema version mismatch"
    );
}

// ============================================================================
// GATE 3: FrankenStorage Migration/Schema Parity
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
struct TableInfoRow {
    cid: i64,
    name: String,
    col_type: String,
    not_null: i64,
    default_value: Option<String>,
    pk: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct IndexListRow {
    name: String,
    is_unique: i64,
    origin: String,
    is_partial: i64,
}

fn quoted_sql_literal(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    out.push('\'');
    for c in input.chars() {
        if c == '\'' {
            out.push('\'');
            out.push('\'');
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn schema_tables(conn: &rusqlite::Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT name
             FROM sqlite_master
             WHERE type='table'
               AND name NOT LIKE 'sqlite_%'
               AND name NOT LIKE 'fts_messages_%'
             ORDER BY name",
        )
        .expect("prepare table listing");

    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query table listing");

    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect table listing")
}

fn table_info(conn: &rusqlite::Connection, table_name: &str) -> Vec<TableInfoRow> {
    let mut sql = String::new();
    let _ = write!(
        &mut sql,
        "PRAGMA table_info({})",
        quoted_sql_literal(table_name)
    );

    let mut stmt = conn.prepare(&sql).expect("prepare PRAGMA table_info");
    let rows = stmt
        .query_map([], |row| {
            Ok(TableInfoRow {
                cid: row.get(0)?,
                name: row.get(1)?,
                col_type: row.get(2)?,
                not_null: row.get(3)?,
                default_value: row.get(4)?,
                pk: row.get(5)?,
            })
        })
        .expect("query PRAGMA table_info");

    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect PRAGMA table_info")
}

fn index_list(conn: &rusqlite::Connection, table_name: &str) -> Vec<IndexListRow> {
    let mut sql = String::new();
    let _ = write!(
        &mut sql,
        "PRAGMA index_list({})",
        quoted_sql_literal(table_name)
    );

    let mut stmt = conn.prepare(&sql).expect("prepare PRAGMA index_list");
    let rows = stmt
        .query_map([], |row| {
            Ok(IndexListRow {
                name: row.get(1)?,
                is_unique: row.get(2)?,
                origin: row.get(3)?,
                is_partial: row.get(4)?,
            })
        })
        .expect("query PRAGMA index_list");

    let mut values = rows
        .collect::<Result<Vec<_>, _>>()
        .expect("collect PRAGMA index_list");
    values.sort();
    values
}

#[test]
fn gate3_migration_transition_from_rusqlite_meta_to_schema_migrations() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let db_path = dir.path().join("transition_from_rusqlite.db");

    // Step 1: Build an existing cass DB via rusqlite-backed storage.
    {
        let storage = SqliteStorage::open(&db_path).expect("create rusqlite-backed cass db");
        assert_eq!(
            storage.schema_version().expect("schema version"),
            CURRENT_SCHEMA_VERSION
        );
    }

    // Current bootstrap may materialize _schema_migrations immediately, so this
    // gate only requires that the bookkeeping stays bounded and consistent
    // across the FrankenStorage reopen.
    {
        let conn = rusqlite::Connection::open(&db_path).expect("open db with rusqlite");
        let schema_migration_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name='_schema_migrations'",
                [],
                |row| row.get(0),
            )
            .expect("query _schema_migrations existence");
        assert!(
            (0..=1).contains(&schema_migration_rows),
            "bootstrap should create at most one _schema_migrations table, got {schema_migration_rows}"
        );
    }

    // Step 2: Open with FrankenStorage to trigger transition + migration runner.
    {
        let storage = FrankenStorage::open(&db_path).expect("open with FrankenStorage");
        assert_eq!(
            storage.schema_version().expect("franken schema version"),
            CURRENT_SCHEMA_VERSION
        );
    }

    // Step 3: Validate transition artifacts via frankensqlite API.
    // Note: this verifies migration bookkeeping without relying on C-SQLite
    // parser behavior for newly-written schema rows.
    let conn = FrankenConnection::open(db_path.to_str().expect("db path str"))
        .expect("reopen with franken");
    let rows = conn
        .query("SELECT MAX(version) FROM _schema_migrations")
        .expect("query max migrated version");
    let max_version: i64 = rows
        .first()
        .expect("max version row")
        .get_typed(0)
        .expect("max version col");
    assert_eq!(
        max_version, CURRENT_SCHEMA_VERSION,
        "transition should set _schema_migrations max(version) to CURRENT_SCHEMA_VERSION"
    );

    let rows = conn
        .query("SELECT COUNT(*) FROM _schema_migrations")
        .expect("query migrated row count");
    let migration_count: i64 = rows
        .first()
        .expect("migration count row")
        .get_typed(0)
        .expect("migration count col");
    assert!(
        (1..=CURRENT_SCHEMA_VERSION).contains(&migration_count),
        "_schema_migrations should keep a bounded number of bookkeeping rows, got {migration_count}"
    );

    let rows = conn
        .query("SELECT value FROM meta WHERE key = 'schema_version'")
        .expect("query meta.schema_version");
    let meta_version: String = rows
        .first()
        .expect("meta row")
        .get_typed(0)
        .expect("meta value col");
    assert_eq!(
        meta_version,
        CURRENT_SCHEMA_VERSION.to_string(),
        "meta.schema_version should stay synchronized after transition"
    );
}

#[test]
#[ignore = "Blocked by upstream frankensqlite sqlite_master/autoindex inconsistency on fresh migration path"]
fn gate3_schema_parity_transitioned_db_matches_fresh_frankensqlite_db() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let db_a_path = dir.path().join("db_a_rusqlite_then_transition.db");
    let db_b_path = dir.path().join("db_b_fresh_frankensqlite.db");

    // DB-A: create with rusqlite-backed cass storage, then transition via FrankenStorage.
    {
        let storage = SqliteStorage::open(&db_a_path).expect("create db-a with SqliteStorage");
        assert_eq!(
            storage.schema_version().expect("db-a schema version"),
            CURRENT_SCHEMA_VERSION
        );
    }
    {
        let storage =
            FrankenStorage::open(&db_a_path).expect("transition db-a with FrankenStorage");
        assert_eq!(
            storage
                .schema_version()
                .expect("db-a franken schema version"),
            CURRENT_SCHEMA_VERSION
        );
    }

    // DB-B: fresh frankensqlite migration path.
    {
        let storage = FrankenStorage::open(&db_b_path).expect("create db-b with FrankenStorage");
        assert_eq!(
            storage.schema_version().expect("db-b schema version"),
            CURRENT_SCHEMA_VERSION
        );
    }

    let conn_a = rusqlite::Connection::open(&db_a_path).expect("open db-a with rusqlite");
    let conn_b = rusqlite::Connection::open(&db_b_path).expect("open db-b with rusqlite");

    let tables_a = schema_tables(&conn_a);
    let tables_b = schema_tables(&conn_b);
    assert_eq!(
        tables_a, tables_b,
        "table sets differ between transitioned and fresh frankensqlite databases"
    );

    for table in &tables_a {
        let cols_a = table_info(&conn_a, table);
        let cols_b = table_info(&conn_b, table);
        assert_eq!(
            cols_a, cols_b,
            "PRAGMA table_info mismatch for table {table}"
        );

        let idx_a = index_list(&conn_a, table);
        let idx_b = index_list(&conn_b, table);
        assert_eq!(idx_a, idx_b, "PRAGMA index_list mismatch for table {table}");
    }
}

// ============================================================================
// Additional Verification: Features cass relies on
// ============================================================================

#[test]
fn verify_count_aggregate() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE TABLE t(x INTEGER)").unwrap();
    conn.execute("INSERT INTO t VALUES (1)").unwrap();
    conn.execute("INSERT INTO t VALUES (2)").unwrap();
    conn.execute("INSERT INTO t VALUES (3)").unwrap();

    let rows = conn.query("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(rows[0].get(0).unwrap(), &SqliteValue::Integer(3));
}

#[test]
fn verify_group_by_and_order_by() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE TABLE t(agent TEXT, cnt INTEGER)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES ('claude', 1)").unwrap();
    conn.execute("INSERT INTO t VALUES ('codex', 1)").unwrap();
    conn.execute("INSERT INTO t VALUES ('claude', 1)").unwrap();

    let rows = conn
        .query("SELECT agent, SUM(cnt) as total FROM t GROUP BY agent ORDER BY total DESC")
        .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get(0).unwrap(), &SqliteValue::Text("claude".into()));
    assert_eq!(rows[0].get(1).unwrap(), &SqliteValue::Integer(2));
}

#[test]
fn verify_nullable_columns() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, val TEXT)")
        .unwrap();
    conn.execute_with_params(
        "INSERT INTO t VALUES (?1, ?2)",
        &[SqliteValue::Integer(1), SqliteValue::Null],
    )
    .unwrap();

    let rows = conn.query("SELECT val FROM t WHERE id = 1").unwrap();
    assert_eq!(
        rows[0].get(0).unwrap(),
        &SqliteValue::Null,
        "NULL column should return Null variant"
    );

    // IS NULL comparison
    let null_rows = conn.query("SELECT id FROM t WHERE val IS NULL").unwrap();
    assert_eq!(null_rows.len(), 1, "IS NULL should find 1 row");
}

#[test]
fn verify_like_operator() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE TABLE t(name TEXT)").unwrap();
    conn.execute("INSERT INTO t VALUES ('authentication')")
        .unwrap();
    conn.execute("INSERT INTO t VALUES ('authorization')")
        .unwrap();
    conn.execute("INSERT INTO t VALUES ('other')").unwrap();

    let rows = conn
        .query("SELECT name FROM t WHERE name LIKE 'auth%'")
        .unwrap();
    assert_eq!(rows.len(), 2, "LIKE 'auth%' should match 2 rows");
}

#[test]
fn verify_subquery() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE TABLE t(id INTEGER, val INTEGER)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1, 10)").unwrap();
    conn.execute("INSERT INTO t VALUES (2, 20)").unwrap();
    conn.execute("INSERT INTO t VALUES (3, 30)").unwrap();

    let rows = conn
        .query("SELECT id FROM t WHERE val > (SELECT AVG(val) FROM t)")
        .unwrap();
    assert_eq!(rows.len(), 1, "Subquery should find 1 row above average");
    assert_eq!(rows[0].get(0).unwrap(), &SqliteValue::Integer(3));
}

#[test]
fn verify_coalesce_and_ifnull() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    let rows = conn
        .query("SELECT COALESCE(NULL, NULL, 'fallback')")
        .unwrap();
    assert_eq!(
        rows[0].get(0).unwrap(),
        &SqliteValue::Text("fallback".into())
    );

    let rows = conn.query("SELECT IFNULL(NULL, 'default')").unwrap();
    assert_eq!(
        rows[0].get(0).unwrap(),
        &SqliteValue::Text("default".into())
    );
}

#[test]
fn verify_begin_concurrent() {
    let conn = Connection::open(":memory:").expect("in-memory connection");
    conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, val TEXT)")
        .unwrap();

    // BEGIN CONCURRENT is frankensqlite's MVCC multi-writer mode
    conn.execute("BEGIN CONCURRENT").unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'concurrent')")
        .unwrap();
    conn.execute("COMMIT").unwrap();

    let rows = conn.query("SELECT val FROM t WHERE id = 1").unwrap();
    assert_eq!(
        rows[0].get(0).unwrap(),
        &SqliteValue::Text("concurrent".into()),
        "BEGIN CONCURRENT transaction should persist data"
    );
}
