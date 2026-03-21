//! `SQLite` backend: schema, pragmas, and migrations.

use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole, Snippet};
use crate::sources::provenance::{LOCAL_SOURCE_ID, Source, SourceKind};
use anyhow::{Context, Result, anyhow};
use frankensqlite::{
    Connection as FrankenConnection, Row as FrankenRow,
    compat::{
        ConnectionExt as FrankenConnectionExt, OpenFlags as FrankenOpenFlags,
        OptionalExtension as FrankenOptionalExtension, ParamValue, RowExt as FrankenRowExt,
        Transaction as FrankenTransaction, TransactionExt as FrankenTransactionExt,
        open_with_flags as open_franken_with_flags, param_slice_to_values,
    },
    migrate::MigrationRunner,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use std::fs;

/// Frankensqlite parameter list builder (avoids name conflict with rusqlite `params!`).
macro_rules! fparams {
    () => {
        &[] as &[ParamValue]
    };
    ($($val:expr),+ $(,)?) => {
        &[$(ParamValue::from($val)),+] as &[ParamValue]
    };
}
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
    #[error("Failed to open FrankenSQLite database at {path}: {source}")]
    FrankenOpenFailed {
        path: PathBuf,
        source: frankensqlite::FrankenError,
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
// LazyFrankenDb — lazy wrapper around FrankenConnection
// -------------------------------------------------------------------------

/// Wrapper around `FrankenConnection` that implements `Send`.
///
/// `FrankenConnection` is `!Send` because it uses `Rc` internally.
/// However, the `Rc` values are entirely self-contained within the Connection
/// and are not shared externally.  When wrapped in a `Mutex`,
/// exclusive access is guaranteed, making cross-thread transfer safe.
pub struct SendFrankenConnection(FrankenConnection);

// Safety: Rc fields inside FrankenConnection are not cloned or shared externally.
// The Mutex<Option<SendFrankenConnection>> ensures exclusive access.
unsafe impl Send for SendFrankenConnection {}

impl SendFrankenConnection {
    pub(crate) fn new(conn: FrankenConnection) -> Self {
        Self(conn)
    }
}

impl std::ops::Deref for SendFrankenConnection {
    type Target = FrankenConnection;
    fn deref(&self) -> &FrankenConnection {
        &self.0
    }
}

/// Lazy-opening wrapper for `FrankenConnection` (frankensqlite).
///
/// Constructing a `LazyFrankenDb` is cheap (no I/O).  The underlying
/// `FrankenConnection` is opened on the first call to [`get`].
/// Subsequent calls return the cached connection.
pub struct LazyFrankenDb {
    path: PathBuf,
    conn: parking_lot::Mutex<Option<SendFrankenConnection>>,
}

/// RAII guard that dereferences to the inner `FrankenConnection`.
pub struct LazyFrankenDbGuard<'a>(parking_lot::MutexGuard<'a, Option<SendFrankenConnection>>);

impl std::fmt::Debug for LazyFrankenDbGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("LazyFrankenDbGuard")
            .field(&self.0.is_some())
            .finish()
    }
}

impl std::ops::Deref for LazyFrankenDbGuard<'_> {
    type Target = FrankenConnection;
    fn deref(&self) -> &FrankenConnection {
        self.0
            .as_ref()
            .expect("LazyFrankenDb connection must be initialized before access")
    }
}

impl LazyFrankenDb {
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
    pub fn get(&self, reason: &str) -> std::result::Result<LazyFrankenDbGuard<'_>, LazyDbError> {
        let mut guard = self.conn.lock();
        if guard.is_none() {
            if !self.path.exists() {
                return Err(LazyDbError::NotFound(self.path.clone()));
            }
            let start = Instant::now();
            let conn =
                FrankenConnection::open(self.path.to_string_lossy().into_owned()).map_err(|e| {
                    LazyDbError::FrankenOpenFailed {
                        path: self.path.clone(),
                        source: e,
                    }
                })?;
            let elapsed_ms = start.elapsed().as_millis();
            info!(
                path = %self.path.display(),
                elapsed_ms = elapsed_ms,
                reason = reason,
                "lazily opened FrankenSQLite database"
            );
            *guard = Some(SendFrankenConnection(conn));
        }
        Ok(LazyFrankenDbGuard(guard))
    }

    /// Get the connection with a timeout, opening the database on first access.
    ///
    /// Like [`get`] but spawns the open in a background thread and waits up to
    /// `timeout` for it to complete. Returns `LazyDbError::FrankenOpenFailed`
    /// with a descriptive message if the timeout elapses. Fix for #128.
    pub fn get_with_timeout(
        &self,
        reason: &str,
        timeout: Duration,
    ) -> std::result::Result<LazyFrankenDbGuard<'_>, LazyDbError> {
        let mut guard = self.conn.lock();
        if guard.is_none() {
            if !self.path.exists() {
                return Err(LazyDbError::NotFound(self.path.clone()));
            }
            let start = Instant::now();
            let path_owned = self.path.to_string_lossy().into_owned();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ =
                    tx.send(FrankenConnection::open(path_owned).map(SendFrankenConnection::new));
            });
            let conn = rx
                .recv_timeout(timeout)
                .map_err(|_| LazyDbError::FrankenOpenFailed {
                    path: self.path.clone(),
                    source: frankensqlite::FrankenError::Internal(format!(
                        "database open timed out after {}s (possible corruption or lock contention)",
                        timeout.as_secs()
                    )),
                })?
                .map_err(|e| LazyDbError::FrankenOpenFailed {
                    path: self.path.clone(),
                    source: e,
                })?;
            let elapsed_ms = start.elapsed().as_millis();
            info!(
                path = %self.path.display(),
                elapsed_ms = elapsed_ms,
                reason = reason,
                "lazily opened FrankenSQLite database (with timeout)"
            );
            *guard = Some(conn);
        }
        Ok(LazyFrankenDbGuard(guard))
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
// FrankenSQLite Connection Manager (bead 3rlf8)
// -------------------------------------------------------------------------
// Multi-connection management: reader pool + concurrent writer connections.
// Replaces the LazyFrankenDb single-connection bottleneck for high-throughput
// scenarios (indexer parallel writes, concurrent TUI reads + indexer writes).

/// Configuration for the [`FrankenConnectionManager`].
#[derive(Debug, Clone)]
pub struct ConnectionManagerConfig {
    /// Number of pre-opened reader connections (default: 4).
    pub reader_count: usize,
    /// Maximum concurrent writer connections (default: available parallelism).
    pub max_writers: usize,
}

impl Default for ConnectionManagerConfig {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self {
            reader_count: 4,
            max_writers: cpus,
        }
    }
}

/// Multi-connection manager for frankensqlite.
///
/// Provides:
/// - A pool of pre-opened reader connections (round-robin, Mutex-protected)
/// - Controlled creation of writer connections with token-based limits
/// - RAII guards that auto-rollback uncommitted transactions on drop
///
/// Thread-safe: reader connections are wrapped in Mutex (FrankenConnection is !Sync).
/// Writer connections are created per-request (each thread gets its own).
pub struct FrankenConnectionManager {
    db_path: PathBuf,
    readers: Vec<parking_lot::Mutex<SendFrankenConnection>>,
    reader_idx: std::sync::atomic::AtomicUsize,
    /// Token-based writer limit: channel pre-filled with `max_writers` tokens.
    /// `recv()` = acquire slot, `send()` = release slot.
    writer_tokens: (
        crossbeam_channel::Sender<()>,
        crossbeam_channel::Receiver<()>,
    ),
    config: ConnectionManagerConfig,
}

// Safety: FrankenConnectionManager is Send+Sync because:
// - readers wrapped in Mutex<SendFrankenConnection> (exclusive access)
// - writer_tokens uses crossbeam (Send+Sync)
// - db_path is PathBuf (Send+Sync)
unsafe impl Send for FrankenConnectionManager {}
unsafe impl Sync for FrankenConnectionManager {}

impl FrankenConnectionManager {
    /// Create a new connection manager.
    ///
    /// Opens `config.reader_count` reader connections immediately.
    /// Writer connections are created on demand (up to `config.max_writers`).
    pub fn new(db_path: impl Into<PathBuf>, config: ConnectionManagerConfig) -> Result<Self> {
        let db_path = db_path.into();
        let path_str = db_path.to_string_lossy().to_string();

        let reader_count = config.reader_count.max(1);
        let mut readers = Vec::with_capacity(reader_count);
        for _ in 0..reader_count {
            let conn = FrankenConnection::open(&path_str)
                .with_context(|| format!("opening reader connection at {}", db_path.display()))?;
            // Apply read-tuned config (no migration, no write PRAGMAs)
            let _ = conn.execute("PRAGMA cache_size = -16384;"); // 16MB reader cache
            readers.push(parking_lot::Mutex::new(SendFrankenConnection(conn)));
        }

        // Pre-fill bounded channel with tokens (acts as counting semaphore)
        let (tx, rx) = crossbeam_channel::bounded(config.max_writers);
        for _ in 0..config.max_writers {
            tx.send(()).expect("pre-filling writer tokens");
        }

        Ok(Self {
            db_path,
            readers,
            reader_idx: std::sync::atomic::AtomicUsize::new(0),
            writer_tokens: (tx, rx),
            config,
        })
    }

    /// Get a reader connection (round-robin from the pool).
    ///
    /// Returns a mutex guard wrapping the connection. The guard prevents
    /// concurrent access to the same connection (FrankenConnection is !Sync).
    pub fn reader(&self) -> parking_lot::MutexGuard<'_, SendFrankenConnection> {
        let idx = self
            .reader_idx
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.readers[idx % self.readers.len()].lock()
    }

    /// Acquire a writer connection.
    ///
    /// Opens a new frankensqlite connection with full config (no migration).
    /// Blocks if `max_writers` connections are already in use.
    /// The returned [`WriterGuard`] auto-rolls back on drop.
    pub fn writer(&self) -> Result<WriterGuard<'_>> {
        self.writer_tokens
            .1
            .recv()
            .map_err(|_| anyhow!("writer token channel closed"))?;
        let path_str = self.db_path.to_string_lossy().to_string();
        let conn = FrankenConnection::open(&path_str)
            .with_context(|| format!("opening writer connection at {}", self.db_path.display()))?;
        let storage = FrankenStorage { conn };
        storage.apply_config()?;
        Ok(WriterGuard {
            storage,
            mgr: self,
            committed: false,
        })
    }

    /// Acquire a concurrent writer connection (BEGIN CONCURRENT via MVCC).
    ///
    /// Similar to [`writer`] but tuned for the parallel indexer write pool.
    /// Uses reduced cache size and is designed for short-lived batch inserts.
    pub fn concurrent_writer(&self) -> Result<WriterGuard<'_>> {
        self.writer_tokens
            .1
            .recv()
            .map_err(|_| anyhow!("writer token channel closed"))?;
        let path_str = self.db_path.to_string_lossy().to_string();
        let conn = FrankenConnection::open(&path_str)
            .with_context(|| format!("opening concurrent writer at {}", self.db_path.display()))?;
        let storage = FrankenStorage { conn };
        storage.apply_config()?;
        // Reduced cache for concurrent writers (they're short-lived)
        let _ = storage.raw().execute("PRAGMA cache_size = -4096;");
        Ok(WriterGuard {
            storage,
            mgr: self,
            committed: false,
        })
    }

    /// Database path managed by this pool.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Number of reader connections in the pool.
    pub fn reader_count(&self) -> usize {
        self.readers.len()
    }

    /// Maximum concurrent writers allowed.
    pub fn max_writers(&self) -> usize {
        self.config.max_writers
    }
}

/// RAII guard for a writer connection.
///
/// Provides access to a [`FrankenStorage`] for write operations.
/// Releases the writer semaphore slot when dropped.
pub struct WriterGuard<'a> {
    storage: FrankenStorage,
    mgr: &'a FrankenConnectionManager,
    committed: bool,
}

impl<'a> WriterGuard<'a> {
    /// Access the underlying storage for read/write operations.
    pub fn storage(&self) -> &FrankenStorage {
        &self.storage
    }

    /// Mark this writer as successfully committed.
    ///
    /// Call after your transaction's `commit()` succeeds. Prevents the drop
    /// guard from attempting a rollback.
    pub fn mark_committed(&mut self) {
        self.committed = true;
    }
}

impl Drop for WriterGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            // Best-effort rollback — connection may already be in autocommit
            let _ = self.storage.raw().execute("ROLLBACK;");
        }
        // Release writer token
        let _ = self.mgr.writer_tokens.0.send(());
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

