//! `SQLite` backend: schema, pragmas, and migrations.

use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole, Snippet};
use crate::sources::provenance::{LOCAL_SOURCE_ID, Source, SourceKind};
use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tracing::info;

// -------------------------------------------------------------------------
// Lazy SQLite Connection (bd-1ueu)
// -------------------------------------------------------------------------
// Defers opening the database until first use, cutting startup cost for
// commands that may not need the DB at all.  Thread-safe via parking_lot
// Mutex; logs the reason and duration of the open on first access.

/// Error from lazy database initialization.
#[derive(Debug, Error)]
pub enum LazyDbError {
    #[error("Database not found at {0}")]
    NotFound(PathBuf),
    #[error("Failed to open database at {path}: {source}")]
    OpenFailed {
        path: PathBuf,
        source: rusqlite::Error,
    },
}

/// A lazily-initialized, thread-safe SQLite connection handle.
///
/// Constructing a `LazyDb` is cheap (no I/O).  The underlying
/// `rusqlite::Connection` is opened on the first call to [`get`].
/// Subsequent calls return the cached connection.
pub struct LazyDb {
    path: PathBuf,
    conn: parking_lot::Mutex<Option<Connection>>,
}

/// RAII guard that dereferences to the inner `Connection`.
pub struct LazyDbGuard<'a>(parking_lot::MutexGuard<'a, Option<Connection>>);

impl std::fmt::Debug for LazyDbGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("LazyDbGuard")
            .field(&self.0.is_some())
            .finish()
    }
}

impl std::ops::Deref for LazyDbGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.0
            .as_ref()
            .expect("LazyDb connection must be initialized before access")
    }
}

impl LazyDb {
    /// Create a lazy handle pointing at `path`.  No I/O is performed.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            conn: parking_lot::Mutex::new(None),
        }
    }

    /// Resolve path from optional CLI overrides.
    ///
    /// Uses `data_dir / agent_search.db` as fallback.
    pub fn from_overrides(data_dir: &Option<PathBuf>, db_override: Option<PathBuf>) -> Self {
        let data_dir = data_dir.clone().unwrap_or_else(crate::default_data_dir);
        let path = db_override.unwrap_or_else(|| data_dir.join("agent_search.db"));
        Self::new(path)
    }

    /// Get the connection, opening the database on first access.
    ///
    /// `reason` is logged alongside the open duration so callers can
    /// identify which command triggered the open.
    pub fn get(&self, reason: &str) -> std::result::Result<LazyDbGuard<'_>, LazyDbError> {
        let mut guard = self.conn.lock();
        if guard.is_none() {
            if !self.path.exists() {
                return Err(LazyDbError::NotFound(self.path.clone()));
            }
            let start = Instant::now();
            let conn = Connection::open(&self.path).map_err(|e| LazyDbError::OpenFailed {
                path: self.path.clone(),
                source: e,
            })?;
            let elapsed_ms = start.elapsed().as_millis();
            info!(
                path = %self.path.display(),
                elapsed_ms = elapsed_ms,
                reason = reason,
                "lazily opened SQLite database"
            );
            *guard = Some(conn);
        }
        Ok(LazyDbGuard(guard))
    }

    /// Path to the database file (even if not yet opened).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the connection has been opened.
    pub fn is_open(&self) -> bool {
        self.conn.lock().is_some()
    }
}

// -------------------------------------------------------------------------
// Binary Metadata Serialization (Opt 3.1)
// -------------------------------------------------------------------------
// MessagePack provides 50-70% storage reduction vs JSON and faster parsing.
// New rows use binary columns; existing JSON is read on fallback.

/// Serialize a JSON value to MessagePack bytes.
/// Returns None for null/empty values to save storage.
fn serialize_json_to_msgpack(value: &serde_json::Value) -> Option<Vec<u8>> {
    if value.is_null() || value.as_object().is_some_and(|o| o.is_empty()) {
        return None;
    }
    rmp_serde::to_vec(value).ok()
}

/// Deserialize MessagePack bytes to a JSON value.
/// Returns default Value::Object({}) on error or empty input.
fn deserialize_msgpack_to_json(bytes: &[u8]) -> serde_json::Value {
    if bytes.is_empty() {
        return serde_json::Value::Object(serde_json::Map::new());
    }
    rmp_serde::from_slice(bytes).unwrap_or_else(|e| {
        tracing::debug!(
            error = %e,
            bytes_len = bytes.len(),
            "Failed to deserialize metadata - returning empty object"
        );
        serde_json::Value::Object(serde_json::Map::new())
    })
}

/// Read metadata from row, preferring binary column, falling back to JSON.
/// This provides backward compatibility during migration.
fn read_metadata_compat(
    row: &rusqlite::Row<'_>,
    json_idx: usize,
    bin_idx: usize,
) -> serde_json::Value {
    // Try binary column first (new format)
    if let Ok(Some(bytes)) = row.get::<_, Option<Vec<u8>>>(bin_idx)
        && !bytes.is_empty()
    {
        return deserialize_msgpack_to_json(&bytes);
    }

    // Fall back to JSON column (old format or migration in progress)
    if let Ok(Some(json_str)) = row.get::<_, Option<String>>(json_idx) {
        return serde_json::from_str(&json_str)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
    }

    serde_json::Value::Object(serde_json::Map::new())
}

// -------------------------------------------------------------------------
// Migration Error Types (P1.5)
// -------------------------------------------------------------------------

/// Error type for schema migration operations.
#[derive(Debug, Error)]
pub enum MigrationError {
    /// The schema requires a full rebuild. The database has been backed up.
    #[error("Rebuild required: {reason}")]
    RebuildRequired {
        reason: String,
        backup_path: Option<std::path::PathBuf>,
    },

    /// A database error occurred during migration.
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// An I/O error occurred during backup.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Other migration error.
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for MigrationError {
    fn from(e: anyhow::Error) -> Self {
        MigrationError::Other(e.to_string())
    }
}

/// Maximum number of backup files to retain.
const MAX_BACKUPS: usize = 3;

/// Files that contain user-authored state and must NEVER be deleted during rebuild.
const USER_DATA_FILES: &[&str] = &["bookmarks.db", "tui_state.json", "sources.toml", ".env"];

/// Check if a file is user-authored data that must be preserved during rebuild.
pub fn is_user_data_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|name| USER_DATA_FILES.contains(&name))
        .unwrap_or(false)
}

/// Create a timestamped backup of the database file.
///
/// Returns the path to the backup file, or None if the source doesn't exist.
pub fn create_backup(db_path: &Path) -> Result<Option<std::path::PathBuf>, MigrationError> {
    if !db_path.exists() {
        return Ok(None);
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let backup_name = format!(
        "{}.backup.{}",
        db_path.file_name().and_then(|n| n.to_str()).unwrap_or("db"),
        timestamp
    );

    let backup_path = db_path.with_file_name(&backup_name);

    // Try to use SQLite's VACUUM INTO command first, which safely handles WAL files
    // and produces a clean, minimized backup.
    let vacuum_success = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .and_then(|conn| {
        let path_str = backup_path.to_string_lossy();
        conn.execute("VACUUM INTO ?", params![path_str])
    })
    .is_ok();

    if vacuum_success {
        return Ok(Some(backup_path));
    }

    // Fallback to filesystem copy if VACUUM INTO failed (e.g., older SQLite or corruption)
    // We strictly assume this is a single-user tool; if another process is writing,
    // this raw copy might be inconsistent, but it's better than nothing.
    fs::copy(db_path, &backup_path)?;

    // Best-effort copy of WAL/SHM sidecar files if they exist
    // SQLite sidecars are named: <path>-wal and <path>-shm
    let path_str = db_path.to_string_lossy();
    let backup_str = backup_path.to_string_lossy();

    let wal_src = std::path::PathBuf::from(format!("{}-wal", path_str));
    let shm_src = std::path::PathBuf::from(format!("{}-shm", path_str));

    if wal_src.exists() {
        let _ = fs::copy(&wal_src, format!("{}-wal", backup_str));
    }
    if shm_src.exists() {
        let _ = fs::copy(&shm_src, format!("{}-shm", backup_str));
    }

    Ok(Some(backup_path))
}

/// Helper to safely remove a database file and its potential WAL/SHM sidecars.
fn remove_database_files(path: &Path) -> std::io::Result<()> {
    // Remove the main database file
    fs::remove_file(path)?;

    // Best-effort removal of sidecar files (ignore errors if they don't exist)
    let path_str = path.to_string_lossy();
    let _ = fs::remove_file(format!("{}-wal", path_str));
    let _ = fs::remove_file(format!("{}-shm", path_str));

    Ok(())
}

/// Remove old backup files, keeping only the most recent `keep_count`.
pub fn cleanup_old_backups(db_path: &Path, keep_count: usize) -> Result<(), std::io::Error> {
    let parent = match db_path.parent() {
        Some(p) => p,
        None => return Ok(()),
    };

    let db_name = db_path.file_name().and_then(|n| n.to_str()).unwrap_or("db");

    let prefix = format!("{}.backup.", db_name);

    // Collect backup files matching the pattern
    let mut backups: Vec<(std::path::PathBuf, SystemTime)> = Vec::new();

    if let Ok(entries) = fs::read_dir(parent) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.starts_with(&prefix)
                && let Ok(meta) = fs::metadata(&path)
                && let Ok(mtime) = meta.modified()
            {
                backups.push((path, mtime));
            }
        }
    }

    // Sort by modification time, newest first
    backups.sort_by_key(|entry| std::cmp::Reverse(entry.1));

    // Delete oldest backups beyond keep_count
    for (path, _) in backups.into_iter().skip(keep_count) {
        let _ = fs::remove_file(&path);

        // Also try to cleanup potential sidecars from fs::copy fallback
        let path_str = path.to_string_lossy();
        let _ = fs::remove_file(format!("{}-wal", path_str));
        let _ = fs::remove_file(format!("{}-shm", path_str));
    }

    Ok(())
}

/// Public schema version constant for external checks.
pub const CURRENT_SCHEMA_VERSION: i64 = 10;

/// Result of checking schema compatibility.
#[derive(Debug, Clone)]
pub enum SchemaCheck {
    /// Schema is up to date, no migration needed.
    Compatible,
    /// Schema needs migration but can be done incrementally.
    NeedsMigration,
    /// Schema is incompatible and needs a full rebuild (with reason).
    NeedsRebuild(String),
}

/// Check schema compatibility without modifying the database.
///
/// Opens the database read-only and checks the schema version.
fn check_schema_compatibility(path: &Path) -> std::result::Result<SchemaCheck, rusqlite::Error> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;

    // Check if meta table exists
    let meta_exists: i32 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='meta'",
        [],
        |row| row.get(0),
    )?;

    if meta_exists == 0 {
        // No meta table - could be empty or very old schema, needs rebuild
        // But first check if there are any tables at all
        let table_count: i32 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
            [],
            |row| row.get(0),
        )?;

        if table_count == 0 {
            // Empty database, will be initialized fresh
            return Ok(SchemaCheck::NeedsMigration);
        }

        // Has tables but no meta - very old or corrupted
        return Ok(SchemaCheck::NeedsRebuild(
            "Database missing schema version metadata".to_string(),
        ));
    }

    // Get the schema version
    let version: Option<i64> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0).map(|s| s.parse().ok()),
        )
        .ok()
        .flatten();

    match version {
        Some(v) if v == SCHEMA_VERSION => Ok(SchemaCheck::Compatible),
        Some(v) if v < SCHEMA_VERSION => Ok(SchemaCheck::NeedsMigration),
        Some(v) => {
            // v > SCHEMA_VERSION - database is from a newer version
            Ok(SchemaCheck::NeedsRebuild(format!(
                "Schema version {} is newer than supported version {}",
                v, SCHEMA_VERSION
            )))
        }
        None => Ok(SchemaCheck::NeedsRebuild(
            "Schema version not found or invalid".to_string(),
        )),
    }
}

const SCHEMA_VERSION: i64 = 10;

const MIGRATION_V1: &str = r"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS agents (
    id INTEGER PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    version TEXT,
    kind TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS workspaces (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    display_name TEXT
);