/// Read metadata from a frankensqlite Row, preferring binary (msgpack) over JSON.
fn franken_read_metadata_compat(
    row: &FrankenRow,
    json_idx: usize,
    bin_idx: usize,
) -> serde_json::Value {
    // Try binary column first (new format)
    if let Ok(Some(bytes)) = row.get_typed::<Option<Vec<u8>>>(bin_idx)
        && !bytes.is_empty()
    {
        return deserialize_msgpack_to_json(&bytes);
    }

    // Fall back to JSON column (old format or migration in progress)
    if let Ok(Some(json_str)) = row.get_typed::<Option<String>>(json_idx) {
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

/// SQL to register the FTS5 virtual table on a frankensqlite connection.
///
/// FrankenSQLite skips virtual-table entries (rootpage=0) when loading
/// `sqlite_master` from a stock-SQLite database.  Executing this CREATE
/// triggers the legacy FTS5 fallback path and materialises the table so
/// subsequent FTS queries work.
pub const FTS5_REGISTER_SQL: &str = "\
    CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages USING fts5(\
        content, title, agent, workspace, source_path, \
        created_at UNINDEXED, \
        content='', tokenize='porter'\
    )";

/// SQL to clear all rows from the contentless `fts_messages` table.
///
/// Contentless FTS5 tables reject ordinary `DELETE FROM ...` statements.
pub const FTS5_DELETE_ALL_SQL: &str =
    "INSERT INTO fts_messages(fts_messages) VALUES('delete-all');";

/// Register the `fts_messages` FTS5 virtual table on a frankensqlite
/// [`Connection`](FrankenConnection).
///
/// This is idempotent (`IF NOT EXISTS`) and safe to call on every open.
/// Returns `Ok(())` on success or if the table already exists.  On failure
/// the error is returned so callers can decide whether to log or propagate.
pub fn register_fts5_on_connection(
    conn: &FrankenConnection,
) -> std::result::Result<(), frankensqlite::FrankenError> {
    conn.execute(FTS5_REGISTER_SQL).map(|_| ())
}

/// Rebuild the `fts_messages` table from the canonical messages/conversations
/// tables using a frankensqlite connection.
pub(crate) fn rebuild_fts_on_connection(conn: &FrankenConnection) -> Result<()> {
    let total_count: i64 = conn.query_row_map(
        "SELECT COUNT(*) FROM messages m JOIN conversations c ON m.conversation_id = c.id JOIN agents a ON c.agent_id = a.id LEFT JOIN workspaces w ON c.workspace_id = w.id",
        &[],
        |r: &FrankenRow| r.get_typed(0),
    )?;
    let batch_size: i64 = 10_000;
    let mut offset: i64 = 0;

    conn.execute_batch("DROP TABLE IF EXISTS fts_messages;")?;
    register_fts5_on_connection(conn)?;

    conn.execute_batch("BEGIN;")?;
    let result = (|| -> Result<()> {
        while offset < total_count {
            info!(
                "Rebuilding FTS: {}/{} rows...",
                offset.min(total_count),
                total_count
            );
            conn.execute_batch(&format!(
                "INSERT INTO fts_messages(rowid, content, title, agent, workspace, source_path, created_at)
                 SELECT m.id, m.content, c.title, a.slug, w.path, c.source_path, m.created_at
                 FROM messages m
                 JOIN conversations c ON m.conversation_id = c.id
                 JOIN agents a ON c.agent_id = a.id
                 LEFT JOIN workspaces w ON c.workspace_id = w.id
                 ORDER BY m.rowid
                 LIMIT {} OFFSET {};",
                batch_size, offset
            ))?;
            offset += batch_size;
        }
        conn.execute_batch("COMMIT;")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
    } else {
        info!(
            "Rebuilding FTS: {}/{} rows complete.",
            total_count, total_count
        );
    }
    result
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
pub const CURRENT_SCHEMA_VERSION: i64 = 14;

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

const SCHEMA_VERSION: i64 = CURRENT_SCHEMA_VERSION;

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

const MIGRATION_V11: &str = r"
-- Analytics fact table: one row per message with pre-extracted metrics.
-- Designed for fast analytical queries without touching message content.
-- Buckets use message created_at (not conversation started_at).
CREATE TABLE IF NOT EXISTS message_metrics (
    message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    created_at_ms INTEGER NOT NULL,
    hour_id INTEGER NOT NULL,          -- hours since 2020-01-01 00:00 UTC
    day_id INTEGER NOT NULL,           -- days since 2020-01-01 (matches daily_stats)

    -- Dimensions
    agent_slug TEXT NOT NULL,
    workspace_id INTEGER NOT NULL DEFAULT 0,  -- 0 = unknown
    source_id TEXT NOT NULL DEFAULT 'local',
    role TEXT NOT NULL,                -- user/assistant/tool/system/other

    -- Content-size metrics (always available, every message)
    content_chars INTEGER NOT NULL,
    content_tokens_est INTEGER NOT NULL,  -- chars/4 deterministic estimate

    -- API usage metrics (nullable — only agents with provider data)
    api_input_tokens INTEGER,
    api_output_tokens INTEGER,
    api_cache_read_tokens INTEGER,
    api_cache_creation_tokens INTEGER,
    api_thinking_tokens INTEGER,
    api_service_tier TEXT,
    api_data_source TEXT NOT NULL DEFAULT 'estimated',  -- 'api' or 'estimated'

    -- Tool / plan flags
    tool_call_count INTEGER NOT NULL DEFAULT 0,
    has_tool_calls INTEGER NOT NULL DEFAULT 0,  -- 0/1
    has_plan INTEGER NOT NULL DEFAULT 0         -- 0/1 (cheap heuristic)
);

CREATE INDEX IF NOT EXISTS idx_mm_hour ON message_metrics(hour_id);
CREATE INDEX IF NOT EXISTS idx_mm_day ON message_metrics(day_id);
CREATE INDEX IF NOT EXISTS idx_mm_agent_hour ON message_metrics(agent_slug, hour_id);
CREATE INDEX IF NOT EXISTS idx_mm_agent_day ON message_metrics(agent_slug, day_id);
CREATE INDEX IF NOT EXISTS idx_mm_workspace_hour ON message_metrics(workspace_id, hour_id);
CREATE INDEX IF NOT EXISTS idx_mm_source_hour ON message_metrics(source_id, hour_id);

-- Hourly rollup table: fast time-series queries at hour granularity.
-- Keyed by (hour_id, agent_slug, workspace_id, source_id).
CREATE TABLE IF NOT EXISTS usage_hourly (
    hour_id INTEGER NOT NULL,
    agent_slug TEXT NOT NULL,
    workspace_id INTEGER NOT NULL DEFAULT 0,
    source_id TEXT NOT NULL DEFAULT 'local',

    -- Counts
    message_count INTEGER NOT NULL DEFAULT 0,
    user_message_count INTEGER NOT NULL DEFAULT 0,
    assistant_message_count INTEGER NOT NULL DEFAULT 0,
    tool_call_count INTEGER NOT NULL DEFAULT 0,
    plan_message_count INTEGER NOT NULL DEFAULT 0,
    api_coverage_message_count INTEGER NOT NULL DEFAULT 0,  -- messages with api_data_source='api'

    -- Content-estimated tokens
    content_tokens_est_total INTEGER NOT NULL DEFAULT 0,
    content_tokens_est_user INTEGER NOT NULL DEFAULT 0,
    content_tokens_est_assistant INTEGER NOT NULL DEFAULT 0,

    -- API tokens
    api_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_input_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_output_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_cache_read_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_cache_creation_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_thinking_tokens_total INTEGER NOT NULL DEFAULT 0,

    last_updated INTEGER NOT NULL DEFAULT 0,

    PRIMARY KEY (hour_id, agent_slug, workspace_id, source_id)
);

CREATE INDEX IF NOT EXISTS idx_uh_agent ON usage_hourly(agent_slug, hour_id);
CREATE INDEX IF NOT EXISTS idx_uh_workspace ON usage_hourly(workspace_id, hour_id);
CREATE INDEX IF NOT EXISTS idx_uh_source ON usage_hourly(source_id, hour_id);

-- Daily rollup table: same schema as hourly, keyed by day_id.
-- Avoids summing 24 hourly rows for daily queries.
CREATE TABLE IF NOT EXISTS usage_daily (
    day_id INTEGER NOT NULL,
    agent_slug TEXT NOT NULL,
    workspace_id INTEGER NOT NULL DEFAULT 0,
    source_id TEXT NOT NULL DEFAULT 'local',

    -- Counts
    message_count INTEGER NOT NULL DEFAULT 0,
    user_message_count INTEGER NOT NULL DEFAULT 0,
    assistant_message_count INTEGER NOT NULL DEFAULT 0,
    tool_call_count INTEGER NOT NULL DEFAULT 0,
    plan_message_count INTEGER NOT NULL DEFAULT 0,
    api_coverage_message_count INTEGER NOT NULL DEFAULT 0,

    -- Content-estimated tokens
    content_tokens_est_total INTEGER NOT NULL DEFAULT 0,
    content_tokens_est_user INTEGER NOT NULL DEFAULT 0,
    content_tokens_est_assistant INTEGER NOT NULL DEFAULT 0,

    -- API tokens
    api_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_input_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_output_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_cache_read_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_cache_creation_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_thinking_tokens_total INTEGER NOT NULL DEFAULT 0,

    last_updated INTEGER NOT NULL DEFAULT 0,

    PRIMARY KEY (day_id, agent_slug, workspace_id, source_id)
);

CREATE INDEX IF NOT EXISTS idx_ud_agent ON usage_daily(agent_slug, day_id);
CREATE INDEX IF NOT EXISTS idx_ud_workspace ON usage_daily(workspace_id, day_id);
CREATE INDEX IF NOT EXISTS idx_ud_source ON usage_daily(source_id, day_id);
";

const MIGRATION_V12: &str = r"
-- Add model dimensions to message_metrics for model-aware Track A analytics.
ALTER TABLE message_metrics ADD COLUMN model_name TEXT;
ALTER TABLE message_metrics ADD COLUMN model_family TEXT NOT NULL DEFAULT 'unknown';
ALTER TABLE message_metrics ADD COLUMN model_tier TEXT NOT NULL DEFAULT 'unknown';
ALTER TABLE message_metrics ADD COLUMN provider TEXT NOT NULL DEFAULT 'unknown';

CREATE INDEX IF NOT EXISTS idx_mm_model_family_day ON message_metrics(model_family, day_id);
CREATE INDEX IF NOT EXISTS idx_mm_provider_day ON message_metrics(provider, day_id);

-- Daily model rollups for fast model-oriented analytics queries.
CREATE TABLE IF NOT EXISTS usage_models_daily (
    day_id INTEGER NOT NULL,
    agent_slug TEXT NOT NULL,
    workspace_id INTEGER NOT NULL DEFAULT 0,
    source_id TEXT NOT NULL DEFAULT 'local',
    model_family TEXT NOT NULL DEFAULT 'unknown',
    model_tier TEXT NOT NULL DEFAULT 'unknown',

    -- Counts
    message_count INTEGER NOT NULL DEFAULT 0,
    user_message_count INTEGER NOT NULL DEFAULT 0,
    assistant_message_count INTEGER NOT NULL DEFAULT 0,
    tool_call_count INTEGER NOT NULL DEFAULT 0,
    plan_message_count INTEGER NOT NULL DEFAULT 0,
    api_coverage_message_count INTEGER NOT NULL DEFAULT 0,

    -- Content-estimated tokens
    content_tokens_est_total INTEGER NOT NULL DEFAULT 0,
    content_tokens_est_user INTEGER NOT NULL DEFAULT 0,
    content_tokens_est_assistant INTEGER NOT NULL DEFAULT 0,

    -- API tokens
    api_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_input_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_output_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_cache_read_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_cache_creation_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_thinking_tokens_total INTEGER NOT NULL DEFAULT 0,

    last_updated INTEGER NOT NULL DEFAULT 0,

    PRIMARY KEY (
        day_id,
        agent_slug,
        workspace_id,
        source_id,
        model_family,
        model_tier
    )
);

CREATE INDEX IF NOT EXISTS idx_umd_model_day ON usage_models_daily(model_family, day_id);
CREATE INDEX IF NOT EXISTS idx_umd_agent_day ON usage_models_daily(agent_slug, day_id);
CREATE INDEX IF NOT EXISTS idx_umd_workspace_day ON usage_models_daily(workspace_id, day_id);
CREATE INDEX IF NOT EXISTS idx_umd_source_day ON usage_models_daily(source_id, day_id);
";

const MIGRATION_V13: &str = r"
-- Add plan-attributed token rollups to usage tables.
ALTER TABLE usage_hourly ADD COLUMN plan_content_tokens_est_total INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_hourly ADD COLUMN plan_api_tokens_total INTEGER NOT NULL DEFAULT 0;

ALTER TABLE usage_daily ADD COLUMN plan_content_tokens_est_total INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_daily ADD COLUMN plan_api_tokens_total INTEGER NOT NULL DEFAULT 0;
";

const MIGRATION_V14: &str = r"
-- Switch FTS5 from internal-content to contentless mode.
-- Internal-content mode duplicated all indexed columns in shadow tables,
-- causing ~45% DB bloat. Contentless mode (content='') stores only the
-- inverted index, and queries JOIN back to source tables for column data.
DROP TABLE IF EXISTS fts_messages;
CREATE VIRTUAL TABLE fts_messages USING fts5(
    content,
    title,
    agent,
    workspace,
    source_path,
    created_at UNINDEXED,
    content='',
    tokenize='porter'
);
INSERT INTO fts_messages(rowid, content, title, agent, workspace, source_path, created_at)
SELECT
    m.id,
    m.content,
    c.title,
    a.slug,
    w.path,
    c.source_path,
    m.created_at
FROM messages m
JOIN conversations c ON m.conversation_id = c.id
JOIN agents a ON c.agent_id = a.id
LEFT JOIN workspaces w ON c.workspace_id = w.id;
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

/// Migration foundation for the future frankensqlite-backed storage backend.
///
/// This intentionally coexists with `SqliteStorage` during the staged migration.
/// Full CRUD parity is tracked in follow-on beads.
pub struct FrankenStorage {
    conn: FrankenConnection,
}

impl FrankenStorage {
    /// Open a frankensqlite connection, run migrations, and apply config.
    ///
    /// Migrations run before PRAGMAs to avoid page lock contention in
    /// frankensqlite's WAL mode on file-based databases.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating db directory {}", parent.display()))?;
        }

        let path_str = path.to_string_lossy().to_string();
        let conn = FrankenConnection::open(&path_str)
            .with_context(|| format!("opening frankensqlite db at {}", path.display()))?;
        let storage = Self { conn };
        storage.run_migrations()?;
        storage.apply_config()?;
        Ok(storage)
    }

    /// Open a writer connection that skips migration (assumes DB already migrated).
    ///
    /// Used by the BEGIN CONCURRENT parallel writer pool: each writer needs its
    /// own connection with config applied, but migrations have already been run
    /// by the primary connection.
    pub fn open_writer(path: &Path) -> Result<Self> {
        let path_str = path.to_string_lossy().to_string();
        let conn = FrankenConnection::open(&path_str)
            .with_context(|| format!("opening frankensqlite writer at {}", path.display()))?;
        // FrankenSQLite skips virtual-table entries (rootpage=0); re-register
        // FTS5 so INSERT into fts_messages works on writer connections.
        let _ = conn.execute(FTS5_REGISTER_SQL);
        let storage = Self { conn };
        storage.apply_config()?;
        Ok(storage)
    }

    /// Open in read-only mode using frankensqlite compat flags.
    ///
    /// Note: current frankensqlite compat `open_with_flags` is a façade and may
    /// not enforce strict read-only behavior yet; this constructor still provides
    /// the migration-compatible call site.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let path_str = path.to_string_lossy().to_string();
        let conn = open_franken_with_flags(&path_str, FrankenOpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening frankensqlite db readonly at {}", path.display()))?;
        let storage = Self { conn };
        storage.apply_config()?;
        Ok(storage)
    }

    /// Access the raw frankensqlite connection.
    pub fn raw(&self) -> &FrankenConnection {
        &self.conn
    }

    /// Apply connection PRAGMAs for parity with SqliteStorage's `apply_pragmas()`.
    ///
    /// Frankensqlite supports all PRAGMAs cass uses (journal_mode, synchronous,
    /// cache_size, foreign_keys, busy_timeout). Its default journal_mode is already
    /// WAL and default synchronous is NORMAL, matching cass's requirements.
    ///
    /// Additional frankensqlite-specific observability PRAGMAs are enabled when
    /// available.
    pub fn apply_config(&self) -> Result<()> {
        // journal_mode: frankensqlite defaults to WAL, same as cass.
        // synchronous: frankensqlite defaults to NORMAL, same as cass.
        // Both are set explicitly for clarity and to match rusqlite behavior.
        self.conn
            .execute("PRAGMA journal_mode = WAL;")
            .with_context(|| "setting journal_mode")?;
        self.conn
            .execute("PRAGMA synchronous = NORMAL;")
            .with_context(|| "setting synchronous")?;

        // cache_size: 64MB (negative value = KiB).
        self.conn
            .execute("PRAGMA cache_size = -65536;")
            .with_context(|| "setting cache_size")?;

        // foreign_keys: enable constraint enforcement.
        self.conn
            .execute("PRAGMA foreign_keys = ON;")
            .with_context(|| "setting foreign_keys")?;

        // busy_timeout: 5 seconds (in milliseconds).
        self.conn
            .execute("PRAGMA busy_timeout = 5000;")
            .with_context(|| "setting busy_timeout")?;

        // temp_store = MEMORY and mmap_size are C SQLite performance knobs.
        // In frankensqlite's architecture (in-memory MVCC engine with pager
        // backend), temp_store is always memory-resident and mmap_size does not
        // apply. Skipped intentionally — these are no-ops or errors.

        // wal_autocheckpoint: frankensqlite manages WAL internally, but the
        // PRAGMA is accepted for compatibility.
        let _ = self.conn.execute("PRAGMA wal_autocheckpoint = 1000;");
        // Explicitly enable concurrent writer mode for BEGIN/transaction paths.
        // Try both namespace variants for compatibility across fsqlite builds.
        let _ = self.conn.execute("PRAGMA fsqlite.concurrent_mode = ON;");
        let _ = self.conn.execute("PRAGMA concurrent_mode = ON;");

        Ok(())
    }

    /// Run all schema migrations, handling transition from meta table versioning.
    ///
    /// The existing `SqliteStorage` tracks schema version in a `meta` table entry.
    /// The new `MigrationRunner` uses a `_schema_migrations` table. This method:
    /// 1. Transitions existing databases from meta table → `_schema_migrations`
    /// 2. Runs pending migrations via `MigrationRunner`
    /// 3. Syncs `meta.schema_version` for backward compatibility
    ///
    /// # Fresh vs existing databases
    ///
    /// Fresh databases use a single combined migration (`MIGRATION_FRESH_SCHEMA`)
    /// that creates the complete V13 schema directly. This avoids the incremental
    /// V5 migration which uses `DROP TABLE` — an operation that triggers a known
    /// frankensqlite autoindex limitation.
    ///
    /// Existing databases (transitioned from SqliteStorage) are typically at V13
    /// already, so no further migrations are needed. For databases at V5-V12,
    /// the additive V6-V13 migrations are applied normally.
    pub fn run_migrations(&self) -> Result<()> {
        transition_from_meta_version(&self.conn)?;

        let runner = build_cass_migrations();
        let result = runner
            .run(&self.conn)
            .with_context(|| "running schema migrations")?;

        if !result.applied.is_empty() {
            info!(
                applied = ?result.applied,
                current = result.current,
                was_fresh = result.was_fresh,
                "frankensqlite schema migrations applied"
            );
        }

        // Keep meta.schema_version in sync for backward compatibility.
        self.sync_meta_schema_version(result.current)?;

        Ok(())
    }

    /// Return the current schema version from `_schema_migrations`.
    pub fn schema_version(&self) -> Result<i64> {
        let rows = self
            .conn
            .query("SELECT MAX(version) FROM _schema_migrations;")
            .with_context(|| "reading schema version from _schema_migrations")?;

        if let Some(row) = rows.first()
            && let Ok(v) = row.get_typed::<Option<i64>>(0)
        {
            return Ok(v.unwrap_or(0));
        }
        Ok(0)
    }

    /// Keep `meta.schema_version` in sync for backward compatibility with `SqliteStorage`.
    fn sync_meta_schema_version(&self, version: i64) -> Result<()> {
        // The meta table is created by V1 migration. If it doesn't exist yet,
        // there's nothing to sync.
        if self.conn.query("SELECT key FROM meta LIMIT 1;").is_err() {
            return Ok(());
        }

        // Only write if the version needs updating to avoid write lock contention
        if let Ok(rows) = self
            .conn
            .query("SELECT value FROM meta WHERE key = 'schema_version';")
            && let Some(row) = rows.first()
            && let Ok(val) = row.get_typed::<String>(0)
            && val == version.to_string()
        {
            return Ok(()); // Already up to date
        }

        self.conn
            .execute_compat(
                "INSERT OR REPLACE INTO meta(key, value) VALUES('schema_version', ?1);",
                &[ParamValue::from(version.to_string())],
            )
            .with_context(|| "syncing meta schema_version")?;

        Ok(())
    }

    /// Resolve the database file path for this connection.
    pub fn database_path(&self) -> Result<PathBuf> {
        let rows = self.conn.query("PRAGMA database_list;")?;
        for row in &rows {
            if let Ok(name) = row.get_typed::<String>(1)
                && name == "main"
            {
                let file_path: String = row.get_typed(2)?;
                return Ok(PathBuf::from(file_path));
            }
        }
        Err(anyhow!("could not resolve database file path"))
    }

    /// Open database with migration, backing up if schema is incompatible.
    ///
    /// Mirrors `SqliteStorage::open_or_rebuild` but uses frankensqlite.
    pub fn open_or_rebuild(path: &Path) -> std::result::Result<Self, MigrationError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        if path.exists() {
            let check_result = franken_check_schema_compatibility(path);
            match check_result {
                Ok(SchemaCheck::Compatible) | Ok(SchemaCheck::NeedsMigration) => {
                    // Continue with normal open
                }
                Ok(SchemaCheck::NeedsRebuild(reason)) => {
                    let backup_path = create_backup(path)?;
                    cleanup_old_backups(path, MAX_BACKUPS)?;
                    remove_database_files(path)?;
                    return Err(MigrationError::RebuildRequired {
                        reason,
                        backup_path,
                    });
                }
                Err(_) => {
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

        let storage = Self::open(path).map_err(|e| MigrationError::Other(e.to_string()))?;
        Ok(storage)
    }
}

/// Check schema compatibility without modifying the database (frankensqlite version).
fn franken_check_schema_compatibility(path: &Path) -> Result<SchemaCheck> {
    let path_str = path.to_string_lossy().to_string();
    let conn = FrankenConnection::open(&path_str)
        .with_context(|| format!("opening db for schema check at {}", path.display()))?;

    let meta_rows =
        conn.query("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='meta'")?;
    let meta_exists: i64 = meta_rows
        .first()
        .and_then(|r| r.get_typed::<i64>(0).ok())
        .unwrap_or(0);

    if meta_exists == 0 {
        let table_rows = conn.query("SELECT COUNT(*) FROM sqlite_master WHERE type='table'")?;
        let table_count: i64 = table_rows
            .first()
            .and_then(|r| r.get_typed::<i64>(0).ok())
            .unwrap_or(0);

        if table_count == 0 {
            return Ok(SchemaCheck::NeedsMigration);
        }
        return Ok(SchemaCheck::NeedsRebuild(
            "Database missing schema version metadata".to_string(),
        ));
    }

    let version_rows = conn.query("SELECT value FROM meta WHERE key = 'schema_version'")?;
    let version: Option<i64> = version_rows
        .first()
        .and_then(|r| r.get_typed::<String>(0).ok())
        .and_then(|s| s.parse().ok());

    match version {
        Some(v) if v == SCHEMA_VERSION => Ok(SchemaCheck::Compatible),
        Some(v) if v < SCHEMA_VERSION => Ok(SchemaCheck::NeedsMigration),
        Some(v) => Ok(SchemaCheck::NeedsRebuild(format!(
            "Schema version {} is newer than supported version {}",
            v, SCHEMA_VERSION
        ))),
        None => Ok(SchemaCheck::NeedsRebuild(
            "Schema version not found or invalid".to_string(),
        )),
    }
}

// -------------------------------------------------------------------------
// Frankensqlite migration helpers
// -------------------------------------------------------------------------

/// Build the `MigrationRunner` for the frankensqlite migration path.
///
/// Uses a single combined migration (version 13) that creates the complete
/// final schema in one step. This avoids the V5 `DROP TABLE conversations`
/// operation which triggers a known frankensqlite limitation: autoindex entries
/// in sqlite_master are not properly cleaned up during DROP TABLE, causing
/// "sqlite_master entry not found" errors.
///
/// For existing databases transitioned from SqliteStorage (typically at V13),
/// the transition function backfills `_schema_migrations` and no further
/// migrations are needed.
fn build_cass_migrations() -> MigrationRunner {
    MigrationRunner::new()
        .add(13, "full_schema_v13", MIGRATION_FRESH_SCHEMA)
        .add(14, "fts_contentless", MIGRATION_V14)
}

/// Combined V13 schema for fresh databases.
///
/// Creates the complete final schema in a single migration, avoiding the
/// incremental V5 `DROP TABLE conversations` which triggers a frankensqlite
/// autoindex limitation. All columns from V1-V13 are included in their
/// respective CREATE TABLE statements.
///
/// Table creation order respects foreign key references:
/// sources → agents/workspaces → conversations → messages → snippets, etc.
const MIGRATION_FRESH_SCHEMA: &str = r"
-- Core tables (V1)
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

-- Sources (V4)
CREATE TABLE IF NOT EXISTS sources (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    host_label TEXT,
    machine_id TEXT,
    platform TEXT,
    config_json TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

INSERT OR IGNORE INTO sources (id, kind, host_label, created_at, updated_at)
VALUES ('local', 'local', NULL, strftime('%s','now')*1000, strftime('%s','now')*1000);

-- Conversations: V1 base + V5 provenance + V7 metadata_bin + V10 token summary
CREATE TABLE IF NOT EXISTS conversations (
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
    metadata_bin BLOB,
    total_input_tokens INTEGER,
    total_output_tokens INTEGER,
    total_cache_read_tokens INTEGER,
    total_cache_creation_tokens INTEGER,
    grand_total_tokens INTEGER,
    estimated_cost_usd REAL,
    primary_model TEXT,
    api_call_count INTEGER,
    tool_call_count INTEGER,
    user_message_count INTEGER,
    assistant_message_count INTEGER
);

-- Named unique index avoids autoindex issues if table is ever recreated
CREATE UNIQUE INDEX IF NOT EXISTS idx_conversations_provenance
    ON conversations(source_id, agent_id, external_id);

-- Messages: V1 base + V7 extra_bin
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    idx INTEGER NOT NULL,
    role TEXT NOT NULL,
    author TEXT,
    created_at INTEGER,
    content TEXT NOT NULL,
    extra_json TEXT,
    extra_bin BLOB,
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

-- Daily stats (V8)
CREATE TABLE IF NOT EXISTS daily_stats (
    day_id INTEGER NOT NULL,
    agent_slug TEXT NOT NULL,
    source_id TEXT NOT NULL DEFAULT 'all',
    session_count INTEGER NOT NULL DEFAULT 0,
    message_count INTEGER NOT NULL DEFAULT 0,
    total_chars INTEGER NOT NULL DEFAULT 0,
    last_updated INTEGER NOT NULL,
    PRIMARY KEY (day_id, agent_slug, source_id)
);

-- Embedding jobs (V9)
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

CREATE UNIQUE INDEX IF NOT EXISTS idx_embedding_jobs_active
ON embedding_jobs(db_path, model_id)
WHERE status IN ('pending', 'running');

-- Token usage ledger (V10)
CREATE TABLE IF NOT EXISTS token_usage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    conversation_id INTEGER NOT NULL,
    agent_id INTEGER NOT NULL,
    workspace_id INTEGER,
    source_id TEXT NOT NULL DEFAULT 'local',
    timestamp_ms INTEGER NOT NULL,
    day_id INTEGER NOT NULL,
    model_name TEXT,
    model_family TEXT,
    model_tier TEXT,
    service_tier TEXT,
    provider TEXT,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_creation_tokens INTEGER,
    thinking_tokens INTEGER,
    total_tokens INTEGER,
    estimated_cost_usd REAL,
    role TEXT NOT NULL,
    content_chars INTEGER NOT NULL,
    has_tool_calls INTEGER NOT NULL DEFAULT 0,
    tool_call_count INTEGER NOT NULL DEFAULT 0,
    data_source TEXT NOT NULL DEFAULT 'api',
    UNIQUE(message_id)
);

-- Token daily stats (V10)
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

-- Model pricing (V10)
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

-- Message metrics: V11 base + V12 model dimensions
CREATE TABLE IF NOT EXISTS message_metrics (
    message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    created_at_ms INTEGER NOT NULL,
    hour_id INTEGER NOT NULL,
    day_id INTEGER NOT NULL,
    agent_slug TEXT NOT NULL,
    workspace_id INTEGER NOT NULL DEFAULT 0,
    source_id TEXT NOT NULL DEFAULT 'local',
    role TEXT NOT NULL,
    content_chars INTEGER NOT NULL,
    content_tokens_est INTEGER NOT NULL,
    api_input_tokens INTEGER,
    api_output_tokens INTEGER,
    api_cache_read_tokens INTEGER,
    api_cache_creation_tokens INTEGER,
    api_thinking_tokens INTEGER,
    api_service_tier TEXT,
    api_data_source TEXT NOT NULL DEFAULT 'estimated',
    tool_call_count INTEGER NOT NULL DEFAULT 0,
    has_tool_calls INTEGER NOT NULL DEFAULT 0,
    has_plan INTEGER NOT NULL DEFAULT 0,
    model_name TEXT,
    model_family TEXT NOT NULL DEFAULT 'unknown',
    model_tier TEXT NOT NULL DEFAULT 'unknown',
    provider TEXT NOT NULL DEFAULT 'unknown'
);

-- Hourly rollups: V11 base + V13 plan columns
CREATE TABLE IF NOT EXISTS usage_hourly (
    hour_id INTEGER NOT NULL,
    agent_slug TEXT NOT NULL,
    workspace_id INTEGER NOT NULL DEFAULT 0,
    source_id TEXT NOT NULL DEFAULT 'local',
    message_count INTEGER NOT NULL DEFAULT 0,
    user_message_count INTEGER NOT NULL DEFAULT 0,
    assistant_message_count INTEGER NOT NULL DEFAULT 0,
    tool_call_count INTEGER NOT NULL DEFAULT 0,
    plan_message_count INTEGER NOT NULL DEFAULT 0,
    api_coverage_message_count INTEGER NOT NULL DEFAULT 0,
    content_tokens_est_total INTEGER NOT NULL DEFAULT 0,
    content_tokens_est_user INTEGER NOT NULL DEFAULT 0,
    content_tokens_est_assistant INTEGER NOT NULL DEFAULT 0,
    api_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_input_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_output_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_cache_read_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_cache_creation_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_thinking_tokens_total INTEGER NOT NULL DEFAULT 0,
    last_updated INTEGER NOT NULL DEFAULT 0,
    plan_content_tokens_est_total INTEGER NOT NULL DEFAULT 0,
    plan_api_tokens_total INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (hour_id, agent_slug, workspace_id, source_id)
);

-- Daily rollups: V11 base + V13 plan columns
CREATE TABLE IF NOT EXISTS usage_daily (
    day_id INTEGER NOT NULL,
    agent_slug TEXT NOT NULL,
    workspace_id INTEGER NOT NULL DEFAULT 0,
    source_id TEXT NOT NULL DEFAULT 'local',
    message_count INTEGER NOT NULL DEFAULT 0,
    user_message_count INTEGER NOT NULL DEFAULT 0,
    assistant_message_count INTEGER NOT NULL DEFAULT 0,
    tool_call_count INTEGER NOT NULL DEFAULT 0,
    plan_message_count INTEGER NOT NULL DEFAULT 0,
    api_coverage_message_count INTEGER NOT NULL DEFAULT 0,
    content_tokens_est_total INTEGER NOT NULL DEFAULT 0,
    content_tokens_est_user INTEGER NOT NULL DEFAULT 0,
    content_tokens_est_assistant INTEGER NOT NULL DEFAULT 0,
    api_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_input_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_output_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_cache_read_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_cache_creation_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_thinking_tokens_total INTEGER NOT NULL DEFAULT 0,
    last_updated INTEGER NOT NULL DEFAULT 0,
    plan_content_tokens_est_total INTEGER NOT NULL DEFAULT 0,
    plan_api_tokens_total INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day_id, agent_slug, workspace_id, source_id)
);

-- Model daily rollups (V12)
CREATE TABLE IF NOT EXISTS usage_models_daily (
    day_id INTEGER NOT NULL,
    agent_slug TEXT NOT NULL,
    workspace_id INTEGER NOT NULL DEFAULT 0,
    source_id TEXT NOT NULL DEFAULT 'local',
    model_family TEXT NOT NULL DEFAULT 'unknown',
    model_tier TEXT NOT NULL DEFAULT 'unknown',
    message_count INTEGER NOT NULL DEFAULT 0,
    user_message_count INTEGER NOT NULL DEFAULT 0,
    assistant_message_count INTEGER NOT NULL DEFAULT 0,
    tool_call_count INTEGER NOT NULL DEFAULT 0,
    plan_message_count INTEGER NOT NULL DEFAULT 0,
    api_coverage_message_count INTEGER NOT NULL DEFAULT 0,
    content_tokens_est_total INTEGER NOT NULL DEFAULT 0,
    content_tokens_est_user INTEGER NOT NULL DEFAULT 0,
    content_tokens_est_assistant INTEGER NOT NULL DEFAULT 0,
    api_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_input_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_output_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_cache_read_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_cache_creation_tokens_total INTEGER NOT NULL DEFAULT 0,
    api_thinking_tokens_total INTEGER NOT NULL DEFAULT 0,
    last_updated INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day_id, agent_slug, workspace_id, source_id, model_family, model_tier)
);

-- All indexes
CREATE INDEX IF NOT EXISTS idx_conversations_agent_started ON conversations(agent_id, started_at DESC);
CREATE INDEX IF NOT EXISTS idx_conversations_source_id ON conversations(source_id);
CREATE INDEX IF NOT EXISTS idx_conversations_source_path ON conversations(source_path);
CREATE INDEX IF NOT EXISTS idx_messages_conv_idx ON messages(conversation_id, idx);
CREATE INDEX IF NOT EXISTS idx_messages_created ON messages(created_at);
CREATE INDEX IF NOT EXISTS idx_daily_stats_agent ON daily_stats(agent_slug, day_id);
CREATE INDEX IF NOT EXISTS idx_daily_stats_source ON daily_stats(source_id, day_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_day ON token_usage(day_id, agent_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_conv ON token_usage(conversation_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_model ON token_usage(model_family, day_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_workspace ON token_usage(workspace_id, day_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_timestamp ON token_usage(timestamp_ms);
CREATE INDEX IF NOT EXISTS idx_token_daily_stats_agent ON token_daily_stats(agent_slug, day_id);
CREATE INDEX IF NOT EXISTS idx_token_daily_stats_model ON token_daily_stats(model_family, day_id);
CREATE INDEX IF NOT EXISTS idx_mm_hour ON message_metrics(hour_id);
CREATE INDEX IF NOT EXISTS idx_mm_day ON message_metrics(day_id);
CREATE INDEX IF NOT EXISTS idx_mm_agent_hour ON message_metrics(agent_slug, hour_id);
CREATE INDEX IF NOT EXISTS idx_mm_agent_day ON message_metrics(agent_slug, day_id);
CREATE INDEX IF NOT EXISTS idx_mm_workspace_hour ON message_metrics(workspace_id, hour_id);
CREATE INDEX IF NOT EXISTS idx_mm_source_hour ON message_metrics(source_id, hour_id);
CREATE INDEX IF NOT EXISTS idx_mm_model_family_day ON message_metrics(model_family, day_id);
CREATE INDEX IF NOT EXISTS idx_mm_provider_day ON message_metrics(provider, day_id);
CREATE INDEX IF NOT EXISTS idx_uh_agent ON usage_hourly(agent_slug, hour_id);
CREATE INDEX IF NOT EXISTS idx_uh_workspace ON usage_hourly(workspace_id, hour_id);
CREATE INDEX IF NOT EXISTS idx_uh_source ON usage_hourly(source_id, hour_id);
CREATE INDEX IF NOT EXISTS idx_ud_agent ON usage_daily(agent_slug, day_id);
CREATE INDEX IF NOT EXISTS idx_ud_workspace ON usage_daily(workspace_id, day_id);
CREATE INDEX IF NOT EXISTS idx_ud_source ON usage_daily(source_id, day_id);
CREATE INDEX IF NOT EXISTS idx_umd_model_day ON usage_models_daily(model_family, day_id);
CREATE INDEX IF NOT EXISTS idx_umd_agent_day ON usage_models_daily(agent_slug, day_id);
CREATE INDEX IF NOT EXISTS idx_umd_workspace_day ON usage_models_daily(workspace_id, day_id);
CREATE INDEX IF NOT EXISTS idx_umd_source_day ON usage_models_daily(source_id, day_id);
";

/// Migration name lookup for backfilling `_schema_migrations` during transition.
const MIGRATION_NAMES: [(i64, &str); 14] = [
    (1, "core_tables"),
    (2, "fts_messages"),
    (3, "fts_messages_rebuild"),
    (4, "sources"),
    (5, "provenance_columns"),
    (6, "source_path_index"),
    (7, "msgpack_columns"),
    (8, "daily_stats"),
    (9, "embedding_jobs"),
    (10, "token_analytics"),
    (11, "message_metrics"),
    (12, "model_dimensions"),
    (13, "plan_token_rollups"),
    (14, "fts_contentless"),
];

/// Transitions an existing database from `meta` table schema versioning to the
/// `_schema_migrations` table used by `MigrationRunner`.
///
/// The existing `SqliteStorage` tracks schema version as a string value in
/// `meta WHERE key = 'schema_version'`. The bead spec references
/// `PRAGMA user_version`, but the actual cass code uses the `meta` table.
/// This function handles the real code path.
///
/// Behavior:
/// - If `_schema_migrations` already exists → skip (already transitioned)
/// - If `meta` table has `schema_version > 0` → create `_schema_migrations`
///   and backfill entries for versions `1..=current_version`
/// - If `meta` table missing or `schema_version = 0` with no tables → fresh DB,
///   let `MigrationRunner` handle it
/// - If `schema_version = 0` but tables exist → corrupted state, log warning
fn transition_from_meta_version(conn: &FrankenConnection) -> Result<()> {
    // Check if _schema_migrations already exists → already transitioned.
    let rows = conn
        .query("SELECT name FROM sqlite_master WHERE type='table' AND name='_schema_migrations';")
        .with_context(|| "checking for _schema_migrations table")?;
    if !rows.is_empty() {
        return Ok(());
    }

    // Check if the meta table exists.
    let rows = conn
        .query("SELECT name FROM sqlite_master WHERE type='table' AND name='meta';")
        .with_context(|| "checking for meta table")?;
    if rows.is_empty() {
        // No meta table → fresh database, let MigrationRunner handle it.
        return Ok(());
    }

    // Read the current schema version from the meta table.
    let rows = conn
        .query("SELECT value FROM meta WHERE key = 'schema_version';")
        .with_context(|| "reading schema_version from meta")?;

    let current_version: i64 = rows
        .first()
        .and_then(|row| row.get_typed::<String>(0).ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    if current_version == 0 {
        // Check if tables actually exist (corrupted state: tables present but version=0).
        let rows = conn
            .query("SELECT name FROM sqlite_master WHERE type='table' AND name='conversations';")
            .with_context(|| "checking for conversations table")?;

        if rows.is_empty() {
            // Truly fresh DB (meta table exists but empty/reset). Let MigrationRunner handle it.
            return Ok(());
        }

        // Tables exist but version=0: corrupted state. Log and skip transition;
        // MigrationRunner will fail on "table already exists" and surface the error.
        info!("meta.schema_version=0 but tables exist; skipping transition (corrupted state)");
        return Ok(());
    }

    // Create _schema_migrations and backfill entries for all applied versions.
    info!(
        current_version,
        "transitioning schema tracking from meta table to _schema_migrations"
    );

    conn.execute(
        "CREATE TABLE IF NOT EXISTS _schema_migrations (\
            version INTEGER PRIMARY KEY, \
            name TEXT NOT NULL, \
            applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))\
        );",
    )
    .with_context(|| "creating _schema_migrations table for transition")?;

    for &(version, name) in &MIGRATION_NAMES {
        if version > current_version {
            break;
        }
        conn.execute_compat(
            "INSERT INTO _schema_migrations (version, name) VALUES (?1, ?2);",
            &[ParamValue::from(version), ParamValue::from(name)],
        )
        .with_context(|| format!("backfilling _schema_migrations version {version}"))?;
    }

    info!(
        current_version,
        "schema version transition complete: backfilled entries for versions 1..={current_version}"
    );

    Ok(())
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

// =========================================================================
// FrankenStorage CRUD operations
// =========================================================================

impl FrankenStorage {
    /// Ensure an agent exists in the database, returning its ID.
    pub fn ensure_agent(&self, agent: &Agent) -> Result<i64> {
        let now = Self::now_millis();
        self.conn.execute_compat(
            "INSERT INTO agents(slug, name, version, kind, created_at, updated_at) VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(slug) DO UPDATE SET name=excluded.name, version=excluded.version, kind=excluded.kind, updated_at=excluded.updated_at",
            fparams![
                agent.slug.as_str(),
                agent.name.as_str(),
                agent.version.as_deref(),
                agent_kind_str(agent.kind.clone()),
                now,
                now
            ],
        )?;

        self.conn
            .query_row_map(
                "SELECT id FROM agents WHERE slug = ?1",
                fparams![agent.slug.as_str()],
                |row| row.get_typed(0),
            )
            .with_context(|| format!("fetching agent id for {}", agent.slug))
    }

    /// Ensure a workspace exists in the database, returning its ID.
    pub fn ensure_workspace(&self, path: &Path, display_name: Option<&str>) -> Result<i64> {
        let path_str = path.to_string_lossy().to_string();
        self.conn.execute_compat(
            "INSERT INTO workspaces(path, display_name) VALUES(?1,?2)
             ON CONFLICT(path) DO UPDATE SET display_name=COALESCE(excluded.display_name, workspaces.display_name)",
            fparams![path_str.as_str(), display_name],
        )?;

        self.conn
            .query_row_map(
                "SELECT id FROM workspaces WHERE path = ?1",
                fparams![path_str.as_str()],
                |row| row.get_typed(0),
            )
            .with_context(|| format!("fetching workspace id for {path_str}"))
    }

    /// Get current time as milliseconds since epoch.
    pub fn now_millis() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }

    /// Convert a millisecond timestamp to a day ID (days since 2020-01-01).
    pub fn day_id_from_millis(timestamp_ms: i64) -> i64 {
        const EPOCH_2020_SECS: i64 = 1_577_836_800;
        let secs = timestamp_ms.div_euclid(1000);
        (secs - EPOCH_2020_SECS).div_euclid(86400)
    }

    /// Convert a millisecond timestamp to an hour ID (hours since 2020-01-01 00:00 UTC).
    pub fn hour_id_from_millis(timestamp_ms: i64) -> i64 {
        const EPOCH_2020_SECS: i64 = 1_577_836_800;
        let secs = timestamp_ms.div_euclid(1000);
        (secs - EPOCH_2020_SECS).div_euclid(3600)
    }

    /// Convert a day ID back to milliseconds (start of day).
    pub fn millis_from_day_id(day_id: i64) -> i64 {
        const EPOCH_2020_SECS: i64 = 1_577_836_800;
        (EPOCH_2020_SECS + day_id * 86400) * 1000
    }

    /// Convert an hour ID back to milliseconds (start of hour).
    pub fn millis_from_hour_id(hour_id: i64) -> i64 {
        const EPOCH_2020_SECS: i64 = 1_577_836_800;
        (EPOCH_2020_SECS + hour_id * 3600) * 1000
    }

    /// Get the timestamp of the last successful scan.
    pub fn get_last_scan_ts(&self) -> Result<Option<i64>> {
        let result: Result<String, _> = self.conn.query_row_map(
            "SELECT value FROM meta WHERE key = 'last_scan_ts'",
            fparams![],
            |row| row.get_typed(0),
        );
        match result.optional() {
            Ok(Some(s)) => Ok(s.parse().ok()),
            Ok(None) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Set the timestamp of the last successful scan (milliseconds since epoch).
    pub fn set_last_scan_ts(&self, ts: i64) -> Result<()> {
        self.conn.execute_compat(
            "INSERT OR REPLACE INTO meta(key, value) VALUES('last_scan_ts', ?1)",
            fparams![ts.to_string()],
        )?;
        Ok(())
    }

    /// Set the timestamp of the last successful index completion (milliseconds since epoch).
    pub fn set_last_indexed_at(&self, ts: i64) -> Result<()> {
        self.conn.execute_compat(
            "INSERT OR REPLACE INTO meta(key, value) VALUES('last_indexed_at', ?1)",
            fparams![ts.to_string()],
        )?;
        Ok(())
    }

    /// List all registered agents.
    pub fn list_agents(&self) -> Result<Vec<Agent>> {
        self.conn
            .query_map_collect(
                "SELECT id, slug, name, version, kind FROM agents ORDER BY slug",
                fparams![],
                |row| {
                    let kind: String = row.get_typed(4)?;
                    Ok(Agent {
                        id: Some(row.get_typed(0)?),
                        slug: row.get_typed(1)?,
                        name: row.get_typed(2)?,
                        version: row.get_typed(3)?,
                        kind: match kind.as_str() {
                            "cli" => AgentKind::Cli,
                            "vscode" => AgentKind::VsCode,
                            _ => AgentKind::Hybrid,
                        },
                    })
                },
            )
            .with_context(|| "listing agents")
    }

    /// List all registered workspaces.
    pub fn list_workspaces(&self) -> Result<Vec<crate::model::types::Workspace>> {
        self.conn
            .query_map_collect(
                "SELECT id, path, display_name FROM workspaces ORDER BY path",
                fparams![],
                |row| {
                    let path_str: String = row.get_typed(1)?;
                    Ok(crate::model::types::Workspace {
                        id: Some(row.get_typed(0)?),
                        path: Path::new(&path_str).to_path_buf(),
                        display_name: row.get_typed(2)?,
                    })
                },
            )
            .with_context(|| "listing workspaces")
    }

    /// List conversations with pagination.
    pub fn list_conversations(&self, limit: i64, offset: i64) -> Result<Vec<Conversation>> {
        self.conn
            .query_map_collect(
                r"SELECT c.id, a.slug, w.path, c.external_id, c.title, c.source_path,
                       c.started_at, c.ended_at, c.approx_tokens, c.metadata_json,
                       c.source_id, c.origin_host, c.metadata_bin
                FROM conversations c
                JOIN agents a ON c.agent_id = a.id
                LEFT JOIN workspaces w ON c.workspace_id = w.id
                ORDER BY c.started_at IS NULL, c.started_at DESC, c.id DESC
                LIMIT ?1 OFFSET ?2",
                fparams![limit, offset],
                |row| {
                    let workspace_path: Option<String> = row.get_typed(2)?;
                    let source_path: String = row.get_typed(5)?;
                    let source_id: Option<String> = row.get_typed(10)?;
                    Ok(Conversation {
                        id: Some(row.get_typed(0)?),
                        agent_slug: row.get_typed(1)?,
                        workspace: workspace_path.map(|p| Path::new(&p).to_path_buf()),
                        external_id: row.get_typed(3)?,
                        title: row.get_typed(4)?,
                        source_path: Path::new(&source_path).to_path_buf(),
                        started_at: row.get_typed(6)?,
                        ended_at: row.get_typed(7)?,
                        approx_tokens: row.get_typed(8)?,
                        metadata_json: franken_read_metadata_compat(row, 9, 12),
                        messages: Vec::new(),
                        source_id: source_id.unwrap_or_else(|| "local".to_string()),
                        origin_host: row.get_typed(11)?,
                    })
                },
            )
            .with_context(|| "listing conversations")
    }

    /// Fetch messages for a conversation.
    pub fn fetch_messages(&self, conversation_id: i64) -> Result<Vec<Message>> {
        self.conn
            .query_map_collect(
                "SELECT id, idx, role, author, created_at, content, extra_json, extra_bin FROM messages WHERE conversation_id = ?1 ORDER BY idx",
                fparams![conversation_id],
                |row| {
                    let role: String = row.get_typed(2)?;
                    Ok(Message {
                        id: Some(row.get_typed(0)?),
                        idx: row.get_typed(1)?,
                        role: match role.as_str() {
                            "user" => MessageRole::User,
                            "agent" | "assistant" => MessageRole::Agent,
                            "tool" => MessageRole::Tool,
                            "system" => MessageRole::System,
                            other => MessageRole::Other(other.to_string()),
                        },
                        author: row.get_typed(3)?,
                        created_at: row.get_typed(4)?,
                        content: row.get_typed(5)?,
                        extra_json: franken_read_metadata_compat(row, 6, 7),
                        snippets: Vec::new(),
                    })
                },
            )
            .with_context(|| format!("fetching messages for conversation {conversation_id}"))
    }

    /// Get a source by ID.
    pub fn get_source(&self, id: &str) -> Result<Option<Source>> {
        let result = self.conn.query_row_map(
            "SELECT id, kind, host_label, machine_id, platform, config_json, created_at, updated_at FROM sources WHERE id = ?1",
            fparams![id],
            |row| {
                let kind_str: String = row.get_typed(1)?;
                let config_json_str: Option<String> = row.get_typed(5)?;
                Ok(Source {
                    id: row.get_typed(0)?,
                    kind: SourceKind::parse(&kind_str).unwrap_or_default(),
                    host_label: row.get_typed(2)?,
                    machine_id: row.get_typed(3)?,
                    platform: row.get_typed(4)?,
                    config_json: config_json_str.and_then(|s| serde_json::from_str(&s).ok()),
                    created_at: row.get_typed(6)?,
                    updated_at: row.get_typed(7)?,
                })
            },
        );
        Ok(result.optional()?)
    }

    /// List all sources.
    pub fn list_sources(&self) -> Result<Vec<Source>> {
        self.conn
            .query_map_collect(
                "SELECT id, kind, host_label, machine_id, platform, config_json, created_at, updated_at FROM sources ORDER BY id",
                fparams![],
                |row| {
                    let kind_str: String = row.get_typed(1)?;
                    let config_json_str: Option<String> = row.get_typed(5)?;
                    Ok(Source {
                        id: row.get_typed(0)?,
                        kind: SourceKind::parse(&kind_str).unwrap_or_default(),
                        host_label: row.get_typed(2)?,
                        machine_id: row.get_typed(3)?,
                        platform: row.get_typed(4)?,
                        config_json: config_json_str.and_then(|s| serde_json::from_str(&s).ok()),
                        created_at: row.get_typed(6)?,
                        updated_at: row.get_typed(7)?,
                    })
                },
            )
            .with_context(|| "listing sources")
    }

    /// Get IDs of all non-local sources.
    pub fn get_source_ids(&self) -> Result<Vec<String>> {
        self.conn
            .query_map_collect(
                "SELECT id FROM sources WHERE id != 'local' ORDER BY id",
                fparams![],
                |row| row.get_typed(0),
            )
            .with_context(|| "listing source ids")
    }

    /// Create or update a source.
    pub fn upsert_source(&self, source: &Source) -> Result<()> {
        let now = Self::now_millis();
        let kind_str = source.kind.to_string();
        let config_json_str = source
            .config_json
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        self.conn.execute_compat(
            "INSERT INTO sources(id, kind, host_label, machine_id, platform, config_json, created_at, updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(id) DO UPDATE SET
                kind=excluded.kind,
                host_label=excluded.host_label,
                machine_id=excluded.machine_id,
                platform=excluded.platform,
                config_json=excluded.config_json,
                updated_at=excluded.updated_at",
            fparams![
                source.id.as_str(),
                kind_str.as_str(),
                source.host_label.as_deref(),
                source.machine_id.as_deref(),
                source.platform.as_deref(),
                config_json_str.as_deref(),
                source.created_at.unwrap_or(now),
                now
            ],
        )?;
        Ok(())
    }

    /// Delete a source by ID. Returns true if a row was deleted.
    pub fn delete_source(&self, id: &str, _cascade: bool) -> Result<bool> {
        if id == LOCAL_SOURCE_ID {
            anyhow::bail!("cannot delete the local source");
        }
        let count = self
            .conn
            .execute_compat("DELETE FROM sources WHERE id = ?1", fparams![id])?;
        Ok(count > 0)
    }

    /// Insert a conversation tree (conversation + messages + snippets + FTS).
    pub fn insert_conversation_tree(
        &self,
        agent_id: i64,
        workspace_id: Option<i64>,
        conv: &Conversation,
    ) -> Result<InsertOutcome> {
        // Check for existing conversation with same (source_id, agent_id, external_id)
        if let Some(ext) = &conv.external_id {
            let existing: Option<i64> = self
                .conn
                .query_row_map(
                    "SELECT id FROM conversations WHERE source_id = ?1 AND agent_id = ?2 AND external_id = ?3",
                    fparams![conv.source_id.as_str(), agent_id, ext.as_str()],
                    |row| row.get_typed(0),
                )
                .optional()?;
            if let Some(existing_id) = existing {
                return self.franken_append_messages(existing_id, conv);
            }
        }

        let mut tx = self.conn.transaction()?;

        let conv_id = franken_insert_conversation(&tx, agent_id, workspace_id, conv)?;
        let mut fts_entries = Vec::with_capacity(conv.messages.len());
        let mut total_chars: i64 = 0;
        for msg in &conv.messages {
            let msg_id = franken_insert_message(&tx, conv_id, msg)?;
            franken_insert_snippets(&tx, msg_id, &msg.snippets)?;
            fts_entries.push(FtsEntry::from_message(msg_id, msg, conv));
            total_chars += msg.content.len() as i64;
        }
        franken_batch_insert_fts(&tx, &fts_entries)?;

        franken_update_daily_stats_in_tx(
            &tx,
            &conv.agent_slug,
            &conv.source_id,
            conv.started_at,
            1,
            conv.messages.len() as i64,
            total_chars,
        )?;

        tx.commit()?;
        Ok(InsertOutcome {
            conversation_id: conv_id,
            inserted_indices: conv.messages.iter().map(|m| m.idx).collect(),
        })
    }

    /// Append new messages to an existing conversation.
    fn franken_append_messages(
        &self,
        conversation_id: i64,
        conv: &Conversation,
    ) -> Result<InsertOutcome> {
        let mut tx = self.conn.transaction()?;

        let rows = tx.query_params(
            "SELECT MAX(idx) FROM messages WHERE conversation_id = ?1",
            fparams![conversation_id],
        )?;
        let cutoff: i64 = rows
            .first()
            .and_then(|r| r.get_typed::<Option<i64>>(0).ok())
            .flatten()
            .unwrap_or(-1);

        let mut inserted_indices = Vec::new();
        let mut fts_entries = Vec::new();
        let mut new_chars: i64 = 0;
        for msg in &conv.messages {
            if msg.idx <= cutoff {
                continue;
            }
            let msg_id = franken_insert_message(&tx, conversation_id, msg)?;
            franken_insert_snippets(&tx, msg_id, &msg.snippets)?;
            fts_entries.push(FtsEntry::from_message(msg_id, msg, conv));
            inserted_indices.push(msg.idx);
            new_chars += msg.content.len() as i64;
        }

        franken_batch_insert_fts(&tx, &fts_entries)?;

        if let Some(last_ts) = conv.messages.iter().filter_map(|m| m.created_at).max() {
            tx.execute_compat(
                "UPDATE conversations SET ended_at = MAX(IFNULL(ended_at, 0), ?1) WHERE id = ?2",
                fparams![last_ts, conversation_id],
            )?;
        }

        if !inserted_indices.is_empty() {
            let message_count = inserted_indices.len() as i64;
            franken_update_daily_stats_in_tx(
                &tx,
                &conv.agent_slug,
                &conv.source_id,
                conv.started_at,
                0,
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

    /// Rebuild the FTS5 index from scratch (chunked to avoid OOM on large databases, #110).
    pub fn rebuild_fts(&self) -> Result<()> {
        rebuild_fts_on_connection(&self.conn)
    }

    /// Fetch all messages for embedding generation.
    pub fn fetch_messages_for_embedding(&self) -> Result<Vec<MessageForEmbedding>> {
        self.conn
            .query_map_collect(
                "SELECT m.id, m.created_at, c.agent_id, c.workspace_id, c.source_id, m.role, m.content
                 FROM messages m
                 JOIN conversations c ON m.conversation_id = c.id
                 ORDER BY m.id",
                fparams![],
                |row| {
                    let source_id: String = row.get_typed::<Option<String>>(4)?
                        .unwrap_or_else(|| "local".to_string());
                    Ok(MessageForEmbedding {
                        message_id: row.get_typed(0)?,
                        created_at: row.get_typed(1)?,
                        agent_id: row.get_typed(2)?,
                        workspace_id: row.get_typed(3)?,
                        source_id_hash: crc32fast::hash(source_id.as_bytes()),
                        role: row.get_typed(5)?,
                        content: row.get_typed(6)?,
                    })
                },
            )
            .with_context(|| "fetching messages for embedding")
    }

    /// Get embedding jobs for a database path.
    pub fn get_embedding_jobs(&self, db_path: &str) -> Result<Vec<EmbeddingJobRow>> {
        self.conn
            .query_map_collect(
                "SELECT id, db_path, model_id, status, total_docs, completed_docs, error_message, created_at, started_at, completed_at
                 FROM embedding_jobs WHERE db_path = ?1 ORDER BY id DESC",
                fparams![db_path],
                |row| {
                    Ok(EmbeddingJobRow {
                        id: row.get_typed(0)?,
                        db_path: row.get_typed(1)?,
                        model_id: row.get_typed(2)?,
                        status: row.get_typed(3)?,
                        total_docs: row.get_typed(4)?,
                        completed_docs: row.get_typed(5)?,
                        error_message: row.get_typed(6)?,
                        created_at: row.get_typed(7)?,
                        started_at: row.get_typed(8)?,
                        completed_at: row.get_typed(9)?,
                    })
                },
            )
            .with_context(|| format!("fetching embedding jobs for {db_path}"))
    }

    /// Create or update an embedding job.
    pub fn upsert_embedding_job(
        &self,
        db_path: &str,
        model_id: &str,
        total_docs: i64,
    ) -> Result<i64> {
        self.conn.execute_compat(
            "INSERT INTO embedding_jobs(db_path, model_id, total_docs) VALUES(?1,?2,?3)
             ON CONFLICT(db_path, model_id) WHERE status IN ('pending', 'running')
             DO UPDATE SET total_docs=excluded.total_docs",
            fparams![db_path, model_id, total_docs],
        )?;
        let rows = self.conn.query("SELECT last_insert_rowid();")?;
        rows.first()
            .and_then(|r| r.get_typed::<i64>(0).ok())
            .filter(|&id| id > 0)
            .with_context(|| "last_insert_rowid() returned NULL or 0 after embedding job INSERT")
    }

    /// Mark an embedding job as started.
    pub fn start_embedding_job(&self, job_id: i64) -> Result<()> {
        self.conn.execute_compat(
            "UPDATE embedding_jobs SET status = 'running', started_at = datetime('now') WHERE id = ?1",
            fparams![job_id],
        )?;
        Ok(())
    }

    /// Mark an embedding job as completed.
    pub fn complete_embedding_job(&self, job_id: i64) -> Result<()> {
        self.conn.execute_compat(
            "UPDATE embedding_jobs SET status = 'completed', completed_at = datetime('now') WHERE id = ?1",
            fparams![job_id],
        )?;
        Ok(())
    }

    /// Mark an embedding job as failed.
    pub fn fail_embedding_job(&self, job_id: i64, error: &str) -> Result<()> {
        self.conn.execute_compat(
            "UPDATE embedding_jobs SET status = 'failed', error_message = ?2, completed_at = datetime('now') WHERE id = ?1",
            fparams![job_id, error],
        )?;
        Ok(())
    }

    /// Cancel embedding jobs for a database path.
    pub fn cancel_embedding_jobs(&self, db_path: &str, model_id: Option<&str>) -> Result<usize> {
        if let Some(mid) = model_id {
            Ok(self.conn.execute_compat(
                "UPDATE embedding_jobs SET status = 'cancelled' WHERE db_path = ?1 AND model_id = ?2 AND status IN ('pending', 'running')",
                fparams![db_path, mid],
            )?)
        } else {
            Ok(self.conn.execute_compat(
                "UPDATE embedding_jobs SET status = 'cancelled' WHERE db_path = ?1 AND status IN ('pending', 'running')",
                fparams![db_path],
            )?)
        }
    }

    /// Update embedding job progress.
    pub fn update_job_progress(&self, job_id: i64, completed_docs: i64) -> Result<()> {
        self.conn.execute_compat(
            "UPDATE embedding_jobs SET completed_docs = ?2 WHERE id = ?1",
            fparams![job_id, completed_docs],
        )?;
        Ok(())
    }

    // =====================================================================
    // Analytics query methods
    // =====================================================================

    /// Get session count for a date range using materialized stats.
    /// Returns (count, is_from_cache) where is_from_cache is true if from daily_stats.
    ///
    /// Falls back to COUNT(*) query when daily_stats table is empty or stale.
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
            .query_row_map("SELECT COUNT(*) FROM daily_stats", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap_or(0);

        if stats_count == 0 {
            return self.count_sessions_direct(start_ts_ms, end_ts_ms, agent_slug, source_id);
        }

        // Use materialized stats
        let start_day = start_ts_ms.map(Self::day_id_from_millis);
        let end_day = end_ts_ms.map(Self::day_id_from_millis);

        let count: i64 = match (start_day, end_day) {
            (Some(start), Some(end)) => self.conn.query_row_map(
                "SELECT COALESCE(SUM(session_count), 0) FROM daily_stats
                 WHERE day_id BETWEEN ?1 AND ?2 AND agent_slug = ?3 AND source_id = ?4",
                fparams![start, end, agent, source],
                |row| row.get_typed(0),
            )?,
            (Some(start), None) => self.conn.query_row_map(
                "SELECT COALESCE(SUM(session_count), 0) FROM daily_stats
                 WHERE day_id >= ?1 AND agent_slug = ?2 AND source_id = ?3",
                fparams![start, agent, source],
                |row| row.get_typed(0),
            )?,
            (None, Some(end)) => self.conn.query_row_map(
                "SELECT COALESCE(SUM(session_count), 0) FROM daily_stats
                 WHERE day_id <= ?1 AND agent_slug = ?2 AND source_id = ?3",
                fparams![end, agent, source],
                |row| row.get_typed(0),
            )?,
            (None, None) => self.conn.query_row_map(
                "SELECT COALESCE(SUM(session_count), 0) FROM daily_stats
                 WHERE agent_slug = ?1 AND source_id = ?2",
                fparams![agent, source],
                |row| row.get_typed(0),
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
        // Build dynamic SQL with positional params
        let mut sql = "SELECT COUNT(*) FROM conversations c
                       JOIN agents a ON c.agent_id = a.id WHERE 1=1"
            .to_string();
        let mut param_values: Vec<ParamValue> = Vec::new();
        let mut idx = 1;

        if let Some(start) = start_ts_ms {
            sql.push_str(&format!(" AND c.started_at >= ?{idx}"));
            param_values.push(ParamValue::from(start));
            idx += 1;
        }
        if let Some(end) = end_ts_ms {
            sql.push_str(&format!(" AND c.started_at <= ?{idx}"));
            param_values.push(ParamValue::from(end));
            idx += 1;
        }
        if let Some(agent) = agent_slug
            && agent != "all"
        {
            sql.push_str(&format!(" AND a.slug = ?{idx}"));
            param_values.push(ParamValue::from(agent));
            idx += 1;
        }
        if let Some(source) = source_id
            && source != "all"
        {
            sql.push_str(&format!(" AND c.source_id = ?{idx}"));
            param_values.push(ParamValue::from(source));
            let _ = idx; // suppress unused warning
        }

        let count: i64 = self
            .conn
            .query_row_map(&sql, &param_values, |row| row.get_typed(0))?;
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

        let rows = self.conn.query_map_collect(
            "SELECT day_id, session_count, message_count, total_chars
             FROM daily_stats
             WHERE day_id BETWEEN ?1 AND ?2 AND agent_slug = ?3 AND source_id = ?4
             ORDER BY day_id",
            fparams![start_day, end_day, agent, source],
            |row| {
                Ok(DailyCount {
                    day_id: row.get_typed(0)?,
                    sessions: row.get_typed(1)?,
                    messages: row.get_typed(2)?,
                    chars: row.get_typed(3)?,
                })
            },
        )?;

        Ok(rows)
    }

    /// Check health of daily stats table.
    pub fn daily_stats_health(&self) -> Result<DailyStatsHealth> {
        let row_count: i64 = self
            .conn
            .query_row_map("SELECT COUNT(*) FROM daily_stats", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap_or(0);

        let oldest_update: Option<i64> = self
            .conn
            .query_row_map(
                "SELECT MIN(last_updated) FROM daily_stats",
                fparams![],
                |row| row.get_typed(0),
            )
            .ok();

        let conversation_count: i64 = self
            .conn
            .query_row_map("SELECT COUNT(*) FROM conversations", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap_or(0);

        let materialized_total: i64 = self
            .conn
            .query_row_map(
                "SELECT COALESCE(SUM(session_count), 0) FROM daily_stats
                 WHERE agent_slug = 'all' AND source_id = 'all'",
                fparams![],
                |row| row.get_typed(0),
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

    /// Batch insert multiple conversations with full analytics (token usage,
    /// message metrics, rollups).  Frankensqlite equivalent of
    /// `SqliteStorage::insert_conversations_batched`.
    pub fn insert_conversations_batched(
        &self,
        conversations: &[(i64, Option<i64>, &Conversation)],
    ) -> Result<Vec<InsertOutcome>> {
        if conversations.is_empty() {
            return Ok(Vec::new());
        }

        let pricing_table = PricingTable::franken_load(&self.conn).unwrap_or_else(|e| {
            tracing::warn!(target: "cass::analytics::pricing", error = %e, "failed to load pricing table");
            PricingTable { entries: Vec::new() }
        });
        let mut pricing_diag = PricingDiagnostics::default();

        let mut tx = self.conn.transaction()?;
        let mut outcomes = Vec::with_capacity(conversations.len());
        let mut fts_entries = Vec::new();
        let mut stats = StatsAggregator::new();
        let mut token_stats = TokenStatsAggregator::new();
        let mut token_entries: Vec<TokenUsageEntry> = Vec::new();
        let mut metrics_entries: Vec<MessageMetricsEntry> = Vec::new();
        let mut rollup_agg = AnalyticsRollupAggregator::new();
        let mut conv_ids_to_summarize: Vec<i64> = Vec::new();

        for &(agent_id, workspace_id, conv) in conversations {
            let conv_id = franken_insert_conversation(&tx, agent_id, workspace_id, conv)?;
            let mut total_chars: i64 = 0;
            let mut inserted_indices = Vec::with_capacity(conv.messages.len());

            for msg in &conv.messages {
                let msg_id = franken_insert_message(&tx, conv_id, msg)?;
                franken_insert_snippets(&tx, msg_id, &msg.snippets)?;
                fts_entries.push(FtsEntry::from_message(msg_id, msg, conv));
                total_chars += msg.content.len() as i64;
                inserted_indices.push(msg.idx);
            }

            let delta = StatsDelta {
                session_count_delta: 1,
                message_count_delta: conv.messages.len() as i64,
                total_chars_delta: total_chars,
            };

            let day_id = conv
                .started_at
                .map(FrankenStorage::day_id_from_millis)
                .unwrap_or(0);
            stats.record_delta(
                &conv.agent_slug,
                &conv.source_id,
                day_id,
                delta.session_count_delta,
                delta.message_count_delta,
                delta.total_chars_delta,
            );

            // Extract token usage from newly inserted messages
            let conv_day_id = day_id;
            let mut session_model_family = String::from("unknown");
            let mut has_any_tokens = false;

            for msg in &conv.messages {
                let role_s = role_str(&msg.role);
                let usage = crate::connectors::extract_tokens_for_agent(
                    &conv.agent_slug,
                    &msg.extra_json,
                    &msg.content,
                    &role_s,
                );

                // Look up message_id from DB
                let msg_rows = tx.query_with_params(
                    "SELECT id FROM messages WHERE conversation_id = ?1 AND idx = ?2",
                    &param_slice_to_values(fparams![conv_id, msg.idx]),
                )?;
                let msg_id: Option<i64> = msg_rows.first().and_then(|r| r.get_typed::<i64>(0).ok());

                let Some(message_id) = msg_id else {
                    continue;
                };

                let msg_ts = msg.created_at.or(conv.started_at).unwrap_or(0);
                let msg_day_id = if msg_ts > 0 {
                    FrankenStorage::day_id_from_millis(msg_ts)
                } else {
                    conv_day_id
                };

                let model_info = usage
                    .model_name
                    .as_deref()
                    .map(crate::connectors::normalize_model);

                let model_family = model_info
                    .as_ref()
                    .map(|i| i.family.clone())
                    .unwrap_or_else(|| "unknown".into());
                let model_tier = model_info
                    .as_ref()
                    .map(|i| i.tier.clone())
                    .unwrap_or_else(|| "unknown".into());
                let provider = usage
                    .provider
                    .clone()
                    .or_else(|| model_info.as_ref().map(|i| i.provider.clone()))
                    .unwrap_or_else(|| "unknown".into());

                if model_family != "unknown" {
                    session_model_family = model_family.clone();
                }

                let estimated_cost = pricing_table.compute_cost(
                    usage.model_name.as_deref(),
                    msg_day_id,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_read_tokens,
                    usage.cache_creation_tokens,
                );
                if estimated_cost.is_some() {
                    pricing_diag.record_priced();
                } else if usage.has_token_data() {
                    pricing_diag.record_unpriced(usage.model_name.as_deref());
                }

                token_stats.record(
                    &conv.agent_slug,
                    &conv.source_id,
                    msg_day_id,
                    &model_family,
                    &role_s,
                    &usage,
                    msg.content.len() as i64,
                    estimated_cost.unwrap_or(0.0),
                );

                if usage.has_token_data() {
                    has_any_tokens = true;
                }

                let content_chars = msg.content.len() as i64;
                let content_tokens_est = content_chars / 4;
                let msg_hour_id = FrankenStorage::hour_id_from_millis(msg_ts);
                let has_plan = has_plan_for_role(&role_s, &msg.content);

                token_entries.push(TokenUsageEntry {
                    message_id,
                    conversation_id: conv_id,
                    agent_id,
                    workspace_id,
                    source_id: conv.source_id.clone(),
                    timestamp_ms: msg_ts,
                    day_id: msg_day_id,
                    model_name: usage.model_name.clone(),
                    model_family: Some(model_family.clone()),
                    model_tier: Some(model_tier.clone()),
                    service_tier: usage.service_tier.clone(),
                    provider: Some(provider.clone()),
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cache_read_tokens: usage.cache_read_tokens,
                    cache_creation_tokens: usage.cache_creation_tokens,
                    thinking_tokens: usage.thinking_tokens,
                    total_tokens: usage.total_tokens(),
                    estimated_cost_usd: estimated_cost,
                    role: role_s.clone(),
                    content_chars,
                    has_tool_calls: usage.has_tool_calls,
                    tool_call_count: usage.tool_call_count,
                    data_source: usage.data_source.as_str().to_string(),
                });

                let mm = MessageMetricsEntry {
                    message_id,
                    created_at_ms: msg_ts,
                    hour_id: msg_hour_id,
                    day_id: msg_day_id,
                    agent_slug: conv.agent_slug.clone(),
                    workspace_id: workspace_id.unwrap_or(0),
                    source_id: conv.source_id.clone(),
                    role: role_s,
                    content_chars,
                    content_tokens_est,
                    model_name: usage.model_name.clone(),
                    model_family: model_family.clone(),
                    model_tier: model_tier.clone(),
                    provider,
                    api_input_tokens: usage.input_tokens,
                    api_output_tokens: usage.output_tokens,
                    api_cache_read_tokens: usage.cache_read_tokens,
                    api_cache_creation_tokens: usage.cache_creation_tokens,
                    api_thinking_tokens: usage.thinking_tokens,
                    api_service_tier: usage.service_tier.clone(),
                    api_data_source: usage.data_source.as_str().to_string(),
                    tool_call_count: usage.tool_call_count as i64,
                    has_tool_calls: usage.has_tool_calls,
                    has_plan,
                };
                rollup_agg.record(&mm);
                metrics_entries.push(mm);
            }

            if delta.session_count_delta > 0 {
                token_stats.record_session(
                    &conv.agent_slug,
                    &conv.source_id,
                    conv_day_id,
                    &session_model_family,
                );
            }

            if has_any_tokens {
                conv_ids_to_summarize.push(conv_id);
            }

            outcomes.push(InsertOutcome {
                conversation_id: conv_id,
                inserted_indices,
            });
        }

        // Batch insert all FTS entries at once
        let fts_count = fts_entries.len();
        if fts_count > 0 {
            let inserted = franken_batch_insert_fts(&tx, &fts_entries)?;
            tracing::debug!(
                target: "cass::perf::fts5",
                total = fts_count,
                inserted = inserted,
                conversations = conversations.len(),
                "franken_batch_fts_insert_complete"
            );
        }

        // Batched daily_stats update
        if !stats.is_empty() {
            let entries = stats.expand();
            let affected = franken_update_daily_stats_batched_in_tx(&tx, &entries)?;
            tracing::debug!(
                target: "cass::perf::daily_stats",
                raw = stats.raw_entry_count(),
                expanded = entries.len(),
                affected = affected,
                "franken_batched_stats_update_complete"
            );
        }

        // Batch insert token_usage rows
        if !token_entries.is_empty() {
            let token_count = token_entries.len();
            let inserted = franken_insert_token_usage_batched_in_tx(&tx, &token_entries)?;
            tracing::debug!(
                target: "cass::perf::token_usage",
                total = token_count,
                inserted = inserted,
                "franken_batch_token_usage_insert_complete"
            );
        }

        // Batched token_daily_stats update
        if !token_stats.is_empty() {
            let entries = token_stats.expand();
            let affected = franken_update_token_daily_stats_batched_in_tx(&tx, &entries)?;
            tracing::debug!(
                target: "cass::perf::token_daily_stats",
                raw = token_stats.raw_entry_count(),
                expanded = entries.len(),
                affected = affected,
                "franken_batched_token_stats_update_complete"
            );
        }

        // Batch insert message_metrics rows
        if !metrics_entries.is_empty() {
            let mm_count = metrics_entries.len();
            let inserted = franken_insert_message_metrics_batched_in_tx(&tx, &metrics_entries)?;
            tracing::debug!(
                target: "cass::perf::message_metrics",
                total = mm_count,
                inserted = inserted,
                "franken_batch_message_metrics_insert_complete"
            );
        }

        // Flush usage_hourly + usage_daily rollups
        if !rollup_agg.is_empty() {
            let (hourly, daily, models_daily) =
                franken_flush_analytics_rollups_in_tx(&tx, &rollup_agg)?;
            tracing::debug!(
                target: "cass::perf::usage_rollups",
                hourly_buckets = rollup_agg.hourly_entry_count(),
                daily_buckets = rollup_agg.daily_entry_count(),
                models_daily_buckets = rollup_agg.models_daily_entry_count(),
                hourly_affected = hourly,
                daily_affected = daily,
                models_daily_affected = models_daily,
                "franken_batched_usage_rollups_complete"
            );
        }

        // Update conversation-level token summaries
        for conv_id in &conv_ids_to_summarize {
            franken_update_conversation_token_summaries_in_tx(&tx, *conv_id)?;
        }

        tx.commit()?;

        pricing_diag.log_summary();

        Ok(outcomes)
    }
}

// =========================================================================
// FrankenStorage transaction helper functions
// =========================================================================

/// Get last_insert_rowid from a frankensqlite transaction.
fn franken_last_rowid(tx: &FrankenTransaction<'_>) -> Result<i64> {
    let rows = tx.query("SELECT last_insert_rowid();")?;
    rows.first()
        .and_then(|r| r.get_typed::<i64>(0).ok())
        .filter(|&id| id > 0)
        .with_context(|| "last_insert_rowid() returned NULL or 0 after INSERT")
}

/// Insert a conversation into the DB within a frankensqlite transaction.
fn franken_insert_conversation(
    tx: &FrankenTransaction<'_>,
    agent_id: i64,
    workspace_id: Option<i64>,
    conv: &Conversation,
) -> Result<i64> {
    let metadata_bin = serialize_json_to_msgpack(&conv.metadata_json);

    let metadata_json_str = serde_json::to_string(&conv.metadata_json)?;
    let metadata_bin_bytes = metadata_bin.as_deref();

    tx.execute_compat(
        "INSERT INTO conversations(
            agent_id, workspace_id, source_id, external_id, title, source_path,
            started_at, ended_at, approx_tokens, metadata_json, origin_host, metadata_bin
        ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        fparams![
            agent_id,
            workspace_id,
            conv.source_id.as_str(),
            conv.external_id.as_deref(),
            conv.title.as_deref(),
            path_to_string(&conv.source_path),
            conv.started_at,
            conv.ended_at,
            conv.approx_tokens,
            metadata_json_str.as_str(),
            conv.origin_host.as_deref(),
            metadata_bin_bytes
        ],
    )?;
    franken_last_rowid(tx)
}

/// Insert a message within a frankensqlite transaction.
fn franken_insert_message(
    tx: &FrankenTransaction<'_>,
    conversation_id: i64,
    msg: &Message,
) -> Result<i64> {
    let extra_bin = serialize_json_to_msgpack(&msg.extra_json);

    let extra_json_str = serde_json::to_string(&msg.extra_json)?;
    let extra_bin_bytes = extra_bin.as_deref();

    tx.execute_compat(
        "INSERT INTO messages(conversation_id, idx, role, author, created_at, content, extra_json, extra_bin)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        fparams![
            conversation_id,
            msg.idx,
            role_str(&msg.role),
            msg.author.as_deref(),
            msg.created_at,
            msg.content.as_str(),
            extra_json_str.as_str(),
            extra_bin_bytes
        ],
    )?;
    franken_last_rowid(tx)
}

/// Insert snippets within a frankensqlite transaction.
fn franken_insert_snippets(
    tx: &FrankenTransaction<'_>,
    message_id: i64,
    snippets: &[Snippet],
) -> Result<()> {
    for snip in snippets {
        let file_path_str = snip.file_path.as_ref().map(path_to_string);
        tx.execute_compat(
            "INSERT INTO snippets(message_id, file_path, start_line, end_line, language, snippet_text)
             VALUES(?1,?2,?3,?4,?5,?6)",
            fparams![
                message_id,
                file_path_str.as_deref(),
                snip.start_line,
                snip.end_line,
                snip.language.as_deref(),
                snip.snippet_text.as_deref()
            ],
        )?;
    }
    Ok(())
}

/// Batch insert FTS5 entries within a frankensqlite transaction.
fn franken_batch_insert_fts(tx: &FrankenTransaction<'_>, entries: &[FtsEntry]) -> Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }

    let mut inserted = 0;

    for chunk in entries.chunks(FTS5_BATCH_SIZE) {
        let placeholders: String = chunk
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let base = i * 7 + 1; // +1 for 1-indexed params
                format!(
                    "(?{},?{},?{},?{},?{},?{},?{})",
                    base,
                    base + 1,
                    base + 2,
                    base + 3,
                    base + 4,
                    base + 5,
                    base + 6
                )
            })
            .collect::<Vec<_>>()
            .join(",");

        let sql = format!(
            "INSERT INTO fts_messages(rowid, content, title, agent, workspace, source_path, created_at) VALUES {placeholders}"
        );

        let mut param_values: Vec<ParamValue> = Vec::with_capacity(chunk.len() * 7);
        for entry in chunk {
            param_values.push(ParamValue::from(entry.message_id));
            param_values.push(ParamValue::from(entry.content.as_str()));
            param_values.push(ParamValue::from(entry.title.as_str()));
            param_values.push(ParamValue::from(entry.agent.as_str()));
            param_values.push(ParamValue::from(entry.workspace.as_str()));
            param_values.push(ParamValue::from(entry.source_path.as_str()));
            param_values.push(ParamValue::from(entry.created_at));
        }

        tx.execute_compat(&sql, &param_values)?;
        inserted += chunk.len();
    }

    Ok(inserted)
}

/// Update daily stats within a frankensqlite transaction.
fn franken_update_daily_stats_in_tx(
    tx: &FrankenTransaction<'_>,
    agent_slug: &str,
    source_id: &str,
    started_at: Option<i64>,
    session_delta: i64,
    message_delta: i64,
    chars_delta: i64,
) -> Result<()> {
    let day_id = started_at
        .map(FrankenStorage::day_id_from_millis)
        .unwrap_or(0);
    let now = FrankenStorage::now_millis();

    // Update agent-specific entry
    tx.execute_compat(
        "INSERT INTO daily_stats(day_id, agent_slug, source_id, session_count, message_count, total_chars, last_updated)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(day_id, agent_slug, source_id) DO UPDATE SET
            session_count = session_count + excluded.session_count,
            message_count = message_count + excluded.message_count,
            total_chars = total_chars + excluded.total_chars,
            last_updated = excluded.last_updated",
        fparams![day_id, agent_slug, source_id, session_delta, message_delta, chars_delta, now],
    )?;

    // Update 'all' agent entry
    tx.execute_compat(
        "INSERT INTO daily_stats(day_id, agent_slug, source_id, session_count, message_count, total_chars, last_updated)
         VALUES(?1,'all',?2,?3,?4,?5,?6)
         ON CONFLICT(day_id, agent_slug, source_id) DO UPDATE SET
            session_count = session_count + excluded.session_count,
            message_count = message_count + excluded.message_count,
            total_chars = total_chars + excluded.total_chars,
            last_updated = excluded.last_updated",
        fparams![day_id, source_id, session_delta, message_delta, chars_delta, now],
    )?;

    // Update 'all' source entry
    tx.execute_compat(
        "INSERT INTO daily_stats(day_id, agent_slug, source_id, session_count, message_count, total_chars, last_updated)
         VALUES(?1,?2,'all',?3,?4,?5,?6)
         ON CONFLICT(day_id, agent_slug, source_id) DO UPDATE SET
            session_count = session_count + excluded.session_count,
            message_count = message_count + excluded.message_count,
            total_chars = total_chars + excluded.total_chars,
            last_updated = excluded.last_updated",
        fparams![day_id, agent_slug, session_delta, message_delta, chars_delta, now],
    )?;

    // Update global 'all'/'all' entry
    tx.execute_compat(
        "INSERT INTO daily_stats(day_id, agent_slug, source_id, session_count, message_count, total_chars, last_updated)
         VALUES(?1,'all','all',?2,?3,?4,?5)
         ON CONFLICT(day_id, agent_slug, source_id) DO UPDATE SET
            session_count = session_count + excluded.session_count,
            message_count = message_count + excluded.message_count,
            total_chars = total_chars + excluded.total_chars,
            last_updated = excluded.last_updated",
        fparams![day_id, session_delta, message_delta, chars_delta, now],
    )?;

    Ok(())
}

// -------------------------------------------------------------------------
// Frankensqlite batch helpers (migration from rusqlite `_in_tx` functions)
// -------------------------------------------------------------------------

/// Batch upsert daily_stats within a frankensqlite transaction.
fn franken_update_daily_stats_batched_in_tx(
    tx: &FrankenTransaction<'_>,
    entries: &[(i64, String, String, StatsDelta)],
) -> Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }

    let now = FrankenStorage::now_millis();
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

        let mut params_vec: Vec<ParamValue> = Vec::with_capacity(chunk.len() * 7);
        for (day_id, agent, source, delta) in chunk {
            params_vec.push(ParamValue::from(*day_id));
            params_vec.push(ParamValue::from(agent.clone()));
            params_vec.push(ParamValue::from(source.clone()));
            params_vec.push(ParamValue::from(delta.session_count_delta));
            params_vec.push(ParamValue::from(delta.message_count_delta));
            params_vec.push(ParamValue::from(delta.total_chars_delta));
            params_vec.push(ParamValue::from(now));
        }

        let values = param_slice_to_values(&params_vec);
        total_affected += tx.execute_with_params(&sql, &values)?;
    }

    Ok(total_affected)
}

/// Batch insert token_usage rows within a frankensqlite transaction.
fn franken_insert_token_usage_batched_in_tx(
    tx: &FrankenTransaction<'_>,
    entries: &[TokenUsageEntry],
) -> Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }

    // 24 columns per row; SQLite limit ~999 params → batch ~41 rows, use 35 for safety
    const BATCH_SIZE: usize = 35;
    let mut total_inserted = 0;

    for chunk in entries.chunks(BATCH_SIZE) {
        let placeholders: String = (0..chunk.len())
            .map(|_| "(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT OR IGNORE INTO token_usage (
                message_id, conversation_id, agent_id, workspace_id, source_id,
                timestamp_ms, day_id,
                model_name, model_family, model_tier, service_tier, provider,
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                thinking_tokens, total_tokens, estimated_cost_usd,
                role, content_chars, has_tool_calls, tool_call_count, data_source
            )
            VALUES {}",
            placeholders
        );

        let mut params_vec: Vec<ParamValue> = Vec::with_capacity(chunk.len() * 24);
        for e in chunk {
            params_vec.push(ParamValue::from(e.message_id));
            params_vec.push(ParamValue::from(e.conversation_id));
            params_vec.push(ParamValue::from(e.agent_id));
            params_vec.push(ParamValue::from(e.workspace_id));
            params_vec.push(ParamValue::from(e.source_id.clone()));
            params_vec.push(ParamValue::from(e.timestamp_ms));
            params_vec.push(ParamValue::from(e.day_id));
            params_vec.push(ParamValue::from(e.model_name.clone()));
            params_vec.push(ParamValue::from(e.model_family.clone()));
            params_vec.push(ParamValue::from(e.model_tier.clone()));
            params_vec.push(ParamValue::from(e.service_tier.clone()));
            params_vec.push(ParamValue::from(e.provider.clone()));
            params_vec.push(ParamValue::from(e.input_tokens));
            params_vec.push(ParamValue::from(e.output_tokens));
            params_vec.push(ParamValue::from(e.cache_read_tokens));
            params_vec.push(ParamValue::from(e.cache_creation_tokens));
            params_vec.push(ParamValue::from(e.thinking_tokens));
            params_vec.push(ParamValue::from(e.total_tokens));
            params_vec.push(ParamValue::from(e.estimated_cost_usd));
            params_vec.push(ParamValue::from(e.role.clone()));
            params_vec.push(ParamValue::from(e.content_chars));
            params_vec.push(ParamValue::from(e.has_tool_calls as i64));
            params_vec.push(ParamValue::from(e.tool_call_count as i64));
            params_vec.push(ParamValue::from(e.data_source.clone()));
        }

        let values = param_slice_to_values(&params_vec);
        total_inserted += tx.execute_with_params(&sql, &values)?;
    }

    Ok(total_inserted)
}

/// Batch upsert token_daily_stats within a frankensqlite transaction.
fn franken_update_token_daily_stats_batched_in_tx(
    tx: &FrankenTransaction<'_>,
    entries: &[(i64, String, String, String, TokenStatsDelta)],
) -> Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }

    let now = FrankenStorage::now_millis();
    const BATCH_SIZE: usize = 25; // 19 params per row → ~52 rows max, use 25 for safety

    let mut total_affected = 0;

    for chunk in entries.chunks(BATCH_SIZE) {
        let placeholders: String = (0..chunk.len())
            .map(|_| "(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT INTO token_daily_stats (
                day_id, agent_slug, source_id, model_family,
                api_call_count, user_message_count, assistant_message_count, tool_message_count,
                total_input_tokens, total_output_tokens, total_cache_read_tokens,
                total_cache_creation_tokens, total_thinking_tokens, grand_total_tokens,
                total_content_chars, total_tool_calls, estimated_cost_usd, session_count,
                last_updated
            )
            VALUES {}
            ON CONFLICT(day_id, agent_slug, source_id, model_family) DO UPDATE SET
                api_call_count = api_call_count + excluded.api_call_count,
                user_message_count = user_message_count + excluded.user_message_count,
                assistant_message_count = assistant_message_count + excluded.assistant_message_count,
                tool_message_count = tool_message_count + excluded.tool_message_count,
                total_input_tokens = total_input_tokens + excluded.total_input_tokens,
                total_output_tokens = total_output_tokens + excluded.total_output_tokens,
                total_cache_read_tokens = total_cache_read_tokens + excluded.total_cache_read_tokens,
                total_cache_creation_tokens = total_cache_creation_tokens + excluded.total_cache_creation_tokens,
                total_thinking_tokens = total_thinking_tokens + excluded.total_thinking_tokens,
                grand_total_tokens = grand_total_tokens + excluded.grand_total_tokens,
                total_content_chars = total_content_chars + excluded.total_content_chars,
                total_tool_calls = total_tool_calls + excluded.total_tool_calls,
                estimated_cost_usd = estimated_cost_usd + excluded.estimated_cost_usd,
                session_count = session_count + excluded.session_count,
                last_updated = excluded.last_updated",
            placeholders
        );

        let mut params_vec: Vec<ParamValue> = Vec::with_capacity(chunk.len() * 19);
        for (day_id, agent, source, model, delta) in chunk {
            params_vec.push(ParamValue::from(*day_id));
            params_vec.push(ParamValue::from(agent.clone()));
            params_vec.push(ParamValue::from(source.clone()));
            params_vec.push(ParamValue::from(model.clone()));
            params_vec.push(ParamValue::from(delta.api_call_count));
            params_vec.push(ParamValue::from(delta.user_message_count));
            params_vec.push(ParamValue::from(delta.assistant_message_count));
            params_vec.push(ParamValue::from(delta.tool_message_count));
            params_vec.push(ParamValue::from(delta.total_input_tokens));
            params_vec.push(ParamValue::from(delta.total_output_tokens));
            params_vec.push(ParamValue::from(delta.total_cache_read_tokens));
            params_vec.push(ParamValue::from(delta.total_cache_creation_tokens));
            params_vec.push(ParamValue::from(delta.total_thinking_tokens));
            params_vec.push(ParamValue::from(delta.grand_total_tokens));
            params_vec.push(ParamValue::from(delta.total_content_chars));
            params_vec.push(ParamValue::from(delta.total_tool_calls));
            params_vec.push(ParamValue::from(delta.estimated_cost_usd));
            params_vec.push(ParamValue::from(delta.session_count));
            params_vec.push(ParamValue::from(now));
        }

        let values = param_slice_to_values(&params_vec);
        total_affected += tx.execute_with_params(&sql, &values)?;
    }

    Ok(total_affected)
}

/// Batch insert message_metrics rows within a frankensqlite transaction.
fn franken_insert_message_metrics_batched_in_tx(
    tx: &FrankenTransaction<'_>,
    entries: &[MessageMetricsEntry],
) -> Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }

    // 24 columns per row → ~41 rows max, use 30 for safety
    const BATCH_SIZE: usize = 30;
    let mut total_inserted = 0;

    for chunk in entries.chunks(BATCH_SIZE) {
        let placeholders: String = (0..chunk.len())
            .map(|_| "(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT OR IGNORE INTO message_metrics (
                message_id, created_at_ms, hour_id, day_id,
                agent_slug, workspace_id, source_id, role,
                content_chars, content_tokens_est,
                model_name, model_family, model_tier, provider,
                api_input_tokens, api_output_tokens, api_cache_read_tokens,
                api_cache_creation_tokens, api_thinking_tokens,
                api_service_tier, api_data_source,
                tool_call_count, has_tool_calls, has_plan
            )
            VALUES {}",
            placeholders
        );

        let mut params_vec: Vec<ParamValue> = Vec::with_capacity(chunk.len() * 24);
        for e in chunk {
            params_vec.push(ParamValue::from(e.message_id));
            params_vec.push(ParamValue::from(e.created_at_ms));
            params_vec.push(ParamValue::from(e.hour_id));
            params_vec.push(ParamValue::from(e.day_id));
            params_vec.push(ParamValue::from(e.agent_slug.clone()));
            params_vec.push(ParamValue::from(e.workspace_id));
            params_vec.push(ParamValue::from(e.source_id.clone()));
            params_vec.push(ParamValue::from(e.role.clone()));
            params_vec.push(ParamValue::from(e.content_chars));
            params_vec.push(ParamValue::from(e.content_tokens_est));
            params_vec.push(ParamValue::from(e.model_name.clone()));
            params_vec.push(ParamValue::from(e.model_family.clone()));
            params_vec.push(ParamValue::from(e.model_tier.clone()));
            params_vec.push(ParamValue::from(e.provider.clone()));
            params_vec.push(ParamValue::from(e.api_input_tokens));
            params_vec.push(ParamValue::from(e.api_output_tokens));
            params_vec.push(ParamValue::from(e.api_cache_read_tokens));
            params_vec.push(ParamValue::from(e.api_cache_creation_tokens));
            params_vec.push(ParamValue::from(e.api_thinking_tokens));
            params_vec.push(ParamValue::from(e.api_service_tier.clone()));
            params_vec.push(ParamValue::from(e.api_data_source.clone()));
            params_vec.push(ParamValue::from(e.tool_call_count));
            params_vec.push(ParamValue::from(e.has_tool_calls as i64));
            params_vec.push(ParamValue::from(e.has_plan as i64));
        }

        let values = param_slice_to_values(&params_vec);
        total_inserted += tx.execute_with_params(&sql, &values)?;
    }

    Ok(total_inserted)
}

/// Flush one rollup table (shared logic for hourly + daily) within a frankensqlite transaction.
fn franken_flush_rollup_table(
    tx: &FrankenTransaction<'_>,
    table: &str,
    bucket_col: &str,
    deltas: &HashMap<(i64, String, i64, String), UsageRollupDelta>,
    now: i64,
) -> Result<usize> {
    if deltas.is_empty() {
        return Ok(0);
    }

    // 22 params per row → ~44 rows max, use 30 for safety
    const BATCH_SIZE: usize = 30;
    let mut total_affected = 0;

    let entries: Vec<_> = deltas.iter().collect();

    for chunk in entries.chunks(BATCH_SIZE) {
        let placeholders: String = (0..chunk.len())
            .map(|_| "(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT INTO {table} (
                {bucket_col}, agent_slug, workspace_id, source_id,
                message_count, user_message_count, assistant_message_count,
                tool_call_count, plan_message_count, plan_content_tokens_est_total,
                plan_api_tokens_total, api_coverage_message_count,
                content_tokens_est_total, content_tokens_est_user, content_tokens_est_assistant,
                api_tokens_total, api_input_tokens_total, api_output_tokens_total,
                api_cache_read_tokens_total, api_cache_creation_tokens_total,
                api_thinking_tokens_total, last_updated
            )
            VALUES {placeholders}
            ON CONFLICT({bucket_col}, agent_slug, workspace_id, source_id) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                user_message_count = user_message_count + excluded.user_message_count,
                assistant_message_count = assistant_message_count + excluded.assistant_message_count,
                tool_call_count = tool_call_count + excluded.tool_call_count,
                plan_message_count = plan_message_count + excluded.plan_message_count,
                plan_content_tokens_est_total = plan_content_tokens_est_total + excluded.plan_content_tokens_est_total,
                plan_api_tokens_total = plan_api_tokens_total + excluded.plan_api_tokens_total,
                api_coverage_message_count = api_coverage_message_count + excluded.api_coverage_message_count,
                content_tokens_est_total = content_tokens_est_total + excluded.content_tokens_est_total,
                content_tokens_est_user = content_tokens_est_user + excluded.content_tokens_est_user,
                content_tokens_est_assistant = content_tokens_est_assistant + excluded.content_tokens_est_assistant,
                api_tokens_total = api_tokens_total + excluded.api_tokens_total,
                api_input_tokens_total = api_input_tokens_total + excluded.api_input_tokens_total,
                api_output_tokens_total = api_output_tokens_total + excluded.api_output_tokens_total,
                api_cache_read_tokens_total = api_cache_read_tokens_total + excluded.api_cache_read_tokens_total,
                api_cache_creation_tokens_total = api_cache_creation_tokens_total + excluded.api_cache_creation_tokens_total,
                api_thinking_tokens_total = api_thinking_tokens_total + excluded.api_thinking_tokens_total,
                last_updated = excluded.last_updated"
        );

        let mut params_vec: Vec<ParamValue> = Vec::with_capacity(chunk.len() * 22);
        for &((bucket_id, agent, workspace_id, source), d) in chunk {
            params_vec.push(ParamValue::from(*bucket_id));
            params_vec.push(ParamValue::from(agent.clone()));
            params_vec.push(ParamValue::from(*workspace_id));
            params_vec.push(ParamValue::from(source.clone()));
            params_vec.push(ParamValue::from(d.message_count));
            params_vec.push(ParamValue::from(d.user_message_count));
            params_vec.push(ParamValue::from(d.assistant_message_count));
            params_vec.push(ParamValue::from(d.tool_call_count));
            params_vec.push(ParamValue::from(d.plan_message_count));
            params_vec.push(ParamValue::from(d.plan_content_tokens_est_total));
            params_vec.push(ParamValue::from(d.plan_api_tokens_total));
            params_vec.push(ParamValue::from(d.api_coverage_message_count));
            params_vec.push(ParamValue::from(d.content_tokens_est_total));
            params_vec.push(ParamValue::from(d.content_tokens_est_user));
            params_vec.push(ParamValue::from(d.content_tokens_est_assistant));
            params_vec.push(ParamValue::from(d.api_tokens_total));
            params_vec.push(ParamValue::from(d.api_input_tokens_total));
            params_vec.push(ParamValue::from(d.api_output_tokens_total));
            params_vec.push(ParamValue::from(d.api_cache_read_tokens_total));
            params_vec.push(ParamValue::from(d.api_cache_creation_tokens_total));
            params_vec.push(ParamValue::from(d.api_thinking_tokens_total));
            params_vec.push(ParamValue::from(now));
        }

        let values = param_slice_to_values(&params_vec);
        total_affected += tx.execute_with_params(&sql, &values)?;
    }

    Ok(total_affected)
}

/// Flush usage_models_daily rollup within a frankensqlite transaction.
fn franken_flush_model_daily_rollup_table(
    tx: &FrankenTransaction<'_>,
    deltas: &HashMap<(i64, String, i64, String, String, String), UsageRollupDelta>,
    now: i64,
) -> Result<usize> {
    if deltas.is_empty() {
        return Ok(0);
    }

    const BATCH_SIZE: usize = 25;
    let mut total_affected = 0;

    let entries: Vec<_> = deltas.iter().collect();

    for chunk in entries.chunks(BATCH_SIZE) {
        let placeholders: String = (0..chunk.len())
            .map(|_| "(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT INTO usage_models_daily (
                day_id, agent_slug, workspace_id, source_id, model_family, model_tier,
                message_count, user_message_count, assistant_message_count,
                tool_call_count, plan_message_count, api_coverage_message_count,
                content_tokens_est_total, content_tokens_est_user, content_tokens_est_assistant,
                api_tokens_total, api_input_tokens_total, api_output_tokens_total,
                api_cache_read_tokens_total, api_cache_creation_tokens_total,
                api_thinking_tokens_total, last_updated
            )
            VALUES {placeholders}
            ON CONFLICT(day_id, agent_slug, workspace_id, source_id, model_family, model_tier) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                user_message_count = user_message_count + excluded.user_message_count,
                assistant_message_count = assistant_message_count + excluded.assistant_message_count,
                tool_call_count = tool_call_count + excluded.tool_call_count,
                plan_message_count = plan_message_count + excluded.plan_message_count,
                api_coverage_message_count = api_coverage_message_count + excluded.api_coverage_message_count,
                content_tokens_est_total = content_tokens_est_total + excluded.content_tokens_est_total,
                content_tokens_est_user = content_tokens_est_user + excluded.content_tokens_est_user,
                content_tokens_est_assistant = content_tokens_est_assistant + excluded.content_tokens_est_assistant,
                api_tokens_total = api_tokens_total + excluded.api_tokens_total,
                api_input_tokens_total = api_input_tokens_total + excluded.api_input_tokens_total,
                api_output_tokens_total = api_output_tokens_total + excluded.api_output_tokens_total,
                api_cache_read_tokens_total = api_cache_read_tokens_total + excluded.api_cache_read_tokens_total,
                api_cache_creation_tokens_total = api_cache_creation_tokens_total + excluded.api_cache_creation_tokens_total,
                api_thinking_tokens_total = api_thinking_tokens_total + excluded.api_thinking_tokens_total,
                last_updated = excluded.last_updated"
        );

        let mut params_vec: Vec<ParamValue> = Vec::with_capacity(chunk.len() * 22);
        for &((day_id, agent, workspace_id, source, model_family, model_tier), d) in chunk {
            params_vec.push(ParamValue::from(*day_id));
            params_vec.push(ParamValue::from(agent.clone()));
            params_vec.push(ParamValue::from(*workspace_id));
            params_vec.push(ParamValue::from(source.clone()));
            params_vec.push(ParamValue::from(model_family.clone()));
            params_vec.push(ParamValue::from(model_tier.clone()));
            params_vec.push(ParamValue::from(d.message_count));
            params_vec.push(ParamValue::from(d.user_message_count));
            params_vec.push(ParamValue::from(d.assistant_message_count));
            params_vec.push(ParamValue::from(d.tool_call_count));
            params_vec.push(ParamValue::from(d.plan_message_count));
            params_vec.push(ParamValue::from(d.api_coverage_message_count));
            params_vec.push(ParamValue::from(d.content_tokens_est_total));
            params_vec.push(ParamValue::from(d.content_tokens_est_user));
            params_vec.push(ParamValue::from(d.content_tokens_est_assistant));
            params_vec.push(ParamValue::from(d.api_tokens_total));
            params_vec.push(ParamValue::from(d.api_input_tokens_total));
            params_vec.push(ParamValue::from(d.api_output_tokens_total));
            params_vec.push(ParamValue::from(d.api_cache_read_tokens_total));
            params_vec.push(ParamValue::from(d.api_cache_creation_tokens_total));
            params_vec.push(ParamValue::from(d.api_thinking_tokens_total));
            params_vec.push(ParamValue::from(now));
        }

        let values = param_slice_to_values(&params_vec);
        total_affected += tx.execute_with_params(&sql, &values)?;
    }

    Ok(total_affected)
}

/// Flush AnalyticsRollupAggregator deltas via frankensqlite transaction.
fn franken_flush_analytics_rollups_in_tx(
    tx: &FrankenTransaction<'_>,
    agg: &AnalyticsRollupAggregator,
) -> Result<(usize, usize, usize)> {
    let now = FrankenStorage::now_millis();

    let hourly_affected =
        franken_flush_rollup_table(tx, "usage_hourly", "hour_id", &agg.hourly, now)?;
    let daily_affected = franken_flush_rollup_table(tx, "usage_daily", "day_id", &agg.daily, now)?;
    let models_daily_affected = franken_flush_model_daily_rollup_table(tx, &agg.models_daily, now)?;

    Ok((hourly_affected, daily_affected, models_daily_affected))
}

/// Update conversation-level token summary columns via frankensqlite transaction.
fn franken_update_conversation_token_summaries_in_tx(
    tx: &FrankenTransaction<'_>,
    conversation_id: i64,
) -> Result<()> {
    tx.execute_compat(
        "UPDATE conversations SET
            total_input_tokens = (SELECT SUM(input_tokens) FROM token_usage WHERE conversation_id = ?1),
            total_output_tokens = (SELECT SUM(output_tokens) FROM token_usage WHERE conversation_id = ?1),
            total_cache_read_tokens = (SELECT SUM(cache_read_tokens) FROM token_usage WHERE conversation_id = ?1),
            total_cache_creation_tokens = (SELECT SUM(cache_creation_tokens) FROM token_usage WHERE conversation_id = ?1),
            grand_total_tokens = (SELECT SUM(total_tokens) FROM token_usage WHERE conversation_id = ?1),
            estimated_cost_usd = (SELECT SUM(estimated_cost_usd) FROM token_usage WHERE conversation_id = ?1),
            primary_model = (SELECT model_name FROM token_usage WHERE conversation_id = ?1
                             AND model_name IS NOT NULL
                             GROUP BY model_name ORDER BY COUNT(*) DESC LIMIT 1),
            api_call_count = (SELECT COUNT(*) FROM token_usage WHERE conversation_id = ?1
                              AND data_source = 'api'),
            tool_call_count = (SELECT SUM(tool_call_count) FROM token_usage WHERE conversation_id = ?1),
            user_message_count = (SELECT COUNT(*) FROM token_usage WHERE conversation_id = ?1
                                  AND role = 'user'),
            assistant_message_count = (SELECT COUNT(*) FROM token_usage WHERE conversation_id = ?1
                                       AND role IN ('assistant', 'agent'))
         WHERE id = ?1",
        fparams![conversation_id],
    )?;
    Ok(())
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

    /// Resolve the primary SQLite database path for this connection.
    ///
    /// Uses `PRAGMA database_list` and returns the filename for `main`.
    pub fn database_path(&self) -> Result<PathBuf> {
        let path: String = self
            .conn
            .query_row("PRAGMA database_list", [], |row| row.get(2))
            .with_context(|| "reading sqlite database path from PRAGMA database_list")?;
        if path.is_empty() {
            anyhow::bail!("sqlite main database path is empty (likely in-memory)");
        }
        Ok(PathBuf::from(path))
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
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        agent_slug: &str,
        source_id: &str,
        day_id: i64,
        model_family: &str,
        role: &str,
        usage: &crate::connectors::ExtractedTokenUsage,
        content_chars: i64,
        estimated_cost_usd: f64,
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
        delta.estimated_cost_usd += estimated_cost_usd;
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

// -------------------------------------------------------------------------
// AnalyticsRollupAggregator — Batched usage_hourly + usage_daily Updates
// -------------------------------------------------------------------------
// Accumulates per-message deltas in memory, then flushes to both
// usage_hourly and usage_daily in a single batched operation.

/// Delta for a single (bucket, agent_slug, workspace_id, source_id) rollup key.
#[derive(Clone, Debug, Default)]
pub struct UsageRollupDelta {
    pub message_count: i64,
    pub user_message_count: i64,
    pub assistant_message_count: i64,
    pub tool_call_count: i64,
    pub plan_message_count: i64,
    pub plan_content_tokens_est_total: i64,
    pub plan_api_tokens_total: i64,
    pub api_coverage_message_count: i64,
    pub content_tokens_est_total: i64,
    pub content_tokens_est_user: i64,
    pub content_tokens_est_assistant: i64,
    pub api_tokens_total: i64,
    pub api_input_tokens_total: i64,
    pub api_output_tokens_total: i64,
    pub api_cache_read_tokens_total: i64,
    pub api_cache_creation_tokens_total: i64,
    pub api_thinking_tokens_total: i64,
}

/// Pending message_metrics row for batch insertion.
#[derive(Debug, Clone)]
pub struct MessageMetricsEntry {
    pub message_id: i64,
    pub created_at_ms: i64,
    pub hour_id: i64,
    pub day_id: i64,
    pub agent_slug: String,
    pub workspace_id: i64,
    pub source_id: String,
    pub role: String,
    pub content_chars: i64,
    pub content_tokens_est: i64,
    pub model_name: Option<String>,
    pub model_family: String,
    pub model_tier: String,
    pub provider: String,
    pub api_input_tokens: Option<i64>,
    pub api_output_tokens: Option<i64>,
    pub api_cache_read_tokens: Option<i64>,
    pub api_cache_creation_tokens: Option<i64>,
    pub api_thinking_tokens: Option<i64>,
    pub api_service_tier: Option<String>,
    pub api_data_source: String,
    pub tool_call_count: i64,
    pub has_tool_calls: bool,
    pub has_plan: bool,
}

/// In-memory aggregator for batched usage_hourly and usage_daily rollup updates.
///
/// Keyed by (bucket_id, agent_slug, workspace_id, source_id).
/// Maintains separate hourly and daily delta maps.
#[derive(Debug, Default)]
pub struct AnalyticsRollupAggregator {
    hourly: HashMap<(i64, String, i64, String), UsageRollupDelta>,
    daily: HashMap<(i64, String, i64, String), UsageRollupDelta>,
    models_daily: HashMap<(i64, String, i64, String, String, String), UsageRollupDelta>,
}

impl AnalyticsRollupAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a single message's contribution to both hourly and daily rollups.
    pub fn record(&mut self, entry: &MessageMetricsEntry) {
        let content_est = entry.content_tokens_est;
        let api_total = entry.api_input_tokens.unwrap_or(0)
            + entry.api_output_tokens.unwrap_or(0)
            + entry.api_cache_read_tokens.unwrap_or(0)
            + entry.api_cache_creation_tokens.unwrap_or(0)
            + entry.api_thinking_tokens.unwrap_or(0);
        let is_api = entry.api_data_source == "api";
        let is_user = entry.role == "user";
        let is_assistant = entry.role == "assistant" || entry.role == "agent";

        // Apply to both hourly and daily
        for (map, bucket_id) in [
            (&mut self.hourly, entry.hour_id),
            (&mut self.daily, entry.day_id),
        ] {
            let key = (
                bucket_id,
                entry.agent_slug.clone(),
                entry.workspace_id,
                entry.source_id.clone(),
            );
            let d = map.entry(key).or_default();
            d.message_count += 1;
            if is_user {
                d.user_message_count += 1;
                d.content_tokens_est_user += content_est;
            }
            if is_assistant {
                d.assistant_message_count += 1;
                d.content_tokens_est_assistant += content_est;
            }
            d.tool_call_count += entry.tool_call_count;
            if entry.has_plan {
                d.plan_message_count += 1;
                d.plan_content_tokens_est_total += content_est;
                if is_api {
                    d.plan_api_tokens_total += api_total;
                }
            }
            if is_api {
                d.api_coverage_message_count += 1;
            }
            d.content_tokens_est_total += content_est;
            d.api_tokens_total += api_total;
            d.api_input_tokens_total += entry.api_input_tokens.unwrap_or(0);
            d.api_output_tokens_total += entry.api_output_tokens.unwrap_or(0);
            d.api_cache_read_tokens_total += entry.api_cache_read_tokens.unwrap_or(0);
            d.api_cache_creation_tokens_total += entry.api_cache_creation_tokens.unwrap_or(0);
            d.api_thinking_tokens_total += entry.api_thinking_tokens.unwrap_or(0);
        }

        let model_key = (
            entry.day_id,
            entry.agent_slug.clone(),
            entry.workspace_id,
            entry.source_id.clone(),
            entry.model_family.clone(),
            entry.model_tier.clone(),
        );
        let d = self.models_daily.entry(model_key).or_default();
        d.message_count += 1;
        if is_user {
            d.user_message_count += 1;
            d.content_tokens_est_user += content_est;
        }
        if is_assistant {
            d.assistant_message_count += 1;
            d.content_tokens_est_assistant += content_est;
        }
        d.tool_call_count += entry.tool_call_count;
        if entry.has_plan {
            d.plan_message_count += 1;
            d.plan_content_tokens_est_total += content_est;
            if is_api {
                d.plan_api_tokens_total += api_total;
            }
        }
        if is_api {
            d.api_coverage_message_count += 1;
        }
        d.content_tokens_est_total += content_est;
        d.api_tokens_total += api_total;
        d.api_input_tokens_total += entry.api_input_tokens.unwrap_or(0);
        d.api_output_tokens_total += entry.api_output_tokens.unwrap_or(0);
        d.api_cache_read_tokens_total += entry.api_cache_read_tokens.unwrap_or(0);
        d.api_cache_creation_tokens_total += entry.api_cache_creation_tokens.unwrap_or(0);
        d.api_thinking_tokens_total += entry.api_thinking_tokens.unwrap_or(0);
    }

    pub fn is_empty(&self) -> bool {
        self.hourly.is_empty() && self.daily.is_empty() && self.models_daily.is_empty()
    }

    pub fn hourly_entry_count(&self) -> usize {
        self.hourly.len()
    }

    pub fn daily_entry_count(&self) -> usize {
        self.daily.len()
    }

    pub fn models_daily_entry_count(&self) -> usize {
        self.models_daily.len()
    }
}

/// Whether the current role should be considered for plan attribution.
///
/// Plan attribution v2 defaults to assistant/agent messages only.
fn has_plan_for_role(role: &str, content: &str) -> bool {
    let role = role.trim();
    (role.eq_ignore_ascii_case("assistant") || role.eq_ignore_ascii_case("agent"))
        && has_plan_heuristic(content)
}

/// Heuristic to detect "plan" messages.
///
/// v2 behavior:
/// - Require an explicit plan marker near the top of the message.
/// - Require structured steps (numbered or bullets) to reduce false positives.
/// - Avoid classifying tool-output blobs as plans.
fn has_plan_heuristic(content: &str) -> bool {
    if content.len() < 24 {
        return false;
    }

    let lower = content.to_lowercase();

    // Ignore tool-output-like blobs unless they also have a strong plan header.
    let looks_like_tool_blob = lower.contains("```")
        || lower.contains("\"tool\"")
        || lower.contains("stdout:")
        || lower.contains("stderr:")
        || lower.contains("exit code:");

    let mut lines: Vec<&str> = Vec::with_capacity(60);
    let mut in_fenced_code = false;
    for raw in lower.lines() {
        let line = raw.trim();
        if line.starts_with("```") {
            in_fenced_code = !in_fenced_code;
            continue;
        }
        if in_fenced_code || line.is_empty() {
            continue;
        }
        lines.push(line);
        if lines.len() >= 60 {
            break;
        }
    }

    let header_pos = lines.iter().position(|line| {
        line.starts_with("## plan")
            || line.starts_with("# plan")
            || line.starts_with("plan:")
            || line.starts_with("implementation plan")
            || line.starts_with("next steps:")
            || line.starts_with("action plan:")
    });
    let preview_top = lines.iter().take(8).copied().collect::<Vec<_>>().join("\n");
    let header_near_top = header_pos.is_some_and(|idx| idx <= 6) || preview_top.contains("plan:");

    if !header_near_top {
        return false;
    }
    if looks_like_tool_blob && header_pos.is_none() {
        return false;
    }

    let numbered_steps = lines
        .iter()
        .filter(|line| is_numbered_step_line(line))
        .count();
    let bullet_steps = lines
        .iter()
        .filter(|line| {
            line.starts_with("- ")
                || line.starts_with("* ")
                || line.starts_with("+ ")
                || line.starts_with("- [ ] ")
                || line.starts_with("- [x] ")
        })
        .count();

    numbered_steps >= 2 || (numbered_steps >= 1 && bullet_steps >= 1) || bullet_steps >= 3
}

fn is_numbered_step_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let digit_count = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    if digit_count == 0 || digit_count > 3 {
        return false;
    }
    let rest = &trimmed[digit_count..];
    rest.starts_with(". ") || rest.starts_with(") ")
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

// -------------------------------------------------------------------------
// PricingTable — In-memory cache for model_pricing lookups (bead z9fse.10)
// -------------------------------------------------------------------------

/// One pricing row loaded from the `model_pricing` table.
#[derive(Debug, Clone)]
pub struct PricingEntry {
    pub model_pattern: String,
    pub provider: String,
    pub input_cost_per_mtok: f64,
    pub output_cost_per_mtok: f64,
    pub cache_read_cost_per_mtok: Option<f64>,
    pub cache_creation_cost_per_mtok: Option<f64>,
    /// Effective date as day_id (days since 2020-01-01).
    pub effective_day_id: i64,
}

/// Diagnostics for pricing coverage during a batch operation.
#[derive(Debug, Clone, Default)]
pub struct PricingDiagnostics {
    pub priced_count: u64,
    pub unpriced_count: u64,
    /// Top unknown model names → count.
    pub unknown_models: HashMap<String, u64>,
}

impl PricingDiagnostics {
    fn record_priced(&mut self) {
        self.priced_count += 1;
    }

    fn record_unpriced(&mut self, model_name: Option<&str>) {
        self.unpriced_count += 1;
        let key = model_name.unwrap_or("(none)").to_string();
        *self.unknown_models.entry(key).or_insert(0) += 1;
    }

    /// Log a summary of pricing coverage.
    pub fn log_summary(&self) {
        let total = self.priced_count + self.unpriced_count;
        if total == 0 {
            return;
        }
        let pct = (self.priced_count as f64 / total as f64) * 100.0;
        tracing::info!(
            target: "cass::analytics::pricing",
            priced = self.priced_count,
            unpriced = self.unpriced_count,
            total = total,
            coverage_pct = format!("{pct:.1}%"),
            "pricing coverage"
        );
        if !self.unknown_models.is_empty() {
            let mut sorted: Vec<_> = self.unknown_models.iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(a.1));
            for (model, count) in sorted.iter().take(5) {
                tracing::debug!(
                    target: "cass::analytics::pricing",
                    model = model.as_str(),
                    count = count,
                    "unknown model (no pricing)"
                );
            }
        }
    }
}

/// In-memory pricing table loaded from `model_pricing` for fast lookups.
#[derive(Debug, Clone)]
pub struct PricingTable {
    entries: Vec<PricingEntry>,
}

impl PricingTable {
    /// Load all pricing entries from the database.
    pub fn load(conn: &rusqlite::Connection) -> Result<Self> {
        let mut stmt = conn.prepare(
            "SELECT model_pattern, provider, input_cost_per_mtok, output_cost_per_mtok,
                    cache_read_cost_per_mtok, cache_creation_cost_per_mtok, effective_date
             FROM model_pricing
             ORDER BY effective_date DESC",
        )?;
        let entries = stmt
            .query_map([], |row| {
                let effective_date: String = row.get(6)?;
                let effective_day_id = date_str_to_day_id(&effective_date).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            e.to_string(),
                        )),
                    )
                })?;
                Ok(PricingEntry {
                    model_pattern: row.get(0)?,
                    provider: row.get(1)?,
                    input_cost_per_mtok: row.get(2)?,
                    output_cost_per_mtok: row.get(3)?,
                    cache_read_cost_per_mtok: row.get(4)?,
                    cache_creation_cost_per_mtok: row.get(5)?,
                    effective_day_id,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Self { entries })
    }

    /// Load all pricing entries from a frankensqlite connection.
    pub fn franken_load(conn: &FrankenConnection) -> Result<Self> {
        let rows = conn.query(
            "SELECT model_pattern, provider, input_cost_per_mtok, output_cost_per_mtok,
                    cache_read_cost_per_mtok, cache_creation_cost_per_mtok, effective_date
             FROM model_pricing
             ORDER BY effective_date DESC",
        )?;
        let mut entries = Vec::with_capacity(rows.len());
        for row in &rows {
            let effective_date: String = row.get_typed(6)?;
            let effective_day_id = date_str_to_day_id(&effective_date)?;
            entries.push(PricingEntry {
                model_pattern: row.get_typed(0)?,
                provider: row.get_typed(1)?,
                input_cost_per_mtok: row.get_typed(2)?,
                output_cost_per_mtok: row.get_typed(3)?,
                cache_read_cost_per_mtok: row.get_typed(4)?,
                cache_creation_cost_per_mtok: row.get_typed(5)?,
                effective_day_id,
            });
        }
        Ok(Self { entries })
    }

    /// Look up the best pricing entry for a given model name and date.
    ///
    /// Selection rules:
    /// 1. Pattern must match model_name (SQL LIKE semantics).
    /// 2. effective_day_id must be <= message_day_id.
    /// 3. Among matches, prefer the most recent effective_date.
    /// 4. Tie-break by pattern specificity (longest pattern wins).
    pub fn lookup(&self, model_name: &str, message_day_id: i64) -> Option<&PricingEntry> {
        let mut best: Option<&PricingEntry> = None;

        for entry in &self.entries {
            if entry.effective_day_id > message_day_id {
                continue;
            }
            if !sql_like_match(model_name, &entry.model_pattern) {
                continue;
            }

            match best {
                None => best = Some(entry),
                Some(current) => {
                    if entry.effective_day_id > current.effective_day_id
                        || (entry.effective_day_id == current.effective_day_id
                            && entry.model_pattern.len() > current.model_pattern.len())
                    {
                        best = Some(entry);
                    }
                }
            }
        }

        best
    }

    /// Compute estimated cost in USD for a set of token counts.
    ///
    /// Returns `None` if no pricing entry matches or if no token counts are available.
    pub fn compute_cost(
        &self,
        model_name: Option<&str>,
        message_day_id: i64,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
        cache_read_tokens: Option<i64>,
        cache_creation_tokens: Option<i64>,
    ) -> Option<f64> {
        let model = model_name?;
        let pricing = self.lookup(model, message_day_id)?;

        if input_tokens.is_none() && output_tokens.is_none() {
            return None;
        }

        let mut cost = 0.0;
        let cache_read = cache_read_tokens.unwrap_or(0);
        let cache_creation = cache_creation_tokens.unwrap_or(0);
        // input_tokens includes cache tokens as a subset; subtract them
        // so we don't charge at both the full input rate AND the cache rate.
        let non_cache_input = input_tokens
            .unwrap_or(0)
            .saturating_sub(cache_read)
            .saturating_sub(cache_creation)
            .max(0);
        cost += non_cache_input as f64 * pricing.input_cost_per_mtok / 1_000_000.0;
        cost += output_tokens.unwrap_or(0) as f64 * pricing.output_cost_per_mtok / 1_000_000.0;

        if let Some(cache_price) = pricing.cache_read_cost_per_mtok {
            cost += cache_read as f64 * cache_price / 1_000_000.0;
        }
        if let Some(cache_price) = pricing.cache_creation_cost_per_mtok {
            cost += cache_creation as f64 * cache_price / 1_000_000.0;
        }

        Some(cost)
    }

    /// Whether the pricing table has any entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Convert "YYYY-MM-DD" date string to day_id (days since 2020-01-01),
/// matching the format produced by `day_id_from_millis`.
fn date_str_to_day_id(s: &str) -> Result<i64> {
    use chrono::NaiveDate;
    const EPOCH_2020: NaiveDate = match NaiveDate::from_ymd_opt(2020, 1, 1) {
        Some(d) => d,
        None => unreachable!(),
    };
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map(|d| (d - EPOCH_2020).num_days())
        .with_context(|| format!("invalid effective_date '{s}'"))
}

/// SQL LIKE pattern matcher (case-insensitive). `%` = any sequence, `_` = any single char.
fn sql_like_match(value: &str, pattern: &str) -> bool {
    sql_like_match_bytes(
        value.to_ascii_lowercase().as_bytes(),
        pattern.to_ascii_lowercase().as_bytes(),
    )
}

/// Determine the byte length of the UTF-8 character starting at `b`.
fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    }
}

fn sql_like_match_bytes(val: &[u8], pat: &[u8]) -> bool {
    if pat.is_empty() {
        return val.is_empty();
    }
    match pat[0] {
        b'%' => {
            let mut p = 1;
            while p < pat.len() && pat[p] == b'%' {
                p += 1;
            }
            let rest = &pat[p..];
            // Iterate only at UTF-8 char boundaries
            let mut i = 0;
            while i <= val.len() {
                if sql_like_match_bytes(&val[i..], rest) {
                    return true;
                }
                if i < val.len() {
                    i += utf8_char_len(val[i]);
                } else {
                    break;
                }
            }
            false
        }
        b'_' => {
            // Match one full UTF-8 character, not just one byte
            if val.is_empty() {
                return false;
            }
            let char_len = utf8_char_len(val[0]);
            val.len() >= char_len && sql_like_match_bytes(&val[char_len..], &pat[1..])
        }
        c => !val.is_empty() && val[0] == c && sql_like_match_bytes(&val[1..], &pat[1..]),
    }
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

        // Load pricing table once for the entire batch (bead z9fse.10)
        let pricing_table = PricingTable::load(&self.conn).unwrap_or_else(|e| {
            tracing::warn!(target: "cass::analytics::pricing", error = %e, "failed to load pricing table");
            PricingTable { entries: Vec::new() }
        });
        let mut pricing_diag = PricingDiagnostics::default();

        let tx = self.conn.transaction()?;
        let mut outcomes = Vec::with_capacity(conversations.len());
        let mut fts_entries = Vec::new();
        let mut stats = StatsAggregator::new();
        let mut token_stats = TokenStatsAggregator::new();
        let mut token_entries: Vec<TokenUsageEntry> = Vec::new();
        let mut metrics_entries: Vec<MessageMetricsEntry> = Vec::new();
        let mut rollup_agg = AnalyticsRollupAggregator::new();
        let mut conv_ids_to_summarize: Vec<i64> = Vec::new();

        // Process all conversations, collecting FTS entries and token data
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

            // Extract token usage from newly inserted messages
            let has_new_messages = !outcome.inserted_indices.is_empty();
            if has_new_messages {
                let conv_day_id = conv
                    .started_at
                    .map(SqliteStorage::day_id_from_millis)
                    .unwrap_or(0);

                // Track primary model for session-level stats
                let mut session_model_family = String::from("unknown");
                let mut has_any_tokens = false;

                // For each newly inserted message, extract tokens and create entries
                // We need the message_id from the DB. Query inserted messages by conv+idx.
                for msg in &conv.messages {
                    if !outcome.inserted_indices.contains(&msg.idx) {
                        continue;
                    }

                    let role_s = role_str(&msg.role);
                    let usage = crate::connectors::extract_tokens_for_agent(
                        &conv.agent_slug,
                        &msg.extra_json,
                        &msg.content,
                        &role_s,
                    );

                    // Look up message_id from DB
                    let msg_id: Option<i64> = tx
                        .query_row(
                            "SELECT id FROM messages WHERE conversation_id = ? AND idx = ?",
                            params![outcome.conversation_id, msg.idx],
                            |row| row.get(0),
                        )
                        .optional()?;

                    let Some(message_id) = msg_id else {
                        continue;
                    };

                    let msg_ts = msg.created_at.or(conv.started_at).unwrap_or(0);
                    let msg_day_id = if msg_ts > 0 {
                        SqliteStorage::day_id_from_millis(msg_ts)
                    } else {
                        conv_day_id
                    };

                    // Normalize model for aggregation
                    let model_info = usage
                        .model_name
                        .as_deref()
                        .map(crate::connectors::normalize_model);

                    let model_family = model_info
                        .as_ref()
                        .map(|i| i.family.clone())
                        .unwrap_or_else(|| "unknown".into());
                    let model_tier = model_info
                        .as_ref()
                        .map(|i| i.tier.clone())
                        .unwrap_or_else(|| "unknown".into());
                    let provider = usage
                        .provider
                        .clone()
                        .or_else(|| model_info.as_ref().map(|i| i.provider.clone()))
                        .unwrap_or_else(|| "unknown".into());

                    if model_family != "unknown" {
                        session_model_family = model_family.clone();
                    }

                    // Compute estimated cost from pricing table (bead z9fse.10)
                    let estimated_cost = pricing_table.compute_cost(
                        usage.model_name.as_deref(),
                        msg_day_id,
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cache_read_tokens,
                        usage.cache_creation_tokens,
                    );
                    if estimated_cost.is_some() {
                        pricing_diag.record_priced();
                    } else if usage.has_token_data() {
                        pricing_diag.record_unpriced(usage.model_name.as_deref());
                    }

                    // Feed into token stats aggregator
                    token_stats.record(
                        &conv.agent_slug,
                        &conv.source_id,
                        msg_day_id,
                        &model_family,
                        &role_s,
                        &usage,
                        msg.content.len() as i64,
                        estimated_cost.unwrap_or(0.0),
                    );

                    if usage.has_token_data() {
                        has_any_tokens = true;
                    }

                    let content_chars = msg.content.len() as i64;
                    let content_tokens_est = content_chars / 4;
                    let msg_hour_id = SqliteStorage::hour_id_from_millis(msg_ts);
                    let has_plan = has_plan_for_role(&role_s, &msg.content);

                    // Build token_usage row
                    token_entries.push(TokenUsageEntry {
                        message_id,
                        conversation_id: outcome.conversation_id,
                        agent_id,
                        workspace_id,
                        source_id: conv.source_id.clone(),
                        timestamp_ms: msg_ts,
                        day_id: msg_day_id,
                        model_name: usage.model_name.clone(),
                        model_family: Some(model_family.clone()),
                        model_tier: Some(model_tier.clone()),
                        service_tier: usage.service_tier.clone(),
                        provider: Some(provider.clone()),
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        cache_read_tokens: usage.cache_read_tokens,
                        cache_creation_tokens: usage.cache_creation_tokens,
                        thinking_tokens: usage.thinking_tokens,
                        total_tokens: usage.total_tokens(),
                        estimated_cost_usd: estimated_cost,
                        role: role_s.clone(),
                        content_chars,
                        has_tool_calls: usage.has_tool_calls,
                        tool_call_count: usage.tool_call_count,
                        data_source: usage.data_source.as_str().to_string(),
                    });

                    // Build message_metrics row and feed rollup aggregator
                    let mm = MessageMetricsEntry {
                        message_id,
                        created_at_ms: msg_ts,
                        hour_id: msg_hour_id,
                        day_id: msg_day_id,
                        agent_slug: conv.agent_slug.clone(),
                        workspace_id: workspace_id.unwrap_or(0),
                        source_id: conv.source_id.clone(),
                        role: role_s,
                        content_chars,
                        content_tokens_est,
                        model_name: usage.model_name.clone(),
                        model_family: model_family.clone(),
                        model_tier: model_tier.clone(),
                        provider,
                        api_input_tokens: usage.input_tokens,
                        api_output_tokens: usage.output_tokens,
                        api_cache_read_tokens: usage.cache_read_tokens,
                        api_cache_creation_tokens: usage.cache_creation_tokens,
                        api_thinking_tokens: usage.thinking_tokens,
                        api_service_tier: usage.service_tier.clone(),
                        api_data_source: usage.data_source.as_str().to_string(),
                        tool_call_count: usage.tool_call_count as i64,
                        has_tool_calls: usage.has_tool_calls,
                        has_plan,
                    };
                    rollup_agg.record(&mm);
                    metrics_entries.push(mm);
                }

                // Record session count in token stats (once per new conversation)
                if delta.session_count_delta > 0 {
                    token_stats.record_session(
                        &conv.agent_slug,
                        &conv.source_id,
                        conv_day_id,
                        &session_model_family,
                    );
                }

                // Mark conversation for summary update if it has any token data
                if has_any_tokens {
                    conv_ids_to_summarize.push(outcome.conversation_id);
                }
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

        // Batch insert token_usage rows
        if !token_entries.is_empty() {
            let token_count = token_entries.len();
            let inserted = insert_token_usage_batched_in_tx(&tx, &token_entries)?;
            tracing::debug!(
                target: "cass::perf::token_usage",
                total = token_count,
                inserted = inserted,
                "batch_token_usage_insert_complete"
            );
        }

        // Batched token_daily_stats update
        if !token_stats.is_empty() {
            let entries = token_stats.expand();
            let affected = update_token_daily_stats_batched_in_tx(&tx, &entries)?;
            tracing::debug!(
                target: "cass::perf::token_daily_stats",
                raw = token_stats.raw_entry_count(),
                expanded = entries.len(),
                affected = affected,
                "batched_token_stats_update_complete"
            );
        }

        // Batch insert message_metrics rows
        if !metrics_entries.is_empty() {
            let mm_count = metrics_entries.len();
            let inserted = insert_message_metrics_batched_in_tx(&tx, &metrics_entries)?;
            tracing::debug!(
                target: "cass::perf::message_metrics",
                total = mm_count,
                inserted = inserted,
                "batch_message_metrics_insert_complete"
            );
        }

        // Flush usage_hourly + usage_daily rollups
        if !rollup_agg.is_empty() {
            let (hourly, daily, models_daily) = flush_analytics_rollups_in_tx(&tx, &rollup_agg)?;
            tracing::debug!(
                target: "cass::perf::usage_rollups",
                hourly_buckets = rollup_agg.hourly_entry_count(),
                daily_buckets = rollup_agg.daily_entry_count(),
                models_daily_buckets = rollup_agg.models_daily_entry_count(),
                hourly_affected = hourly,
                daily_affected = daily,
                models_daily_affected = models_daily,
                "batched_usage_rollups_complete"
            );
        }

        // Update conversation-level token summaries
        for conv_id in &conv_ids_to_summarize {
            update_conversation_token_summaries_in_tx(&tx, *conv_id)?;
        }

        tx.commit()?;

        // Log pricing coverage diagnostics (bead z9fse.10)
        pricing_diag.log_summary();

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

    /// Fetch messages for embedding that were inserted after `since_id`.
    /// Used for incremental semantic indexing in watch mode.
    pub fn fetch_messages_for_embedding_since(
        &self,
        since_id: i64,
    ) -> Result<Vec<MessageForEmbedding>> {
        let mut stmt = self.conn.prepare(
            r"SELECT m.id, m.created_at, c.agent_id, c.workspace_id, c.source_id, m.role, m.content
              FROM messages m
              JOIN conversations c ON m.conversation_id = c.id
              WHERE m.id > ?1
              ORDER BY m.id",
        )?;

        let rows = stmt.query_map(params![since_id], |row| {
            let source_id_str: String = row
                .get::<_, Option<String>>(4)?
                .unwrap_or_else(|| "local".to_string());
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

    /// Get the watermark for incremental semantic embedding.
    /// Returns the highest message id that has been embedded, or None if no
    /// embedding pass has been recorded yet.
    pub fn get_last_embedded_message_id(&self) -> Result<Option<i64>> {
        let result: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_embedded_message_id'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(result.and_then(|s| s.parse().ok()))
    }

    /// Set the watermark for incremental semantic embedding.
    pub fn set_last_embedded_message_id(&mut self, id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES('last_embedded_message_id', ?)",
            params![id.to_string()],
        )?;
        Ok(())
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
        // Chunked insert to avoid OOM on large databases (#110)
        let total_count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages m JOIN conversations c ON m.conversation_id = c.id JOIN agents a ON c.agent_id = a.id LEFT JOIN workspaces w ON c.workspace_id = w.id",
                [],
                |row| row.get(0),
            )?;
        let batch_size: i64 = 10_000;
        let mut offset: i64 = 0;

        let tx = self.conn.transaction()?;
        tx.execute_batch(FTS5_DELETE_ALL_SQL)?;
        while offset < total_count {
            info!(
                "Rebuilding FTS: {}/{} rows...",
                offset.min(total_count),
                total_count
            );
            tx.execute(
                "INSERT INTO fts_messages(rowid, content, title, agent, workspace, source_path, created_at)
                 SELECT m.id, m.content, c.title, a.slug, w.path, c.source_path, m.created_at
                 FROM messages m
                 JOIN conversations c ON m.conversation_id = c.id
                 JOIN agents a ON c.agent_id = a.id
                 LEFT JOIN workspaces w ON c.workspace_id = w.id
                 ORDER BY m.rowid
                 LIMIT ?1 OFFSET ?2",
                params![batch_size, offset],
            )?;
            offset += batch_size;
        }
        tx.commit()?;
        info!(
            "Rebuilding FTS: {}/{} rows complete.",
            total_count, total_count
        );
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
        let secs = timestamp_ms.div_euclid(1000);
        (secs - Self::EPOCH_2020_SECS).div_euclid(86400)
    }

    /// Convert a millisecond timestamp to an hour_id (hours since 2020-01-01 00:00 UTC).
    pub fn hour_id_from_millis(timestamp_ms: i64) -> i64 {
        let secs = timestamp_ms.div_euclid(1000);
        (secs - Self::EPOCH_2020_SECS).div_euclid(3600)
    }

    /// Convert a day_id back to a timestamp (milliseconds, start of day UTC).
    pub fn millis_from_day_id(day_id: i64) -> i64 {
        (Self::EPOCH_2020_SECS + day_id * 86400) * 1000
    }

    /// Convert an hour_id back to a timestamp (milliseconds, start of hour UTC).
    pub fn millis_from_hour_id(hour_id: i64) -> i64 {
        (Self::EPOCH_2020_SECS + hour_id * 3600) * 1000
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
        let mut params_vec: Vec<rusqlite::types::Value> = Vec::new();

        if let Some(start) = start_ts_ms {
            sql.push_str(" AND c.started_at >= ?");
            params_vec.push(start.into());
        }
        if let Some(end) = end_ts_ms {
            sql.push_str(" AND c.started_at <= ?");
            params_vec.push(end.into());
        }
        if let Some(agent) = agent_slug
            && agent != "all"
        {
            sql.push_str(" AND a.slug = ?");
            params_vec.push(agent.to_string().into());
        }
        if let Some(source) = source_id
            && source != "all"
        {
            sql.push_str(" AND c.source_id = ?");
            params_vec.push(source.to_string().into());
        }

        let count: i64 =
            self.conn
                .query_row(&sql, rusqlite::params_from_iter(params_vec), |r| r.get(0))?;
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

    // -------------------------------------------------------------------------
    // Analytics Rebuild / Backfill (bead z9fse.4)
    // -------------------------------------------------------------------------

    /// Rebuild analytics tables (message_metrics + rollups) from existing
    /// messages in the database. Does NOT re-parse raw agent session files.
    ///
    /// Algorithm:
    /// 1. Clear message_metrics, usage_hourly, usage_daily, usage_models_daily in a transaction
    /// 2. Stream messages joined with conversation/agent dims in chunks
    /// 3. For each message, call extract_tokens_for_agent and build MessageMetricsEntry
    /// 4. Batch insert message_metrics rows
    /// 5. Populate rollups via SQL aggregation from message_metrics
    pub fn rebuild_analytics(&mut self) -> Result<AnalyticsRebuildResult> {
        let start = Instant::now();

        // Count total messages for progress reporting
        let total_messages: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM messages", [], |row| {
                    row.get::<_, i64>(0)
                })?;
        tracing::info!(
            target: "cass::analytics",
            total_messages,
            "analytics_rebuild_start"
        );

        let tx = self.conn.transaction()?;

        // Step 1: Clear analytics tables
        tx.execute("DELETE FROM message_metrics")?;
        tx.execute("DELETE FROM usage_hourly")?;
        tx.execute("DELETE FROM usage_daily")?;
        tx.execute("DELETE FROM usage_models_daily")?;

        // Step 2: Stream messages in chunks, extract metrics, batch insert
        const CHUNK_SIZE: i64 = 10_000;
        let mut offset: i64 = 0;
        let mut total_inserted: usize = 0;

        loop {
            // Fetch a chunk of messages with their conversation/agent dims
            let mut stmt = tx.prepare(
                "SELECT m.id, m.idx, m.role, m.content, m.extra_json, m.extra_bin,
                        m.created_at,
                        c.id AS conv_id, c.started_at AS conv_started_at,
                        c.source_id, c.workspace_id,
                        a.slug AS agent_slug
                 FROM messages m
                 JOIN conversations c ON m.conversation_id = c.id
                 JOIN agents a ON c.agent_id = a.id
                 ORDER BY m.id
                 LIMIT ? OFFSET ?",
            )?;

            #[allow(clippy::type_complexity)]
            let rows: Vec<(
                i64,
                String,
                String,
                Option<serde_json::Value>,
                Option<i64>,
                Option<i64>,
                String,
                Option<i64>,
                String,
            )> = stmt
                .query_map(params![CHUNK_SIZE, offset], |row| {
                    let msg_id: i64 = row.get(0)?;
                    let role: String = row.get(2)?;
                    let content: String = row.get(3)?;
                    // Try extra_json first, fall back to deserializing extra_bin
                    let extra_json: Option<serde_json::Value> = row
                        .get::<_, Option<String>>(4)?
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .or_else(|| {
                            row.get::<_, Option<Vec<u8>>>(5)
                                .ok()
                                .flatten()
                                .and_then(|b| rmp_serde::from_slice(&b).ok())
                        });
                    let msg_ts: Option<i64> = row.get(6)?;
                    let conv_started_at: Option<i64> = row.get(8)?;
                    let source_id: String = row.get(9)?;
                    let workspace_id: Option<i64> = row.get(10)?;
                    let agent_slug: String = row.get(11)?;

                    let effective_ts = msg_ts.or(conv_started_at).unwrap_or(0);

                    Ok((
                        msg_id,
                        role,
                        content,
                        extra_json,
                        Some(effective_ts),
                        workspace_id,
                        source_id,
                        conv_started_at,
                        agent_slug,
                    ))
                })?
                .filter_map(|r| r.ok())
                .collect();

            if rows.is_empty() {
                break;
            }

            let chunk_len = rows.len();
            let mut entries = Vec::with_capacity(chunk_len);

            for (
                msg_id,
                role,
                content,
                extra_json,
                effective_ts,
                workspace_id,
                source_id,
                _conv_started_at,
                agent_slug,
            ) in &rows
            {
                let ts = effective_ts.unwrap_or(0);
                let day_id = Self::day_id_from_millis(ts);
                let hour_id = Self::hour_id_from_millis(ts);
                let content_chars = content.len() as i64;
                let content_tokens_est = content_chars / 4;

                let extra = extra_json
                    .as_ref()
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let usage =
                    crate::connectors::extract_tokens_for_agent(agent_slug, &extra, content, role);
                let model_info = usage
                    .model_name
                    .as_deref()
                    .map(crate::connectors::normalize_model);
                let model_family = model_info
                    .as_ref()
                    .map(|i| i.family.clone())
                    .unwrap_or_else(|| "unknown".into());
                let model_tier = model_info
                    .as_ref()
                    .map(|i| i.tier.clone())
                    .unwrap_or_else(|| "unknown".into());
                let provider = usage
                    .provider
                    .clone()
                    .or_else(|| model_info.as_ref().map(|i| i.provider.clone()))
                    .unwrap_or_else(|| "unknown".into());

                entries.push(MessageMetricsEntry {
                    message_id: *msg_id,
                    created_at_ms: ts,
                    hour_id,
                    day_id,
                    agent_slug: agent_slug.clone(),
                    workspace_id: workspace_id.unwrap_or(0),
                    source_id: source_id.clone(),
                    role: role.clone(),
                    content_chars,
                    content_tokens_est,
                    model_name: usage.model_name.clone(),
                    model_family,
                    model_tier,
                    provider,
                    api_input_tokens: usage.input_tokens,
                    api_output_tokens: usage.output_tokens,
                    api_cache_read_tokens: usage.cache_read_tokens,
                    api_cache_creation_tokens: usage.cache_creation_tokens,
                    api_thinking_tokens: usage.thinking_tokens,
                    api_service_tier: usage.service_tier,
                    api_data_source: usage.data_source.as_str().to_string(),
                    tool_call_count: usage.tool_call_count as i64,
                    has_tool_calls: usage.has_tool_calls,
                    has_plan: has_plan_for_role(role, content),
                });
            }

            let inserted = insert_message_metrics_batched_in_tx(&tx, &entries)?;
            total_inserted += inserted;
            offset += chunk_len as i64;

            tracing::debug!(
                target: "cass::analytics",
                offset,
                chunk = chunk_len,
                inserted,
                total = total_inserted,
                "analytics_rebuild_chunk"
            );

            if (chunk_len as i64) < CHUNK_SIZE {
                break;
            }
        }

        // Step 3: Populate rollups via SQL aggregation from message_metrics
        let now_ms = Self::now_millis();

        let hourly_rows = tx.execute(
            "INSERT INTO usage_hourly (
                    hour_id, agent_slug, workspace_id, source_id,
                    message_count, user_message_count, assistant_message_count,
                    tool_call_count, plan_message_count, plan_content_tokens_est_total,
                    plan_api_tokens_total, api_coverage_message_count,
                    content_tokens_est_total, content_tokens_est_user, content_tokens_est_assistant,
                    api_tokens_total, api_input_tokens_total, api_output_tokens_total,
                    api_cache_read_tokens_total, api_cache_creation_tokens_total,
                    api_thinking_tokens_total, last_updated
                )
                SELECT
                    hour_id, agent_slug, workspace_id, source_id,
                    COUNT(*),
                    SUM(CASE WHEN role = 'user' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN role IN ('assistant', 'agent') THEN 1 ELSE 0 END),
                    SUM(tool_call_count),
                    SUM(has_plan),
                    SUM(CASE WHEN has_plan = 1 THEN content_tokens_est ELSE 0 END),
                    SUM(
                        CASE
                            WHEN has_plan = 1 AND api_data_source = 'api'
                                THEN COALESCE(api_input_tokens, 0)
                                    + COALESCE(api_output_tokens, 0)
                                    + COALESCE(api_cache_read_tokens, 0)
                                    + COALESCE(api_cache_creation_tokens, 0)
                                    + COALESCE(api_thinking_tokens, 0)
                            ELSE 0
                        END
                    ),
                    SUM(CASE WHEN api_data_source = 'api' THEN 1 ELSE 0 END),
                    SUM(content_tokens_est),
                    SUM(CASE WHEN role = 'user' THEN content_tokens_est ELSE 0 END),
                    SUM(CASE WHEN role IN ('assistant', 'agent') THEN content_tokens_est ELSE 0 END),
                    SUM(COALESCE(api_input_tokens, 0) + COALESCE(api_output_tokens, 0) + COALESCE(api_cache_read_tokens, 0) + COALESCE(api_cache_creation_tokens, 0) + COALESCE(api_thinking_tokens, 0)),
                    SUM(COALESCE(api_input_tokens, 0)),
                    SUM(COALESCE(api_output_tokens, 0)),
                    SUM(COALESCE(api_cache_read_tokens, 0)),
                    SUM(COALESCE(api_cache_creation_tokens, 0)),
                    SUM(COALESCE(api_thinking_tokens, 0)),
                    ?1
                FROM message_metrics
                GROUP BY hour_id, agent_slug, workspace_id, source_id",
            params![now_ms],
        )?;

        let daily_rows = tx.execute(
            "INSERT INTO usage_daily (
                    day_id, agent_slug, workspace_id, source_id,
                    message_count, user_message_count, assistant_message_count,
                    tool_call_count, plan_message_count, plan_content_tokens_est_total,
                    plan_api_tokens_total, api_coverage_message_count,
                    content_tokens_est_total, content_tokens_est_user, content_tokens_est_assistant,
                    api_tokens_total, api_input_tokens_total, api_output_tokens_total,
                    api_cache_read_tokens_total, api_cache_creation_tokens_total,
                    api_thinking_tokens_total, last_updated
                )
                SELECT
                    day_id, agent_slug, workspace_id, source_id,
                    COUNT(*),
                    SUM(CASE WHEN role = 'user' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN role IN ('assistant', 'agent') THEN 1 ELSE 0 END),
                    SUM(tool_call_count),
                    SUM(has_plan),
                    SUM(CASE WHEN has_plan = 1 THEN content_tokens_est ELSE 0 END),
                    SUM(
                        CASE
                            WHEN has_plan = 1 AND api_data_source = 'api'
                                THEN COALESCE(api_input_tokens, 0)
                                    + COALESCE(api_output_tokens, 0)
                                    + COALESCE(api_cache_read_tokens, 0)
                                    + COALESCE(api_cache_creation_tokens, 0)
                                    + COALESCE(api_thinking_tokens, 0)
                            ELSE 0
                        END
                    ),
                    SUM(CASE WHEN api_data_source = 'api' THEN 1 ELSE 0 END),
                    SUM(content_tokens_est),
                    SUM(CASE WHEN role = 'user' THEN content_tokens_est ELSE 0 END),
                    SUM(CASE WHEN role IN ('assistant', 'agent') THEN content_tokens_est ELSE 0 END),
                    SUM(COALESCE(api_input_tokens, 0) + COALESCE(api_output_tokens, 0) + COALESCE(api_cache_read_tokens, 0) + COALESCE(api_cache_creation_tokens, 0) + COALESCE(api_thinking_tokens, 0)),
                    SUM(COALESCE(api_input_tokens, 0)),
                    SUM(COALESCE(api_output_tokens, 0)),
                    SUM(COALESCE(api_cache_read_tokens, 0)),
                    SUM(COALESCE(api_cache_creation_tokens, 0)),
                    SUM(COALESCE(api_thinking_tokens, 0)),
                    ?1
                FROM message_metrics
                GROUP BY day_id, agent_slug, workspace_id, source_id",
            params![now_ms],
        )?;

        let models_daily_rows = tx.execute(
            "INSERT INTO usage_models_daily (
                    day_id, agent_slug, workspace_id, source_id, model_family, model_tier,
                    message_count, user_message_count, assistant_message_count,
                    tool_call_count, plan_message_count, api_coverage_message_count,
                    content_tokens_est_total, content_tokens_est_user, content_tokens_est_assistant,
                    api_tokens_total, api_input_tokens_total, api_output_tokens_total,
                    api_cache_read_tokens_total, api_cache_creation_tokens_total,
                    api_thinking_tokens_total, last_updated
                )
                SELECT
                    day_id,
                    agent_slug,
                    workspace_id,
                    source_id,
                    COALESCE(NULLIF(model_family, ''), 'unknown'),
                    COALESCE(NULLIF(model_tier, ''), 'unknown'),
                    COUNT(*),
                    SUM(CASE WHEN role = 'user' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN role IN ('assistant', 'agent') THEN 1 ELSE 0 END),
                    SUM(tool_call_count),
                    SUM(has_plan),
                    SUM(CASE WHEN api_data_source = 'api' THEN 1 ELSE 0 END),
                    SUM(content_tokens_est),
                    SUM(CASE WHEN role = 'user' THEN content_tokens_est ELSE 0 END),
                    SUM(CASE WHEN role IN ('assistant', 'agent') THEN content_tokens_est ELSE 0 END),
                    SUM(COALESCE(api_input_tokens, 0) + COALESCE(api_output_tokens, 0) + COALESCE(api_cache_read_tokens, 0) + COALESCE(api_cache_creation_tokens, 0) + COALESCE(api_thinking_tokens, 0)),
                    SUM(COALESCE(api_input_tokens, 0)),
                    SUM(COALESCE(api_output_tokens, 0)),
                    SUM(COALESCE(api_cache_read_tokens, 0)),
                    SUM(COALESCE(api_cache_creation_tokens, 0)),
                    SUM(COALESCE(api_thinking_tokens, 0)),
                    ?1
                FROM message_metrics
                GROUP BY
                    day_id,
                    agent_slug,
                    workspace_id,
                    source_id,
                    COALESCE(NULLIF(model_family, ''), 'unknown'),
                    COALESCE(NULLIF(model_tier, ''), 'unknown')",
            params![now_ms],
        )?;

        tx.commit()?;

        let elapsed = start.elapsed();
        let elapsed_ms = elapsed.as_millis() as u64;
        let msgs_per_sec = if elapsed_ms > 0 {
            (total_inserted as f64) / (elapsed_ms as f64 / 1000.0)
        } else {
            0.0
        };

        tracing::info!(
            target: "cass::analytics",
            message_metrics_rows = total_inserted,
            usage_hourly_rows = hourly_rows,
            usage_daily_rows = daily_rows,
            usage_models_daily_rows = models_daily_rows,
            elapsed_ms,
            messages_per_sec = format!("{:.0}", msgs_per_sec),
            "analytics_rebuild_complete"
        );

        Ok(AnalyticsRebuildResult {
            message_metrics_rows: total_inserted,
            usage_hourly_rows: hourly_rows,
            usage_daily_rows: daily_rows,
            usage_models_daily_rows: models_daily_rows,
            elapsed_ms,
            messages_per_sec: msgs_per_sec,
        })
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
        tx.execute("DELETE FROM daily_stats")?;

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