CREATE TABLE IF NOT EXISTS conversations (
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

CREATE TABLE IF NOT EXISTS messages (
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

CREATE TABLE IF NOT EXISTS snippets (
    id INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    file_path TEXT,
    start_line INTEGER,
    end_line INTEGER,
    language TEXT,
    snippet_text TEXT
);

CREATE TABLE IF NOT EXISTS tags (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS conversation_tags (
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (conversation_id, tag_id)
);

CREATE INDEX IF NOT EXISTS idx_conversations_agent_started
    ON conversations(agent_id, started_at DESC);

CREATE INDEX IF NOT EXISTS idx_messages_conv_idx
    ON messages(conversation_id, idx);

CREATE INDEX IF NOT EXISTS idx_messages_created
    ON messages(created_at);
";

const MIGRATION_V2: &str = r"
CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages USING fts5(
    content,
    title,
    agent,
    workspace,
    source_path,
    created_at UNINDEXED,
    message_id UNINDEXED,
    tokenize='porter'
);
INSERT INTO fts_messages(content, title, agent, workspace, source_path, created_at, message_id)
SELECT
    m.content,
    c.title,
    a.slug,
    w.path,
    c.source_path,
    m.created_at,
    m.id
FROM messages m
JOIN conversations c ON m.conversation_id = c.id
JOIN agents a ON c.agent_id = a.id
LEFT JOIN workspaces w ON c.workspace_id = w.id;
";

const MIGRATION_V3: &str = r"
DROP TABLE IF EXISTS fts_messages;
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
INSERT INTO fts_messages(content, title, agent, workspace, source_path, created_at, message_id)
SELECT
    m.content,
    c.title,
    a.slug,
    w.path,
    c.source_path,
    m.created_at,
    m.id
FROM messages m
JOIN conversations c ON m.conversation_id = c.id
JOIN agents a ON c.agent_id = a.id
LEFT JOIN workspaces w ON c.workspace_id = w.id;
";

const MIGRATION_V4: &str = r"
-- Sources table for tracking where conversations come from
CREATE TABLE IF NOT EXISTS sources (
    id TEXT PRIMARY KEY,           -- source_id (e.g., 'local', 'work-laptop')
    kind TEXT NOT NULL,            -- 'local', 'ssh', etc.
    host_label TEXT,               -- display label
    machine_id TEXT,               -- optional stable machine id
    platform TEXT,                 -- 'macos', 'linux', 'windows'
    config_json TEXT,              -- JSON blob for extra config (SSH params, path rewrites)
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Bootstrap: Insert the default 'local' source
INSERT OR IGNORE INTO sources (id, kind, host_label, created_at, updated_at)
VALUES ('local', 'local', NULL, strftime('%s','now')*1000, strftime('%s','now')*1000);
";

const MIGRATION_V5: &str = r"
-- Add provenance columns to conversations table
-- SQLite cannot alter unique constraints, so we need to recreate the table

-- Create new table with provenance columns and updated unique constraint
CREATE TABLE conversations_new (
    id INTEGER PRIMARY KEY,
    agent_id INTEGER NOT NULL REFERENCES agents(id),
    workspace_id INTEGER REFERENCES workspaces(id),
    source_id TEXT NOT NULL DEFAULT 'local' REFERENCES sources(id),
    external_id TEXT,
    title TEXT,
    source_path TEXT NOT NULL,
    started_at INTEGER,
    ended_at INTEGER,
    approx_tokens INTEGER,
    metadata_json TEXT,
    origin_host TEXT,
    UNIQUE(source_id, agent_id, external_id)
);

-- Copy data from old table (all existing conversations get source_id='local')
INSERT INTO conversations_new (id, agent_id, workspace_id, source_id, external_id, title,
                               source_path, started_at, ended_at, approx_tokens, metadata_json, origin_host)
SELECT id, agent_id, workspace_id, 'local', external_id, title,
       source_path, started_at, ended_at, approx_tokens, metadata_json, NULL
FROM conversations;

-- Drop old table and rename new
DROP TABLE conversations;
ALTER TABLE conversations_new RENAME TO conversations;

-- Recreate indexes
CREATE INDEX IF NOT EXISTS idx_conversations_agent_started ON conversations(agent_id, started_at DESC);
CREATE INDEX IF NOT EXISTS idx_conversations_source_id ON conversations(source_id);
";

const MIGRATION_V6: &str = r"
-- Optimize lookup by source_path (used by TUI detail view)
CREATE INDEX IF NOT EXISTS idx_conversations_source_path ON conversations(source_path);
";

const MIGRATION_V7: &str = r"
-- Add binary columns for MessagePack serialization (Opt 3.1)
-- Binary format is 50-70% smaller than JSON and faster to parse
ALTER TABLE conversations ADD COLUMN metadata_bin BLOB;
ALTER TABLE messages ADD COLUMN extra_bin BLOB;
";

const MIGRATION_V8: &str = r"
-- Opt 3.2: Daily stats materialized table for O(1) time-range histograms
-- Provides fast aggregated queries for stats/dashboard without full table scans

CREATE TABLE IF NOT EXISTS daily_stats (
    day_id INTEGER NOT NULL,              -- Days since 2020-01-01 (Unix epoch + offset)
    agent_slug TEXT NOT NULL,             -- 'all' for totals, or specific agent slug
    source_id TEXT NOT NULL DEFAULT 'all', -- 'all' for totals, or specific source
    session_count INTEGER NOT NULL DEFAULT 0,
    message_count INTEGER NOT NULL DEFAULT 0,
    total_chars INTEGER NOT NULL DEFAULT 0,
    last_updated INTEGER NOT NULL,
    PRIMARY KEY (day_id, agent_slug, source_id)
);

CREATE INDEX IF NOT EXISTS idx_daily_stats_agent ON daily_stats(agent_slug, day_id);
CREATE INDEX IF NOT EXISTS idx_daily_stats_source ON daily_stats(source_id, day_id);
";

const MIGRATION_V9: &str = r"
-- Background embedding jobs tracking table
CREATE TABLE IF NOT EXISTS embedding_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    db_path TEXT NOT NULL,
    model_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    total_docs INTEGER NOT NULL DEFAULT 0,
    completed_docs INTEGER NOT NULL DEFAULT 0,
    error_message TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    started_at TEXT,
    completed_at TEXT
);

-- Only one pending or running job per (db_path, model_id) at a time.
-- Multiple completed/failed/cancelled jobs are allowed for history.
CREATE UNIQUE INDEX IF NOT EXISTS idx_embedding_jobs_active
ON embedding_jobs(db_path, model_id)
WHERE status IN ('pending', 'running');
";

const MIGRATION_V10: &str = r"
-- Token analytics: per-message token usage ledger
CREATE TABLE IF NOT EXISTS token_usage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    conversation_id INTEGER NOT NULL,
    agent_id INTEGER NOT NULL,
    workspace_id INTEGER,
    source_id TEXT NOT NULL DEFAULT 'local',

    -- Timing
    timestamp_ms INTEGER NOT NULL,
    day_id INTEGER NOT NULL,

    -- Model identification
    model_name TEXT,
    model_family TEXT,
    model_tier TEXT,
    service_tier TEXT,
    provider TEXT,

    -- Token counts (nullable — not all agents provide all fields)
    input_tokens INTEGER,
    output_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_creation_tokens INTEGER,
    thinking_tokens INTEGER,
    total_tokens INTEGER,

    -- Cost estimation
    estimated_cost_usd REAL,

    -- Message context
    role TEXT NOT NULL,
    content_chars INTEGER NOT NULL,
    has_tool_calls INTEGER NOT NULL DEFAULT 0,
    tool_call_count INTEGER NOT NULL DEFAULT 0,

    -- Data quality
    data_source TEXT NOT NULL DEFAULT 'api',

    UNIQUE(message_id)
);

CREATE INDEX IF NOT EXISTS idx_token_usage_day ON token_usage(day_id, agent_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_conv ON token_usage(conversation_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_model ON token_usage(model_family, day_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_workspace ON token_usage(workspace_id, day_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_timestamp ON token_usage(timestamp_ms);

-- Token analytics: pre-aggregated daily rollups
CREATE TABLE IF NOT EXISTS token_daily_stats (
    day_id INTEGER NOT NULL,
    agent_slug TEXT NOT NULL,
    source_id TEXT NOT NULL DEFAULT 'all',
    model_family TEXT NOT NULL DEFAULT 'all',

    api_call_count INTEGER NOT NULL DEFAULT 0,
    user_message_count INTEGER NOT NULL DEFAULT 0,
    assistant_message_count INTEGER NOT NULL DEFAULT 0,
    tool_message_count INTEGER NOT NULL DEFAULT 0,

    total_input_tokens INTEGER NOT NULL DEFAULT 0,
    total_output_tokens INTEGER NOT NULL DEFAULT 0,
    total_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    total_cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    total_thinking_tokens INTEGER NOT NULL DEFAULT 0,
    grand_total_tokens INTEGER NOT NULL DEFAULT 0,

    total_content_chars INTEGER NOT NULL DEFAULT 0,
    total_tool_calls INTEGER NOT NULL DEFAULT 0,

    estimated_cost_usd REAL NOT NULL DEFAULT 0.0,

    session_count INTEGER NOT NULL DEFAULT 0,

    last_updated INTEGER NOT NULL,

    PRIMARY KEY (day_id, agent_slug, source_id, model_family)
);

CREATE INDEX IF NOT EXISTS idx_token_daily_stats_agent ON token_daily_stats(agent_slug, day_id);
CREATE INDEX IF NOT EXISTS idx_token_daily_stats_model ON token_daily_stats(model_family, day_id);

-- Model pricing lookup table
CREATE TABLE IF NOT EXISTS model_pricing (
    model_pattern TEXT NOT NULL,
    provider TEXT NOT NULL,
    input_cost_per_mtok REAL NOT NULL,
    output_cost_per_mtok REAL NOT NULL,
    cache_read_cost_per_mtok REAL,
    cache_creation_cost_per_mtok REAL,
    effective_date TEXT NOT NULL,
    PRIMARY KEY (model_pattern, effective_date)
);

-- Seed with current pricing (as of 2026-02)
INSERT OR IGNORE INTO model_pricing VALUES
    ('claude-opus-4%', 'anthropic', 15.0, 75.0, 1.5, 18.75, '2025-10-01'),
    ('claude-sonnet-4%', 'anthropic', 3.0, 15.0, 0.3, 3.75, '2025-10-01'),
    ('claude-haiku-4%', 'anthropic', 0.80, 4.0, 0.08, 1.0, '2025-10-01'),
    ('gpt-4o%', 'openai', 2.50, 10.0, NULL, NULL, '2025-01-01'),
    ('gpt-4-turbo%', 'openai', 10.0, 30.0, NULL, NULL, '2024-04-01'),
    ('gpt-4.1%', 'openai', 2.0, 8.0, NULL, NULL, '2025-04-01'),
    ('o3%', 'openai', 2.0, 8.0, NULL, NULL, '2025-04-01'),
    ('o4-mini%', 'openai', 1.10, 4.40, NULL, NULL, '2025-04-01'),
    ('gemini-2%flash%', 'google', 0.075, 0.30, NULL, NULL, '2025-01-01'),
    ('gemini-2%pro%', 'google', 1.25, 10.0, NULL, NULL, '2025-01-01');

-- Extend conversations table with token summary columns
ALTER TABLE conversations ADD COLUMN total_input_tokens INTEGER;
ALTER TABLE conversations ADD COLUMN total_output_tokens INTEGER;
ALTER TABLE conversations ADD COLUMN total_cache_read_tokens INTEGER;
ALTER TABLE conversations ADD COLUMN total_cache_creation_tokens INTEGER;
ALTER TABLE conversations ADD COLUMN grand_total_tokens INTEGER;
ALTER TABLE conversations ADD COLUMN estimated_cost_usd REAL;
ALTER TABLE conversations ADD COLUMN primary_model TEXT;
ALTER TABLE conversations ADD COLUMN api_call_count INTEGER;
ALTER TABLE conversations ADD COLUMN tool_call_count INTEGER;
ALTER TABLE conversations ADD COLUMN user_message_count INTEGER;
ALTER TABLE conversations ADD COLUMN assistant_message_count INTEGER;
";

/// Row from the embedding_jobs table.
#[derive(Debug, Clone)]
pub struct EmbeddingJobRow {
    pub id: i64,
    pub db_path: String,
    pub model_id: String,
    pub status: String,
    pub total_docs: i64,
    pub completed_docs: i64,
    pub error_message: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

pub struct SqliteStorage {
    conn: Connection,
}

pub struct InsertOutcome {
    pub conversation_id: i64,
    pub inserted_indices: Vec<i64>,
}

/// Message data needed for semantic embedding generation.
pub struct MessageForEmbedding {
    pub message_id: i64,
    pub created_at: Option<i64>,
    pub agent_id: i64,
    pub workspace_id: Option<i64>,
    pub source_id_hash: u32,
    pub role: String,
    pub content: String,
}

impl SqliteStorage {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating db directory {}", parent.display()))?;
        }

        let mut conn = Connection::open(path)
            .with_context(|| format!("opening sqlite db at {}", path.display()))?;

        apply_pragmas(&mut conn)?;
        init_meta(&mut conn)?;
        migrate(&mut conn)?;

        Ok(Self { conn })
    }

    pub fn open_readonly(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening sqlite db readonly at {}", path.display()))?;

        apply_common_pragmas(&conn)?;

        Ok(Self { conn })
    }

    /// Open database with migration, backing up and signaling rebuild if schema is incompatible.
    ///
    /// This is the recommended entry point for the indexer. It handles:
    /// - Schema version checking
    /// - Automatic backup before destructive operations
    /// - Cleanup of old backups
    /// - Clear signaling when a full rebuild is required
    ///
    /// # Returns
    /// - `Ok(storage)` if migration succeeded or no migration was needed
    /// - `Err(MigrationError::RebuildRequired { .. })` if the caller should rebuild from scratch
    ///
    /// When `RebuildRequired` is returned, the caller should:
    /// 1. Delete the database file (it's already backed up)
    /// 2. Create a fresh database
    /// 3. Re-index all conversations from source files
    pub fn open_or_rebuild(path: &Path) -> std::result::Result<Self, MigrationError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Check if we need to handle an incompatible schema before opening
        if path.exists() {
            let check_result = check_schema_compatibility(path);
            match check_result {
                Ok(SchemaCheck::Compatible) => {
                    // Continue with normal open
                }
                Ok(SchemaCheck::NeedsMigration) => {
                    // Continue with normal open, migration will handle it
                }
                Ok(SchemaCheck::NeedsRebuild(reason)) => {
                    // Schema from future or otherwise incompatible - trigger rebuild
                    let backup_path = create_backup(path)?;
                    cleanup_old_backups(path, MAX_BACKUPS)?;
                    remove_database_files(path)?;
                    return Err(MigrationError::RebuildRequired {
                        reason,
                        backup_path,
                    });
                }
                Err(_) => {
                    // If we can't even check, it's likely corrupt - trigger rebuild
                    let backup_path = create_backup(path)?;
                    cleanup_old_backups(path, MAX_BACKUPS)?;
                    remove_database_files(path)?;
                    return Err(MigrationError::RebuildRequired {
                        reason: "Database appears corrupted".to_string(),
                        backup_path,
                    });
                }
            }
        }

        // Now open and migrate normally
        let mut conn = Connection::open(path)?;
        apply_pragmas(&mut conn).map_err(|e| MigrationError::Other(e.to_string()))?;
        init_meta(&mut conn).map_err(|e| MigrationError::Other(e.to_string()))?;
        migrate(&mut conn).map_err(|e| MigrationError::Other(e.to_string()))?;

        Ok(Self { conn })
    }

    pub fn raw(&self) -> &Connection {
        &self.conn
    }

    pub fn schema_version(&self) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |row| row.get::<_, String>(0).map(|s| s.parse().unwrap_or(0)),
            )
            .optional()?
            .ok_or_else(|| anyhow!("schema_version missing"))
    }

    pub fn ensure_agent(&self, agent: &Agent) -> Result<i64> {
        let now = Self::now_millis();
        self.conn.execute(
            "INSERT INTO agents(slug, name, version, kind, created_at, updated_at) VALUES(?,?,?,?,?,?)
             ON CONFLICT(slug) DO UPDATE SET name=excluded.name, version=excluded.version, kind=excluded.kind, updated_at=excluded.updated_at",
            params![
                &agent.slug,
                &agent.name,
                &agent.version,
                agent_kind_str(agent.kind.clone()),
                now,
                now
            ],
        )?;

        self.conn
            .query_row(
                "SELECT id FROM agents WHERE slug = ?",
                params![&agent.slug],
                |row| row.get(0),
            )
            .with_context(|| format!("fetching agent id for {}", agent.slug))
    }

    pub fn ensure_workspace(&self, path: &Path, display_name: Option<&str>) -> Result<i64> {
        let path_str = path.to_string_lossy();
        self.conn.execute(
            "INSERT INTO workspaces(path, display_name) VALUES(?,?)
             ON CONFLICT(path) DO UPDATE SET display_name=COALESCE(excluded.display_name, workspaces.display_name)",
            params![path_str, display_name],
        )?;

        self.conn
            .query_row(
                "SELECT id FROM workspaces WHERE path = ?",
                params![path_str],
                |row| row.get(0),
            )
            .with_context(|| format!("fetching workspace id for {path_str}"))
    }
}

// -------------------------------------------------------------------------
// IndexingCache (Opt 7.2) - N+1 Prevention for Agent/Workspace IDs
// -------------------------------------------------------------------------

use std::collections::HashMap;

/// Cache for agent and workspace IDs during batch indexing.
///
/// Prevents N+1 database queries by caching the results of ensure_agent
/// and ensure_workspace calls within a batch. This is per-batch and
/// single-threaded, so no synchronization is needed.
///
/// # Usage
/// ```ignore
/// let mut cache = IndexingCache::new();
/// for conv in conversations {
///     let agent_id = cache.get_or_insert_agent(storage, &agent)?;
///     let workspace_id = cache.get_or_insert_workspace(storage, workspace)?;
///     // ... use agent_id and workspace_id
/// }
/// ```
///
/// # Rollback
/// Set environment variable `CASS_SQLITE_CACHE=0` to bypass caching
/// and use direct DB calls (useful for debugging).
#[derive(Debug, Default)]
pub struct IndexingCache {
    agent_ids: HashMap<String, i64>,
    workspace_ids: HashMap<PathBuf, i64>,
    hits: u64,
    misses: u64,
}

impl IndexingCache {
    /// Create a new empty cache.
    pub fn new() -> Self {
        Self {
            agent_ids: HashMap::new(),
            workspace_ids: HashMap::new(),
            hits: 0,
            misses: 0,
        }
    }

    /// Check if caching is enabled via environment variable.
    /// Returns true unless CASS_SQLITE_CACHE is set to "0" or "false".
    pub fn is_enabled() -> bool {
        dotenvy::var("CASS_SQLITE_CACHE")
            .map(|v| v != "0" && v.to_lowercase() != "false")
            .unwrap_or(true)
    }

    /// Get or insert an agent ID, using cache if available.
    ///
    /// Returns the cached ID if present, otherwise calls ensure_agent
    /// and caches the result.
    pub fn get_or_insert_agent(&mut self, storage: &SqliteStorage, agent: &Agent) -> Result<i64> {
        if let Some(&cached) = self.agent_ids.get(&agent.slug) {
            self.hits += 1;
            return Ok(cached);
        }

        self.misses += 1;
        let id = storage.ensure_agent(agent)?;
        self.agent_ids.insert(agent.slug.clone(), id);
        Ok(id)
    }

    /// Get or insert a workspace ID, using cache if available.
    ///
    /// Returns the cached ID if present, otherwise calls ensure_workspace
    /// and caches the result.
    pub fn get_or_insert_workspace(
        &mut self,
        storage: &SqliteStorage,
        path: &Path,
        display_name: Option<&str>,
    ) -> Result<i64> {
        if let Some(&cached) = self.workspace_ids.get(path) {
            self.hits += 1;
            return Ok(cached);
        }

        self.misses += 1;
        let id = storage.ensure_workspace(path, display_name)?;
        self.workspace_ids.insert(path.to_path_buf(), id);
        Ok(id)
    }

    /// Get cache statistics: (hits, misses, hit_rate).
    pub fn stats(&self) -> (u64, u64, f64) {
        let total = self.hits + self.misses;
        let hit_rate = if total > 0 {
            self.hits as f64 / total as f64
        } else {
            0.0
        };
        (self.hits, self.misses, hit_rate)
    }

    /// Clear the cache, resetting all state.
    pub fn clear(&mut self) {
        self.agent_ids.clear();
        self.workspace_ids.clear();
        self.hits = 0;
        self.misses = 0;
    }

    /// Number of cached agents.
    pub fn agent_count(&self) -> usize {
        self.agent_ids.len()
    }

    /// Number of cached workspaces.
    pub fn workspace_count(&self) -> usize {
        self.workspace_ids.len()
    }
}

// -------------------------------------------------------------------------
// StatsAggregator (kzxu) - Batched Daily Stats Updates
// -------------------------------------------------------------------------
// Aggregates daily stats in memory during batch ingestion, then flushes
// to the database in a single batched INSERT...ON CONFLICT operation.
// This prevents N×4 database writes (4 permutations per conversation).

/// Accumulated statistics delta for a single (day_id, agent, source) combination.
#[derive(Clone, Debug, Default)]
pub struct StatsDelta {
    pub session_count_delta: i64,
    pub message_count_delta: i64,
    pub total_chars_delta: i64,
}

/// In-memory aggregator for batched daily stats updates.
///
/// During batch ingestion, we accumulate deltas per (day_id, agent, source) key.
/// After processing all conversations, call `expand()` to generate the 4
/// permutations per raw entry, then flush via `SqliteStorage::update_daily_stats_batched`.
///
/// # Example
/// ```ignore
/// let mut agg = StatsAggregator::new();
/// for conv in conversations {
///     agg.record(&conv.agent_slug, source_id, day_id, msg_count, char_count);
/// }
/// let entries = agg.expand();
/// storage.update_daily_stats_batched(&entries)?;
/// ```
#[derive(Debug, Default)]
pub struct StatsAggregator {
    /// Raw deltas keyed by (day_id, agent_slug, source_id).
    /// Only stores specific (non-"all") combinations.
    deltas: HashMap<(i64, String, String), StatsDelta>,
}

impl StatsAggregator {
    /// Create a new empty aggregator.
    pub fn new() -> Self {
        Self {
            deltas: HashMap::new(),
        }
    }

    /// Record a conversation's contribution to stats (session + messages + chars).
    ///
    /// This increments session_count by 1.
    ///
    /// # Arguments
    /// * `agent_slug` - The specific agent slug (not "all")
    /// * `source_id` - The specific source ID (not "all")
    /// * `day_id` - Days since 2020-01-01 (from `SqliteStorage::day_id_from_millis`)
    /// * `message_count` - Number of messages in the conversation
    /// * `total_chars` - Total character count across all messages
    pub fn record(
        &mut self,
        agent_slug: &str,
        source_id: &str,
        day_id: i64,
        message_count: i64,
        total_chars: i64,
    ) {
        self.record_delta(agent_slug, source_id, day_id, 1, message_count, total_chars);
    }

    /// Record an arbitrary delta. Use this for append-only updates where
    /// `session_count_delta` may be 0 but message/char deltas are non-zero.
    pub fn record_delta(
        &mut self,
        agent_slug: &str,
        source_id: &str,
        day_id: i64,
        session_count_delta: i64,
        message_count_delta: i64,
        total_chars_delta: i64,
    ) {
        if session_count_delta == 0 && message_count_delta == 0 && total_chars_delta == 0 {
            return;
        }
        let key = (day_id, agent_slug.to_owned(), source_id.to_owned());
        let delta = self.deltas.entry(key).or_default();
        delta.session_count_delta += session_count_delta;
        delta.message_count_delta += message_count_delta;
        delta.total_chars_delta += total_chars_delta;
    }

    /// Expand raw deltas into the 4 permutation keys:
    /// - (agent, source) - specific both
    /// - ("all", source) - all agents, specific source
    /// - (agent, "all") - specific agent, all sources
    /// - ("all", "all") - totals
    ///
    /// Returns entries sorted by (day_id, agent_slug, source_id) for deterministic batching.
    pub fn expand(&self) -> Vec<(i64, String, String, StatsDelta)> {
        let mut expanded: HashMap<(i64, String, String), StatsDelta> = HashMap::new();

        for ((day_id, agent, source), delta) in &self.deltas {
            let permutations = [
                (agent.as_str(), source.as_str()),
                ("all", source.as_str()),
                (agent.as_str(), "all"),
                ("all", "all"),
            ];

            // Ensure we don't double-apply deltas if agent/source is already "all".
            for idx in 0..permutations.len() {
                let (a, s) = permutations[idx];
                if permutations[..idx].contains(&(a, s)) {
                    continue;
                }
                let key = (*day_id, a.to_owned(), s.to_owned());
                let entry = expanded.entry(key).or_default();
                entry.session_count_delta += delta.session_count_delta;
                entry.message_count_delta += delta.message_count_delta;
                entry.total_chars_delta += delta.total_chars_delta;
            }
        }

        let mut out: Vec<(i64, String, String, StatsDelta)> = expanded
            .into_iter()
            .map(|((d, a, s), delta)| (d, a, s, delta))
            .collect();
        out.sort_by(|(d1, a1, s1, _), (d2, a2, s2, _)| {
            d1.cmp(d2).then_with(|| a1.cmp(a2)).then_with(|| s1.cmp(s2))
        });
        out
    }

    /// Check if the aggregator is empty (no data recorded).
    pub fn is_empty(&self) -> bool {
        self.deltas.is_empty()
    }

    /// Get number of distinct raw (day, agent, source) combinations recorded.
    pub fn raw_entry_count(&self) -> usize {
        self.deltas.len()
    }
}

// -------------------------------------------------------------------------
// TokenStatsAggregator — Batched Token Analytics Daily Stats
// -------------------------------------------------------------------------
// Mirrors StatsAggregator pattern for token-level metrics.
// Aggregates token usage in memory during batch ingestion, then flushes
// to token_daily_stats in a single batched INSERT...ON CONFLICT operation.

/// Accumulated token statistics delta for a single (day_id, agent, source, model_family) combination.
#[derive(Clone, Debug, Default)]
pub struct TokenStatsDelta {
    pub api_call_count: i64,
    pub user_message_count: i64,
    pub assistant_message_count: i64,
    pub tool_message_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cache_creation_tokens: i64,
    pub total_thinking_tokens: i64,
    pub grand_total_tokens: i64,
    pub total_content_chars: i64,
    pub total_tool_calls: i64,
    pub estimated_cost_usd: f64,
    pub session_count: i64,
}

/// In-memory aggregator for batched token daily stats updates.
///
/// During batch ingestion, accumulate token deltas per (day_id, agent, source, model_family) key.
/// After processing, call `expand()` to generate the 5 permutation keys, then flush via
/// `update_token_daily_stats_batched_in_tx`.
#[derive(Debug, Default)]
pub struct TokenStatsAggregator {
    /// Raw deltas keyed by (day_id, agent_slug, source_id, model_family).
    deltas: HashMap<(i64, String, String, String), TokenStatsDelta>,
}

impl TokenStatsAggregator {
    pub fn new() -> Self {
        Self {
            deltas: HashMap::new(),
        }
    }

    /// Record a single message's token contribution.
    pub fn record(
        &mut self,
        agent_slug: &str,
        source_id: &str,
        day_id: i64,
        model_family: &str,
        role: &str,
        usage: &crate::connectors::ExtractedTokenUsage,
        content_chars: i64,
    ) {
        let key = (
            day_id,
            agent_slug.to_owned(),
            source_id.to_owned(),
            model_family.to_owned(),
        );
        let delta = self.deltas.entry(key).or_default();

        delta.api_call_count += 1;
        match role {
            "user" => delta.user_message_count += 1,
            "assistant" | "agent" => delta.assistant_message_count += 1,
            "tool" => delta.tool_message_count += 1,
            _ => {}
        }

        delta.total_input_tokens += usage.input_tokens.unwrap_or(0);
        delta.total_output_tokens += usage.output_tokens.unwrap_or(0);
        delta.total_cache_read_tokens += usage.cache_read_tokens.unwrap_or(0);
        delta.total_cache_creation_tokens += usage.cache_creation_tokens.unwrap_or(0);
        delta.total_thinking_tokens += usage.thinking_tokens.unwrap_or(0);
        delta.grand_total_tokens += usage.total_tokens().unwrap_or(0);
        delta.total_content_chars += content_chars;
        delta.total_tool_calls += usage.tool_call_count as i64;
    }

    /// Record a session count bump for a given day/agent/source/model.
    pub fn record_session(
        &mut self,
        agent_slug: &str,
        source_id: &str,
        day_id: i64,
        model_family: &str,
    ) {
        let key = (
            day_id,
            agent_slug.to_owned(),
            source_id.to_owned(),
            model_family.to_owned(),
        );
        self.deltas.entry(key).or_default().session_count += 1;
    }

    /// Expand raw deltas into 5 permutation keys for the 4-dimensional composite PK:
    /// - (agent, source, model)  — specific all three
    /// - ("all", source, model)  — all agents
    /// - (agent, "all", model)   — all sources
    /// - (agent, source, "all")  — all models
    /// - ("all", "all", "all")   — global total
    pub fn expand(&self) -> Vec<(i64, String, String, String, TokenStatsDelta)> {
        let mut expanded: HashMap<(i64, String, String, String), TokenStatsDelta> = HashMap::new();

        for ((day_id, agent, source, model), delta) in &self.deltas {
            let permutations = [
                (agent.as_str(), source.as_str(), model.as_str()),
                ("all", source.as_str(), model.as_str()),
                (agent.as_str(), "all", model.as_str()),
                (agent.as_str(), source.as_str(), "all"),
                ("all", "all", "all"),
            ];

            for idx in 0..permutations.len() {
                let (a, s, m) = permutations[idx];
                // Deduplicate if agent/source/model is already "all"
                if permutations[..idx].contains(&(a, s, m)) {
                    continue;
                }
                let key = (*day_id, a.to_owned(), s.to_owned(), m.to_owned());
                let entry = expanded.entry(key).or_default();
                entry.api_call_count += delta.api_call_count;
                entry.user_message_count += delta.user_message_count;
                entry.assistant_message_count += delta.assistant_message_count;
                entry.tool_message_count += delta.tool_message_count;
                entry.total_input_tokens += delta.total_input_tokens;
                entry.total_output_tokens += delta.total_output_tokens;
                entry.total_cache_read_tokens += delta.total_cache_read_tokens;
                entry.total_cache_creation_tokens += delta.total_cache_creation_tokens;
                entry.total_thinking_tokens += delta.total_thinking_tokens;
                entry.grand_total_tokens += delta.grand_total_tokens;
                entry.total_content_chars += delta.total_content_chars;
                entry.total_tool_calls += delta.total_tool_calls;
                entry.estimated_cost_usd += delta.estimated_cost_usd;
                entry.session_count += delta.session_count;
            }
        }

        let mut out: Vec<(i64, String, String, String, TokenStatsDelta)> = expanded
            .into_iter()
            .map(|((d, a, s, m), delta)| (d, a, s, m, delta))
            .collect();
        out.sort_by(|(d1, a1, s1, m1, _), (d2, a2, s2, m2, _)| {
            d1.cmp(d2)
                .then_with(|| a1.cmp(a2))
                .then_with(|| s1.cmp(s2))
                .then_with(|| m1.cmp(m2))
        });
        out
    }

    pub fn is_empty(&self) -> bool {
        self.deltas.is_empty()
    }

    pub fn raw_entry_count(&self) -> usize {
        self.deltas.len()
    }
}

/// Pending token_usage row to be batch-inserted.
#[derive(Debug, Clone)]
pub struct TokenUsageEntry {
    pub message_id: i64,
    pub conversation_id: i64,
    pub agent_id: i64,
    pub workspace_id: Option<i64>,
    pub source_id: String,
    pub timestamp_ms: i64,
    pub day_id: i64,
    pub model_name: Option<String>,
    pub model_family: Option<String>,
    pub model_tier: Option<String>,
    pub service_tier: Option<String>,
    pub provider: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub thinking_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub estimated_cost_usd: Option<f64>,
    pub role: String,
    pub content_chars: i64,
    pub has_tool_calls: bool,
    pub tool_call_count: u32,
    pub data_source: String,
}

impl SqliteStorage {
    pub fn insert_conversation_tree(
        &mut self,
        agent_id: i64,
        workspace_id: Option<i64>,
        conv: &Conversation,
    ) -> Result<InsertOutcome> {
        // Check for existing conversation with same (source_id, agent_id, external_id)
        if let Some(ext) = &conv.external_id
            && let Some(existing) = self
                .conn
                .query_row(
                    "SELECT id FROM conversations WHERE source_id = ? AND agent_id = ? AND external_id = ?",
                    params![&conv.source_id, agent_id, ext],
                    |row| row.get(0),
                )
                .optional()?
        {
            return self.append_messages(existing, conv);
        }

        let tx = self.conn.transaction()?;

        let conv_id = insert_conversation(&tx, agent_id, workspace_id, conv)?;
        let mut fts_entries = Vec::with_capacity(conv.messages.len());
        let mut total_chars: i64 = 0;
        for msg in &conv.messages {
            let msg_id = insert_message(&tx, conv_id, msg)?;
            insert_snippets(&tx, msg_id, &msg.snippets)?;
            fts_entries.push(FtsEntry::from_message(msg_id, msg, conv));
            total_chars += msg.content.len() as i64;
        }
        // Batch insert FTS entries
        batch_insert_fts_messages(&tx, &fts_entries)?;

        // Update daily stats (+1 session, +N messages)
        update_daily_stats_in_tx(
            &tx,
            &conv.agent_slug,
            &conv.source_id,
            conv.started_at,
            1, // New session
            conv.messages.len() as i64,
            total_chars,
        )?;

        tx.commit()?;
        Ok(InsertOutcome {
            conversation_id: conv_id,
            inserted_indices: conv.messages.iter().map(|m| m.idx).collect(),
        })
    }

    fn append_messages(
        &mut self,
        conversation_id: i64,
        conv: &Conversation,
    ) -> Result<InsertOutcome> {
        let tx = self.conn.transaction()?;

        let max_idx: Option<i64> = tx.query_row(
            "SELECT MAX(idx) FROM messages WHERE conversation_id = ?",
            params![conversation_id],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        let cutoff = max_idx.unwrap_or(-1);

        let mut inserted_indices = Vec::new();
        let mut fts_entries = Vec::new();
        let mut new_chars: i64 = 0;
        for msg in &conv.messages {
            if msg.idx <= cutoff {
                continue;
            }
            let msg_id = insert_message(&tx, conversation_id, msg)?;
            insert_snippets(&tx, msg_id, &msg.snippets)?;
            fts_entries.push(FtsEntry::from_message(msg_id, msg, conv));
            inserted_indices.push(msg.idx);
            new_chars += msg.content.len() as i64;
        }

        // Batch insert FTS entries
        batch_insert_fts_messages(&tx, &fts_entries)?;

        if let Some(last_ts) = conv.messages.iter().filter_map(|m| m.created_at).max() {
            // Use IFNULL to handle NULL ended_at values correctly.
            // SQLite's scalar MAX(NULL, x) returns NULL, so we need to coalesce first.
            tx.execute(
                "UPDATE conversations SET ended_at = MAX(IFNULL(ended_at, 0), ?) WHERE id = ?",
                params![last_ts, conversation_id],
            )?;
        }

        // Update daily stats if new messages were appended (+0 sessions, +N messages)
        if !inserted_indices.is_empty() {
            let message_count = inserted_indices.len() as i64;
            update_daily_stats_in_tx(
                &tx,
                &conv.agent_slug,
                &conv.source_id,
                conv.started_at,
                0, // Existing session
                message_count,
                new_chars,
            )?;
        }

        tx.commit()?;
        Ok(InsertOutcome {
            conversation_id,
            inserted_indices,
        })
    }

    /// Insert multiple conversations in a single transaction with batch FTS indexing.
    ///
    /// Uses multi-value INSERT for FTS5 entries (P2 Opt 2.1) to reduce
    /// transaction overhead and improve indexing throughput by 10-20%.
    pub fn insert_conversations_batched(
        &mut self,
        conversations: &[(i64, Option<i64>, &Conversation)],
    ) -> Result<Vec<InsertOutcome>> {
        if conversations.is_empty() {
            return Ok(Vec::new());
        }

        let tx = self.conn.transaction()?;
        let mut outcomes = Vec::with_capacity(conversations.len());
        let mut fts_entries = Vec::new();
        let mut stats = StatsAggregator::new();

        // Process all conversations, collecting FTS entries
        for &(agent_id, workspace_id, conv) in conversations {
            let (outcome, delta) = insert_conversation_in_tx_batched(
                &tx,
                agent_id,
                workspace_id,
                conv,
                &mut fts_entries,
            )?;
            if delta.session_count_delta != 0
                || delta.message_count_delta != 0
                || delta.total_chars_delta != 0
            {
                let day_id = conv
                    .started_at
                    .map(SqliteStorage::day_id_from_millis)
                    .unwrap_or(0);
                stats.record_delta(
                    &conv.agent_slug,
                    &conv.source_id,
                    day_id,
                    delta.session_count_delta,
                    delta.message_count_delta,
                    delta.total_chars_delta,
                );
            }
            outcomes.push(outcome);
        }

        // Batch insert all FTS entries at once
        let fts_count = fts_entries.len();
        if fts_count > 0 {
            let inserted = batch_insert_fts_messages(&tx, &fts_entries)?;
            tracing::debug!(
                target: "cass::perf::fts5",
                total = fts_count,
                inserted = inserted,
                conversations = conversations.len(),
                "batch_fts_insert_complete"
            );
        }

        // Batched daily_stats update (avoid N*4 upserts).
        if !stats.is_empty() {
            let entries = stats.expand();
            let affected = update_daily_stats_batched_in_tx(&tx, &entries)?;
            tracing::debug!(
                target: "cass::perf::daily_stats",
                raw = stats.raw_entry_count(),
                expanded = entries.len(),
                affected = affected,
                "batched_stats_update_complete"
            );
        }

        tx.commit()?;
        Ok(outcomes)
    }

    pub fn list_agents(&self) -> Result<Vec<Agent>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, slug, name, version, kind FROM agents ORDER BY slug")?;
        let rows = stmt.query_map([], |row| {
            let kind: String = row.get(4)?;
            Ok(Agent {
                id: Some(row.get(0)?),
                slug: row.get(1)?,
                name: row.get(2)?,
                version: row.get(3)?,
                kind: match kind.as_str() {
                    "cli" => AgentKind::Cli,
                    "vscode" => AgentKind::VsCode,
                    _ => AgentKind::Hybrid,
                },
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_workspaces(&self) -> Result<Vec<crate::model::types::Workspace>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, path, display_name FROM workspaces ORDER BY path")?;
        let rows = stmt.query_map([], |row| {
            Ok(crate::model::types::Workspace {
                id: Some(row.get(0)?),
                path: Path::new(&row.get::<_, String>(1)?).to_path_buf(),
                display_name: row.get::<_, Option<String>>(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_conversations(&self, limit: i64, offset: i64) -> Result<Vec<Conversation>> {
        let mut stmt = self.conn.prepare(
            r"SELECT c.id, a.slug, w.path, c.external_id, c.title, c.source_path,
                       c.started_at, c.ended_at, c.approx_tokens, c.metadata_json,
                       c.source_id, c.origin_host, c.metadata_bin
                FROM conversations c
                JOIN agents a ON c.agent_id = a.id
                LEFT JOIN workspaces w ON c.workspace_id = w.id
                ORDER BY c.started_at IS NULL, c.started_at DESC, c.id DESC
                LIMIT ? OFFSET ?",
        )?;

        let rows = stmt.query_map(params![limit, offset], |row| {
            Ok(Conversation {
                id: Some(row.get(0)?),
                agent_slug: row.get(1)?,
                workspace: row
                    .get::<_, Option<String>>(2)?
                    .map(|p| Path::new(&p).to_path_buf()),
                external_id: row.get(3)?,
                title: row.get(4)?,
                source_path: Path::new(&row.get::<_, String>(5)?).to_path_buf(),
                started_at: row.get(6)?,
                ended_at: row.get(7)?,
                approx_tokens: row.get(8)?,
                // Read from binary column first (idx 12), fallback to JSON (idx 9)
                metadata_json: read_metadata_compat(row, 9, 12),
                messages: Vec::new(),
                source_id: row
                    .get::<_, String>(10)
                    .unwrap_or_else(|_| "local".to_string()),
                origin_host: row.get(11)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn fetch_messages(&self, conversation_id: i64) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, idx, role, author, created_at, content, extra_json, extra_bin FROM messages WHERE conversation_id = ? ORDER BY idx",
        )?;
        let rows = stmt.query_map(params![conversation_id], |row| {
            let role: String = row.get(2)?;
            Ok(Message {
                id: Some(row.get(0)?),
                idx: row.get(1)?,
                role: match role.as_str() {
                    "user" => MessageRole::User,
                    "agent" | "assistant" => MessageRole::Agent,
                    "tool" => MessageRole::Tool,
                    "system" => MessageRole::System,
                    other => MessageRole::Other(other.to_string()),
                },
                author: row.get::<_, Option<String>>(3)?,
                created_at: row.get::<_, Option<i64>>(4)?,
                content: row.get(5)?,
                // Read from binary column first (idx 7), fallback to JSON (idx 6)
                extra_json: read_metadata_compat(row, 6, 7),
                snippets: Vec::new(),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Fetch all messages with their conversation metadata for semantic indexing.
    ///
    /// Returns MessageForEmbedding records with all metadata needed for vector indexing.
    pub fn fetch_messages_for_embedding(&self) -> Result<Vec<MessageForEmbedding>> {
        let mut stmt = self.conn.prepare(
            r"SELECT m.id, m.created_at, c.agent_id, c.workspace_id, c.source_id, m.role, m.content
              FROM messages m
              JOIN conversations c ON m.conversation_id = c.id
              ORDER BY m.id",
        )?;

        let rows = stmt.query_map([], |row| {
            let source_id_str: String = row
                .get::<_, Option<String>>(4)?
                .unwrap_or_else(|| "local".to_string());
            // CRC32 hash of source_id string for compact storage
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(source_id_str.as_bytes());
            let source_id_hash = hasher.finalize();

            Ok(MessageForEmbedding {
                message_id: row.get(0)?,
                created_at: row.get(1)?,
                agent_id: row.get(2)?,
                workspace_id: row.get(3)?,
                source_id_hash,
                role: row.get(5)?,
                content: row.get(6)?,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Insert or update an embedding job, returning the job ID.
    pub fn upsert_embedding_job(
        &self,
        db_path: &str,
        model_id: &str,
        total_docs: i64,
    ) -> Result<i64> {
        // Cancel any existing pending/running jobs for this db_path+model_id
        self.conn.execute(
            "UPDATE embedding_jobs SET status = 'cancelled', completed_at = datetime('now')
             WHERE db_path = ?1 AND model_id = ?2 AND status IN ('pending', 'running')",
            params![db_path, model_id],
        )?;
        self.conn.execute(
            "INSERT INTO embedding_jobs (db_path, model_id, status, total_docs)
             VALUES (?1, ?2, 'pending', ?3)",
            params![db_path, model_id, total_docs],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Mark an embedding job as running.
    pub fn start_embedding_job(&self, job_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE embedding_jobs SET status = 'running', started_at = datetime('now') WHERE id = ?1",
            params![job_id],
        )?;
        Ok(())
    }

    /// Mark an embedding job as completed.
    pub fn complete_embedding_job(&self, job_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE embedding_jobs SET status = 'completed', completed_at = datetime('now') WHERE id = ?1",
            params![job_id],
        )?;
        Ok(())
    }

    /// Mark an embedding job as failed with an error message.
    pub fn fail_embedding_job(&self, job_id: i64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE embedding_jobs SET status = 'failed', error_message = ?2, completed_at = datetime('now') WHERE id = ?1",
            params![job_id, error],
        )?;
        Ok(())
    }

    /// Cancel pending/running embedding jobs for a db_path, optionally filtered by model_id.
    pub fn cancel_embedding_jobs(&self, db_path: &str, model_id: Option<&str>) -> Result<usize> {
        let count = if let Some(mid) = model_id {
            self.conn.execute(
                "UPDATE embedding_jobs SET status = 'cancelled', completed_at = datetime('now')
                 WHERE db_path = ?1 AND model_id = ?2 AND status IN ('pending', 'running')",
                params![db_path, mid],
            )?
        } else {
            self.conn.execute(
                "UPDATE embedding_jobs SET status = 'cancelled', completed_at = datetime('now')
                 WHERE db_path = ?1 AND status IN ('pending', 'running')",
                params![db_path],
            )?
        };
        Ok(count)
    }

    /// Update the progress of an embedding job.
    pub fn update_job_progress(&self, job_id: i64, completed_docs: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE embedding_jobs SET completed_docs = ?2 WHERE id = ?1",
            params![job_id, completed_docs],
        )?;
        Ok(())
    }

    /// Get all embedding jobs for a given db_path.
    pub fn get_embedding_jobs(&self, db_path: &str) -> Result<Vec<EmbeddingJobRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, db_path, model_id, status, total_docs, completed_docs,
                    error_message, created_at, started_at, completed_at
             FROM embedding_jobs WHERE db_path = ?1 ORDER BY id DESC",
        )?;
        let rows = stmt.query_map(params![db_path], |row| {
            Ok(EmbeddingJobRow {
                id: row.get(0)?,
                db_path: row.get(1)?,
                model_id: row.get(2)?,
                status: row.get(3)?,
                total_docs: row.get(4)?,
                completed_docs: row.get(5)?,
                error_message: row.get(6)?,
                created_at: row.get(7)?,
                started_at: row.get(8)?,
                completed_at: row.get(9)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn rebuild_fts(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM fts_messages", [])?;
        self.conn.execute_batch(
            r"INSERT INTO fts_messages(content, title, agent, workspace, source_path, created_at, message_id)
               SELECT m.content, c.title, a.slug, w.path, c.source_path, m.created_at, m.id
               FROM messages m
               JOIN conversations c ON m.conversation_id = c.id
               JOIN agents a ON c.agent_id = a.id
               LEFT JOIN workspaces w ON c.workspace_id = w.id;",
        )?;
        Ok(())
    }

    /// Get the timestamp of the last successful scan (milliseconds since epoch).
    /// Returns None if no scan has been recorded yet.
    pub fn get_last_scan_ts(&self) -> Result<Option<i64>> {
        let ts: Option<i64> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_scan_ts'",
                [],
                |row| {
                    let s: String = row.get(0)?;
                    Ok(s.parse().ok())
                },
            )
            .optional()?
            .flatten();
        Ok(ts)
    }

    /// Set the timestamp of the last successful scan (milliseconds since epoch).
    pub fn set_last_scan_ts(&mut self, ts: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES('last_scan_ts', ?)",
            params![ts.to_string()],
        )?;
        Ok(())
    }

    /// Set the timestamp of the last successful index completion (milliseconds since epoch).
    pub fn set_last_indexed_at(&mut self, ts: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES('last_indexed_at', ?)",
            params![ts.to_string()],
        )?;
        Ok(())
    }

    /// Get current time as milliseconds since epoch.
    pub fn now_millis() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }

    // -------------------------------------------------------------------------
    // Source CRUD operations
    // -------------------------------------------------------------------------

    /// Get a source by ID.
    pub fn get_source(&self, id: &str) -> Result<Option<Source>> {
        self.conn
            .query_row(
                "SELECT id, kind, host_label, machine_id, platform, config_json, created_at, updated_at
                 FROM sources WHERE id = ?",
                params![id],
                |row| {
                    let kind_str: String = row.get(1)?;
                    let config_json_str: Option<String> = row.get(5)?;
                    Ok(Source {
                        id: row.get(0)?,
                        kind: SourceKind::parse(&kind_str).unwrap_or_default(),
                        host_label: row.get(2)?,
                        machine_id: row.get(3)?,
                        platform: row.get(4)?,
                        config_json: config_json_str
                            .and_then(|s| serde_json::from_str(&s).ok()),
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                    })
                },
            )
            .optional()
            .with_context(|| format!("fetching source with id '{id}'"))
    }

    /// List all sources.
    pub fn list_sources(&self) -> Result<Vec<Source>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, host_label, machine_id, platform, config_json, created_at, updated_at
             FROM sources ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            let kind_str: String = row.get(1)?;
            let config_json_str: Option<String> = row.get(5)?;
            Ok(Source {
                id: row.get(0)?,
                kind: SourceKind::parse(&kind_str).unwrap_or_default(),
                host_label: row.get(2)?,
                machine_id: row.get(3)?,
                platform: row.get(4)?,
                config_json: config_json_str.and_then(|s| serde_json::from_str(&s).ok()),
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Get list of unique source IDs (for P4.4 TUI source filter menu).
    /// Returns source IDs ordered by ID, excluding 'local' which is always present.
    pub fn get_source_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT id FROM sources WHERE id != 'local' ORDER BY id")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Create or update a source.
    pub fn upsert_source(&self, source: &Source) -> Result<()> {
        let now = Self::now_millis();
        let config_json_str = source
            .config_json
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        self.conn.execute(
            "INSERT INTO sources(id, kind, host_label, machine_id, platform, config_json, created_at, updated_at)
             VALUES(?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                kind = excluded.kind,
                host_label = excluded.host_label,
                machine_id = excluded.machine_id,
                platform = excluded.platform,
                config_json = excluded.config_json,
                updated_at = excluded.updated_at",
            params![
                source.id,
                source.kind.as_str(),
                source.host_label,
                source.machine_id,
                source.platform,
                config_json_str,
                source.created_at.unwrap_or(now),
                now
            ],
        )?;
        Ok(())
    }

    /// Delete a source by ID.
    ///
    /// If `cascade` is true, also deletes all conversations from this source.
    /// Note: Currently conversations don't have a source_id column, so cascade
    /// is a no-op until P1.3 is implemented.
    pub fn delete_source(&self, id: &str, _cascade: bool) -> Result<bool> {
        // Prevent deletion of the local source
        if id == LOCAL_SOURCE_ID {
            return Err(anyhow!("cannot delete the local source"));
        }

        let rows_affected = self
            .conn
            .execute("DELETE FROM sources WHERE id = ?", params![id])?;

        Ok(rows_affected > 0)
    }

    // -------------------------------------------------------------------------
    // Daily Stats (Opt 3.2) - Materialized Aggregates for O(1) Range Queries
    // -------------------------------------------------------------------------

    /// Epoch offset: Days are counted from 2020-01-01 (Unix timestamp 1577836800).
    const EPOCH_2020_SECS: i64 = 1577836800;

    /// Convert a millisecond timestamp to a day_id (days since 2020-01-01).
    pub fn day_id_from_millis(timestamp_ms: i64) -> i64 {
        let secs = timestamp_ms / 1000;
        (secs - Self::EPOCH_2020_SECS).div_euclid(86400)
    }

    /// Convert a day_id back to a timestamp (milliseconds, start of day UTC).
    pub fn millis_from_day_id(day_id: i64) -> i64 {
        (Self::EPOCH_2020_SECS + day_id * 86400) * 1000
    }

    /// Get session count for a date range using materialized stats.
    /// Returns (count, is_from_cache) - is_from_cache is true if from daily_stats.
    ///
    /// If daily_stats table is empty or stale, falls back to COUNT(*) query.
    pub fn count_sessions_in_range(
        &self,
        start_ts_ms: Option<i64>,
        end_ts_ms: Option<i64>,
        agent_slug: Option<&str>,
        source_id: Option<&str>,
    ) -> Result<(i64, bool)> {
        let agent = agent_slug.unwrap_or("all");
        let source = source_id.unwrap_or("all");

        // Check if we have materialized stats
        let stats_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM daily_stats", [], |r| r.get(0))
            .unwrap_or(0);

        if stats_count == 0 {
            // Fall back to direct COUNT(*)
            return self.count_sessions_direct(start_ts_ms, end_ts_ms, agent_slug, source_id);
        }

        // Use materialized stats
        let start_day = start_ts_ms.map(Self::day_id_from_millis);
        let end_day = end_ts_ms.map(Self::day_id_from_millis);

        let count: i64 = match (start_day, end_day) {
            (Some(start), Some(end)) => self.conn.query_row(
                "SELECT COALESCE(SUM(session_count), 0) FROM daily_stats
                 WHERE day_id BETWEEN ? AND ? AND agent_slug = ? AND source_id = ?",
                params![start, end, agent, source],
                |r| r.get(0),
            )?,
            (Some(start), None) => self.conn.query_row(
                "SELECT COALESCE(SUM(session_count), 0) FROM daily_stats
                 WHERE day_id >= ? AND agent_slug = ? AND source_id = ?",
                params![start, agent, source],
                |r| r.get(0),
            )?,
            (None, Some(end)) => self.conn.query_row(
                "SELECT COALESCE(SUM(session_count), 0) FROM daily_stats
                 WHERE day_id <= ? AND agent_slug = ? AND source_id = ?",
                params![end, agent, source],
                |r| r.get(0),
            )?,
            (None, None) => self.conn.query_row(
                "SELECT COALESCE(SUM(session_count), 0) FROM daily_stats
                 WHERE agent_slug = ? AND source_id = ?",
                params![agent, source],
                |r| r.get(0),
            )?,
        };

        Ok((count, true))
    }

    /// Direct COUNT(*) query as fallback when daily_stats is empty.
    fn count_sessions_direct(
        &self,
        start_ts_ms: Option<i64>,
        end_ts_ms: Option<i64>,
        agent_slug: Option<&str>,
        source_id: Option<&str>,
    ) -> Result<(i64, bool)> {
        let mut sql = "SELECT COUNT(*) FROM conversations c
                       JOIN agents a ON c.agent_id = a.id WHERE 1=1"
            .to_string();
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(start) = start_ts_ms {
            sql.push_str(" AND c.started_at >= ?");
            params_vec.push(Box::new(start));
        }
        if let Some(end) = end_ts_ms {
            sql.push_str(" AND c.started_at <= ?");
            params_vec.push(Box::new(end));
        }
        if let Some(agent) = agent_slug
            && agent != "all"
        {
            sql.push_str(" AND a.slug = ?");
            params_vec.push(Box::new(agent.to_string()));
        }
        if let Some(source) = source_id
            && source != "all"
        {
            sql.push_str(" AND c.source_id = ?");
            params_vec.push(Box::new(source.to_string()));
        }

        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|b| b.as_ref()).collect();
        let count: i64 = self
            .conn
            .query_row(&sql, params_refs.as_slice(), |r| r.get(0))?;
        Ok((count, false))
    }

    /// Get daily histogram data for a date range.
    pub fn get_daily_histogram(
        &self,
        start_ts_ms: i64,
        end_ts_ms: i64,
        agent_slug: Option<&str>,
        source_id: Option<&str>,
    ) -> Result<Vec<DailyCount>> {
        let start_day = Self::day_id_from_millis(start_ts_ms);
        let end_day = Self::day_id_from_millis(end_ts_ms);
        let agent = agent_slug.unwrap_or("all");
        let source = source_id.unwrap_or("all");

        let mut stmt = self.conn.prepare(
            "SELECT day_id, session_count, message_count, total_chars
             FROM daily_stats
             WHERE day_id BETWEEN ? AND ? AND agent_slug = ? AND source_id = ?
             ORDER BY day_id",
        )?;

        let rows = stmt.query_map(params![start_day, end_day, agent, source], |row| {
            Ok(DailyCount {
                day_id: row.get(0)?,
                sessions: row.get(1)?,
                messages: row.get(2)?,
                chars: row.get(3)?,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Rebuild all daily stats from scratch.
    /// Use this for recovery or when stats appear to be out of sync.
    pub fn rebuild_daily_stats(&mut self) -> Result<DailyStatsRebuildResult> {
        let tx = self.conn.transaction()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);

        // Clear existing stats
        tx.execute("DELETE FROM daily_stats", [])?;

        // Rebuild from conversations table - per agent, per source
        // Note: COALESCE wraps the entire day_id calculation to match Rust's unwrap_or(0) behavior
        // for conversations with NULL started_at timestamps
        tx.execute(
            r"INSERT INTO daily_stats (day_id, agent_slug, source_id, session_count, message_count, total_chars, last_updated)
              SELECT
                  COALESCE(
                  CASE
                    WHEN (c.started_at / 1000 - 1577836800) >= 0 THEN (c.started_at / 1000 - 1577836800) / 86400
                    ELSE (c.started_at / 1000 - 1577836800 - 86399) / 86400
                  END,
                0) as day_id,
                  a.slug as agent_slug,
                  c.source_id,
                  COUNT(DISTINCT c.id) as session_count,
                  COUNT(m.id) as message_count,
                  COALESCE(SUM(LENGTH(m.content)), 0) as total_chars,
                  ? as last_updated
              FROM conversations c
              JOIN agents a ON c.agent_id = a.id
              LEFT JOIN messages m ON m.conversation_id = c.id
              GROUP BY day_id, a.slug, c.source_id",
            params![now],
        )?;

        // Add 'all' agent aggregates for each source
        tx.execute(
            r"INSERT INTO daily_stats (day_id, agent_slug, source_id, session_count, message_count, total_chars, last_updated)
              SELECT
                  COALESCE(
                  CASE
                    WHEN (c.started_at / 1000 - 1577836800) >= 0 THEN (c.started_at / 1000 - 1577836800) / 86400
                    ELSE (c.started_at / 1000 - 1577836800 - 86399) / 86400
                  END,
                0) as day_id,
                  'all',
                  c.source_id,
                  COUNT(DISTINCT c.id) as session_count,
                  COUNT(m.id) as message_count,
                  COALESCE(SUM(LENGTH(m.content)), 0) as total_chars,
                  ? as last_updated
              FROM conversations c
              LEFT JOIN messages m ON m.conversation_id = c.id
              GROUP BY day_id, c.source_id",
            params![now],
        )?;

        // Add per-agent aggregates for 'all' sources
        tx.execute(
            r"INSERT INTO daily_stats (day_id, agent_slug, source_id, session_count, message_count, total_chars, last_updated)
              SELECT
                  COALESCE(
                  CASE
                    WHEN (c.started_at / 1000 - 1577836800) >= 0 THEN (c.started_at / 1000 - 1577836800) / 86400
                    ELSE (c.started_at / 1000 - 1577836800 - 86399) / 86400
                  END,
                0) as day_id,
                  a.slug,
                  'all',
                  COUNT(DISTINCT c.id) as session_count,
                  COUNT(m.id) as message_count,
                  COALESCE(SUM(LENGTH(m.content)), 0) as total_chars,
                  ? as last_updated
              FROM conversations c
              JOIN agents a ON c.agent_id = a.id
              LEFT JOIN messages m ON m.conversation_id = c.id
              GROUP BY day_id, a.slug",
            params![now],
        )?;

        // Add global 'all'/'all' aggregates
        tx.execute(
            r"INSERT INTO daily_stats (day_id, agent_slug, source_id, session_count, message_count, total_chars, last_updated)
              SELECT
                  COALESCE(
                  CASE
                    WHEN (c.started_at / 1000 - 1577836800) >= 0 THEN (c.started_at / 1000 - 1577836800) / 86400
                    ELSE (c.started_at / 1000 - 1577836800 - 86399) / 86400
                  END,
                0) as day_id,
                  'all',
                  'all',
                  COUNT(DISTINCT c.id) as session_count,
                  COUNT(m.id) as message_count,
                  COALESCE(SUM(LENGTH(m.content)), 0) as total_chars,
                  ? as last_updated
              FROM conversations c
              LEFT JOIN messages m ON m.conversation_id = c.id
              GROUP BY day_id",
            params![now],
        )?;

        let rows_created: i64 =
            tx.query_row("SELECT COUNT(*) FROM daily_stats", [], |r| r.get(0))?;
        let total_sessions: i64 = tx.query_row(
            "SELECT COALESCE(SUM(session_count), 0) FROM daily_stats WHERE agent_slug = 'all' AND source_id = 'all'",
            [],
            |r| r.get(0),
        )?;

        tx.commit()?;

        tracing::info!(
            target: "cass::perf::daily_stats",
            rows_created = rows_created,
            total_sessions = total_sessions,
            "Daily stats rebuilt from conversations"
        );

        Ok(DailyStatsRebuildResult {
            rows_created,
            total_sessions,
        })
    }

    /// Flush aggregated stats deltas to daily_stats table in a single batch.
    ///
    /// Uses multi-value INSERT with ON CONFLICT for efficient upserts.
    /// This is the batched alternative to `update_daily_stats_in_tx` which
    /// does 4 writes per conversation.
    ///
    /// # Arguments
    /// * `entries` - Expanded entries from `StatsAggregator::expand()`.
    ///   Each tuple is (day_id, agent_slug, source_id, delta).
    ///
    /// # Returns
    /// Number of rows affected (inserted + updated).
    pub fn update_daily_stats_batched(
        &mut self,
        entries: &[(i64, String, String, StatsDelta)],
    ) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let now = Self::now_millis();
        let tx = self.conn.transaction()?;

        // SQLite supports up to 999 variables per statement (though 32766 in newer versions).
        // With 7 variables per row, we can safely batch ~100 rows.
        const BATCH_SIZE: usize = 100;
        let mut total_affected = 0;

        for chunk in entries.chunks(BATCH_SIZE) {
            // Build multi-value INSERT statement
            let placeholders: String = (0..chunk.len())
                .map(|_| "(?, ?, ?, ?, ?, ?, ?)")
                .collect::<Vec<_>>()
                .join(", ");

            let sql = format!(
                "INSERT INTO daily_stats (day_id, agent_slug, source_id, session_count, message_count, total_chars, last_updated)
                 VALUES {}
                 ON CONFLICT(day_id, agent_slug, source_id) DO UPDATE SET
                     session_count = session_count + excluded.session_count,
                     message_count = message_count + excluded.message_count,
                     total_chars = total_chars + excluded.total_chars,
                     last_updated = excluded.last_updated",
                placeholders
            );

            // Flatten parameters for rusqlite
            let mut params_vec: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() * 7);

            for (day_id, agent, source, delta) in chunk {
                params_vec.push((*day_id).into());
                params_vec.push(agent.clone().into());
                params_vec.push(source.clone().into());
                params_vec.push(delta.session_count_delta.into());
                params_vec.push(delta.message_count_delta.into());
                params_vec.push(delta.total_chars_delta.into());
                params_vec.push(now.into());
            }

            let affected = tx.execute(&sql, rusqlite::params_from_iter(params_vec))?;
            total_affected += affected;
        }

        tx.commit()?;

        tracing::debug!(
            target: "cass::perf::daily_stats",
            entries = entries.len(),
            affected = total_affected,
            "batched_stats_update_complete"
        );

        Ok(total_affected)
    }

    /// Check if daily_stats are populated and reasonably fresh.
    pub fn daily_stats_health(&self) -> Result<DailyStatsHealth> {
        let row_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM daily_stats", [], |r| r.get(0))
            .unwrap_or(0);

        let oldest_update: Option<i64> = self
            .conn
            .query_row("SELECT MIN(last_updated) FROM daily_stats", [], |r| {
                r.get(0)
            })
            .ok();

        let conversation_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
            .unwrap_or(0);

        // Get materialized total
        let materialized_total: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(session_count), 0) FROM daily_stats
                 WHERE agent_slug = 'all' AND source_id = 'all'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        Ok(DailyStatsHealth {
            populated: row_count > 0,
            row_count,
            oldest_update_ms: oldest_update,
            conversation_count,
            materialized_total,
            drift: (conversation_count - materialized_total).abs(),
        })
    }
}

/// Daily count data for histogram display.
#[derive(Debug, Clone)]
pub struct DailyCount {
    pub day_id: i64,
    pub sessions: i64,
    pub messages: i64,
    pub chars: i64,
}

/// Result of rebuilding daily stats.
#[derive(Debug, Clone)]
pub struct DailyStatsRebuildResult {
    pub rows_created: i64,
    pub total_sessions: i64,
}

/// Health status of daily stats table.
#[derive(Debug, Clone)]
pub struct DailyStatsHealth {
    pub populated: bool,
    pub row_count: i64,
    pub oldest_update_ms: Option<i64>,
    pub conversation_count: i64,
    pub materialized_total: i64,
    pub drift: i64,
}

/// Update daily stats within a transaction.
/// Handles incrementing session_count, message_count, and total_chars for:
/// - Specific agent + source
/// - All agents + specific source
/// - Specific agent + all sources
/// - All agents + all sources
fn update_daily_stats_in_tx(
    tx: &Transaction<'_>,
    agent_slug: &str,
    source_id: &str,
    started_at_ms: Option<i64>,
    session_count_delta: i64,
    message_count: i64,
    total_chars: i64,
) -> Result<()> {
    if session_count_delta == 0 && message_count == 0 && total_chars == 0 {
        return Ok(());
    }

    let day_id = started_at_ms
        .map(SqliteStorage::day_id_from_millis)
        .unwrap_or(0);
    let now = SqliteStorage::now_millis();

    let mut unique_updates = Vec::with_capacity(4);

    // Add specific entry if neither is "all"
    if agent_slug != "all" && source_id != "all" {
        unique_updates.push((agent_slug, source_id));
    }

    // Add "all agents" entry for this source
    if source_id != "all" {
        unique_updates.push(("all", source_id));
    }

    // Add "all sources" entry for this agent
    if agent_slug != "all" {
        unique_updates.push((agent_slug, "all"));
    }

    // Always add global total
    unique_updates.push(("all", "all"));

    for (agent, source) in unique_updates {
        tx.execute(
            "INSERT INTO daily_stats (day_id, agent_slug, source_id, session_count, message_count, total_chars, last_updated)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(day_id, agent_slug, source_id) DO UPDATE SET
                 session_count = session_count + excluded.session_count,
                 message_count = message_count + excluded.message_count,
                 total_chars = total_chars + excluded.total_chars,
                 last_updated = excluded.last_updated",
            params![day_id, agent, source, session_count_delta, message_count, total_chars, now],
        )?;
    }

    Ok(())
}

fn apply_pragmas(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(
        r"
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA wal_autocheckpoint = 1000;
        ",
    )?;
    apply_common_pragmas(conn)
}

fn apply_common_pragmas(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(
        r"
        PRAGMA temp_store = MEMORY;
        PRAGMA cache_size = -65536; -- 64MB
        PRAGMA mmap_size = 268435456; -- 256MB
        PRAGMA foreign_keys = ON;
        ",
    )?;
    Ok(())
}

fn init_meta(conn: &mut Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        [],
    )?;

    let existing: Option<i64> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0).map(|s| s.parse().unwrap_or(0)),
        )
        .optional()?;

    if existing.is_none() {
        // Start at version 0 so migrate() applies full schema on first open.
        conn.execute(
            "INSERT INTO meta(key, value) VALUES('schema_version', 0)",
            [],
        )?;
    }

    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0).map(|s| s.parse().unwrap_or(0)),
        )
        .optional()?
        .unwrap_or(0);

    if current == SCHEMA_VERSION {
        return Ok(());
    }

    // Disable foreign keys for the migration transaction (needed for V5 table recreation).
    // PRAGMA foreign_keys is a no-op inside a transaction, so we must set it before.
    conn.execute("PRAGMA foreign_keys = OFF", [])?;

    let tx = conn.transaction()?;

    match current {
        0 => {
            tx.execute_batch(MIGRATION_V1)?;
            tx.execute_batch(MIGRATION_V2)?;
            tx.execute_batch(MIGRATION_V3)?;
            tx.execute_batch(MIGRATION_V4)?;
            tx.execute_batch(MIGRATION_V5)?;
            tx.execute_batch(MIGRATION_V6)?;
            tx.execute_batch(MIGRATION_V7)?;
            tx.execute_batch(MIGRATION_V8)?;
            tx.execute_batch(MIGRATION_V9)?;
            tx.execute_batch(MIGRATION_V10)?;
        }
        1 => {
            tx.execute_batch(MIGRATION_V2)?;
            tx.execute_batch(MIGRATION_V3)?;
            tx.execute_batch(MIGRATION_V4)?;
            tx.execute_batch(MIGRATION_V5)?;
            tx.execute_batch(MIGRATION_V6)?;
            tx.execute_batch(MIGRATION_V7)?;
            tx.execute_batch(MIGRATION_V8)?;
            tx.execute_batch(MIGRATION_V9)?;
            tx.execute_batch(MIGRATION_V10)?;
        }
        2 => {
            tx.execute_batch(MIGRATION_V3)?;
            tx.execute_batch(MIGRATION_V4)?;
            tx.execute_batch(MIGRATION_V5)?;
            tx.execute_batch(MIGRATION_V6)?;
            tx.execute_batch(MIGRATION_V7)?;
            tx.execute_batch(MIGRATION_V8)?;
            tx.execute_batch(MIGRATION_V9)?;
            tx.execute_batch(MIGRATION_V10)?;
        }
        3 => {
            tx.execute_batch(MIGRATION_V4)?;
            tx.execute_batch(MIGRATION_V5)?;
            tx.execute_batch(MIGRATION_V6)?;
            tx.execute_batch(MIGRATION_V7)?;
            tx.execute_batch(MIGRATION_V8)?;
            tx.execute_batch(MIGRATION_V9)?;
            tx.execute_batch(MIGRATION_V10)?;
        }
        4 => {
            tx.execute_batch(MIGRATION_V5)?;
            tx.execute_batch(MIGRATION_V6)?;
            tx.execute_batch(MIGRATION_V7)?;
            tx.execute_batch(MIGRATION_V8)?;
            tx.execute_batch(MIGRATION_V9)?;
            tx.execute_batch(MIGRATION_V10)?;
        }
        5 => {
            tx.execute_batch(MIGRATION_V6)?;
            tx.execute_batch(MIGRATION_V7)?;
            tx.execute_batch(MIGRATION_V8)?;
            tx.execute_batch(MIGRATION_V9)?;
            tx.execute_batch(MIGRATION_V10)?;
        }
        6 => {
            tx.execute_batch(MIGRATION_V7)?;
            tx.execute_batch(MIGRATION_V8)?;
            tx.execute_batch(MIGRATION_V9)?;
            tx.execute_batch(MIGRATION_V10)?;
        }
        7 => {
            tx.execute_batch(MIGRATION_V8)?;
            tx.execute_batch(MIGRATION_V9)?;
            tx.execute_batch(MIGRATION_V10)?;
        }
        8 => {
            tx.execute_batch(MIGRATION_V9)?;
            tx.execute_batch(MIGRATION_V10)?;
        }
        9 => {
            tx.execute_batch(MIGRATION_V10)?;
        }
        v => return Err(anyhow!("unsupported schema version {v}")),
    }

    tx.execute(
        "UPDATE meta SET value = ? WHERE key = 'schema_version'",
        params![SCHEMA_VERSION.to_string()],
    )?;

    tx.commit()?;

    // Re-enable foreign keys after migration
    conn.execute("PRAGMA foreign_keys = ON", [])?;

    Ok(())
}

fn insert_conversation(
    tx: &Transaction<'_>,
    agent_id: i64,
    workspace_id: Option<i64>,
    conv: &Conversation,
) -> Result<i64> {
    // Serialize metadata to both JSON (for compatibility) and binary (for efficiency)
    let metadata_bin = serialize_json_to_msgpack(&conv.metadata_json);

    tx.execute(
        "INSERT INTO conversations(
            agent_id, workspace_id, source_id, external_id, title, source_path,
            started_at, ended_at, approx_tokens, metadata_json, origin_host, metadata_bin
        ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            agent_id,
            workspace_id,
            &conv.source_id,
            conv.external_id,
            conv.title,
            path_to_string(&conv.source_path),
            conv.started_at,
            conv.ended_at,
            conv.approx_tokens,
            serde_json::to_string(&conv.metadata_json)?,
            conv.origin_host,
            metadata_bin
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

fn insert_message(tx: &Transaction<'_>, conversation_id: i64, msg: &Message) -> Result<i64> {
    // Serialize extra to both JSON (for compatibility) and binary (for efficiency)
    let extra_bin = serialize_json_to_msgpack(&msg.extra_json);

    tx.execute(
        "INSERT INTO messages(conversation_id, idx, role, author, created_at, content, extra_json, extra_bin)
         VALUES(?,?,?,?,?,?,?,?)",
        params![
            conversation_id,
            msg.idx,
            role_str(&msg.role),
            msg.author,
            msg.created_at,
            msg.content,
            serde_json::to_string(&msg.extra_json)?,
            extra_bin
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

fn insert_snippets(tx: &Transaction<'_>, message_id: i64, snippets: &[Snippet]) -> Result<()> {
    for snip in snippets {
        tx.execute(
            "INSERT INTO snippets(message_id, file_path, start_line, end_line, language, snippet_text)
             VALUES(?,?,?,?,?,?)",
            params![
                message_id,
                snip.file_path.as_ref().map(path_to_string),
                snip.start_line,
                snip.end_line,
                snip.language,
                snip.snippet_text,
            ],
        )?;
    }
    Ok(())
}

// -------------------------------------------------------------------------
// FTS5 Batch Insert (P2 Opt 2.1)
// -------------------------------------------------------------------------

/// Batch size for FTS5 inserts. With 7 columns per row and SQLite's
/// SQLITE_MAX_VARIABLE_NUMBER default of 999, max batch is ~142 rows.
/// Using 100 for safety margin and memory efficiency.
const FTS5_BATCH_SIZE: usize = 100;

/// Entry for pending FTS5 insert.
#[derive(Debug, Clone)]
pub struct FtsEntry {
    pub content: String,
    pub title: String,
    pub agent: String,
    pub workspace: String,
    pub source_path: String,
    pub created_at: Option<i64>,
    pub message_id: i64,
}

impl FtsEntry {
    /// Create an FTS entry from a message and conversation.
    pub fn from_message(message_id: i64, msg: &Message, conv: &Conversation) -> Self {
        FtsEntry {
            content: msg.content.clone(),
            title: conv.title.clone().unwrap_or_default(),
            agent: conv.agent_slug.clone(),
            workspace: conv
                .workspace
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            source_path: path_to_string(&conv.source_path),
            created_at: msg.created_at.or(conv.started_at),
            message_id,
        }
    }
}

/// Batch insert FTS5 entries for better performance.
///
/// Uses multi-value INSERT to reduce transaction overhead and
/// SQLite statement preparation costs.
fn batch_insert_fts_messages(tx: &Transaction<'_>, entries: &[FtsEntry]) -> Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }

    let mut inserted = 0;

    for chunk in entries.chunks(FTS5_BATCH_SIZE) {
        // Build multi-value INSERT
        let placeholders: String = chunk
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let base = i * 7;
                format!(
                    "(?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{})",
                    base + 1,
                    base + 2,
                    base + 3,
                    base + 4,
                    base + 5,
                    base + 6,
                    base + 7
                )
            })
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT INTO fts_messages(content, title, agent, workspace, source_path, created_at, message_id) VALUES {}",
            placeholders
        );

        // Flatten parameters
        // Capacity: chunk.len() * 7
        let mut params_refs: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 7);
        for entry in chunk {
            params_refs.push(&entry.content);
            params_refs.push(&entry.title);
            params_refs.push(&entry.agent);
            params_refs.push(&entry.workspace);
            params_refs.push(&entry.source_path);
            params_refs.push(&entry.created_at);
            params_refs.push(&entry.message_id);
        }

        if let Err(e) = tx.execute(&sql, params_refs.as_slice()) {
            // FTS is best-effort; log and continue
            tracing::debug!(
                batch_size = chunk.len(),
                error = %e,
                "fts_batch_insert_failed"
            );
            // Fall back to individual inserts for this batch
            for entry in chunk {
                if let Err(e2) = tx.execute(
                    "INSERT INTO fts_messages(content, title, agent, workspace, source_path, created_at, message_id)
                     VALUES(?,?,?,?,?,?,?)",
                    params![
                        entry.content,
                        entry.title,
                        entry.agent,
                        entry.workspace,
                        entry.source_path,
                        entry.created_at,
                        entry.message_id
                    ],
                ) {
                    tracing::debug!(
                        message_id = entry.message_id,
                        error = %e2,
                        "fts_insert_skipped"
                    );
                } else {
                    inserted += 1;
                }
            }
        } else {
            inserted += chunk.len();
        }
    }

    Ok(inserted)
}

/// Insert or update a single conversation within an existing transaction.
/// Used by insert_conversations_batched to process multiple conversations efficiently.
/// Collects FTS entries into the provided vector for batch insertion.
fn insert_conversation_in_tx_batched(
    tx: &Transaction<'_>,
    agent_id: i64,
    workspace_id: Option<i64>,
    conv: &Conversation,
    fts_entries: &mut Vec<FtsEntry>,
) -> Result<(InsertOutcome, StatsDelta)> {
    // Check for existing conversation with same (source_id, agent_id, external_id)
    if let Some(ext) = &conv.external_id {
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM conversations WHERE source_id = ? AND agent_id = ? AND external_id = ?",
                params![&conv.source_id, agent_id, ext],
                |row| row.get(0),
            )
            .optional()?;

        if let Some(conversation_id) = existing {
            // Append messages to existing conversation
            let max_idx: Option<i64> = tx.query_row(
                "SELECT MAX(idx) FROM messages WHERE conversation_id = ?",
                params![conversation_id],
                |row| row.get::<_, Option<i64>>(0),
            )?;
            let cutoff = max_idx.unwrap_or(-1);

            let mut inserted_indices = Vec::new();
            let mut new_chars: i64 = 0;
            for msg in &conv.messages {
                if msg.idx <= cutoff {
                    continue;
                }
                let msg_id = insert_message(tx, conversation_id, msg)?;
                insert_snippets(tx, msg_id, &msg.snippets)?;
                // Collect FTS entry instead of inserting immediately
                fts_entries.push(FtsEntry::from_message(msg_id, msg, conv));
                inserted_indices.push(msg.idx);
                new_chars += msg.content.len() as i64;
            }

            // Update metadata fields and ended_at
            if !inserted_indices.is_empty() {
                // Update ended_at
                if let Some(last_ts) = conv.messages.iter().filter_map(|m| m.created_at).max() {
                    tx.execute(
                        "UPDATE conversations SET ended_at = MAX(IFNULL(ended_at, 0), ?) WHERE id = ?",
                        params![last_ts, conversation_id],
                    )?;
                }

                // Update metadata, approx_tokens, etc.
                // We overwrite with new metadata assuming the scanner produces complete/updated metadata.
                let metadata_bin = serialize_json_to_msgpack(&conv.metadata_json);
                tx.execute(
                    "UPDATE conversations SET 
                        title = COALESCE(?, title),
                        approx_tokens = COALESCE(?, approx_tokens),
                        metadata_json = ?,
                        metadata_bin = ?,
                        origin_host = COALESCE(?, origin_host)
                     WHERE id = ?",
                    params![
                        conv.title,
                        conv.approx_tokens,
                        serde_json::to_string(&conv.metadata_json)?,
                        metadata_bin,
                        conv.origin_host,
                        conversation_id
                    ],
                )?;

                // Note: Daily stats update skipped here to prevent double counting.
                // The caller (ingest_batch) handles stats aggregation efficiently.
            }

            let delta = StatsDelta {
                session_count_delta: 0,
                message_count_delta: inserted_indices.len() as i64,
                total_chars_delta: new_chars,
            };

            return Ok((
                InsertOutcome {
                    conversation_id,
                    inserted_indices,
                },
                delta,
            ));
        }
    }

    // Insert new conversation
    let conv_id = insert_conversation(tx, agent_id, workspace_id, conv)?;
    let mut total_chars: i64 = 0;
    for msg in &conv.messages {
        let msg_id = insert_message(tx, conv_id, msg)?;
        insert_snippets(tx, msg_id, &msg.snippets)?;
        // Collect FTS entry instead of inserting immediately
        fts_entries.push(FtsEntry::from_message(msg_id, msg, conv));
        total_chars += msg.content.len() as i64;
    }

    // Note: Daily stats update skipped here to prevent double counting.
    // The caller (ingest_batch) handles stats aggregation efficiently.

    let delta = StatsDelta {
        session_count_delta: 1,
        message_count_delta: conv.messages.len() as i64,
        total_chars_delta: total_chars,
    };

    Ok((
        InsertOutcome {
            conversation_id: conv_id,
            inserted_indices: conv.messages.iter().map(|m| m.idx).collect(),
        },
        delta,
    ))
}

/// Upsert daily_stats deltas inside an existing transaction.
///
/// This mirrors `SqliteStorage::update_daily_stats_batched` but avoids starting a
/// nested transaction so callers can keep all writes (conversations/messages/fts/stats)
/// atomic.
fn update_daily_stats_batched_in_tx(
    tx: &Transaction<'_>,
    entries: &[(i64, String, String, StatsDelta)],
) -> Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }

    let now = SqliteStorage::now_millis();
    const BATCH_SIZE: usize = 100;
    let mut total_affected = 0;

    for chunk in entries.chunks(BATCH_SIZE) {
        let placeholders: String = (0..chunk.len())
            .map(|_| "(?, ?, ?, ?, ?, ?, ?)")
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT INTO daily_stats (day_id, agent_slug, source_id, session_count, message_count, total_chars, last_updated)
             VALUES {}
             ON CONFLICT(day_id, agent_slug, source_id) DO UPDATE SET
                 session_count = session_count + excluded.session_count,
                 message_count = message_count + excluded.message_count,
                 total_chars = total_chars + excluded.total_chars,
                 last_updated = excluded.last_updated",
            placeholders
        );

        let mut params_vec: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() * 7);
        for (day_id, agent, source, delta) in chunk {
            params_vec.push((*day_id).into());
            params_vec.push(agent.clone().into());
            params_vec.push(source.clone().into());
            params_vec.push(delta.session_count_delta.into());
            params_vec.push(delta.message_count_delta.into());
            params_vec.push(delta.total_chars_delta.into());
            params_vec.push(now.into());
        }

        total_affected += tx.execute(&sql, rusqlite::params_from_iter(params_vec))?;
    }

    Ok(total_affected)
}

fn path_to_string<P: AsRef<Path>>(p: P) -> String {
    p.as_ref().to_string_lossy().into_owned()
}

fn role_str(role: &MessageRole) -> String {
    match role {
        MessageRole::User => "user".to_owned(),
        MessageRole::Agent => "agent".to_owned(),
        MessageRole::Tool => "tool".to_owned(),
        MessageRole::System => "system".to_owned(),
        MessageRole::Other(v) => v.clone(),
    }
}

fn agent_kind_str(kind: AgentKind) -> String {
    match kind {
        AgentKind::Cli => "cli".into(),
        AgentKind::VsCode => "vscode".into(),
        AgentKind::Hybrid => "hybrid".into(),
    }
}

// =============================================================================
// Tests (bead yln.4)
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // =========================================================================
    // User data file protection tests (bead yln.4)
    // =========================================================================

    #[test]
    fn is_user_data_file_detects_bookmarks() {
        assert!(is_user_data_file(Path::new("/data/bookmarks.db")));
        assert!(is_user_data_file(Path::new("bookmarks.db")));
    }

    #[test]
    fn is_user_data_file_detects_tui_state() {
        assert!(is_user_data_file(Path::new("/data/tui_state.json")));
    }

    #[test]
    fn is_user_data_file_detects_sources_toml() {
        assert!(is_user_data_file(Path::new("/config/sources.toml")));
    }

    #[test]
    fn is_user_data_file_detects_env() {
        assert!(is_user_data_file(Path::new(".env")));
    }

    #[test]
    fn is_user_data_file_rejects_other_files() {
        assert!(!is_user_data_file(Path::new("index.db")));
        assert!(!is_user_data_file(Path::new("conversations.db")));
        assert!(!is_user_data_file(Path::new("random.txt")));
    }

    // =========================================================================
    // Backup creation tests (bead yln.4)
    // =========================================================================

    #[test]
    fn create_backup_returns_none_for_nonexistent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("nonexistent.db");
        let result = create_backup(&db_path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn create_backup_creates_timestamped_file() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        std::fs::write(&db_path, b"test data").unwrap();

        let backup_path = create_backup(&db_path).unwrap();
        assert!(backup_path.is_some());
        let backup = backup_path.unwrap();
        assert!(backup.exists());
        assert!(
            backup
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains("backup")
        );
    }

    #[test]
    fn create_backup_preserves_content() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let original_content = b"test database content 12345";
        std::fs::write(&db_path, original_content).unwrap();

        let backup_path = create_backup(&db_path).unwrap().unwrap();
        let backup_content = std::fs::read(&backup_path).unwrap();
        assert_eq!(backup_content, original_content);
    }

    // =========================================================================
    // Backup cleanup tests (bead yln.4)
    // =========================================================================

    #[test]
    fn cleanup_old_backups_keeps_recent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");

        // Create 5 backup files with different timestamps
        for i in 0..5 {
            let backup_name = format!("test.db.backup.{}", 1000 + i);
            std::fs::write(dir.path().join(&backup_name), format!("backup {i}")).unwrap();
        }

        cleanup_old_backups(&db_path, 3).unwrap();

        // Count remaining backup files
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_str().unwrap_or("").contains("backup"))
            .collect();

        assert!(backups.len() <= 3);
    }

    // =========================================================================
    // Storage open/create tests (bead yln.4)
    // =========================================================================

    #[test]
    fn open_creates_new_database() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("new.db");
        assert!(!db_path.exists());

        let storage = SqliteStorage::open(&db_path).unwrap();
        assert!(db_path.exists());
        drop(storage);
    }

    #[test]
    fn open_readonly_fails_for_nonexistent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("nonexistent.db");
        let result = SqliteStorage::open_readonly(&db_path);
        assert!(result.is_err());
    }

    #[test]
    fn open_readonly_succeeds_for_existing() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("existing.db");

        // Create first
        let _storage = SqliteStorage::open(&db_path).unwrap();
        drop(_storage);

        // Now open readonly
        let storage = SqliteStorage::open_readonly(&db_path).unwrap();
        assert!(storage.schema_version().is_ok());
    }

    // =========================================================================
    // Schema version tests (bead yln.4)
    // =========================================================================

    #[test]
    fn schema_version_returns_current() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let version = storage.schema_version().unwrap();
        assert!(version >= 5, "Schema version should be at least 5");
    }

    // =========================================================================
    // Agent storage tests (bead yln.4)
    // =========================================================================

    #[test]
    fn ensure_agent_creates_new() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "test_agent".into(),
            name: "Test Agent".into(),
            version: Some("1.0".into()),
            kind: AgentKind::Cli,
        };

        let id = storage.ensure_agent(&agent).unwrap();
        assert!(id > 0);
    }

    #[test]
    fn ensure_agent_returns_existing_id() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: None,
            kind: AgentKind::Cli,
        };

        let id1 = storage.ensure_agent(&agent).unwrap();
        let id2 = storage.ensure_agent(&agent).unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn list_agents_returns_inserted() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "new_agent".into(),
            name: "New Agent".into(),
            version: None,
            kind: AgentKind::VsCode,
        };
        storage.ensure_agent(&agent).unwrap();

        let agents = storage.list_agents().unwrap();
        assert!(agents.iter().any(|a| a.slug == "new_agent"));
    }

    // =========================================================================
    // Workspace storage tests (bead yln.4)
    // =========================================================================

    #[test]
    fn ensure_workspace_creates_new() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let id = storage
            .ensure_workspace(Path::new("/home/user/project"), Some("My Project"))
            .unwrap();
        assert!(id > 0);
    }

    #[test]
    fn ensure_workspace_returns_existing() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let path = Path::new("/home/user/myproject");
        let id1 = storage.ensure_workspace(path, None).unwrap();
        let id2 = storage.ensure_workspace(path, None).unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn list_workspaces_returns_inserted() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        storage
            .ensure_workspace(Path::new("/test/workspace"), Some("Test WS"))
            .unwrap();

        let workspaces = storage.list_workspaces().unwrap();
        assert!(
            workspaces
                .iter()
                .any(|w| w.path.to_str() == Some("/test/workspace"))
        );
    }

    // =========================================================================
    // Source storage tests (bead yln.4)
    // =========================================================================

    #[test]
    fn upsert_source_creates_new() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let source = Source {
            id: "test-laptop".into(),
            kind: SourceKind::Ssh,
            host_label: Some("test.local".into()),
            machine_id: Some("test-machine-id".into()),
            platform: None,
            config_json: None,
            created_at: Some(SqliteStorage::now_millis()),
            updated_at: None,
        };

        storage.upsert_source(&source).unwrap();
        let fetched = storage.get_source("test-laptop").unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().host_label, Some("test.local".into()));
    }

    #[test]
    fn upsert_source_updates_existing() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let source1 = Source {
            id: "my-source".into(),
            kind: SourceKind::Ssh,
            host_label: Some("Original Label".into()),
            machine_id: None,
            platform: None,
            config_json: None,
            created_at: Some(SqliteStorage::now_millis()),
            updated_at: None,
        };
        storage.upsert_source(&source1).unwrap();

        let source2 = Source {
            id: "my-source".into(),
            kind: SourceKind::Ssh,
            host_label: Some("Updated Label".into()),
            machine_id: None,
            platform: Some("linux".into()),
            config_json: None,
            created_at: Some(SqliteStorage::now_millis()),
            updated_at: Some(SqliteStorage::now_millis()),
        };
        storage.upsert_source(&source2).unwrap();

        let fetched = storage.get_source("my-source").unwrap().unwrap();
        assert_eq!(fetched.host_label, Some("Updated Label".into()));
        assert!(fetched.platform.is_some());
    }

    #[test]
    fn delete_source_removes_entry() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let source = Source {
            id: "to-delete".into(),
            kind: SourceKind::Local,
            host_label: None,
            machine_id: None,
            platform: None,
            config_json: None,
            created_at: Some(SqliteStorage::now_millis()),
            updated_at: None,
        };
        storage.upsert_source(&source).unwrap();

        let deleted = storage.delete_source("to-delete", false).unwrap();
        assert!(deleted);

        let fetched = storage.get_source("to-delete").unwrap();
        assert!(fetched.is_none());
    }

    #[test]
    fn delete_source_cannot_delete_local() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let result = storage.delete_source(LOCAL_SOURCE_ID, false);
        assert!(result.is_err());
    }

    #[test]
    fn list_sources_includes_local() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let sources = storage.list_sources().unwrap();
        assert!(sources.iter().any(|s| s.id == LOCAL_SOURCE_ID));
    }

    #[test]
    fn get_source_ids_excludes_local() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        // Add a non-local source
        let source = Source {
            id: "remote-1".into(),
            kind: SourceKind::Ssh,
            host_label: Some("server".into()),
            machine_id: None,
            platform: None,
            config_json: None,
            created_at: Some(SqliteStorage::now_millis()),
            updated_at: None,
        };
        storage.upsert_source(&source).unwrap();

        let ids = storage.get_source_ids().unwrap();
        assert!(!ids.contains(&LOCAL_SOURCE_ID.to_string()));
        assert!(ids.contains(&"remote-1".to_string()));
    }

    // =========================================================================
    // Scan timestamp tests (bead yln.4)
    // =========================================================================

    #[test]
    fn get_last_scan_ts_returns_none_initially() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let ts = storage.get_last_scan_ts().unwrap();
        assert!(ts.is_none());
    }

    #[test]
    fn set_and_get_last_scan_ts() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();

        let expected_ts = 1700000000000_i64;
        storage.set_last_scan_ts(expected_ts).unwrap();

        let actual_ts = storage.get_last_scan_ts().unwrap();
        assert_eq!(actual_ts, Some(expected_ts));
    }

    // =========================================================================
    // now_millis utility test (bead yln.4)
    // =========================================================================

    #[test]
    fn now_millis_returns_reasonable_value() {
        let ts = SqliteStorage::now_millis();
        // Should be after Jan 1, 2020 (approx 1577836800000)
        assert!(ts > 1577836800000);
        // Should be before Jan 1, 2100 (approx 4102444800000)
        assert!(ts < 4102444800000);
    }

    // =========================================================================
    // Binary Metadata Serialization Tests (Opt 3.1)
    // =========================================================================

    #[test]
    fn msgpack_roundtrip_basic_object() {
        let value = serde_json::json!({
            "key": "value",
            "number": 42,
            "nested": { "inner": true }
        });

        let bytes = serialize_json_to_msgpack(&value).expect("should serialize");
        let recovered = deserialize_msgpack_to_json(&bytes);

        assert_eq!(value, recovered);
    }

    #[test]
    fn msgpack_returns_none_for_null() {
        let value = serde_json::Value::Null;
        assert!(serialize_json_to_msgpack(&value).is_none());
    }

    #[test]
    fn msgpack_returns_none_for_empty_object() {
        let value = serde_json::json!({});
        assert!(serialize_json_to_msgpack(&value).is_none());
    }

    #[test]
    fn msgpack_serializes_non_empty_array() {
        let value = serde_json::json!([1, 2, 3]);
        let bytes = serialize_json_to_msgpack(&value).expect("should serialize array");
        let recovered = deserialize_msgpack_to_json(&bytes);
        assert_eq!(value, recovered);
    }

    #[test]
    fn msgpack_smaller_than_json() {
        let value = serde_json::json!({
            "field_name_one": "some_value",
            "field_name_two": 123456,
            "field_name_three": [1, 2, 3, 4, 5],
            "field_name_four": { "nested": true }
        });

        let json_bytes = serde_json::to_vec(&value).unwrap();
        let msgpack_bytes = serialize_json_to_msgpack(&value).unwrap();

        // MessagePack should be smaller due to more compact encoding
        assert!(
            msgpack_bytes.len() < json_bytes.len(),
            "MessagePack ({} bytes) should be smaller than JSON ({} bytes)",
            msgpack_bytes.len(),
            json_bytes.len()
        );
    }

    #[test]
    fn migration_v7_adds_binary_columns() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        // Verify metadata_bin column exists
        let has_metadata_bin: bool = storage
            .raw()
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('conversations') WHERE name = 'metadata_bin'",
                [],
                |r| r.get::<_, i64>(0).map(|c| c > 0),
            )
            .unwrap();
        assert!(
            has_metadata_bin,
            "conversations should have metadata_bin column"
        );

        // Verify extra_bin column exists
        let has_extra_bin: bool = storage
            .raw()
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = 'extra_bin'",
                [],
                |r| r.get::<_, i64>(0).map(|c| c > 0),
            )
            .unwrap();
        assert!(has_extra_bin, "messages should have extra_bin column");
    }

    #[test]
    fn msgpack_deserialize_empty_returns_default() {
        let recovered = deserialize_msgpack_to_json(&[]);
        assert_eq!(recovered, serde_json::Value::Object(serde_json::Map::new()));
    }

    #[test]
    fn msgpack_deserialize_garbage_returns_default() {
        // Use truncated msgpack data that will fail to parse
        // 0x85 indicates a fixmap with 5 elements, but we don't provide them
        let recovered = deserialize_msgpack_to_json(&[0x85]);
        assert_eq!(recovered, serde_json::Value::Object(serde_json::Map::new()));
    }

    #[test]
    fn stats_aggregator_collects_and_expands() {
        let mut agg = StatsAggregator::new();
        assert!(agg.is_empty());

        // Record some stats
        // Day 100, agent "claude", source "local"
        agg.record("claude", "local", 100, 5, 500);
        // Day 100, agent "codex", source "local"
        agg.record("codex", "local", 100, 3, 300);
        // Day 101, agent "claude", source "local"
        agg.record("claude", "local", 101, 2, 200);

        assert!(!agg.is_empty());
        assert_eq!(agg.raw_entry_count(), 3);

        let entries = agg.expand();
        // Each raw entry expands to 4 permutations.
        // But (all, local) and (all, all) will aggregate.
        //
        // Raw:
        // 1. (100, claude, local) -> 1 sess, 5 msgs, 500 chars
        // 2. (100, codex, local)  -> 1 sess, 3 msgs, 300 chars
        // 3. (101, claude, local) -> 1 sess, 2 msgs, 200 chars
        //
        // Expanded 1 (day 100):
        // - (100, claude, local): 1 sess, 5 msgs, 500 chars
        // - (100, all, local):    1 (from claude) + 1 (from codex) = 2 sess, 8 msgs, 800 chars
        // - (100, claude, all):   1 sess, 5 msgs, 500 chars
        // - (100, codex, local):  1 sess, 3 msgs, 300 chars
        // - (100, codex, all):    1 sess, 3 msgs, 300 chars
        // - (100, all, all):      2 sess, 8 msgs, 800 chars
        //
        // Expanded 3 (day 101):
        // - (101, claude, local): 1 sess, 2 msgs, 200 chars
        // - (101, all, local):    1 sess, 2 msgs, 200 chars
        // - (101, claude, all):   1 sess, 2 msgs, 200 chars
        // - (101, all, all):      1 sess, 2 msgs, 200 chars
        //
        // Total unique keys in expanded map:
        // Day 100: (claude, local), (codex, local), (all, local), (claude, all), (codex, all), (all, all) = 6
        // Day 101: (claude, local), (all, local), (claude, all), (all, all) = 4
        // Total = 10 entries

        assert_eq!(entries.len(), 10);

        // Verify totals for day 100, all/all
        let day100_all = entries
            .iter()
            .find(|(d, a, s, _)| *d == 100 && a == "all" && s == "all")
            .unwrap();
        assert_eq!(day100_all.3.session_count_delta, 2);
        assert_eq!(day100_all.3.message_count_delta, 8);
        assert_eq!(day100_all.3.total_chars_delta, 800);
    }

    // =========================================================================
    // LazyDb tests (bd-1ueu)
    // =========================================================================

    #[test]
    fn lazy_db_not_open_before_get() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("lazy_test.db");

        // Create a real DB so the path exists
        let _storage = SqliteStorage::open(&db_path).unwrap();

        let lazy = LazyDb::new(db_path);
        assert!(!lazy.is_open(), "LazyDb must not open on construction");
    }

    #[test]
    fn lazy_db_opens_on_first_get() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("lazy_test.db");

        // Create a real DB so the path exists
        let _storage = SqliteStorage::open(&db_path).unwrap();
        drop(_storage);

        let lazy = LazyDb::new(db_path);
        assert!(!lazy.is_open());

        let conn = lazy.get("test").expect("should open successfully");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        drop(conn);

        assert!(lazy.is_open(), "LazyDb must be open after get()");
    }

    #[test]
    fn lazy_db_reuses_connection() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("lazy_test.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        drop(_storage);

        let lazy = LazyDb::new(db_path);

        // First access opens
        {
            let conn = lazy.get("first").unwrap();
            conn.execute_batch("CREATE TABLE IF NOT EXISTS test_tbl (id INTEGER)")
                .unwrap();
        }

        // Second access reuses (table still exists)
        {
            let conn = lazy.get("second").unwrap();
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM test_tbl", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0);
        }
    }

    #[test]
    fn lazy_db_not_found_error() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("nonexistent.db");

        let lazy = LazyDb::new(db_path);
        let result = lazy.get("test");
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), LazyDbError::NotFound(_)),
            "should return NotFound for missing DB"
        );
    }

    #[test]
    fn lazy_db_path_accessor() {
        let path = PathBuf::from("/tmp/test_lazy.db");
        let lazy = LazyDb::new(path.clone());
        assert_eq!(lazy.path(), path.as_path());
    }
}