/// Result of an analytics rebuild operation.
#[derive(Debug, Clone)]
pub struct AnalyticsRebuildResult {
    pub message_metrics_rows: usize,
    pub usage_hourly_rows: usize,
    pub usage_daily_rows: usize,
    pub usage_models_daily_rows: usize,
    pub elapsed_ms: u64,
    pub messages_per_sec: f64,
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
        conn.execute("INSERT INTO meta(key, value) VALUES('schema_version', 0)", [])?;
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
            tx.execute_batch(MIGRATION_V11)?;
            tx.execute_batch(MIGRATION_V12)?;
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
            tx.execute_batch(MIGRATION_V11)?;
            tx.execute_batch(MIGRATION_V12)?;
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
            tx.execute_batch(MIGRATION_V11)?;
            tx.execute_batch(MIGRATION_V12)?;
        }
        3 => {
            tx.execute_batch(MIGRATION_V4)?;
            tx.execute_batch(MIGRATION_V5)?;
            tx.execute_batch(MIGRATION_V6)?;
            tx.execute_batch(MIGRATION_V7)?;
            tx.execute_batch(MIGRATION_V8)?;
            tx.execute_batch(MIGRATION_V9)?;
            tx.execute_batch(MIGRATION_V10)?;
            tx.execute_batch(MIGRATION_V11)?;
            tx.execute_batch(MIGRATION_V12)?;
        }
        4 => {
            tx.execute_batch(MIGRATION_V5)?;
            tx.execute_batch(MIGRATION_V6)?;
            tx.execute_batch(MIGRATION_V7)?;
            tx.execute_batch(MIGRATION_V8)?;
            tx.execute_batch(MIGRATION_V9)?;
            tx.execute_batch(MIGRATION_V10)?;
            tx.execute_batch(MIGRATION_V11)?;
            tx.execute_batch(MIGRATION_V12)?;
        }
        5 => {
            tx.execute_batch(MIGRATION_V6)?;
            tx.execute_batch(MIGRATION_V7)?;
            tx.execute_batch(MIGRATION_V8)?;
            tx.execute_batch(MIGRATION_V9)?;
            tx.execute_batch(MIGRATION_V10)?;
            tx.execute_batch(MIGRATION_V11)?;
            tx.execute_batch(MIGRATION_V12)?;
        }
        6 => {
            tx.execute_batch(MIGRATION_V7)?;
            tx.execute_batch(MIGRATION_V8)?;
            tx.execute_batch(MIGRATION_V9)?;
            tx.execute_batch(MIGRATION_V10)?;
            tx.execute_batch(MIGRATION_V11)?;
            tx.execute_batch(MIGRATION_V12)?;
        }
        7 => {
            tx.execute_batch(MIGRATION_V8)?;
            tx.execute_batch(MIGRATION_V9)?;
            tx.execute_batch(MIGRATION_V10)?;
            tx.execute_batch(MIGRATION_V11)?;
            tx.execute_batch(MIGRATION_V12)?;
        }
        8 => {
            tx.execute_batch(MIGRATION_V9)?;
            tx.execute_batch(MIGRATION_V10)?;
            tx.execute_batch(MIGRATION_V11)?;
            tx.execute_batch(MIGRATION_V12)?;
        }
        9 => {
            tx.execute_batch(MIGRATION_V10)?;
            tx.execute_batch(MIGRATION_V11)?;
            tx.execute_batch(MIGRATION_V12)?;
        }
        10 => {
            tx.execute_batch(MIGRATION_V11)?;
            tx.execute_batch(MIGRATION_V12)?;
        }
        11 => {
            tx.execute_batch(MIGRATION_V12)?;
        }
        12 => {}
        13 => {}
        14 => {}
        v => return Err(anyhow!("unsupported schema version {v}")),
    }

    if current < 13 {
        tx.execute_batch(MIGRATION_V13)?;
    }

    if current < 14 {
        tx.execute_batch(MIGRATION_V14)?;
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

/// Batch size for FTS5 inserts. With 7 columns per row (rowid + 6 cols) and
/// SQLite's SQLITE_MAX_VARIABLE_NUMBER default of 999, max batch is ~142 rows.
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
            "INSERT INTO fts_messages(rowid, content, title, agent, workspace, source_path, created_at) VALUES {}",
            placeholders
        );

        // Flatten parameters
        // Capacity: chunk.len() * 7
        let mut params_vec: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() * 7);
        for entry in chunk {
            params_vec.push(entry.message_id.into());
            params_vec.push(entry.content.clone().into());
            params_vec.push(entry.title.clone().into());
            params_vec.push(entry.agent.clone().into());
            params_vec.push(entry.workspace.clone().into());
            params_vec.push(entry.source_path.clone().into());
            params_vec.push(entry.created_at.into());
        }

        if let Err(e) = tx.execute(&sql, rusqlite::params_from_iter(params_vec)) {
            let batch_error = e.to_string();
            tracing::debug!(
                batch_size = chunk.len(),
                error = %batch_error,
                "fts_batch_insert_failed"
            );
            // Fall back to per-row inserts for compatibility, but keep the
            // overall write transactional: if any FTS row still fails, abort
            // so canonical rows and search index rows cannot diverge.
            for entry in chunk {
                tx.execute(
                    "INSERT INTO fts_messages(rowid, content, title, agent, workspace, source_path, created_at)
                     VALUES(?,?,?,?,?,?,?)",
                    params![
                        entry.message_id,
                        entry.content,
                        entry.title,
                        entry.agent,
                        entry.workspace,
                        entry.source_path,
                        entry.created_at
                    ],
                )
                .with_context(|| {
                    format!(
                        "inserting FTS row for message {} after batch failure ({batch_error})",
                        entry.message_id
                    )
                })?;
                inserted += 1;
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

        let mut params_vec: Vec<ParamValue> = Vec::with_capacity(chunk.len() * 7);
        for (day_id, agent, source, delta) in chunk {
            params_vec.push((*day_id).into());
            params_vec.push(agent.clone().into());
            params_vec.push(source.clone().into());
            params_vec.push(delta.session_count_delta.into());
            params_vec.push(delta.message_count_delta.into());
            params_vec.push(delta.total_chars_delta.into());
            params_vec.push(now.into());
        }

        total_affected += tx.execute_with_params(&sql, &param_slice_to_values(&params_vec))?;
    }

    Ok(total_affected)
}

// -------------------------------------------------------------------------
// Token Usage Batch Insert
// -------------------------------------------------------------------------

/// Batch insert token_usage rows inside an existing transaction.
fn insert_token_usage_batched_in_tx(
    tx: &Transaction<'_>,
    entries: &[TokenUsageEntry],
) -> Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }

    // 24 columns per row; SQLite limit ~999 params → batch ~41 rows, use 35 for safety
    const BATCH_SIZE: usize = 35;
    let mut total_inserted = 0;

    for chunk in entries.chunks(BATCH_SIZE) {
        let placeholders: String = (0..chunk.len())
            .map(|_| "(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT OR IGNORE INTO token_usage (
                message_id, conversation_id, agent_id, workspace_id, source_id,
                timestamp_ms, day_id,
                model_name, model_family, model_tier, service_tier, provider,
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                thinking_tokens, total_tokens, estimated_cost_usd,
                role, content_chars, has_tool_calls, tool_call_count, data_source
            )
            VALUES {}",
            placeholders
        );

        let mut params_vec: Vec<ParamValue> = Vec::with_capacity(chunk.len() * 24);
        for e in chunk {
            params_vec.push(e.message_id.into());
            params_vec.push(e.conversation_id.into());
            params_vec.push(e.agent_id.into());
            params_vec.push(
                e.workspace_id
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(e.source_id.clone().into());
            params_vec.push(e.timestamp_ms.into());
            params_vec.push(e.day_id.into());
            params_vec.push(
                e.model_name
                    .clone()
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.model_family
                    .clone()
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.model_tier
                    .clone()
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.service_tier
                    .clone()
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.provider
                    .clone()
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.input_tokens
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.output_tokens
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.cache_read_tokens
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.cache_creation_tokens
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.thinking_tokens
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.total_tokens
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.estimated_cost_usd
                    .map(|v| ParamValue::from(v))
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(e.role.clone().into());
            params_vec.push(e.content_chars.into());
            params_vec.push((e.has_tool_calls as i64).into());
            params_vec.push((e.tool_call_count as i64).into());
            params_vec.push(e.data_source.clone().into());
        }

        total_inserted += tx.execute_with_params(&sql, &param_slice_to_values(&params_vec))?;
    }

    Ok(total_inserted)
}

/// Batch upsert token_daily_stats deltas inside an existing transaction.
fn update_token_daily_stats_batched_in_tx(
    tx: &Transaction<'_>,
    entries: &[(i64, String, String, String, TokenStatsDelta)],
) -> Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }

    let now = SqliteStorage::now_millis();
    const BATCH_SIZE: usize = 25; // 19 params per row → ~52 rows max, use 25 for safety

    let mut total_affected = 0;

    for chunk in entries.chunks(BATCH_SIZE) {
        let placeholders: String = (0..chunk.len())
            .map(|_| "(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT INTO token_daily_stats (
                day_id, agent_slug, source_id, model_family,
                api_call_count, user_message_count, assistant_message_count, tool_message_count,
                total_input_tokens, total_output_tokens, total_cache_read_tokens,
                total_cache_creation_tokens, total_thinking_tokens, grand_total_tokens,
                total_content_chars, total_tool_calls, estimated_cost_usd, session_count,
                last_updated
            )
            VALUES {}
            ON CONFLICT(day_id, agent_slug, source_id, model_family) DO UPDATE SET
                api_call_count = api_call_count + excluded.api_call_count,
                user_message_count = user_message_count + excluded.user_message_count,
                assistant_message_count = assistant_message_count + excluded.assistant_message_count,
                tool_message_count = tool_message_count + excluded.tool_message_count,
                total_input_tokens = total_input_tokens + excluded.total_input_tokens,
                total_output_tokens = total_output_tokens + excluded.total_output_tokens,
                total_cache_read_tokens = total_cache_read_tokens + excluded.total_cache_read_tokens,
                total_cache_creation_tokens = total_cache_creation_tokens + excluded.total_cache_creation_tokens,
                total_thinking_tokens = total_thinking_tokens + excluded.total_thinking_tokens,
                grand_total_tokens = grand_total_tokens + excluded.grand_total_tokens,
                total_content_chars = total_content_chars + excluded.total_content_chars,
                total_tool_calls = total_tool_calls + excluded.total_tool_calls,
                estimated_cost_usd = estimated_cost_usd + excluded.estimated_cost_usd,
                session_count = session_count + excluded.session_count,
                last_updated = excluded.last_updated",
            placeholders
        );

        let mut params_vec: Vec<ParamValue> = Vec::with_capacity(chunk.len() * 19);
        for (day_id, agent, source, model, delta) in chunk {
            params_vec.push((*day_id).into());
            params_vec.push(agent.clone().into());
            params_vec.push(source.clone().into());
            params_vec.push(model.clone().into());
            params_vec.push(delta.api_call_count.into());
            params_vec.push(delta.user_message_count.into());
            params_vec.push(delta.assistant_message_count.into());
            params_vec.push(delta.tool_message_count.into());
            params_vec.push(delta.total_input_tokens.into());
            params_vec.push(delta.total_output_tokens.into());
            params_vec.push(delta.total_cache_read_tokens.into());
            params_vec.push(delta.total_cache_creation_tokens.into());
            params_vec.push(delta.total_thinking_tokens.into());
            params_vec.push(delta.grand_total_tokens.into());
            params_vec.push(delta.total_content_chars.into());
            params_vec.push(delta.total_tool_calls.into());
            params_vec.push(ParamValue::from(delta.estimated_cost_usd));
            params_vec.push(delta.session_count.into());
            params_vec.push(now.into());
        }

        total_affected += tx.execute_with_params(&sql, &param_slice_to_values(&params_vec))?;
    }

    Ok(total_affected)
}

/// Batch insert message_metrics rows inside an existing transaction.
/// Uses INSERT OR IGNORE (message_id is PK, skip duplicates on re-index).
fn insert_message_metrics_batched_in_tx(
    tx: &Transaction<'_>,
    entries: &[MessageMetricsEntry],
) -> Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }

    // 24 columns per row → ~41 rows max, use 30 for safety
    const BATCH_SIZE: usize = 30;
    let mut total_inserted = 0;

    for chunk in entries.chunks(BATCH_SIZE) {
        let placeholders: String = (0..chunk.len())
            .map(|_| "(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT OR IGNORE INTO message_metrics (
                message_id, created_at_ms, hour_id, day_id,
                agent_slug, workspace_id, source_id, role,
                content_chars, content_tokens_est,
                model_name, model_family, model_tier, provider,
                api_input_tokens, api_output_tokens, api_cache_read_tokens,
                api_cache_creation_tokens, api_thinking_tokens,
                api_service_tier, api_data_source,
                tool_call_count, has_tool_calls, has_plan
            )
            VALUES {}",
            placeholders
        );

        let mut params_vec: Vec<ParamValue> = Vec::with_capacity(chunk.len() * 24);
        for e in chunk {
            params_vec.push(e.message_id.into());
            params_vec.push(e.created_at_ms.into());
            params_vec.push(e.hour_id.into());
            params_vec.push(e.day_id.into());
            params_vec.push(e.agent_slug.clone().into());
            params_vec.push(e.workspace_id.into());
            params_vec.push(e.source_id.clone().into());
            params_vec.push(e.role.clone().into());
            params_vec.push(e.content_chars.into());
            params_vec.push(e.content_tokens_est.into());
            params_vec.push(
                e.model_name
                    .clone()
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(e.model_family.clone().into());
            params_vec.push(e.model_tier.clone().into());
            params_vec.push(e.provider.clone().into());
            params_vec.push(
                e.api_input_tokens
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.api_output_tokens
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.api_cache_read_tokens
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.api_cache_creation_tokens
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.api_thinking_tokens
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(
                e.api_service_tier
                    .clone()
                    .map(|v| v.into())
                    .unwrap_or(ParamValue(frankensqlite::SqliteValue::Null)),
            );
            params_vec.push(e.api_data_source.clone().into());
            params_vec.push(e.tool_call_count.into());
            params_vec.push((e.has_tool_calls as i64).into());
            params_vec.push((e.has_plan as i64).into());
        }

        total_inserted += tx.execute_with_params(&sql, &param_slice_to_values(&params_vec))?;
    }

    Ok(total_inserted)
}

/// Flush AnalyticsRollupAggregator deltas to usage_hourly and usage_daily tables.
/// Uses INSERT...ON CONFLICT DO UPDATE for additive rollup semantics.
fn flush_analytics_rollups_in_tx(
    tx: &Transaction<'_>,
    agg: &AnalyticsRollupAggregator,
) -> Result<(usize, usize, usize)> {
    let now = SqliteStorage::now_millis();

    let hourly_affected = flush_rollup_table(tx, "usage_hourly", "hour_id", &agg.hourly, now)?;
    let daily_affected = flush_rollup_table(tx, "usage_daily", "day_id", &agg.daily, now)?;
    let models_daily_affected = flush_model_daily_rollup_table(tx, &agg.models_daily, now)?;

    Ok((hourly_affected, daily_affected, models_daily_affected))
}

/// Flush one rollup table (shared logic for hourly + daily).
fn flush_rollup_table(
    tx: &Transaction<'_>,
    table: &str,
    bucket_col: &str,
    deltas: &HashMap<(i64, String, i64, String), UsageRollupDelta>,
    now: i64,
) -> Result<usize> {
    if deltas.is_empty() {
        return Ok(0);
    }

    // 22 params per row → ~44 rows max, use 30 for safety
    const BATCH_SIZE: usize = 30;
    let mut total_affected = 0;

    let entries: Vec<_> = deltas.iter().collect();

    for chunk in entries.chunks(BATCH_SIZE) {
        let placeholders: String = (0..chunk.len())
            .map(|_| "(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT INTO {table} (
                {bucket_col}, agent_slug, workspace_id, source_id,
                message_count, user_message_count, assistant_message_count,
                tool_call_count, plan_message_count, plan_content_tokens_est_total,
                plan_api_tokens_total, api_coverage_message_count,
                content_tokens_est_total, content_tokens_est_user, content_tokens_est_assistant,
                api_tokens_total, api_input_tokens_total, api_output_tokens_total,
                api_cache_read_tokens_total, api_cache_creation_tokens_total,
                api_thinking_tokens_total, last_updated
            )
            VALUES {placeholders}
            ON CONFLICT({bucket_col}, agent_slug, workspace_id, source_id) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                user_message_count = user_message_count + excluded.user_message_count,
                assistant_message_count = assistant_message_count + excluded.assistant_message_count,
                tool_call_count = tool_call_count + excluded.tool_call_count,
                plan_message_count = plan_message_count + excluded.plan_message_count,
                plan_content_tokens_est_total = plan_content_tokens_est_total + excluded.plan_content_tokens_est_total,
                plan_api_tokens_total = plan_api_tokens_total + excluded.plan_api_tokens_total,
                api_coverage_message_count = api_coverage_message_count + excluded.api_coverage_message_count,
                content_tokens_est_total = content_tokens_est_total + excluded.content_tokens_est_total,
                content_tokens_est_user = content_tokens_est_user + excluded.content_tokens_est_user,
                content_tokens_est_assistant = content_tokens_est_assistant + excluded.content_tokens_est_assistant,
                api_tokens_total = api_tokens_total + excluded.api_tokens_total,
                api_input_tokens_total = api_input_tokens_total + excluded.api_input_tokens_total,
                api_output_tokens_total = api_output_tokens_total + excluded.api_output_tokens_total,
                api_cache_read_tokens_total = api_cache_read_tokens_total + excluded.api_cache_read_tokens_total,
                api_cache_creation_tokens_total = api_cache_creation_tokens_total + excluded.api_cache_creation_tokens_total,
                api_thinking_tokens_total = api_thinking_tokens_total + excluded.api_thinking_tokens_total,
                last_updated = excluded.last_updated"
        );

        let mut params_vec: Vec<ParamValue> = Vec::with_capacity(chunk.len() * 22);
        for &((bucket_id, agent, workspace_id, source), d) in chunk {
            params_vec.push((*bucket_id).into());
            params_vec.push(agent.clone().into());
            params_vec.push((*workspace_id).into());
            params_vec.push(source.clone().into());
            params_vec.push(d.message_count.into());
            params_vec.push(d.user_message_count.into());
            params_vec.push(d.assistant_message_count.into());
            params_vec.push(d.tool_call_count.into());
            params_vec.push(d.plan_message_count.into());
            params_vec.push(d.plan_content_tokens_est_total.into());
            params_vec.push(d.plan_api_tokens_total.into());
            params_vec.push(d.api_coverage_message_count.into());
            params_vec.push(d.content_tokens_est_total.into());
            params_vec.push(d.content_tokens_est_user.into());
            params_vec.push(d.content_tokens_est_assistant.into());
            params_vec.push(d.api_tokens_total.into());
            params_vec.push(d.api_input_tokens_total.into());
            params_vec.push(d.api_output_tokens_total.into());
            params_vec.push(d.api_cache_read_tokens_total.into());
            params_vec.push(d.api_cache_creation_tokens_total.into());
            params_vec.push(d.api_thinking_tokens_total.into());
            params_vec.push(now.into());
        }

        total_affected += tx.execute_with_params(&sql, &param_slice_to_values(&params_vec))?;
    }

    Ok(total_affected)
}

fn flush_model_daily_rollup_table(
    tx: &Transaction<'_>,
    deltas: &HashMap<(i64, String, i64, String, String, String), UsageRollupDelta>,
    now: i64,
) -> Result<usize> {
    if deltas.is_empty() {
        return Ok(0);
    }

    // 22 params per row, keep conservative batch size.
    const BATCH_SIZE: usize = 25;
    let mut total_affected = 0;

    let entries: Vec<_> = deltas.iter().collect();

    for chunk in entries.chunks(BATCH_SIZE) {
        let placeholders: String = (0..chunk.len())
            .map(|_| "(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT INTO usage_models_daily (
                day_id, agent_slug, workspace_id, source_id, model_family, model_tier,
                message_count, user_message_count, assistant_message_count,
                tool_call_count, plan_message_count, api_coverage_message_count,
                content_tokens_est_total, content_tokens_est_user, content_tokens_est_assistant,
                api_tokens_total, api_input_tokens_total, api_output_tokens_total,
                api_cache_read_tokens_total, api_cache_creation_tokens_total,
                api_thinking_tokens_total, last_updated
            )
            VALUES {placeholders}
            ON CONFLICT(day_id, agent_slug, workspace_id, source_id, model_family, model_tier) DO UPDATE SET
                message_count = message_count + excluded.message_count,
                user_message_count = user_message_count + excluded.user_message_count,
                assistant_message_count = assistant_message_count + excluded.assistant_message_count,
                tool_call_count = tool_call_count + excluded.tool_call_count,
                plan_message_count = plan_message_count + excluded.plan_message_count,
                api_coverage_message_count = api_coverage_message_count + excluded.api_coverage_message_count,
                content_tokens_est_total = content_tokens_est_total + excluded.content_tokens_est_total,
                content_tokens_est_user = content_tokens_est_user + excluded.content_tokens_est_user,
                content_tokens_est_assistant = content_tokens_est_assistant + excluded.content_tokens_est_assistant,
                api_tokens_total = api_tokens_total + excluded.api_tokens_total,
                api_input_tokens_total = api_input_tokens_total + excluded.api_input_tokens_total,
                api_output_tokens_total = api_output_tokens_total + excluded.api_output_tokens_total,
                api_cache_read_tokens_total = api_cache_read_tokens_total + excluded.api_cache_read_tokens_total,
                api_cache_creation_tokens_total = api_cache_creation_tokens_total + excluded.api_cache_creation_tokens_total,
                api_thinking_tokens_total = api_thinking_tokens_total + excluded.api_thinking_tokens_total,
                last_updated = excluded.last_updated"
        );

        let mut params_vec: Vec<ParamValue> = Vec::with_capacity(chunk.len() * 22);
        for &((day_id, agent, workspace_id, source, model_family, model_tier), d) in chunk {
            params_vec.push((*day_id).into());
            params_vec.push(agent.clone().into());
            params_vec.push((*workspace_id).into());
            params_vec.push(source.clone().into());
            params_vec.push(model_family.clone().into());
            params_vec.push(model_tier.clone().into());
            params_vec.push(d.message_count.into());
            params_vec.push(d.user_message_count.into());
            params_vec.push(d.assistant_message_count.into());
            params_vec.push(d.tool_call_count.into());
            params_vec.push(d.plan_message_count.into());
            params_vec.push(d.api_coverage_message_count.into());
            params_vec.push(d.content_tokens_est_total.into());
            params_vec.push(d.content_tokens_est_user.into());
            params_vec.push(d.content_tokens_est_assistant.into());
            params_vec.push(d.api_tokens_total.into());
            params_vec.push(d.api_input_tokens_total.into());
            params_vec.push(d.api_output_tokens_total.into());
            params_vec.push(d.api_cache_read_tokens_total.into());
            params_vec.push(d.api_cache_creation_tokens_total.into());
            params_vec.push(d.api_thinking_tokens_total.into());
            params_vec.push(now.into());
        }

        total_affected += tx.execute_with_params(&sql, &param_slice_to_values(&params_vec))?;
    }

    Ok(total_affected)
}

/// Update conversation-level token summary columns from token_usage data.
fn update_conversation_token_summaries_in_tx(
    tx: &Transaction<'_>,
    conversation_id: i64,
) -> Result<()> {
    tx.execute(
        "UPDATE conversations SET
            total_input_tokens = (SELECT SUM(input_tokens) FROM token_usage WHERE conversation_id = ?1),
            total_output_tokens = (SELECT SUM(output_tokens) FROM token_usage WHERE conversation_id = ?1),
            total_cache_read_tokens = (SELECT SUM(cache_read_tokens) FROM token_usage WHERE conversation_id = ?1),
            total_cache_creation_tokens = (SELECT SUM(cache_creation_tokens) FROM token_usage WHERE conversation_id = ?1),
            grand_total_tokens = (SELECT SUM(total_tokens) FROM token_usage WHERE conversation_id = ?1),
            estimated_cost_usd = (SELECT SUM(estimated_cost_usd) FROM token_usage WHERE conversation_id = ?1),
            primary_model = (SELECT model_name FROM token_usage WHERE conversation_id = ?1
                             AND model_name IS NOT NULL
                             GROUP BY model_name ORDER BY COUNT(*) DESC LIMIT 1),
            api_call_count = (SELECT COUNT(*) FROM token_usage WHERE conversation_id = ?1
                              AND data_source = 'api'),
            tool_call_count = (SELECT SUM(tool_call_count) FROM token_usage WHERE conversation_id = ?1),
            user_message_count = (SELECT COUNT(*) FROM token_usage WHERE conversation_id = ?1
                                  AND role = 'user'),
            assistant_message_count = (SELECT COUNT(*) FROM token_usage WHERE conversation_id = ?1
                                       AND role IN ('assistant', 'agent'))
         WHERE id = ?1",
        params![conversation_id],
    )?;
    Ok(())
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

    #[test]
    fn reopen_existing_current_schema_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("existing.db");

        // First open creates and migrates to current schema.
        {
            let storage = SqliteStorage::open(&db_path).unwrap();
            assert_eq!(storage.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
        }

        // Re-open should not fail on current schema.
        let reopened = SqliteStorage::open(&db_path).unwrap();
        assert_eq!(
            reopened.schema_version().unwrap(),
            CURRENT_SCHEMA_VERSION,
            "reopening current schema DB should be idempotent"
        );
    }

    #[test]
    fn open_or_rebuild_current_schema_does_not_trigger_rebuild() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("existing.db");

        // Create DB at current schema.
        {
            let storage = SqliteStorage::open(&db_path).unwrap();
            assert_eq!(storage.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
        }

        // Should open normally, not require rebuild.
        let reopened = SqliteStorage::open_or_rebuild(&db_path)
            .expect("current schema DB should open without rebuild");
        assert_eq!(reopened.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
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
    // Current analytics/schema smoke test (bead z9fse.11)
    // =========================================================================

    #[test]
    fn migration_v13_creates_analytics_tables() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        // Schema version should be current.
        let version = storage.schema_version().unwrap();
        assert_eq!(
            version, CURRENT_SCHEMA_VERSION,
            "Schema version must match CURRENT_SCHEMA_VERSION after migration"
        );

        let conn = storage.raw();

        // Helper: collect column names from PRAGMA table_info
        fn col_names(conn: &Connection, table: &str) -> Vec<String> {
            let mut stmt = conn
                .prepare(&format!("PRAGMA table_info({})", table))
                .unwrap();
            stmt.query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        }

        // Helper: collect index names from PRAGMA index_list
        fn idx_names(conn: &Connection, table: &str) -> Vec<String> {
            let mut stmt = conn
                .prepare(&format!("PRAGMA index_list({})", table))
                .unwrap();
            stmt.query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        }

        // Verify message_metrics table exists with expected columns
        let mm_cols = col_names(conn, "message_metrics");
        for expected in &[
            "message_id",
            "hour_id",
            "day_id",
            "content_tokens_est",
            "model_name",
            "model_family",
            "model_tier",
            "provider",
            "api_input_tokens",
            "has_plan",
            "agent_slug",
            "role",
            "api_data_source",
        ] {
            assert!(
                mm_cols.contains(&expected.to_string()),
                "message_metrics missing column: {expected}"
            );
        }

        // Verify usage_hourly table
        let uh_cols = col_names(conn, "usage_hourly");
        for expected in &[
            "hour_id",
            "plan_message_count",
            "plan_content_tokens_est_total",
            "plan_api_tokens_total",
            "api_coverage_message_count",
            "content_tokens_est_user",
            "api_thinking_tokens_total",
        ] {
            assert!(
                uh_cols.contains(&expected.to_string()),
                "usage_hourly missing column: {expected}"
            );
        }

        // Verify usage_daily table
        let ud_cols = col_names(conn, "usage_daily");
        for expected in &[
            "day_id",
            "plan_content_tokens_est_total",
            "plan_api_tokens_total",
            "api_thinking_tokens_total",
            "content_tokens_est_assistant",
            "message_count",
        ] {
            assert!(
                ud_cols.contains(&expected.to_string()),
                "usage_daily missing column: {expected}"
            );
        }

        // Verify usage_models_daily table
        let umd_cols = col_names(conn, "usage_models_daily");
        for expected in &[
            "day_id",
            "model_family",
            "model_tier",
            "message_count",
            "api_tokens_total",
            "api_coverage_message_count",
        ] {
            assert!(
                umd_cols.contains(&expected.to_string()),
                "usage_models_daily missing column: {expected}"
            );
        }

        // Verify indexes on message_metrics
        let mm_idxs = idx_names(conn, "message_metrics");
        assert!(
            mm_idxs.iter().any(|n| n.contains("idx_mm_hour")),
            "message_metrics must have hour index"
        );
        assert!(
            mm_idxs.iter().any(|n| n.contains("idx_mm_agent_day")),
            "message_metrics must have agent+day index"
        );
        assert!(
            mm_idxs
                .iter()
                .any(|n| n.contains("idx_mm_model_family_day")),
            "message_metrics must have model_family+day index"
        );

        // Verify indexes on usage_hourly
        let uh_idxs = idx_names(conn, "usage_hourly");
        assert!(
            uh_idxs.iter().any(|n| n.contains("idx_uh_agent")),
            "usage_hourly must have agent index"
        );

        // Verify indexes on usage_daily
        let ud_idxs = idx_names(conn, "usage_daily");
        assert!(
            ud_idxs.iter().any(|n| n.contains("idx_ud_agent")),
            "usage_daily must have agent index"
        );

        // Verify indexes on usage_models_daily
        let umd_idxs = idx_names(conn, "usage_models_daily");
        assert!(
            umd_idxs.iter().any(|n| n.contains("idx_umd_model_day")),
            "usage_models_daily must have model+day index"
        );
    }

    #[test]
    fn hour_id_round_trip() {
        // 2026-02-06 12:00:00 UTC
        let ts_ms = 1_770_508_800_000_i64;
        let hour_id = SqliteStorage::hour_id_from_millis(ts_ms);
        let day_id = SqliteStorage::day_id_from_millis(ts_ms);

        // hour_id should be 24x day_id (approximately)
        assert_eq!(hour_id / 24, day_id, "hour_id/24 should equal day_id");

        // Round-trip: millis_from_hour_id should give start of that hour
        let back = SqliteStorage::millis_from_hour_id(hour_id);
        assert!(
            back <= ts_ms && ts_ms - back < 3_600_000,
            "Round-trip should land within the same hour"
        );
    }

    #[test]
    fn day_and_hour_ids_floor_negative_millis() {
        // One millisecond before the Unix epoch should still floor into the
        // previous second/hour/day rather than truncating toward zero.
        let ts_ms = -1_i64;
        let expected_secs = -1_i64;
        let epoch_2020_secs = 1_577_836_800_i64;

        assert_eq!(
            SqliteStorage::day_id_from_millis(ts_ms),
            (expected_secs - epoch_2020_secs).div_euclid(86_400)
        );
        assert_eq!(
            SqliteStorage::hour_id_from_millis(ts_ms),
            (expected_secs - epoch_2020_secs).div_euclid(3_600)
        );
    }

    #[test]
    fn migration_v13_from_v10() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");

        // Open at v10 first by faking it
        {
            let mut conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);",
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES('schema_version', '10')",
                [],
            )
            .unwrap();
            // Apply V1-V10 so schema is correct
            let tx = conn.transaction().unwrap();
            tx.execute_batch(MIGRATION_V1).unwrap();
            tx.execute_batch(MIGRATION_V2).unwrap();
            tx.execute_batch(MIGRATION_V3).unwrap();
            tx.execute_batch(MIGRATION_V4).unwrap();
            tx.execute_batch(MIGRATION_V5).unwrap();
            tx.execute_batch(MIGRATION_V6).unwrap();
            tx.execute_batch(MIGRATION_V7).unwrap();
            tx.execute_batch(MIGRATION_V8).unwrap();
            tx.execute_batch(MIGRATION_V9).unwrap();
            tx.execute_batch(MIGRATION_V10).unwrap();
            tx.execute(
                "UPDATE meta SET value = '10' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
            tx.commit().unwrap();
        }

        // Now open with SqliteStorage — should auto-migrate to current schema
        let storage = SqliteStorage::open(&db_path).unwrap();
        let version = storage.schema_version().unwrap();
        assert_eq!(
            version, CURRENT_SCHEMA_VERSION,
            "Should have migrated from v10 to the current schema"
        );

        // Verify new tables exist
        let count: i64 = storage
            .raw()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('message_metrics', 'usage_hourly', 'usage_daily', 'usage_models_daily')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(count, 4, "All 4 analytics tables should exist");
    }

    // =========================================================================
    // Analytics ingest integration test (bead z9fse.2)
    // =========================================================================

    #[test]
    fn analytics_ingest_populates_metrics_and_rollups() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();

        // Register agent + workspace
        let agent = Agent {
            id: None,
            slug: "claude_code".into(),
            name: "Claude Code".into(),
            version: Some("1.0".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        // Create a synthetic conversation with 3 messages at a known timestamp
        // 2026-02-06 10:30:00 UTC → day_id = 2228, hour_id = 53472
        let ts_ms = 1_770_551_400_000_i64;
        let expected_day = SqliteStorage::day_id_from_millis(ts_ms);
        let expected_hour = SqliteStorage::hour_id_from_millis(ts_ms);

        // Include a JSON usage block on the assistant message (like Claude Code data)
        let usage_json = serde_json::json!({
            "message": {
                "model": "claude-opus-4-6",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 50,
                    "cache_read_input_tokens": 200,
                    "cache_creation_input_tokens": 30,
                    "service_tier": "standard"
                }
            }
        });

        let conv = Conversation {
            id: None,
            agent_slug: "claude_code".into(),
            workspace: None,
            external_id: Some("test-conv-1".into()),
            title: Some("Test conversation".into()),
            source_path: PathBuf::from("/tmp/test.jsonl"),
            started_at: Some(ts_ms),
            ended_at: Some(ts_ms + 60_000),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![
                Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(ts_ms),
                    content: "Hello, can you help me with a plan?".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: vec![],
                },
                Message {
                    id: None,
                    idx: 1,
                    role: MessageRole::Agent,
                    author: None,
                    created_at: Some(ts_ms + 30_000),
                    content: "## Plan\n\n1. First step\n2. Second step\n3. Third step".into(),
                    extra_json: usage_json,
                    snippets: vec![],
                },
                Message {
                    id: None,
                    idx: 2,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(ts_ms + 60_000),
                    content: "Great, let's proceed!".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: vec![],
                },
            ],
            source_id: "local".into(),
            origin_host: None,
        };

        let outcomes = storage
            .insert_conversations_batched(&[(agent_id, None, &conv)])
            .unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].inserted_indices.len(), 3);

        let conn = storage.raw();

        // Verify message_metrics rows
        let mm_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM message_metrics", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(mm_count, 3, "Should have 3 message_metrics rows");

        // Verify hour_id and day_id are correct
        let mut stmt = conn
            .prepare("SELECT hour_id, day_id, role, content_tokens_est, has_plan, api_data_source, model_family, model_tier, provider FROM message_metrics ORDER BY message_id")
            .unwrap();
        #[allow(clippy::type_complexity)]
        let rows: Vec<(i64, i64, String, i64, i64, String, String, String, String)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert_eq!(rows.len(), 3);
        // All messages in the same hour/day
        assert_eq!(rows[0].0, expected_hour);
        assert_eq!(rows[0].1, expected_day);
        // First message is user
        assert_eq!(rows[0].2, "user");
        // Second message (assistant) should have has_plan=1 (contains "## Plan" + numbered steps)
        assert_eq!(
            rows[1].4, 1,
            "Assistant message with plan should have has_plan=1"
        );
        // Second message should have api data source
        assert_eq!(
            rows[1].5, "api",
            "Claude Code assistant message should have api data source"
        );
        // First and third (user) messages should be estimated
        assert_eq!(rows[0].5, "estimated");
        assert_eq!(rows[2].5, "estimated");
        assert_eq!(rows[1].6, "claude");
        assert_eq!(rows[1].7, "opus");
        assert_eq!(rows[1].8, "anthropic");
        assert_eq!(rows[0].6, "unknown");
        // content_tokens_est = chars / 4
        let user_chars = "Hello, can you help me with a plan?".len() as i64;
        assert_eq!(rows[0].3, user_chars / 4);

        // Verify usage_hourly rollup
        let (uh_msg, uh_user, uh_asst, uh_plan, uh_plan_content, uh_plan_api, uh_api_cov): (
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = conn
            .query_row(
                "SELECT message_count, user_message_count, assistant_message_count, plan_message_count,
                        plan_content_tokens_est_total, plan_api_tokens_total, api_coverage_message_count
                 FROM usage_hourly WHERE hour_id = ?",
                params![expected_hour],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(uh_msg, 3, "Hourly rollup should have 3 messages");
        assert_eq!(uh_user, 2, "Hourly rollup should have 2 user messages");
        assert_eq!(uh_asst, 1, "Hourly rollup should have 1 assistant message");
        assert_eq!(uh_plan, 1, "Hourly rollup should have 1 plan message");
        assert!(
            uh_plan_content > 0,
            "Hourly rollup should include plan content tokens"
        );
        assert!(
            uh_plan_api > 0,
            "Hourly rollup should include plan API tokens"
        );
        assert_eq!(
            uh_api_cov, 1,
            "Hourly rollup should have 1 API-covered message"
        );

        // Verify usage_daily rollup matches hourly (same day)
        let (ud_msg, ud_api_cov): (i64, i64) = conn
            .query_row(
                "SELECT message_count, api_coverage_message_count FROM usage_daily WHERE day_id = ?",
                params![expected_day],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(ud_msg, 3, "Daily rollup should match hourly");
        assert_eq!(
            ud_api_cov, 1,
            "Daily api_coverage should be 1 (only assistant msg has real API data)"
        );

        // Verify the API input tokens from message_metrics (only API-sourced)
        let api_only_input: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(api_input_tokens), 0) FROM message_metrics WHERE day_id = ? AND api_data_source = 'api'",
                params![expected_day],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(
            api_only_input, 100,
            "Only API-sourced input tokens should be 100"
        );

        // Verify rollups match summed message_metrics
        let mm_total_content_est: i64 = conn
            .query_row(
                "SELECT SUM(content_tokens_est) FROM message_metrics WHERE day_id = ?",
                params![expected_day],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        let mm_plan_content_est: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(content_tokens_est), 0) FROM message_metrics WHERE day_id = ? AND has_plan = 1",
                params![expected_day],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        let mm_plan_api_total: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(COALESCE(api_input_tokens, 0) + COALESCE(api_output_tokens, 0) + COALESCE(api_cache_read_tokens, 0) + COALESCE(api_cache_creation_tokens, 0) + COALESCE(api_thinking_tokens, 0)), 0)
                 FROM message_metrics WHERE day_id = ? AND has_plan = 1 AND api_data_source = 'api'",
                params![expected_day],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        let ud_content_est: i64 = conn
            .query_row(
                "SELECT content_tokens_est_total FROM usage_daily WHERE day_id = ?",
                params![expected_day],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        let (ud_plan_content_est, ud_plan_api_total): (i64, i64) = conn
            .query_row(
                "SELECT plan_content_tokens_est_total, plan_api_tokens_total FROM usage_daily WHERE day_id = ?",
                params![expected_day],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            mm_total_content_est, ud_content_est,
            "Daily rollup content_tokens_est_total must equal SUM of message_metrics"
        );
        assert_eq!(
            mm_plan_content_est, ud_plan_content_est,
            "Daily rollup plan_content_tokens_est_total must equal planned message_metrics content sum"
        );
        assert_eq!(
            mm_plan_api_total, ud_plan_api_total,
            "Daily rollup plan_api_tokens_total must equal planned message_metrics API token sum"
        );

        // Verify model rollup rows
        let (claude_msg, claude_user, claude_asst, claude_api_total, claude_api_cov): (
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = conn
            .query_row(
                "SELECT message_count, user_message_count, assistant_message_count, api_tokens_total, api_coverage_message_count
                 FROM usage_models_daily
                 WHERE day_id = ? AND model_family = 'claude' AND model_tier = 'opus'",
                params![expected_day],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        assert_eq!(claude_msg, 1);
        assert_eq!(claude_user, 0);
        assert_eq!(claude_asst, 1);
        assert_eq!(claude_api_total, 380);
        assert_eq!(claude_api_cov, 1);

        let unknown_msg: i64 = conn
            .query_row(
                "SELECT message_count FROM usage_models_daily
                 WHERE day_id = ? AND model_family = 'unknown' AND model_tier = 'unknown'",
                params![expected_day],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            unknown_msg, 2,
            "user messages should land in unknown model bucket"
        );
    }

    #[test]
    fn has_plan_heuristic_detects_plans() {
        assert!(has_plan_heuristic(
            "## Plan\n\n1. First step\n2. Second step"
        ));
        assert!(has_plan_heuristic(
            "# Plan\nHere is what we will do:\n1. Step one\n2. Step two"
        ));
        assert!(has_plan_heuristic(
            "Plan:\n- Gather baseline\n- Implement changes\n- Validate with tests"
        ));
        assert!(has_plan_heuristic(
            "Next steps:\n1. Update schema\n2. Rebuild rollups"
        ));
        assert!(!has_plan_heuristic("Hello world"));
        assert!(!has_plan_heuristic("Short"));
        assert!(!has_plan_heuristic(
            "This is a regular message without plans"
        ));
        assert!(!has_plan_heuristic(
            "```json\n{\"tool\":\"shell\",\"stdout\":\"1. install\\n2. run\"}\n```"
        ));
    }

    #[test]
    fn has_plan_for_role_only_counts_assistant_messages() {
        let plan_text = "## Plan\n1. First\n2. Second";
        assert!(has_plan_for_role("assistant", plan_text));
        assert!(has_plan_for_role("agent", plan_text));
        assert!(has_plan_for_role("Assistant", plan_text));
        assert!(!has_plan_for_role("user", plan_text));
        assert!(!has_plan_for_role("tool", plan_text));
    }

    #[test]
    fn plan_api_rollup_requires_api_data_source() {
        let mut agg = AnalyticsRollupAggregator::new();

        let estimated_plan = MessageMetricsEntry {
            message_id: 1,
            created_at_ms: 0,
            hour_id: 1,
            day_id: 1,
            agent_slug: "codex".into(),
            workspace_id: 0,
            source_id: "local".into(),
            role: "assistant".into(),
            content_chars: 120,
            content_tokens_est: 30,
            model_name: None,
            model_family: "unknown".into(),
            model_tier: "unknown".into(),
            provider: "unknown".into(),
            api_input_tokens: Some(100),
            api_output_tokens: Some(50),
            api_cache_read_tokens: Some(0),
            api_cache_creation_tokens: Some(0),
            api_thinking_tokens: Some(0),
            api_service_tier: None,
            api_data_source: "estimated".into(),
            tool_call_count: 0,
            has_tool_calls: false,
            has_plan: true,
        };
        agg.record(&estimated_plan);

        let api_plan = MessageMetricsEntry {
            message_id: 2,
            created_at_ms: 0,
            hour_id: 1,
            day_id: 1,
            agent_slug: "codex".into(),
            workspace_id: 0,
            source_id: "local".into(),
            role: "assistant".into(),
            content_chars: 80,
            content_tokens_est: 20,
            model_name: None,
            model_family: "unknown".into(),
            model_tier: "unknown".into(),
            provider: "unknown".into(),
            api_input_tokens: Some(40),
            api_output_tokens: Some(10),
            api_cache_read_tokens: Some(0),
            api_cache_creation_tokens: Some(0),
            api_thinking_tokens: Some(0),
            api_service_tier: None,
            api_data_source: "api".into(),
            tool_call_count: 0,
            has_tool_calls: false,
            has_plan: true,
        };
        agg.record(&api_plan);

        let key = (1_i64, "codex".to_string(), 0_i64, "local".to_string());
        let hourly = agg.hourly.get(&key).expect("hourly rollup key must exist");

        // Content rollup includes both plan messages.
        assert_eq!(hourly.plan_message_count, 2);
        assert_eq!(hourly.plan_content_tokens_est_total, 50);
        // API plan tokens must include only api_data_source='api' rows.
        assert_eq!(hourly.plan_api_tokens_total, 50);
        // Overall API tokens still include all row-level API token fields.
        assert_eq!(hourly.api_tokens_total, 200);
    }

    #[test]
    fn has_plan_heuristic_curated_corpus_thresholds() {
        // Cross-agent-style positives.
        let positives = [
            "## Plan\n1. Inspect current schema\n2. Add migration\n3. Verify rebuild",
            "Plan:\n1) Reproduce\n2) Patch\n3) Add tests",
            "Implementation plan:\n- Parse inputs\n- Update rollups\n- Run checks",
            "Next steps:\n1. Reserve file\n2. Implement\n3. Report status",
            "# Plan\n1. Gather requirements\n2. Ship changes",
            "Action plan:\n- Identify root cause\n- Fix it\n- Validate",
        ];

        // Typical false positives we want to avoid.
        let negatives = [
            "The plan is to move fast and fix things later.",
            "```json\n{\"tool\":\"shell\",\"stdout\":\"1. ls\\n2. cat\"}\n```",
            "stdout:\n1. Build started\n2. Build finished\nexit code: 0",
            "I can help with that request. Let me know if you want details.",
            "Here is a list:\n- apples\n- oranges",
            "Status update: completed tasks and blockers below.",
        ];

        let tp = positives
            .iter()
            .filter(|msg| has_plan_heuristic(msg))
            .count();
        let fp = negatives
            .iter()
            .filter(|msg| has_plan_heuristic(msg))
            .count();

        let recall = tp as f64 / positives.len() as f64;
        let false_positive_rate = fp as f64 / negatives.len() as f64;

        assert!(
            recall >= 0.80,
            "plan heuristic recall too low: got {recall:.2}"
        );
        assert!(
            false_positive_rate <= 0.20,
            "plan heuristic false-positive rate too high: got {false_positive_rate:.2}"
        );
    }

    #[test]
    fn rebuild_analytics_repopulates_from_messages() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();

        // Register agent
        let agent = Agent {
            id: None,
            slug: "claude_code".into(),
            name: "Claude Code".into(),
            version: Some("1.0".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        // 2026-02-06 10:30:00 UTC
        let ts_ms = 1_770_551_400_000_i64;
        let expected_day = SqliteStorage::day_id_from_millis(ts_ms);
        let expected_hour = SqliteStorage::hour_id_from_millis(ts_ms);

        let usage_json = serde_json::json!({
            "message": {
                "model": "claude-opus-4-6",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 50,
                    "cache_read_input_tokens": 200,
                    "cache_creation_input_tokens": 30,
                    "service_tier": "standard"
                }
            }
        });

        let conv = Conversation {
            id: None,
            agent_slug: "claude_code".into(),
            workspace: None,
            external_id: Some("test-rebuild-1".into()),
            title: Some("Test conversation".into()),
            source_path: PathBuf::from("/tmp/test.jsonl"),
            started_at: Some(ts_ms),
            ended_at: Some(ts_ms + 60_000),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![
                Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(ts_ms),
                    content: "Hello, can you help me with a plan?".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: vec![],
                },
                Message {
                    id: None,
                    idx: 1,
                    role: MessageRole::Agent,
                    author: None,
                    created_at: Some(ts_ms + 30_000),
                    content: "## Plan\n\n1. First step\n2. Second step\n3. Third step".into(),
                    extra_json: usage_json,
                    snippets: vec![],
                },
                Message {
                    id: None,
                    idx: 2,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(ts_ms + 60_000),
                    content: "Great, let's proceed!".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: vec![],
                },
            ],
            source_id: "local".into(),
            origin_host: None,
        };

        storage
            .insert_conversations_batched(&[(agent_id, None, &conv)])
            .unwrap();

        // Save original analytics state
        let conn = storage.raw();
        let orig_mm: i64 = conn
            .query_row("SELECT COUNT(*) FROM message_metrics", [], |row| row.get(0))
            .unwrap();
        let orig_hourly: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_hourly", [], |row| row.get(0))
            .unwrap();
        let orig_daily: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_daily", [], |row| row.get(0))
            .unwrap();
        let orig_models_daily: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_models_daily", [], |row| {
                row.get(0)
            })
            .unwrap();
        let orig_api_input: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(api_input_tokens), 0) FROM message_metrics WHERE api_data_source = 'api'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(orig_mm, 3);
        assert!(orig_hourly > 0);
        assert!(orig_daily > 0);
        assert!(orig_models_daily > 0);

        // Destroy analytics tables (simulate corruption)
        conn.execute("DELETE FROM message_metrics").unwrap();
        conn.execute("DELETE FROM usage_hourly").unwrap();
        conn.execute("DELETE FROM usage_daily").unwrap();
        conn.execute("DELETE FROM usage_models_daily").unwrap();

        // Verify they're empty
        let zero: i64 = conn
            .query_row("SELECT COUNT(*) FROM message_metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(zero, 0);

        // Rebuild analytics
        let result = storage.rebuild_analytics().unwrap();

        assert_eq!(result.message_metrics_rows, 3);
        assert!(result.usage_hourly_rows > 0);
        assert!(result.usage_daily_rows > 0);
        assert!(result.usage_models_daily_rows > 0);
        assert!(
            result.elapsed_ms < 10_000,
            "Rebuild should be fast for 3 msgs"
        );

        // Verify rebuilt data matches
        let conn = storage.raw();
        let rebuilt_mm: i64 = conn
            .query_row("SELECT COUNT(*) FROM message_metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            rebuilt_mm, orig_mm,
            "Rebuilt message_metrics count should match"
        );

        let rebuilt_hourly: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_hourly", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            rebuilt_hourly, orig_hourly,
            "Rebuilt hourly rows should match"
        );

        let rebuilt_daily: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_daily", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rebuilt_daily, orig_daily, "Rebuilt daily rows should match");

        let rebuilt_models_daily: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_models_daily", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            rebuilt_models_daily, orig_models_daily,
            "Rebuilt model rollup rows should match"
        );

        // Verify API token data preserved through rebuild
        let rebuilt_api_input: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(api_input_tokens), 0) FROM message_metrics WHERE api_data_source = 'api'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            rebuilt_api_input, orig_api_input,
            "Rebuilt API input tokens should match original"
        );

        // Verify rollups have correct data
        let (uh_msg, uh_user, uh_asst, uh_plan, uh_plan_content, uh_plan_api): (
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = conn
            .query_row(
                "SELECT message_count, user_message_count, assistant_message_count, plan_message_count,
                        plan_content_tokens_est_total, plan_api_tokens_total
                 FROM usage_hourly WHERE hour_id = ?",
                params![expected_hour],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(uh_msg, 3);
        assert_eq!(uh_user, 2);
        assert_eq!(uh_asst, 1);
        assert_eq!(uh_plan, 1);
        assert!(uh_plan_content > 0);
        assert!(uh_plan_api > 0);

        let ud_msg: i64 = conn
            .query_row(
                "SELECT message_count FROM usage_daily WHERE day_id = ?",
                params![expected_day],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ud_msg, 3);
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

    // =========================================================================
    // Pricing / cost estimation tests (bead z9fse.10)
    // =========================================================================

    #[test]
    fn sql_like_match_basic_patterns() {
        assert!(sql_like_match("claude-opus-4-20250101", "claude-opus-4%"));
        assert!(sql_like_match("claude-opus-4", "claude-opus-4%"));
        assert!(!sql_like_match("claude-sonnet-4", "claude-opus-4%"));

        // Middle wildcard (gemini pattern)
        assert!(sql_like_match("gemini-2.0-flash-001", "gemini-2%flash%"));
        assert!(sql_like_match("gemini-2-flash", "gemini-2%flash%"));
        assert!(!sql_like_match("gemini-2-pro", "gemini-2%flash%"));

        // Exact match
        assert!(sql_like_match("hello", "hello"));
        assert!(!sql_like_match("hello!", "hello"));

        // Underscore wildcard
        assert!(sql_like_match("gpt-4o", "gpt-4_"));
        assert!(!sql_like_match("gpt-4oo", "gpt-4_"));

        // Case insensitive
        assert!(sql_like_match("Claude-Opus-4", "claude-opus-4%"));
    }

    #[test]
    fn date_str_to_day_id_converts_correctly() {
        // 2025-10-01 is 2100 days after 2020-01-01
        assert_eq!(date_str_to_day_id("2025-10-01").unwrap(), 2100);
        // 2024-04-01 is 1552 days after 2020-01-01
        assert_eq!(date_str_to_day_id("2024-04-01").unwrap(), 1552);
        assert!(date_str_to_day_id("invalid").is_err());
    }

    #[test]
    fn pricing_table_lookup_selects_matching_entry() {
        let effective_day = date_str_to_day_id("2025-10-01").unwrap();
        let lookup_day = date_str_to_day_id("2026-02-06").unwrap();
        let table = PricingTable {
            entries: vec![
                PricingEntry {
                    model_pattern: "claude-opus-4%".into(),
                    provider: "anthropic".into(),
                    input_cost_per_mtok: 15.0,
                    output_cost_per_mtok: 75.0,
                    cache_read_cost_per_mtok: Some(1.5),
                    cache_creation_cost_per_mtok: Some(18.75),
                    effective_day_id: effective_day,
                },
                PricingEntry {
                    model_pattern: "claude-sonnet-4%".into(),
                    provider: "anthropic".into(),
                    input_cost_per_mtok: 3.0,
                    output_cost_per_mtok: 15.0,
                    cache_read_cost_per_mtok: Some(0.3),
                    cache_creation_cost_per_mtok: Some(3.75),
                    effective_day_id: effective_day,
                },
            ],
        };

        let result = table.lookup("claude-opus-4-20260101", lookup_day);
        assert!(result.is_some());
        assert_eq!(result.unwrap().input_cost_per_mtok, 15.0);

        let result = table.lookup("claude-sonnet-4-latest", lookup_day);
        assert!(result.is_some());
        assert_eq!(result.unwrap().input_cost_per_mtok, 3.0);

        assert!(table.lookup("unknown-model", lookup_day).is_none());
    }

    #[test]
    fn pricing_table_lookup_respects_effective_date() {
        let effective_day_1 = date_str_to_day_id("2025-10-01").unwrap();
        let effective_day_2 = date_str_to_day_id("2026-01-01").unwrap();
        let table = PricingTable {
            entries: vec![
                PricingEntry {
                    model_pattern: "claude-opus-4%".into(),
                    provider: "anthropic".into(),
                    input_cost_per_mtok: 15.0,
                    output_cost_per_mtok: 75.0,
                    cache_read_cost_per_mtok: None,
                    cache_creation_cost_per_mtok: None,
                    effective_day_id: effective_day_1,
                },
                PricingEntry {
                    model_pattern: "claude-opus-4%".into(),
                    provider: "anthropic".into(),
                    input_cost_per_mtok: 12.0,
                    output_cost_per_mtok: 60.0,
                    cache_read_cost_per_mtok: None,
                    cache_creation_cost_per_mtok: None,
                    effective_day_id: effective_day_2,
                },
            ],
        };

        // Before price drop
        let result = table.lookup("claude-opus-4", date_str_to_day_id("2025-11-01").unwrap());
        assert!(result.is_some());
        assert_eq!(result.unwrap().input_cost_per_mtok, 15.0);

        // After price drop
        let result = table.lookup("claude-opus-4", date_str_to_day_id("2026-02-01").unwrap());
        assert!(result.is_some());
        assert_eq!(result.unwrap().input_cost_per_mtok, 12.0);

        // Before all pricing
        assert!(
            table
                .lookup("claude-opus-4", date_str_to_day_id("2024-01-01").unwrap())
                .is_none()
        );
    }

    #[test]
    fn pricing_table_lookup_specificity_tiebreak() {
        let effective_day = date_str_to_day_id("2025-01-01").unwrap();
        let lookup_day = date_str_to_day_id("2026-01-01").unwrap();
        let table = PricingTable {
            entries: vec![
                PricingEntry {
                    model_pattern: "gpt-4%".into(),
                    provider: "openai".into(),
                    input_cost_per_mtok: 10.0,
                    output_cost_per_mtok: 30.0,
                    cache_read_cost_per_mtok: None,
                    cache_creation_cost_per_mtok: None,
                    effective_day_id: effective_day,
                },
                PricingEntry {
                    model_pattern: "gpt-4-turbo%".into(),
                    provider: "openai".into(),
                    input_cost_per_mtok: 5.0,
                    output_cost_per_mtok: 15.0,
                    cache_read_cost_per_mtok: None,
                    cache_creation_cost_per_mtok: None,
                    effective_day_id: effective_day,
                },
            ],
        };

        // Longer pattern wins for specific model
        let result = table.lookup("gpt-4-turbo-2025", lookup_day);
        assert!(result.is_some());
        assert_eq!(result.unwrap().input_cost_per_mtok, 5.0);

        // Shorter pattern matches broader model
        let result = table.lookup("gpt-4o", lookup_day);
        assert!(result.is_some());
        assert_eq!(result.unwrap().input_cost_per_mtok, 10.0);
    }

    #[test]
    fn pricing_table_compute_cost_basic() {
        let effective_day = date_str_to_day_id("2025-10-01").unwrap();
        let table = PricingTable {
            entries: vec![PricingEntry {
                model_pattern: "claude-opus-4%".into(),
                provider: "anthropic".into(),
                input_cost_per_mtok: 15.0,
                output_cost_per_mtok: 75.0,
                cache_read_cost_per_mtok: Some(1.5),
                cache_creation_cost_per_mtok: Some(18.75),
                effective_day_id: effective_day,
            }],
        };

        let cost = table.compute_cost(
            Some("claude-opus-4-latest"),
            date_str_to_day_id("2026-02-06").unwrap(),
            Some(1000),
            Some(500),
            None,
            None,
        );
        assert!(cost.is_some());
        // 1000 * 15.0 / 1M + 500 * 75.0 / 1M = 0.015 + 0.0375 = 0.0525
        assert!((cost.unwrap() - 0.0525).abs() < 1e-10);
    }

    #[test]
    fn pricing_table_compute_cost_with_cache() {
        let effective_day = date_str_to_day_id("2025-10-01").unwrap();
        let table = PricingTable {
            entries: vec![PricingEntry {
                model_pattern: "claude-opus-4%".into(),
                provider: "anthropic".into(),
                input_cost_per_mtok: 15.0,
                output_cost_per_mtok: 75.0,
                cache_read_cost_per_mtok: Some(1.5),
                cache_creation_cost_per_mtok: Some(18.75),
                effective_day_id: effective_day,
            }],
        };

        let cost = table.compute_cost(
            Some("claude-opus-4-latest"),
            date_str_to_day_id("2026-02-06").unwrap(),
            Some(1_000_000),
            Some(100_000),
            Some(500_000),
            Some(200_000),
        );
        assert!(cost.is_some());
        // input: 1M * 15/1M = 15.0, output: 100K * 75/1M = 7.5
        // cache_read: 500K * 1.5/1M = 0.75, cache_creation: 200K * 18.75/1M = 3.75
        // total = 27.0
        assert!((cost.unwrap() - 27.0).abs() < 1e-10);
    }

    #[test]
    fn pricing_table_compute_cost_returns_none_for_unknown_model() {
        let effective_day = date_str_to_day_id("2025-10-01").unwrap();
        let lookup_day = date_str_to_day_id("2026-02-06").unwrap();
        let table = PricingTable {
            entries: vec![PricingEntry {
                model_pattern: "claude-opus-4%".into(),
                provider: "anthropic".into(),
                input_cost_per_mtok: 15.0,
                output_cost_per_mtok: 75.0,
                cache_read_cost_per_mtok: None,
                cache_creation_cost_per_mtok: None,
                effective_day_id: effective_day,
            }],
        };

        assert!(
            table
                .compute_cost(
                    Some("unknown-model"),
                    lookup_day,
                    Some(1000),
                    Some(500),
                    None,
                    None
                )
                .is_none()
        );
        assert!(
            table
                .compute_cost(None, lookup_day, Some(1000), Some(500), None, None)
                .is_none()
        );
        assert!(
            table
                .compute_cost(Some("claude-opus-4"), lookup_day, None, None, None, None)
                .is_none()
        );
    }

    #[test]
    fn pricing_table_load_from_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let table = PricingTable::load(&storage.conn).unwrap();
        assert!(!table.is_empty());

        let lookup_day = date_str_to_day_id("2026-02-06").unwrap();

        let opus = table.lookup("claude-opus-4-latest", lookup_day);
        assert!(opus.is_some());
        assert_eq!(opus.unwrap().input_cost_per_mtok, 15.0);

        let flash = table.lookup("gemini-2.0-flash-001", lookup_day);
        assert!(flash.is_some());
        assert_eq!(flash.unwrap().input_cost_per_mtok, 0.075);
    }

    #[test]
    fn pricing_table_load_rejects_invalid_effective_date() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        storage
            .conn
            .execute_compat(
                "INSERT INTO model_pricing (
                    model_pattern, provider, input_cost_per_mtok, output_cost_per_mtok,
                    cache_read_cost_per_mtok, cache_creation_cost_per_mtok, effective_date
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                fparams![
                    "broken-model%",
                    "test",
                    1.0_f64,
                    2.0_f64,
                    Option::<f64>::None,
                    Option::<f64>::None,
                    "not-a-date"
                ],
            )
            .unwrap();

        let err = PricingTable::load(&storage.conn).unwrap_err();
        assert!(err.to_string().contains("invalid effective_date"));
    }

    #[test]
    fn pricing_diagnostics_tracks_coverage() {
        let mut diag = PricingDiagnostics::default();
        diag.record_priced();
        diag.record_priced();
        diag.record_unpriced(Some("custom-model-v1"));
        diag.record_unpriced(Some("custom-model-v1"));
        diag.record_unpriced(None);

        assert_eq!(diag.priced_count, 2);
        assert_eq!(diag.unpriced_count, 3);
        assert_eq!(diag.unknown_models.len(), 2);
        assert_eq!(diag.unknown_models["custom-model-v1"], 2);
        assert_eq!(diag.unknown_models["(none)"], 1);
    }

    // =========================================================================
    // FrankenStorage migration tests (bead 2j6p6)
    // =========================================================================

    /// Helper: create a FrankenStorage wrapping an in-memory connection and
    /// run migrations. This exercises the same code path as `open()` but avoids
    /// frankensqlite's file-based autoindex renaming limitation (V5 uses
    /// ALTER TABLE RENAME which triggers sqlite_autoindex lookup issues on
    /// file-based pagers).
    fn franken_storage_in_memory() -> FrankenStorage {
        let conn = FrankenConnection::open(":memory:").unwrap();
        let storage = FrankenStorage { conn };
        storage.run_migrations().unwrap();
        storage.apply_config().unwrap();
        storage
    }

    #[test]
    fn franken_migrations_create_all_tables() {
        let storage = franken_storage_in_memory();

        // Should be at CURRENT_SCHEMA_VERSION.
        let version = storage.schema_version().unwrap();
        assert_eq!(
            version, CURRENT_SCHEMA_VERSION,
            "fresh FrankenStorage should be at current schema version"
        );

        // Core tables from V1 should exist.
        let rows = storage
            .raw()
            .query("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name;")
            .unwrap();
        let table_names: Vec<String> = rows
            .iter()
            .filter_map(|r| r.get_typed::<String>(0).ok())
            .collect();

        for required in [
            "meta",
            "agents",
            "workspaces",
            "conversations",
            "messages",
            "snippets",
            "tags",
            "conversation_tags",
        ] {
            assert!(
                table_names.contains(&required.to_string()),
                "missing table: {required}"
            );
        }

        // V4 sources table.
        assert!(
            table_names.contains(&"sources".to_string()),
            "missing sources table"
        );

        // V8 daily_stats table.
        assert!(
            table_names.contains(&"daily_stats".to_string()),
            "missing daily_stats table"
        );

        // V9 embedding_jobs table.
        assert!(
            table_names.contains(&"embedding_jobs".to_string()),
            "missing embedding_jobs table"
        );

        // V11 message_metrics, usage_hourly, usage_daily tables.
        for analytics_table in ["message_metrics", "usage_hourly", "usage_daily"] {
            assert!(
                table_names.contains(&analytics_table.to_string()),
                "missing table: {analytics_table}"
            );
        }

        // Fresh frankensqlite databases should record both applied migrations:
        // the combined V13 base schema and the V14 contentless FTS upgrade.
        let rows = storage
            .raw()
            .query("SELECT COUNT(*) FROM _schema_migrations;")
            .unwrap();
        let count: i64 = rows.first().unwrap().get_typed(0).unwrap();
        assert_eq!(
            count, 2,
            "_schema_migrations should record the V13 base schema and V14 FTS migration"
        );

        // The latest applied migration should be the current schema version.
        let rows = storage
            .raw()
            .query("SELECT version FROM _schema_migrations ORDER BY version;")
            .unwrap();
        let versions: Vec<i64> = rows
            .iter()
            .map(|row| row.get_typed(0))
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            versions,
            vec![13, 14],
            "_schema_migrations should contain v13 and v14"
        );
    }

    #[test]
    fn franken_migrations_idempotent() {
        let storage = franken_storage_in_memory();
        assert_eq!(storage.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);

        // Re-running migrations on the same connection is a no-op.
        storage.run_migrations().unwrap();
        assert_eq!(storage.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn franken_meta_schema_version_in_sync() {
        let storage = franken_storage_in_memory();

        // meta.schema_version should be kept in sync.
        let rows = storage
            .raw()
            .query("SELECT value FROM meta WHERE key = 'schema_version';")
            .unwrap();
        let meta_version: String = rows.first().unwrap().get_typed(0).unwrap();
        assert_eq!(
            meta_version,
            CURRENT_SCHEMA_VERSION.to_string(),
            "meta.schema_version should match CURRENT_SCHEMA_VERSION"
        );
    }

    #[test]
    fn franken_transition_from_meta_version() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_transition.db");

        // Simulate an existing database created by SqliteStorage at version 10.
        // We create just enough schema to test the transition.
        let conn = FrankenConnection::open(db_path.to_string_lossy().to_string()).unwrap();
        conn.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        conn.execute("INSERT INTO meta(key, value) VALUES('schema_version', '10');")
            .unwrap();
        // Create a dummy conversations table so transition doesn't think it's corrupted.
        conn.execute("CREATE TABLE conversations (id INTEGER PRIMARY KEY);")
            .unwrap();
        drop(conn);

        // Now run the transition function.
        let conn = FrankenConnection::open(db_path.to_string_lossy().to_string()).unwrap();
        transition_from_meta_version(&conn).unwrap();

        // _schema_migrations should exist with entries for versions 1..=10.
        let rows = conn
            .query("SELECT version FROM _schema_migrations ORDER BY version;")
            .unwrap();
        let versions: Vec<i64> = rows.iter().filter_map(|r| r.get_typed(0).ok()).collect();
        assert_eq!(
            versions,
            (1..=10).collect::<Vec<i64>>(),
            "transition should backfill versions 1..=10"
        );
    }

    #[test]
    fn franken_transition_skips_when_already_done() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_transition_skip.db");

        // Create a DB that already has _schema_migrations.
        let conn = FrankenConnection::open(db_path.to_string_lossy().to_string()).unwrap();
        conn.execute(
            "CREATE TABLE _schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at TEXT NOT NULL DEFAULT 'now');",
        ).unwrap();
        conn.execute("INSERT INTO _schema_migrations (version, name) VALUES (1, 'test');")
            .unwrap();

        // Transition should be a no-op.
        transition_from_meta_version(&conn).unwrap();

        // Should still have exactly 1 entry.
        let rows = conn
            .query("SELECT COUNT(*) FROM _schema_migrations;")
            .unwrap();
        let count: i64 = rows.first().unwrap().get_typed(0).unwrap();
        assert_eq!(
            count, 1,
            "transition should not re-run on already-transitioned DB"
        );
    }

    #[test]
    fn franken_transition_fresh_db_is_noop() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_fresh_noop.db");

        // Empty database — no meta table, no tables at all.
        let conn = FrankenConnection::open(db_path.to_string_lossy().to_string()).unwrap();
        transition_from_meta_version(&conn).unwrap();

        // _schema_migrations should NOT have been created.
        let res = conn.query("SELECT * FROM \"_schema_migrations\";");
        assert!(
            res.is_err(),
            "transition should not create _schema_migrations on fresh DB"
        );
    }

    #[test]
    fn build_cass_migrations_applies_combined_v13() {
        let conn = FrankenConnection::open(":memory:").unwrap();
        let runner = build_cass_migrations();
        let result = runner.run(&conn).unwrap();

        assert!(result.was_fresh);
        assert_eq!(
            result.applied,
            vec![13, 14],
            "should apply combined V13 + FTS contentless V14"
        );
        assert_eq!(result.current, 14);
    }

    // =========================================================================
    // FrankenConnectionManager tests (bead 3rlf8)
    // =========================================================================

    #[test]
    fn connection_manager_creates_readers() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("cm.db");

        // Create the DB first
        let fs = FrankenStorage::open(&db_path).unwrap();
        drop(fs);

        let config = ConnectionManagerConfig {
            reader_count: 3,
            max_writers: 2,
        };
        let mgr = FrankenConnectionManager::new(&db_path, config).unwrap();
        assert_eq!(mgr.reader_count(), 3);
        assert_eq!(mgr.max_writers(), 2);
    }

    #[test]
    fn connection_manager_reader_round_robin() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("cm.db");

        let fs = FrankenStorage::open(&db_path).unwrap();
        drop(fs);

        let config = ConnectionManagerConfig {
            reader_count: 2,
            max_writers: 1,
        };
        let mgr = FrankenConnectionManager::new(&db_path, config).unwrap();

        // Reader index should advance (round-robin)
        let idx_before = mgr.reader_idx.load(std::sync::atomic::Ordering::Relaxed);
        let _r1 = mgr.reader();
        let idx_after = mgr.reader_idx.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(idx_after, idx_before + 1, "reader index should advance");
    }

    #[test]
    fn connection_manager_writer_reads_and_writes() {
        use frankensqlite::compat::RowExt;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("cm.db");

        let fs = FrankenStorage::open(&db_path).unwrap();
        drop(fs);

        let mgr = FrankenConnectionManager::new(&db_path, Default::default()).unwrap();

        // Acquire writer and insert data
        {
            let mut guard = mgr.writer().unwrap();
            guard
                .storage()
                .raw()
                .execute("CREATE TABLE IF NOT EXISTS cm_test (id INTEGER PRIMARY KEY, val TEXT)")
                .unwrap();
            guard
                .storage()
                .raw()
                .execute("INSERT INTO cm_test (val) VALUES ('hello')")
                .unwrap();
            guard.mark_committed();
        }

        // Verify via reader (returns MutexGuard<SendFrankenConnection>)
        let reader_guard = mgr.reader();
        let rows = reader_guard.query("SELECT val FROM cm_test").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_typed::<String>(0).unwrap(), "hello");
    }

    #[test]
    fn connection_manager_writer_guard_drops_releases_slot() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("cm.db");

        let fs = FrankenStorage::open(&db_path).unwrap();
        drop(fs);

        let config = ConnectionManagerConfig {
            reader_count: 1,
            max_writers: 1,
        };
        let mgr = FrankenConnectionManager::new(&db_path, config).unwrap();

        // Acquire and release writer
        {
            let mut guard = mgr.writer().unwrap();
            guard.mark_committed();
        }

        // Should be able to acquire again (slot released)
        let mut guard2 = mgr.writer().unwrap();
        guard2.mark_committed();
    }

    #[test]
    fn connection_manager_concurrent_writer_works() {
        use frankensqlite::compat::RowExt;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("cm.db");

        let fs = FrankenStorage::open(&db_path).unwrap();
        drop(fs);

        let config = ConnectionManagerConfig {
            reader_count: 1,
            max_writers: 2,
        };
        let mgr = FrankenConnectionManager::new(&db_path, config).unwrap();

        {
            let mut guard = mgr.concurrent_writer().unwrap();
            guard
                .storage()
                .raw()
                .execute("CREATE TABLE IF NOT EXISTS cm_conc (id INTEGER PRIMARY KEY, val TEXT)")
                .unwrap();
            guard
                .storage()
                .raw()
                .execute("INSERT INTO cm_conc (val) VALUES ('concurrent')")
                .unwrap();
            guard.mark_committed();
        }

        let reader_guard = mgr.reader();
        let rows = reader_guard.query("SELECT val FROM cm_conc").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_typed::<String>(0).unwrap(), "concurrent");
    }

    #[test]
    fn connection_manager_default_config() {
        let config = ConnectionManagerConfig::default();
        assert_eq!(config.reader_count, 4);
        assert!(config.max_writers > 0);
    }
}
