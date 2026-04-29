//! `SQLite` backend: schema, pragmas, and migrations.

use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole, Snippet};
use crate::sources::provenance::{LOCAL_SOURCE_ID, Source, SourceKind};
use anyhow::{Context, Result, anyhow};
use frankensqlite::{
    Connection as FrankenConnection, Row as FrankenRow, SqliteValue,
    compat::{
        ConnectionExt as FrankenConnectionExt, OpenFlags as FrankenOpenFlags,
        OptionalExtension as FrankenOptionalExtension, ParamValue, RowExt as FrankenRowExt,
        Transaction as FrankenTransaction, TransactionExt as FrankenTransactionExt,
        open_with_flags as open_franken_with_flags, param_slice_to_values, params_from_iter,
    },
    migrate::MigrationRunner,
};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI8, AtomicI64, AtomicU64, Ordering},
};

/// Frankensqlite parameter list builder.
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
// Lazy FrankenSQLite Connection (bd-1ueu)
// -------------------------------------------------------------------------
// Defers opening the database until first use, cutting startup cost for
// commands that may not need the DB at all.  Thread-safe via parking_lot
// Mutex; logs the reason and duration of the open on first access.

/// Error from lazy database initialization.
#[derive(Debug, Error)]
pub enum LazyDbError {
    #[error("Database not found at {0}")]
    NotFound(PathBuf),
    #[error("Failed to open FrankenSQLite database at {path}: {source}")]
    FrankenOpenFailed {
        path: PathBuf,
        source: frankensqlite::FrankenError,
    },
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

    pub(crate) fn into_inner(self) -> FrankenConnection {
        self.0
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

static FRANKEN_RETRY_JITTER_STATE: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

fn next_franken_retry_jitter_ms(max_inclusive: u64) -> u64 {
    let mut value = FRANKEN_RETRY_JITTER_STATE.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^= value >> 31;
    value % max_inclusive.saturating_add(1)
}

/// Sleep with jittered exponential backoff to avoid lock-step retry storms
/// when many threads hit the same transient SQLite/frankensqlite contention.
pub(crate) fn sleep_with_franken_retry_backoff(
    backoff: &mut Duration,
    remaining: Duration,
    max_backoff: Duration,
) {
    let capped = (*backoff).min(remaining);
    let extra_budget = remaining.saturating_sub(capped).min(capped);
    let extra_ms = extra_budget.as_millis().min(u128::from(u64::MAX)) as u64;
    let sleep_for = if extra_ms == 0 {
        capped
    } else {
        capped
            .saturating_add(Duration::from_millis(next_franken_retry_jitter_ms(
                extra_ms,
            )))
            .min(remaining)
    };
    std::thread::sleep(sleep_for);
    *backoff = backoff.saturating_mul(2).min(max_backoff);
}

pub(crate) fn open_franken_storage_with_timeout(
    path: &Path,
    timeout: Duration,
) -> Result<FrankenStorage> {
    if !path.exists() {
        return Err(anyhow!("Database not found at {}", path.display()));
    }

    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_millis(4);
    loop {
        match FrankenStorage::open(path) {
            Ok(storage) => return Ok(storage),
            Err(err) if retryable_franken_anyhow(&err) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(err);
                }
                let remaining = deadline.saturating_duration_since(now);
                sleep_with_franken_retry_backoff(
                    &mut backoff,
                    remaining,
                    Duration::from_millis(128),
                );
            }
            Err(err) => return Err(err),
        }
    }
}

pub(crate) fn open_current_schema_storage_with_timeout(
    path: &Path,
    timeout: Duration,
) -> Result<Option<FrankenStorage>> {
    if !path.exists() {
        return Ok(None);
    }

    let mut storage = FrankenStorage::new(
        open_franken_raw_connection_with_timeout(path, timeout)?,
        path.to_path_buf(),
    );
    storage.apply_open_stage_busy_timeout();

    let version = storage
        .raw()
        .query("SELECT value FROM meta WHERE key = 'schema_version';")
        .ok()
        .and_then(|rows| rows.first().cloned())
        .and_then(|row| row.get_typed::<String>(0).ok())
        .and_then(|raw| raw.parse::<i64>().ok());

    if version != Some(CURRENT_SCHEMA_VERSION) {
        if let Err(close_err) = storage.close_without_checkpoint_in_place() {
            tracing::debug!(
                error = %close_err,
                db_path = %path.display(),
                "open_current_schema_storage_with_timeout: close_without_checkpoint_in_place failed; falling back to best-effort close"
            );
            storage.close_best_effort_in_place();
        }
        return Ok(None);
    }

    transition_from_meta_version(&storage.conn)?;
    storage.repair_missing_current_schema_objects()?;
    storage.apply_config()?;
    Ok(Some(storage))
}

pub(crate) fn open_franken_readonly_storage_with_timeout(
    path: &Path,
    timeout: Duration,
) -> Result<FrankenStorage> {
    if !path.exists() {
        return Err(anyhow!("Database not found at {}", path.display()));
    }

    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_millis(4);
    loop {
        match FrankenStorage::open_readonly(path) {
            Ok(storage) => return Ok(storage),
            Err(err) if retryable_franken_anyhow(&err) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(err);
                }
                let remaining = deadline.saturating_duration_since(now);
                sleep_with_franken_retry_backoff(
                    &mut backoff,
                    remaining,
                    Duration::from_millis(128),
                );
            }
            Err(err) => return Err(err),
        }
    }
}

pub(crate) fn open_franken_raw_connection_with_timeout(
    path: &Path,
    timeout: Duration,
) -> Result<FrankenConnection> {
    if !path.exists() {
        return Err(anyhow!("Database not found at {}", path.display()));
    }

    let path_str = path.to_string_lossy().to_string();
    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_millis(4);
    loop {
        match FrankenConnection::open(&path_str)
            .with_context(|| format!("opening raw frankensqlite db at {}", path.display()))
        {
            Ok(conn) => return Ok(conn),
            Err(err) if retryable_franken_anyhow(&err) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(err);
                }
                let remaining = deadline.saturating_duration_since(now);
                sleep_with_franken_retry_backoff(
                    &mut backoff,
                    remaining,
                    Duration::from_millis(128),
                );
            }
            Err(err) => return Err(err),
        }
    }
}

pub(crate) fn open_franken_raw_readonly_connection_with_timeout(
    path: &Path,
    timeout: Duration,
) -> Result<FrankenConnection> {
    if !path.exists() {
        return Err(anyhow!("Database not found at {}", path.display()));
    }

    let path_str = path.to_string_lossy().to_string();
    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_millis(4);
    loop {
        match open_franken_with_flags(&path_str, FrankenOpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| {
                format!(
                    "opening raw frankensqlite db readonly at {}",
                    path.display()
                )
            }) {
            Ok(conn) => return Ok(conn),
            Err(err) if retryable_franken_anyhow(&err) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(err);
                }
                let remaining = deadline.saturating_duration_since(now);
                sleep_with_franken_retry_backoff(
                    &mut backoff,
                    remaining,
                    Duration::from_millis(128),
                );
            }
            Err(err) => return Err(err),
        }
    }
}

pub(crate) fn retryable_franken_error(err: &frankensqlite::FrankenError) -> bool {
    let lower = err.to_string().to_ascii_lowercase();
    matches!(
        err,
        frankensqlite::FrankenError::Busy
            | frankensqlite::FrankenError::BusyRecovery
            | frankensqlite::FrankenError::BusySnapshot { .. }
            | frankensqlite::FrankenError::WriteConflict { .. }
            | frankensqlite::FrankenError::SerializationFailure { .. }
    ) || lower.contains("busy")
        || lower.contains("locked")
        || lower.contains("contention")
        || lower.contains("temporarily unavailable")
        || lower.contains("would block")
}

pub(crate) fn retryable_franken_anyhow(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<frankensqlite::FrankenError>()
            .is_some_and(retryable_franken_error)
            || {
                let lower = cause.to_string().to_ascii_lowercase();
                lower.contains("busy")
                    || lower.contains("locked")
                    || lower.contains("contention")
                    || lower.contains("temporarily unavailable")
                    || lower.contains("would block")
            }
    })
}

impl Drop for LazyFrankenDb {
    fn drop(&mut self) {
        let Some(mut conn) = self.conn.get_mut().take() else {
            return;
        };
        conn.0.close_best_effort_in_place();
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
            let _ = conn.execute("PRAGMA busy_timeout = 5000;"); // match writer config
            let _ = conn.execute("PRAGMA cache_size = -16384;"); // 16MB reader cache
            readers.push(parking_lot::Mutex::new(SendFrankenConnection(conn)));
        }

        let max_writers = config.max_writers.max(1);

        // Pre-fill bounded channel with tokens (acts as counting semaphore).
        // A zero-capacity channel with no initial tokens would make the first
        // writer acquisition block forever.
        let (tx, rx) = crossbeam_channel::bounded(max_writers);
        for _ in 0..max_writers {
            tx.send(())
                .map_err(|_| anyhow!("writer token channel closed during initialization"))?;
        }

        Ok(Self {
            db_path,
            readers,
            reader_idx: std::sync::atomic::AtomicUsize::new(0),
            writer_tokens: (tx, rx),
            config: ConnectionManagerConfig {
                reader_count,
                max_writers,
            },
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
        let conn = match FrankenConnection::open(&path_str) {
            Ok(c) => c,
            Err(e) => {
                let _ = self.writer_tokens.0.send(());
                return Err(anyhow::Error::from(e).context(format!(
                    "opening writer connection at {}",
                    self.db_path.display()
                )));
            }
        };
        let storage = FrankenStorage::new(conn, self.db_path.clone());
        if let Err(e) = storage.apply_config() {
            let _ = self.writer_tokens.0.send(());
            return Err(e);
        }
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
        let conn = match FrankenConnection::open(&path_str) {
            Ok(c) => c,
            Err(e) => {
                let _ = self.writer_tokens.0.send(());
                return Err(anyhow::Error::from(e).context(format!(
                    "opening concurrent writer at {}",
                    self.db_path.display()
                )));
            }
        };
        let storage = FrankenStorage::new(conn, self.db_path.clone());
        if let Err(e) = storage.apply_config() {
            let _ = self.writer_tokens.0.send(());
            return Err(e);
        }
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

impl Drop for FrankenConnectionManager {
    fn drop(&mut self) {
        for reader in &mut self.readers {
            reader.get_mut().0.close_best_effort_in_place();
        }
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
        self.storage.close_best_effort_in_place();
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
    Database(#[from] frankensqlite::FrankenError),

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

const FTS_FRANKEN_REBUILD_META_KEY: &str = "fts_frankensqlite_rebuild_generation";
const FTS_FRANKEN_REBUILD_FINGERPRINT_META_KEY: &str = "fts_frankensqlite_archive_fingerprint";
const FTS_FRANKEN_REBUILD_GENERATION: i64 = 1;
const DAILY_STATS_HEALTH_META_KEY: &str = "daily_stats_archive_fingerprint";
const DAILY_STATS_HEALTH_GENERATION_META_KEY: &str = "daily_stats_health_generation";
const DAILY_STATS_HEALTH_GENERATION: i64 = 1;

/// SQL to clear all rows from the contentless `fts_messages` table.
///
/// Contentless FTS5 tables reject ordinary `DELETE FROM ...` statements.
pub const FTS5_DELETE_ALL_SQL: &str =
    "INSERT INTO fts_messages(fts_messages) VALUES('delete-all');";

#[cfg(test)]
pub(crate) fn materialize_fresh_fts_schema_via_rusqlite(db_path: &Path) -> Result<()> {
    // Delegate to FrankenStorage: DROP TABLE IF EXISTS + CREATE VIRTUAL TABLE
    // is fully supported by the frankensqlite FTS5 path at
    // FrankenStorage::rebuild_fts_via_frankensqlite. We call rebuild which
    // also populates rows, matching the historical semantics ("fresh FTS"
    // means the schema exists and is consistent with message rows).
    let storage = FrankenStorage::open(db_path).with_context(|| {
        format!(
            "opening frankensqlite db at {} for FTS materialization",
            db_path.display()
        )
    })?;
    storage.rebuild_fts_via_frankensqlite().map(|_| ())
}

#[cfg(test)]
pub(crate) fn rebuild_fts_via_rusqlite(db_path: &Path) -> Result<usize> {
    let storage = FrankenStorage::open(db_path).with_context(|| {
        format!(
            "opening frankensqlite db at {} for FTS rebuild",
            db_path.display()
        )
    })?;
    let inserted = storage.rebuild_fts_via_frankensqlite()?;
    storage.record_fts_franken_rebuild_generation()?;
    Ok(inserted)
}

pub(crate) fn ensure_fts_consistency_via_rusqlite(db_path: &Path) -> Result<FtsConsistencyRepair> {
    // Delegates to the FrankenStorage-native path. The function name retains
    // the `_via_rusqlite` suffix only for backwards compatibility with the
    // few test-site callers; all operations now run through frankensqlite.
    let storage = FrankenStorage::open(db_path).with_context(|| {
        format!(
            "opening frankensqlite db at {} for FTS consistency check",
            db_path.display()
        )
    })?;
    storage.ensure_search_fallback_fts_consistency()
}

/// Create a uniquely named backup of the database file.
///
/// Returns the path to the backup file, or None if the source doesn't exist.
pub fn create_backup(db_path: &Path) -> Result<Option<std::path::PathBuf>, MigrationError> {
    if !db_path.exists() {
        return Ok(None);
    }

    let backup_path = unique_backup_path(db_path);

    // Try to use SQLite's VACUUM INTO command first, which safely handles WAL files
    // and produces a clean, minimized backup.
    let vacuum_success = open_franken_with_flags(
        &db_path.to_string_lossy(),
        FrankenOpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .and_then(|mut conn| {
        let path_str = backup_path.to_string_lossy();
        let result = conn.execute_compat("VACUUM INTO ?", fparams![path_str.as_ref()]);
        if let Err(close_err) = conn.close_in_place() {
            tracing::warn!(
                error = %close_err,
                db_path = %db_path.display(),
                "create_backup: close_in_place failed after VACUUM INTO; falling back to best-effort close"
            );
            conn.close_best_effort_in_place();
        }
        result
    })
    .is_ok();

    if vacuum_success {
        sync_file_if_exists(&backup_path)?;
        if let Some(parent) = backup_path.parent() {
            sync_parent_directory(parent)?;
        }
        return Ok(Some(backup_path));
    }

    // Fallback to filesystem copy if VACUUM INTO failed (e.g., older SQLite or corruption)
    // We strictly assume this is a single-user tool; if another process is writing,
    // this raw copy might be inconsistent, but it's better than nothing.
    fs::copy(db_path, &backup_path)?;
    sync_file_if_exists(&backup_path)?;

    // Best-effort copy of WAL/SHM sidecar files if they exist
    // SQLite sidecars are named: <path>-wal and <path>-shm
    let wal_src = database_sidecar_path(db_path, "-wal");
    let shm_src = database_sidecar_path(db_path, "-shm");

    if wal_src.exists() {
        let wal_backup = database_sidecar_path(&backup_path, "-wal");
        let _ = fs::copy(&wal_src, &wal_backup);
        let _ = sync_file_if_exists(&wal_backup);
    }
    if shm_src.exists() {
        let shm_backup = database_sidecar_path(&backup_path, "-shm");
        let _ = fs::copy(&shm_src, &shm_backup);
        let _ = sync_file_if_exists(&shm_backup);
    }
    if let Some(parent) = backup_path.parent() {
        sync_parent_directory(parent)?;
    }

    Ok(Some(backup_path))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DatabaseBundleMoveResult {
    pub database: bool,
    pub wal: bool,
    pub shm: bool,
}

impl DatabaseBundleMoveResult {
    pub fn moved_any(&self) -> bool {
        self.database || self.wal || self.shm
    }
}

fn database_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{}", path.to_string_lossy(), suffix))
}

/// Move a database file and its WAL/SHM sidecars to a new basename.
///
/// This is used for non-destructive quarantine of a corrupted bundle before a
/// rebuild. If the main database file is already missing but orphaned sidecars
/// remain, those sidecars are still moved so a fresh database can be created
/// without inheriting stale WAL state.
pub(crate) fn move_database_bundle(
    source_root: &Path,
    destination_root: &Path,
) -> std::io::Result<DatabaseBundleMoveResult> {
    let mut moved = DatabaseBundleMoveResult::default();
    if let Some(parent) = destination_root.parent() {
        fs::create_dir_all(parent)?;
        sync_parent_directory(parent)?;
    }

    if source_root.exists() {
        fs::rename(source_root, destination_root)?;
        moved.database = true;
    }

    let wal_source = database_sidecar_path(source_root, "-wal");
    if wal_source.exists() {
        fs::rename(&wal_source, database_sidecar_path(destination_root, "-wal"))?;
        moved.wal = true;
    }

    let shm_source = database_sidecar_path(source_root, "-shm");
    if shm_source.exists() {
        fs::rename(&shm_source, database_sidecar_path(destination_root, "-shm"))?;
        moved.shm = true;
    }

    if moved.moved_any() {
        if let Some(parent) = source_root.parent() {
            sync_parent_directory(parent)?;
        }
        if let Some(parent) = destination_root.parent() {
            sync_parent_directory(parent)?;
        }
    }

    Ok(moved)
}

fn copy_database_bundle(source_root: &Path, destination_root: &Path) -> Result<()> {
    if let Some(parent) = destination_root.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "creating destination directory for database bundle copy: {}",
                parent.display()
            )
        })?;
        sync_parent_directory(parent)
            .with_context(|| format!("syncing destination directory {}", parent.display()))?;
    }

    fs::copy(source_root, destination_root).with_context(|| {
        format!(
            "copying database bundle {} -> {}",
            source_root.display(),
            destination_root.display()
        )
    })?;
    sync_file_if_exists(destination_root).with_context(|| {
        format!(
            "syncing copied database bundle {}",
            destination_root.display()
        )
    })?;

    for suffix in ["-wal", "-shm"] {
        let source_sidecar = database_sidecar_path(source_root, suffix);
        if !source_sidecar.exists() {
            continue;
        }
        let destination_sidecar = database_sidecar_path(destination_root, suffix);
        fs::copy(&source_sidecar, &destination_sidecar).with_context(|| {
            format!(
                "copying database bundle sidecar {} -> {}",
                source_sidecar.display(),
                destination_sidecar.display()
            )
        })?;
        sync_file_if_exists(&destination_sidecar).with_context(|| {
            format!(
                "syncing copied database bundle sidecar {}",
                destination_sidecar.display()
            )
        })?;
    }

    if let Some(parent) = destination_root.parent() {
        sync_parent_directory(parent)
            .with_context(|| format!("syncing destination directory {}", parent.display()))?;
    }

    Ok(())
}

/// Helper to safely remove a database file and its potential WAL/SHM sidecars.
pub(crate) fn remove_database_files(path: &Path) -> std::io::Result<()> {
    let mut removed_any = false;

    match fs::remove_file(path) {
        Ok(()) => removed_any = true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    // Best-effort removal of sidecar files (ignore errors if they don't exist)
    for suffix in ["-wal", "-shm"] {
        match fs::remove_file(database_sidecar_path(path, suffix)) {
            Ok(()) => removed_any = true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }

    if removed_any && let Some(parent) = path.parent() {
        sync_parent_directory(parent)?;
    }

    Ok(())
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn sync_file_if_exists(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        fs::File::open(path)?.sync_all()?;
    }
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
                && is_backup_root_name(name, &prefix)
                && let Ok(meta) = fs::metadata(&path)
                && meta.is_file()
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
        let _ = fs::remove_file(database_sidecar_path(&path, "-wal"));
        let _ = fs::remove_file(database_sidecar_path(&path, "-shm"));
    }

    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct HistoricalDatabaseBundle {
    root_path: PathBuf,
    total_bytes: u64,
    modified_at_ms: i64,
    supports_direct_readonly: bool,
    probe: HistoricalBundleProbe,
}

#[derive(Debug, Clone, Copy, Default)]
struct HistoricalBundleProbe {
    schema_version: Option<i64>,
    fts_schema_rows: Option<i64>,
    fts_queryable: bool,
    max_message_id: i64,
}

#[cfg(test)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SqliteDatabaseHealthProbe {
    pub schema_version: Option<i64>,
    pub quick_check_ok: bool,
    pub fts_schema_rows: i64,
    pub fts_queryable: bool,
    pub message_count: i64,
    pub max_message_id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FtsConsistencyRepair {
    AlreadyHealthy {
        rows: usize,
    },
    IncrementalCatchUp {
        inserted_rows: usize,
        total_rows: usize,
    },
    Rebuilt {
        inserted_rows: usize,
    },
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HistoricalSalvageOutcome {
    pub bundles_considered: usize,
    pub bundles_imported: usize,
    pub conversations_imported: usize,
    pub messages_imported: usize,
}

impl HistoricalSalvageOutcome {
    pub(crate) fn accumulate(&mut self, other: Self) {
        self.bundles_considered += other.bundles_considered;
        self.bundles_imported += other.bundles_imported;
        self.conversations_imported += other.conversations_imported;
        self.messages_imported += other.messages_imported;
    }
}

#[derive(Debug)]
struct HistoricalReadConnection {
    conn: FrankenConnection,
    method: &'static str,
    _tempdir: Option<tempfile::TempDir>,
}

const HISTORICAL_RECOVERY_CORE_SCHEMA: &str = r"
CREATE TABLE sources (
    id TEXT PRIMARY KEY,
    kind TEXT,
    host_label TEXT,
    machine_id TEXT,
    platform TEXT,
    config_json TEXT,
    created_at INTEGER,
    updated_at INTEGER
);
CREATE TABLE agents (
    id INTEGER PRIMARY KEY,
    slug TEXT,
    name TEXT,
    version TEXT,
    kind TEXT,
    created_at INTEGER,
    updated_at INTEGER
);
CREATE TABLE workspaces (
    id INTEGER PRIMARY KEY,
    path TEXT,
    display_name TEXT
);
CREATE TABLE conversations (
    id INTEGER PRIMARY KEY,
    agent_id INTEGER,
    workspace_id INTEGER,
    source_id TEXT,
    external_id TEXT,
    title TEXT,
    source_path TEXT,
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
    assistant_message_count INTEGER,
    last_message_idx INTEGER,
    last_message_created_at INTEGER
);
CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    conversation_id INTEGER,
    idx INTEGER,
    role TEXT,
    author TEXT,
    created_at INTEGER,
    content TEXT,
    extra_json TEXT,
    extra_bin BLOB
);
CREATE TABLE snippets (
    id INTEGER PRIMARY KEY,
    message_id INTEGER,
    file_path TEXT,
    start_line INTEGER,
    end_line INTEGER,
    language TEXT,
    snippet_text TEXT
);
";
const HISTORICAL_SALVAGE_LEDGER_VERSION: u32 = 2;
const HISTORICAL_SALVAGE_PROGRESS_VERSION: u32 = 1;
const SOURCE_PATH_MERGE_START_TOLERANCE_MS: i64 = 5 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoricalBundleProgress {
    progress_version: u32,
    path: String,
    bytes: u64,
    modified_at_ms: i64,
    method: String,
    last_completed_source_row_id: i64,
    conversations_imported: usize,
    messages_imported: usize,
    updated_at_ms: i64,
}

#[derive(Debug, Clone)]
struct HistoricalBatchEntry {
    source_row_id: i64,
    agent_id: i64,
    workspace_id: Option<i64>,
    conversation: Conversation,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct HistoricalBatchImportTotals {
    inserted_source_rows: usize,
    inserted_messages: usize,
}

fn historical_bundle_root_paths(db_path: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let Some(parent) = db_path.parent() else {
        return roots;
    };
    let db_name = db_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("agent_search.db");
    let db_stem = db_path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("agent_search");

    let mut push_root = |path: PathBuf| {
        if path == db_path {
            return;
        }
        if !roots.iter().any(|existing| existing == &path) {
            roots.push(path);
        }
    };

    if let Ok(entries) = fs::read_dir(parent) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.ends_with("-wal") || name.ends_with("-shm") {
                continue;
            }
            if name.starts_with(&format!("{db_name}.backup."))
                || name.starts_with(&format!("{db_stem}.corrupt."))
            {
                push_root(path);
            }
        }
    }

    let backups_dir = parent.join("backups");
    if let Ok(entries) = fs::read_dir(backups_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.ends_with("-wal") || name.ends_with("-shm") {
                continue;
            }
            if name.starts_with(&format!("{db_name}.")) && name.ends_with(".bak") {
                push_root(path);
            }
        }
    }

    push_named_database_children(&mut roots, db_path, &parent.join("repair-lab"), db_name);
    push_named_database_children(&mut roots, db_path, &parent.join("snapshots"), db_name);

    roots
}

fn push_named_database_children(
    roots: &mut Vec<PathBuf>,
    canonical_db_path: &Path,
    dir: &Path,
    db_name: &str,
) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let candidate = entry.path().join(db_name);
            if candidate == canonical_db_path {
                continue;
            }
            if candidate.exists() && !roots.iter().any(|existing| existing == &candidate) {
                roots.push(candidate);
            }
        }
    }
}

fn file_mtime_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|ts| ts.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn bundle_total_bytes(root_path: &Path) -> u64 {
    let mut total = fs::metadata(root_path).map(|meta| meta.len()).unwrap_or(0);
    for suffix in ["-wal", "-shm"] {
        let sidecar = database_sidecar_path(root_path, suffix);
        total = total.saturating_add(fs::metadata(sidecar).map(|meta| meta.len()).unwrap_or(0));
    }
    total
}

pub(crate) fn discover_historical_database_bundles(
    db_path: &Path,
) -> Vec<HistoricalDatabaseBundle> {
    let mut bundles: Vec<_> = historical_bundle_root_paths(db_path)
        .into_iter()
        .filter(|root| root.exists())
        .map(|root_path| {
            let modified_at_ms = file_mtime_ms(&root_path);
            let total_bytes = bundle_total_bytes(&root_path);
            let supports_direct_readonly = historical_bundle_supports_direct_readonly(&root_path);
            let probe = probe_historical_bundle(&root_path, supports_direct_readonly);
            HistoricalDatabaseBundle {
                modified_at_ms,
                total_bytes,
                supports_direct_readonly,
                root_path,
                probe,
            }
        })
        .filter(|bundle| bundle.total_bytes > 0)
        .collect();

    fn bundle_priority(path: &Path) -> i32 {
        let path_str = path.to_string_lossy();
        if path_str.contains("/repair-lab/replay-") {
            return 5;
        }
        if path_str.contains("/repair-lab/") {
            return 4;
        }
        if path_str.contains("/snapshots/") {
            return 3;
        }
        if path_str.contains(".corrupt.") || path_str.contains("failed-baseline-seed") {
            return 0;
        }
        1
    }

    fn bundle_health_rank(bundle: &HistoricalDatabaseBundle) -> i32 {
        // Classify FTS health. The probe only sets `fts_queryable = true`
        // when `fts_schema_rows == Some(1)` (see
        // `historical_bundle_fts_queryable_via_frankensqlite`), so we have
        // two legitimate "clean" shapes for a bundle:
        //
        //   * `fts_schema_rows == Some(1) && fts_queryable` — a pre-V14
        //     bundle where the FTS virtual table was eagerly created by
        //     migration and is queryable right now.
        //
        //   * `fts_schema_rows == Some(0) && schema_version == Some(V14+)` —
        //     a modern bundle where `MIGRATION_V14` dropped fts_messages on
        //     purpose and cass recreates it lazily via
        //     `ensure_search_fallback_fts_consistency` on the first open.
        //     Gating on `schema_version == CURRENT_SCHEMA_VERSION` is critical:
        //     `MIGRATION_FRESH_SCHEMA` (V13) creates fts_messages eagerly,
        //     so a schema-13 bundle with 0 fts rows is a symptom of a
        //     crashed / tampered migration — not a legitimate lazy-FTS
        //     state — and must not be promoted alongside real lazy-V14
        //     bundles. A `None` schema_version (schema marker unreadable)
        //     is excluded for the same reason.
        //
        // Everything else — `Some(1)` without queryability, `Some(n)` for
        // n >= 2 (duplicated CREATE VIRTUAL TABLE rows from a broken legacy
        // rebuild), `None` entirely, or `Some(0)` on a non-current schema —
        // is not "fts clean".
        let fts_clean = match bundle.probe.fts_schema_rows {
            Some(1) => bundle.probe.fts_queryable,
            Some(0) => bundle.probe.schema_version == Some(CURRENT_SCHEMA_VERSION),
            _ => false,
        };

        let clean_schema14_fts =
            bundle.probe.schema_version == Some(CURRENT_SCHEMA_VERSION) && fts_clean;
        if clean_schema14_fts {
            return 5;
        }

        if fts_clean {
            return 4;
        }

        if bundle.probe.schema_version == Some(CURRENT_SCHEMA_VERSION)
            && bundle.supports_direct_readonly
        {
            return 3;
        }

        if bundle.supports_direct_readonly {
            return 2;
        }

        1
    }

    bundles.sort_by(|left, right| {
        bundle_health_rank(right)
            .cmp(&bundle_health_rank(left))
            .then_with(|| right.probe.max_message_id.cmp(&left.probe.max_message_id))
            .then_with(|| bundle_priority(&right.root_path).cmp(&bundle_priority(&left.root_path)))
            .then_with(|| {
                right
                    .supports_direct_readonly
                    .cmp(&left.supports_direct_readonly)
            })
            .then_with(|| right.total_bytes.cmp(&left.total_bytes))
            .then_with(|| right.modified_at_ms.cmp(&left.modified_at_ms))
            .then_with(|| right.root_path.cmp(&left.root_path))
    });
    bundles
}

fn probe_historical_bundle(
    root_path: &Path,
    supports_direct_readonly: bool,
) -> HistoricalBundleProbe {
    if !supports_direct_readonly {
        return HistoricalBundleProbe::default();
    }

    let Ok(conn) = open_historical_bundle_readonly(root_path) else {
        return HistoricalBundleProbe::default();
    };

    let schema_version = read_meta_schema_version(&conn).ok().flatten();
    let fts_schema_rows: Option<i64> = conn
        .query_row_map(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fts_messages'",
            fparams![],
            |row| row.get_typed(0),
        )
        .ok();
    let fts_queryable =
        historical_bundle_fts_queryable_via_frankensqlite(root_path, fts_schema_rows);
    let max_message_id: i64 = conn
        .query_row_map(
            "SELECT COALESCE(MAX(id), 0) FROM messages",
            fparams![],
            |row| row.get_typed(0),
        )
        .unwrap_or(0);

    HistoricalBundleProbe {
        schema_version,
        fts_schema_rows,
        fts_queryable,
        max_message_id,
    }
}

fn historical_bundle_fts_queryable_via_frankensqlite(
    root_path: &Path,
    fts_schema_rows: Option<i64>,
) -> bool {
    matches!(fts_schema_rows, Some(1))
        && FrankenStorage::open_readonly(root_path)
            .map(|storage| {
                storage
                    .raw()
                    .query("SELECT rowid FROM fts_messages LIMIT 1")
                    .is_ok()
            })
            .unwrap_or(false)
}

fn historical_bundle_supports_direct_readonly(root_path: &Path) -> bool {
    open_historical_bundle_readonly(root_path)
        .and_then(|conn| historical_bundle_has_queryable_core_tables(&conn))
        .is_ok()
}

fn historical_table_exists(conn: &FrankenConnection, table: &str) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row_map(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1",
            fparams![table],
            |row| row.get_typed(0),
        )
        .optional()
        .with_context(|| format!("checking for historical table {table}"))?;
    Ok(found.is_some())
}

fn probe_historical_table_reads(conn: &FrankenConnection, table: &str) -> Result<()> {
    if !historical_table_exists(conn, table)? {
        return Err(anyhow!(
            "historical database missing required table {table}"
        ));
    }

    let sql = format!("SELECT rowid FROM {table} LIMIT 1");
    let _: Option<i64> = conn
        .query_row_map(&sql, fparams![], |row| row.get_typed(0))
        .optional()
        .with_context(|| format!("probing rows from historical table {table}"))?;
    Ok(())
}

fn historical_bundle_has_queryable_core_tables(conn: &FrankenConnection) -> Result<()> {
    probe_historical_table_reads(conn, "conversations")?;
    probe_historical_table_reads(conn, "messages")?;
    Ok(())
}

fn open_historical_bundle_readonly(root_path: &Path) -> Result<FrankenConnection> {
    let path_str = root_path.to_string_lossy();
    let flags = FrankenOpenFlags::SQLITE_OPEN_READ_ONLY;
    let conn = open_franken_with_flags(&path_str, flags)
        .with_context(|| format!("opening historical database {}", root_path.display()))?;
    Ok(conn)
}

fn is_recoverable_insert_line(line: &str) -> bool {
    [
        "sources",
        "agents",
        "workspaces",
        "conversations",
        "messages",
        "snippets",
    ]
    .iter()
    .any(|table| {
        line.starts_with(&format!("INSERT INTO '{table}'"))
            || line.starts_with(&format!("INSERT OR IGNORE INTO '{table}'"))
            || line.starts_with(&format!("INSERT INTO \"{table}\""))
            || line.starts_with(&format!("INSERT OR IGNORE INTO \"{table}\""))
    })
}

fn recover_historical_bundle_via_sqlite3(
    bundle: &HistoricalDatabaseBundle,
) -> Result<HistoricalReadConnection> {
    let tempdir = tempfile::TempDir::new().context("creating temporary salvage directory")?;
    let recovered_db = tempdir.path().join("historical-recovered.db");
    let temp_conn = FrankenConnection::open(recovered_db.to_string_lossy().as_ref())
        .with_context(|| format!("creating recovered database {}", recovered_db.display()))?;
    temp_conn
        .execute_batch(HISTORICAL_RECOVERY_CORE_SCHEMA)
        .with_context(|| format!("initializing recovered schema {}", recovered_db.display()))?;
    drop(temp_conn);

    let bundle_uri = format!("file:{}?immutable=1", bundle.root_path.to_string_lossy());
    let mut recover = Command::new("sqlite3")
        .arg(&bundle_uri)
        .arg(".recover")
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "launching sqlite3 .recover for historical bundle {}",
                bundle.root_path.display()
            )
        })?;
    let recover_stdout = recover
        .stdout
        .take()
        .context("capturing sqlite3 .recover stdout")?;

    let mut importer = Command::new("sqlite3")
        .arg(&recovered_db)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "launching sqlite3 importer for recovered bundle {}",
                recovered_db.display()
            )
        })?;

    {
        let importer_stdin = importer
            .stdin
            .as_mut()
            .context("opening sqlite3 importer stdin")?;
        importer_stdin
            .write_all(b"BEGIN;\n")
            .context("starting recovery import transaction")?;

        let reader = BufReader::new(recover_stdout);
        for line in reader.lines() {
            let line = line.context("reading sqlite3 .recover output")?;
            if is_recoverable_insert_line(&line) {
                importer_stdin
                    .write_all(line.as_bytes())
                    .context("writing recovered INSERT")?;
                importer_stdin
                    .write_all(b"\n")
                    .context("writing recovered INSERT newline")?;
            }
        }

        importer_stdin
            .write_all(b"COMMIT;\n")
            .context("committing recovery import transaction")?;
    }

    let recover_status = recover
        .wait()
        .context("waiting for sqlite3 .recover process")?;
    if !recover_status.success() {
        anyhow::bail!(
            "sqlite3 .recover exited with status {} for {}",
            recover_status,
            bundle.root_path.display()
        );
    }

    let importer_status = importer
        .wait()
        .context("waiting for sqlite3 recovery importer")?;
    if !importer_status.success() {
        anyhow::bail!(
            "sqlite3 recovery importer exited with status {} for {}",
            importer_status,
            recovered_db.display()
        );
    }

    let conn = open_historical_bundle_readonly(&recovered_db)?;
    historical_bundle_has_queryable_core_tables(&conn)?;
    Ok(HistoricalReadConnection {
        conn,
        method: "sqlite3-recover",
        _tempdir: Some(tempdir),
    })
}

fn open_historical_bundle_for_salvage(
    bundle: &HistoricalDatabaseBundle,
) -> Result<HistoricalReadConnection> {
    match open_historical_bundle_readonly(&bundle.root_path) {
        Ok(conn) => {
            if historical_bundle_has_queryable_core_tables(&conn).is_ok() {
                return Ok(HistoricalReadConnection {
                    conn,
                    method: "direct-readonly",
                    _tempdir: None,
                });
            }
        }
        Err(err) => {
            tracing::warn!(
                path = %bundle.root_path.display(),
                error = %err,
                "historical bundle direct open failed; falling back to sqlite3 .recover"
            );
        }
    }

    recover_historical_bundle_via_sqlite3(bundle)
}

fn historical_bundle_counts(conn: &FrankenConnection) -> Result<(usize, usize)> {
    let conversations: i64 =
        conn.query_row_map("SELECT COUNT(*) FROM conversations", fparams![], |row| {
            row.get_typed(0)
        })?;
    let messages: i64 = conn.query_row_map("SELECT COUNT(*) FROM messages", fparams![], |row| {
        row.get_typed(0)
    })?;
    Ok((
        usize::try_from(conversations.max(0)).unwrap_or(usize::MAX),
        usize::try_from(messages.max(0)).unwrap_or(usize::MAX),
    ))
}

fn clear_seeded_runtime_meta(conn: &FrankenConnection) -> Result<()> {
    conn.execute(
        "DELETE FROM meta
         WHERE key LIKE 'historical_bundle_salvaged:%'
            OR key IN ('last_scan_ts', 'last_indexed_at', 'last_embedded_message_id')",
    )?;
    Ok(())
}

fn record_historical_bundle_import(
    conn: &FrankenConnection,
    bundle: &HistoricalDatabaseBundle,
    method: &str,
    conversations_imported: usize,
    messages_imported: usize,
) -> Result<()> {
    let key = FrankenStorage::historical_bundle_meta_key(bundle);
    let value = serde_json::json!({
        "salvage_version": HISTORICAL_SALVAGE_LEDGER_VERSION,
        "path": bundle.root_path.display().to_string(),
        "bytes": bundle.total_bytes,
        "modified_at_ms": bundle.modified_at_ms,
        "method": method,
        "conversations_imported": conversations_imported,
        "messages_imported": messages_imported,
        "recorded_at_ms": FrankenStorage::now_millis(),
    });
    let value_str = serde_json::to_string(&value)?;
    conn.execute_compat(
        "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
        fparams![key, value_str],
    )?;
    Ok(())
}

fn finalize_seeded_canonical_bundle_via_rusqlite(
    canonical_db_path: &Path,
    bundle: &HistoricalDatabaseBundle,
    conversations_imported: usize,
    messages_imported: usize,
) -> Result<()> {
    let _fts_repair =
        ensure_fts_consistency_via_rusqlite(canonical_db_path).with_context(|| {
            format!(
                "repairing staged canonical FTS consistency before finalization: {}",
                canonical_db_path.display()
            )
        })?;

    let path_str = canonical_db_path.to_string_lossy();
    let conn = FrankenConnection::open(path_str.as_ref()).with_context(|| {
        format!(
            "opening seeded canonical database for post-seed finalization: {}",
            canonical_db_path.display()
        )
    })?;
    conn.execute("PRAGMA busy_timeout = 30000;")
        .with_context(|| {
            format!(
                "configuring busy timeout for seeded canonical database {}",
                canonical_db_path.display()
            )
        })?;
    let schema_version = read_meta_schema_version(&conn)?;

    if let Some(version) = schema_version
        && version < CURRENT_SCHEMA_VERSION
        && version != 13
    {
        anyhow::bail!(
            "seeded canonical bundle schema_version {version} is too old for baseline import and cannot be finalized automatically"
        );
    }

    clear_seeded_runtime_meta(&conn)?;

    conn.execute_compat(
        "INSERT OR REPLACE INTO meta(key, value) VALUES('schema_version', ?1)",
        fparams![CURRENT_SCHEMA_VERSION.to_string()],
    )?;

    conn.execute_compat(
        "INSERT OR IGNORE INTO _schema_migrations(version, name) VALUES(?1, 'fts_contentless')",
        fparams![CURRENT_SCHEMA_VERSION],
    )?;
    record_historical_bundle_import(
        &conn,
        bundle,
        "baseline-bulk-sql-copy",
        conversations_imported,
        messages_imported,
    )?;
    Ok(())
}

fn read_meta_schema_version(conn: &FrankenConnection) -> Result<Option<i64>> {
    let version: Option<String> = conn
        .query_row_map(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            fparams![],
            |row| row.get_typed(0),
        )
        .optional()?;
    Ok(version.and_then(|raw| raw.parse::<i64>().ok()))
}

#[cfg(test)]
fn franken_fts_schema_rows(conn: &FrankenConnection) -> Result<i64> {
    conn.query_row_map(
        "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fts_messages'",
        fparams![],
        |row| row.get_typed(0),
    )
    .context("counting sqlite_master rows for fts_messages via frankensqlite")
}

#[cfg(test)]
fn franken_fts_limit_probe(conn: &FrankenConnection) -> bool {
    conn.query("SELECT rowid FROM fts_messages LIMIT 1").is_ok()
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn probe_database_health_via_frankensqlite(
    db_path: &Path,
) -> Result<SqliteDatabaseHealthProbe> {
    let path_str = db_path.to_string_lossy();
    let conn = FrankenConnection::open(path_str.as_ref()).with_context(|| {
        format!(
            "opening frankensqlite db at {} for database health probe",
            db_path.display()
        )
    })?;
    conn.execute_batch("PRAGMA busy_timeout = 30000;")
        .with_context(|| {
            format!(
                "configuring busy timeout for database health probe at {}",
                db_path.display()
            )
        })?;

    let schema_version = read_meta_schema_version(&conn)?;
    let quick_check_status: String = conn
        .query_row_map("PRAGMA quick_check(1)", fparams![], |row| row.get_typed(0))
        .with_context(|| format!("running PRAGMA quick_check(1) for {}", db_path.display()))?;
    let quick_check_ok = quick_check_status.trim().eq_ignore_ascii_case("ok");
    let fts_schema_rows = franken_fts_schema_rows(&conn)?;
    let fts_queryable = fts_schema_rows == 1 && franken_fts_limit_probe(&conn);

    if !quick_check_ok {
        return Ok(SqliteDatabaseHealthProbe {
            schema_version,
            quick_check_ok,
            fts_schema_rows,
            fts_queryable,
            message_count: 0,
            max_message_id: 0,
        });
    }

    let message_count: i64 = conn
        .query_row_map("SELECT COUNT(*) FROM messages", fparams![], |row| {
            row.get_typed(0)
        })
        .context("counting messages during frankensqlite database health probe")?;
    let max_message_id: i64 = conn
        .query_row_map(
            "SELECT COALESCE(MAX(id), 0) FROM messages",
            fparams![],
            |row| row.get_typed(0),
        )
        .context("reading max message id during frankensqlite database health probe")?;

    Ok(SqliteDatabaseHealthProbe {
        schema_version,
        quick_check_ok,
        fts_schema_rows,
        fts_queryable,
        message_count,
        max_message_id,
    })
}

struct StagedHistoricalSeed {
    tempdir: tempfile::TempDir,
    db_path: PathBuf,
}

fn stage_historical_bundle_for_seed(
    canonical_db_path: &Path,
    bundle: &HistoricalDatabaseBundle,
) -> Result<StagedHistoricalSeed> {
    let canonical_parent = canonical_db_path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(canonical_parent).with_context(|| {
        format!(
            "creating canonical database directory before bulk historical seed import: {}",
            canonical_parent.display()
        )
    })?;
    let tempdir = tempfile::TempDir::new_in(canonical_parent)
        .context("creating temporary baseline seed directory")?;
    let staged_seed_db = tempdir.path().join("baseline-seed-output.db");
    copy_database_bundle(&bundle.root_path, &staged_seed_db)?;

    Ok(StagedHistoricalSeed {
        tempdir,
        db_path: staged_seed_db,
    })
}

fn promote_staged_historical_seed(
    canonical_db_path: &Path,
    staged_seed: &StagedHistoricalSeed,
) -> Result<()> {
    let canonical_backup = staged_seed
        .tempdir
        .path()
        .join("pre-seed-canonical-backup.db");
    let had_canonical = canonical_db_path.exists()
        || database_sidecar_path(canonical_db_path, "-wal").exists()
        || database_sidecar_path(canonical_db_path, "-shm").exists();

    if had_canonical {
        move_database_bundle(canonical_db_path, &canonical_backup).with_context(|| {
            format!(
                "backing up canonical database before promoting staged historical seed import: {}",
                canonical_db_path.display()
            )
        })?;
    }

    if let Err(err) =
        move_database_bundle(&staged_seed.db_path, canonical_db_path).with_context(|| {
            format!(
                "promoting staged historical seed database bundle {} into canonical path {}",
                staged_seed.db_path.display(),
                canonical_db_path.display()
            )
        })
    {
        if had_canonical {
            let _ = move_database_bundle(&canonical_backup, canonical_db_path);
        }
        return Err(err);
    }

    Ok(())
}

pub(crate) fn seed_canonical_from_best_historical_bundle(
    canonical_db_path: &Path,
) -> Result<Option<HistoricalSalvageOutcome>> {
    let ordered_bundles = discover_historical_database_bundles(canonical_db_path);
    let mut last_seed_error: Option<anyhow::Error> = None;
    for bundle in ordered_bundles
        .into_iter()
        .filter(|bundle| bundle.supports_direct_readonly)
    {
        if let Some(version) = bundle.probe.schema_version
            && version < 13
        {
            let err = anyhow!(
                "historical bundle {} schema_version {version} is too old for baseline import",
                bundle.root_path.display()
            );
            tracing::warn!(
                path = %bundle.root_path.display(),
                schema_version = version,
                "historical bundle is too old for baseline seed import"
            );
            last_seed_error = Some(err);
            continue;
        }

        let source = open_historical_bundle_for_salvage(&bundle).with_context(|| {
            format!(
                "opening historical seed bundle {} for baseline import",
                bundle.root_path.display()
            )
        })?;
        let (conversations_imported, messages_imported) = historical_bundle_counts(&source.conn)?;

        let staged_seed = match stage_historical_bundle_for_seed(canonical_db_path, &bundle) {
            Ok(staged_seed) => staged_seed,
            Err(err) => {
                tracing::warn!(
                    path = %bundle.root_path.display(),
                    error = %err,
                    "bulk baseline seed staging from historical bundle failed; trying next candidate"
                );
                last_seed_error = Some(err);
                continue;
            }
        };

        if let Err(err) = finalize_seeded_canonical_bundle_via_rusqlite(
            &staged_seed.db_path,
            &bundle,
            conversations_imported,
            messages_imported,
        ) {
            tracing::warn!(
                path = %bundle.root_path.display(),
                error = %err,
                "finalizing staged historical seed import failed; trying next candidate"
            );
            last_seed_error = Some(err);
            continue;
        }

        if let Err(err) = promote_staged_historical_seed(canonical_db_path, &staged_seed) {
            tracing::warn!(
                path = %bundle.root_path.display(),
                error = %err,
                "promoting staged historical seed import failed; trying next candidate"
            );
            last_seed_error = Some(err);
            continue;
        }

        tracing::info!(
            path = %bundle.root_path.display(),
            conversations_imported,
            messages_imported,
            "seeded empty canonical database from largest healthy historical bundle"
        );

        return Ok(Some(HistoricalSalvageOutcome {
            bundles_considered: 0,
            bundles_imported: 1,
            conversations_imported,
            messages_imported,
        }));
    }
    if let Some(err) = last_seed_error {
        return Err(err);
    }
    Ok(None)
}

fn parse_json_column(value: Option<String>) -> serde_json::Value {
    value
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(serde_json::Value::Null)
}

const HISTORICAL_RAW_JSON_SENTINEL_KEY: &str = "__cass_historical_raw_json__";

fn wrap_historical_raw_json(raw: String) -> serde_json::Value {
    serde_json::json!({ HISTORICAL_RAW_JSON_SENTINEL_KEY: raw })
}

fn historical_raw_json(value: &serde_json::Value) -> Option<&str> {
    match value {
        serde_json::Value::Object(map) if map.len() == 1 => map
            .get(HISTORICAL_RAW_JSON_SENTINEL_KEY)
            .and_then(serde_json::Value::as_str),
        _ => None,
    }
}

fn parse_historical_json_column(value: Option<String>) -> serde_json::Value {
    match value {
        Some(raw) if raw.trim().is_empty() => serde_json::Value::Null,
        Some(raw) => wrap_historical_raw_json(raw),
        None => serde_json::Value::Null,
    }
}

fn historical_salvage_debug_enabled() -> bool {
    std::env::var_os("CASS_DEBUG_HISTORICAL_SALVAGE").is_some()
}

#[derive(Debug, Clone, Copy)]
struct HistoricalImportBatchLimits {
    conversations: usize,
    messages: usize,
    payload_chars: usize,
}

fn env_positive_usize(key: &str) -> Option<usize> {
    dotenvy::var(key)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
}

fn historical_import_batch_limits() -> HistoricalImportBatchLimits {
    let cpu_count = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);

    let default_limits = if cpu_count >= 32 {
        HistoricalImportBatchLimits {
            conversations: 128,
            messages: 16_384,
            payload_chars: 12_000_000,
        }
    } else {
        HistoricalImportBatchLimits {
            conversations: 32,
            messages: 4_096,
            payload_chars: 3_000_000,
        }
    };

    HistoricalImportBatchLimits {
        conversations: env_positive_usize("CASS_HISTORICAL_IMPORT_BATCH_CONVERSATIONS")
            .unwrap_or(default_limits.conversations),
        messages: env_positive_usize("CASS_HISTORICAL_IMPORT_BATCH_MESSAGES")
            .unwrap_or(default_limits.messages),
        payload_chars: env_positive_usize("CASS_HISTORICAL_IMPORT_BATCH_CHARS")
            .unwrap_or(default_limits.payload_chars),
    }
}

fn json_value_size_hint(value: &serde_json::Value) -> usize {
    if let Some(raw) = historical_raw_json(value) {
        return raw.len();
    }
    match value {
        serde_json::Value::Null => 0,
        other => serde_json::to_string(other)
            .map(|raw| raw.len())
            .unwrap_or(0),
    }
}

fn message_payload_size_hint(message: &Message) -> usize {
    message
        .content
        .len()
        .saturating_add(json_value_size_hint(&message.extra_json))
}

fn is_backup_root_name(name: &str, prefix: &str) -> bool {
    name.starts_with(prefix) && !name.ends_with("-wal") && !name.ends_with("-shm")
}

/// Public schema version constant for external checks.
pub const CURRENT_SCHEMA_VERSION: i64 = 15;
const MIN_IN_PLACE_MIGRATION_SCHEMA_VERSION: i64 = 13;

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

fn schema_check_error_requires_rebuild(err: &frankensqlite::FrankenError) -> bool {
    // Only on-disk corruption classes justify destructive rebuild.
    // Locking, open, and generic I/O failures are often transient and must
    // surface as errors rather than deleting the database under the caller.
    matches!(
        err,
        frankensqlite::FrankenError::DatabaseCorrupt { .. }
            | frankensqlite::FrankenError::WalCorrupt { .. }
            | frankensqlite::FrankenError::NotADatabase { .. }
            | frankensqlite::FrankenError::ShortRead { .. }
    )
}

fn unique_backup_path(path: &Path) -> PathBuf {
    static NEXT_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let nonce = NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("db");

    path.with_file_name(format!(
        "{file_name}.backup.{}.{}.{}",
        std::process::id(),
        timestamp,
        nonce
    ))
}

/// Check schema compatibility without modifying the database.
///
/// Opens the database read-only and checks the schema version.
fn check_schema_compatibility(
    path: &Path,
) -> std::result::Result<SchemaCheck, frankensqlite::FrankenError> {
    let mut conn = open_franken_with_flags(
        &path.to_string_lossy(),
        FrankenOpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;

    let result = (|| {
        // Check if meta table exists
        let meta_exists: i32 = conn.query_row_map(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='meta'",
            fparams![],
            |row| row.get_typed(0),
        )?;

        if meta_exists == 0 {
            // No meta table - could be empty or very old schema, needs rebuild
            // But first check if there are any tables at all
            let table_count: i32 = conn.query_row_map(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
                fparams![],
                |row| row.get_typed(0),
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
            .query_row_map(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                fparams![],
                |row| Ok(row.get_typed::<String>(0)?.parse().ok()),
            )
            .ok()
            .flatten();

        match version {
            Some(v) if v == SCHEMA_VERSION => Ok(SchemaCheck::Compatible),
            Some(v) if (MIN_IN_PLACE_MIGRATION_SCHEMA_VERSION..SCHEMA_VERSION).contains(&v) => {
                Ok(SchemaCheck::NeedsMigration)
            }
            Some(v) if v > 0 && v < MIN_IN_PLACE_MIGRATION_SCHEMA_VERSION => {
                Ok(SchemaCheck::NeedsRebuild(format!(
                    "Schema version {} is too old for in-place migration; supported upgrade path starts at version {}",
                    v, MIN_IN_PLACE_MIGRATION_SCHEMA_VERSION
                )))
            }
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
    })();

    if let Err(close_err) = conn.close_in_place() {
        tracing::warn!(
            error = %close_err,
            db_path = %path.display(),
            "check_schema_compatibility: close_in_place failed; falling back to best-effort close"
        );
        conn.close_best_effort_in_place();
    }

    result
}

const SCHEMA_VERSION: i64 = CURRENT_SCHEMA_VERSION;

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
#[allow(dead_code)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
const MIGRATION_V6: &str = r"
-- Optimize lookup by source_path (used by TUI detail view)
CREATE INDEX IF NOT EXISTS idx_conversations_source_path ON conversations(source_path);
";

#[cfg(test)]
const MIGRATION_V7: &str = r"
-- Add binary columns for MessagePack serialization (Opt 3.1)
-- Binary format is 50-70% smaller than JSON and faster to parse
ALTER TABLE conversations ADD COLUMN metadata_bin BLOB;
ALTER TABLE messages ADD COLUMN extra_bin BLOB;
";

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

const MIGRATION_V14: &str = r"
-- Switch FTS5 from internal-content to contentless mode (CASS #163).
-- Drop the old V13 internal-content fts_messages first so that
-- sqlite_schema does not contain two conflicting CREATE VIRTUAL TABLE
-- entries, which makes the database completely unreadable.
-- The current contentless table is recreated lazily after open() only when the
-- frankensqlite FTS consistency check finds it missing or malformed.
DROP TABLE IF EXISTS fts_messages;
";

const MIGRATION_V15: &str = r"
-- Cache append-tail state on the conversation row so append-only merges do not
-- have to rescan messages for MAX(idx)/MAX(created_at) on every call.
ALTER TABLE conversations ADD COLUMN last_message_idx INTEGER;
ALTER TABLE conversations ADD COLUMN last_message_created_at INTEGER;

UPDATE conversations
SET last_message_idx = (
        SELECT MAX(m.idx)
        FROM messages m
        WHERE m.conversation_id = conversations.id
    ),
    last_message_created_at = (
        SELECT MAX(m.created_at)
        FROM messages m
        WHERE m.conversation_id = conversations.id
    );
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

/// Lightweight conversation projection used while rebuilding the lexical index.
///
/// This intentionally omits `metadata_json` / `metadata_bin` and other bulky
/// fields because Tantivy only needs the stable envelope plus provenance
/// identifiers. Reading full metadata here can force frankensqlite to traverse
/// large overflow chains before the first lexical checkpoint is committed.
#[derive(Debug, Clone)]
pub struct LexicalRebuildConversationRow {
    pub id: Option<i64>,
    pub agent_slug: String,
    pub workspace: Option<PathBuf>,
    pub external_id: Option<String>,
    pub title: Option<String>,
    pub source_path: PathBuf,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub source_id: String,
    pub origin_host: Option<String>,
}

/// Lightweight per-conversation footprint used to pre-plan lexical rebuild
/// shard boundaries without re-reading full message bodies in the hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LexicalRebuildConversationFootprintRow {
    pub conversation_id: i64,
    pub message_count: usize,
    pub message_bytes: usize,
}

/// Lightweight message projection used by the streaming lexical rebuild path.
#[derive(Debug, Clone)]
pub struct LexicalRebuildMessageRow {
    pub conversation_id: i64,
    pub id: i64,
    pub idx: i64,
    pub role: String,
    pub author: Option<String>,
    pub created_at: Option<i64>,
    pub content: String,
}

/// Even lighter message projection used only by the grouped lexical rebuild
/// stream hot path. It keeps just the per-message fields the rebuild consumes
/// and tracks the final message id at conversation scope instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexicalRebuildGroupedMessageRow {
    pub idx: i64,
    pub is_tool_role: bool,
    pub created_at: Option<i64>,
    pub content: String,
}

pub type LexicalRebuildGroupedMessageRows = SmallVec<[LexicalRebuildGroupedMessageRow; 32]>;

/// Compatibility alias retained while call sites finish converging on `FrankenStorage`.
pub type SqliteStorage = FrankenStorage;

/// Primary frankensqlite-backed storage backend.
pub struct FrankenStorage {
    conn: FrankenConnection,
    db_path: PathBuf,
    ephemeral_writer_preflight_verified: AtomicBool,
    index_writer_checkpoint_pages: AtomicI64,
    cached_ephemeral_writer: parking_lot::Mutex<CachedEphemeralWriter>,
    ensured_conversation_sources: Arc<parking_lot::Mutex<HashSet<EnsuredConversationSourceKey>>>,
    ensured_daily_stats_keys: Arc<parking_lot::Mutex<HashSet<EnsuredDailyStatsKey>>>,
    fts_messages_present_cache: AtomicI8,
}

/// Keep ordinary storage commits from tripping over frequent auto-checkpoints
/// while still bounding WAL growth. Bulk index paths may override this through
/// their explicit checkpoint policy.
const DEFAULT_WAL_AUTOCHECKPOINT_PAGES: i64 = 4096;
const UNSET_INDEX_WRITER_CHECKPOINT_PAGES: i64 = i64::MIN;
const FTS_MESSAGES_PRESENT_UNKNOWN: i8 = 0;
const FTS_MESSAGES_PRESENT_ABSENT: i8 = 1;
const FTS_MESSAGES_PRESENT_PRESENT: i8 = 2;

enum CachedEphemeralWriter {
    Uninitialized,
    Cached(Box<SendFrankenConnection>),
    InUse,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EnsuredConversationSourceKey {
    id: String,
    kind: SourceKind,
    host_label: Option<String>,
}

impl EnsuredConversationSourceKey {
    fn from_source(source: &Source) -> Self {
        Self {
            id: source.id.clone(),
            kind: source.kind,
            host_label: source.host_label.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EnsuredDailyStatsKey {
    day_id: i64,
    agent_slug: String,
    source_id: String,
}

impl EnsuredDailyStatsKey {
    fn new(day_id: i64, agent_slug: &str, source_id: &str) -> Self {
        Self {
            day_id,
            agent_slug: agent_slug.to_owned(),
            source_id: source_id.to_owned(),
        }
    }
}

const AUTOCOMMIT_RETAIN_OFF_PRAGMAS: [&str; 2] = [
    "PRAGMA fsqlite.autocommit_retain = OFF;",
    "PRAGMA autocommit_retain = OFF;",
];

fn disable_autocommit_retain<E>(
    mut execute: impl FnMut(&'static str) -> std::result::Result<(), E>,
) -> Result<&'static str>
where
    E: std::fmt::Display,
{
    let mut failures = Vec::new();
    for pragma in AUTOCOMMIT_RETAIN_OFF_PRAGMAS {
        match execute(pragma) {
            Ok(()) => return Ok(pragma),
            Err(err) => {
                let error = err.to_string();
                tracing::debug!(
                    %pragma,
                    error = %error,
                    "autocommit_retain PRAGMA variant not supported"
                );
                failures.push(format!("{pragma}: {error}"));
            }
        }
    }

    Err(anyhow!(
        "failed to disable autocommit_retain on frankensqlite connection; \
         refusing to keep a long-lived MVCC connection that may accumulate \
         unbounded write snapshots. Upgrade frankensqlite to a version that \
         supports one of these PRAGMAs or use a short-lived connection path. \
         attempts: {}",
        failures.join("; ")
    ))
}

impl FrankenStorage {
    fn new(conn: FrankenConnection, db_path: PathBuf) -> Self {
        Self::new_with_shared_caches(
            conn,
            db_path,
            Arc::new(parking_lot::Mutex::new(HashSet::new())),
            Arc::new(parking_lot::Mutex::new(HashSet::new())),
        )
    }

    fn new_with_shared_caches(
        conn: FrankenConnection,
        db_path: PathBuf,
        ensured_conversation_sources: Arc<
            parking_lot::Mutex<HashSet<EnsuredConversationSourceKey>>,
        >,
        ensured_daily_stats_keys: Arc<parking_lot::Mutex<HashSet<EnsuredDailyStatsKey>>>,
    ) -> Self {
        Self {
            conn,
            db_path,
            ephemeral_writer_preflight_verified: AtomicBool::new(false),
            index_writer_checkpoint_pages: AtomicI64::new(UNSET_INDEX_WRITER_CHECKPOINT_PAGES),
            cached_ephemeral_writer: parking_lot::Mutex::new(CachedEphemeralWriter::Uninitialized),
            ensured_conversation_sources,
            ensured_daily_stats_keys,
            fts_messages_present_cache: AtomicI8::new(FTS_MESSAGES_PRESENT_UNKNOWN),
        }
    }

    fn apply_open_stage_busy_timeout(&self) {
        if let Err(err) = self.conn.execute("PRAGMA busy_timeout = 5000;") {
            tracing::debug!(
                error = %err,
                "failed to apply open-stage busy_timeout before migrations"
            );
        }
    }

    /// Open a frankensqlite connection, run migrations, and apply config.
    ///
    /// This initializes canonical schema state only. Derived fallback search
    /// structures like the in-database `fts_messages` table are repaired
    /// separately so ordinary opens never block on heavyweight maintenance.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating db directory {}", parent.display()))?;
        }

        let path_str = path.to_string_lossy().to_string();
        let conn = FrankenConnection::open(&path_str)
            .with_context(|| format!("opening frankensqlite db at {}", path.display()))?;
        let storage = Self::new(conn, path.to_path_buf());
        storage.apply_open_stage_busy_timeout();
        storage.run_migrations()?;
        storage.repair_missing_current_schema_objects()?;
        storage.apply_config()?;
        Ok(storage)
    }

    /// Open a writer connection that skips migration (assumes DB already migrated).
    ///
    /// Used by the BEGIN CONCURRENT parallel writer pool: each writer needs its
    /// own connection with config applied, but migrations have already been run
    /// by the primary connection.
    pub fn open_writer(path: &Path) -> Result<Self> {
        Self::open_writer_with_shared_caches(
            path,
            Arc::new(parking_lot::Mutex::new(HashSet::new())),
            Arc::new(parking_lot::Mutex::new(HashSet::new())),
        )
    }

    fn open_writer_with_shared_caches(
        path: &Path,
        ensured_conversation_sources: Arc<
            parking_lot::Mutex<HashSet<EnsuredConversationSourceKey>>,
        >,
        ensured_daily_stats_keys: Arc<parking_lot::Mutex<HashSet<EnsuredDailyStatsKey>>>,
    ) -> Result<Self> {
        let path_str = path.to_string_lossy().to_string();
        let conn = FrankenConnection::open(&path_str)
            .with_context(|| format!("opening frankensqlite writer at {}", path.display()))?;
        let storage = Self::new_with_shared_caches(
            conn,
            path.to_path_buf(),
            ensured_conversation_sources,
            ensured_daily_stats_keys,
        );
        storage.apply_config()?;
        Ok(storage)
    }

    pub(crate) fn acquire_cached_ephemeral_writer(&self) -> Result<(Self, bool)> {
        let mut cached = self.cached_ephemeral_writer.lock();
        match std::mem::replace(&mut *cached, CachedEphemeralWriter::InUse) {
            CachedEphemeralWriter::Cached(conn) => Ok((
                Self::new_with_shared_caches(
                    (*conn).into_inner(),
                    self.db_path.clone(),
                    Arc::clone(&self.ensured_conversation_sources),
                    Arc::clone(&self.ensured_daily_stats_keys),
                ),
                true,
            )),
            CachedEphemeralWriter::Uninitialized => {
                drop(cached);
                match Self::open_writer_with_shared_caches(
                    &self.db_path,
                    Arc::clone(&self.ensured_conversation_sources),
                    Arc::clone(&self.ensured_daily_stats_keys),
                ) {
                    Ok(writer) => Ok((writer, true)),
                    Err(err) => {
                        let mut cached = self.cached_ephemeral_writer.lock();
                        if matches!(&*cached, CachedEphemeralWriter::InUse) {
                            *cached = CachedEphemeralWriter::Uninitialized;
                        }
                        Err(err)
                    }
                }
            }
            CachedEphemeralWriter::InUse => {
                *cached = CachedEphemeralWriter::InUse;
                drop(cached);
                Ok((
                    Self::open_writer_with_shared_caches(
                        &self.db_path,
                        Arc::clone(&self.ensured_conversation_sources),
                        Arc::clone(&self.ensured_daily_stats_keys),
                    )?,
                    false,
                ))
            }
        }
    }

    pub(crate) fn release_cached_ephemeral_writer(&self, writer: Self) {
        let conn = writer.into_raw();
        let mut cached = self.cached_ephemeral_writer.lock();
        debug_assert!(
            matches!(&*cached, CachedEphemeralWriter::InUse),
            "cached ephemeral writer state should be in-use when releasing"
        );
        *cached = CachedEphemeralWriter::Cached(Box::new(SendFrankenConnection::new(conn)));
    }

    pub(crate) fn discard_cached_ephemeral_writer(&self, mut writer: Self) {
        writer.close_best_effort_in_place();
        let mut cached = self.cached_ephemeral_writer.lock();
        if matches!(&*cached, CachedEphemeralWriter::InUse) {
            *cached = CachedEphemeralWriter::Uninitialized;
        }
    }

    fn conversation_source_already_ensured(&self, key: &EnsuredConversationSourceKey) -> bool {
        self.ensured_conversation_sources.lock().contains(key)
    }

    fn mark_conversation_source_ensured(&self, key: EnsuredConversationSourceKey) {
        self.ensured_conversation_sources.lock().insert(key);
    }

    fn daily_stats_key_already_ensured(&self, key: &EnsuredDailyStatsKey) -> bool {
        self.ensured_daily_stats_keys.lock().contains(key)
    }

    fn mark_daily_stats_key_ensured(&self, key: EnsuredDailyStatsKey) {
        self.ensured_daily_stats_keys.lock().insert(key);
    }

    fn fts_messages_present_cached(&self, tx: &FrankenTransaction<'_>) -> bool {
        match self.fts_messages_present_cache.load(Ordering::Acquire) {
            FTS_MESSAGES_PRESENT_PRESENT => return true,
            FTS_MESSAGES_PRESENT_ABSENT => return false,
            _ => {}
        }

        let present = tx
            .query_row_map(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE name = 'fts_messages'
                   AND rootpage > 0",
                fparams![],
                |row| row.get_typed::<i64>(0),
            )
            .map(|count| count > 0)
            .unwrap_or_else(|err| {
                tracing::debug!(
                    error = %err,
                    "failed to probe fts_messages presence; skipping db-resident FTS maintenance"
                );
                false
            });
        self.set_fts_messages_present_cache(present);
        present
    }

    fn set_fts_messages_present_cache(&self, present: bool) {
        self.fts_messages_present_cache.store(
            if present {
                FTS_MESSAGES_PRESENT_PRESENT
            } else {
                FTS_MESSAGES_PRESENT_ABSENT
            },
            Ordering::Release,
        );
    }

    fn invalidate_fts_messages_present_cache(&self) {
        self.fts_messages_present_cache
            .store(FTS_MESSAGES_PRESENT_UNKNOWN, Ordering::Release);
    }

    fn invalidate_conversation_source_cache(&self, source_id: &str) {
        self.ensured_conversation_sources
            .lock()
            .retain(|key| key.id != source_id);
    }

    fn close_cached_ephemeral_writer_best_effort_in_place(&mut self) {
        let cached = self.cached_ephemeral_writer.get_mut();
        if let CachedEphemeralWriter::Cached(conn) =
            std::mem::replace(cached, CachedEphemeralWriter::Uninitialized)
        {
            let mut conn = conn;
            conn.0.close_best_effort_in_place();
        }
    }

    fn close_cached_ephemeral_writer_without_checkpoint_in_place(&mut self) -> Result<()> {
        let cached = self.cached_ephemeral_writer.get_mut();
        match std::mem::replace(cached, CachedEphemeralWriter::Uninitialized) {
            CachedEphemeralWriter::Cached(mut conn) => conn
                .0
                .close_without_checkpoint_in_place()
                .with_context(|| "closing cached frankensqlite writer without final checkpoint"),
            CachedEphemeralWriter::Uninitialized | CachedEphemeralWriter::InUse => Ok(()),
        }
    }

    /// Open in read-only mode using frankensqlite compat flags.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let path_str = path.to_string_lossy().to_string();
        let conn = open_franken_with_flags(&path_str, FrankenOpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening frankensqlite db readonly at {}", path.display()))?;
        let storage = Self::new(conn, path.to_path_buf());
        storage.apply_readonly_config()?;
        Ok(storage)
    }

    pub fn close(self) -> Result<()> {
        let mut this = self;
        this.close_cached_ephemeral_writer_best_effort_in_place();
        this.conn
            .close()
            .with_context(|| "closing frankensqlite connection")
    }

    pub fn close_without_checkpoint(self) -> Result<()> {
        let mut this = self;
        this.close_cached_ephemeral_writer_without_checkpoint_in_place()?;
        this.conn
            .close_without_checkpoint()
            .with_context(|| "closing frankensqlite connection without final checkpoint")
    }

    pub fn close_best_effort_in_place(&mut self) {
        self.close_cached_ephemeral_writer_best_effort_in_place();
        self.conn.close_best_effort_in_place();
    }

    pub fn close_without_checkpoint_in_place(&mut self) -> Result<()> {
        self.close_cached_ephemeral_writer_without_checkpoint_in_place()?;
        self.conn
            .close_without_checkpoint_in_place()
            .with_context(|| "closing frankensqlite connection without final checkpoint")
    }

    /// Access the raw frankensqlite connection.
    pub fn raw(&self) -> &FrankenConnection {
        &self.conn
    }

    /// Consume the storage wrapper and return the underlying frankensqlite
    /// connection after migrations/repair have already been applied.
    pub fn into_raw(self) -> FrankenConnection {
        let mut this = self;
        this.close_cached_ephemeral_writer_best_effort_in_place();
        this.conn
    }

    /// Apply connection PRAGMAs for parity with SqliteStorage's `apply_pragmas()`.
    ///
    /// Frankensqlite supports all PRAGMAs cass uses (journal_mode, synchronous,
    /// cache_size, foreign_keys, busy_timeout). Its default journal_mode is already
    /// WAL and default synchronous is NORMAL, matching cass's requirements.
    ///
    pub fn apply_config(&self) -> Result<()> {
        // journal_mode: frankensqlite defaults to WAL, same as cass.
        // synchronous: frankensqlite defaults to NORMAL, same as cass.
        // Both are set explicitly for clarity.
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

        // wal_autocheckpoint: use a bounded cadence that avoids checkpointing
        // inside common append batches without deferring checkpoints forever.
        let checkpoint_pragma =
            format!("PRAGMA wal_autocheckpoint = {DEFAULT_WAL_AUTOCHECKPOINT_PAGES};");
        let _ = self.conn.execute(&checkpoint_pragma);
        self.index_writer_checkpoint_pages
            .store(DEFAULT_WAL_AUTOCHECKPOINT_PAGES, Ordering::Relaxed);
        // Explicitly enable concurrent writer mode for BEGIN/transaction paths.
        // Try both namespace variants for compatibility across fsqlite builds.
        let _ = self.conn.execute("PRAGMA fsqlite.concurrent_mode = ON;");
        let _ = self.conn.execute("PRAGMA concurrent_mode = ON;");
        // Frankensqlite retained autocommit currently mis-serves same-connection
        // read-after-write queries on cass's storage paths; keep it off here
        // until the upstream visibility bug is fixed.
        //
        // CASS #163 item 3: If neither PRAGMA variant succeeds, the MVCC engine
        // will accumulate write snapshots for the lifetime of the connection,
        // causing unbounded memory growth on long-lived watch-mode handles.
        // Log at warn level so the failure is visible instead of silently
        // swallowed, and set a flag for callers that need to periodically
        // recycle the connection.
        let autocommit_pragma =
            disable_autocommit_retain(|pragma| self.conn.execute(pragma).map(|_| ()))?;
        tracing::debug!(
            pragma = autocommit_pragma,
            "disabled frankensqlite autocommit_retain for storage connection"
        );

        Ok(())
    }

    fn apply_readonly_config(&self) -> Result<()> {
        self.conn
            .execute("PRAGMA query_only = 1;")
            .with_context(|| "setting query_only")?;
        self.conn
            .execute("PRAGMA busy_timeout = 5000;")
            .with_context(|| "setting busy_timeout")?;
        self.conn
            .execute("PRAGMA cache_size = -65536;")
            .with_context(|| "setting cache_size")?;
        self.conn
            .execute("PRAGMA foreign_keys = ON;")
            .with_context(|| "setting foreign_keys")?;
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

    /// Some historical canonical rebuild paths produced databases whose
    /// version markers claim the current schema while post-V10 analytics
    /// tables were never materialized. Detect that drift and backfill the
    /// idempotent table/index set from the combined schema migration.
    fn repair_missing_current_schema_objects(&self) -> Result<()> {
        let mut missing_tables = Vec::new();
        for &(table_name, probe_sql) in REQUIRED_CURRENT_SCHEMA_TABLE_PROBES {
            if let Err(err) = self.conn.query(probe_sql) {
                if error_indicates_missing_table(&err) {
                    missing_tables.push(table_name);
                    continue;
                }
                return Err(err).with_context(|| {
                    format!("probing required schema table {table_name} for completeness")
                });
            }
        }

        if missing_tables.is_empty() {
            return Ok(());
        }

        info!(
            missing_tables = ?missing_tables,
            "repairing missing current-schema tables on an already-versioned cass database"
        );

        for batch in current_schema_repair_batches_for_missing_tables(&missing_tables)? {
            self.conn
                .execute_batch(batch.sql)
                .with_context(|| format!("repairing current-schema batch {}", batch.name))?;
        }

        for &(table_name, probe_sql) in REQUIRED_CURRENT_SCHEMA_TABLE_PROBES {
            if !missing_tables.contains(&table_name) {
                continue;
            }
            self.conn
                .query(probe_sql)
                .with_context(|| format!("verifying repaired schema table {table_name}"))?;
        }
        Ok(())
    }

    /// Detect and remove orphan rows whose FK parent has gone missing.
    ///
    /// A `Connection` dropped mid-transaction (the `drop_close` warning emitted
    /// by frankensqlite's `Drop` impl) can leave child rows persisted without a
    /// matching parent — `messages` referencing a `conversation_id` that does
    /// not exist, `message_metrics`/`token_usage`/`snippets` referencing a
    /// `message_id` that does not exist, etc. With `PRAGMA foreign_keys = ON`,
    /// every subsequent indexer pass then trips `FOREIGN KEY constraint failed`
    /// on the next write, the session never gets marked indexed, and the
    /// pending backlog grows without bound (issue #202).
    ///
    /// This pass runs at indexer startup as defense in depth: it scans each
    /// child table for rows whose parent row has gone missing and removes them
    /// in a single transaction, breaking the failure cycle even when the
    /// underlying transaction-discipline bug has not been fully root-caused.
    /// The pass is bounded (one count + one DELETE per child table), idempotent
    /// (a clean database is a no-op), and emits a `WARN` after a successful
    /// commit so the upstream `drop_close` condition stays visible.
    pub(crate) fn cleanup_orphan_fk_rows(&self) -> Result<OrphanFkCleanupReport> {
        let mut report = OrphanFkCleanupReport::default();
        let mut to_delete: SmallVec<[&'static OrphanFkTable; 8]> = SmallVec::new();
        for entry in ORPHAN_FK_TABLES {
            let count: i64 = match self
                .conn
                .query_row_map(entry.count_sql, fparams![], |row| row.get_typed::<i64>(0))
            {
                Ok(c) => c,
                Err(err) => {
                    // Tolerant probe: a missing child or parent table (older
                    // schema, freshly-rebuilt DB) just means there's nothing to
                    // clean up. Anything else is logged at debug and skipped
                    // rather than failing indexer startup.
                    tracing::debug!(
                        target: "cass::fk_repair",
                        child_table = entry.child_table,
                        error = %err,
                        "skipping orphan probe (table or column unavailable)"
                    );
                    continue;
                }
            };
            if count > 0 {
                report.record(entry.child_table, count);
                to_delete.push(entry);
            }
        }

        if to_delete.is_empty() {
            return Ok(report);
        }

        let mut tx = self.conn.transaction()?;
        for entry in &to_delete {
            tx.execute_compat(entry.delete_sql, fparams![])?;
        }
        tx.commit()?;

        // WARN only fires after a successful commit so the message accurately
        // reflects what actually happened on disk. db_path is included so logs
        // from concurrent indexers against different databases stay
        // disambiguated.
        tracing::warn!(
            target: "cass::fk_repair",
            db_path = %self.db_path.display(),
            total_orphans = report.total,
            per_table = ?report.per_table,
            "cass#202: removed orphan rows left behind by interrupted index transactions"
        );

        Ok(report)
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
        Ok(self.db_path.clone())
    }

    pub(crate) fn ephemeral_writer_preflight_verified(&self) -> bool {
        self.ephemeral_writer_preflight_verified
            .load(Ordering::Relaxed)
    }

    pub(crate) fn mark_ephemeral_writer_preflight_verified(&self) {
        self.ephemeral_writer_preflight_verified
            .store(true, Ordering::Relaxed);
    }

    pub(crate) fn index_writer_checkpoint_pages(&self) -> Option<i64> {
        let pages = self.index_writer_checkpoint_pages.load(Ordering::Relaxed);
        (pages != UNSET_INDEX_WRITER_CHECKPOINT_PAGES).then_some(pages)
    }

    pub(crate) fn mark_index_writer_checkpoint_pages(&self, pages: i64) {
        self.index_writer_checkpoint_pages
            .store(pages, Ordering::Relaxed);
    }

    /// Open database with migration, backing up if schema is incompatible.
    pub fn open_or_rebuild(path: &Path) -> std::result::Result<Self, MigrationError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        if path.exists() {
            let check_result = check_schema_compatibility(path);
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
                Err(err) if schema_check_error_requires_rebuild(&err) => {
                    let backup_path = create_backup(path)?;
                    cleanup_old_backups(path, MAX_BACKUPS)?;
                    remove_database_files(path)?;
                    return Err(MigrationError::RebuildRequired {
                        reason: format!("Database appears corrupted: {err}"),
                        backup_path,
                    });
                }
                Err(err) => return Err(MigrationError::Database(err)),
            }
        }

        let storage = Self::open(path).map_err(|e| MigrationError::Other(e.to_string()))?;
        Ok(storage)
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
        .add(15, "conversation_tail_state_cache", MIGRATION_V15)
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

-- Lexical FTS index (current contentless form)
CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages USING fts5(
    content,
    title,
    agent,
    workspace,
    source_path,
    created_at UNINDEXED,
    content='',
    tokenize='porter'
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

#[derive(Clone, Copy)]
struct SchemaRepairBatch {
    name: &'static str,
    tables: &'static [&'static str],
    sql: &'static str,
}

const CURRENT_SCHEMA_REPAIR_SOURCES_SQL: &str = r"
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
";

const CURRENT_SCHEMA_REPAIR_DAILY_STATS_SQL: &str = r"
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

CREATE INDEX IF NOT EXISTS idx_daily_stats_agent ON daily_stats(agent_slug, day_id);
CREATE INDEX IF NOT EXISTS idx_daily_stats_source ON daily_stats(source_id, day_id);
";

const CURRENT_SCHEMA_REPAIR_EMBEDDING_JOBS_SQL: &str = r"
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
";

const CURRENT_SCHEMA_REPAIR_TOKEN_ANALYTICS_SQL: &str = r"
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

CREATE INDEX IF NOT EXISTS idx_token_usage_day ON token_usage(day_id, agent_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_conv ON token_usage(conversation_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_model ON token_usage(model_family, day_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_workspace ON token_usage(workspace_id, day_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_timestamp ON token_usage(timestamp_ms);

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
";

const CURRENT_SCHEMA_REPAIR_MESSAGE_METRICS_SQL: &str = r"
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

const CURRENT_SCHEMA_REPAIR_BATCHES: &[SchemaRepairBatch] = &[
    SchemaRepairBatch {
        name: "sources",
        tables: &["sources"],
        sql: CURRENT_SCHEMA_REPAIR_SOURCES_SQL,
    },
    SchemaRepairBatch {
        name: "daily_stats",
        tables: &["daily_stats"],
        sql: CURRENT_SCHEMA_REPAIR_DAILY_STATS_SQL,
    },
    SchemaRepairBatch {
        name: "embedding_jobs",
        tables: &["embedding_jobs"],
        sql: CURRENT_SCHEMA_REPAIR_EMBEDDING_JOBS_SQL,
    },
    SchemaRepairBatch {
        name: "token_analytics",
        tables: &["token_usage", "token_daily_stats", "model_pricing"],
        sql: CURRENT_SCHEMA_REPAIR_TOKEN_ANALYTICS_SQL,
    },
    SchemaRepairBatch {
        name: "message_rollups",
        tables: &[
            "message_metrics",
            "usage_hourly",
            "usage_daily",
            "usage_models_daily",
        ],
        sql: CURRENT_SCHEMA_REPAIR_MESSAGE_METRICS_SQL,
    },
];

fn current_schema_repair_batches_for_missing_tables(
    missing_tables: &[&'static str],
) -> Result<Vec<&'static SchemaRepairBatch>> {
    let missing_set: HashSet<&'static str> = missing_tables.iter().copied().collect();
    let mut selected_batches = Vec::new();
    let mut covered_tables = HashSet::new();

    for batch in CURRENT_SCHEMA_REPAIR_BATCHES {
        if !batch
            .tables
            .iter()
            .any(|table_name| missing_set.contains(table_name))
        {
            continue;
        }
        selected_batches.push(batch);
        covered_tables.extend(batch.tables.iter().copied());
    }

    for &table_name in missing_tables {
        if !covered_tables.contains(table_name) {
            return Err(anyhow!(
                "no current-schema repair batch registered for missing table {table_name}"
            ));
        }
    }

    Ok(selected_batches)
}

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
    // Avoid sqlite_master enumeration here. Databases with FTS virtual tables
    // can trigger frankensqlite parse-recovery on sqlite_master reads, which is
    // enough to break the transition on otherwise-healthy legacy cass DBs.
    if conn
        .query("SELECT version FROM \"_schema_migrations\";")
        .is_ok()
    {
        return Ok(());
    }

    // Check if the meta table exists.
    if conn.query("SELECT key FROM meta;").is_err() {
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
        if conn.query("SELECT id FROM conversations LIMIT 1;").is_err() {
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

const REQUIRED_CURRENT_SCHEMA_TABLE_PROBES: &[(&str, &str)] = &[
    ("sources", "SELECT id FROM sources LIMIT 1;"),
    ("daily_stats", "SELECT day_id FROM daily_stats LIMIT 1;"),
    ("embedding_jobs", "SELECT id FROM embedding_jobs LIMIT 1;"),
    ("token_usage", "SELECT id FROM token_usage LIMIT 1;"),
    (
        "token_daily_stats",
        "SELECT day_id FROM token_daily_stats LIMIT 1;",
    ),
    (
        "model_pricing",
        "SELECT model_pattern FROM model_pricing LIMIT 1;",
    ),
    (
        "message_metrics",
        "SELECT message_id FROM message_metrics LIMIT 1;",
    ),
    ("usage_hourly", "SELECT hour_id FROM usage_hourly LIMIT 1;"),
    ("usage_daily", "SELECT day_id FROM usage_daily LIMIT 1;"),
    (
        "usage_models_daily",
        "SELECT day_id FROM usage_models_daily LIMIT 1;",
    ),
];

fn error_indicates_missing_table(err: &impl std::fmt::Display) -> bool {
    err.to_string()
        .to_ascii_lowercase()
        .contains("no such table")
}

/// Tables whose FK parent rows can go missing when an index transaction is
/// dropped mid-flight. The count and delete SQL strings are intentionally
/// static (no dynamic table names) so they can be audited at a glance and so
/// they cannot be subverted by injected identifiers. Each entry's count and
/// delete statements share the identical predicate to guarantee they identify
/// the same row set.
struct OrphanFkTable {
    child_table: &'static str,
    count_sql: &'static str,
    delete_sql: &'static str,
}

const ORPHAN_FK_TABLES: &[OrphanFkTable] = &[
    OrphanFkTable {
        child_table: "messages",
        count_sql: "SELECT COUNT(*) FROM messages \
                    WHERE NOT EXISTS (SELECT 1 FROM conversations WHERE conversations.id = messages.conversation_id)",
        delete_sql: "DELETE FROM messages \
                     WHERE NOT EXISTS (SELECT 1 FROM conversations WHERE conversations.id = messages.conversation_id)",
    },
    OrphanFkTable {
        child_table: "message_metrics",
        count_sql: "SELECT COUNT(*) FROM message_metrics \
                    WHERE NOT EXISTS (SELECT 1 FROM messages WHERE messages.id = message_metrics.message_id)",
        delete_sql: "DELETE FROM message_metrics \
                     WHERE NOT EXISTS (SELECT 1 FROM messages WHERE messages.id = message_metrics.message_id)",
    },
    OrphanFkTable {
        child_table: "token_usage",
        count_sql: "SELECT COUNT(*) FROM token_usage \
                    WHERE NOT EXISTS (SELECT 1 FROM messages WHERE messages.id = token_usage.message_id)",
        delete_sql: "DELETE FROM token_usage \
                     WHERE NOT EXISTS (SELECT 1 FROM messages WHERE messages.id = token_usage.message_id)",
    },
    OrphanFkTable {
        child_table: "snippets",
        count_sql: "SELECT COUNT(*) FROM snippets \
                    WHERE NOT EXISTS (SELECT 1 FROM messages WHERE messages.id = snippets.message_id)",
        delete_sql: "DELETE FROM snippets \
                     WHERE NOT EXISTS (SELECT 1 FROM messages WHERE messages.id = snippets.message_id)",
    },
    OrphanFkTable {
        child_table: "conversation_tags",
        count_sql: "SELECT COUNT(*) FROM conversation_tags \
                    WHERE NOT EXISTS (SELECT 1 FROM conversations WHERE conversations.id = conversation_tags.conversation_id)",
        delete_sql: "DELETE FROM conversation_tags \
                     WHERE NOT EXISTS (SELECT 1 FROM conversations WHERE conversations.id = conversation_tags.conversation_id)",
    },
];

/// Summary of orphan rows detected and removed by `cleanup_orphan_fk_rows`.
///
/// Counts come from the count-phase `SELECT COUNT(*)` rather than from the
/// `DELETE`'s rows-changed return, so they reflect "orphans observed before
/// the delete transaction started." Under the function's intended use — a
/// single indexer-startup pass holding the index run lock — no concurrent
/// writers exist, so these counts match the rows the function's own `DELETE`
/// statements removed. Additional rows can be removed by `ON DELETE CASCADE`
/// triggers when an orphan message is deleted (its `message_metrics` /
/// `token_usage` / `snippets` children cascade away with it); those cascade
/// removals are an expected consequence of the cleanup but are *not*
/// separately counted in `total` or `per_table`.
#[derive(Debug, Default, Clone)]
pub(crate) struct OrphanFkCleanupReport {
    pub total: i64,
    pub per_table: Vec<(&'static str, i64)>,
}

impl OrphanFkCleanupReport {
    fn record(&mut self, child_table: &'static str, count: i64) {
        self.per_table.push((child_table, count));
        self.total = self.total.saturating_add(count);
    }
}

pub struct InsertOutcome {
    pub conversation_id: i64,
    pub conversation_inserted: bool,
    pub inserted_indices: Vec<i64>,
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
struct MessageInsertSubstageProfile {
    single_row_calls: usize,
    batch_calls: usize,
    batch_rows: usize,
    payload_duration: Duration,
    sql_build_duration: Duration,
    param_build_duration: Duration,
    execute_duration: Duration,
    rowid_duration: Duration,
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
struct InsertConversationTreePerfProfile {
    invocations: usize,
    messages: usize,
    inserted_messages: usize,
    total_duration: Duration,
    source_duration: Duration,
    tx_open_duration: Duration,
    existing_lookup_duration: Duration,
    existing_idx_lookup_duration: Duration,
    existing_replay_lookup_duration: Duration,
    dedupe_filter_duration: Duration,
    conversation_row_duration: Duration,
    message_insert_duration: Duration,
    message_insert_breakdown: MessageInsertSubstageProfile,
    snippet_insert_duration: Duration,
    fts_entry_duration: Duration,
    fts_flush_duration: Duration,
    analytics_duration: Duration,
    commit_duration: Duration,
}

#[cfg(test)]
impl InsertConversationTreePerfProfile {
    fn millis(duration: Duration) -> f64 {
        duration.as_secs_f64() * 1000.0
    }

    fn log_summary(&self, label: &str) {
        let calls = self.invocations.max(1) as f64;
        let accounted_duration = self.source_duration
            + self.tx_open_duration
            + self.existing_lookup_duration
            + self.existing_idx_lookup_duration
            + self.existing_replay_lookup_duration
            + self.dedupe_filter_duration
            + self.conversation_row_duration
            + self.message_insert_duration
            + self.snippet_insert_duration
            + self.fts_entry_duration
            + self.fts_flush_duration
            + self.analytics_duration
            + self.commit_duration;
        let residual_duration = self.total_duration.saturating_sub(accounted_duration);
        eprintln!(
            concat!(
                "CASS_INSERT_TREE_STAGE_PROFILE ",
                "label={} calls={} messages={} inserted_messages={} ",
                "total_ms={:.3} source_ms={:.3} tx_open_ms={:.3} existing_lookup_ms={:.3} ",
                "existing_idx_lookup_ms={:.3} existing_replay_lookup_ms={:.3} dedupe_filter_ms={:.3} ",
                "conversation_row_ms={:.3} message_insert_ms={:.3} snippet_insert_ms={:.3} ",
                "fts_entry_ms={:.3} fts_flush_ms={:.3} analytics_ms={:.3} commit_ms={:.3} ",
                "msg_payload_ms={:.3} msg_sql_ms={:.3} msg_param_ms={:.3} msg_execute_ms={:.3} msg_rowid_ms={:.3} ",
                "residual_ms={:.3} avg_total_ms={:.3} avg_message_insert_ms={:.3} ",
                "avg_msg_execute_ms={:.3} avg_msg_payload_ms={:.3} avg_snippet_insert_ms={:.3} avg_fts_entry_ms={:.3} avg_commit_ms={:.3}"
            ),
            label,
            self.invocations,
            self.messages,
            self.inserted_messages,
            Self::millis(self.total_duration),
            Self::millis(self.source_duration),
            Self::millis(self.tx_open_duration),
            Self::millis(self.existing_lookup_duration),
            Self::millis(self.existing_idx_lookup_duration),
            Self::millis(self.existing_replay_lookup_duration),
            Self::millis(self.dedupe_filter_duration),
            Self::millis(self.conversation_row_duration),
            Self::millis(self.message_insert_duration),
            Self::millis(self.snippet_insert_duration),
            Self::millis(self.fts_entry_duration),
            Self::millis(self.fts_flush_duration),
            Self::millis(self.analytics_duration),
            Self::millis(self.commit_duration),
            Self::millis(self.message_insert_breakdown.payload_duration),
            Self::millis(self.message_insert_breakdown.sql_build_duration),
            Self::millis(self.message_insert_breakdown.param_build_duration),
            Self::millis(self.message_insert_breakdown.execute_duration),
            Self::millis(self.message_insert_breakdown.rowid_duration),
            Self::millis(residual_duration),
            Self::millis(self.total_duration) / calls,
            Self::millis(self.message_insert_duration) / calls,
            Self::millis(self.message_insert_breakdown.execute_duration) / calls,
            Self::millis(self.message_insert_breakdown.payload_duration) / calls,
            Self::millis(self.snippet_insert_duration) / calls,
            Self::millis(self.fts_entry_duration) / calls,
            Self::millis(self.commit_duration) / calls,
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PendingConversationKey {
    External {
        source_id: String,
        agent_id: i64,
        external_id: String,
    },
    SourcePath {
        source_id: String,
        agent_id: i64,
        source_path: String,
        started_at: Option<i64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MessageMergeFingerprint {
    idx: i64,
    created_at: Option<i64>,
    role: MessageRole,
    author: Option<String>,
    content_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MessageReplayFingerprint {
    created_at: Option<i64>,
    role: MessageRole,
    author: Option<String>,
    content_hash: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConversationMergeEvidence {
    exact_overlap: usize,
    replay_overlap: usize,
    smaller_replay_set: usize,
    started_close: bool,
    start_distance_ms: i64,
}

struct ExistingConversationNewMessages<'a> {
    messages: Vec<&'a Message>,
    new_chars: i64,
    idx_collision_count: usize,
    first_collision_idx: Option<i64>,
}

fn conversation_effective_started_at(conv: &Conversation) -> Option<i64> {
    conv.started_at
        .or_else(|| conv.messages.iter().filter_map(|msg| msg.created_at).min())
}

fn conversation_tail_state(conv: &Conversation) -> (Option<i64>, Option<i64>) {
    (
        conv.messages.iter().map(|msg| msg.idx).max(),
        conv.messages.iter().filter_map(|msg| msg.created_at).max(),
    )
}

fn borrowed_messages_tail_state(messages: &[&Message]) -> (Option<i64>, Option<i64>) {
    (
        messages.iter().map(|msg| msg.idx).max(),
        messages.iter().filter_map(|msg| msg.created_at).max(),
    )
}

fn role_from_str(role: &str) -> MessageRole {
    match role {
        "user" => MessageRole::User,
        "agent" | "assistant" => MessageRole::Agent,
        "tool" => MessageRole::Tool,
        "system" => MessageRole::System,
        other => MessageRole::Other(other.to_string()),
    }
}

fn message_merge_fingerprint(msg: &Message) -> MessageMergeFingerprint {
    MessageMergeFingerprint {
        idx: msg.idx,
        created_at: msg.created_at,
        role: msg.role.clone(),
        author: msg.author.clone(),
        content_hash: *blake3::hash(msg.content.as_bytes()).as_bytes(),
    }
}

fn message_replay_fingerprint(msg: &Message) -> MessageReplayFingerprint {
    MessageReplayFingerprint {
        created_at: msg.created_at,
        role: msg.role.clone(),
        author: msg.author.clone(),
        content_hash: *blake3::hash(msg.content.as_bytes()).as_bytes(),
    }
}

fn conversation_message_fingerprints(conv: &Conversation) -> HashSet<MessageMergeFingerprint> {
    conv.messages
        .iter()
        .map(message_merge_fingerprint)
        .collect()
}

fn conversation_message_replay_fingerprints(
    conv: &Conversation,
) -> HashSet<MessageReplayFingerprint> {
    conv.messages
        .iter()
        .map(message_replay_fingerprint)
        .collect()
}

fn replay_fingerprint_from_merge(
    fingerprint: &MessageMergeFingerprint,
) -> MessageReplayFingerprint {
    MessageReplayFingerprint {
        created_at: fingerprint.created_at,
        role: fingerprint.role.clone(),
        author: fingerprint.author.clone(),
        content_hash: fingerprint.content_hash,
    }
}

fn replay_fingerprints_from_merge_set(
    fingerprints: &HashSet<MessageMergeFingerprint>,
) -> HashSet<MessageReplayFingerprint> {
    fingerprints
        .iter()
        .map(replay_fingerprint_from_merge)
        .collect()
}

fn collect_new_messages_for_existing_conversation<'a>(
    conversation_id: i64,
    conv: &'a Conversation,
    existing_messages: &mut HashMap<i64, MessageMergeFingerprint>,
    existing_replay_fingerprints: &mut HashSet<MessageReplayFingerprint>,
    replay_skip_log: &'static str,
) -> ExistingConversationNewMessages<'a> {
    let mut idx_collision_count = 0usize;
    let mut first_collision_idx: Option<i64> = None;
    let mut new_chars: i64 = 0;
    let mut messages = Vec::new();

    for msg in &conv.messages {
        let incoming_fingerprint = message_merge_fingerprint(msg);
        if let Some(existing_fingerprint) = existing_messages.get(&msg.idx) {
            if existing_fingerprint != &incoming_fingerprint {
                idx_collision_count = idx_collision_count.saturating_add(1);
                first_collision_idx.get_or_insert(msg.idx);
            }
            continue;
        }

        let incoming_replay = replay_fingerprint_from_merge(&incoming_fingerprint);
        if existing_replay_fingerprints.contains(&incoming_replay) {
            tracing::debug!(
                conversation_id,
                idx = msg.idx,
                source_path = %conv.source_path.display(),
                "{replay_skip_log}"
            );
            continue;
        }

        existing_messages.insert(msg.idx, incoming_fingerprint);
        existing_replay_fingerprints.insert(incoming_replay);
        new_chars += msg.content.len() as i64;
        messages.push(msg);
    }

    ExistingConversationNewMessages {
        messages,
        new_chars,
        idx_collision_count,
        first_collision_idx,
    }
}

fn franken_existing_conversation_append_tail_state(
    tx: &FrankenTransaction<'_>,
    conversation_id: i64,
) -> Result<Option<(i64, i64)>> {
    let cached: (Option<i64>, Option<i64>) = tx.query_row_map(
        "SELECT last_message_idx, last_message_created_at
         FROM conversations
         WHERE id = ?1",
        fparams![conversation_id],
        |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
    )?;
    if let Some(state) = cached.0.zip(cached.1) {
        return Ok(Some(state));
    }

    let (max_idx, max_created_at): (Option<i64>, Option<i64>) = tx.query_row_map(
        "SELECT MAX(idx), MAX(created_at)
         FROM messages
         WHERE conversation_id = ?1",
        fparams![conversation_id],
        |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
    )?;
    if let Some((last_message_idx, last_message_created_at)) = max_idx.zip(max_created_at) {
        franken_update_conversation_tail_state(
            tx,
            conversation_id,
            None,
            Some(last_message_idx),
            Some(last_message_created_at),
        )?;
        return Ok(Some((last_message_idx, last_message_created_at)));
    }
    Ok(None)
}

fn franken_update_conversation_tail_state(
    tx: &FrankenTransaction<'_>,
    conversation_id: i64,
    ended_at_candidate: Option<i64>,
    last_message_idx_candidate: Option<i64>,
    last_message_created_at_candidate: Option<i64>,
) -> Result<()> {
    if ended_at_candidate.is_none()
        && last_message_idx_candidate.is_none()
        && last_message_created_at_candidate.is_none()
    {
        return Ok(());
    }

    tx.execute_compat(
        "UPDATE conversations
         SET ended_at = CASE
                 WHEN ?1 IS NULL THEN ended_at
                 ELSE MAX(IFNULL(ended_at, 0), ?1)
             END,
             last_message_idx = CASE
                 WHEN ?2 IS NULL THEN last_message_idx
                 WHEN last_message_idx IS NULL OR last_message_idx < ?2 THEN ?2
                 ELSE last_message_idx
             END,
             last_message_created_at = CASE
                 WHEN ?3 IS NULL THEN last_message_created_at
                 WHEN last_message_created_at IS NULL OR last_message_created_at < ?3 THEN ?3
                 ELSE last_message_created_at
             END
         WHERE id = ?4",
        fparams![
            ended_at_candidate,
            last_message_idx_candidate,
            last_message_created_at_candidate,
            conversation_id
        ],
    )?;
    Ok(())
}

fn collect_append_only_tail_messages<'a>(
    conv: &'a Conversation,
    existing_max_idx: i64,
    existing_max_created_at: i64,
) -> Option<ExistingConversationNewMessages<'a>> {
    if conv.messages.is_empty() {
        return Some(ExistingConversationNewMessages {
            messages: Vec::new(),
            new_chars: 0,
            idx_collision_count: 0,
            first_collision_idx: None,
        });
    }

    let mut split_idx = None;
    let mut prev_idx = None;
    for (pos, msg) in conv.messages.iter().enumerate() {
        if prev_idx.is_some_and(|prev| msg.idx < prev) {
            return None;
        }
        prev_idx = Some(msg.idx);
        if split_idx.is_none() && msg.idx > existing_max_idx {
            split_idx = Some(pos);
        }
    }
    let split_idx = split_idx?;

    let mut seen_tail_idx = HashSet::new();
    let mut seen_tail_replay = HashSet::new();
    let mut new_chars = 0i64;
    let mut messages = Vec::new();
    for msg in &conv.messages[split_idx..] {
        let created_at = msg.created_at?;
        if created_at <= existing_max_created_at {
            return None;
        }

        if !seen_tail_idx.insert(msg.idx) {
            return None;
        }

        let replay_fingerprint = message_replay_fingerprint(msg);
        if !seen_tail_replay.insert(replay_fingerprint) {
            return None;
        }

        new_chars += msg.content.len() as i64;
        messages.push(msg);
    }

    Some(ExistingConversationNewMessages {
        messages,
        new_chars,
        idx_collision_count: 0,
        first_collision_idx: None,
    })
}

fn start_distance_ms(left: Option<i64>, right: Option<i64>) -> i64 {
    match (left, right) {
        (Some(left), Some(right)) => (i128::from(left) - i128::from(right))
            .abs()
            .try_into()
            .unwrap_or(i64::MAX),
        _ => i64::MAX,
    }
}

fn conversation_merge_evidence(
    incoming_exact: &HashSet<MessageMergeFingerprint>,
    incoming_replay: &HashSet<MessageReplayFingerprint>,
    existing_exact: &HashSet<MessageMergeFingerprint>,
    existing_replay: &HashSet<MessageReplayFingerprint>,
    incoming_started_at: Option<i64>,
    existing_started_at: Option<i64>,
) -> Option<ConversationMergeEvidence> {
    let exact_overlap = incoming_exact.intersection(existing_exact).count();
    let replay_overlap = incoming_replay.intersection(existing_replay).count();
    if exact_overlap == 0 && replay_overlap == 0 {
        return None;
    }

    let smaller_replay_set = incoming_replay.len().min(existing_replay.len());
    let started_close = timestamps_within_tolerance(
        incoming_started_at,
        existing_started_at,
        SOURCE_PATH_MERGE_START_TOLERANCE_MS,
    );
    let full_replay_subset_match = smaller_replay_set >= 2 && replay_overlap == smaller_replay_set;

    let merge_allowed = if started_close {
        exact_overlap >= 1 || replay_overlap >= 2
    } else {
        exact_overlap >= 2 || full_replay_subset_match
    };

    merge_allowed.then_some(ConversationMergeEvidence {
        exact_overlap,
        replay_overlap,
        smaller_replay_set,
        started_close,
        start_distance_ms: start_distance_ms(incoming_started_at, existing_started_at),
    })
}

fn timestamps_within_tolerance(left: Option<i64>, right: Option<i64>, tolerance_ms: i64) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            (i128::from(left) - i128::from(right)).abs() <= i128::from(tolerance_ms)
        }
        _ => false,
    }
}

fn conversation_merge_key(agent_id: i64, conv: &Conversation) -> PendingConversationKey {
    if let Some(external_id) = conv.external_id.clone() {
        PendingConversationKey::External {
            source_id: conv.source_id.clone(),
            agent_id,
            external_id,
        }
    } else {
        PendingConversationKey::SourcePath {
            source_id: conv.source_id.clone(),
            agent_id,
            source_path: path_to_string(&conv.source_path),
            started_at: conversation_effective_started_at(conv),
        }
    }
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
        let kind = agent_kind_str(agent.kind.clone());
        self.conn.execute_compat(
            "INSERT INTO agents(slug, name, version, kind, created_at, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(slug) DO UPDATE SET
                 name = excluded.name,
                 version = excluded.version,
                 kind = excluded.kind,
                 updated_at = excluded.updated_at
             WHERE NOT (
                 agents.name IS excluded.name
                 AND agents.version IS excluded.version
                 AND agents.kind IS excluded.kind
             )",
            fparams![
                agent.slug.as_str(),
                agent.name.as_str(),
                agent.version.as_deref(),
                kind.as_str(),
                now,
                now
            ],
        )?;

        self.conn
            .query_row_map(
                "SELECT id FROM agents WHERE slug = ?1 LIMIT 1",
                fparams![agent.slug.as_str()],
                |row| row.get_typed(0),
            )
            .with_context(|| format!("fetching agent id for {}", agent.slug))
    }

    /// Ensure a workspace exists in the database, returning its ID.
    pub fn ensure_workspace(&self, path: &Path, display_name: Option<&str>) -> Result<i64> {
        let path_str = path.to_string_lossy().to_string();
        if let Some(display_name) = display_name {
            self.conn.execute_compat(
                "INSERT INTO workspaces(path, display_name)
                 VALUES(?1, ?2)
                 ON CONFLICT(path) DO UPDATE SET
                     display_name = excluded.display_name
                 WHERE NOT (workspaces.display_name IS excluded.display_name)",
                fparams![path_str.as_str(), display_name],
            )?;
        } else {
            self.conn.execute_compat(
                "INSERT OR IGNORE INTO workspaces(path, display_name) VALUES(?1, NULL)",
                fparams![path_str.as_str()],
            )?;
        }

        self.conn
            .query_row_map(
                "SELECT id FROM workspaces WHERE path = ?1 LIMIT 1",
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

    /// Get the timestamp of the last successful index completion.
    pub fn get_last_indexed_at(&self) -> Result<Option<i64>> {
        let result: Result<String, _> = self.conn.query_row_map(
            "SELECT value FROM meta WHERE key = 'last_indexed_at'",
            fparams![],
            |row| row.get_typed(0),
        );
        match result.optional() {
            Ok(Some(s)) => Ok(s.parse().ok()),
            Ok(None) => Ok(None),
            Err(e) => Err(e.into()),
        }
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

    /// Count all archived conversations.
    pub fn total_conversation_count(&self) -> Result<usize> {
        let count: i64 =
            self.conn
                .query_row_map("SELECT COUNT(*) FROM conversations", fparams![], |row| {
                    row.get_typed(0)
                })?;
        Ok(count.max(0) as usize)
    }

    /// Count all archived messages.
    pub fn total_message_count(&self) -> Result<usize> {
        let count: i64 =
            self.conn
                .query_row_map("SELECT COUNT(*) FROM messages", fparams![], |row| {
                    row.get_typed(0)
                })?;
        Ok(count.max(0) as usize)
    }

    /// Remove all archived conversations/messages for one agent slug.
    ///
    /// This only affects cass's local archive database. Source session files on
    /// disk are untouched.
    pub fn purge_agent_archive_data(&self, agent_slug: &str) -> Result<AgentArchivePurgeResult> {
        let normalized = agent_slug.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return Err(anyhow!("agent slug cannot be empty"));
        }

        let Some(agent_id) = self
            .conn
            .query_row_map(
                "SELECT id FROM agents WHERE slug = ?1",
                fparams![normalized.as_str()],
                |row| row.get_typed::<i64>(0),
            )
            .optional()?
        else {
            return Ok(AgentArchivePurgeResult::default());
        };

        let conversations_deleted: i64 = self.conn.query_row_map(
            "SELECT COUNT(*) FROM conversations WHERE agent_id = ?1",
            fparams![agent_id],
            |row| row.get_typed(0),
        )?;
        if conversations_deleted == 0 {
            return Ok(AgentArchivePurgeResult::default());
        }

        let messages_deleted: i64 = self.conn.query_row_map(
            "SELECT COUNT(*)
             FROM messages
             WHERE conversation_id IN (
                 SELECT id FROM conversations WHERE agent_id = ?1
             )",
            fparams![agent_id],
            |row| row.get_typed(0),
        )?;

        let mut tx = self.conn.transaction()?;
        tx.execute_compat(
            "DELETE FROM conversations WHERE agent_id = ?1",
            fparams![agent_id],
        )?;
        tx.execute_compat(
            "DELETE FROM agents
             WHERE id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM conversations WHERE agent_id = ?1
               )",
            fparams![agent_id],
        )?;
        tx.commit()?;

        Ok(AgentArchivePurgeResult {
            conversations_deleted: conversations_deleted.max(0) as usize,
            messages_deleted: messages_deleted.max(0) as usize,
        })
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
        // Avoid the multi-table JOIN with LIMIT/OFFSET that triggers
        // frankensqlite's materialization fallback (see c38edcd9, 860acb12).
        // Use correlated subqueries for the tiny agents (~20 rows) and
        // workspaces (~30 rows) lookup tables and degrade NULL agent_id to
        // the same 'unknown' sentinel that 8a0c547c established for the
        // lexical rebuild path.
        self.conn
            .query_map_collect(
                r"SELECT c.id,
                         COALESCE((SELECT a.slug FROM agents a WHERE a.id = c.agent_id), 'unknown'),
                         (SELECT w.path FROM workspaces w WHERE w.id = c.workspace_id),
                         c.external_id, c.title, c.source_path,
                         c.started_at, c.ended_at, c.approx_tokens, c.metadata_json,
                         c.source_id, c.origin_host, c.metadata_bin
                FROM conversations c
                ORDER BY CASE WHEN c.started_at IS NULL THEN 1 ELSE 0 END, c.started_at DESC, c.id DESC
                LIMIT ?1 OFFSET ?2",
                fparams![limit, offset],
                |row| {
                    let workspace_path: Option<String> = row.get_typed(2)?;
                    let source_path: String = row.get_typed(5)?;
                    let raw_source_id: Option<String> = row.get_typed(10)?;
                    let raw_origin_host: Option<String> = row.get_typed(11)?;
                    let (source_id, _, origin_host) = normalized_storage_source_parts(
                        raw_source_id.as_deref(),
                        None,
                        raw_origin_host.as_deref(),
                    );
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
                        source_id,
                        origin_host,
                    })
                },
            )
            .with_context(|| "listing conversations")
    }

    /// Build lookup maps for agents and workspaces to avoid JOINs in
    /// paged conversation queries.  Both tables are tiny (tens of rows)
    /// so this is effectively free.
    pub fn build_lexical_rebuild_lookups(
        &self,
    ) -> Result<(HashMap<i64, String>, HashMap<i64, PathBuf>)> {
        let agents: HashMap<i64, String> = self
            .conn
            .query_map_collect("SELECT id, slug FROM agents", fparams![], |row| {
                Ok((row.get_typed::<i64>(0)?, row.get_typed::<String>(1)?))
            })
            .with_context(|| "loading agent lookup for lexical rebuild")?
            .into_iter()
            .collect();
        let workspaces: HashMap<i64, PathBuf> = self
            .conn
            .query_map_collect("SELECT id, path FROM workspaces", fparams![], |row| {
                let path_str: String = row.get_typed(1)?;
                Ok((row.get_typed::<i64>(0)?, PathBuf::from(path_str)))
            })
            .with_context(|| "loading workspace lookup for lexical rebuild")?
            .into_iter()
            .collect();
        Ok((agents, workspaces))
    }

    /// List per-conversation message footprints in primary-key order.
    ///
    /// This deliberately avoids a conversations/messages JOIN because
    /// frankensqlite can fall back to full materialization on multi-table
    /// rebuild-path queries. Instead we merge two cheap ordered single-table
    /// scans: one over conversation ids and one grouped aggregate over messages.
    pub fn list_conversation_footprints_for_lexical_rebuild(
        &self,
    ) -> Result<Vec<LexicalRebuildConversationFootprintRow>> {
        let conversation_ids: Vec<i64> = self
            .conn
            .query_map_collect(
                "SELECT id FROM conversations ORDER BY id ASC",
                fparams![],
                |row| row.get_typed(0),
            )
            .with_context(|| "listing conversation ids for lexical rebuild footprints")?;
        let aggregate_rows: Vec<(i64, i64, i64)> = self
            .conn
            .query_map_collect(
                r"SELECT conversation_id,
                          COUNT(*),
                          COALESCE(SUM(LENGTH(CAST(content AS BLOB))), 0)
                   FROM messages
                   GROUP BY conversation_id
                   ORDER BY conversation_id ASC",
                fparams![],
                |row| Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?)),
            )
            .with_context(|| "aggregating lexical rebuild conversation footprints")?;

        let mut aggregates = aggregate_rows.into_iter().peekable();
        let mut footprints = Vec::with_capacity(conversation_ids.len());
        for conversation_id in conversation_ids {
            while let Some((aggregate_conversation_id, _, _)) = aggregates.peek() {
                if *aggregate_conversation_id < conversation_id {
                    aggregates.next();
                } else {
                    break;
                }
            }

            let (message_count, message_bytes) = match aggregates.peek() {
                Some((aggregate_conversation_id, _, _))
                    if *aggregate_conversation_id == conversation_id =>
                {
                    let (_, count, bytes) = aggregates
                        .next()
                        .expect("peeked lexical rebuild aggregate row should exist");
                    (
                        usize::try_from(count.max(0)).unwrap_or(usize::MAX),
                        usize::try_from(bytes.max(0)).unwrap_or(usize::MAX),
                    )
                }
                _ => (0, 0),
            };
            footprints.push(LexicalRebuildConversationFootprintRow {
                conversation_id,
                message_count,
                message_bytes,
            });
        }

        Ok(footprints)
    }

    /// Legacy OFFSET-based traversal for one-time checkpoint migration only.
    ///
    /// New code must use `list_conversations_for_lexical_rebuild_after_id`
    /// for keyset pagination.
    pub fn list_conversations_for_lexical_rebuild_by_offset(
        &self,
        limit: i64,
        offset: i64,
        agent_slugs: &HashMap<i64, String>,
        workspace_paths: &HashMap<i64, PathBuf>,
    ) -> Result<Vec<LexicalRebuildConversationRow>> {
        // Single-table query avoids the 3-table JOIN that triggers
        // frankensqlite's full-materialization fallback path.
        self.conn
            .query_map_collect(
                r"SELECT id, agent_id, workspace_id, external_id, title, source_path,
                       started_at, ended_at, source_id, origin_host
                FROM conversations
                ORDER BY id ASC
                LIMIT ?1 OFFSET ?2",
                fparams![limit, offset],
                |row| {
                    let agent_id: Option<i64> = row.get_typed(1)?;
                    let workspace_id: Option<i64> = row.get_typed(2)?;
                    let source_path: String = row.get_typed(5)?;
                    let raw_source_id: Option<String> = row.get_typed(8)?;
                    let raw_origin_host: Option<String> = row.get_typed(9)?;
                    let (source_id, _, origin_host) = normalized_storage_source_parts(
                        raw_source_id.as_deref(),
                        None,
                        raw_origin_host.as_deref(),
                    );
                    Ok(LexicalRebuildConversationRow {
                        id: Some(row.get_typed(0)?),
                        agent_slug: agent_id
                            .and_then(|aid| agent_slugs.get(&aid).cloned())
                            .unwrap_or_else(|| "unknown".to_string()),
                        workspace: workspace_id.and_then(|wid| workspace_paths.get(&wid).cloned()),
                        external_id: row.get_typed(3)?,
                        title: row.get_typed(4)?,
                        source_path: Path::new(&source_path).to_path_buf(),
                        started_at: row.get_typed(6)?,
                        ended_at: row.get_typed(7)?,
                        source_id,
                        origin_host,
                    })
                },
            )
            .with_context(|| "listing conversations for lexical rebuild")
    }

    /// List lexical rebuild conversations strictly after the given primary key.
    ///
    /// Keyset pagination keeps later rebuild pages as cheap as earlier ones,
    /// avoiding the ever-growing `OFFSET` scan cost during large rebuilds.
    pub fn list_conversations_for_lexical_rebuild_after_id(
        &self,
        limit: i64,
        after_conversation_id: i64,
        agent_slugs: &HashMap<i64, String>,
        workspace_paths: &HashMap<i64, PathBuf>,
    ) -> Result<Vec<LexicalRebuildConversationRow>> {
        self.conn
            .query_map_collect(
                r"SELECT id, agent_id, workspace_id, external_id, title, source_path,
                       started_at, ended_at, source_id, origin_host
                FROM conversations
                WHERE id > ?2
                ORDER BY id ASC
                LIMIT ?1",
                fparams![limit, after_conversation_id],
                |row| {
                    let agent_id: Option<i64> = row.get_typed(1)?;
                    let workspace_id: Option<i64> = row.get_typed(2)?;
                    let source_path: String = row.get_typed(5)?;
                    let raw_source_id: Option<String> = row.get_typed(8)?;
                    let raw_origin_host: Option<String> = row.get_typed(9)?;
                    let (source_id, _, origin_host) = normalized_storage_source_parts(
                        raw_source_id.as_deref(),
                        None,
                        raw_origin_host.as_deref(),
                    );
                    Ok(LexicalRebuildConversationRow {
                        id: Some(row.get_typed(0)?),
                        agent_slug: agent_id
                            .and_then(|aid| agent_slugs.get(&aid).cloned())
                            .unwrap_or_else(|| "unknown".to_string()),
                        workspace: workspace_id.and_then(|wid| workspace_paths.get(&wid).cloned()),
                        external_id: row.get_typed(3)?,
                        title: row.get_typed(4)?,
                        source_path: Path::new(&source_path).to_path_buf(),
                        started_at: row.get_typed(6)?,
                        ended_at: row.get_typed(7)?,
                        source_id,
                        origin_host,
                    })
                },
            )
            .with_context(|| {
                format!(
                    "listing conversations for lexical rebuild after id {after_conversation_id}"
                )
            })
    }

    /// List lexical rebuild conversations inside an `(after_id, through_id]`
    /// primary-key window.
    ///
    /// This lets the rebuild producer respect planned shard boundaries without
    /// falling back to client-side trimming or multi-table joins.
    pub fn list_conversations_for_lexical_rebuild_after_id_through_id(
        &self,
        limit: i64,
        after_conversation_id: i64,
        through_conversation_id: i64,
        agent_slugs: &HashMap<i64, String>,
        workspace_paths: &HashMap<i64, PathBuf>,
    ) -> Result<Vec<LexicalRebuildConversationRow>> {
        if through_conversation_id <= after_conversation_id {
            return Ok(Vec::new());
        }
        self.conn
            .query_map_collect(
                r"SELECT id, agent_id, workspace_id, external_id, title, source_path,
                       started_at, ended_at, source_id, origin_host
                FROM conversations
                WHERE id > ?2 AND id <= ?3
                ORDER BY id ASC
                LIMIT ?1",
                fparams![limit, after_conversation_id, through_conversation_id],
                |row| {
                    let agent_id: Option<i64> = row.get_typed(1)?;
                    let workspace_id: Option<i64> = row.get_typed(2)?;
                    let source_path: String = row.get_typed(5)?;
                    let raw_source_id: Option<String> = row.get_typed(8)?;
                    let raw_origin_host: Option<String> = row.get_typed(9)?;
                    let (source_id, _, origin_host) = normalized_storage_source_parts(
                        raw_source_id.as_deref(),
                        None,
                        raw_origin_host.as_deref(),
                    );
                    Ok(LexicalRebuildConversationRow {
                        id: Some(row.get_typed(0)?),
                        agent_slug: agent_id
                            .and_then(|aid| agent_slugs.get(&aid).cloned())
                            .unwrap_or_else(|| "unknown".to_string()),
                        workspace: workspace_id.and_then(|wid| workspace_paths.get(&wid).cloned()),
                        external_id: row.get_typed(3)?,
                        title: row.get_typed(4)?,
                        source_path: Path::new(&source_path).to_path_buf(),
                        started_at: row.get_typed(6)?,
                        ended_at: row.get_typed(7)?,
                        source_id,
                        origin_host,
                    })
                },
            )
            .with_context(|| {
                format!(
                    "listing conversations for lexical rebuild after id {after_conversation_id} through id {through_conversation_id}"
                )
            })
    }

    /// Fetch messages for a conversation.
    pub fn fetch_messages(&self, conversation_id: i64) -> Result<Vec<Message>> {
        let hinted_sql = "SELECT id, idx, role, author, created_at, content, extra_json, extra_bin \
             FROM messages INDEXED BY sqlite_autoindex_messages_1 \
             WHERE conversation_id = ?1 ORDER BY idx";
        let fallback_sql = "SELECT id, idx, role, author, created_at, content, extra_json, extra_bin \
             FROM messages \
             WHERE conversation_id = ?1 ORDER BY idx";

        self.conn
            .query_map_collect(hinted_sql, fparams![conversation_id], |row| {
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
            })
            .or_else(|err| {
                if err
                    .to_string()
                    .contains("no such index: sqlite_autoindex_messages_1")
                {
                    return self.conn.query_map_collect(
                        fallback_sql,
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
                    );
                }
                Err(err)
            })
            .with_context(|| format!("fetching messages for conversation {conversation_id}"))
    }

    /// Fetch messages for lexical index rebuilds without deserializing extra metadata.
    ///
    /// Tantivy only needs message text and core envelope fields, so avoiding
    /// `extra_json` here prevents rebuilds from rehydrating enormous historical
    /// payloads that are irrelevant to lexical search.
    pub fn fetch_messages_for_lexical_rebuild(&self, conversation_id: i64) -> Result<Vec<Message>> {
        let hinted_sql = "SELECT id, idx, role, author, created_at, content \
                 FROM messages INDEXED BY sqlite_autoindex_messages_1 \
                 WHERE conversation_id = ?1 ORDER BY idx";
        let fallback_sql = "SELECT id, idx, role, author, created_at, content \
                 FROM messages \
                 WHERE conversation_id = ?1 ORDER BY idx";

        self.conn
            .query_map_collect(hinted_sql, fparams![conversation_id], |row| {
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
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                })
            })
            .or_else(|err| {
                if err
                    .to_string()
                    .contains("no such index: sqlite_autoindex_messages_1")
                {
                    return self.conn.query_map_collect(
                        fallback_sql,
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
                                extra_json: serde_json::Value::Null,
                                snippets: Vec::new(),
                            })
                        },
                    );
                }
                Err(err)
            })
            .with_context(|| {
                format!("fetching messages for lexical rebuild of conversation {conversation_id}")
            })
    }

    /// Fetch messages for multiple conversations during lexical rebuilds.
    ///
    /// This preserves the lightweight lexical-rebuild projection while avoiding
    /// one round-trip per conversation when rebuilding large canonical indexes.
    pub fn fetch_messages_for_lexical_rebuild_batch(
        &self,
        conversation_ids: &[i64],
        max_messages: Option<usize>,
        max_content_bytes: Option<usize>,
    ) -> Result<HashMap<i64, Vec<Message>>> {
        if conversation_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut sql = String::from(
            "SELECT conversation_id, id, idx, role, author, created_at, content \
             FROM messages \
             WHERE conversation_id IN (",
        );
        let mut params: Vec<ParamValue> = Vec::with_capacity(conversation_ids.len());
        for (idx, conversation_id) in conversation_ids.iter().enumerate() {
            if idx > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&format!("?{}", idx + 1));
            params.push(ParamValue::from(*conversation_id));
        }
        sql.push_str(") ORDER BY conversation_id ASC, idx ASC");

        let mut grouped: HashMap<i64, Vec<Message>> =
            HashMap::with_capacity(conversation_ids.len());
        let mut total_messages = 0usize;
        let mut total_content_bytes = 0usize;
        let _: Vec<()> = self
            .conn
            .query_map_collect(&sql, &params, |row| {
                let role: String = row.get_typed(3)?;
                let conversation_id: i64 = row.get_typed(0)?;
                let content: String = row.get_typed(6)?;
                total_messages = total_messages.saturating_add(1);
                total_content_bytes = total_content_bytes.saturating_add(content.len());
                if let Some(limit) = max_messages
                    && total_messages > limit
                {
                    return Err(frankensqlite::FrankenError::Internal(format!(
                        "lexical rebuild batch fetch exceeded message guardrail: messages={total_messages} limit={limit} conversations={}",
                        conversation_ids.len()
                    )));
                }
                if let Some(limit) = max_content_bytes
                    && total_content_bytes > limit
                {
                    return Err(frankensqlite::FrankenError::Internal(format!(
                        "lexical rebuild batch fetch exceeded content-byte guardrail: bytes={total_content_bytes} limit={limit} conversations={}",
                        conversation_ids.len()
                    )));
                }

                let message = Message {
                    id: Some(row.get_typed(1)?),
                    idx: row.get_typed(2)?,
                    role: match role.as_str() {
                        "user" => MessageRole::User,
                        "agent" | "assistant" => MessageRole::Agent,
                        "tool" => MessageRole::Tool,
                        "system" => MessageRole::System,
                        other => MessageRole::Other(other.to_string()),
                    },
                    author: row.get_typed(4)?,
                    created_at: row.get_typed(5)?,
                    content,
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                };
                grouped.entry(conversation_id).or_default().push(message);
                Ok(())
            })
            .with_context(|| {
                format!(
                    "fetching lexical rebuild messages for {} conversations",
                    conversation_ids.len()
                )
            })?;
        Ok(grouped)
    }

    /// Stream lexical rebuild message rows in `(conversation_id, idx)` order
    /// without materializing the full result set.
    pub fn stream_messages_for_lexical_rebuild_between_conversation_ids<F>(
        &self,
        start_conversation_id: i64,
        end_conversation_id: i64,
        mut f: F,
    ) -> Result<()>
    where
        F: FnMut(LexicalRebuildMessageRow) -> Result<()>,
    {
        if end_conversation_id < start_conversation_id {
            return Ok(());
        }

        let conversation_ids: Vec<i64> = self
            .conn
            .query_map_collect(
                "SELECT id FROM conversations WHERE id >= ?1 AND id <= ?2 ORDER BY id ASC",
                fparams![start_conversation_id, end_conversation_id],
                |row| row.get_typed(0),
            )
            .with_context(|| "listing conversation ids for streamed lexical rebuild")?;

        for conversation_id in conversation_ids {
            let messages = self
                .fetch_messages_for_lexical_rebuild(conversation_id)
                .with_context(|| {
                    format!("streaming lexical rebuild messages for conversation {conversation_id}")
                })?;

            for message in messages {
                let message_id = message.id.ok_or_else(|| {
                    anyhow!(
                        "lexical rebuild message missing id for conversation {conversation_id} idx {}",
                        message.idx
                    )
                })?;
                f(LexicalRebuildMessageRow {
                    conversation_id,
                    id: message_id,
                    idx: message.idx,
                    role: role_str(&message.role),
                    author: message.author,
                    created_at: message.created_at,
                    content: message.content,
                })?;
            }
        }

        Ok(())
    }

    /// Stream grouped lexical rebuild message rows in `(conversation_id, idx)`
    /// order by reusing the canonical per-message stream and coalescing rows
    /// per conversation.
    pub fn stream_grouped_messages_for_lexical_rebuild_between_conversation_ids<F>(
        &self,
        start_conversation_id: i64,
        end_conversation_id: i64,
        mut f: F,
    ) -> Result<()>
    where
        F: FnMut(i64, LexicalRebuildGroupedMessageRows, i64) -> Result<()>,
    {
        if end_conversation_id < start_conversation_id {
            return Ok(());
        }

        let mut current_conversation_id: Option<i64> = None;
        let mut current_messages: LexicalRebuildGroupedMessageRows = SmallVec::new();
        let mut current_last_message_id = 0i64;
        let mut flush_current = |current_conversation_id: &mut Option<i64>,
                                 current_messages: &mut LexicalRebuildGroupedMessageRows,
                                 current_last_message_id: &mut i64|
         -> Result<()> {
            let Some(conversation_id) = current_conversation_id.take() else {
                return Ok(());
            };
            let messages = std::mem::take(current_messages);
            let last_message_id = std::mem::take(current_last_message_id);
            f(conversation_id, messages, last_message_id)
        };

        self.stream_messages_for_lexical_rebuild_between_conversation_ids(
            start_conversation_id,
            end_conversation_id,
            |row| {
                if current_conversation_id != Some(row.conversation_id) {
                    flush_current(
                        &mut current_conversation_id,
                        &mut current_messages,
                        &mut current_last_message_id,
                    )?;
                    current_conversation_id = Some(row.conversation_id);
                }
                current_last_message_id = row.id;
                current_messages.push(LexicalRebuildGroupedMessageRow {
                    idx: row.idx,
                    is_tool_role: row.role == "tool",
                    created_at: row.created_at,
                    content: row.content,
                });
                Ok(())
            },
        )
        .with_context(|| "streaming grouped lexical rebuild messages")?;

        flush_current(
            &mut current_conversation_id,
            &mut current_messages,
            &mut current_last_message_id,
        )
        .with_context(|| "flushing grouped lexical rebuild messages")
    }

    /// Stream grouped lexical rebuild message rows from a starting conversation
    /// id to the end of the table.
    pub fn stream_grouped_messages_for_lexical_rebuild_from_conversation_id<F>(
        &self,
        start_conversation_id: i64,
        f: F,
    ) -> Result<()>
    where
        F: FnMut(i64, LexicalRebuildGroupedMessageRows, i64) -> Result<()>,
    {
        self.stream_grouped_messages_for_lexical_rebuild_between_conversation_ids(
            start_conversation_id,
            i64::MAX,
            f,
        )
    }

    /// Stream lexical rebuild message rows from a starting conversation id to
    /// the end of the table.
    pub fn stream_messages_for_lexical_rebuild_from_conversation_id<F>(
        &self,
        start_conversation_id: i64,
        f: F,
    ) -> Result<()>
    where
        F: FnMut(LexicalRebuildMessageRow) -> Result<()>,
    {
        self.stream_messages_for_lexical_rebuild_between_conversation_ids(
            start_conversation_id,
            i64::MAX,
            f,
        )
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
        self.invalidate_conversation_source_cache(source.id.as_str());
        let now = Self::now_millis();
        let kind_str = source.kind.to_string();
        let config_json_str = source
            .config_json
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        // Re-indexing commonly reuses the same normalized source metadata
        // across many conversations. Skip the write entirely when the row is
        // already identical so we avoid needless WAL churn and timestamp bumps.
        self.conn.execute_compat(
            "INSERT INTO sources(id, kind, host_label, machine_id, platform, config_json, created_at, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                 kind = excluded.kind,
                 host_label = excluded.host_label,
                 machine_id = excluded.machine_id,
                 platform = excluded.platform,
                 config_json = excluded.config_json,
                 updated_at = excluded.updated_at
             WHERE NOT (
                 sources.kind IS excluded.kind
                 AND sources.host_label IS excluded.host_label
                 AND sources.machine_id IS excluded.machine_id
                 AND sources.platform IS excluded.platform
                 AND sources.config_json IS excluded.config_json
             )",
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

    fn historical_bundle_key_hash(
        version: u32,
        bundle: &HistoricalDatabaseBundle,
        include_bundle_stats: bool,
    ) -> String {
        let signature = if include_bundle_stats {
            format!(
                "{}:{}:{}:{}",
                version,
                bundle.root_path.display(),
                bundle.total_bytes,
                bundle.modified_at_ms
            )
        } else {
            format!("{}:{}", version, bundle.root_path.display())
        };
        blake3::hash(signature.as_bytes()).to_hex().to_string()
    }

    fn historical_bundle_meta_key(bundle: &HistoricalDatabaseBundle) -> String {
        format!(
            "historical_bundle_salvaged:{}",
            Self::historical_bundle_key_hash(HISTORICAL_SALVAGE_LEDGER_VERSION, bundle, false)
        )
    }

    fn historical_bundle_legacy_meta_key(bundle: &HistoricalDatabaseBundle) -> String {
        let signature = format!(
            "{}:{}:{}:{}",
            HISTORICAL_SALVAGE_LEDGER_VERSION,
            bundle.root_path.display(),
            bundle.total_bytes,
            bundle.modified_at_ms
        );
        format!(
            "historical_bundle_salvaged:{}",
            blake3::hash(signature.as_bytes()).to_hex()
        )
    }

    fn historical_bundle_progress_key(bundle: &HistoricalDatabaseBundle) -> String {
        format!(
            "historical_bundle_progress:{}",
            Self::historical_bundle_key_hash(HISTORICAL_SALVAGE_PROGRESS_VERSION, bundle, false)
        )
    }

    fn historical_bundle_legacy_progress_key(bundle: &HistoricalDatabaseBundle) -> String {
        let signature = format!(
            "{}:{}:{}:{}",
            HISTORICAL_SALVAGE_PROGRESS_VERSION,
            bundle.root_path.display(),
            bundle.total_bytes,
            bundle.modified_at_ms
        );
        format!(
            "historical_bundle_progress:{}",
            blake3::hash(signature.as_bytes()).to_hex()
        )
    }

    fn historical_bundle_already_imported(
        &self,
        bundle: &HistoricalDatabaseBundle,
    ) -> Result<bool> {
        for key in [
            Self::historical_bundle_meta_key(bundle),
            Self::historical_bundle_legacy_meta_key(bundle),
        ] {
            let existing: Option<String> = self
                .conn
                .query_row_map(
                    "SELECT value FROM meta WHERE key = ?1",
                    fparams![key.as_str()],
                    |row| row.get_typed(0),
                )
                .optional()?;
            if existing.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn has_pending_historical_bundles(&self, canonical_db_path: &Path) -> Result<bool> {
        for bundle in discover_historical_database_bundles(canonical_db_path) {
            if !self.historical_bundle_already_imported(&bundle)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn load_historical_bundle_progress(
        &self,
        bundle: &HistoricalDatabaseBundle,
    ) -> Result<Option<HistoricalBundleProgress>> {
        for key in [
            Self::historical_bundle_progress_key(bundle),
            Self::historical_bundle_legacy_progress_key(bundle),
        ] {
            let raw: Option<String> = self
                .conn
                .query_row_map(
                    "SELECT value FROM meta WHERE key = ?1",
                    fparams![key.as_str()],
                    |row| row.get_typed(0),
                )
                .optional()?;
            let Some(raw) = raw else {
                continue;
            };
            let parsed: HistoricalBundleProgress =
                serde_json::from_str(&raw).with_context(|| {
                    format!(
                        "parsing historical salvage progress checkpoint for {}",
                        bundle.root_path.display()
                    )
                })?;
            if parsed.progress_version == HISTORICAL_SALVAGE_PROGRESS_VERSION {
                return Ok(Some(parsed));
            }
        }
        Ok(None)
    }

    fn record_historical_bundle_progress(
        &self,
        bundle: &HistoricalDatabaseBundle,
        method: &str,
        last_completed_source_row_id: i64,
        conversations_imported: usize,
        messages_imported: usize,
    ) -> Result<()> {
        let key = Self::historical_bundle_progress_key(bundle);
        let value = HistoricalBundleProgress {
            progress_version: HISTORICAL_SALVAGE_PROGRESS_VERSION,
            path: bundle.root_path.display().to_string(),
            bytes: bundle.total_bytes,
            modified_at_ms: bundle.modified_at_ms,
            method: method.to_string(),
            last_completed_source_row_id,
            conversations_imported,
            messages_imported,
            updated_at_ms: Self::now_millis(),
        };
        let value_str = serde_json::to_string(&value)?;
        self.conn.execute_compat(
            "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
            fparams![key.as_str(), value_str.as_str()],
        )?;
        Ok(())
    }

    fn clear_historical_bundle_progress(&self, bundle: &HistoricalDatabaseBundle) -> Result<()> {
        for key in [
            Self::historical_bundle_progress_key(bundle),
            Self::historical_bundle_legacy_progress_key(bundle),
        ] {
            self.conn
                .execute_compat("DELETE FROM meta WHERE key = ?1", fparams![key.as_str()])?;
        }
        Ok(())
    }

    fn record_historical_bundle_import(
        &self,
        bundle: &HistoricalDatabaseBundle,
        method: &str,
        conversations_imported: usize,
        messages_imported: usize,
    ) -> Result<()> {
        let key = Self::historical_bundle_meta_key(bundle);
        let value = serde_json::json!({
            "salvage_version": HISTORICAL_SALVAGE_LEDGER_VERSION,
            "path": bundle.root_path.display().to_string(),
            "bytes": bundle.total_bytes,
            "modified_at_ms": bundle.modified_at_ms,
            "method": method,
            "conversations_imported": conversations_imported,
            "messages_imported": messages_imported,
            "recorded_at_ms": Self::now_millis(),
        });
        let value_str = serde_json::to_string(&value)?;
        self.conn.execute_compat(
            "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
            fparams![key.as_str(), value_str.as_str()],
        )?;
        Ok(())
    }

    fn historical_import_error_is_split_retryable(err: &anyhow::Error) -> bool {
        const RETRYABLE_PATTERNS: &[&str] = &[
            "out of memory",
            "string or blob too big",
            "too many sql variables",
        ];
        err.chain().any(|cause| {
            let rendered = cause.to_string().to_ascii_lowercase();
            RETRYABLE_PATTERNS
                .iter()
                .any(|pattern| rendered.contains(pattern))
        })
    }

    fn split_historical_batch_entry_messages(
        entry: &HistoricalBatchEntry,
    ) -> Option<(HistoricalBatchEntry, HistoricalBatchEntry)> {
        if entry.conversation.messages.len() < 2 {
            return None;
        }
        let split_at = entry.conversation.messages.len() / 2;
        if split_at == 0 || split_at >= entry.conversation.messages.len() {
            return None;
        }

        let mut left = entry.clone();
        left.conversation.messages = entry.conversation.messages[..split_at].to_vec();

        let mut right = entry.clone();
        right.conversation.messages = entry.conversation.messages[split_at..].to_vec();

        Some((left, right))
    }

    fn import_historical_batch_with_retry<F>(
        entries: &[HistoricalBatchEntry],
        insert_batch: &mut F,
    ) -> Result<HistoricalBatchImportTotals>
    where
        F: FnMut(&[HistoricalBatchEntry]) -> Result<HistoricalBatchImportTotals>,
    {
        match insert_batch(entries) {
            Ok(totals) => Ok(totals),
            Err(err) if Self::historical_import_error_is_split_retryable(&err) => {
                if entries.len() > 1 {
                    let mid = entries.len() / 2;
                    tracing::warn!(
                        batch_entries = entries.len(),
                        split_left = mid,
                        split_right = entries.len() - mid,
                        error = %err,
                        "historical salvage batch failed; retrying in smaller sub-batches"
                    );
                    let left =
                        Self::import_historical_batch_with_retry(&entries[..mid], insert_batch)?;
                    let right =
                        Self::import_historical_batch_with_retry(&entries[mid..], insert_batch)?;
                    return Ok(HistoricalBatchImportTotals {
                        inserted_source_rows: left.inserted_source_rows
                            + right.inserted_source_rows,
                        inserted_messages: left.inserted_messages + right.inserted_messages,
                    });
                }

                if let Some(entry) = entries.first()
                    && let Some((left, right)) = Self::split_historical_batch_entry_messages(entry)
                {
                    tracing::warn!(
                        source_row_id = entry.source_row_id,
                        message_count = entry.conversation.messages.len(),
                        error = %err,
                        "historical salvage conversation failed; retrying in smaller message slices"
                    );
                    let left_totals = Self::import_historical_batch_with_retry(
                        std::slice::from_ref(&left),
                        insert_batch,
                    )?;
                    let right_totals = Self::import_historical_batch_with_retry(
                        std::slice::from_ref(&right),
                        insert_batch,
                    )?;
                    return Ok(HistoricalBatchImportTotals {
                        inserted_source_rows: usize::from(
                            left_totals.inserted_source_rows > 0
                                || right_totals.inserted_source_rows > 0,
                        ),
                        inserted_messages: left_totals
                            .inserted_messages
                            .saturating_add(right_totals.inserted_messages),
                    });
                }

                Err(err)
            }
            Err(err) => Err(err),
        }
    }

    fn import_historical_sources(&self, source_conn: &FrankenConnection) -> Result<()> {
        let sources: Vec<Source> = match source_conn.query_map_collect(
            "SELECT id, kind, host_label, machine_id, platform, config_json, created_at, updated_at
             FROM sources",
            fparams![],
            |row| {
                let raw_source_id: String = row.get_typed(0)?;
                let kind_str: String = row.get_typed(1)?;
                let raw_host_label: Option<String> = row.get_typed(2)?;
                let config_json_raw: Option<String> = row.get_typed(5)?;
                let (source_id, source_kind, host_label) = normalized_storage_source_parts(
                    Some(raw_source_id.as_str()),
                    Some(kind_str.as_str()),
                    raw_host_label.as_deref(),
                );
                Ok(Source {
                    id: source_id,
                    kind: source_kind,
                    host_label,
                    machine_id: row.get_typed(3)?,
                    platform: row.get_typed(4)?,
                    config_json: config_json_raw.and_then(|raw| serde_json::from_str(&raw).ok()),
                    created_at: row.get_typed(6)?,
                    updated_at: row.get_typed(7)?,
                })
            },
        ) {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(error = %err, "historical sources table unavailable; skipping source import");
                return Ok(());
            }
        };

        for source in sources {
            self.upsert_source(&source)?;
        }
        Ok(())
    }

    fn import_historical_conversations(
        &self,
        bundle: &HistoricalDatabaseBundle,
        salvage_method: &str,
        source_conn: &FrankenConnection,
    ) -> Result<(usize, usize)> {
        let batch_limits = historical_import_batch_limits();
        let cache_enabled = IndexingCache::is_enabled();
        let mut indexing_cache = IndexingCache::new();
        let mut known_sources: HashSet<String> = self
            .list_sources()?
            .into_iter()
            .map(|source| source.id)
            .collect();
        let resume_progress = self.load_historical_bundle_progress(bundle)?;
        let resume_after_row_id = resume_progress
            .as_ref()
            .map(|progress| progress.last_completed_source_row_id)
            .filter(|row_id| *row_id > 0);

        tracing::info!(
            target: "cass::historical_salvage",
            batch_conversations = batch_limits.conversations,
            batch_messages = batch_limits.messages,
            batch_payload_chars = batch_limits.payload_chars,
            cache_enabled,
            resume_after_row_id,
            "configured historical salvage batch limits"
        );

        if let Some(progress) = &resume_progress {
            tracing::info!(
                target: "cass::historical_salvage",
                path = %bundle.root_path.display(),
                resume_after_row_id = progress.last_completed_source_row_id,
                prior_conversations_imported = progress.conversations_imported,
                prior_messages_imported = progress.messages_imported,
                "resuming historical salvage bundle from durable checkpoint"
            );
        }

        // LEFT JOIN + COALESCE on agents so legacy source databases with NULL
        // agent_id (the V1 schema did not require NOT NULL) still have their
        // conversations imported, degrading to 'unknown' slug like the other
        // rebuild paths.  Using INNER JOIN here would silently drop those
        // conversations during historical salvage, which is data loss.
        let conv_sql = if resume_after_row_id.is_some() {
            "SELECT
                c.id,
                COALESCE(a.slug, 'unknown'),
                w.path,
                c.external_id,
                c.title,
                c.source_path,
                c.started_at,
                c.ended_at,
                c.approx_tokens,
                c.metadata_json,
                c.source_id,
                c.origin_host
             FROM conversations c
             LEFT JOIN agents a ON c.agent_id = a.id
             LEFT JOIN workspaces w ON c.workspace_id = w.id
             WHERE c.id > ?1
             ORDER BY c.id"
        } else {
            "SELECT
                c.id,
                COALESCE(a.slug, 'unknown'),
                w.path,
                c.external_id,
                c.title,
                c.source_path,
                c.started_at,
                c.ended_at,
                c.approx_tokens,
                c.metadata_json,
                c.source_id,
                c.origin_host
             FROM conversations c
             LEFT JOIN agents a ON c.agent_id = a.id
             LEFT JOIN workspaces w ON c.workspace_id = w.id
             ORDER BY c.id"
        };
        let conv_params: &[ParamValue] =
            if let Some(last_completed_source_row_id) = resume_after_row_id {
                &[ParamValue::from(last_completed_source_row_id)]
            } else {
                &[]
            };

        #[allow(clippy::type_complexity)]
        let conv_rows: Vec<(
            i64,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<String>,
        )> = source_conn
            .query_map_collect(conv_sql, conv_params, |row| {
                Ok((
                    row.get_typed::<i64>(0)?,
                    row.get_typed::<String>(1)?,
                    row.get_typed::<Option<String>>(2)?,
                    row.get_typed::<Option<String>>(3)?,
                    row.get_typed::<Option<String>>(4)?,
                    row.get_typed::<String>(5)?,
                    row.get_typed::<Option<i64>>(6)?,
                    row.get_typed::<Option<i64>>(7)?,
                    row.get_typed::<Option<i64>>(8)?,
                    row.get_typed::<Option<String>>(9)?,
                    row.get_typed::<Option<String>>(10)?,
                    row.get_typed::<Option<String>>(11)?,
                ))
            })
            .context("querying historical conversations")?;

        let msg_sql = "SELECT idx, role, author, created_at, content, extra_json
             FROM messages
             WHERE conversation_id = ?1
             ORDER BY idx";

        let mut imported_conversations = resume_progress
            .as_ref()
            .map(|progress| progress.conversations_imported)
            .unwrap_or(0);
        let mut imported_messages = resume_progress
            .as_ref()
            .map(|progress| progress.messages_imported)
            .unwrap_or(0);
        let mut pending_batch: Vec<HistoricalBatchEntry> = Vec::new();
        let mut pending_batch_messages = 0usize;
        let mut pending_batch_chars = 0usize;
        let mut pending_batch_first_row_id: Option<i64> = None;
        let mut pending_batch_last_row_id: Option<i64> = None;

        let flush_batch = |storage: &FrankenStorage,
                           batch: &mut Vec<HistoricalBatchEntry>,
                           pending_messages: &mut usize,
                           pending_chars: &mut usize,
                           first_row_id: &mut Option<i64>,
                           last_row_id: &mut Option<i64>,
                           imported_conversations: &mut usize,
                           imported_messages: &mut usize|
         -> Result<()> {
            if batch.is_empty() {
                return Ok(());
            }

            let batch_first_row_id = *first_row_id;
            let batch_last_row_id = *last_row_id;
            if historical_salvage_debug_enabled() {
                eprintln!(
                    "[historical-salvage] flushing batch rows {:?}..{:?} conversations={} messages={} payload_chars={}",
                    batch_first_row_id,
                    batch_last_row_id,
                    batch.len(),
                    *pending_messages,
                    *pending_chars
                );
            }
            tracing::info!(
                target: "cass::historical_salvage",
                batch_conversations = batch.len(),
                batch_messages = *pending_messages,
                batch_payload_chars = *pending_chars,
                first_source_row_id = batch_first_row_id,
                last_source_row_id = batch_last_row_id,
                "flushing historical salvage batch"
            );

            let mut insert_batch =
                |entries: &[HistoricalBatchEntry]| -> Result<HistoricalBatchImportTotals> {
                    let borrowed_batch: Vec<(i64, Option<i64>, &Conversation)> = entries
                        .iter()
                        .map(|entry| (entry.agent_id, entry.workspace_id, &entry.conversation))
                        .collect();
                    let outcomes = storage
                        .insert_conversations_batched(&borrowed_batch)
                        .with_context(|| {
                            let first_source_row_id =
                                entries.first().map(|entry| entry.source_row_id);
                            let last_source_row_id =
                                entries.last().map(|entry| entry.source_row_id);
                            format!(
                                "inserting historical salvage batch source rows {:?}..{:?}",
                                first_source_row_id, last_source_row_id
                            )
                        })?;
                    let mut totals = HistoricalBatchImportTotals::default();
                    for outcome in outcomes {
                        if !outcome.inserted_indices.is_empty() {
                            totals.inserted_source_rows += 1;
                            totals.inserted_messages += outcome.inserted_indices.len();
                        }
                    }
                    Ok(totals)
                };
            let totals =
                Self::import_historical_batch_with_retry(batch.as_slice(), &mut insert_batch)?;
            *imported_conversations =
                (*imported_conversations).saturating_add(totals.inserted_source_rows);
            *imported_messages = (*imported_messages).saturating_add(totals.inserted_messages);
            if let Some(last_completed_row_id) = batch_last_row_id {
                storage.record_historical_bundle_progress(
                    bundle,
                    salvage_method,
                    last_completed_row_id,
                    *imported_conversations,
                    *imported_messages,
                )?;
            }
            tracing::info!(
                target: "cass::historical_salvage",
                batch_conversations = batch.len(),
                batch_messages = *pending_messages,
                imported_conversations = *imported_conversations,
                imported_messages = *imported_messages,
                first_source_row_id = batch_first_row_id,
                last_source_row_id = batch_last_row_id,
                "historical salvage batch committed"
            );
            if historical_salvage_debug_enabled() {
                eprintln!(
                    "[historical-salvage] committed batch rows {:?}..{:?} imported_conversations={} imported_messages={}",
                    batch_first_row_id,
                    batch_last_row_id,
                    *imported_conversations,
                    *imported_messages
                );
            }
            batch.clear();
            *pending_messages = 0;
            *pending_chars = 0;
            *first_row_id = None;
            *last_row_id = None;
            Ok(())
        };

        for (
            conversation_row_id,
            agent_slug,
            workspace_path,
            external_id,
            title,
            source_path,
            started_at,
            ended_at,
            approx_tokens,
            metadata_json_raw,
            raw_source_id,
            raw_origin_host,
        ) in conv_rows
        {
            let source_id = crate::search::tantivy::normalized_index_source_id(
                raw_source_id.as_deref(),
                None,
                raw_origin_host.as_deref(),
            );
            let origin_host =
                crate::search::tantivy::normalized_index_origin_host(raw_origin_host.as_deref());

            let messages: Vec<Message> = source_conn
                .query_map_collect(msg_sql, fparams![conversation_row_id], |msg_row| {
                    let role: String = msg_row.get_typed(1)?;
                    Ok(Message {
                        id: None,
                        idx: msg_row.get_typed(0)?,
                        role: match role.as_str() {
                            "user" => MessageRole::User,
                            "agent" | "assistant" => MessageRole::Agent,
                            "tool" => MessageRole::Tool,
                            "system" => MessageRole::System,
                            other => MessageRole::Other(other.to_string()),
                        },
                        author: msg_row.get_typed(2)?,
                        created_at: msg_row.get_typed(3)?,
                        content: msg_row.get_typed(4)?,
                        extra_json: parse_historical_json_column(msg_row.get_typed(5)?),
                        snippets: Vec::new(),
                    })
                })
                .context("collecting historical message rows")?;

            if messages.is_empty() {
                continue;
            }

            let conversation_message_count = messages.len();
            let conversation_chars = messages
                .iter()
                .map(message_payload_size_hint)
                .sum::<usize>();

            let conversation = Conversation {
                id: None,
                agent_slug: agent_slug.clone(),
                workspace: workspace_path.map(PathBuf::from),
                external_id,
                title,
                source_path: PathBuf::from(source_path),
                started_at,
                ended_at,
                approx_tokens,
                metadata_json: parse_json_column(metadata_json_raw),
                messages,
                source_id,
                origin_host,
            };

            if !known_sources.contains(&conversation.source_id) {
                let placeholder = if conversation.source_id == LOCAL_SOURCE_ID {
                    Source::local()
                } else {
                    Source {
                        id: conversation.source_id.clone(),
                        kind: SourceKind::Ssh,
                        host_label: conversation.origin_host.clone(),
                        machine_id: None,
                        platform: None,
                        config_json: None,
                        created_at: None,
                        updated_at: None,
                    }
                };
                self.upsert_source(&placeholder)?;
                known_sources.insert(conversation.source_id.clone());
            }

            let agent = Agent {
                id: None,
                slug: agent_slug.clone(),
                name: agent_slug,
                version: None,
                kind: AgentKind::Cli,
            };
            let agent_id = if cache_enabled {
                indexing_cache.get_or_insert_agent(self, &agent)?
            } else {
                self.ensure_agent(&agent)?
            };
            let workspace_id = if let Some(workspace) = &conversation.workspace {
                if cache_enabled {
                    Some(indexing_cache.get_or_insert_workspace(self, workspace, None)?)
                } else {
                    Some(self.ensure_workspace(workspace, None)?)
                }
            } else {
                None
            };

            let exceeds_pending_limits = !pending_batch.is_empty()
                && (pending_batch.len() >= batch_limits.conversations
                    || pending_batch_messages.saturating_add(conversation_message_count)
                        > batch_limits.messages
                    || pending_batch_chars.saturating_add(conversation_chars)
                        > batch_limits.payload_chars);
            if exceeds_pending_limits {
                flush_batch(
                    self,
                    &mut pending_batch,
                    &mut pending_batch_messages,
                    &mut pending_batch_chars,
                    &mut pending_batch_first_row_id,
                    &mut pending_batch_last_row_id,
                    &mut imported_conversations,
                    &mut imported_messages,
                )?;
            }

            if pending_batch_first_row_id.is_none() {
                pending_batch_first_row_id = Some(conversation_row_id);
            }
            pending_batch_last_row_id = Some(conversation_row_id);
            pending_batch_messages =
                pending_batch_messages.saturating_add(conversation_message_count);
            pending_batch_chars = pending_batch_chars.saturating_add(conversation_chars);
            pending_batch.push(HistoricalBatchEntry {
                source_row_id: conversation_row_id,
                agent_id,
                workspace_id,
                conversation,
            });

            if pending_batch.len() >= batch_limits.conversations
                || pending_batch_messages >= batch_limits.messages
                || pending_batch_chars >= batch_limits.payload_chars
            {
                flush_batch(
                    self,
                    &mut pending_batch,
                    &mut pending_batch_messages,
                    &mut pending_batch_chars,
                    &mut pending_batch_first_row_id,
                    &mut pending_batch_last_row_id,
                    &mut imported_conversations,
                    &mut imported_messages,
                )?;
            }
        }

        flush_batch(
            self,
            &mut pending_batch,
            &mut pending_batch_messages,
            &mut pending_batch_chars,
            &mut pending_batch_first_row_id,
            &mut pending_batch_last_row_id,
            &mut imported_conversations,
            &mut imported_messages,
        )?;

        if cache_enabled {
            let (hits, misses, hit_rate) = indexing_cache.stats();
            tracing::info!(
                target: "cass::historical_salvage",
                hits,
                misses,
                hit_rate = format!("{:.1}%", hit_rate * 100.0),
                agents = indexing_cache.agent_count(),
                workspaces = indexing_cache.workspace_count(),
                sources = known_sources.len(),
                "historical salvage cache stats"
            );
        }

        Ok((imported_conversations, imported_messages))
    }

    pub fn salvage_historical_databases(
        &self,
        canonical_db_path: &Path,
    ) -> Result<HistoricalSalvageOutcome> {
        let ordered_bundles = discover_historical_database_bundles(canonical_db_path);
        let mut outcome = HistoricalSalvageOutcome {
            bundles_considered: ordered_bundles.len(),
            ..HistoricalSalvageOutcome::default()
        };

        for bundle in ordered_bundles {
            if self.historical_bundle_already_imported(&bundle)? {
                self.clear_historical_bundle_progress(&bundle)?;
                continue;
            }

            let source = match open_historical_bundle_for_salvage(&bundle).with_context(|| {
                format!(
                    "opening historical bundle {} for salvage",
                    bundle.root_path.display()
                )
            }) {
                Ok(source) => source,
                Err(err) => {
                    tracing::warn!(
                        path = %bundle.root_path.display(),
                        error = %err,
                        "skipping unreadable historical cass database bundle during salvage"
                    );
                    self.clear_historical_bundle_progress(&bundle)?;
                    continue;
                }
            };

            self.import_historical_sources(&source.conn)?;
            let (imported_conversations, imported_messages) =
                self.import_historical_conversations(&bundle, source.method, &source.conn)?;
            self.record_historical_bundle_import(
                &bundle,
                source.method,
                imported_conversations,
                imported_messages,
            )?;
            self.clear_historical_bundle_progress(&bundle)?;

            outcome.bundles_imported += 1;
            outcome.conversations_imported += imported_conversations;
            outcome.messages_imported += imported_messages;

            tracing::info!(
                path = %bundle.root_path.display(),
                bytes = bundle.total_bytes,
                method = source.method,
                imported_conversations,
                imported_messages,
                "salvaged historical cass database bundle"
            );
        }

        Ok(outcome)
    }

    /// Delete a source by ID. Returns true if a row was deleted.
    pub fn delete_source(&self, id: &str, _cascade: bool) -> Result<bool> {
        if id == LOCAL_SOURCE_ID {
            anyhow::bail!("cannot delete the local source");
        }
        let count = self
            .conn
            .execute_compat("DELETE FROM sources WHERE id = ?1", fparams![id])?;
        if count > 0 {
            self.invalidate_conversation_source_cache(id);
        }
        Ok(count > 0)
    }

    /// Insert a conversation tree (conversation + messages + snippets + FTS).
    pub fn insert_conversation_tree(
        &self,
        agent_id: i64,
        workspace_id: Option<i64>,
        conv: &Conversation,
    ) -> Result<InsertOutcome> {
        let normalized_conv = normalized_conversation_for_storage(conv);
        let conv = normalized_conv.as_ref();
        self.ensure_source_for_conversation(conv)?;
        let defer_lexical_updates = defer_storage_lexical_updates_enabled();
        let defer_analytics_updates = defer_analytics_updates_enabled();
        let conversation_key = conversation_merge_key(agent_id, conv);
        let mut tx = self.conn.transaction()?;
        let existing =
            franken_find_existing_conversation_by_key(&tx, &conversation_key, Some(conv))?;
        if let Some(existing_id) = existing {
            let outcome = self.franken_append_messages_in_tx(&tx, existing_id, conv)?;
            tx.commit()?;
            return Ok(outcome);
        }

        let conv_id = match franken_insert_conversation_or_get_existing_after_miss(
            &tx,
            agent_id,
            workspace_id,
            conv,
            &conversation_key,
        )? {
            ConversationInsertStatus::Inserted(conv_id) => conv_id,
            ConversationInsertStatus::Existing(existing_id) => {
                let ExistingMessageLookup {
                    by_idx: mut existing_messages,
                    replay: mut existing_replay_fingerprints,
                } = franken_existing_message_lookup(&tx, existing_id, &conv.messages)?;
                let ExistingConversationNewMessages {
                    messages: new_messages,
                    new_chars,
                    idx_collision_count,
                    first_collision_idx,
                } = collect_new_messages_for_existing_conversation(
                    existing_id,
                    conv,
                    &mut existing_messages,
                    &mut existing_replay_fingerprints,
                    "skipping replay-equivalent recovered message with shifted idx",
                );
                let (inserted_last_idx, inserted_last_created_at) =
                    borrowed_messages_tail_state(&new_messages);
                let mut inserted_indices = Vec::new();
                let mut fts_entries = Vec::new();
                let mut fts_pending_chars = 0usize;
                let mut _fts_inserted_total = 0usize;
                let inserted_message_ids =
                    franken_append_insert_new_messages(&tx, existing_id, &new_messages)?;
                for (msg_id, msg) in inserted_message_ids.into_iter().zip(new_messages) {
                    franken_insert_snippets(&tx, msg_id, &msg.snippets)?;
                    if !defer_lexical_updates {
                        fts_entries.push(FtsEntry::from_message(msg_id, msg, conv));
                        fts_pending_chars = fts_pending_chars.saturating_add(msg.content.len());
                        if fts_entries.len() >= FTS_ENTRY_BATCH_MAX_DOCS
                            || fts_pending_chars >= FTS_ENTRY_BATCH_MAX_CHARS
                        {
                            flush_pending_fts_entries(
                                self,
                                &tx,
                                &mut fts_entries,
                                &mut fts_pending_chars,
                                &mut _fts_inserted_total,
                            )?;
                        }
                    }
                    inserted_indices.push(msg.idx);
                }

                if idx_collision_count > 0 {
                    tracing::warn!(
                        conversation_id = existing_id,
                        collision_count = idx_collision_count,
                        first_idx = first_collision_idx,
                        source_path = %conv.source_path.display(),
                        "message idx collisions encountered while merging recovered conversation; retaining canonical message variants"
                    );
                }

                if !defer_lexical_updates {
                    flush_pending_fts_entries(
                        self,
                        &tx,
                        &mut fts_entries,
                        &mut fts_pending_chars,
                        &mut _fts_inserted_total,
                    )?;
                }

                let conv_last_ts = conv.messages.iter().filter_map(|m| m.created_at).max();
                franken_update_conversation_tail_state(
                    &tx,
                    existing_id,
                    conv_last_ts,
                    inserted_last_idx,
                    inserted_last_created_at,
                )?;

                if !defer_analytics_updates && !inserted_indices.is_empty() {
                    franken_update_daily_stats_in_tx(
                        self,
                        &tx,
                        &conv.agent_slug,
                        &conv.source_id,
                        conversation_effective_started_at(conv),
                        StatsDelta {
                            session_count_delta: 0,
                            message_count_delta: inserted_indices.len() as i64,
                            total_chars_delta: new_chars,
                        },
                    )?;
                }

                tx.commit()?;
                return Ok(InsertOutcome {
                    conversation_id: existing_id,
                    conversation_inserted: false,
                    inserted_indices,
                });
            }
        };
        let mut fts_entries = Vec::new();
        let mut fts_pending_chars = 0usize;
        let mut _fts_inserted_total = 0usize;
        let mut total_chars: i64 = 0;
        let mut inserted_indices = Vec::new();
        let mut pending_messages = HashMap::new();
        let mut pending_replay_fingerprints = HashSet::new();
        let mut idx_collision_count = 0usize;
        let mut first_collision_idx: Option<i64> = None;
        let mut new_messages = Vec::new();
        for msg in &conv.messages {
            let incoming_fingerprint = message_merge_fingerprint(msg);
            if let Some(existing_fingerprint) = pending_messages.get(&msg.idx) {
                if existing_fingerprint != &incoming_fingerprint {
                    idx_collision_count = idx_collision_count.saturating_add(1);
                    first_collision_idx.get_or_insert(msg.idx);
                }
                continue;
            }
            let incoming_replay = message_replay_fingerprint(msg);
            if pending_replay_fingerprints.contains(&incoming_replay) {
                tracing::debug!(
                    conversation_id = conv_id,
                    idx = msg.idx,
                    source_path = %conv.source_path.display(),
                    "skipping replay-equivalent duplicate message within new conversation insert"
                );
                continue;
            }
            pending_messages.insert(msg.idx, incoming_fingerprint);
            pending_replay_fingerprints.insert(incoming_replay);
            new_messages.push(msg);
        }
        let inserted_message_ids = franken_batch_insert_new_messages(&tx, conv_id, &new_messages)?;
        for (msg_id, msg) in inserted_message_ids.into_iter().zip(new_messages) {
            franken_insert_snippets(&tx, msg_id, &msg.snippets)?;
            if !defer_lexical_updates {
                fts_entries.push(FtsEntry::from_message(msg_id, msg, conv));
                fts_pending_chars = fts_pending_chars.saturating_add(msg.content.len());
                if fts_entries.len() >= FTS_ENTRY_BATCH_MAX_DOCS
                    || fts_pending_chars >= FTS_ENTRY_BATCH_MAX_CHARS
                {
                    flush_pending_fts_entries(
                        self,
                        &tx,
                        &mut fts_entries,
                        &mut fts_pending_chars,
                        &mut _fts_inserted_total,
                    )?;
                }
            }
            total_chars += msg.content.len() as i64;
            inserted_indices.push(msg.idx);
        }
        if idx_collision_count > 0 {
            tracing::warn!(
                conversation_id = conv_id,
                collision_count = idx_collision_count,
                first_idx = first_collision_idx,
                source_path = %conv.source_path.display(),
                "message idx collisions encountered while inserting a new conversation; retaining the first canonical variant per idx"
            );
        }
        if !defer_lexical_updates {
            flush_pending_fts_entries(
                self,
                &tx,
                &mut fts_entries,
                &mut fts_pending_chars,
                &mut _fts_inserted_total,
            )?;
        }

        if !defer_analytics_updates {
            franken_update_daily_stats_in_tx(
                self,
                &tx,
                &conv.agent_slug,
                &conv.source_id,
                conversation_effective_started_at(conv),
                StatsDelta {
                    session_count_delta: 1,
                    message_count_delta: inserted_indices.len() as i64,
                    total_chars_delta: total_chars,
                },
            )?;
        }

        tx.commit()?;
        Ok(InsertOutcome {
            conversation_id: conv_id,
            conversation_inserted: true,
            inserted_indices,
        })
    }

    #[cfg(test)]
    fn insert_conversation_tree_with_profile(
        &self,
        agent_id: i64,
        workspace_id: Option<i64>,
        conv: &Conversation,
        profile: &mut InsertConversationTreePerfProfile,
    ) -> Result<InsertOutcome> {
        let total_start = Instant::now();
        let normalized_conv = normalized_conversation_for_storage(conv);
        let conv = normalized_conv.as_ref();

        let source_start = Instant::now();
        self.ensure_source_for_conversation(conv)?;
        profile.source_duration += source_start.elapsed();

        let defer_lexical_updates = defer_storage_lexical_updates_enabled();
        let defer_analytics_updates = defer_analytics_updates_enabled();
        let conversation_key = conversation_merge_key(agent_id, conv);

        let tx_open_start = Instant::now();
        let mut tx = self.conn.transaction()?;
        profile.tx_open_duration += tx_open_start.elapsed();

        let existing_lookup_start = Instant::now();
        let existing =
            franken_find_existing_conversation_by_key(&tx, &conversation_key, Some(conv))?;
        profile.existing_lookup_duration += existing_lookup_start.elapsed();
        if let Some(existing_id) = existing {
            return Err(anyhow!(
                "profile helper expects new conversation path, found existing id {existing_id}"
            ));
        }

        let conversation_row_start = Instant::now();
        let conv_id = match franken_insert_conversation_or_get_existing_after_miss(
            &tx,
            agent_id,
            workspace_id,
            conv,
            &conversation_key,
        )? {
            ConversationInsertStatus::Inserted(conv_id) => conv_id,
            ConversationInsertStatus::Existing(existing_id) => {
                return Err(anyhow!(
                    "profile helper expected inserted conversation row, reused existing id {existing_id}"
                ));
            }
        };
        profile.conversation_row_duration += conversation_row_start.elapsed();

        let mut fts_entries = Vec::new();
        let mut fts_pending_chars = 0usize;
        let mut fts_inserted_total = 0usize;
        let mut total_chars: i64 = 0;
        let mut inserted_indices = Vec::new();
        let mut pending_messages = HashMap::new();
        let mut pending_replay_fingerprints = HashSet::new();
        let mut idx_collision_count = 0usize;
        let mut first_collision_idx: Option<i64> = None;
        let mut new_messages = Vec::new();

        for msg in &conv.messages {
            let incoming_fingerprint = message_merge_fingerprint(msg);
            if let Some(existing_fingerprint) = pending_messages.get(&msg.idx) {
                if existing_fingerprint != &incoming_fingerprint {
                    idx_collision_count = idx_collision_count.saturating_add(1);
                    first_collision_idx.get_or_insert(msg.idx);
                }
                continue;
            }

            let incoming_replay = message_replay_fingerprint(msg);
            if pending_replay_fingerprints.contains(&incoming_replay) {
                tracing::debug!(
                    conversation_id = conv_id,
                    idx = msg.idx,
                    source_path = %conv.source_path.display(),
                    "skipping replay-equivalent duplicate message within profiled new conversation insert"
                );
                continue;
            }

            pending_messages.insert(msg.idx, incoming_fingerprint);
            pending_replay_fingerprints.insert(incoming_replay);
            new_messages.push(msg);
        }

        let message_insert_start = Instant::now();
        let inserted_message_ids = franken_batch_insert_new_messages_with_profile(
            &tx,
            conv_id,
            &new_messages,
            &mut profile.message_insert_breakdown,
        )?;
        profile.message_insert_duration += message_insert_start.elapsed();

        for (msg_id, msg) in inserted_message_ids.into_iter().zip(new_messages) {
            let snippet_insert_start = Instant::now();
            franken_insert_snippets(&tx, msg_id, &msg.snippets)?;
            profile.snippet_insert_duration += snippet_insert_start.elapsed();

            if !defer_lexical_updates {
                let fts_entry_start = Instant::now();
                fts_entries.push(FtsEntry::from_message(msg_id, msg, conv));
                fts_pending_chars = fts_pending_chars.saturating_add(msg.content.len());
                profile.fts_entry_duration += fts_entry_start.elapsed();
                if fts_entries.len() >= FTS_ENTRY_BATCH_MAX_DOCS
                    || fts_pending_chars >= FTS_ENTRY_BATCH_MAX_CHARS
                {
                    let fts_flush_start = Instant::now();
                    flush_pending_fts_entries(
                        self,
                        &tx,
                        &mut fts_entries,
                        &mut fts_pending_chars,
                        &mut fts_inserted_total,
                    )?;
                    profile.fts_flush_duration += fts_flush_start.elapsed();
                }
            }

            total_chars += msg.content.len() as i64;
            inserted_indices.push(msg.idx);
        }

        if idx_collision_count > 0 {
            tracing::warn!(
                conversation_id = conv_id,
                collision_count = idx_collision_count,
                first_idx = first_collision_idx,
                source_path = %conv.source_path.display(),
                "message idx collisions encountered while profiling a new conversation insert; retaining the first canonical variant per idx"
            );
        }

        if !defer_lexical_updates {
            let fts_flush_start = Instant::now();
            flush_pending_fts_entries(
                self,
                &tx,
                &mut fts_entries,
                &mut fts_pending_chars,
                &mut fts_inserted_total,
            )?;
            profile.fts_flush_duration += fts_flush_start.elapsed();
        }

        if !defer_analytics_updates {
            let analytics_start = Instant::now();
            franken_update_daily_stats_in_tx(
                self,
                &tx,
                &conv.agent_slug,
                &conv.source_id,
                conversation_effective_started_at(conv),
                StatsDelta {
                    session_count_delta: 1,
                    message_count_delta: inserted_indices.len() as i64,
                    total_chars_delta: total_chars,
                },
            )?;
            profile.analytics_duration += analytics_start.elapsed();
        }

        let commit_start = Instant::now();
        tx.commit()?;
        profile.commit_duration += commit_start.elapsed();
        profile.invocations += 1;
        profile.messages += conv.messages.len();
        profile.inserted_messages += inserted_indices.len();
        profile.total_duration += total_start.elapsed();

        Ok(InsertOutcome {
            conversation_id: conv_id,
            conversation_inserted: true,
            inserted_indices,
        })
    }

    #[cfg(test)]
    fn append_existing_conversation_with_profile(
        &self,
        agent_id: i64,
        _workspace_id: Option<i64>,
        conv: &Conversation,
        profile: &mut InsertConversationTreePerfProfile,
    ) -> Result<InsertOutcome> {
        let total_start = Instant::now();
        let normalized_conv = normalized_conversation_for_storage(conv);
        let conv = normalized_conv.as_ref();

        let source_start = Instant::now();
        self.ensure_source_for_conversation(conv)?;
        profile.source_duration += source_start.elapsed();

        let defer_lexical_updates = defer_storage_lexical_updates_enabled();
        let defer_analytics_updates = defer_analytics_updates_enabled();
        let conversation_key = conversation_merge_key(agent_id, conv);

        let tx_open_start = Instant::now();
        let mut tx = self.conn.transaction()?;
        profile.tx_open_duration += tx_open_start.elapsed();

        let existing_lookup_start = Instant::now();
        let existing =
            franken_find_existing_conversation_by_key(&tx, &conversation_key, Some(conv))?;
        profile.existing_lookup_duration += existing_lookup_start.elapsed();
        let existing_id = existing.ok_or_else(|| {
            anyhow!("append profile helper expects existing conversation for {conversation_key:?}")
        })?;

        let existing_idx_lookup_start = Instant::now();
        let existing_plan = if let Some((existing_max_idx, existing_ended_at)) =
            franken_existing_conversation_append_tail_state(&tx, existing_id)?
        {
            collect_append_only_tail_messages(conv, existing_max_idx, existing_ended_at)
        } else {
            None
        };
        profile.existing_idx_lookup_duration += existing_idx_lookup_start.elapsed();

        let dedupe_filter_start = Instant::now();
        let ExistingConversationNewMessages {
            messages: new_messages,
            new_chars,
            idx_collision_count,
            first_collision_idx,
        } = if let Some(existing_plan) = existing_plan {
            existing_plan
        } else {
            let ExistingMessageLookup {
                by_idx: mut existing_messages,
                replay: mut existing_replay_fingerprints,
            } = franken_existing_message_lookup(&tx, existing_id, &conv.messages)?;
            collect_new_messages_for_existing_conversation(
                existing_id,
                conv,
                &mut existing_messages,
                &mut existing_replay_fingerprints,
                "skipping replay-equivalent profiled append message with shifted idx",
            )
        };
        profile.dedupe_filter_duration += dedupe_filter_start.elapsed();

        let mut inserted_indices = Vec::new();
        let mut fts_entries = Vec::new();
        let mut fts_pending_chars = 0usize;
        let mut fts_inserted_total = 0usize;
        let (inserted_last_idx, inserted_last_created_at) =
            borrowed_messages_tail_state(&new_messages);

        let message_insert_start = Instant::now();
        let inserted_message_ids = franken_append_insert_new_messages_with_profile(
            &tx,
            existing_id,
            &new_messages,
            &mut profile.message_insert_breakdown,
        )?;
        profile.message_insert_duration += message_insert_start.elapsed();

        for (msg_id, msg) in inserted_message_ids.into_iter().zip(new_messages) {
            let snippet_insert_start = Instant::now();
            franken_insert_snippets(&tx, msg_id, &msg.snippets)?;
            profile.snippet_insert_duration += snippet_insert_start.elapsed();

            if !defer_lexical_updates {
                let fts_entry_start = Instant::now();
                fts_entries.push(FtsEntry::from_message(msg_id, msg, conv));
                fts_pending_chars = fts_pending_chars.saturating_add(msg.content.len());
                profile.fts_entry_duration += fts_entry_start.elapsed();
                if fts_entries.len() >= FTS_ENTRY_BATCH_MAX_DOCS
                    || fts_pending_chars >= FTS_ENTRY_BATCH_MAX_CHARS
                {
                    let fts_flush_start = Instant::now();
                    flush_pending_fts_entries(
                        self,
                        &tx,
                        &mut fts_entries,
                        &mut fts_pending_chars,
                        &mut fts_inserted_total,
                    )?;
                    profile.fts_flush_duration += fts_flush_start.elapsed();
                }
            }

            inserted_indices.push(msg.idx);
        }

        if idx_collision_count > 0 {
            tracing::warn!(
                conversation_id = existing_id,
                collision_count = idx_collision_count,
                first_idx = first_collision_idx,
                source_path = %conv.source_path.display(),
                "message idx collisions encountered while profiling append merge; retaining canonical message variants"
            );
        }

        if !defer_lexical_updates {
            let fts_flush_start = Instant::now();
            flush_pending_fts_entries(
                self,
                &tx,
                &mut fts_entries,
                &mut fts_pending_chars,
                &mut fts_inserted_total,
            )?;
            profile.fts_flush_duration += fts_flush_start.elapsed();
        }

        let conversation_row_start = Instant::now();
        let conv_last_ts = conv.messages.iter().filter_map(|m| m.created_at).max();
        franken_update_conversation_tail_state(
            &tx,
            existing_id,
            conv_last_ts,
            inserted_last_idx,
            inserted_last_created_at,
        )?;
        profile.conversation_row_duration += conversation_row_start.elapsed();

        if !defer_analytics_updates && !inserted_indices.is_empty() {
            let analytics_start = Instant::now();
            franken_update_daily_stats_in_tx(
                self,
                &tx,
                &conv.agent_slug,
                &conv.source_id,
                conversation_effective_started_at(conv),
                StatsDelta {
                    session_count_delta: 0,
                    message_count_delta: inserted_indices.len() as i64,
                    total_chars_delta: new_chars,
                },
            )?;
            profile.analytics_duration += analytics_start.elapsed();
        }

        let commit_start = Instant::now();
        tx.commit()?;
        profile.commit_duration += commit_start.elapsed();
        profile.invocations += 1;
        profile.messages += conv.messages.len();
        profile.inserted_messages += inserted_indices.len();
        profile.total_duration += total_start.elapsed();

        Ok(InsertOutcome {
            conversation_id: existing_id,
            conversation_inserted: false,
            inserted_indices,
        })
    }

    /// Append new messages to an existing conversation within an active transaction.
    fn franken_append_messages_in_tx(
        &self,
        tx: &FrankenTransaction<'_>,
        conversation_id: i64,
        conv: &Conversation,
    ) -> Result<InsertOutcome> {
        let defer_lexical_updates = defer_storage_lexical_updates_enabled();
        let defer_analytics_updates = defer_analytics_updates_enabled();
        let append_plan = if let Some((existing_max_idx, existing_ended_at)) =
            franken_existing_conversation_append_tail_state(tx, conversation_id)?
        {
            collect_append_only_tail_messages(conv, existing_max_idx, existing_ended_at)
        } else {
            None
        };
        let ExistingConversationNewMessages {
            messages: new_messages,
            new_chars,
            idx_collision_count,
            first_collision_idx,
        } = if let Some(append_plan) = append_plan {
            append_plan
        } else {
            let ExistingMessageLookup {
                by_idx: mut existing_messages,
                replay: mut existing_replay_fingerprints,
            } = franken_existing_message_lookup(tx, conversation_id, &conv.messages)?;
            collect_new_messages_for_existing_conversation(
                conversation_id,
                conv,
                &mut existing_messages,
                &mut existing_replay_fingerprints,
                "skipping replay-equivalent recovered message with shifted idx",
            )
        };

        let mut inserted_indices = Vec::new();
        let mut fts_entries = Vec::new();
        let mut fts_pending_chars = 0usize;
        let mut _fts_inserted_total = 0usize;
        let (inserted_last_idx, inserted_last_created_at) =
            borrowed_messages_tail_state(&new_messages);
        let inserted_message_ids =
            franken_append_insert_new_messages(tx, conversation_id, &new_messages)?;
        for (msg_id, msg) in inserted_message_ids.into_iter().zip(new_messages) {
            franken_insert_snippets(tx, msg_id, &msg.snippets)?;
            if !defer_lexical_updates {
                fts_entries.push(FtsEntry::from_message(msg_id, msg, conv));
                fts_pending_chars = fts_pending_chars.saturating_add(msg.content.len());
                if fts_entries.len() >= FTS_ENTRY_BATCH_MAX_DOCS
                    || fts_pending_chars >= FTS_ENTRY_BATCH_MAX_CHARS
                {
                    flush_pending_fts_entries(
                        self,
                        tx,
                        &mut fts_entries,
                        &mut fts_pending_chars,
                        &mut _fts_inserted_total,
                    )?;
                }
            }
            inserted_indices.push(msg.idx);
        }

        if idx_collision_count > 0 {
            tracing::warn!(
                conversation_id,
                collision_count = idx_collision_count,
                first_idx = first_collision_idx,
                source_path = %conv.source_path.display(),
                "message idx collisions encountered while appending to an existing conversation; retaining canonical message variants"
            );
        }

        if !defer_lexical_updates {
            flush_pending_fts_entries(
                self,
                tx,
                &mut fts_entries,
                &mut fts_pending_chars,
                &mut _fts_inserted_total,
            )?;
        }

        let conv_last_ts = conv.messages.iter().filter_map(|m| m.created_at).max();
        franken_update_conversation_tail_state(
            tx,
            conversation_id,
            conv_last_ts,
            inserted_last_idx,
            inserted_last_created_at,
        )?;

        if !defer_analytics_updates && !inserted_indices.is_empty() {
            let message_count = inserted_indices.len() as i64;
            franken_update_daily_stats_in_tx(
                self,
                tx,
                &conv.agent_slug,
                &conv.source_id,
                conversation_effective_started_at(conv),
                StatsDelta {
                    session_count_delta: 0,
                    message_count_delta: message_count,
                    total_chars_delta: new_chars,
                },
            )?;
        }

        Ok(InsertOutcome {
            conversation_id,
            conversation_inserted: false,
            inserted_indices,
        })
    }

    /// Rebuild the FTS5 index from scratch (chunked to avoid OOM on large databases, #110).
    pub fn rebuild_fts(&self) -> Result<()> {
        self.rebuild_fts_via_frankensqlite().map(|_| ())
    }

    /// Best-effort repair for the derived SQLite FTS fallback index.
    ///
    /// The canonical archive and Tantivy index remain authoritative, so callers
    /// should invoke this from maintenance paths rather than ordinary opens.
    pub(crate) fn ensure_search_fallback_fts_consistency(&self) -> Result<FtsConsistencyRepair> {
        self.ensure_fts_consistency_via_frankensqlite()
    }

    pub(crate) fn fallback_fts_is_known_healthy_for_archive_fingerprint(
        &self,
        archive_fingerprint: &str,
    ) -> Result<bool> {
        Ok(
            self.read_fts_franken_rebuild_generation()? == Some(FTS_FRANKEN_REBUILD_GENERATION)
                && self
                    .read_fts_franken_rebuild_archive_fingerprint()?
                    .as_deref()
                    == Some(archive_fingerprint),
        )
    }

    pub(crate) fn record_search_fallback_fts_archive_fingerprint(
        &self,
        archive_fingerprint: &str,
    ) -> Result<()> {
        self.conn
            .execute_compat(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
                fparams![
                    FTS_FRANKEN_REBUILD_FINGERPRINT_META_KEY,
                    archive_fingerprint.to_string()
                ],
            )
            .with_context(|| "recording frankensqlite FTS archive fingerprint")?;
        Ok(())
    }

    pub(crate) fn daily_stats_is_known_healthy_for_archive_fingerprint(
        &self,
        archive_fingerprint: &str,
    ) -> Result<bool> {
        Ok(
            self.read_daily_stats_health_generation()? == Some(DAILY_STATS_HEALTH_GENERATION)
                && self.read_daily_stats_archive_fingerprint()?.as_deref()
                    == Some(archive_fingerprint),
        )
    }

    pub(crate) fn record_daily_stats_archive_fingerprint(
        &self,
        archive_fingerprint: &str,
    ) -> Result<()> {
        self.conn
            .execute_compat(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
                fparams![
                    DAILY_STATS_HEALTH_GENERATION_META_KEY,
                    DAILY_STATS_HEALTH_GENERATION.to_string()
                ],
            )
            .with_context(|| "recording daily_stats health generation")?;
        self.conn
            .execute_compat(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
                fparams![DAILY_STATS_HEALTH_META_KEY, archive_fingerprint.to_string()],
            )
            .with_context(|| "recording daily_stats archive fingerprint")?;
        Ok(())
    }

    fn read_fts_franken_rebuild_generation(&self) -> Result<Option<i64>> {
        let value: Option<String> = self
            .conn
            .query_row_map(
                "SELECT value FROM meta WHERE key = ?1",
                fparams![FTS_FRANKEN_REBUILD_META_KEY],
                |row| row.get_typed(0),
            )
            .optional()?;
        Ok(value.and_then(|v| v.parse::<i64>().ok()))
    }

    fn read_fts_franken_rebuild_archive_fingerprint(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row_map(
                "SELECT value FROM meta WHERE key = ?1",
                fparams![FTS_FRANKEN_REBUILD_FINGERPRINT_META_KEY],
                |row| row.get_typed(0),
            )
            .optional()?)
    }

    fn read_daily_stats_health_generation(&self) -> Result<Option<i64>> {
        let value: Option<String> = self
            .conn
            .query_row_map(
                "SELECT value FROM meta WHERE key = ?1",
                fparams![DAILY_STATS_HEALTH_GENERATION_META_KEY],
                |row| row.get_typed(0),
            )
            .optional()?;
        Ok(value.and_then(|value| value.parse::<i64>().ok()))
    }

    fn read_daily_stats_archive_fingerprint(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row_map(
                "SELECT value FROM meta WHERE key = ?1",
                fparams![DAILY_STATS_HEALTH_META_KEY],
                |row| row.get_typed(0),
            )
            .optional()?)
    }

    fn record_fts_franken_rebuild_generation(&self) -> Result<()> {
        self.conn
            .execute_compat(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
                fparams![
                    FTS_FRANKEN_REBUILD_META_KEY,
                    FTS_FRANKEN_REBUILD_GENERATION.to_string()
                ],
            )
            .with_context(|| "recording frankensqlite FTS rebuild generation")?;
        Ok(())
    }

    fn ensure_fts_consistency_via_frankensqlite(&self) -> Result<FtsConsistencyRepair> {
        if self.read_fts_franken_rebuild_generation()? != Some(FTS_FRANKEN_REBUILD_GENERATION) {
            // Before triggering an expensive full rebuild, probe whether
            // fts_messages is already populated and consistent.  On large
            // databases the rebuild can take hours and OOM — skip it when
            // the only thing missing is the generation marker (#184).
            let fts_already_healthy = (|| -> Result<bool> {
                let fts_exists: i64 = self.conn.query_row_map(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fts_messages'",
                    fparams![],
                    |row| row.get_typed(0),
                )?;
                if fts_exists != 1 {
                    return Ok(false);
                }
                let total: i64 = self.conn.query_row_map(
                    "SELECT COUNT(*) FROM messages",
                    fparams![],
                    |row| row.get_typed(0),
                )?;
                if total == 0 {
                    return Ok(false);
                }
                let indexed: i64 = self.conn.query_row_map(
                    "SELECT COUNT(*) FROM fts_messages",
                    fparams![],
                    |row| row.get_typed(0),
                )?;
                // Consider healthy if >=90% of messages are indexed
                Ok(indexed > 0 && indexed * 100 >= total * 90)
            })()
            .unwrap_or(false);

            if fts_already_healthy {
                tracing::info!(
                    target: "cass::fts_rebuild",
                    "FTS already populated and consistent; setting generation marker without rebuild"
                );
                self.record_fts_franken_rebuild_generation()?;
                self.set_fts_messages_present_cache(true);
            } else {
                let inserted_rows = self.rebuild_fts_via_frankensqlite()?;
                self.record_fts_franken_rebuild_generation()?;
                return Ok(FtsConsistencyRepair::Rebuilt { inserted_rows });
            }
        }

        let inspection = (|| -> Result<(i64, bool)> {
            let fts_schema_rows = self.conn.query_row_map(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fts_messages'",
                fparams![],
                |row| row.get_typed::<i64>(0),
            )?;
            let fts_queryable = fts_schema_rows == 1
                && self
                    .conn
                    .query("SELECT rowid FROM fts_messages LIMIT 1")
                    .is_ok();
            Ok((fts_schema_rows, fts_queryable))
        })();

        let (fts_schema_rows, fts_queryable) = match inspection {
            Ok(result) => result,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "frankensqlite FTS consistency probe failed; rebuilding authoritative FTS"
                );
                let inserted_rows = self.rebuild_fts_via_frankensqlite()?;
                self.record_fts_franken_rebuild_generation()?;
                return Ok(FtsConsistencyRepair::Rebuilt { inserted_rows });
            }
        };

        if fts_schema_rows != 1 || !fts_queryable {
            let inserted_rows = self.rebuild_fts_via_frankensqlite()?;
            self.record_fts_franken_rebuild_generation()?;
            return Ok(FtsConsistencyRepair::Rebuilt { inserted_rows });
        }

        let total_messages =
            self.conn
                .query_row_map("SELECT COUNT(*) FROM messages", fparams![], |row| {
                    row.get_typed::<i64>(0)
                })?;
        let indexed_messages =
            self.conn
                .query_row_map("SELECT COUNT(*) FROM fts_messages", fparams![], |row| {
                    row.get_typed::<i64>(0)
                })?;

        if indexed_messages == total_messages {
            self.set_fts_messages_present_cache(true);
            return Ok(FtsConsistencyRepair::AlreadyHealthy {
                rows: usize::try_from(total_messages.max(0)).unwrap_or(usize::MAX),
            });
        }

        if indexed_messages > total_messages {
            let inserted_rows = self.rebuild_fts_via_frankensqlite()?;
            self.record_fts_franken_rebuild_generation()?;
            return Ok(FtsConsistencyRepair::Rebuilt { inserted_rows });
        }

        let inserted_rows = self
            .conn
            .execute_compat(
                "INSERT INTO fts_messages(rowid, content, title, agent, workspace, source_path, created_at)
                 SELECT m.id, m.content, c.title,
                        COALESCE((SELECT a.slug FROM agents a WHERE a.id = c.agent_id), 'unknown'),
                        (SELECT w.path FROM workspaces w WHERE w.id = c.workspace_id),
                        c.source_path, m.created_at
                 FROM messages m
                 JOIN conversations c ON m.conversation_id = c.id
                 WHERE NOT EXISTS (SELECT 1 FROM fts_messages f WHERE f.rowid = m.id)
                 ORDER BY m.rowid",
                fparams![],
            )
            .with_context(|| "incrementally repairing missing FTS rows via frankensqlite")?;
        let repaired_rows =
            self.conn
                .query_row_map("SELECT COUNT(*) FROM fts_messages", fparams![], |row| {
                    row.get_typed::<i64>(0)
                })?;
        if repaired_rows == total_messages {
            self.set_fts_messages_present_cache(true);
            return Ok(FtsConsistencyRepair::IncrementalCatchUp {
                inserted_rows,
                total_rows: usize::try_from(repaired_rows.max(0)).unwrap_or(usize::MAX),
            });
        }

        // The incremental catch-up found nothing to insert, yet the gap
        // between total_messages (all rows, including orphans) and
        // indexed_messages (only rows with valid conversation_id, since the
        // FTS INSERT inner-joins on conversations) remains.  A full rebuild
        // cannot close this gap either — the orphaned messages will be
        // excluded again — so falling through to one would just re-do ~5 min
        // of work on every startup.  Accept the current state.
        if inserted_rows == 0 {
            tracing::debug!(
                target: "cass::fts_rebuild",
                indexed_messages = repaired_rows,
                total_messages,
                un_indexable_gap = total_messages.saturating_sub(repaired_rows),
                "FTS catch-up inserted 0 rows; remaining gap is un-indexable (likely orphaned messages with dangling conversation_id)"
            );
            self.set_fts_messages_present_cache(true);
            return Ok(FtsConsistencyRepair::IncrementalCatchUp {
                inserted_rows: 0,
                total_rows: usize::try_from(repaired_rows.max(0)).unwrap_or(usize::MAX),
            });
        }

        // Incremental made progress but didn't fully close the gap — something
        // is genuinely inconsistent, so do a full rebuild.
        let inserted_rows = self.rebuild_fts_via_frankensqlite()?;
        self.record_fts_franken_rebuild_generation()?;
        Ok(FtsConsistencyRepair::Rebuilt { inserted_rows })
    }

    pub(crate) fn rebuild_fts_via_frankensqlite(&self) -> Result<usize> {
        self.invalidate_fts_messages_present_cache();
        self.conn
            .execute("DROP TABLE IF EXISTS fts_messages;")
            .with_context(|| "dropping derived fts_messages before frankensqlite rebuild")?;
        self.conn
            .execute_compat(FTS5_REGISTER_SQL, fparams![])
            .with_context(|| "creating derived fts_messages via frankensqlite rebuild")?;
        self.set_fts_messages_present_cache(true);

        // Bug #168: Batch the FTS rebuild INSERT to avoid OOM when messages
        // table is large (e.g. 179K+ rows).  We paginate through messages by
        // rowid, inserting FTS_REBUILD_BATCH_SIZE rows per batch.
        let batch_size = fts_rebuild_batch_size() as i64;
        let batch_offset = (batch_size - 1).max(0);
        let mut total_inserted: usize = 0;
        let mut last_rowid: i64 = 0;

        loop {
            // Find the upper bound rowid for this batch using a cheap index scan.
            // OFFSET (batch_size - 1) with LIMIT 1 gives us the batch_size-th row.
            let batch_max_rowid: Option<i64> = self
                .conn
                .query_row_map(
                    "SELECT m.rowid FROM messages m
                     WHERE m.rowid > ?1
                     ORDER BY m.rowid
                     LIMIT 1 OFFSET ?2",
                    fparams![last_rowid, batch_offset],
                    |row| row.get_typed(0),
                )
                .optional()?;

            let inserted = if let Some(upper) = batch_max_rowid {
                self.conn
                    .execute_compat(
                        "INSERT INTO fts_messages(rowid, content, title, agent, workspace, source_path, created_at)
                         SELECT m.id, m.content, c.title,
                                COALESCE((SELECT a.slug FROM agents a WHERE a.id = c.agent_id), 'unknown'),
                                (SELECT w.path FROM workspaces w WHERE w.id = c.workspace_id),
                                c.source_path, m.created_at
                         FROM messages m
                         JOIN conversations c ON m.conversation_id = c.id
                         WHERE m.rowid > ?1 AND m.rowid <= ?2
                         ORDER BY m.rowid",
                        fparams![last_rowid, upper],
                    )
                    .with_context(|| {
                        format!(
                            "populating derived fts_messages via frankensqlite rebuild (batch rowid {}..{})",
                            last_rowid + 1, upper
                        )
                    })?
            } else {
                // Fewer than batch_size rows remain; insert the tail.
                self.conn
                    .execute_compat(
                        "INSERT INTO fts_messages(rowid, content, title, agent, workspace, source_path, created_at)
                         SELECT m.id, m.content, c.title,
                                COALESCE((SELECT a.slug FROM agents a WHERE a.id = c.agent_id), 'unknown'),
                                (SELECT w.path FROM workspaces w WHERE w.id = c.workspace_id),
                                c.source_path, m.created_at
                         FROM messages m
                         JOIN conversations c ON m.conversation_id = c.id
                         WHERE m.rowid > ?1
                         ORDER BY m.rowid",
                        fparams![last_rowid],
                    )
                    .with_context(|| {
                        format!(
                            "populating derived fts_messages via frankensqlite rebuild (final batch after rowid {})",
                            last_rowid
                        )
                    })?
            };

            total_inserted = total_inserted.saturating_add(inserted);

            if let Some(upper) = batch_max_rowid {
                // Advance past this batch and loop regardless of inserted count.
                // A zero-insert batch is possible when every row in the range is
                // orphaned (e.g. dangling conversation_id), and breaking early
                // would skip any valid messages that live beyond the orphaned
                // range.  Termination is driven by batch_max_rowid returning
                // None once we scan past the last message.
                last_rowid = upper;
            } else {
                // Final batch processed; we're done.
                tracing::debug!(
                    target: "cass::fts_rebuild",
                    batch_inserted = inserted,
                    total_inserted,
                    "FTS rebuild final batch complete"
                );
                break;
            }

            tracing::debug!(
                target: "cass::fts_rebuild",
                batch_inserted = inserted,
                total_inserted,
                last_rowid,
                "FTS rebuild batch complete"
            );
        }

        Ok(total_inserted)
    }

    /// Fetch all messages for embedding generation.
    pub fn fetch_messages_for_embedding(&self) -> Result<Vec<MessageForEmbedding>> {
        // COALESCE(c.agent_id, 0) so legacy V1 conversations with NULL
        // agent_id don't cause a runtime row-decode failure (agent_id in
        // MessageForEmbedding is i64).  saturating_u32_from_i64 downstream
        // turns 0 into the "unknown agent" sentinel for doc-id hashing.
        self.conn
            .query_map_collect(
                "SELECT m.id, m.created_at, COALESCE(c.agent_id, 0), c.workspace_id, c.source_id, m.role, m.content
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

    /// Get the watermark for incremental semantic embedding.
    pub fn get_last_embedded_message_id(&self) -> Result<Option<i64>> {
        let result: Result<String, _> = self.conn.query_row_map(
            "SELECT value FROM meta WHERE key = 'last_embedded_message_id'",
            fparams![],
            |row| row.get_typed(0),
        );
        match result.optional() {
            Ok(Some(s)) => Ok(s.parse().ok()),
            Ok(None) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Set the watermark for incremental semantic embedding.
    pub fn set_last_embedded_message_id(&self, id: i64) -> Result<()> {
        self.conn.execute_compat(
            "INSERT OR REPLACE INTO meta(key, value) VALUES('last_embedded_message_id', ?1)",
            fparams![id.to_string()],
        )?;
        Ok(())
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
        let updated = self.conn.execute_compat(
            "UPDATE embedding_jobs
             SET total_docs = ?3
             WHERE db_path = ?1 AND model_id = ?2 AND status IN ('pending', 'running')",
            fparams![db_path, model_id, total_docs],
        )?;
        if updated == 0 {
            let insert_result = self.conn.execute_compat(
                "INSERT INTO embedding_jobs(db_path, model_id, total_docs) VALUES(?1,?2,?3)",
                fparams![db_path, model_id, total_docs],
            );
            if let Err(err) = insert_result {
                if !matches!(err, frankensqlite::FrankenError::UniqueViolation { .. }) {
                    return Err(err.into());
                }
                self.conn.execute_compat(
                    "UPDATE embedding_jobs
                     SET total_docs = ?3
                     WHERE db_path = ?1 AND model_id = ?2 AND status IN ('pending', 'running')",
                    fparams![db_path, model_id, total_docs],
                )?;
            }
        }
        self.conn
            .query_row_map(
                "SELECT id FROM embedding_jobs
                 WHERE db_path = ?1 AND model_id = ?2 AND status IN ('pending', 'running')
                 ORDER BY id DESC
                 LIMIT 1",
                fparams![db_path, model_id],
                |row| row.get_typed(0),
            )
            .with_context(|| "resolving embedding job id after upsert")
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
        // Build dynamic SQL with positional params.  Single-table scan of
        // conversations; filter on agent slug via an EXISTS subquery only
        // when that filter is actually requested.  This avoids the unneeded
        // 2-table JOIN (which also silently dropped legacy conversations
        // with NULL agent_id) and sidesteps frankensqlite's materialization
        // fallback entirely.
        let mut sql = "SELECT COUNT(*) FROM conversations c WHERE 1=1".to_string();
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
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM agents a WHERE a.id = c.agent_id AND a.slug = ?{idx})"
            ));
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
        let row_count: i64 =
            self.conn
                .query_row_map("SELECT COUNT(*) FROM daily_stats", fparams![], |row| {
                    row.get_typed(0)
                })?;

        let oldest_update: Option<i64> = self.conn.query_row_map(
            "SELECT MIN(last_updated) FROM daily_stats",
            fparams![],
            |row| row.get_typed(0),
        )?;

        let conversation_count: i64 =
            self.conn
                .query_row_map("SELECT COUNT(*) FROM conversations", fparams![], |row| {
                    row.get_typed(0)
                })?;

        let materialized_total: i64 = self.conn.query_row_map(
            "SELECT COALESCE(SUM(session_count), 0) FROM daily_stats
                 WHERE agent_slug = 'all' AND source_id = 'all'",
            fparams![],
            |row| row.get_typed(0),
        )?;

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

        self.ensure_sources_for_batch(conversations)?;

        let defer_lexical_updates = defer_storage_lexical_updates_enabled();
        let defer_analytics_updates = defer_analytics_updates_enabled();

        let pricing_table = PricingTable::franken_load(&self.conn).unwrap_or_else(|e| {
            tracing::warn!(target: "cass::analytics::pricing", error = %e, "failed to load pricing table");
            PricingTable { entries: Vec::new() }
        });
        let mut pricing_diag = PricingDiagnostics::default();

        let mut tx = self.conn.transaction()?;

        // Bug #167: Ensure all referenced agents, workspaces, and sources
        // exist inside the transaction so FK checks pass.  The caller resolves
        // IDs via ensure_agent / ensure_workspace / ensure_sources_for_batch
        // outside the transaction, but those autocommit writes may not be
        // visible inside the transaction snapshot in frankensqlite.  Re-verify
        // (and insert if missing) within the tx.
        ensure_agents_in_tx(&tx, conversations)?;
        ensure_workspaces_in_tx(&tx, conversations)?;
        ensure_sources_in_tx(&tx, conversations)?;

        let mut outcomes = Vec::with_capacity(conversations.len());
        let mut fts_entries = Vec::new();
        let mut fts_pending_chars = 0usize;
        let mut fts_inserted_total = 0usize;
        let mut fts_count_total = 0usize;
        let mut stats = StatsAggregator::new();
        let mut token_stats = TokenStatsAggregator::new();
        let mut token_entries: Vec<TokenUsageEntry> = Vec::new();
        let mut metrics_entries: Vec<MessageMetricsEntry> = Vec::new();
        let mut rollup_agg = AnalyticsRollupAggregator::new();
        let mut conv_ids_to_summarize: Vec<i64> = Vec::new();
        let mut pending_conversation_ids: HashMap<PendingConversationKey, i64> = HashMap::new();
        let mut pending_message_fingerprints: HashMap<i64, HashMap<i64, MessageMergeFingerprint>> =
            HashMap::new();
        let mut pending_message_replay_fingerprints: HashMap<
            i64,
            HashSet<MessageReplayFingerprint>,
        > = HashMap::new();

        for &(agent_id, workspace_id, raw_conv) in conversations {
            let normalized_conv = normalized_conversation_for_storage(raw_conv);
            let conv = normalized_conv.as_ref();
            let mut total_chars: i64 = 0;
            let mut inserted_indices = Vec::with_capacity(conv.messages.len());
            let mut inserted_messages: Vec<(i64, &Message)> =
                Vec::with_capacity(conv.messages.len());
            let mut session_count_delta = 1_i64;
            let conversation_key = conversation_merge_key(agent_id, conv);

            let existing_conv_id = if let Some(existing_id) =
                pending_conversation_ids.get(&conversation_key)
            {
                Some(*existing_id)
            } else {
                let existing_id =
                    franken_find_existing_conversation_by_key(&tx, &conversation_key, Some(conv))?;
                if let Some(existing_id) = existing_id {
                    pending_conversation_ids.insert(conversation_key.clone(), existing_id);
                }
                existing_id
            };

            let conv_id = if let Some(existing_id) = existing_conv_id {
                session_count_delta = 0;
                let ExistingMessageLookup {
                    by_idx: mut existing_messages,
                    replay: mut existing_replay_fingerprints,
                } = franken_existing_message_lookup_with_pending(
                    &tx,
                    existing_id,
                    &conv.messages,
                    &mut pending_message_fingerprints,
                    &mut pending_message_replay_fingerprints,
                )?;
                let ExistingConversationNewMessages {
                    messages: new_messages,
                    new_chars,
                    idx_collision_count,
                    first_collision_idx,
                } = collect_new_messages_for_existing_conversation(
                    existing_id,
                    conv,
                    &mut existing_messages,
                    &mut existing_replay_fingerprints,
                    "skipping replay-equivalent recovered message with shifted idx during batched merge",
                );
                let (inserted_last_idx, inserted_last_created_at) =
                    borrowed_messages_tail_state(&new_messages);
                let inserted_message_ids =
                    franken_append_insert_new_messages(&tx, existing_id, &new_messages)?;
                total_chars += new_chars;
                for (msg_id, msg) in inserted_message_ids.into_iter().zip(new_messages) {
                    franken_insert_snippets(&tx, msg_id, &msg.snippets)?;
                    if !defer_lexical_updates {
                        fts_entries.push(FtsEntry::from_message(msg_id, msg, conv));
                        fts_count_total += 1;
                        fts_pending_chars = fts_pending_chars.saturating_add(msg.content.len());
                        if fts_entries.len() >= FTS_ENTRY_BATCH_MAX_DOCS
                            || fts_pending_chars >= FTS_ENTRY_BATCH_MAX_CHARS
                        {
                            flush_pending_fts_entries(
                                self,
                                &tx,
                                &mut fts_entries,
                                &mut fts_pending_chars,
                                &mut fts_inserted_total,
                            )?;
                        }
                    }
                    inserted_indices.push(msg.idx);
                    inserted_messages.push((msg_id, msg));
                }

                if idx_collision_count > 0 {
                    tracing::warn!(
                        conversation_id = existing_id,
                        collision_count = idx_collision_count,
                        first_idx = first_collision_idx,
                        source_path = %conv.source_path.display(),
                        "message idx collisions encountered during batched conversation merge; retaining canonical message variants"
                    );
                }

                let conv_last_ts = conv.messages.iter().filter_map(|m| m.created_at).max();
                franken_update_conversation_tail_state(
                    &tx,
                    existing_id,
                    conv_last_ts,
                    inserted_last_idx,
                    inserted_last_created_at,
                )?;

                pending_message_fingerprints.insert(existing_id, existing_messages);
                pending_message_replay_fingerprints
                    .insert(existing_id, existing_replay_fingerprints);

                existing_id
            } else {
                match franken_insert_conversation_or_get_existing(
                    &tx,
                    agent_id,
                    workspace_id,
                    conv,
                )? {
                    ConversationInsertStatus::Inserted(new_conv_id) => {
                        pending_conversation_ids.insert(conversation_key.clone(), new_conv_id);
                        let pending_messages =
                            pending_message_fingerprints.entry(new_conv_id).or_default();
                        let pending_replay_fingerprints = pending_message_replay_fingerprints
                            .entry(new_conv_id)
                            .or_default();
                        let mut new_messages = Vec::new();
                        for msg in &conv.messages {
                            let incoming_replay = message_replay_fingerprint(msg);
                            if pending_messages.contains_key(&msg.idx)
                                || pending_replay_fingerprints.contains(&incoming_replay)
                            {
                                continue;
                            }
                            pending_messages.insert(msg.idx, message_merge_fingerprint(msg));
                            pending_replay_fingerprints.insert(incoming_replay);
                            new_messages.push(msg);
                        }
                        let inserted_message_ids =
                            franken_batch_insert_new_messages(&tx, new_conv_id, &new_messages)?;
                        for (msg_id, msg) in inserted_message_ids.into_iter().zip(new_messages) {
                            franken_insert_snippets(&tx, msg_id, &msg.snippets)?;
                            if !defer_lexical_updates {
                                fts_entries.push(FtsEntry::from_message(msg_id, msg, conv));
                                fts_count_total += 1;
                                fts_pending_chars =
                                    fts_pending_chars.saturating_add(msg.content.len());
                                if fts_entries.len() >= FTS_ENTRY_BATCH_MAX_DOCS
                                    || fts_pending_chars >= FTS_ENTRY_BATCH_MAX_CHARS
                                {
                                    flush_pending_fts_entries(
                                        self,
                                        &tx,
                                        &mut fts_entries,
                                        &mut fts_pending_chars,
                                        &mut fts_inserted_total,
                                    )?;
                                }
                            }
                            total_chars += msg.content.len() as i64;
                            inserted_indices.push(msg.idx);
                            inserted_messages.push((msg_id, msg));
                        }
                        new_conv_id
                    }
                    ConversationInsertStatus::Existing(existing_id) => {
                        session_count_delta = 0;
                        pending_conversation_ids.insert(conversation_key.clone(), existing_id);
                        let ExistingMessageLookup {
                            by_idx: mut existing_messages,
                            replay: mut existing_replay_fingerprints,
                        } = franken_existing_message_lookup_with_pending(
                            &tx,
                            existing_id,
                            &conv.messages,
                            &mut pending_message_fingerprints,
                            &mut pending_message_replay_fingerprints,
                        )?;
                        let ExistingConversationNewMessages {
                            messages: new_messages,
                            new_chars,
                            idx_collision_count,
                            first_collision_idx,
                        } = collect_new_messages_for_existing_conversation(
                            existing_id,
                            conv,
                            &mut existing_messages,
                            &mut existing_replay_fingerprints,
                            "skipping replay-equivalent recovered message with shifted idx after duplicate conversation recovery",
                        );
                        let (inserted_last_idx, inserted_last_created_at) =
                            borrowed_messages_tail_state(&new_messages);
                        let inserted_message_ids =
                            franken_append_insert_new_messages(&tx, existing_id, &new_messages)?;
                        total_chars += new_chars;
                        for (msg_id, msg) in inserted_message_ids.into_iter().zip(new_messages) {
                            franken_insert_snippets(&tx, msg_id, &msg.snippets)?;
                            if !defer_lexical_updates {
                                fts_entries.push(FtsEntry::from_message(msg_id, msg, conv));
                                fts_count_total += 1;
                                fts_pending_chars =
                                    fts_pending_chars.saturating_add(msg.content.len());
                                if fts_entries.len() >= FTS_ENTRY_BATCH_MAX_DOCS
                                    || fts_pending_chars >= FTS_ENTRY_BATCH_MAX_CHARS
                                {
                                    flush_pending_fts_entries(
                                        self,
                                        &tx,
                                        &mut fts_entries,
                                        &mut fts_pending_chars,
                                        &mut fts_inserted_total,
                                    )?;
                                }
                            }
                            inserted_indices.push(msg.idx);
                            inserted_messages.push((msg_id, msg));
                        }

                        if idx_collision_count > 0 {
                            tracing::warn!(
                                conversation_id = existing_id,
                                collision_count = idx_collision_count,
                                first_idx = first_collision_idx,
                                source_path = %conv.source_path.display(),
                                "message idx collisions encountered after duplicate conversation recovery; retaining canonical message variants"
                            );
                        }

                        let conv_last_ts = conv.messages.iter().filter_map(|m| m.created_at).max();
                        franken_update_conversation_tail_state(
                            &tx,
                            existing_id,
                            conv_last_ts,
                            inserted_last_idx,
                            inserted_last_created_at,
                        )?;

                        pending_message_fingerprints.insert(existing_id, existing_messages);
                        pending_message_replay_fingerprints
                            .insert(existing_id, existing_replay_fingerprints);

                        existing_id
                    }
                }
            };

            if !defer_analytics_updates {
                let delta = StatsDelta {
                    session_count_delta,
                    message_count_delta: inserted_messages.len() as i64,
                    total_chars_delta: total_chars,
                };

                let effective_started_at = conversation_effective_started_at(conv);
                let day_id = effective_started_at
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

                let conv_day_id = day_id;
                let mut session_model_family = String::from("unknown");
                let mut has_any_tokens = false;

                for &(message_id, msg) in &inserted_messages {
                    let role_s = role_str(&msg.role);
                    let usage = if historical_raw_json(&msg.extra_json).is_some() {
                        crate::connectors::extract_tokens_for_agent(
                            &conv.agent_slug,
                            &serde_json::Value::Null,
                            &msg.content,
                            &role_s,
                        )
                    } else {
                        crate::connectors::extract_tokens_for_agent(
                            &conv.agent_slug,
                            &msg.extra_json,
                            &msg.content,
                            &role_s,
                        )
                    };

                    let msg_ts = msg
                        .created_at
                        .or(conversation_effective_started_at(conv))
                        .unwrap_or(0);
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
                        role: role_s.to_string(),
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
                        role: role_s.to_string(),
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

                if session_count_delta > 0 {
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
            }

            outcomes.push(InsertOutcome {
                conversation_id: conv_id,
                conversation_inserted: session_count_delta > 0,
                inserted_indices,
            });
        }

        // Batch insert all FTS entries at once
        if !defer_lexical_updates {
            flush_pending_fts_entries(
                self,
                &tx,
                &mut fts_entries,
                &mut fts_pending_chars,
                &mut fts_inserted_total,
            )?;
        }
        if !defer_lexical_updates && fts_count_total > 0 {
            tracing::debug!(
                target: "cass::perf::fts5",
                total = fts_count_total,
                inserted = fts_inserted_total,
                conversations = conversations.len(),
                "franken_batch_fts_insert_complete"
            );
        }

        // Batched daily_stats update
        if !defer_analytics_updates && !stats.is_empty() {
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
        if !defer_analytics_updates && !token_entries.is_empty() {
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
        if !defer_analytics_updates && !token_stats.is_empty() {
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
        if !defer_analytics_updates && !metrics_entries.is_empty() {
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
        if !defer_analytics_updates && !rollup_agg.is_empty() {
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
        if !defer_analytics_updates {
            for conv_id in &conv_ids_to_summarize {
                franken_update_conversation_token_summaries_in_tx(&tx, *conv_id)?;
            }
        }

        tx.commit()?;

        pricing_diag.log_summary();

        Ok(outcomes)
    }
}

fn normalized_storage_source_parts(
    source_id: Option<&str>,
    origin_kind: Option<&str>,
    origin_host: Option<&str>,
) -> (String, SourceKind, Option<String>) {
    let host_label = crate::search::tantivy::normalized_index_origin_host(origin_host);
    let source_id = crate::search::tantivy::normalized_index_source_id(
        source_id,
        origin_kind,
        host_label.as_deref(),
    );

    if source_id == LOCAL_SOURCE_ID {
        (source_id, SourceKind::Local, None)
    } else {
        (source_id, SourceKind::Ssh, host_label)
    }
}

fn normalized_source_for_conversation(conv: &Conversation) -> Source {
    let (id, kind, host_label) = normalized_storage_source_parts(
        Some(conv.source_id.as_str()),
        None,
        conv.origin_host.as_deref(),
    );
    Source {
        id,
        kind,
        host_label,
        machine_id: None,
        platform: None,
        config_json: None,
        created_at: None,
        updated_at: None,
    }
}

fn is_bootstrap_local_source(source: &Source) -> bool {
    source.id == LOCAL_SOURCE_ID
        && matches!(source.kind, SourceKind::Local)
        && source.host_label.is_none()
        && source.machine_id.is_none()
        && source.platform.is_none()
        && source.config_json.is_none()
        && source.created_at.is_none()
        && source.updated_at.is_none()
}

fn normalized_conversation_for_storage<'a>(conv: &'a Conversation) -> Cow<'a, Conversation> {
    let normalized_source = normalized_source_for_conversation(conv);
    if normalized_source.id == conv.source_id && normalized_source.host_label == conv.origin_host {
        Cow::Borrowed(conv)
    } else {
        let mut normalized = conv.clone();
        normalized.source_id = normalized_source.id;
        normalized.origin_host = normalized_source.host_label;
        Cow::Owned(normalized)
    }
}

impl FrankenStorage {
    fn ensure_source_for_conversation(&self, conv: &Conversation) -> Result<()> {
        let source = normalized_source_for_conversation(conv);
        if is_bootstrap_local_source(&source) {
            // `open()` and schema repair always seed the canonical local source row.
            // Avoid an autocommit UPDATE on every local conversation insert.
            return Ok(());
        }
        let cache_key = EnsuredConversationSourceKey::from_source(&source);
        if self.conversation_source_already_ensured(&cache_key) {
            return Ok(());
        }
        self.upsert_source(&source)?;
        self.mark_conversation_source_ensured(cache_key);
        Ok(())
    }

    fn ensure_sources_for_batch(
        &self,
        conversations: &[(i64, Option<i64>, &Conversation)],
    ) -> Result<()> {
        let mut seen = HashSet::with_capacity(conversations.len());
        for &(_, _, conv) in conversations {
            let source = normalized_source_for_conversation(conv);
            if seen.insert(source.id.clone()) {
                if is_bootstrap_local_source(&source) {
                    continue;
                }
                self.upsert_source(&source)?;
                self.mark_conversation_source_ensured(EnsuredConversationSourceKey::from_source(
                    &source,
                ));
            }
        }
        Ok(())
    }
}

// =========================================================================
// FrankenStorage transaction helper functions
// =========================================================================

/// Get last_insert_rowid from a frankensqlite transaction.
fn franken_last_rowid(tx: &FrankenTransaction<'_>) -> Result<i64> {
    tx.last_insert_rowid()
        .ok()
        .filter(|&id| id > 0)
        .with_context(|| "last_insert_rowid() returned NULL or 0 after INSERT")
}

/// Bug #167: Ensure all agents referenced by a batch exist within the
/// transaction.  The caller already resolved `agent_id` values via
/// `ensure_agent` outside the transaction, but those autocommit writes may
/// not be visible inside a frankensqlite transaction snapshot.  This function
/// checks each unique agent_id and creates a stub row if it's missing.
fn ensure_agents_in_tx(
    tx: &FrankenTransaction<'_>,
    conversations: &[(i64, Option<i64>, &Conversation)],
) -> Result<()> {
    let mut seen = HashSet::new();
    let now = FrankenStorage::now_millis();
    for &(agent_id, _, conv) in conversations {
        if !seen.insert(agent_id) {
            continue;
        }
        let exists: i64 = tx.query_row_map(
            "SELECT COUNT(*) FROM agents WHERE id = ?1",
            fparams![agent_id],
            |row| row.get_typed(0),
        )?;
        if exists == 0 {
            tracing::debug!(
                target: "cass::fk_guard",
                agent_id,
                slug = %conv.agent_slug,
                "inserting agent row inside transaction to satisfy FK constraint"
            );
            // INSERT OR IGNORE: the slug might already exist with a different
            // id from a concurrent writer.  If the slug row exists, the FK
            // constraint is already satisfied (the caller just got a stale id).
            tx.execute_compat(
                "INSERT OR IGNORE INTO agents(id, slug, name, kind, created_at, updated_at)
                 VALUES(?1, ?2, ?3, 'cli', ?4, ?5)",
                fparams![
                    agent_id,
                    conv.agent_slug.as_str(),
                    conv.agent_slug.as_str(),
                    now,
                    now
                ],
            )?;
        }
    }
    Ok(())
}

/// Bug #167: Ensure all workspaces referenced by a batch exist within the
/// transaction.  Same rationale as `ensure_agents_in_tx`.
fn ensure_workspaces_in_tx(
    tx: &FrankenTransaction<'_>,
    conversations: &[(i64, Option<i64>, &Conversation)],
) -> Result<()> {
    let mut seen = HashSet::new();
    for &(_, workspace_id, conv) in conversations {
        let ws_id = match workspace_id {
            Some(id) => id,
            None => continue,
        };
        if !seen.insert(ws_id) {
            continue;
        }
        let exists: i64 = tx.query_row_map(
            "SELECT COUNT(*) FROM workspaces WHERE id = ?1",
            fparams![ws_id],
            |row| row.get_typed(0),
        )?;
        if exists == 0 {
            let path_str = conv
                .workspace
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            tracing::debug!(
                target: "cass::fk_guard",
                workspace_id = ws_id,
                path = %path_str,
                "inserting workspace row inside transaction to satisfy FK constraint"
            );
            tx.execute_compat(
                "INSERT OR IGNORE INTO workspaces(id, path) VALUES(?1, ?2)",
                fparams![ws_id, path_str.as_str()],
            )?;
        }
    }
    Ok(())
}

/// Bug #167: Ensure all sources referenced by a batch exist within the
/// transaction.  Same rationale as `ensure_agents_in_tx` — source_id is a
/// TEXT FK on the conversations table.
fn ensure_sources_in_tx(
    tx: &FrankenTransaction<'_>,
    conversations: &[(i64, Option<i64>, &Conversation)],
) -> Result<()> {
    let mut seen = HashSet::new();
    for &(_, _, conv) in conversations {
        let (source_id, source_kind, host_label) = normalized_storage_source_parts(
            Some(conv.source_id.as_str()),
            None,
            conv.origin_host.as_deref(),
        );
        if !seen.insert(source_id.clone()) {
            continue;
        }
        let exists: i64 = tx.query_row_map(
            "SELECT COUNT(*) FROM sources WHERE id = ?1",
            fparams![source_id.as_str()],
            |row| row.get_typed(0),
        )?;
        if exists == 0 {
            let kind_str = source_kind.to_string();
            let now = FrankenStorage::now_millis();
            tracing::debug!(
                target: "cass::fk_guard",
                source_id = %source_id,
                kind = kind_str.as_str(),
                "inserting source row inside transaction to satisfy FK constraint"
            );
            tx.execute_compat(
                "INSERT OR IGNORE INTO sources(id, kind, host_label, created_at, updated_at)
                 VALUES(?1, ?2, ?3, ?4, ?5)",
                fparams![
                    source_id.as_str(),
                    kind_str.as_str(),
                    host_label.as_deref(),
                    now,
                    now
                ],
            )?;
        }
    }
    Ok(())
}

fn env_flag_enabled(name: &str) -> bool {
    dotenvy::var(name)
        .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
        .unwrap_or(false)
}

fn defer_storage_lexical_updates_enabled() -> bool {
    env_flag_enabled("CASS_DEFER_LEXICAL_UPDATES")
}

fn defer_analytics_updates_enabled() -> bool {
    env_flag_enabled("CASS_DEFER_ANALYTICS_UPDATES")
}

enum ConversationInsertStatus {
    Inserted(i64),
    Existing(i64),
}

fn franken_find_existing_conversation_by_key(
    tx: &FrankenTransaction<'_>,
    key: &PendingConversationKey,
    conv: Option<&Conversation>,
) -> Result<Option<i64>> {
    match key {
        PendingConversationKey::External {
            source_id,
            agent_id,
            external_id,
        } => tx
            .query_row_map(
                "SELECT id
                 FROM conversations NOT INDEXED
                 WHERE source_id = ?1 AND agent_id = ?2 AND external_id = ?3",
                fparams![source_id.as_str(), *agent_id, external_id.as_str()],
                |row| row.get_typed(0),
            )
            .optional()
            .map_err(Into::into),
        PendingConversationKey::SourcePath {
            source_id,
            agent_id,
            source_path,
            started_at,
        } => {
            let exact_match = tx
                .query_row_map(
                    "SELECT c.id
                     FROM conversations c NOT INDEXED
                     WHERE c.source_id = ?1
                       AND c.agent_id = ?2
                       AND c.source_path = ?3
                       AND ((
                            COALESCE(
                                c.started_at,
                                (SELECT MIN(created_at)
                                 FROM messages
                                 WHERE conversation_id = c.id
                                   AND created_at IS NOT NULL)
                            ) IS NULL
                            AND ?4 IS NULL
                       ) OR COALESCE(
                            c.started_at,
                            (SELECT MIN(created_at)
                             FROM messages
                             WHERE conversation_id = c.id
                               AND created_at IS NOT NULL)
                       ) = ?4)
                     ORDER BY c.id
                     LIMIT 1",
                    fparams![
                        source_id.as_str(),
                        *agent_id,
                        source_path.as_str(),
                        *started_at
                    ],
                    |row| row.get_typed(0),
                )
                .optional()?;
            if exact_match.is_some() {
                return Ok(exact_match);
            }

            let Some(conv) = conv else {
                return Ok(None);
            };
            let incoming_fingerprints = conversation_message_fingerprints(conv);
            if incoming_fingerprints.is_empty() {
                return Ok(None);
            }
            let incoming_replay_fingerprints = conversation_message_replay_fingerprints(conv);

            let candidates: Vec<(i64, Option<i64>)> = tx.query_map_collect(
                "SELECT
                     c.id,
                     COALESCE(
                         c.started_at,
                         (SELECT MIN(created_at)
                          FROM messages
                          WHERE conversation_id = c.id
                            AND created_at IS NOT NULL)
                     ) AS effective_started_at
                 FROM conversations c NOT INDEXED
                 WHERE c.source_id = ?1
                   AND c.agent_id = ?2
                   AND c.source_path = ?3
                 ORDER BY c.id",
                fparams![source_id.as_str(), *agent_id, source_path.as_str()],
                |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
            )?;

            let mut best_candidate: Option<(i64, ConversationMergeEvidence)> = None;
            for (candidate_id, candidate_started_at) in candidates {
                let existing_fingerprints =
                    franken_existing_message_fingerprints(tx, candidate_id)?;
                let existing_replay_fingerprints =
                    replay_fingerprints_from_merge_set(&existing_fingerprints);
                let Some(evidence) = conversation_merge_evidence(
                    &incoming_fingerprints,
                    &incoming_replay_fingerprints,
                    &existing_fingerprints,
                    &existing_replay_fingerprints,
                    *started_at,
                    candidate_started_at,
                ) else {
                    continue;
                };

                let candidate_key = (
                    evidence.exact_overlap,
                    evidence.replay_overlap,
                    evidence.started_close,
                    evidence.smaller_replay_set,
                    std::cmp::Reverse(evidence.start_distance_ms),
                );
                let should_replace = best_candidate
                    .as_ref()
                    .map(|(_, best_evidence)| {
                        candidate_key
                            > (
                                best_evidence.exact_overlap,
                                best_evidence.replay_overlap,
                                best_evidence.started_close,
                                best_evidence.smaller_replay_set,
                                std::cmp::Reverse(best_evidence.start_distance_ms),
                            )
                    })
                    .unwrap_or(true);

                if should_replace {
                    best_candidate = Some((candidate_id, evidence));
                }
            }

            Ok(best_candidate.map(|(candidate_id, _)| candidate_id))
        }
    }
}

fn franken_insert_conversation_or_get_existing(
    tx: &FrankenTransaction<'_>,
    agent_id: i64,
    workspace_id: Option<i64>,
    conv: &Conversation,
) -> Result<ConversationInsertStatus> {
    let conversation_key = conversation_merge_key(agent_id, conv);
    if let Some(existing_id) =
        franken_find_existing_conversation_by_key(tx, &conversation_key, Some(conv))?
    {
        return Ok(ConversationInsertStatus::Existing(existing_id));
    }

    franken_insert_conversation_or_get_existing_after_miss(
        tx,
        agent_id,
        workspace_id,
        conv,
        &conversation_key,
    )
}

fn franken_insert_conversation_or_get_existing_after_miss(
    tx: &FrankenTransaction<'_>,
    agent_id: i64,
    workspace_id: Option<i64>,
    conv: &Conversation,
    conversation_key: &PendingConversationKey,
) -> Result<ConversationInsertStatus> {
    match franken_insert_conversation(tx, agent_id, workspace_id, conv) {
        Ok(Some(conv_id)) => Ok(ConversationInsertStatus::Inserted(conv_id)),
        Ok(None) => {
            // A concurrent writer won the unique-provenance race. Resolve the
            // canonical row so callers can merge messages into it.
            let existing_id =
                franken_find_existing_conversation_by_key(tx, conversation_key, Some(conv))?
                    .with_context(|| {
                        format!(
                            "conversation INSERT produced a duplicate conflict but existing row was not found for source_id={} agent_id={} external_id={:?} source_path={}",
                            conv.source_id,
                            agent_id,
                            conv.external_id,
                            conv.source_path.display()
                        )
                    })?;
            tracing::warn!(
                source_id = %conv.source_id,
                agent_id,
                external_id = ?conv.external_id,
                existing_id,
                source_path = %conv.source_path.display(),
                "conversation INSERT: duplicate gracefully recovered, reusing existing row"
            );
            Ok(ConversationInsertStatus::Existing(existing_id))
        }
        Err(error) => {
            tracing::error!(
                source_id = %conv.source_id,
                agent_id,
                external_id = ?conv.external_id,
                error = %error,
                source_path = %conv.source_path.display(),
                "franken_insert_conversation failed"
            );
            Err(error)
        }
    }
}

/// Insert a conversation into the DB within a frankensqlite transaction.
///
/// Uses a plain `INSERT` so the common miss path stays on the slim direct
/// insert lane. Duplicate provenance conflicts are converted into `Ok(None)`
/// so callers can recover the canonical row and merge messages into it.
fn franken_insert_conversation(
    tx: &FrankenTransaction<'_>,
    agent_id: i64,
    workspace_id: Option<i64>,
    conv: &Conversation,
) -> Result<Option<i64>> {
    let metadata_bin = serialize_json_to_msgpack(&conv.metadata_json);
    let (last_message_idx, last_message_created_at) = conversation_tail_state(conv);

    let metadata_json_str = serde_json::to_string(&conv.metadata_json)?;
    let metadata_bin_bytes = metadata_bin.as_deref();

    match tx.execute_compat(
        "INSERT INTO conversations(
            agent_id, workspace_id, source_id, external_id, title, source_path,
            started_at, ended_at, approx_tokens, metadata_json, origin_host, metadata_bin,
            last_message_idx, last_message_created_at
        ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
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
            metadata_bin_bytes,
            last_message_idx,
            last_message_created_at
        ],
    ) {
        Ok(_) => Ok(Some(franken_last_rowid(tx)?)),
        Err(frankensqlite::FrankenError::UniqueViolation { .. }) => {
            tracing::debug!(
                source_id = %conv.source_id,
                agent_id,
                external_id = ?conv.external_id,
                source_path = %conv.source_path.display(),
                "conversation INSERT: duplicate provenance conflict"
            );
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

fn franken_insert_new_message(
    tx: &FrankenTransaction<'_>,
    conversation_id: i64,
    msg: &Message,
) -> Result<i64> {
    let (extra_json_str, extra_bin) = franken_message_insert_payload(msg)?;
    let extra_bin_bytes = extra_bin.as_deref();

    tx.execute_compat(
        "INSERT INTO messages(conversation_id, idx, role, author, created_at, content, extra_json, extra_bin)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            fparams![
                conversation_id,
                msg.idx,
                role_as_str(&msg.role),
                msg.author.as_deref(),
                msg.created_at,
                msg.content.as_str(),
                extra_json_str.as_ref(),
                extra_bin_bytes
        ],
    )?;
    franken_last_rowid(tx)
}

fn franken_message_insert_payload(msg: &Message) -> Result<(Cow<'_, str>, Option<Vec<u8>>)> {
    if let Some(raw) = historical_raw_json(&msg.extra_json) {
        Ok((Cow::Borrowed(raw), None))
    } else {
        Ok((
            Cow::Owned(serde_json::to_string(&msg.extra_json)?),
            serialize_json_to_msgpack(&msg.extra_json),
        ))
    }
}

/// Batch size for proven-new message inserts.
///
/// Each row binds 8 values, so 100 rows stays well under SQLite's default
/// `SQLITE_MAX_VARIABLE_NUMBER` limit of 999 while still amortizing parse cost.
const MESSAGE_INSERT_BATCH_SIZE: usize = 100;

/// Append workloads commit fastest with medium chunks on current frankensqlite.
///
/// After frankensqlite's child-FK insert fast path, 20-row chunks beat both the
/// old 10-row setting and row-wise direct inserts on the append-merge profile,
/// while 50-row chunks bring back commit-time pressure.
const APPEND_MESSAGE_INSERT_BATCH_SIZE: usize = 20;

fn franken_batch_insert_new_messages(
    tx: &FrankenTransaction<'_>,
    conversation_id: i64,
    messages: &[&Message],
) -> Result<Vec<i64>> {
    franken_batch_insert_new_messages_with_batch_size(
        tx,
        conversation_id,
        messages,
        MESSAGE_INSERT_BATCH_SIZE,
    )
}

fn franken_append_insert_new_messages(
    tx: &FrankenTransaction<'_>,
    conversation_id: i64,
    messages: &[&Message],
) -> Result<Vec<i64>> {
    franken_batch_insert_new_messages_with_batch_size(
        tx,
        conversation_id,
        messages,
        APPEND_MESSAGE_INSERT_BATCH_SIZE,
    )
}

fn franken_batch_insert_new_messages_with_batch_size(
    tx: &FrankenTransaction<'_>,
    conversation_id: i64,
    messages: &[&Message],
    batch_size: usize,
) -> Result<Vec<i64>> {
    let batch_size = batch_size.max(1);
    let mut inserted_ids = Vec::with_capacity(messages.len());
    for chunk in messages.chunks(batch_size) {
        if chunk.len() == 1 {
            inserted_ids.push(franken_insert_new_message(tx, conversation_id, chunk[0])?);
            continue;
        }
        let placeholders = (0..chunk.len())
            .map(|idx| {
                let base = idx * 8;
                format!(
                    "(?{},?{},?{},?{},?{},?{},?{},?{})",
                    base + 1,
                    base + 2,
                    base + 3,
                    base + 4,
                    base + 5,
                    base + 6,
                    base + 7,
                    base + 8
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "INSERT INTO messages(conversation_id, idx, role, author, created_at, content, extra_json, extra_bin) VALUES {placeholders}"
        );

        let mut param_values: Vec<SqliteValue> = Vec::with_capacity(chunk.len() * 8);
        for msg in chunk {
            let (extra_json_str, extra_bin) = franken_message_insert_payload(msg)?;
            param_values.push(SqliteValue::from(conversation_id));
            param_values.push(SqliteValue::from(msg.idx));
            param_values.push(SqliteValue::from(role_as_str(&msg.role)));
            param_values.push(SqliteValue::from(msg.author.as_deref()));
            param_values.push(SqliteValue::from(msg.created_at));
            param_values.push(SqliteValue::from(msg.content.as_str()));
            param_values.push(SqliteValue::from(extra_json_str.as_ref()));
            param_values.push(SqliteValue::from(extra_bin.as_deref()));
        }

        tx.execute_with_params(&sql, &param_values)?;

        let last_id = franken_last_rowid(tx)?;
        let first_id = last_id
            .checked_sub((chunk.len() - 1) as i64)
            .with_context(|| {
                format!(
                    "inferring rowid range for {}-row message batch ending at {last_id}",
                    chunk.len()
                )
            })?;
        inserted_ids.extend((0..chunk.len()).map(|offset| first_id + offset as i64));
    }

    Ok(inserted_ids)
}

#[cfg(test)]
fn franken_insert_new_message_with_profile(
    tx: &FrankenTransaction<'_>,
    conversation_id: i64,
    msg: &Message,
    profile: &mut MessageInsertSubstageProfile,
) -> Result<i64> {
    profile.single_row_calls += 1;
    profile.batch_rows += 1;

    let payload_start = Instant::now();
    let (extra_json_str, extra_bin) = franken_message_insert_payload(msg)?;
    profile.payload_duration += payload_start.elapsed();
    let extra_bin_bytes = extra_bin.as_deref();

    let execute_start = Instant::now();
    tx.execute_compat(
        "INSERT INTO messages(conversation_id, idx, role, author, created_at, content, extra_json, extra_bin)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            fparams![
                conversation_id,
                msg.idx,
                role_as_str(&msg.role),
                msg.author.as_deref(),
                msg.created_at,
                msg.content.as_str(),
                extra_json_str.as_ref(),
                extra_bin_bytes
        ],
    )?;
    profile.execute_duration += execute_start.elapsed();

    let rowid_start = Instant::now();
    let rowid = franken_last_rowid(tx)?;
    profile.rowid_duration += rowid_start.elapsed();
    Ok(rowid)
}

#[cfg(test)]
fn franken_batch_insert_new_messages_with_profile(
    tx: &FrankenTransaction<'_>,
    conversation_id: i64,
    messages: &[&Message],
    profile: &mut MessageInsertSubstageProfile,
) -> Result<Vec<i64>> {
    franken_batch_insert_new_messages_with_profile_batch_size(
        tx,
        conversation_id,
        messages,
        profile,
        MESSAGE_INSERT_BATCH_SIZE,
    )
}

#[cfg(test)]
fn franken_append_insert_new_messages_with_profile(
    tx: &FrankenTransaction<'_>,
    conversation_id: i64,
    messages: &[&Message],
    profile: &mut MessageInsertSubstageProfile,
) -> Result<Vec<i64>> {
    franken_batch_insert_new_messages_with_profile_batch_size(
        tx,
        conversation_id,
        messages,
        profile,
        APPEND_MESSAGE_INSERT_BATCH_SIZE,
    )
}

#[cfg(test)]
fn franken_batch_insert_new_messages_with_profile_batch_size(
    tx: &FrankenTransaction<'_>,
    conversation_id: i64,
    messages: &[&Message],
    profile: &mut MessageInsertSubstageProfile,
    batch_size: usize,
) -> Result<Vec<i64>> {
    let batch_size = batch_size.max(1);
    let mut inserted_ids = Vec::with_capacity(messages.len());
    for chunk in messages.chunks(batch_size) {
        if chunk.len() == 1 {
            inserted_ids.push(franken_insert_new_message_with_profile(
                tx,
                conversation_id,
                chunk[0],
                profile,
            )?);
            continue;
        }

        profile.batch_calls += 1;
        profile.batch_rows += chunk.len();

        let sql_build_start = Instant::now();
        let placeholders = (0..chunk.len())
            .map(|idx| {
                let base = idx * 8;
                format!(
                    "(?{},?{},?{},?{},?{},?{},?{},?{})",
                    base + 1,
                    base + 2,
                    base + 3,
                    base + 4,
                    base + 5,
                    base + 6,
                    base + 7,
                    base + 8
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "INSERT INTO messages(conversation_id, idx, role, author, created_at, content, extra_json, extra_bin) VALUES {placeholders}"
        );
        profile.sql_build_duration += sql_build_start.elapsed();

        let mut param_values: Vec<SqliteValue> = Vec::with_capacity(chunk.len() * 8);
        for msg in chunk {
            let payload_start = Instant::now();
            let (extra_json_str, extra_bin) = franken_message_insert_payload(msg)?;
            profile.payload_duration += payload_start.elapsed();

            let param_build_start = Instant::now();
            param_values.push(SqliteValue::from(conversation_id));
            param_values.push(SqliteValue::from(msg.idx));
            param_values.push(SqliteValue::from(role_as_str(&msg.role)));
            param_values.push(SqliteValue::from(msg.author.as_deref()));
            param_values.push(SqliteValue::from(msg.created_at));
            param_values.push(SqliteValue::from(msg.content.as_str()));
            param_values.push(SqliteValue::from(extra_json_str.as_ref()));
            param_values.push(SqliteValue::from(extra_bin.as_deref()));
            profile.param_build_duration += param_build_start.elapsed();
        }

        let execute_start = Instant::now();
        tx.execute_with_params(&sql, &param_values)?;
        profile.execute_duration += execute_start.elapsed();

        let rowid_start = Instant::now();
        let last_id = franken_last_rowid(tx)?;
        let first_id = last_id
            .checked_sub((chunk.len() - 1) as i64)
            .with_context(|| {
                format!(
                    "inferring rowid range for {}-row message batch ending at {last_id}",
                    chunk.len()
                )
            })?;
        inserted_ids.extend((0..chunk.len()).map(|offset| first_id + offset as i64));
        profile.rowid_duration += rowid_start.elapsed();
    }

    Ok(inserted_ids)
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

fn franken_existing_message_fingerprints(
    tx: &FrankenTransaction<'_>,
    conversation_id: i64,
) -> Result<HashSet<MessageMergeFingerprint>> {
    let rows = tx.query_params(
        "SELECT idx, role, author, created_at, content
         FROM messages
         WHERE conversation_id = ?1",
        fparams![conversation_id],
    )?;
    let mut fingerprints = HashSet::with_capacity(rows.len());
    for row in rows {
        let role: String = row.get_typed(1)?;
        let content: String = row.get_typed(4)?;
        fingerprints.insert(MessageMergeFingerprint {
            idx: row.get_typed(0)?,
            created_at: row.get_typed(3)?,
            role: role_from_str(&role),
            author: row.get_typed(2)?,
            content_hash: *blake3::hash(content.as_bytes()).as_bytes(),
        });
    }
    Ok(fingerprints)
}

struct ExistingMessageLookup {
    by_idx: HashMap<i64, MessageMergeFingerprint>,
    replay: HashSet<MessageReplayFingerprint>,
}

fn franken_existing_message_lookup(
    tx: &FrankenTransaction<'_>,
    conversation_id: i64,
    incoming_messages: &[Message],
) -> Result<ExistingMessageLookup> {
    if incoming_messages.is_empty() {
        return Ok(ExistingMessageLookup {
            by_idx: HashMap::new(),
            replay: HashSet::new(),
        });
    }

    let min_idx = incoming_messages
        .iter()
        .map(|msg| msg.idx)
        .min()
        .unwrap_or(0);
    let max_idx = incoming_messages
        .iter()
        .map(|msg| msg.idx)
        .max()
        .unwrap_or(min_idx);
    let requires_full_scan = incoming_messages.iter().any(|msg| msg.created_at.is_none());
    let created_bounds = incoming_messages
        .iter()
        .filter_map(|msg| msg.created_at)
        .fold(None, |bounds: Option<(i64, i64)>, created_at| {
            Some(match bounds {
                Some((min_created_at, max_created_at)) => (
                    min_created_at.min(created_at),
                    max_created_at.max(created_at),
                ),
                None => (created_at, created_at),
            })
        });

    let (rows, replay_full_scan) = if requires_full_scan {
        (
            tx.query_params(
                "SELECT idx, role, author, created_at, content
                 FROM messages
                 WHERE conversation_id = ?1",
                fparams![conversation_id],
            )?,
            true,
        )
    } else if let Some((min_created_at, max_created_at)) = created_bounds {
        (
            tx.query_params(
                "SELECT idx, role, author, created_at, content
                 FROM messages
                 WHERE conversation_id = ?1
                   AND (
                        (idx >= ?2 AND idx <= ?3)
                        OR (
                            created_at IS NOT NULL
                            AND created_at >= ?4
                            AND created_at <= ?5
                        )
                   )",
                fparams![
                    conversation_id,
                    min_idx,
                    max_idx,
                    min_created_at,
                    max_created_at
                ],
            )?,
            false,
        )
    } else {
        (
            tx.query_params(
                "SELECT idx, role, author, created_at, content
                 FROM messages
                 WHERE conversation_id = ?1",
                fparams![conversation_id],
            )?,
            true,
        )
    };

    let mut by_idx = HashMap::with_capacity(rows.len());
    let mut replay = HashSet::with_capacity(rows.len());
    for row in rows {
        let idx: i64 = row.get_typed(0)?;
        let role: String = row.get_typed(1)?;
        let author: Option<String> = row.get_typed(2)?;
        let created_at: Option<i64> = row.get_typed(3)?;
        let content: String = row.get_typed(4)?;
        let role = role_from_str(&role);
        let content_hash = *blake3::hash(content.as_bytes()).as_bytes();

        if idx >= min_idx && idx <= max_idx {
            by_idx.insert(
                idx,
                MessageMergeFingerprint {
                    idx,
                    created_at,
                    role: role.clone(),
                    author: author.clone(),
                    content_hash,
                },
            );
        }

        let replay_matches = if replay_full_scan {
            true
        } else if let Some((min_created_at, max_created_at)) = created_bounds {
            created_at.is_some_and(|ts| ts >= min_created_at && ts <= max_created_at)
        } else {
            true
        };
        if replay_matches {
            replay.insert(MessageReplayFingerprint {
                created_at,
                role,
                author,
                content_hash,
            });
        }
    }

    Ok(ExistingMessageLookup { by_idx, replay })
}

fn franken_existing_message_lookup_with_pending(
    tx: &FrankenTransaction<'_>,
    conversation_id: i64,
    incoming_messages: &[Message],
    pending_message_fingerprints: &mut HashMap<i64, HashMap<i64, MessageMergeFingerprint>>,
    pending_message_replay_fingerprints: &mut HashMap<i64, HashSet<MessageReplayFingerprint>>,
) -> Result<ExistingMessageLookup> {
    if let (Some(by_idx), Some(replay)) = (
        pending_message_fingerprints.get(&conversation_id),
        pending_message_replay_fingerprints.get(&conversation_id),
    ) {
        return Ok(ExistingMessageLookup {
            by_idx: by_idx.clone(),
            replay: replay.clone(),
        });
    }

    let lookup = franken_existing_message_lookup(tx, conversation_id, incoming_messages)?;
    pending_message_fingerprints.insert(conversation_id, lookup.by_idx.clone());
    pending_message_replay_fingerprints.insert(conversation_id, lookup.replay.clone());
    Ok(lookup)
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

        let mut param_values: Vec<SqliteValue> = Vec::with_capacity(chunk.len() * 7);
        for entry in chunk {
            param_values.push(SqliteValue::from(entry.message_id));
            param_values.push(SqliteValue::from(entry.content.as_str()));
            param_values.push(SqliteValue::from(entry.title.as_str()));
            param_values.push(SqliteValue::from(entry.agent.as_str()));
            param_values.push(SqliteValue::from(entry.workspace.as_str()));
            param_values.push(SqliteValue::from(entry.source_path.as_str()));
            param_values.push(SqliteValue::from(entry.created_at));
        }

        match tx.execute_with_params(&sql, &param_values) {
            Ok(_) => {
                inserted += chunk.len();
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    chunk_docs = chunk.len(),
                    "frankensqlite FTS batch insert failed; skipping db-resident FTS maintenance because Tantivy is authoritative"
                );
                return Ok(inserted);
            }
        }
    }

    Ok(inserted)
}

/// Update daily stats within a frankensqlite transaction.
fn franken_update_daily_stats_in_tx(
    storage: &FrankenStorage,
    tx: &FrankenTransaction<'_>,
    agent_slug: &str,
    source_id: &str,
    started_at: Option<i64>,
    delta: StatsDelta,
) -> Result<()> {
    let day_id = started_at
        .map(FrankenStorage::day_id_from_millis)
        .unwrap_or(0);
    let now = FrankenStorage::now_millis();

    franken_apply_daily_stats_delta_in_tx(
        storage,
        tx,
        DailyStatsTarget {
            day_id,
            agent_slug,
            source_id,
        },
        now,
        delta,
    )?;
    franken_apply_daily_stats_delta_in_tx(
        storage,
        tx,
        DailyStatsTarget {
            day_id,
            agent_slug: "all",
            source_id,
        },
        now,
        delta,
    )?;
    franken_apply_daily_stats_delta_in_tx(
        storage,
        tx,
        DailyStatsTarget {
            day_id,
            agent_slug,
            source_id: "all",
        },
        now,
        delta,
    )?;
    franken_apply_daily_stats_delta_in_tx(
        storage,
        tx,
        DailyStatsTarget {
            day_id,
            agent_slug: "all",
            source_id: "all",
        },
        now,
        delta,
    )?;

    Ok(())
}

#[derive(Clone, Copy)]
struct DailyStatsTarget<'a> {
    day_id: i64,
    agent_slug: &'a str,
    source_id: &'a str,
}

fn franken_apply_daily_stats_delta_in_tx(
    storage: &FrankenStorage,
    tx: &FrankenTransaction<'_>,
    target: DailyStatsTarget<'_>,
    now: i64,
    delta: StatsDelta,
) -> Result<()> {
    let cache_key = EnsuredDailyStatsKey::new(target.day_id, target.agent_slug, target.source_id);
    if storage.daily_stats_key_already_ensured(&cache_key) {
        let rows_changed = tx.execute_compat(
            "UPDATE daily_stats
             SET session_count = session_count + ?4,
                 message_count = message_count + ?5,
                 total_chars = total_chars + ?6,
                 last_updated = ?7
             WHERE day_id = ?1 AND agent_slug = ?2 AND source_id = ?3",
            fparams![
                target.day_id,
                target.agent_slug,
                target.source_id,
                delta.session_count_delta,
                delta.message_count_delta,
                delta.total_chars_delta,
                now
            ],
        )?;
        if rows_changed > 0 {
            return Ok(());
        }
    }

    tx.execute_compat(
        "INSERT INTO daily_stats(day_id, agent_slug, source_id, session_count, message_count, total_chars, last_updated)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(day_id, agent_slug, source_id) DO UPDATE SET
            session_count = session_count + excluded.session_count,
            message_count = message_count + excluded.message_count,
            total_chars = total_chars + excluded.total_chars,
            last_updated = excluded.last_updated",
        fparams![
            target.day_id,
            target.agent_slug,
            target.source_id,
            delta.session_count_delta,
            delta.message_count_delta,
            delta.total_chars_delta,
            now
        ],
    )?;
    storage.mark_daily_stats_key_ensured(cache_key);
    Ok(())
}

// -------------------------------------------------------------------------
// Frankensqlite batch helpers
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
    let mut total_affected = 0;

    // Keep frankensqlite UPSERTs row-wise inside the transaction. The
    // multi-row VALUES ... ON CONFLICT form still falls back through
    // INSERT...SELECT in fsqlite-core, which rejects UPSERT/RETURNING during
    // real cass indexing.
    for (day_id, agent, source, delta) in entries {
        total_affected += tx.execute_compat(
            "INSERT INTO daily_stats (day_id, agent_slug, source_id, session_count, message_count, total_chars, last_updated)
             VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(day_id, agent_slug, source_id) DO UPDATE SET
                 session_count = session_count + excluded.session_count,
                 message_count = message_count + excluded.message_count,
                 total_chars = total_chars + excluded.total_chars,
                 last_updated = excluded.last_updated",
            fparams![
                *day_id,
                agent.as_str(),
                source.as_str(),
                delta.session_count_delta,
                delta.message_count_delta,
                delta.total_chars_delta,
                now
            ],
        )?;
    }

    Ok(total_affected)
}

/// Batch insert token_usage rows within a frankensqlite transaction.
///
/// Uses row-wise INSERT OR IGNORE to avoid the frankensqlite limitation where
/// multi-row VALUES lists fall through to INSERT...SELECT, which rejects
/// UPSERT/OR IGNORE conflict clauses.
fn franken_insert_token_usage_batched_in_tx(
    tx: &FrankenTransaction<'_>,
    entries: &[TokenUsageEntry],
) -> Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }

    let mut total_inserted = 0;

    for e in entries {
        let params_vec: Vec<ParamValue> = vec![
            ParamValue::from(e.message_id),
            ParamValue::from(e.conversation_id),
            ParamValue::from(e.agent_id),
            ParamValue::from(e.workspace_id),
            ParamValue::from(e.source_id.clone()),
            ParamValue::from(e.timestamp_ms),
            ParamValue::from(e.day_id),
            ParamValue::from(e.model_name.clone()),
            ParamValue::from(e.model_family.clone()),
            ParamValue::from(e.model_tier.clone()),
            ParamValue::from(e.service_tier.clone()),
            ParamValue::from(e.provider.clone()),
            ParamValue::from(e.input_tokens),
            ParamValue::from(e.output_tokens),
            ParamValue::from(e.cache_read_tokens),
            ParamValue::from(e.cache_creation_tokens),
            ParamValue::from(e.thinking_tokens),
            ParamValue::from(e.total_tokens),
            ParamValue::from(e.estimated_cost_usd),
            ParamValue::from(e.role.clone()),
            ParamValue::from(e.content_chars),
            ParamValue::from(e.has_tool_calls as i64),
            ParamValue::from(e.tool_call_count as i64),
            ParamValue::from(e.data_source.clone()),
        ];

        let values = param_slice_to_values(&params_vec);
        total_inserted += tx.execute_with_params(
            "INSERT OR IGNORE INTO token_usage (
                message_id, conversation_id, agent_id, workspace_id, source_id,
                timestamp_ms, day_id,
                model_name, model_family, model_tier, service_tier, provider,
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                thinking_tokens, total_tokens, estimated_cost_usd,
                role, content_chars, has_tool_calls, tool_call_count, data_source
            )
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)",
            &values,
        )?;
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
    let mut total_affected = 0;

    for (day_id, agent, source, model, delta) in entries {
        total_affected += tx.execute_compat(
            "INSERT INTO token_daily_stats (
                day_id, agent_slug, source_id, model_family,
                api_call_count, user_message_count, assistant_message_count, tool_message_count,
                total_input_tokens, total_output_tokens, total_cache_read_tokens,
                total_cache_creation_tokens, total_thinking_tokens, grand_total_tokens,
                total_content_chars, total_tool_calls, estimated_cost_usd, session_count,
                last_updated
            )
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
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
            fparams![
                *day_id,
                agent.as_str(),
                source.as_str(),
                model.as_str(),
                delta.api_call_count,
                delta.user_message_count,
                delta.assistant_message_count,
                delta.tool_message_count,
                delta.total_input_tokens,
                delta.total_output_tokens,
                delta.total_cache_read_tokens,
                delta.total_cache_creation_tokens,
                delta.total_thinking_tokens,
                delta.grand_total_tokens,
                delta.total_content_chars,
                delta.total_tool_calls,
                delta.estimated_cost_usd,
                delta.session_count,
                now
            ],
        )?;
    }

    Ok(total_affected)
}

/// Batch insert message_metrics rows within a frankensqlite transaction.
///
/// Uses row-wise INSERT OR IGNORE to avoid the frankensqlite limitation where
/// multi-row VALUES lists fall through to INSERT...SELECT, which rejects
/// UPSERT/OR IGNORE conflict clauses.
fn franken_insert_message_metrics_batched_in_tx(
    tx: &FrankenTransaction<'_>,
    entries: &[MessageMetricsEntry],
) -> Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }

    let mut total_inserted = 0;

    for e in entries {
        let params_vec: Vec<ParamValue> = vec![
            ParamValue::from(e.message_id),
            ParamValue::from(e.created_at_ms),
            ParamValue::from(e.hour_id),
            ParamValue::from(e.day_id),
            ParamValue::from(e.agent_slug.clone()),
            ParamValue::from(e.workspace_id),
            ParamValue::from(e.source_id.clone()),
            ParamValue::from(e.role.clone()),
            ParamValue::from(e.content_chars),
            ParamValue::from(e.content_tokens_est),
            ParamValue::from(e.model_name.clone()),
            ParamValue::from(e.model_family.clone()),
            ParamValue::from(e.model_tier.clone()),
            ParamValue::from(e.provider.clone()),
            ParamValue::from(e.api_input_tokens),
            ParamValue::from(e.api_output_tokens),
            ParamValue::from(e.api_cache_read_tokens),
            ParamValue::from(e.api_cache_creation_tokens),
            ParamValue::from(e.api_thinking_tokens),
            ParamValue::from(e.api_service_tier.clone()),
            ParamValue::from(e.api_data_source.clone()),
            ParamValue::from(e.tool_call_count),
            ParamValue::from(e.has_tool_calls as i64),
            ParamValue::from(e.has_plan as i64),
        ];

        let values = param_slice_to_values(&params_vec);
        total_inserted += tx.execute_with_params(
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
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)",
            &values,
        )?;
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

    let mut total_affected = 0;

    for ((bucket_id, agent, workspace_id, source), d) in deltas {
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
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)
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

        total_affected += tx.execute_compat(
            &sql,
            fparams![
                *bucket_id,
                agent.as_str(),
                *workspace_id,
                source.as_str(),
                d.message_count,
                d.user_message_count,
                d.assistant_message_count,
                d.tool_call_count,
                d.plan_message_count,
                d.plan_content_tokens_est_total,
                d.plan_api_tokens_total,
                d.api_coverage_message_count,
                d.content_tokens_est_total,
                d.content_tokens_est_user,
                d.content_tokens_est_assistant,
                d.api_tokens_total,
                d.api_input_tokens_total,
                d.api_output_tokens_total,
                d.api_cache_read_tokens_total,
                d.api_cache_creation_tokens_total,
                d.api_thinking_tokens_total,
                now
            ],
        )?;
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

    let mut total_affected = 0;

    for ((day_id, agent, workspace_id, source, model_family, model_tier), d) in deltas {
        total_affected += tx.execute_compat(
            "INSERT INTO usage_models_daily (
                day_id, agent_slug, workspace_id, source_id, model_family, model_tier,
                message_count, user_message_count, assistant_message_count,
                tool_call_count, plan_message_count, api_coverage_message_count,
                content_tokens_est_total, content_tokens_est_user, content_tokens_est_assistant,
                api_tokens_total, api_input_tokens_total, api_output_tokens_total,
                api_cache_read_tokens_total, api_cache_creation_tokens_total,
                api_thinking_tokens_total, last_updated
            )
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)
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
                last_updated = excluded.last_updated",
            fparams![
                *day_id,
                agent.as_str(),
                *workspace_id,
                source.as_str(),
                model_family.as_str(),
                model_tier.as_str(),
                d.message_count,
                d.user_message_count,
                d.assistant_message_count,
                d.tool_call_count,
                d.plan_message_count,
                d.api_coverage_message_count,
                d.content_tokens_est_total,
                d.content_tokens_est_user,
                d.content_tokens_est_assistant,
                d.api_tokens_total,
                d.api_input_tokens_total,
                d.api_output_tokens_total,
                d.api_cache_read_tokens_total,
                d.api_cache_creation_tokens_total,
                d.api_thinking_tokens_total,
                now
            ],
        )?;
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

impl FrankenStorage {
    /// Rebuild token_daily_stats from the token_usage ledger.
    pub fn rebuild_token_daily_stats(&self) -> Result<usize> {
        const CONVERSATION_BATCH_SIZE: usize = 1_000;
        const TOKEN_USAGE_BATCH_SIZE: usize = 10_000;

        let total_usage_rows: i64 =
            self.conn
                .query_row_map("SELECT COUNT(*) FROM token_usage", fparams![], |row| {
                    row.get_typed(0)
                })?;
        tracing::info!(
            target: "cass::analytics",
            total_usage_rows,
            "token_daily_stats_rebuild_start"
        );

        let mut tx = self.conn.transaction()?;
        tx.execute("DELETE FROM token_daily_stats")?;

        let mut last_conversation_id = 0_i64;
        let mut rows_created = 0_usize;

        loop {
            let conversation_rows = tx.query_map_collect(
                "SELECT c.id, c.started_at, c.source_id,
                        COALESCE((SELECT a.slug FROM agents a WHERE a.id = c.agent_id), 'unknown')
                 FROM conversations c
                 WHERE c.id > ?1
                 ORDER BY c.id
                 LIMIT ?2",
                fparams![last_conversation_id, CONVERSATION_BATCH_SIZE as i64],
                |row| {
                    Ok((
                        row.get_typed::<i64>(0)?,
                        row.get_typed::<Option<i64>>(1)?,
                        row.get_typed::<String>(2)?,
                        row.get_typed::<String>(3)?,
                    ))
                },
            )?;
            if conversation_rows.is_empty() {
                break;
            }

            let mut aggregate = TokenStatsAggregator::new();

            for (conversation_id, started_at, source_id, agent_slug) in conversation_rows {
                last_conversation_id = conversation_id;
                let conversation_day_id = started_at.map(Self::day_id_from_millis).unwrap_or(0);
                let mut last_token_usage_id = 0_i64;
                let mut session_model_family = String::from("unknown");

                loop {
                    let usage_rows = tx.query_map_collect(
                        "SELECT id, day_id, role,
                                COALESCE(model_family, 'unknown'),
                                input_tokens, output_tokens, cache_read_tokens,
                                cache_creation_tokens, thinking_tokens,
                                has_tool_calls, tool_call_count,
                                content_chars, estimated_cost_usd
                         FROM token_usage
                         WHERE conversation_id = ?1
                           AND id > ?2
                         ORDER BY id
                         LIMIT ?3",
                        fparams![
                            conversation_id,
                            last_token_usage_id,
                            TOKEN_USAGE_BATCH_SIZE as i64
                        ],
                        |row| {
                            Ok((
                                row.get_typed::<i64>(0)?,
                                row.get_typed::<i64>(1)?,
                                row.get_typed::<String>(2)?,
                                row.get_typed::<String>(3)?,
                                row.get_typed::<Option<i64>>(4)?,
                                row.get_typed::<Option<i64>>(5)?,
                                row.get_typed::<Option<i64>>(6)?,
                                row.get_typed::<Option<i64>>(7)?,
                                row.get_typed::<Option<i64>>(8)?,
                                row.get_typed::<i64>(9)?,
                                row.get_typed::<i64>(10)?,
                                row.get_typed::<i64>(11)?,
                                row.get_typed::<Option<f64>>(12)?,
                            ))
                        },
                    )?;
                    if usage_rows.is_empty() {
                        break;
                    }

                    for (
                        token_usage_id,
                        day_id,
                        role,
                        model_family,
                        input_tokens,
                        output_tokens,
                        cache_read_tokens,
                        cache_creation_tokens,
                        thinking_tokens,
                        has_tool_calls,
                        tool_call_count,
                        content_chars,
                        estimated_cost_usd,
                    ) in usage_rows
                    {
                        last_token_usage_id = token_usage_id;
                        if model_family != "unknown" {
                            session_model_family = model_family.clone();
                        }
                        let usage = crate::connectors::ExtractedTokenUsage {
                            model_name: None,
                            provider: None,
                            input_tokens,
                            output_tokens,
                            cache_read_tokens,
                            cache_creation_tokens,
                            thinking_tokens,
                            service_tier: None,
                            has_tool_calls: has_tool_calls != 0,
                            tool_call_count: u32::try_from(tool_call_count.max(0)).unwrap_or(0),
                            data_source: franken_agent_detection::TokenDataSource::Api,
                        };
                        aggregate.record(
                            &agent_slug,
                            &source_id,
                            day_id,
                            &model_family,
                            &role,
                            &usage,
                            content_chars,
                            estimated_cost_usd.unwrap_or(0.0),
                        );
                    }
                }

                aggregate.record_session(
                    &agent_slug,
                    &source_id,
                    conversation_day_id,
                    &session_model_family,
                );
            }

            let entries = aggregate.expand();
            rows_created = rows_created.saturating_add(entries.len());
            franken_update_token_daily_stats_batched_in_tx(&tx, &entries)?;
        }

        tx.commit()?;

        tracing::info!(
            target: "cass::analytics",
            rows_created,
            "token_daily_stats_rebuild_complete"
        );

        Ok(rows_created)
    }

    /// Rebuild analytics tables (message_metrics + rollups) from existing
    /// messages in the database. Does NOT re-parse raw agent session files.
    pub fn rebuild_analytics(&self) -> Result<AnalyticsRebuildResult> {
        let start = Instant::now();

        let total_messages: i64 =
            self.conn
                .query_row_map("SELECT COUNT(*) FROM messages", fparams![], |row| {
                    row.get_typed(0)
                })?;
        tracing::info!(
            target: "cass::analytics",
            total_messages,
            "analytics_rebuild_start"
        );

        let mut tx = self.conn.transaction()?;

        tx.execute("DELETE FROM message_metrics")?;
        tx.execute("DELETE FROM usage_hourly")?;
        tx.execute("DELETE FROM usage_daily")?;
        tx.execute("DELETE FROM usage_models_daily")?;

        const CHUNK_SIZE: i64 = 10_000;
        let mut offset: i64 = 0;
        let mut total_inserted: usize = 0;
        let mut usage_hourly_rows: usize = 0;
        let mut usage_daily_rows: usize = 0;
        let mut usage_models_daily_rows: usize = 0;

        loop {
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
            )> = tx.query_map_collect(
                // Avoid the 3-table JOIN with LIMIT/OFFSET that triggers
                // frankensqlite's materialization fallback (see 860acb12).
                // Inline the agent slug lookup as a correlated subquery and
                // fall back to 'unknown' for NULL agent_id, matching the
                // FTS / lexical rebuild paths.
                "SELECT m.id, m.idx, m.role, m.content, m.extra_json, m.extra_bin,
                        m.created_at,
                        c.id AS conv_id, c.started_at AS conv_started_at,
                        c.source_id, c.workspace_id,
                        COALESCE((SELECT a.slug FROM agents a WHERE a.id = c.agent_id), 'unknown') AS agent_slug
                 FROM messages m
                 JOIN conversations c ON m.conversation_id = c.id
                 ORDER BY m.id
                 LIMIT ?1 OFFSET ?2",
                fparams![CHUNK_SIZE, offset],
                |row| {
                    let msg_id: i64 = row.get_typed(0)?;
                    let role: String = row.get_typed(2)?;
                    let content: String = row.get_typed(3)?;
                    let extra_json = row
                        .get_typed::<Option<String>>(4)?
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .or_else(|| {
                            row.get_typed::<Option<Vec<u8>>>(5)
                                .ok()
                                .flatten()
                                .and_then(|b| rmp_serde::from_slice(&b).ok())
                        });
                    let msg_ts: Option<i64> = row.get_typed(6)?;
                    let conv_started_at: Option<i64> = row.get_typed(8)?;
                    let source_id: String = row.get_typed(9)?;
                    let workspace_id: Option<i64> = row.get_typed(10)?;
                    let agent_slug: String = row.get_typed(11)?;
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
                },
            )?;

            if rows.is_empty() {
                break;
            }

            let chunk_len = rows.len();
            let mut entries = Vec::with_capacity(chunk_len);
            let mut rollup_agg = AnalyticsRollupAggregator::new();

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

                let entry = MessageMetricsEntry {
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
                };
                rollup_agg.record(&entry);
                entries.push(entry);
            }

            total_inserted += franken_insert_message_metrics_batched_in_tx(&tx, &entries)?;
            let (hourly, daily, models_daily) =
                franken_flush_analytics_rollups_in_tx(&tx, &rollup_agg)?;
            usage_hourly_rows += hourly;
            usage_daily_rows += daily;
            usage_models_daily_rows += models_daily;
            offset += chunk_len as i64;

            tracing::debug!(
                target: "cass::analytics",
                offset,
                chunk = chunk_len,
                inserted = entries.len(),
                total = total_inserted,
                "analytics_rebuild_chunk"
            );

            if (chunk_len as i64) < CHUNK_SIZE {
                break;
            }
        }

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
            usage_hourly_rows,
            usage_daily_rows,
            usage_models_daily_rows,
            elapsed_ms,
            messages_per_sec = format!("{:.0}", msgs_per_sec),
            "analytics_rebuild_complete"
        );

        Ok(AnalyticsRebuildResult {
            message_metrics_rows: total_inserted,
            usage_hourly_rows,
            usage_daily_rows,
            usage_models_daily_rows,
            elapsed_ms,
            messages_per_sec: msgs_per_sec,
        })
    }

    /// Rebuild all daily stats from scratch.
    pub fn rebuild_daily_stats(&self) -> Result<DailyStatsRebuildResult> {
        const DAILY_STATS_REBUILD_CONVERSATION_BATCH_SIZE: usize = 1_000;
        const DAILY_STATS_REBUILD_MESSAGE_BATCH_SIZE: usize = 10_000;

        let mut conversation_batch_size = rebuild_batch_size_env(
            "CASS_DAILY_STATS_REBUILD_CONVERSATION_BATCH_SIZE",
            DAILY_STATS_REBUILD_CONVERSATION_BATCH_SIZE,
        );
        let mut message_batch_size = rebuild_batch_size_env(
            "CASS_DAILY_STATS_REBUILD_MESSAGE_BATCH_SIZE",
            DAILY_STATS_REBUILD_MESSAGE_BATCH_SIZE,
        );

        let total_messages: i64 =
            self.conn
                .query_row_map("SELECT COUNT(*) FROM messages", fparams![], |row| {
                    row.get_typed(0)
                })?;
        let message_metrics_rows: i64 =
            self.conn
                .query_row_map("SELECT COUNT(*) FROM message_metrics", fparams![], |row| {
                    row.get_typed(0)
                })?;
        let use_message_metrics = total_messages > 0 && total_messages == message_metrics_rows;

        tracing::info!(
            target: "cass::perf::daily_stats",
            total_messages,
            message_metrics_rows,
            use_message_metrics,
            "daily_stats rebuild selected message source"
        );

        let mut tx = self.conn.transaction()?;
        tx.execute("DELETE FROM daily_stats")?;

        let mut last_conversation_id = 0_i64;
        let mut conversation_batch_count = 0_usize;
        let mut conversations_processed = 0_usize;
        let mut messages_processed = 0_usize;
        let mut message_batch_count = 0_usize;
        let mut raw_entries_flushed = 0_usize;
        let mut expanded_entries_flushed = 0_usize;
        let message_scan_sql = if use_message_metrics {
            "SELECT m.idx, mm.content_chars
             FROM messages m
             JOIN message_metrics mm ON mm.message_id = m.id
             WHERE m.conversation_id = ?1
               AND m.idx > ?2
             ORDER BY m.conversation_id, m.idx
             LIMIT ?3"
        } else {
            "SELECT m.idx, COALESCE(LENGTH(CAST(m.content AS BLOB)), 0)
             FROM messages m
             WHERE m.conversation_id = ?1
               AND m.idx > ?2
             ORDER BY m.conversation_id, m.idx
             LIMIT ?3"
        };

        loop {
            // Avoid the 2-table JOIN with LIMIT that triggers frankensqlite's
            // materialization fallback (which is what the OOM retry below is
            // defending against — see 860acb12).  Inline agent slug via
            // correlated subquery and degrade NULL agent_id to 'unknown' for
            // consistency with the lexical/FTS rebuild paths.
            let conversation_rows = match self.conn.query_with_params(
                "SELECT c.id, c.started_at,
                        COALESCE((SELECT a.slug FROM agents a WHERE a.id = c.agent_id), 'unknown'),
                        c.source_id
                 FROM conversations c
                 WHERE c.id > ?1
                 ORDER BY c.id
                 LIMIT ?2",
                &params_from_iter([
                    ParamValue::from(last_conversation_id),
                    ParamValue::from(conversation_batch_size as i64),
                ]),
            ) {
                Ok(rows) => rows,
                Err(err) if is_out_of_memory_error(&err) && conversation_batch_size > 1 => {
                    let previous_batch_size = conversation_batch_size;
                    conversation_batch_size = (conversation_batch_size / 2).max(1);
                    tracing::warn!(
                        previous_batch_size,
                        conversation_batch_size,
                        last_conversation_id,
                        "daily_stats conversation scan ran out of memory; retrying with smaller batch"
                    );
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            if conversation_rows.is_empty() {
                break;
            }

            let mut aggregate = StatsAggregator::new();
            let mut conversation_batch_meta: Vec<(i64, i64, String, String)> =
                Vec::with_capacity(conversation_rows.len());
            for row in &conversation_rows {
                let conversation_id: i64 = row.get_typed(0)?;
                let started_at: Option<i64> = row.get_typed(1)?;
                let agent_slug: String = row.get_typed(2)?;
                let source_id: String = row.get_typed(3)?;
                last_conversation_id = conversation_id;
                let day_id = started_at.map(Self::day_id_from_millis).unwrap_or(0);
                aggregate.record_delta(&agent_slug, &source_id, day_id, 1, 0, 0);
                conversation_batch_meta.push((conversation_id, day_id, agent_slug, source_id));
                conversations_processed += 1;
            }

            conversation_batch_count += 1;
            raw_entries_flushed += aggregate.raw_entry_count();
            let entries = aggregate.expand();
            expanded_entries_flushed += entries.len();
            if !entries.is_empty() {
                franken_update_daily_stats_batched_in_tx(&tx, &entries)?;
            }
            if conversation_batch_count.is_multiple_of(25) {
                tracing::info!(
                    target: "cass::perf::daily_stats",
                    conversations_processed,
                    batches = conversation_batch_count,
                    batch_size = conversation_batch_size,
                    last_conversation_id,
                    "daily_stats rebuild conversation scan progress"
                );
            }
            if conversation_batch_meta.is_empty() {
                continue;
            }

            for (conversation_id, day_id, agent_slug, source_id) in conversation_batch_meta {
                let mut cursor_message_idx = -1_i64;
                loop {
                    let message_rows = match self.conn.query_with_params(
                        message_scan_sql,
                        &params_from_iter([
                            ParamValue::from(conversation_id),
                            ParamValue::from(cursor_message_idx),
                            ParamValue::from(message_batch_size as i64),
                        ]),
                    ) {
                        Ok(rows) => rows,
                        Err(err) if is_out_of_memory_error(&err) && message_batch_size > 1 => {
                            let previous_batch_size = message_batch_size;
                            message_batch_size = (message_batch_size / 2).max(1);
                            tracing::warn!(
                                previous_batch_size,
                                message_batch_size,
                                conversation_id,
                                cursor_message_idx,
                                "daily_stats message scan ran out of memory; retrying with smaller batch"
                            );
                            continue;
                        }
                        Err(err) => return Err(err.into()),
                    };
                    if message_rows.is_empty() {
                        break;
                    }

                    let mut aggregate = StatsAggregator::new();
                    for row in &message_rows {
                        let message_idx: i64 = row.get_typed(0)?;
                        let content_len: i64 = row.get_typed(1)?;
                        cursor_message_idx = message_idx;
                        aggregate.record_delta(&agent_slug, &source_id, day_id, 0, 1, content_len);
                        messages_processed += 1;
                    }

                    message_batch_count += 1;
                    raw_entries_flushed += aggregate.raw_entry_count();
                    let entries = aggregate.expand();
                    expanded_entries_flushed += entries.len();
                    if !entries.is_empty() {
                        franken_update_daily_stats_batched_in_tx(&tx, &entries)?;
                    }
                    if message_batch_count.is_multiple_of(50) {
                        tracing::info!(
                            target: "cass::perf::daily_stats",
                            messages_processed,
                            batches = message_batch_count,
                            batch_size = message_batch_size,
                            source = if use_message_metrics {
                                "message_metrics"
                            } else {
                                "messages"
                            },
                            conversation_id,
                            cursor_message_idx,
                            "daily_stats rebuild message scan progress"
                        );
                    }
                }
            }
        }

        let rows_created: i64 =
            tx.query_row_map("SELECT COUNT(*) FROM daily_stats", fparams![], |row| {
                row.get_typed(0)
            })?;
        let total_sessions: i64 = tx.query_row_map(
            "SELECT COALESCE(SUM(session_count), 0) FROM daily_stats WHERE agent_slug = 'all' AND source_id = 'all'",
            fparams![],
            |row| row.get_typed(0),
        )?;

        tx.commit()?;

        tracing::info!(
            target: "cass::perf::daily_stats",
            rows_created,
            total_sessions,
            conversations_processed,
            conversation_batches = conversation_batch_count,
            conversation_batch_size,
            message_batches = message_batch_count,
            message_batch_size,
            messages_processed,
            use_message_metrics,
            raw_entries_flushed,
            expanded_entries_flushed,
            "Daily stats rebuilt from conversations"
        );

        Ok(DailyStatsRebuildResult {
            rows_created,
            total_sessions,
        })
    }
}

// SqliteStorage impl block removed: SqliteStorage is now a type alias for FrankenStorage.
// All methods are available through FrankenStorage.

// -------------------------------------------------------------------------
// IndexingCache (Opt 7.2) - N+1 Prevention for Agent/Workspace IDs
// -------------------------------------------------------------------------

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

pub trait IndexingCacheStorage {
    fn ensure_indexing_agent(&self, agent: &Agent) -> Result<i64>;
    fn ensure_indexing_workspace(&self, path: &Path, display_name: Option<&str>) -> Result<i64>;
}

impl IndexingCacheStorage for FrankenStorage {
    fn ensure_indexing_agent(&self, agent: &Agent) -> Result<i64> {
        self.ensure_agent(agent)
    }

    fn ensure_indexing_workspace(&self, path: &Path, display_name: Option<&str>) -> Result<i64> {
        self.ensure_workspace(path, display_name)
    }
}

// IndexingCacheStorage for SqliteStorage removed: SqliteStorage is a type alias for FrankenStorage.

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
    pub fn get_or_insert_agent<S>(&mut self, storage: &S, agent: &Agent) -> Result<i64>
    where
        S: IndexingCacheStorage + ?Sized,
    {
        if let Some(&cached) = self.agent_ids.get(&agent.slug) {
            self.hits += 1;
            return Ok(cached);
        }

        self.misses += 1;
        let id = storage.ensure_indexing_agent(agent)?;
        self.agent_ids.insert(agent.slug.clone(), id);
        Ok(id)
    }

    /// Get or insert a workspace ID, using cache if available.
    ///
    /// Returns the cached ID if present, otherwise calls ensure_workspace
    /// and caches the result.
    pub fn get_or_insert_workspace(
        &mut self,
        storage: &(impl IndexingCacheStorage + ?Sized),
        path: &Path,
        display_name: Option<&str>,
    ) -> Result<i64> {
        if let Some(&cached) = self.workspace_ids.get(path) {
            self.hits += 1;
            return Ok(cached);
        }

        self.misses += 1;
        let id = storage.ensure_indexing_workspace(path, display_name)?;
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
#[derive(Clone, Copy, Debug, Default)]
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
                d.api_tokens_total += api_total;
                d.api_input_tokens_total += entry.api_input_tokens.unwrap_or(0);
                d.api_output_tokens_total += entry.api_output_tokens.unwrap_or(0);
                d.api_cache_read_tokens_total += entry.api_cache_read_tokens.unwrap_or(0);
                d.api_cache_creation_tokens_total += entry.api_cache_creation_tokens.unwrap_or(0);
                d.api_thinking_tokens_total += entry.api_thinking_tokens.unwrap_or(0);
            }
            d.content_tokens_est_total += content_est;
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
            d.api_tokens_total += api_total;
            d.api_input_tokens_total += entry.api_input_tokens.unwrap_or(0);
            d.api_output_tokens_total += entry.api_output_tokens.unwrap_or(0);
            d.api_cache_read_tokens_total += entry.api_cache_read_tokens.unwrap_or(0);
            d.api_cache_creation_tokens_total += entry.api_cache_creation_tokens.unwrap_or(0);
            d.api_thinking_tokens_total += entry.api_thinking_tokens.unwrap_or(0);
        }
        d.content_tokens_est_total += content_est;
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
    pub fn load(conn: &FrankenConnection) -> Result<Self> {
        Self::franken_load(conn)
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

fn rebuild_batch_size_env(var: &str, default: usize) -> usize {
    dotenvy::var(var)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn is_out_of_memory_error(err: &impl std::fmt::Display) -> bool {
    err.to_string()
        .to_ascii_lowercase()
        .contains("out of memory")
}

// Second SqliteStorage impl block removed: SqliteStorage is now a type alias for FrankenStorage.
// All methods (insert_conversation_tree, list_agents, list_conversations, etc.) are
// available through FrankenStorage.

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

/// Result of purging archived data for a single agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentArchivePurgeResult {
    pub conversations_deleted: usize,
    pub messages_deleted: usize,
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

const FTS_ENTRY_BATCH_MAX_DOCS: usize = 512;
const FTS_ENTRY_BATCH_MAX_CHARS: usize = 1024 * 1024;

/// Default batch size for the FTS rebuild INSERT (Bug #168).  When
/// `fts_messages` is empty but `messages` has 100K+ rows, a single unbounded
/// INSERT-SELECT OOMs.  This constant caps each batch so peak memory stays
/// bounded.  Override via `CASS_FTS_REBUILD_BATCH_SIZE` for tuning.
const FTS_REBUILD_BATCH_SIZE_DEFAULT: usize = 5_000;

/// Read the FTS rebuild batch size from the environment, falling back to the
/// compiled-in default.
fn fts_rebuild_batch_size() -> usize {
    dotenvy::var("CASS_FTS_REBUILD_BATCH_SIZE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(FTS_REBUILD_BATCH_SIZE_DEFAULT)
}

fn flush_pending_fts_entries(
    storage: &FrankenStorage,
    tx: &FrankenTransaction<'_>,
    entries: &mut Vec<FtsEntry>,
    pending_chars: &mut usize,
    inserted_total: &mut usize,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    if storage.fts_messages_present_cached(tx) {
        *inserted_total += franken_batch_insert_fts(tx, entries)?;
    }
    entries.clear();
    *pending_chars = 0;
    Ok(())
}

fn path_to_string<P: AsRef<Path>>(p: P) -> String {
    p.as_ref().to_string_lossy().into_owned()
}

fn role_str(role: &MessageRole) -> String {
    role_as_str(role).to_owned()
}

fn role_as_str(role: &MessageRole) -> &str {
    match role {
        MessageRole::User => "user",
        MessageRole::Agent => "agent",
        MessageRole::Tool => "tool",
        MessageRole::System => "system",
        MessageRole::Other(v) => v.as_str(),
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
    use serial_test::serial;
    use tempfile::TempDir;

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                // SAFETY: test helper restores prior process env for isolation.
                unsafe {
                    std::env::set_var(self.key, value);
                }
            } else {
                // SAFETY: test helper restores prior process env for isolation.
                unsafe {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    fn set_env_var(key: &'static str, value: impl AsRef<str>) -> EnvGuard {
        let previous = dotenvy::var(key).ok();
        // SAFETY: test helper toggles a process-local env var for isolation.
        unsafe {
            std::env::set_var(key, value.as_ref());
        }
        EnvGuard { key, previous }
    }

    #[test]
    fn autocommit_retain_disable_tries_compat_then_canonical_pragma() {
        let mut attempts = Vec::new();

        let selected = disable_autocommit_retain(|pragma| {
            attempts.push(pragma);
            if pragma == "PRAGMA fsqlite.autocommit_retain = OFF;" {
                Err("compat namespace unavailable")
            } else {
                Ok(())
            }
        })
        .expect("canonical pragma should disable autocommit retain");

        assert_eq!(selected, "PRAGMA autocommit_retain = OFF;");
        assert_eq!(attempts, AUTOCOMMIT_RETAIN_OFF_PRAGMAS);
    }

    #[test]
    fn autocommit_retain_disable_fails_closed_when_no_pragma_works() {
        let mut attempts = Vec::new();

        let err = disable_autocommit_retain(|pragma| {
            attempts.push(pragma);
            Err("unsupported pragma")
        })
        .expect_err("unsupported autocommit retain controls should fail closed");

        assert_eq!(attempts, AUTOCOMMIT_RETAIN_OFF_PRAGMAS);
        let message = err.to_string();
        assert!(
            message.contains("refusing to keep a long-lived MVCC connection"),
            "error should force callers away from unbounded snapshot retention: {message}"
        );
        assert!(
            message.contains("PRAGMA fsqlite.autocommit_retain = OFF;")
                && message.contains("PRAGMA autocommit_retain = OFF;"),
            "error should preserve attempted PRAGMAs for diagnostics: {message}"
        );
    }

    /// Open a rusqlite connection on `db_path` for the narrow purpose of
    /// injecting (or inspecting the raw projection of) sqlite_master
    /// corruption patterns in test fixtures. Frankensqlite intentionally does
    /// not support `PRAGMA writable_schema` writes or raw inserts to
    /// sqlite_master (see AGENTS.md: "PRAGMA writable_schema: Not supported for
    /// write operations"), so these fixtures retain rusqlite as the standard-
    /// SQLite interop layer. All callers are in this test module and run under
    /// #[cfg(test)]; no production code path touches rusqlite here.
    fn rusqlite_test_fixture_conn(db_path: &Path) -> rusqlite::Connection {
        rusqlite::Connection::open(db_path).expect("open rusqlite test fixture connection")
    }

    fn seed_historical_db_direct(
        db_path: &Path,
        conversations: &[crate::model::types::Conversation],
    ) {
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        let conn = FrankenConnection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute_batch(HISTORICAL_RECOVERY_CORE_SCHEMA).unwrap();
        conn.execute_compat(
            "INSERT INTO agents(id, slug, name, version, kind, created_at, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            fparams![1_i64, "codex", "Codex", "0.2.3", "cli", 0_i64, 0_i64],
        )
        .unwrap();

        let mut next_message_id = 1_i64;
        for (conv_index, conv) in conversations.iter().enumerate() {
            let conversation_id = i64::try_from(conv_index + 1).unwrap();
            let workspace_id = conv.workspace.as_ref().map(|workspace| {
                let workspace_id = conversation_id;
                let workspace_path = workspace.to_string_lossy().into_owned();
                conn.execute_compat(
                    "INSERT INTO workspaces(id, path, display_name) VALUES(?1, ?2, ?3)",
                    fparams![
                        workspace_id,
                        workspace_path.as_str(),
                        workspace_path.as_str()
                    ],
                )
                .unwrap();
                workspace_id
            });
            let source_path = conv.source_path.to_string_lossy().into_owned();
            let metadata_json = conv.metadata_json.to_string();
            conn.execute_compat(
                "INSERT INTO conversations (
                    id, agent_id, workspace_id, source_id, external_id, title, source_path,
                    started_at, ended_at, approx_tokens, metadata_json, origin_host
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                fparams![
                    conversation_id,
                    1_i64,
                    workspace_id,
                    conv.source_id.as_str(),
                    conv.external_id.as_deref(),
                    conv.title.as_deref(),
                    source_path.as_str(),
                    conv.started_at,
                    conv.ended_at,
                    conv.approx_tokens,
                    metadata_json.as_str(),
                    conv.origin_host.as_deref()
                ],
            )
            .unwrap();

            for msg in &conv.messages {
                let extra_json = msg.extra_json.to_string();
                let role = role_str(&msg.role);
                conn.execute_compat(
                    "INSERT INTO messages(
                        id, conversation_id, idx, role, author, created_at, content, extra_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    fparams![
                        next_message_id,
                        conversation_id,
                        msg.idx,
                        role.as_str(),
                        msg.author.as_deref(),
                        msg.created_at,
                        msg.content.as_str(),
                        extra_json.as_str()
                    ],
                )
                .unwrap();
                next_message_id += 1;
            }
        }
    }

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
    fn create_backup_creates_named_file() {
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
    fn create_backup_paths_are_unique() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        std::fs::write(&db_path, b"test data").unwrap();

        let first = create_backup(&db_path).unwrap().unwrap();
        let second = create_backup(&db_path).unwrap().unwrap();

        assert_ne!(first, second);
        assert!(first.exists());
        assert!(second.exists());
    }

    #[test]
    fn lexical_rebuild_messages_query_uses_conversation_idx_access_path() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("agent_search.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "claude_code".into(),
            name: "Claude Code".into(),
            version: None,
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();
        let conversation = Conversation {
            id: None,
            agent_slug: "claude_code".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("conv-1".into()),
            title: Some("Lexical rebuild".into()),
            source_path: PathBuf::from("/tmp/conv-1.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![
                Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::User,
                    author: Some("user".into()),
                    created_at: Some(1_700_000_000_010),
                    content: "first".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                },
                Message {
                    id: None,
                    idx: 1,
                    role: MessageRole::Agent,
                    author: Some("assistant".into()),
                    created_at: Some(1_700_000_000_020),
                    content: "second".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                },
            ],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };
        storage
            .insert_conversation_tree(agent_id, None, &conversation)
            .unwrap();
        let conversation_id = storage
            .conn
            .query_row_map(
                "SELECT id FROM conversations WHERE external_id = ?1",
                fparams!["conv-1"],
                |row| row.get_typed::<i64>(0),
            )
            .unwrap();

        let opcodes: Vec<String> = storage
            .conn
            .query_map_collect(
                "EXPLAIN \
                 SELECT id, idx, role, author, created_at, content \
                 FROM messages \
                 WHERE conversation_id = ?1 ORDER BY idx",
                fparams![conversation_id],
                |row| row.get_typed(1),
            )
            .unwrap();

        assert!(
            opcodes.iter().any(|opcode| opcode == "SeekGE"),
            "expected lexical rebuild message fetch to seek into the conversation_id/idx access path, got {opcodes:?}"
        );
        assert!(
            !opcodes.iter().any(|opcode| opcode == "SorterOpen"),
            "expected lexical rebuild message fetch to avoid sorter temp b-trees, got {opcodes:?}"
        );
    }

    #[test]
    fn schema_check_rebuild_classification_ignores_transient_errors() {
        assert!(!schema_check_error_requires_rebuild(
            &frankensqlite::FrankenError::Busy
        ));
        assert!(!schema_check_error_requires_rebuild(
            &frankensqlite::FrankenError::CannotOpen {
                path: PathBuf::from("/tmp/test.db"),
            }
        ));
        assert!(!schema_check_error_requires_rebuild(
            &frankensqlite::FrankenError::Io(std::io::Error::other("disk hiccup"))
        ));
    }

    #[test]
    fn schema_check_rebuild_classification_keeps_corruption_errors() {
        assert!(schema_check_error_requires_rebuild(
            &frankensqlite::FrankenError::DatabaseCorrupt {
                detail: "bad header".to_string(),
            }
        ));
        assert!(schema_check_error_requires_rebuild(
            &frankensqlite::FrankenError::WalCorrupt {
                detail: "bad wal".to_string(),
            }
        ));
        assert!(schema_check_error_requires_rebuild(
            &frankensqlite::FrankenError::NotADatabase {
                path: PathBuf::from("/tmp/test.db"),
            }
        ));
        assert!(schema_check_error_requires_rebuild(
            &frankensqlite::FrankenError::ShortRead {
                expected: 4096,
                actual: 64,
            }
        ));
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

    #[test]
    fn create_backup_copies_sidecars_when_present() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        std::fs::write(&db_path, b"db").unwrap();
        std::fs::write(database_sidecar_path(&db_path, "-wal"), b"wal").unwrap();
        std::fs::write(database_sidecar_path(&db_path, "-shm"), b"shm").unwrap();

        let backup_path = create_backup(&db_path).unwrap().unwrap();

        assert_eq!(
            std::fs::read(database_sidecar_path(&backup_path, "-wal")).unwrap(),
            b"wal"
        );
        assert_eq!(
            std::fs::read(database_sidecar_path(&backup_path, "-shm")).unwrap(),
            b"shm"
        );
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

        assert_eq!(backups.len(), 3);
    }

    #[test]
    fn cleanup_old_backups_ignores_wal_and_shm_sidecars() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");

        for i in 0..3 {
            let backup_name = format!("test.db.backup.{}", 1000 + i);
            let backup_path = dir.path().join(&backup_name);
            std::fs::write(&backup_path, format!("backup {i}")).unwrap();
            std::fs::write(format!("{}-wal", backup_path.display()), b"wal").unwrap();
            std::fs::write(format!("{}-shm", backup_path.display()), b"shm").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        cleanup_old_backups(&db_path, 2).unwrap();

        let mut roots = Vec::new();
        let mut wals = Vec::new();
        let mut shms = Vec::new();
        for entry in std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with("-wal") {
                wals.push(name);
            } else if name.ends_with("-shm") {
                shms.push(name);
            } else if name.contains("backup") {
                roots.push(name);
            }
        }

        assert_eq!(roots.len(), 2, "should keep two backup roots");
        assert_eq!(
            wals.len(),
            2,
            "should keep WAL sidecars only for retained backups"
        );
        assert_eq!(
            shms.len(),
            2,
            "should keep SHM sidecars only for retained backups"
        );
    }

    #[test]
    fn move_database_bundle_moves_database_and_sidecars() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let backup_path = dir.path().join("test.db.corrupt");

        std::fs::write(&db_path, b"db").unwrap();
        std::fs::write(database_sidecar_path(&db_path, "-wal"), b"wal").unwrap();
        std::fs::write(database_sidecar_path(&db_path, "-shm"), b"shm").unwrap();

        let moved = move_database_bundle(&db_path, &backup_path).unwrap();
        assert_eq!(
            moved,
            DatabaseBundleMoveResult {
                database: true,
                wal: true,
                shm: true
            }
        );
        assert!(moved.moved_any());

        assert!(!db_path.exists());
        assert!(!database_sidecar_path(&db_path, "-wal").exists());
        assert!(!database_sidecar_path(&db_path, "-shm").exists());

        assert_eq!(std::fs::read(&backup_path).unwrap(), b"db");
        assert_eq!(
            std::fs::read(database_sidecar_path(&backup_path, "-wal")).unwrap(),
            b"wal"
        );
        assert_eq!(
            std::fs::read(database_sidecar_path(&backup_path, "-shm")).unwrap(),
            b"shm"
        );
    }

    #[test]
    fn move_database_bundle_preserves_orphan_sidecars_without_main_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let backup_path = dir.path().join("test.db.corrupt");

        std::fs::write(database_sidecar_path(&db_path, "-wal"), b"wal").unwrap();
        std::fs::write(database_sidecar_path(&db_path, "-shm"), b"shm").unwrap();

        let moved = move_database_bundle(&db_path, &backup_path).unwrap();
        assert_eq!(
            moved,
            DatabaseBundleMoveResult {
                database: false,
                wal: true,
                shm: true
            }
        );
        assert!(moved.moved_any());
        assert!(!db_path.exists());
        assert!(!database_sidecar_path(&db_path, "-wal").exists());
        assert!(!database_sidecar_path(&db_path, "-shm").exists());
        assert_eq!(
            std::fs::read(database_sidecar_path(&backup_path, "-wal")).unwrap(),
            b"wal"
        );
        assert_eq!(
            std::fs::read(database_sidecar_path(&backup_path, "-shm")).unwrap(),
            b"shm"
        );
    }

    #[test]
    fn copy_database_bundle_copies_database_and_sidecars() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let copied_path = dir.path().join("copy.db");

        std::fs::write(&db_path, b"db").unwrap();
        std::fs::write(database_sidecar_path(&db_path, "-wal"), b"wal").unwrap();
        std::fs::write(database_sidecar_path(&db_path, "-shm"), b"shm").unwrap();

        copy_database_bundle(&db_path, &copied_path).unwrap();

        assert_eq!(std::fs::read(&copied_path).unwrap(), b"db");
        assert_eq!(
            std::fs::read(database_sidecar_path(&copied_path, "-wal")).unwrap(),
            b"wal"
        );
        assert_eq!(
            std::fs::read(database_sidecar_path(&copied_path, "-shm")).unwrap(),
            b"shm"
        );
        assert_eq!(std::fs::read(&db_path).unwrap(), b"db");
    }

    #[test]
    fn copy_database_bundle_creates_destination_parent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let copied_path = dir.path().join("nested/copies/copy.db");

        std::fs::write(&db_path, b"db").unwrap();
        std::fs::write(database_sidecar_path(&db_path, "-wal"), b"wal").unwrap();

        copy_database_bundle(&db_path, &copied_path).unwrap();

        assert!(copied_path.parent().unwrap().is_dir());
        assert_eq!(std::fs::read(&copied_path).unwrap(), b"db");
        assert_eq!(
            std::fs::read(database_sidecar_path(&copied_path, "-wal")).unwrap(),
            b"wal"
        );
    }

    #[test]
    fn move_database_bundle_creates_destination_parent_and_moves_sidecars() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let backup_path = dir.path().join("nested/backups/test.db.corrupt");

        std::fs::write(&db_path, b"db").unwrap();
        std::fs::write(database_sidecar_path(&db_path, "-wal"), b"wal").unwrap();
        std::fs::write(database_sidecar_path(&db_path, "-shm"), b"shm").unwrap();

        let moved = move_database_bundle(&db_path, &backup_path).unwrap();
        assert_eq!(
            moved,
            DatabaseBundleMoveResult {
                database: true,
                wal: true,
                shm: true
            }
        );
        assert!(backup_path.parent().unwrap().is_dir());
        assert_eq!(std::fs::read(&backup_path).unwrap(), b"db");
        assert_eq!(
            std::fs::read(database_sidecar_path(&backup_path, "-wal")).unwrap(),
            b"wal"
        );
        assert_eq!(
            std::fs::read(database_sidecar_path(&backup_path, "-shm")).unwrap(),
            b"shm"
        );
    }

    #[test]
    fn remove_database_files_removes_orphan_sidecars_without_main_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");

        std::fs::write(database_sidecar_path(&db_path, "-wal"), b"wal").unwrap();
        std::fs::write(database_sidecar_path(&db_path, "-shm"), b"shm").unwrap();

        remove_database_files(&db_path).unwrap();

        assert!(!db_path.exists());
        assert!(!database_sidecar_path(&db_path, "-wal").exists());
        assert!(!database_sidecar_path(&db_path, "-shm").exists());
    }

    #[test]
    fn cleanup_old_backups_ignores_backup_named_directories() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");

        for i in 0..3 {
            let backup_name = format!("test.db.backup.{}", 1000 + i);
            std::fs::write(dir.path().join(&backup_name), format!("backup {i}")).unwrap();
        }
        std::fs::create_dir(dir.path().join("test.db.backup.directory")).unwrap();

        cleanup_old_backups(&db_path, 2).unwrap();

        let mut backup_files = Vec::new();
        let mut backup_dirs = Vec::new();
        for entry in std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("test.db.backup.") {
                continue;
            }
            if entry.path().is_dir() {
                backup_dirs.push(name);
            } else {
                backup_files.push(name);
            }
        }

        assert_eq!(
            backup_files.len(),
            2,
            "only real backup files count toward retention"
        );
        assert_eq!(
            backup_dirs.len(),
            1,
            "backup-named directories should be ignored"
        );
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
        storage.close().unwrap();
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

    #[test]
    fn open_or_rebuild_does_not_treat_non_database_paths_as_corruption() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db_dir");
        std::fs::create_dir(&db_path).unwrap();

        let result = SqliteStorage::open_or_rebuild(&db_path);

        match result {
            Err(MigrationError::Database(_)) | Err(MigrationError::Io(_)) => {}
            Err(MigrationError::RebuildRequired { reason, .. }) => {
                panic!("should not rebuild non-database path: {reason}")
            }
            Err(MigrationError::Other(msg)) => {
                panic!("should preserve underlying open error, got Other: {msg}")
            }
            Ok(_) => panic!("directory path must not open as a database"),
        }

        assert!(
            db_path.is_dir(),
            "non-database directory must be left in place"
        );
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
        fn col_names(conn: &FrankenConnection, table: &str) -> Vec<String> {
            conn.query_map_collect(
                &format!("PRAGMA table_info({})", table),
                fparams![],
                |row: &FrankenRow| row.get_typed(1),
            )
            .unwrap()
        }

        // Helper: collect index names from PRAGMA index_list
        fn idx_names(conn: &FrankenConnection, table: &str) -> Vec<String> {
            conn.query_map_collect(
                &format!("PRAGMA index_list({})", table),
                fparams![],
                |row: &FrankenRow| row.get_typed(1),
            )
            .unwrap()
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
            let conn = FrankenConnection::open(db_path.to_string_lossy().into_owned()).unwrap();
            conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);",
            )
            .unwrap();
            conn.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('schema_version', '10')")
                .unwrap();
            // Apply V1-V10 so schema is correct
            let mut tx = conn.transaction().unwrap();
            tx.execute_batch(MIGRATION_V1).unwrap();
            tx.execute_batch(MIGRATION_V2).unwrap();
            tx.execute_batch(MIGRATION_V4).unwrap();
            tx.execute_batch(MIGRATION_V5).unwrap();
            tx.execute_batch(MIGRATION_V6).unwrap();
            tx.execute_batch(MIGRATION_V7).unwrap();
            tx.execute_batch(MIGRATION_V8).unwrap();
            tx.execute_batch(MIGRATION_V9).unwrap();
            tx.execute_batch(MIGRATION_V10).unwrap();
            tx.execute("UPDATE meta SET value = '10' WHERE key = 'schema_version'")
                .unwrap();
            tx.commit().unwrap();
        }
        materialize_fresh_fts_schema_via_rusqlite(&db_path).unwrap();

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
            .query_row_map(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('message_metrics', 'usage_hourly', 'usage_daily', 'usage_models_daily')",
                &[],
                |row: &FrankenRow| row.get_typed::<i64>(0),
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
        let storage = SqliteStorage::open(&db_path).unwrap();

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
            .query_row_map("SELECT COUNT(*) FROM message_metrics", &[], |row| {
                row.get_typed::<i64>(0)
            })
            .unwrap();
        assert_eq!(mm_count, 3, "Should have 3 message_metrics rows");

        // Verify hour_id and day_id are correct
        #[allow(clippy::type_complexity)]
        let rows: Vec<(i64, i64, String, i64, i64, String, String, String, String)> = conn
            .query_map_collect(
                "SELECT hour_id, day_id, role, content_tokens_est, has_plan, api_data_source, model_family, model_tier, provider FROM message_metrics ORDER BY message_id",
                fparams![],
                |row: &FrankenRow| {
                    Ok((
                        row.get_typed(0)?,
                        row.get_typed(1)?,
                        row.get_typed(2)?,
                        row.get_typed(3)?,
                        row.get_typed(4)?,
                        row.get_typed(5)?,
                        row.get_typed(6)?,
                        row.get_typed(7)?,
                        row.get_typed(8)?,
                    ))
                },
            )
            .unwrap();

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
            .query_row_map(
                "SELECT message_count, user_message_count, assistant_message_count, plan_message_count,
                        plan_content_tokens_est_total, plan_api_tokens_total, api_coverage_message_count
                 FROM usage_hourly WHERE hour_id = ?",
                fparams![expected_hour],
                |row: &FrankenRow| {
                    Ok((
                        row.get_typed(0)?,
                        row.get_typed(1)?,
                        row.get_typed(2)?,
                        row.get_typed(3)?,
                        row.get_typed(4)?,
                        row.get_typed(5)?,
                        row.get_typed(6)?,
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
            .query_row_map(
                "SELECT message_count, api_coverage_message_count FROM usage_daily WHERE day_id = ?",
                fparams![expected_day],
                |row: &FrankenRow| Ok((row.get_typed(0)?, row.get_typed(1)?)),
            )
            .unwrap();
        assert_eq!(ud_msg, 3, "Daily rollup should match hourly");
        assert_eq!(
            ud_api_cov, 1,
            "Daily api_coverage should be 1 (only assistant msg has real API data)"
        );

        // Verify the API input tokens from message_metrics (only API-sourced)
        let api_only_input: i64 = conn
            .query_row_map(
                "SELECT COALESCE(SUM(api_input_tokens), 0) FROM message_metrics WHERE day_id = ? AND api_data_source = 'api'",
                fparams![expected_day],
                |row: &FrankenRow| row.get_typed::<i64>(0),
            )
            .unwrap();
        assert_eq!(
            api_only_input, 100,
            "Only API-sourced input tokens should be 100"
        );

        // Verify rollups match summed message_metrics
        let mm_total_content_est: i64 = conn
            .query_row_map(
                "SELECT SUM(content_tokens_est) FROM message_metrics WHERE day_id = ?",
                fparams![expected_day],
                |row| row.get_typed::<i64>(0),
            )
            .unwrap();
        let mm_plan_content_est: i64 = conn
            .query_row_map(
                "SELECT COALESCE(SUM(content_tokens_est), 0) FROM message_metrics WHERE day_id = ? AND has_plan = 1",
                fparams![expected_day],
                |row: &FrankenRow| row.get_typed::<i64>(0),
            )
            .unwrap();
        let mm_plan_api_total: i64 = conn
            .query_row_map(
                "SELECT COALESCE(SUM(COALESCE(api_input_tokens, 0) + COALESCE(api_output_tokens, 0) + COALESCE(api_cache_read_tokens, 0) + COALESCE(api_cache_creation_tokens, 0) + COALESCE(api_thinking_tokens, 0)), 0)
                 FROM message_metrics WHERE day_id = ? AND has_plan = 1 AND api_data_source = 'api'",
                fparams![expected_day],
                |row: &FrankenRow| row.get_typed::<i64>(0),
            )
            .unwrap();
        let ud_content_est: i64 = conn
            .query_row_map(
                "SELECT content_tokens_est_total FROM usage_daily WHERE day_id = ?",
                fparams![expected_day],
                |row| row.get_typed::<i64>(0),
            )
            .unwrap();
        let (ud_plan_content_est, ud_plan_api_total): (i64, i64) = conn
            .query_row_map(
                "SELECT plan_content_tokens_est_total, plan_api_tokens_total FROM usage_daily WHERE day_id = ?",
                fparams![expected_day],
                |row: &FrankenRow| Ok((row.get_typed(0)?, row.get_typed(1)?)),
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
            .query_row_map(
                "SELECT message_count, user_message_count, assistant_message_count, api_tokens_total, api_coverage_message_count
                 FROM usage_models_daily
                 WHERE day_id = ? AND model_family = 'claude' AND model_tier = 'opus'",
                fparams![expected_day],
                |row: &FrankenRow| Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?, row.get_typed(3)?, row.get_typed(4)?)),
            )
            .unwrap();
        assert_eq!(claude_msg, 1);
        assert_eq!(claude_user, 0);
        assert_eq!(claude_asst, 1);
        assert_eq!(claude_api_total, 380);
        assert_eq!(claude_api_cov, 1);

        let unknown_msg: i64 = conn
            .query_row_map(
                "SELECT message_count FROM usage_models_daily
                 WHERE day_id = ? AND model_family = 'unknown' AND model_tier = 'unknown'",
                fparams![expected_day],
                |row| row.get_typed(0),
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
    fn api_rollups_require_api_data_source() {
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
        let daily = agg.daily.get(&key).expect("daily rollup key must exist");
        let model_key = (
            1_i64,
            "codex".to_string(),
            0_i64,
            "local".to_string(),
            "unknown".to_string(),
            "unknown".to_string(),
        );
        let models_daily = agg
            .models_daily
            .get(&model_key)
            .expect("model rollup key must exist");

        // Content rollup includes both plan messages.
        assert_eq!(hourly.plan_message_count, 2);
        assert_eq!(hourly.plan_content_tokens_est_total, 50);
        // API plan tokens must include only api_data_source='api' rows.
        assert_eq!(hourly.plan_api_tokens_total, 50);
        assert_eq!(daily.plan_api_tokens_total, 50);
        assert_eq!(models_daily.plan_api_tokens_total, 50);
        // Overall API totals must also exclude estimated rows.
        assert_eq!(hourly.api_tokens_total, 50);
        assert_eq!(hourly.api_input_tokens_total, 40);
        assert_eq!(hourly.api_output_tokens_total, 10);
        assert_eq!(hourly.api_coverage_message_count, 1);
        assert_eq!(daily.api_tokens_total, 50);
        assert_eq!(models_daily.api_tokens_total, 50);
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
        let storage = SqliteStorage::open(&db_path).unwrap();

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
            .query_row_map("SELECT COUNT(*) FROM message_metrics", &[], |row| {
                row.get_typed(0)
            })
            .unwrap();
        let orig_hourly: i64 = conn
            .query_row_map("SELECT COUNT(*) FROM usage_hourly", &[], |row| {
                row.get_typed(0)
            })
            .unwrap();
        let orig_daily: i64 = conn
            .query_row_map("SELECT COUNT(*) FROM usage_daily", &[], |row| {
                row.get_typed(0)
            })
            .unwrap();
        let orig_models_daily: i64 = conn
            .query_row_map("SELECT COUNT(*) FROM usage_models_daily", &[], |row| {
                row.get_typed(0)
            })
            .unwrap();
        let orig_api_input: i64 = conn
            .query_row_map(
                "SELECT COALESCE(SUM(api_input_tokens), 0) FROM message_metrics WHERE api_data_source = 'api'",
                &[],
                |row: &FrankenRow| row.get_typed(0),
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
            .query_row_map("SELECT COUNT(*) FROM message_metrics", &[], |row| {
                row.get_typed(0)
            })
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
            .query_row_map("SELECT COUNT(*) FROM message_metrics", &[], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(
            rebuilt_mm, orig_mm,
            "Rebuilt message_metrics count should match"
        );

        let rebuilt_hourly: i64 = conn
            .query_row_map("SELECT COUNT(*) FROM usage_hourly", &[], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(
            rebuilt_hourly, orig_hourly,
            "Rebuilt hourly rows should match"
        );

        let rebuilt_daily: i64 = conn
            .query_row_map("SELECT COUNT(*) FROM usage_daily", &[], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(rebuilt_daily, orig_daily, "Rebuilt daily rows should match");

        let rebuilt_models_daily: i64 = conn
            .query_row_map("SELECT COUNT(*) FROM usage_models_daily", &[], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(
            rebuilt_models_daily, orig_models_daily,
            "Rebuilt model rollup rows should match"
        );

        // Verify API token data preserved through rebuild
        let rebuilt_api_input: i64 = conn
            .query_row_map(
                "SELECT COALESCE(SUM(api_input_tokens), 0) FROM message_metrics WHERE api_data_source = 'api'",
                &[],
                |row: &FrankenRow| row.get_typed(0),
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
            .query_row_map(
                "SELECT message_count, user_message_count, assistant_message_count, plan_message_count,
                        plan_content_tokens_est_total, plan_api_tokens_total
                 FROM usage_hourly WHERE hour_id = ?",
                fparams![expected_hour],
                |row: &FrankenRow| {
                    Ok((
                        row.get_typed(0)?,
                        row.get_typed(1)?,
                        row.get_typed(2)?,
                        row.get_typed(3)?,
                        row.get_typed(4)?,
                        row.get_typed(5)?,
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
            .query_row_map(
                "SELECT message_count FROM usage_daily WHERE day_id = ?",
                fparams![expected_day],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(ud_msg, 3);
    }

    #[test]
    fn insert_conversations_batched_flushes_large_fts_batches() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        // V14 drops fts_messages during migration; cass normally recreates it
        // during startup via `ensure_search_fallback_fts_consistency`. Tests
        // that inspect fts_messages directly need to run the same repair pass
        // to exercise the "insert flushes FTS" contract.
        storage
            .ensure_search_fallback_fts_consistency()
            .expect("ensure FTS consistency before insert");

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let content = "y".repeat((FTS_ENTRY_BATCH_MAX_CHARS / 2) + 1);
        let messages: Vec<_> = (0_i64..2)
            .map(|i| Message {
                id: None,
                idx: i,
                role: MessageRole::Agent,
                author: None,
                created_at: Some(1_700_000_000_000 + i),
                content: format!("{i}-{content}"),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            })
            .collect();
        let conv = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("fts-large-batch".into()),
            title: Some("FTS Large Batch".into()),
            source_path: PathBuf::from("/tmp/rollout.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_999),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages,
            source_id: "local".into(),
            origin_host: None,
        };

        let outcomes = storage
            .insert_conversations_batched(&[(agent_id, None, &conv)])
            .unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].inserted_indices.len(), conv.messages.len());

        let message_count: i64 = storage
            .conn
            .query_row_map("SELECT COUNT(*) FROM messages", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        let fts_count: i64 = storage
            .conn
            .query_row_map("SELECT COUNT(*) FROM fts_messages", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();

        assert_eq!(message_count, conv.messages.len() as i64);
        assert_eq!(fts_count, conv.messages.len() as i64);
    }

    fn make_profiled_storage_remote_conversation(
        external_id: i64,
        msg_count: usize,
    ) -> Conversation {
        Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/ws/profiled-storage-remote")),
            external_id: Some(format!("profiled-storage-remote-{external_id}")),
            title: Some(format!(
                "Profiled storage remote conversation {external_id}"
            )),
            source_path: PathBuf::from(format!("/log/profiled-storage-remote-{external_id}.jsonl")),
            started_at: Some(10_000 + external_id * 100),
            ended_at: Some(10_000 + external_id * 100 + msg_count as i64),
            approx_tokens: Some(msg_count as i64 * 32),
            metadata_json: serde_json::json!({ "bench": true }),
            messages: (0..msg_count)
                .map(|idx| Message {
                    id: None,
                    idx: idx as i64,
                    role: if idx % 2 == 0 {
                        MessageRole::User
                    } else {
                        MessageRole::Agent
                    },
                    author: Some("tester".into()),
                    created_at: Some(20_000 + external_id * 100 + idx as i64),
                    content: format!(
                        "profiled storage remote content ext={external_id} idx={idx} {}",
                        "x".repeat(64)
                    ),
                    extra_json: serde_json::json!({ "idx": idx }),
                    snippets: Vec::new(),
                })
                .collect(),
            source_id: "profiled-storage-remote-source".into(),
            origin_host: Some("builder-profile".into()),
        }
    }

    fn make_profiled_append_remote_merge_conversation(
        external_id: i64,
        msg_count: usize,
    ) -> Conversation {
        let base_ts = 100_000 + external_id * 1_000;
        Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/ws/profiled-append-remote")),
            external_id: Some(format!("profiled-append-remote-{external_id}")),
            title: Some(format!("Profiled append remote conversation {external_id}")),
            source_path: PathBuf::from(format!("/log/profiled-append-remote-{external_id}.jsonl")),
            started_at: Some(base_ts),
            ended_at: Some(base_ts + msg_count as i64),
            approx_tokens: Some(msg_count as i64 * 50),
            metadata_json: serde_json::json!({ "bench": true }),
            messages: (0..msg_count)
                .map(|idx| Message {
                    id: None,
                    idx: idx as i64,
                    role: if idx % 2 == 0 {
                        MessageRole::User
                    } else {
                        MessageRole::Agent
                    },
                    author: Some(format!("model-{}", external_id % 5)),
                    created_at: Some(base_ts + idx as i64),
                    content: format!(
                        "Profiled append remote conversation {} message {}: Lorem ipsum dolor sit amet, consectetur adipiscing elit.",
                        external_id, idx
                    ),
                    extra_json: serde_json::json!({ "bench": true }),
                    snippets: Vec::new(),
                })
                .collect(),
            source_id: "profiled-append-remote-source".into(),
            origin_host: Some("builder-profile".into()),
        }
    }

    #[test]
    fn insert_conversation_tree_batched_new_message_ids_match_snippet_rows() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("batched-message-ids.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let agent_id = storage
            .ensure_agent(&Agent {
                id: None,
                slug: "codex".into(),
                name: "Codex".into(),
                version: None,
                kind: AgentKind::Cli,
            })
            .unwrap();
        let workspace_id = storage
            .ensure_workspace(&PathBuf::from("/ws/profiled-storage-remote"), None)
            .unwrap();
        let mut conv = make_profiled_storage_remote_conversation(42, 5);
        for (idx, msg) in conv.messages.iter_mut().enumerate() {
            msg.snippets.push(Snippet {
                id: None,
                file_path: Some(PathBuf::from(format!("src/file_{idx}.rs"))),
                start_line: Some((idx + 1) as i64),
                end_line: Some((idx + 2) as i64),
                language: Some("rust".into()),
                snippet_text: Some(format!("fn snippet_{idx}() {{}}")),
            });
        }
        let outcome = storage
            .insert_conversation_tree(agent_id, Some(workspace_id), &conv)
            .unwrap();

        let message_count: i64 = storage
            .conn
            .query_row_map(
                "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1",
                fparams![outcome.conversation_id],
                |row| row.get_typed(0),
            )
            .unwrap();
        let joined_snippet_count: i64 = storage
            .conn
            .query_row_map(
                "SELECT COUNT(*)
                 FROM snippets s
                 JOIN messages m ON s.message_id = m.id
                 WHERE m.conversation_id = ?1",
                fparams![outcome.conversation_id],
                |row| row.get_typed(0),
            )
            .unwrap();

        assert_eq!(message_count, conv.messages.len() as i64);
        assert_eq!(joined_snippet_count, conv.messages.len() as i64);
    }

    #[test]
    fn insert_conversation_tree_batched_appended_message_ids_match_snippet_rows() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("batched-append-message-ids.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let agent_id = storage
            .ensure_agent(&Agent {
                id: None,
                slug: "codex".into(),
                name: "Codex".into(),
                version: None,
                kind: AgentKind::Cli,
            })
            .unwrap();
        let workspace_id = storage
            .ensure_workspace(&PathBuf::from("/ws/profiled-storage-remote"), None)
            .unwrap();

        let mut initial = make_profiled_storage_remote_conversation(77, 2);
        for (idx, msg) in initial.messages.iter_mut().enumerate() {
            msg.snippets.push(Snippet {
                id: None,
                file_path: Some(PathBuf::from(format!("src/append_initial_{idx}.rs"))),
                start_line: Some((idx + 1) as i64),
                end_line: Some((idx + 2) as i64),
                language: Some("rust".into()),
                snippet_text: Some(format!("fn append_initial_{idx}() {{}}")),
            });
        }
        let first = storage
            .insert_conversation_tree(agent_id, Some(workspace_id), &initial)
            .unwrap();
        assert_eq!(first.inserted_indices, vec![0, 1]);

        let mut appended = make_profiled_storage_remote_conversation(77, 5);
        for (idx, msg) in appended.messages.iter_mut().enumerate() {
            msg.snippets.push(Snippet {
                id: None,
                file_path: Some(PathBuf::from(format!("src/append_full_{idx}.rs"))),
                start_line: Some((idx + 10) as i64),
                end_line: Some((idx + 11) as i64),
                language: Some("rust".into()),
                snippet_text: Some(format!("fn append_full_{idx}() {{}}")),
            });
        }
        let second = storage
            .insert_conversation_tree(agent_id, Some(workspace_id), &appended)
            .unwrap();
        assert_eq!(second.conversation_id, first.conversation_id);
        assert_eq!(second.inserted_indices, vec![2, 3, 4]);

        let message_count: i64 = storage
            .conn
            .query_row_map(
                "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1",
                fparams![first.conversation_id],
                |row| row.get_typed(0),
            )
            .unwrap();
        let joined_snippets: Vec<(i64, String)> = storage
            .conn
            .query_map_collect(
                "SELECT m.idx, s.file_path
                 FROM snippets s
                 JOIN messages m ON s.message_id = m.id
                 WHERE m.conversation_id = ?1
                 ORDER BY m.idx, s.id",
                fparams![first.conversation_id],
                |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
            )
            .unwrap();

        assert_eq!(message_count, 5);
        assert_eq!(
            joined_snippets,
            vec![
                (0, "src/append_initial_0.rs".to_string()),
                (1, "src/append_initial_1.rs".to_string()),
                (2, "src/append_full_2.rs".to_string()),
                (3, "src/append_full_3.rs".to_string()),
                (4, "src/append_full_4.rs".to_string()),
            ]
        );
    }

    #[test]
    fn insert_conversation_tree_recreates_daily_stats_after_manual_clear() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let agent_id = storage
            .ensure_agent(&Agent {
                id: None,
                slug: "codex".into(),
                name: "Codex".into(),
                version: None,
                kind: AgentKind::Cli,
            })
            .unwrap();
        let workspace = PathBuf::from("/ws/profiled-storage-remote");
        let workspace_id = storage.ensure_workspace(&workspace, None).unwrap();

        storage
            .insert_conversation_tree(
                agent_id,
                Some(workspace_id),
                &make_profiled_storage_remote_conversation(0, 3),
            )
            .unwrap();
        storage.conn.execute("DELETE FROM daily_stats").unwrap();

        storage
            .insert_conversation_tree(
                agent_id,
                Some(workspace_id),
                &make_profiled_storage_remote_conversation(1, 2),
            )
            .unwrap();

        let row_count: i64 = storage
            .conn
            .query_row_map("SELECT COUNT(*) FROM daily_stats", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        let (session_count, message_count): (i64, i64) = storage
            .conn
            .query_row_map(
                "SELECT session_count, message_count
                 FROM daily_stats
                 WHERE agent_slug = 'all' AND source_id = 'all'",
                fparams![],
                |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
            )
            .unwrap();

        assert_eq!(row_count, 4);
        assert_eq!(session_count, 1);
        assert_eq!(message_count, 2);
    }

    #[test]
    #[serial]
    fn insert_conversation_tree_stage_profile_tracks_steady_state_remote_reuse() {
        let _defer_guard = set_env_var("CASS_DEFER_LEXICAL_UPDATES", "0");

        for &(msg_count, iterations) in &[(5usize, 80usize), (20, 50), (50, 24)] {
            let dir = TempDir::new().unwrap();
            let db_path = dir.path().join(format!("profile-{msg_count}.db"));
            let storage = SqliteStorage::open(&db_path).unwrap();
            let agent_id = storage
                .ensure_agent(&Agent {
                    id: None,
                    slug: "codex".into(),
                    name: "Codex".into(),
                    version: None,
                    kind: AgentKind::Cli,
                })
                .unwrap();
            let workspace = PathBuf::from("/ws/profiled-storage-remote");
            let workspace_id = storage.ensure_workspace(&workspace, None).unwrap();

            storage
                .insert_conversation_tree(
                    agent_id,
                    Some(workspace_id),
                    &make_profiled_storage_remote_conversation(0, msg_count),
                )
                .unwrap();

            let mut profile = InsertConversationTreePerfProfile::default();
            for external_id in 1..=iterations {
                storage
                    .insert_conversation_tree_with_profile(
                        agent_id,
                        Some(workspace_id),
                        &make_profiled_storage_remote_conversation(external_id as i64, msg_count),
                        &mut profile,
                    )
                    .unwrap();
            }

            let accounted_duration = profile.source_duration
                + profile.tx_open_duration
                + profile.existing_lookup_duration
                + profile.conversation_row_duration
                + profile.message_insert_duration
                + profile.snippet_insert_duration
                + profile.fts_entry_duration
                + profile.fts_flush_duration
                + profile.analytics_duration
                + profile.commit_duration;
            assert_eq!(profile.invocations, iterations);
            assert_eq!(profile.messages, iterations * msg_count);
            assert_eq!(profile.inserted_messages, iterations * msg_count);
            assert!(
                profile.total_duration >= accounted_duration,
                "accounted stage durations cannot exceed total duration"
            );

            profile.log_summary(&format!("remote_reuse_{msg_count}_msgs"));
        }
    }

    #[test]
    #[serial]
    fn insert_conversation_tree_stage_profile_tracks_append_remote_source_merge() {
        let _defer_guard = set_env_var("CASS_DEFER_LEXICAL_UPDATES", "0");

        for &(msg_count, iterations) in &[(5usize, 80usize), (20, 50), (50, 24)] {
            let dir = TempDir::new().unwrap();
            let db_path = dir.path().join(format!("append-profile-{msg_count}.db"));
            let storage = SqliteStorage::open(&db_path).unwrap();
            let agent_id = storage
                .ensure_agent(&Agent {
                    id: None,
                    slug: "codex".into(),
                    name: "Codex".into(),
                    version: None,
                    kind: AgentKind::Cli,
                })
                .unwrap();
            let workspace = PathBuf::from("/ws/profiled-append-remote");
            let workspace_id = storage.ensure_workspace(&workspace, None).unwrap();

            for external_id in 0..iterations {
                storage
                    .insert_conversation_tree(
                        agent_id,
                        Some(workspace_id),
                        &make_profiled_append_remote_merge_conversation(
                            external_id as i64,
                            msg_count,
                        ),
                    )
                    .unwrap();
            }

            let mut profile = InsertConversationTreePerfProfile::default();
            for external_id in 0..iterations {
                storage
                    .append_existing_conversation_with_profile(
                        agent_id,
                        Some(workspace_id),
                        &make_profiled_append_remote_merge_conversation(
                            external_id as i64,
                            msg_count * 2,
                        ),
                        &mut profile,
                    )
                    .unwrap();
            }

            let accounted_duration = profile.source_duration
                + profile.tx_open_duration
                + profile.existing_lookup_duration
                + profile.existing_idx_lookup_duration
                + profile.existing_replay_lookup_duration
                + profile.dedupe_filter_duration
                + profile.conversation_row_duration
                + profile.message_insert_duration
                + profile.snippet_insert_duration
                + profile.fts_entry_duration
                + profile.fts_flush_duration
                + profile.analytics_duration
                + profile.commit_duration;
            assert_eq!(profile.invocations, iterations);
            assert_eq!(profile.messages, iterations * msg_count * 2);
            assert_eq!(profile.inserted_messages, iterations * msg_count);
            assert!(
                profile.total_duration >= accounted_duration,
                "accounted append stage durations cannot exceed total duration"
            );

            profile.log_summary(&format!("append_remote_merge_{msg_count}_msgs"));
        }
    }

    #[test]
    fn rebuild_daily_stats_recomputes_materialized_totals_without_monolithic_group_by() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let started_at = 1_700_000_000_000_i64;
        let day_id = FrankenStorage::day_id_from_millis(started_at);
        let hour_id = FrankenStorage::hour_id_from_millis(started_at);

        storage
            .conn
            .execute_compat(
                "INSERT INTO agents (id, slug, name, version, kind, created_at, updated_at)
                 VALUES (?1, ?2, ?3, NULL, ?4, 0, 0)",
                fparams![1_i64, "codex", "Codex", "cli"],
            )
            .unwrap();
        storage
            .conn
            .execute_compat(
                "INSERT INTO agents (id, slug, name, version, kind, created_at, updated_at)
                 VALUES (?1, ?2, ?3, NULL, ?4, 0, 0)",
                fparams![2_i64, "claude", "Claude", "cli"],
            )
            .unwrap();

        storage
            .conn
            .execute_compat(
                "INSERT INTO conversations (
                    id, agent_id, workspace_id, source_id, external_id, title, source_path,
                    started_at, ended_at, approx_tokens, metadata_json, origin_host, metadata_bin
                 ) VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, NULL, NULL)",
                fparams![
                    1_i64,
                    1_i64,
                    LOCAL_SOURCE_ID,
                    "daily-a",
                    "Daily A",
                    "/tmp/daily-a.jsonl",
                    started_at,
                    started_at + 200,
                    "{}"
                ],
            )
            .unwrap();
        storage
            .conn
            .execute_compat(
                "INSERT INTO conversations (
                    id, agent_id, workspace_id, source_id, external_id, title, source_path,
                    started_at, ended_at, approx_tokens, metadata_json, origin_host, metadata_bin
                 ) VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, NULL, NULL)",
                fparams![
                    2_i64,
                    2_i64,
                    LOCAL_SOURCE_ID,
                    "daily-b",
                    "Daily B",
                    "/tmp/daily-b.jsonl",
                    started_at,
                    started_at + 300,
                    "{}"
                ],
            )
            .unwrap();

        storage
            .conn
            .execute_compat(
                "INSERT INTO messages (
                    id, conversation_id, idx, role, author, created_at, content, extra_json, extra_bin
                 ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, NULL, NULL)",
                fparams![1_i64, 1_i64, 0_i64, "user", started_at, "hello"],
            )
            .unwrap();
        storage
            .conn
            .execute_compat(
                "INSERT INTO messages (
                    id, conversation_id, idx, role, author, created_at, content, extra_json, extra_bin
                 ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, NULL, NULL)",
                fparams![2_i64, 1_i64, 1_i64, "assistant", started_at + 100, "response"],
            )
            .unwrap();
        storage
            .conn
            .execute_compat(
                "INSERT INTO messages (
                    id, conversation_id, idx, role, author, created_at, content, extra_json, extra_bin
                 ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, NULL, NULL)",
                fparams![3_i64, 2_i64, 0_i64, "user", started_at + 50, "abc"],
            )
            .unwrap();

        for (message_id, agent_slug, role, content_len) in [
            (1_i64, "codex", "user", 5_i64),
            (2_i64, "codex", "assistant", 8_i64),
            (3_i64, "claude", "user", 3_i64),
        ] {
            storage
                .conn
                .execute_compat(
                    "INSERT INTO message_metrics (
                        message_id, created_at_ms, hour_id, day_id, agent_slug, workspace_id, source_id,
                        role, content_chars, content_tokens_est, api_input_tokens, api_output_tokens,
                        api_cache_read_tokens, api_cache_creation_tokens, api_thinking_tokens,
                        api_service_tier, api_data_source, tool_call_count, has_tool_calls, has_plan,
                        model_name, model_family, model_tier, provider
                     ) VALUES (
                        ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                        ?8, ?9, ?10, ?11, ?12,
                        ?13, ?14, ?15,
                        ?16, ?17, ?18, ?19, ?20,
                        ?21, ?22, ?23, ?24
                     )",
                    fparams![
                        message_id,
                        started_at,
                        hour_id,
                        day_id,
                        agent_slug,
                        0_i64,
                        LOCAL_SOURCE_ID,
                        role,
                        content_len,
                        content_len / 4,
                        0_i64,
                        0_i64,
                        0_i64,
                        0_i64,
                        0_i64,
                        "",
                        "estimated",
                        0_i64,
                        0_i64,
                        0_i64,
                        "",
                        "unknown",
                        "unknown",
                        "unknown"
                    ],
                )
                .unwrap();
        }

        storage.conn.execute("DELETE FROM daily_stats").unwrap();

        let rebuilt = storage.rebuild_daily_stats().unwrap();
        assert_eq!(rebuilt.total_sessions, 2);

        let health = storage.daily_stats_health().unwrap();
        assert_eq!(health.conversation_count, 2);
        assert_eq!(health.materialized_total, 2);
        assert_eq!(health.drift, 0);

        let total_messages: i64 = storage
            .conn
            .query_row_map(
                "SELECT message_count FROM daily_stats WHERE agent_slug = 'all' AND source_id = 'all'",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(total_messages, 3);
    }

    #[test]
    fn rebuild_daily_stats_preserves_byte_counts_with_message_metrics() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let content = "ASCII🙂é漢字";
        let expected_bytes = content.len() as i64;
        let started_at = 1_704_067_200_000_i64;
        let day_id = FrankenStorage::day_id_from_millis(started_at);
        let hour_id = FrankenStorage::hour_id_from_millis(started_at);

        storage
            .conn
            .execute_compat(
                "INSERT INTO agents (id, slug, name, version, kind, created_at, updated_at)
                 VALUES (?1, ?2, ?3, NULL, ?4, 0, 0)",
                fparams![1_i64, "tester", "Tester", "cli"],
            )
            .unwrap();
        storage
            .conn
            .execute_compat(
                "INSERT INTO conversations (
                    id, agent_id, workspace_id, source_id, external_id, title, source_path,
                    started_at, ended_at, approx_tokens, metadata_json, origin_host, metadata_bin
                 ) VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, NULL, NULL, ?8, NULL, NULL)",
                fparams![
                    1_i64,
                    1_i64,
                    LOCAL_SOURCE_ID,
                    "unicode-metrics",
                    "Unicode Metrics",
                    "/tmp/unicode-metrics.jsonl",
                    started_at,
                    "{}"
                ],
            )
            .unwrap();
        storage
            .conn
            .execute_compat(
                "INSERT INTO messages (
                    id, conversation_id, idx, role, author, created_at, content, extra_json, extra_bin
                 ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, NULL, NULL)",
                fparams![1_i64, 1_i64, 0_i64, "user", started_at, content],
            )
            .unwrap();
        storage
            .conn
            .execute_compat(
                "INSERT INTO message_metrics (
                    message_id, created_at_ms, hour_id, day_id, agent_slug, workspace_id, source_id,
                    role, content_chars, content_tokens_est, api_input_tokens, api_output_tokens,
                    api_cache_read_tokens, api_cache_creation_tokens, api_thinking_tokens,
                    api_service_tier, api_data_source, tool_call_count, has_tool_calls, has_plan,
                    model_name, model_family, model_tier, provider
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                    ?8, ?9, ?10, ?11, ?12,
                    ?13, ?14, ?15,
                    ?16, ?17, ?18, ?19, ?20,
                    ?21, ?22, ?23, ?24
                 )",
                fparams![
                    1_i64,
                    started_at,
                    hour_id,
                    day_id,
                    "tester",
                    0_i64,
                    LOCAL_SOURCE_ID,
                    "user",
                    expected_bytes,
                    expected_bytes / 4,
                    0_i64,
                    0_i64,
                    0_i64,
                    0_i64,
                    0_i64,
                    "",
                    "estimated",
                    0_i64,
                    0_i64,
                    0_i64,
                    "",
                    "unknown",
                    "unknown",
                    "unknown"
                ],
            )
            .unwrap();

        let mut tx = storage.conn.transaction().unwrap();
        franken_update_daily_stats_in_tx(
            &storage,
            &tx,
            "tester",
            LOCAL_SOURCE_ID,
            Some(started_at),
            StatsDelta {
                session_count_delta: 1,
                message_count_delta: 1,
                total_chars_delta: expected_bytes,
            },
        )
        .unwrap();
        tx.commit().unwrap();

        let inline_total: i64 = storage
            .conn
            .query_row_map(
                "SELECT total_chars FROM daily_stats WHERE agent_slug = 'all' AND source_id = 'all'",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(inline_total, expected_bytes);

        storage.conn.execute("DELETE FROM daily_stats").unwrap();

        let rebuilt = storage.rebuild_daily_stats().unwrap();
        assert_eq!(rebuilt.total_sessions, 1);

        let rebuilt_total: i64 = storage
            .conn
            .query_row_map(
                "SELECT total_chars FROM daily_stats WHERE agent_slug = 'all' AND source_id = 'all'",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(rebuilt_total, expected_bytes);
    }

    #[test]
    fn rebuild_daily_stats_raw_fallback_preserves_byte_counts() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let content = "fallback🙂é漢字";
        let expected_bytes = content.len() as i64;
        let started_at = 1_704_067_200_000_i64;
        storage
            .conn
            .execute_compat(
                "INSERT INTO agents (id, slug, name, version, kind, created_at, updated_at)
                 VALUES (?1, ?2, ?3, NULL, ?4, 0, 0)",
                fparams![1_i64, "tester", "Tester", "cli"],
            )
            .unwrap();
        storage
            .conn
            .execute_compat(
                "INSERT INTO conversations (
                    id, agent_id, workspace_id, source_id, external_id, title, source_path,
                    started_at, ended_at, approx_tokens, metadata_json, origin_host, metadata_bin
                 ) VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, NULL, NULL, ?8, NULL, NULL)",
                fparams![
                    1_i64,
                    1_i64,
                    LOCAL_SOURCE_ID,
                    "unicode-fallback",
                    "Unicode Fallback",
                    "/tmp/unicode-fallback.jsonl",
                    started_at,
                    "{}"
                ],
            )
            .unwrap();
        storage
            .conn
            .execute_compat(
                "INSERT INTO messages (
                    id, conversation_id, idx, role, author, created_at, content, extra_json, extra_bin
                 ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, NULL, NULL)",
                fparams![1_i64, 1_i64, 0_i64, "assistant", started_at, content],
            )
            .unwrap();

        let mut tx = storage.conn.transaction().unwrap();
        franken_update_daily_stats_in_tx(
            &storage,
            &tx,
            "tester",
            LOCAL_SOURCE_ID,
            Some(started_at),
            StatsDelta {
                session_count_delta: 1,
                message_count_delta: 1,
                total_chars_delta: expected_bytes,
            },
        )
        .unwrap();
        tx.commit().unwrap();

        storage.conn.execute("DELETE FROM daily_stats").unwrap();

        let rebuilt = storage.rebuild_daily_stats().unwrap();
        assert_eq!(rebuilt.total_sessions, 1);

        let rebuilt_total: i64 = storage
            .conn
            .query_row_map(
                "SELECT total_chars FROM daily_stats WHERE agent_slug = 'all' AND source_id = 'all'",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(rebuilt_total, expected_bytes);
    }

    #[test]
    fn insert_conversations_batched_appends_duplicate_external_id() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let base_conv = |messages: Vec<Message>| Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("shared-session".into()),
            title: Some("Shared Session".into()),
            source_path: PathBuf::from("/tmp/rollout.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_999),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages,
            source_id: "local".into(),
            origin_host: None,
        };

        let conv_a = base_conv(vec![
            Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_000),
                content: "first".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            },
            Message {
                id: None,
                idx: 1,
                role: MessageRole::Agent,
                author: None,
                created_at: Some(1_700_000_000_100),
                content: "second".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            },
        ]);
        let conv_b = base_conv(vec![
            Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_000),
                content: "first".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            },
            Message {
                id: None,
                idx: 1,
                role: MessageRole::Agent,
                author: None,
                created_at: Some(1_700_000_000_100),
                content: "second".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            },
            Message {
                id: None,
                idx: 2,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_200),
                content: "third".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            },
            Message {
                id: None,
                idx: 3,
                role: MessageRole::Agent,
                author: None,
                created_at: Some(1_700_000_000_300),
                content: "fourth".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            },
        ]);

        let outcomes = storage
            .insert_conversations_batched(&[(agent_id, None, &conv_a), (agent_id, None, &conv_b)])
            .unwrap();
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].inserted_indices, vec![0, 1]);
        assert_eq!(outcomes[1].inserted_indices, vec![2, 3]);
        assert_eq!(outcomes[0].conversation_id, outcomes[1].conversation_id);

        let conversation_count: i64 = storage
            .conn
            .query_row_map("SELECT COUNT(*) FROM conversations", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        let conversation_count_not_indexed: i64 = storage
            .conn
            .query_row_map(
                "SELECT COUNT(*) FROM conversations NOT INDEXED",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        let conversation_count_source_index: i64 = storage
            .conn
            .query_row_map(
                "SELECT COUNT(*) FROM conversations INDEXED BY idx_conversations_source_id",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        let message_count: i64 = storage
            .conn
            .query_row_map("SELECT COUNT(*) FROM messages", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        let reopened_storage = SqliteStorage::open(&db_path).unwrap();
        let reopened_conversation_count: i64 = reopened_storage
            .conn
            .query_row_map("SELECT COUNT(*) FROM conversations", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        let reopened_conversation_count_not_indexed: i64 = reopened_storage
            .conn
            .query_row_map(
                "SELECT COUNT(*) FROM conversations NOT INDEXED",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        let reopened_conversation_ids: Vec<i64> = reopened_storage
            .conn
            .query_map_collect(
                "SELECT id FROM conversations ORDER BY id",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        let reopened_conversation_ids_not_indexed: Vec<i64> = reopened_storage
            .conn
            .query_map_collect(
                "SELECT id FROM conversations NOT INDEXED ORDER BY id",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        let reopened_conversation_ids_source_index: Vec<i64> = reopened_storage
            .conn
            .query_map_collect(
                "SELECT id FROM conversations INDEXED BY idx_conversations_source_id ORDER BY id",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();

        assert_eq!(reopened_conversation_ids, vec![outcomes[0].conversation_id]);
        assert_eq!(
            reopened_conversation_ids_not_indexed,
            vec![outcomes[0].conversation_id]
        );
        assert_eq!(
            reopened_conversation_ids_source_index,
            vec![outcomes[0].conversation_id]
        );
        assert_eq!(reopened_conversation_count, 1);
        assert_eq!(reopened_conversation_count_not_indexed, 1);
        assert_eq!(conversation_count_not_indexed, 1);
        assert_eq!(conversation_count_source_index, 1);
        assert_eq!(conversation_count, 1);
        assert_eq!(message_count, 4);
    }

    #[test]
    fn franken_insert_conversation_or_get_existing_recovers_unique_conflict() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let conv = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("recover-duplicate".into()),
            title: Some("Recover Duplicate".into()),
            source_path: PathBuf::from("/tmp/rollout.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_000),
                content: "hello".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: "local".into(),
            origin_host: None,
        };

        let tx = storage.conn.transaction().unwrap();
        let inserted_id = franken_insert_conversation(&tx, agent_id, None, &conv)
            .unwrap()
            .expect("first insert should succeed");

        let conversation_key = conversation_merge_key(agent_id, &conv);
        let resolved = franken_insert_conversation_or_get_existing_after_miss(
            &tx,
            agent_id,
            None,
            &conv,
            &conversation_key,
        )
        .unwrap();

        match resolved {
            ConversationInsertStatus::Existing(existing_id) => {
                assert_eq!(existing_id, inserted_id);
            }
            ConversationInsertStatus::Inserted(new_id) => {
                panic!("expected existing conversation id, got freshly inserted {new_id}");
            }
        }

        let conversation_count: i64 = tx
            .query_row_map("SELECT COUNT(*) FROM conversations", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(conversation_count, 1);
    }

    #[test]
    fn insert_conversations_batched_merges_duplicate_external_id_with_gaps() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let base_conv = |messages: Vec<Message>| Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("shared-session-gap".into()),
            title: Some("Shared Session Gap".into()),
            source_path: PathBuf::from("/tmp/rollout.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_999),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages,
            source_id: "local".into(),
            origin_host: None,
        };

        let conv_a = base_conv(vec![
            Message {
                id: None,
                idx: 2,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_200),
                content: "third".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            },
            Message {
                id: None,
                idx: 3,
                role: MessageRole::Agent,
                author: None,
                created_at: Some(1_700_000_000_300),
                content: "fourth".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            },
        ]);
        let conv_b = base_conv(vec![
            Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_000),
                content: "first".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            },
            Message {
                id: None,
                idx: 1,
                role: MessageRole::Agent,
                author: None,
                created_at: Some(1_700_000_000_100),
                content: "second".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            },
            Message {
                id: None,
                idx: 3,
                role: MessageRole::Agent,
                author: None,
                created_at: Some(1_700_000_000_300),
                content: "fourth".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            },
        ]);

        let outcomes = storage
            .insert_conversations_batched(&[(agent_id, None, &conv_a), (agent_id, None, &conv_b)])
            .unwrap();
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].inserted_indices, vec![2, 3]);
        assert_eq!(outcomes[1].inserted_indices, vec![0, 1]);
        assert_eq!(outcomes[0].conversation_id, outcomes[1].conversation_id);

        let stored_indices: Vec<i64> = storage
            .conn
            .query_map_collect("SELECT idx FROM messages ORDER BY idx", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(stored_indices, vec![0, 1, 2, 3]);
    }

    #[test]
    fn insert_conversations_batched_reprocessing_conversation_is_idempotent() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        const MESSAGE_COUNT: i64 = 64;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let messages: Vec<Message> = (0..MESSAGE_COUNT)
            .map(|idx| Message {
                id: None,
                idx,
                role: if idx % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Agent
                },
                author: None,
                created_at: Some(1_700_000_000_000 + idx),
                content: format!("message {idx}"),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            })
            .collect();

        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("large-reprocess-session".into()),
            title: Some("Large Reprocess Session".into()),
            source_path: PathBuf::from("/tmp/large-reprocess-session.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_000 + MESSAGE_COUNT - 1),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages,
            source_id: "local".into(),
            origin_host: None,
        };

        let first = storage
            .insert_conversations_batched(&[(agent_id, None, &conversation)])
            .unwrap();
        let second = storage
            .insert_conversations_batched(&[(agent_id, None, &conversation)])
            .unwrap();

        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].inserted_indices.len(), MESSAGE_COUNT as usize);
        assert!(
            second[0].inserted_indices.is_empty(),
            "full reprocessing of a large conversation must not attempt duplicate idx inserts"
        );
        assert_eq!(first[0].conversation_id, second[0].conversation_id);

        let conversation_count: i64 = storage
            .conn
            .query_row_map("SELECT COUNT(*) FROM conversations", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        let message_count: i64 = storage
            .conn
            .query_row_map("SELECT COUNT(*) FROM messages", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();

        assert_eq!(conversation_count, 1);
        assert_eq!(message_count, MESSAGE_COUNT);
    }

    #[test]
    fn parallel_insert_conversation_tree_keeps_unique_external_ids_distinct() {
        use crate::connectors::{NormalizedConversation, NormalizedMessage};
        use crate::indexer::persist::map_to_internal;
        use crate::model::types::{Agent, AgentKind};
        use frankensqlite::compat::{ConnectionExt, RowExt};
        use rand::RngExt;
        use rayon::prelude::*;

        fn retryable_franken_error(err: &anyhow::Error) -> bool {
            err.downcast_ref::<frankensqlite::FrankenError>()
                .or_else(|| {
                    err.root_cause()
                        .downcast_ref::<frankensqlite::FrankenError>()
                })
                .is_some_and(|inner| {
                    matches!(
                        inner,
                        frankensqlite::FrankenError::Busy
                            | frankensqlite::FrankenError::BusyRecovery
                            | frankensqlite::FrankenError::BusySnapshot { .. }
                            | frankensqlite::FrankenError::WriteConflict { .. }
                            | frankensqlite::FrankenError::SerializationFailure { .. }
                    )
                })
        }

        fn with_retry<F, T>(mut f: F) -> anyhow::Result<T>
        where
            F: FnMut() -> anyhow::Result<T>,
        {
            let mut rng = rand::rng();
            let mut backoff_ms = 4_u64;
            for attempt in 0..=24 {
                match f() {
                    Ok(value) => return Ok(value),
                    Err(err) if attempt < 24 && retryable_franken_error(&err) => {
                        let sleep_ms = backoff_ms + rng.random_range(0..=backoff_ms);
                        std::thread::sleep(Duration::from_millis(sleep_ms));
                        backoff_ms = (backoff_ms * 2).min(512);
                    }
                    Err(err) => return Err(err),
                }
            }
            unreachable!("retry loop must return on success or final failure")
        }

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("parallel_insert_conversation_tree.db");
        let seed = FrankenStorage::open(&db_path).unwrap();
        drop(seed);

        let conversations: Vec<NormalizedConversation> = (0..10)
            .map(|i| NormalizedConversation {
                agent_slug: format!("agent-{}", i % 3),
                external_id: Some(format!("conv-{i}")),
                title: Some(format!("Conversation {i}")),
                workspace: Some(PathBuf::from(format!("/ws/{i}"))),
                source_path: PathBuf::from(format!("/log/{i}.jsonl")),
                started_at: Some(1_000 + i * 100),
                ended_at: Some(1_000 + i * 100 + 50),
                metadata: serde_json::json!({}),
                messages: (0..3)
                    .map(|j| NormalizedMessage {
                        idx: j,
                        role: if j % 2 == 0 { "user" } else { "assistant" }.to_string(),
                        author: Some("tester".into()),
                        created_at: Some(1_000 + i * 100 + j * 10),
                        content: format!("parallel-distinct-test conv={i} msg={j}"),
                        extra: serde_json::json!({}),
                        snippets: vec![],
                        invocations: Vec::new(),
                    })
                    .collect(),
            })
            .collect();

        let mut outcomes: Vec<(String, i64, Vec<i64>)> = conversations
            .par_chunks(3)
            .map(|chunk| {
                let storage = FrankenStorage::open_writer(&db_path).unwrap();
                let mut agent_cache: HashMap<String, i64> = HashMap::new();
                let mut workspace_cache: HashMap<PathBuf, i64> = HashMap::new();
                let mut chunk_outcomes = Vec::with_capacity(chunk.len());

                for conv in chunk {
                    let agent_slug = conv.agent_slug.clone();
                    let workspace = conv.workspace.clone();
                    let external_id = conv.external_id.clone().expect("external id");
                    let internal = map_to_internal(conv);
                    let outcome = with_retry(|| {
                        let agent_id = if let Some(id) = agent_cache.get(&agent_slug) {
                            *id
                        } else {
                            let agent = Agent {
                                id: None,
                                slug: agent_slug.clone(),
                                name: agent_slug.clone(),
                                version: None,
                                kind: AgentKind::Cli,
                            };
                            let id = storage.ensure_agent(&agent)?;
                            agent_cache.insert(agent_slug.clone(), id);
                            id
                        };
                        let workspace_id = if let Some(path) = &workspace {
                            if let Some(id) = workspace_cache.get(path) {
                                Some(*id)
                            } else {
                                let id = storage.ensure_workspace(path, None)?;
                                workspace_cache.insert(path.clone(), id);
                                Some(id)
                            }
                        } else {
                            None
                        };
                        storage.insert_conversation_tree(agent_id, workspace_id, &internal)
                    })
                    .unwrap();
                    chunk_outcomes.push((
                        external_id,
                        outcome.conversation_id,
                        outcome.inserted_indices,
                    ));
                }

                storage.close().unwrap();
                chunk_outcomes
            })
            .flatten()
            .collect();
        outcomes.sort_by(|left, right| left.0.cmp(&right.0));

        assert!(
            outcomes
                .iter()
                .all(|(_, _, inserted_indices)| inserted_indices == &vec![0, 1, 2]),
            "unique external ids must not be routed through the existing-conversation merge path: {outcomes:?}"
        );

        let distinct_ids: HashSet<i64> = outcomes
            .iter()
            .map(|(_, conversation_id, _)| *conversation_id)
            .collect();
        assert_eq!(
            distinct_ids.len(),
            conversations.len(),
            "unique external ids must produce distinct conversation ids: {outcomes:?}"
        );

        let reader = FrankenStorage::open(&db_path).unwrap();
        let stored_rows: Vec<(i64, String)> = reader
            .raw()
            .query_map_collect(
                "SELECT id, external_id FROM conversations ORDER BY id",
                &[],
                |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
            )
            .unwrap();
        let stored_count: i64 = reader
            .raw()
            .query_row_map("SELECT COUNT(*) FROM conversations", &[], |row| {
                row.get_typed(0)
            })
            .unwrap();

        assert_eq!(
            stored_count as usize,
            conversations.len(),
            "parallel distinct inserts must persist one row per external id; rows={stored_rows:?}; outcomes={outcomes:?}"
        );
        assert_eq!(
            stored_rows.len(),
            conversations.len(),
            "parallel distinct inserts must remain visible after reopening; rows={stored_rows:?}; outcomes={outcomes:?}"
        );
    }

    #[test]
    fn insert_conversation_tree_merges_duplicate_external_id_with_gaps() {
        use crate::connectors::{NormalizedConversation, NormalizedMessage};
        use crate::indexer::persist::map_to_internal;
        use crate::model::types::{Agent, AgentKind};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let base_conv = |messages: Vec<NormalizedMessage>| NormalizedConversation {
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("tree-gap-session".into()),
            title: Some("Tree Gap Session".into()),
            source_path: PathBuf::from("/tmp/tree.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_999),
            metadata: serde_json::Value::Null,
            messages,
        };

        let conv_a = map_to_internal(&base_conv(vec![
            NormalizedMessage {
                idx: 2,
                role: "user".into(),
                author: None,
                created_at: Some(1_700_000_000_200),
                content: "third".into(),
                extra: serde_json::Value::Null,
                snippets: Vec::new(),
                invocations: Vec::new(),
            },
            NormalizedMessage {
                idx: 3,
                role: "assistant".into(),
                author: None,
                created_at: Some(1_700_000_000_300),
                content: "fourth".into(),
                extra: serde_json::Value::Null,
                snippets: Vec::new(),
                invocations: Vec::new(),
            },
        ]));
        let conv_b = map_to_internal(&base_conv(vec![
            NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(1_700_000_000_000),
                content: "first".into(),
                extra: serde_json::Value::Null,
                snippets: Vec::new(),
                invocations: Vec::new(),
            },
            NormalizedMessage {
                idx: 1,
                role: "assistant".into(),
                author: None,
                created_at: Some(1_700_000_000_100),
                content: "second".into(),
                extra: serde_json::Value::Null,
                snippets: Vec::new(),
                invocations: Vec::new(),
            },
            NormalizedMessage {
                idx: 3,
                role: "assistant".into(),
                author: None,
                created_at: Some(1_700_000_000_300),
                content: "fourth".into(),
                extra: serde_json::Value::Null,
                snippets: Vec::new(),
                invocations: Vec::new(),
            },
        ]));

        let first = storage
            .insert_conversation_tree(agent_id, None, &conv_a)
            .unwrap();
        let second = storage
            .insert_conversation_tree(agent_id, None, &conv_b)
            .unwrap();

        assert_eq!(first.inserted_indices, vec![2, 3]);
        assert_eq!(second.inserted_indices, vec![0, 1]);
        assert_eq!(first.conversation_id, second.conversation_id);

        let stored_indices: Vec<i64> = storage
            .conn
            .query_map_collect("SELECT idx FROM messages ORDER BY idx", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(stored_indices, vec![0, 1, 2, 3]);
    }

    #[test]
    fn insert_conversation_tree_skips_duplicate_message_indices_for_new_conversation() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("duplicate-new-session".into()),
            title: Some("Duplicate New Session".into()),
            source_path: PathBuf::from("/tmp/duplicate-new-session.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_999),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![
                Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(1_700_000_000_000),
                    content: "first canonical".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                },
                Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(1_700_000_000_001),
                    content: "duplicate idx should be skipped".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                },
                Message {
                    id: None,
                    idx: 1,
                    role: MessageRole::Agent,
                    author: None,
                    created_at: Some(1_700_000_000_100),
                    content: "second".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                },
            ],
            source_id: "local".into(),
            origin_host: None,
        };

        let outcome = storage
            .insert_conversation_tree(agent_id, None, &conversation)
            .unwrap();

        assert_eq!(outcome.inserted_indices, vec![0, 1]);

        let stored_messages: Vec<(i64, String)> = storage
            .conn
            .query_map_collect(
                "SELECT idx, content FROM messages ORDER BY idx",
                fparams![],
                |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
            )
            .unwrap();
        assert_eq!(
            stored_messages,
            vec![
                (0, "first canonical".to_string()),
                (1, "second".to_string())
            ]
        );
    }

    #[test]
    fn insert_conversation_tree_merges_duplicate_source_path_without_external_id() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let base_conv = |messages: Vec<Message>| Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: None,
            title: Some("Source Path Merge".into()),
            source_path: PathBuf::from("/tmp/shared-session.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_999),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages,
            source_id: "local".into(),
            origin_host: None,
        };

        let first = storage
            .insert_conversation_tree(
                agent_id,
                None,
                &base_conv(vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::User,
                        author: None,
                        created_at: Some(1_700_000_000_000),
                        content: "first".into(),
                        extra_json: serde_json::Value::Null,
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_700_000_000_100),
                        content: "second".into(),
                        extra_json: serde_json::Value::Null,
                        snippets: Vec::new(),
                    },
                ]),
            )
            .unwrap();

        let second = storage
            .insert_conversation_tree(
                agent_id,
                None,
                &base_conv(vec![
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_700_000_000_100),
                        content: "second".into(),
                        extra_json: serde_json::Value::Null,
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 2,
                        role: MessageRole::User,
                        author: None,
                        created_at: Some(1_700_000_000_200),
                        content: "third".into(),
                        extra_json: serde_json::Value::Null,
                        snippets: Vec::new(),
                    },
                ]),
            )
            .unwrap();

        assert_eq!(first.conversation_id, second.conversation_id);
        assert_eq!(first.inserted_indices, vec![0, 1]);
        assert_eq!(second.inserted_indices, vec![2]);

        let stored_indices: Vec<i64> = storage
            .conn
            .query_map_collect("SELECT idx FROM messages ORDER BY idx", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(stored_indices, vec![0, 1, 2]);
    }

    #[test]
    fn insert_conversation_tree_merges_source_path_duplicates_with_start_drift() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let base_conv = |started_at: Option<i64>, messages: Vec<Message>| Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: None,
            title: Some("Drift Merge".into()),
            source_path: PathBuf::from("/tmp/drift-session.jsonl"),
            started_at,
            ended_at: Some(1_700_000_000_999),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages,
            source_id: "local".into(),
            origin_host: None,
        };

        let first = storage
            .insert_conversation_tree(
                agent_id,
                None,
                &base_conv(
                    Some(1_700_000_000_000),
                    vec![
                        Message {
                            id: None,
                            idx: 0,
                            role: MessageRole::User,
                            author: None,
                            created_at: Some(1_700_000_000_000),
                            content: "first".into(),
                            extra_json: serde_json::Value::Null,
                            snippets: Vec::new(),
                        },
                        Message {
                            id: None,
                            idx: 1,
                            role: MessageRole::Agent,
                            author: None,
                            created_at: Some(1_700_000_000_100),
                            content: "second".into(),
                            extra_json: serde_json::Value::Null,
                            snippets: Vec::new(),
                        },
                    ],
                ),
            )
            .unwrap();

        let second = storage
            .insert_conversation_tree(
                agent_id,
                None,
                &base_conv(
                    Some(1_700_000_004_000),
                    vec![
                        Message {
                            id: None,
                            idx: 1,
                            role: MessageRole::Agent,
                            author: None,
                            created_at: Some(1_700_000_000_100),
                            content: "second".into(),
                            extra_json: serde_json::Value::Null,
                            snippets: Vec::new(),
                        },
                        Message {
                            id: None,
                            idx: 2,
                            role: MessageRole::User,
                            author: None,
                            created_at: Some(1_700_000_004_200),
                            content: "third".into(),
                            extra_json: serde_json::Value::Null,
                            snippets: Vec::new(),
                        },
                    ],
                ),
            )
            .unwrap();

        assert_eq!(first.conversation_id, second.conversation_id);
        assert_eq!(second.inserted_indices, vec![2]);
    }

    #[test]
    fn insert_conversation_tree_keeps_single_message_overlap_sessions_separate() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let make_conv = |started_at: i64, idx: i64, content: &str| Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: None,
            title: Some("Partial overlap".into()),
            source_path: PathBuf::from("/tmp/reused-session.jsonl"),
            started_at: Some(started_at),
            ended_at: Some(started_at + 500),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx,
                role: MessageRole::User,
                author: None,
                created_at: Some(started_at),
                content: content.into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: "local".into(),
            origin_host: None,
        };

        storage
            .insert_conversation_tree(
                agent_id,
                None,
                &Conversation {
                    messages: vec![
                        Message {
                            id: None,
                            idx: 0,
                            role: MessageRole::User,
                            author: None,
                            created_at: Some(1_700_000_000_000),
                            content: "shared opener".into(),
                            extra_json: serde_json::Value::Null,
                            snippets: Vec::new(),
                        },
                        Message {
                            id: None,
                            idx: 1,
                            role: MessageRole::Agent,
                            author: None,
                            created_at: Some(1_700_000_000_100),
                            content: "first session unique".into(),
                            extra_json: serde_json::Value::Null,
                            snippets: Vec::new(),
                        },
                    ],
                    ..make_conv(1_700_000_000_000, 0, "unused")
                },
            )
            .unwrap();
        storage
            .insert_conversation_tree(
                agent_id,
                None,
                &make_conv(1_700_000_900_000, 0, "shared opener"),
            )
            .unwrap();

        let conversation_count: i64 = storage
            .conn
            .query_row_map("SELECT COUNT(*) FROM conversations", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(conversation_count, 2);
    }

    #[test]
    fn insert_conversation_tree_keeps_distinct_source_path_sessions_separate() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let make_conv = |started_at: i64, created_at: i64, content: &str| Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: None,
            title: Some("Same Path Different Session".into()),
            source_path: PathBuf::from("/tmp/reused-session.jsonl"),
            started_at: Some(started_at),
            ended_at: Some(started_at + 500),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(created_at),
                content: content.into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: "local".into(),
            origin_host: None,
        };

        storage
            .insert_conversation_tree(
                agent_id,
                None,
                &make_conv(1_700_000_000_000, 1_700_000_000_000, "first session"),
            )
            .unwrap();
        storage
            .insert_conversation_tree(
                agent_id,
                None,
                &make_conv(1_700_000_900_000, 1_700_000_900_000, "second session"),
            )
            .unwrap();

        let conversation_count: i64 = storage
            .conn
            .query_row_map("SELECT COUNT(*) FROM conversations", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(conversation_count, 2);
    }

    #[test]
    fn insert_conversation_tree_merges_replay_equivalent_messages_with_shifted_idx() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let make_conv = |started_at: i64, messages: Vec<Message>| Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: None,
            title: Some("Shifted replay".into()),
            source_path: PathBuf::from("/tmp/replay-session.jsonl"),
            started_at: Some(started_at),
            ended_at: Some(started_at + 500),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages,
            source_id: "local".into(),
            origin_host: None,
        };

        let first = storage
            .insert_conversation_tree(
                agent_id,
                None,
                &make_conv(
                    1_700_000_000_000,
                    vec![
                        Message {
                            id: None,
                            idx: 0,
                            role: MessageRole::User,
                            author: None,
                            created_at: Some(1_700_000_000_000),
                            content: "first".into(),
                            extra_json: serde_json::Value::Null,
                            snippets: Vec::new(),
                        },
                        Message {
                            id: None,
                            idx: 1,
                            role: MessageRole::Agent,
                            author: None,
                            created_at: Some(1_700_000_000_100),
                            content: "second".into(),
                            extra_json: serde_json::Value::Null,
                            snippets: Vec::new(),
                        },
                    ],
                ),
            )
            .unwrap();

        let second = storage
            .insert_conversation_tree(
                agent_id,
                None,
                &make_conv(
                    1_700_000_900_000,
                    vec![
                        Message {
                            id: None,
                            idx: 10,
                            role: MessageRole::User,
                            author: None,
                            created_at: Some(1_700_000_000_000),
                            content: "first".into(),
                            extra_json: serde_json::Value::Null,
                            snippets: Vec::new(),
                        },
                        Message {
                            id: None,
                            idx: 11,
                            role: MessageRole::Agent,
                            author: None,
                            created_at: Some(1_700_000_000_100),
                            content: "second".into(),
                            extra_json: serde_json::Value::Null,
                            snippets: Vec::new(),
                        },
                        Message {
                            id: None,
                            idx: 12,
                            role: MessageRole::User,
                            author: None,
                            created_at: Some(1_700_000_000_200),
                            content: "third".into(),
                            extra_json: serde_json::Value::Null,
                            snippets: Vec::new(),
                        },
                    ],
                ),
            )
            .unwrap();

        assert_eq!(first.conversation_id, second.conversation_id);
        assert_eq!(second.inserted_indices, vec![12]);

        let stored_indices: Vec<i64> = storage
            .conn
            .query_map_collect(
                "SELECT idx FROM messages WHERE conversation_id = ?1 ORDER BY idx",
                fparams![first.conversation_id],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(stored_indices, vec![0, 1, 12]);
    }

    #[test]
    fn salvage_historical_databases_imports_backups_once_and_merges_overlap() {
        use crate::model::types::{Conversation, Message, MessageRole};
        use std::path::PathBuf;

        fn base_conv(source_path: &str, messages: Vec<Message>) -> Conversation {
            Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: Some(PathBuf::from("/tmp/workspace")),
                external_id: None,
                title: Some("Recovered".into()),
                source_path: PathBuf::from(source_path),
                started_at: Some(1_700_000_000_000),
                ended_at: Some(1_700_000_000_999),
                approx_tokens: None,
                metadata_json: serde_json::Value::Null,
                messages,
                source_id: "local".into(),
                origin_host: None,
            }
        }

        let dir = TempDir::new().unwrap();
        let canonical_db = dir.path().join("agent_search.db");
        let storage = SqliteStorage::open(&canonical_db).unwrap();

        let overlapping_a = base_conv(
            "/tmp/shared-history.jsonl",
            vec![
                Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(1_700_000_000_000),
                    content: "first".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                },
                Message {
                    id: None,
                    idx: 1,
                    role: MessageRole::Agent,
                    author: None,
                    created_at: Some(1_700_000_000_100),
                    content: "second".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                },
            ],
        );
        let overlapping_b = base_conv(
            "/tmp/shared-history.jsonl",
            vec![
                Message {
                    id: None,
                    idx: 1,
                    role: MessageRole::Agent,
                    author: None,
                    created_at: Some(1_700_000_000_100),
                    content: "second".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                },
                Message {
                    id: None,
                    idx: 2,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(1_700_000_000_200),
                    content: "third".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                },
            ],
        );
        let unique = Conversation {
            source_path: PathBuf::from("/tmp/unique-history.jsonl"),
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_001_000),
                content: "unique".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            started_at: Some(1_700_000_001_000),
            ended_at: Some(1_700_000_001_100),
            ..base_conv("/tmp/unique-history.jsonl", Vec::new())
        };

        seed_historical_db_direct(
            &dir.path()
                .join("backups/agent_search.db.20260322T020200.bak"),
            std::slice::from_ref(&overlapping_a),
        );
        seed_historical_db_direct(
            &dir.path().join("agent_search.corrupt.20260324_212907"),
            &[overlapping_b, unique],
        );

        let first = storage.salvage_historical_databases(&canonical_db).unwrap();
        assert_eq!(first.bundles_considered, 2);
        assert_eq!(first.bundles_imported, 2);
        assert_eq!(first.messages_imported, 4);

        let conversations = storage.list_conversations(10, 0).unwrap();
        assert_eq!(conversations.len(), 2);

        let shared_id = conversations
            .iter()
            .find(|conv| conv.source_path == std::path::Path::new("/tmp/shared-history.jsonl"))
            .and_then(|conv| conv.id)
            .unwrap();
        let shared_indices: Vec<i64> = storage
            .fetch_messages(shared_id)
            .unwrap()
            .into_iter()
            .map(|msg| msg.idx)
            .collect();
        assert_eq!(shared_indices, vec![0, 1, 2]);

        let second = storage.salvage_historical_databases(&canonical_db).unwrap();
        assert_eq!(second.bundles_imported, 0);
        assert_eq!(second.messages_imported, 0);
    }

    #[test]
    fn salvage_historical_databases_normalizes_host_only_remote_provenance() {
        use crate::model::types::{Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let canonical_db = dir.path().join("agent_search.db");
        let storage = SqliteStorage::open(&canonical_db).unwrap();

        let host_only_remote = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: None,
            title: Some("Recovered Host Only Remote".into()),
            source_path: PathBuf::from("/tmp/host-only-history.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_999),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_000),
                content: "host-only remote".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: "   ".into(),
            origin_host: Some("builder-5".into()),
        };

        let historical_db = dir
            .path()
            .join("backups/agent_search.db.20260322T020200.bak");
        seed_historical_db_direct(&historical_db, std::slice::from_ref(&host_only_remote));

        let historical_conn =
            FrankenConnection::open(historical_db.to_string_lossy().into_owned()).unwrap();
        historical_conn
            .execute_compat(
                "INSERT INTO sources(id, kind, host_label, created_at, updated_at) VALUES(?1, ?2, ?3, ?4, ?5)",
                fparams!["   ", "ssh", "builder-5", 0_i64, 0_i64],
            )
            .unwrap();
        historical_conn
            .execute_compat(
                "UPDATE conversations SET source_id = ?1, origin_host = ?2 WHERE source_path = ?3",
                fparams!["   ", "builder-5", "/tmp/host-only-history.jsonl"],
            )
            .unwrap();
        historical_conn
            .execute_compat("DELETE FROM sources WHERE id = ?1", fparams!["builder-5"])
            .unwrap();
        drop(historical_conn);

        let first = storage.salvage_historical_databases(&canonical_db).unwrap();
        assert_eq!(first.bundles_imported, 1);
        assert_eq!(first.messages_imported, 1);

        let source_ids = storage.get_source_ids().unwrap();
        assert_eq!(source_ids, vec!["builder-5".to_string()]);

        let conversations = storage.list_conversations(10, 0).unwrap();
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].source_id, "builder-5");
        assert_eq!(conversations[0].origin_host.as_deref(), Some("builder-5"));
    }

    #[test]
    fn historical_salvage_retry_splits_single_conversation_until_it_fits() {
        use crate::model::types::{Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let mut attempts: Vec<Vec<usize>> = Vec::new();
        let entry = HistoricalBatchEntry {
            source_row_id: 77,
            agent_id: 1,
            workspace_id: None,
            conversation: Conversation {
                id: None,
                agent_slug: "gemini".into(),
                workspace: Some(PathBuf::from("/tmp/workspace")),
                external_id: Some("conv-77".into()),
                title: Some("Large recovered conversation".into()),
                source_path: PathBuf::from("/tmp/history.jsonl"),
                started_at: Some(1_700_000_000_000),
                ended_at: Some(1_700_000_000_999),
                approx_tokens: None,
                metadata_json: serde_json::Value::Null,
                messages: (0..4)
                    .map(|idx| Message {
                        id: None,
                        idx,
                        role: MessageRole::User,
                        author: None,
                        created_at: Some(1_700_000_000_000 + idx),
                        content: format!("message-{idx}"),
                        extra_json: serde_json::Value::Null,
                        snippets: Vec::new(),
                    })
                    .collect(),
                source_id: LOCAL_SOURCE_ID.into(),
                origin_host: None,
            },
        };

        let totals = SqliteStorage::import_historical_batch_with_retry(
            std::slice::from_ref(&entry),
            &mut |batch| {
                attempts.push(
                    batch
                        .iter()
                        .map(|entry| entry.conversation.messages.len())
                        .collect(),
                );
                let total_messages: usize = batch
                    .iter()
                    .map(|entry| entry.conversation.messages.len())
                    .sum();
                if total_messages > 1 {
                    Err(anyhow!("out of memory"))
                } else {
                    Ok(HistoricalBatchImportTotals {
                        inserted_source_rows: batch.len(),
                        inserted_messages: total_messages,
                    })
                }
            },
        )
        .unwrap();

        assert_eq!(
            totals,
            HistoricalBatchImportTotals {
                inserted_source_rows: 1,
                inserted_messages: 4,
            }
        );
        assert_eq!(attempts.first().cloned(), Some(vec![4]));
        assert!(
            attempts.iter().filter(|sizes| sizes == &&vec![1]).count() >= 4,
            "expected recursive fallback to reach one-message slices"
        );
    }

    #[test]
    fn salvage_historical_databases_resumes_from_progress_checkpoint() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        fn make_conv(source_path: &str, idx_seed: i64) -> Conversation {
            Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: Some(PathBuf::from("/tmp/workspace")),
                external_id: Some(format!("conv-{idx_seed}")),
                title: Some(format!("Recovered {idx_seed}")),
                source_path: PathBuf::from(source_path),
                started_at: Some(1_700_000_000_000 + idx_seed),
                ended_at: Some(1_700_000_000_100 + idx_seed),
                approx_tokens: None,
                metadata_json: serde_json::Value::Null,
                messages: vec![Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(1_700_000_000_000 + idx_seed),
                    content: format!("message-{idx_seed}"),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                }],
                source_id: LOCAL_SOURCE_ID.into(),
                origin_host: None,
            }
        }

        let dir = TempDir::new().unwrap();
        let canonical_db = dir.path().join("agent_search.db");
        let backup_db = dir
            .path()
            .join("backups/agent_search.db.20260322T020200.bak");
        let storage = SqliteStorage::open(&canonical_db).unwrap();
        let conv_a = make_conv("/tmp/one.jsonl", 1);
        let conv_b = make_conv("/tmp/two.jsonl", 2);
        let conv_c = make_conv("/tmp/three.jsonl", 3);
        seed_historical_db_direct(
            &backup_db,
            &[conv_a.clone(), conv_b.clone(), conv_c.clone()],
        );

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();
        storage
            .insert_conversation_tree(agent_id, None, &conv_a)
            .unwrap();

        let bundle = discover_historical_database_bundles(&canonical_db)
            .into_iter()
            .find(|bundle| bundle.root_path == backup_db)
            .unwrap();
        let first_row_id: i64 = FrankenConnection::open(backup_db.to_string_lossy().into_owned())
            .unwrap()
            .query_row_map(
                "SELECT id FROM conversations WHERE source_path = ?1",
                fparams!["/tmp/one.jsonl"],
                |row| row.get_typed(0),
            )
            .unwrap();
        storage
            .record_historical_bundle_progress(&bundle, "direct-readonly", first_row_id, 50, 99)
            .unwrap();

        let outcome = storage.salvage_historical_databases(&canonical_db).unwrap();
        assert_eq!(outcome.bundles_imported, 1);
        assert_eq!(outcome.conversations_imported, 52);
        assert_eq!(outcome.messages_imported, 101);
        assert_eq!(storage.list_conversations(10, 0).unwrap().len(), 3);

        let progress_key = SqliteStorage::historical_bundle_progress_key(&bundle);
        let progress_left: Option<String> = storage
            .conn
            .query_row_map(
                "SELECT value FROM meta WHERE key = ?1",
                fparams![progress_key.as_str()],
                |row| row.get_typed(0),
            )
            .optional()
            .unwrap();
        assert!(
            progress_left.is_none(),
            "completed salvage should clear bundle progress"
        );

        let second = storage.salvage_historical_databases(&canonical_db).unwrap();
        assert_eq!(second.bundles_imported, 0);
        assert_eq!(second.messages_imported, 0);
    }

    #[test]
    fn list_conversations_for_lexical_rebuild_uses_stable_id_order() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("agent_search.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let make_conv = |source_path: &str, started_at: i64| Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some(source_path.to_string()),
            title: Some(source_path.to_string()),
            source_path: PathBuf::from(source_path),
            started_at: Some(started_at),
            ended_at: Some(started_at + 1),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(started_at),
                content: format!("message for {source_path}"),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };

        let conv_a = make_conv("/tmp/a.jsonl", 3_000);
        let conv_b = make_conv("/tmp/b.jsonl", 1_000);
        let conv_c = make_conv("/tmp/c.jsonl", 2_000);

        storage
            .insert_conversation_tree(agent_id, None, &conv_a)
            .unwrap();
        storage
            .insert_conversation_tree(agent_id, None, &conv_b)
            .unwrap();
        storage
            .insert_conversation_tree(agent_id, None, &conv_c)
            .unwrap();

        let user_order: Vec<PathBuf> = storage
            .list_conversations(10, 0)
            .unwrap()
            .into_iter()
            .map(|conv| conv.source_path)
            .collect();
        assert_eq!(
            user_order,
            vec![
                PathBuf::from("/tmp/a.jsonl"),
                PathBuf::from("/tmp/c.jsonl"),
                PathBuf::from("/tmp/b.jsonl"),
            ]
        );

        let (agent_slugs, workspace_paths) = storage.build_lexical_rebuild_lookups().unwrap();
        let rebuild_order: Vec<PathBuf> = storage
            .list_conversations_for_lexical_rebuild_after_id(10, 0, &agent_slugs, &workspace_paths)
            .unwrap()
            .into_iter()
            .map(|conv| conv.source_path)
            .collect();
        assert_eq!(
            rebuild_order,
            vec![
                PathBuf::from("/tmp/a.jsonl"),
                PathBuf::from("/tmp/b.jsonl"),
                PathBuf::from("/tmp/c.jsonl"),
            ]
        );

        let first_page = storage
            .list_conversations_for_lexical_rebuild_after_id(2, 0, &agent_slugs, &workspace_paths)
            .unwrap();
        let first_page_paths: Vec<PathBuf> = first_page
            .iter()
            .map(|conv| conv.source_path.clone())
            .collect();
        assert_eq!(
            first_page_paths,
            vec![PathBuf::from("/tmp/a.jsonl"), PathBuf::from("/tmp/b.jsonl")]
        );

        let second_page = storage
            .list_conversations_for_lexical_rebuild_after_id(
                2,
                first_page
                    .last()
                    .and_then(|conv| conv.id)
                    .expect("first page should include an id"),
                &agent_slugs,
                &workspace_paths,
            )
            .unwrap();
        let second_page_paths: Vec<PathBuf> = second_page
            .iter()
            .map(|conv| conv.source_path.clone())
            .collect();
        assert_eq!(second_page_paths, vec![PathBuf::from("/tmp/c.jsonl")]);

        let bounded_page = storage
            .list_conversations_for_lexical_rebuild_after_id_through_id(
                10,
                0,
                first_page
                    .last()
                    .and_then(|conv| conv.id)
                    .expect("first page should include an id"),
                &agent_slugs,
                &workspace_paths,
            )
            .unwrap();
        let bounded_paths: Vec<PathBuf> = bounded_page
            .iter()
            .map(|conv| conv.source_path.clone())
            .collect();
        assert_eq!(
            bounded_paths,
            vec![PathBuf::from("/tmp/a.jsonl"), PathBuf::from("/tmp/b.jsonl")]
        );
    }

    #[test]
    fn keyset_traversal_handles_sparse_holey_conversation_ids() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("agent_search.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let make_conv = |label: &str, ts: i64| Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some(label.to_string()),
            title: Some(label.to_string()),
            source_path: PathBuf::from(format!("/tmp/{label}.jsonl")),
            started_at: Some(ts),
            ended_at: Some(ts + 1),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(ts),
                content: format!("msg for {label}"),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };

        for i in 0..6 {
            storage
                .insert_conversation_tree(
                    agent_id,
                    None,
                    &make_conv(&format!("conv-{i}"), 1000 + i),
                )
                .unwrap();
        }

        storage.conn.execute("PRAGMA foreign_keys = OFF").unwrap();
        storage
            .conn
            .execute_compat("DELETE FROM conversations WHERE id IN (2, 4)", fparams![])
            .unwrap();
        storage
            .conn
            .execute_compat(
                "DELETE FROM messages WHERE conversation_id IN (2, 4)",
                fparams![],
            )
            .unwrap();
        storage.conn.execute("PRAGMA foreign_keys = ON").unwrap();

        let (agent_slugs, workspace_paths) = storage.build_lexical_rebuild_lookups().unwrap();

        let page1 = storage
            .list_conversations_for_lexical_rebuild_after_id(2, 0, &agent_slugs, &workspace_paths)
            .unwrap();
        assert_eq!(page1.len(), 2);
        let page1_ids: Vec<i64> = page1.iter().map(|c| c.id.unwrap()).collect();
        assert_eq!(page1_ids, vec![1, 3]);

        let page2 = storage
            .list_conversations_for_lexical_rebuild_after_id(
                2,
                *page1_ids.last().unwrap(),
                &agent_slugs,
                &workspace_paths,
            )
            .unwrap();
        assert_eq!(page2.len(), 2);
        let page2_ids: Vec<i64> = page2.iter().map(|c| c.id.unwrap()).collect();
        assert_eq!(page2_ids, vec![5, 6]);

        let page3 = storage
            .list_conversations_for_lexical_rebuild_after_id(
                2,
                *page2_ids.last().unwrap(),
                &agent_slugs,
                &workspace_paths,
            )
            .unwrap();
        assert!(page3.is_empty());

        let all_ids: Vec<i64> = page1_ids.iter().chain(page2_ids.iter()).copied().collect();
        assert_eq!(all_ids, vec![1, 3, 5, 6]);
    }

    #[test]
    fn keyset_traversal_through_id_with_sparse_ranges() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("agent_search.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let make_conv = |label: &str, ts: i64| Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some(label.to_string()),
            title: Some(label.to_string()),
            source_path: PathBuf::from(format!("/tmp/{label}.jsonl")),
            started_at: Some(ts),
            ended_at: Some(ts + 1),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(ts),
                content: format!("msg for {label}"),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };

        for i in 0..10 {
            storage
                .insert_conversation_tree(
                    agent_id,
                    None,
                    &make_conv(&format!("conv-{i}"), 1000 + i),
                )
                .unwrap();
        }

        storage.conn.execute("PRAGMA foreign_keys = OFF").unwrap();
        storage
            .conn
            .execute_compat(
                "DELETE FROM conversations WHERE id IN (3, 5, 7, 8)",
                fparams![],
            )
            .unwrap();
        storage
            .conn
            .execute_compat(
                "DELETE FROM messages WHERE conversation_id IN (3, 5, 7, 8)",
                fparams![],
            )
            .unwrap();
        storage.conn.execute("PRAGMA foreign_keys = ON").unwrap();

        let (agent_slugs, workspace_paths) = storage.build_lexical_rebuild_lookups().unwrap();

        let through_5 = storage
            .list_conversations_for_lexical_rebuild_after_id_through_id(
                100,
                0,
                5,
                &agent_slugs,
                &workspace_paths,
            )
            .unwrap();
        let through_5_ids: Vec<i64> = through_5.iter().map(|c| c.id.unwrap()).collect();
        assert_eq!(through_5_ids, vec![1, 2, 4]);

        let after_4_through_10 = storage
            .list_conversations_for_lexical_rebuild_after_id_through_id(
                100,
                4,
                10,
                &agent_slugs,
                &workspace_paths,
            )
            .unwrap();
        let ids: Vec<i64> = after_4_through_10.iter().map(|c| c.id.unwrap()).collect();
        assert_eq!(ids, vec![6, 9, 10]);

        let after_10 = storage
            .list_conversations_for_lexical_rebuild_after_id_through_id(
                100,
                10,
                20,
                &agent_slugs,
                &workspace_paths,
            )
            .unwrap();
        assert!(after_10.is_empty());
    }

    #[test]
    fn list_conversation_footprints_for_lexical_rebuild_tracks_utf8_bytes_and_empty_conversations()
    {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("agent_search.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let insert = |external_id: &str, base_ts: i64, messages: Vec<Message>| {
            storage
                .insert_conversation_tree(
                    agent_id,
                    None,
                    &Conversation {
                        id: None,
                        agent_slug: "codex".into(),
                        workspace: Some(PathBuf::from("/tmp/workspace")),
                        external_id: Some(external_id.to_string()),
                        title: Some(external_id.to_string()),
                        source_path: PathBuf::from(format!("/tmp/{external_id}.jsonl")),
                        started_at: Some(base_ts),
                        ended_at: Some(base_ts + 100),
                        approx_tokens: None,
                        metadata_json: serde_json::Value::Null,
                        messages,
                        source_id: LOCAL_SOURCE_ID.into(),
                        origin_host: None,
                    },
                )
                .unwrap()
                .conversation_id
        };

        let ascii_id = insert(
            "footprint-ascii",
            1_700_000_000_000,
            vec![
                Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(1_700_000_000_001),
                    content: "abc".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                },
                Message {
                    id: None,
                    idx: 1,
                    role: MessageRole::Agent,
                    author: None,
                    created_at: Some(1_700_000_000_002),
                    content: "defg".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                },
            ],
        );
        let empty_id = insert("footprint-empty", 1_700_000_001_000, Vec::new());
        let utf8_id = insert(
            "footprint-utf8",
            1_700_000_002_000,
            vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::Tool,
                author: None,
                created_at: Some(1_700_000_002_001),
                content: "hé🙂".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
        );

        let footprints = storage
            .list_conversation_footprints_for_lexical_rebuild()
            .unwrap();
        assert_eq!(
            footprints,
            vec![
                LexicalRebuildConversationFootprintRow {
                    conversation_id: ascii_id,
                    message_count: 2,
                    message_bytes: 7,
                },
                LexicalRebuildConversationFootprintRow {
                    conversation_id: empty_id,
                    message_count: 0,
                    message_bytes: 0,
                },
                LexicalRebuildConversationFootprintRow {
                    conversation_id: utf8_id,
                    message_count: 1,
                    message_bytes: "hé🙂".len(),
                },
            ]
        );
    }

    #[test]
    fn lexical_rebuild_listing_normalizes_host_only_remote_source_from_blank_source_id() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("agent_search.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();
        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("legacy-blank-source".into()),
            title: Some("Legacy blank source".into()),
            source_path: PathBuf::from("/tmp/legacy-blank-source.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_000),
                content: "hello".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };

        let conversation_id = storage
            .insert_conversation_tree(agent_id, None, &conversation)
            .unwrap()
            .conversation_id;
        storage.conn.execute("PRAGMA foreign_keys = OFF").unwrap();
        storage
            .conn
            .execute_compat(
                "UPDATE conversations SET source_id = ?1, origin_host = ?2 WHERE id = ?3",
                fparams!["   ", "dev@laptop", conversation_id],
            )
            .unwrap();
        storage.conn.execute("PRAGMA foreign_keys = ON").unwrap();

        let listed = storage.list_conversations(10, 0).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].source_id, "dev@laptop");
        assert_eq!(listed[0].origin_host.as_deref(), Some("dev@laptop"));

        let (agent_slugs, workspace_paths) = storage.build_lexical_rebuild_lookups().unwrap();
        let rebuild_listed = storage
            .list_conversations_for_lexical_rebuild_after_id(10, 0, &agent_slugs, &workspace_paths)
            .unwrap();
        assert_eq!(rebuild_listed.len(), 1);
        assert_eq!(rebuild_listed[0].source_id, "dev@laptop");
        assert_eq!(rebuild_listed[0].origin_host.as_deref(), Some("dev@laptop"));
    }

    #[test]
    fn seed_canonical_from_best_historical_bundle_copies_data_and_resets_runtime_meta() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let canonical_db = dir.path().join("agent_search.db");
        let source_db = dir
            .path()
            .join("backups/agent_search.db.20260322T020200.bak");

        fs::create_dir_all(source_db.parent().unwrap()).unwrap();

        let source = SqliteStorage::open(&source_db).unwrap();
        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = source.ensure_agent(&agent).unwrap();
        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("seed-conv".into()),
            title: Some("Historical seed".into()),
            source_path: PathBuf::from("/tmp/historical-seed.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            approx_tokens: Some(42),
            metadata_json: serde_json::json!({"seed": true}),
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::Agent,
                author: Some("assistant".into()),
                created_at: Some(1_700_000_000_050),
                content: "seeded message".into(),
                extra_json: serde_json::json!({"usage": {"total_tokens": 12}}),
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };
        source
            .insert_conversation_tree(agent_id, None, &conversation)
            .unwrap();
        source.set_last_scan_ts(123).unwrap();
        source.set_last_indexed_at(456).unwrap();
        source.set_last_embedded_message_id(789).unwrap();
        source
            .conn
            .execute_compat(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
                fparams!["historical_bundle_salvaged:stale", "{\"stale\":true}"],
            )
            .unwrap();
        drop(source);

        // Legacy "duplicate FTS" fixture reconstruction.
        //
        // Post-V14 migration cass drops the V13-era fts_messages virtual table
        // and recreates it lazily, so a freshly-opened canonical DB has zero
        // fts_messages entries in sqlite_master. To reproduce the historical
        // failure mode this test exercises — a legacy v13 bundle with a
        // duplicated CREATE VIRTUAL TABLE row — we have to inject *both*
        // entries: the original V13-era contentless row and the buggy duplicate
        // row. Before V14 existed the original was already present after
        // migration and only the duplicate needed manual injection.
        let legacy_v13_fts_sql = "CREATE VIRTUAL TABLE fts_messages USING fts5(content, title, agent, workspace, source_path, created_at UNINDEXED, content='', tokenize='porter')";
        let duplicate_legacy_fts_sql = "CREATE VIRTUAL TABLE fts_messages USING fts5(content, title, agent, workspace, source_path, created_at UNINDEXED, message_id UNINDEXED, tokenize='porter')";
        let legacy = rusqlite_test_fixture_conn(&source_db);
        legacy
            .execute_batch(
                "UPDATE meta SET value = '13' WHERE key = 'schema_version';
                 DELETE FROM _schema_migrations WHERE version = 14;
                 PRAGMA writable_schema = ON;",
            )
            .unwrap();
        legacy
            .execute(
                "DELETE FROM meta WHERE key = ?1",
                [FTS_FRANKEN_REBUILD_META_KEY],
            )
            .unwrap();
        // Inject the V13 original first.
        legacy
            .execute(
                "INSERT INTO sqlite_master(type, name, tbl_name, rootpage, sql)
                 VALUES('table', 'fts_messages', 'fts_messages', 0, ?1)",
                [legacy_v13_fts_sql],
            )
            .unwrap();
        // Then the duplicate that's the real subject of the fixup logic.
        legacy
            .execute(
                "INSERT INTO sqlite_master(type, name, tbl_name, rootpage, sql)
                 VALUES('table', 'fts_messages', 'fts_messages', 0, ?1)",
                [duplicate_legacy_fts_sql],
            )
            .unwrap();
        legacy
            .execute_batch("PRAGMA writable_schema = OFF;")
            .unwrap();
        drop(legacy);

        // Verify fixture with rusqlite+writable_schema to see raw
        // sqlite_master rows (frankensqlite deduplicates schema entries).
        {
            let verify = rusqlite_test_fixture_conn(&source_db);
            verify
                .execute_batch("PRAGMA writable_schema = ON;")
                .unwrap();
            let fts_entries: i64 = verify
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fts_messages'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                fts_entries, 2,
                "test fixture should reproduce the duplicate legacy fts_messages rows"
            );
            let msg_count: i64 = verify
                .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
                .unwrap();
            assert_eq!(msg_count, 1);
        }

        let fresh = SqliteStorage::open(&canonical_db).unwrap();
        drop(fresh);

        let outcome = seed_canonical_from_best_historical_bundle(&canonical_db)
            .unwrap()
            .unwrap();
        assert_eq!(outcome.bundles_imported, 1);
        assert_eq!(outcome.conversations_imported, 1);
        assert_eq!(outcome.messages_imported, 1);

        let readonly = open_franken_with_flags(
            &canonical_db.to_string_lossy(),
            FrankenOpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let readonly_message_count: i64 = readonly
            .query_row_map("SELECT COUNT(*) FROM messages", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(readonly_message_count, 1);

        let seeded = SqliteStorage::open(&canonical_db).unwrap();
        assert_eq!(
            seeded
                .count_sessions_in_range(None, None, None, None)
                .unwrap()
                .0,
            1
        );
        let message_count: i64 = seeded
            .conn
            .query_row_map("SELECT COUNT(*) FROM messages", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(message_count, 1);
        assert_eq!(seeded.get_last_scan_ts().unwrap(), None);
        assert_eq!(seeded.get_last_embedded_message_id().unwrap(), None);

        let last_indexed: Option<String> = seeded
            .conn
            .query_row_map(
                "SELECT value FROM meta WHERE key = 'last_indexed_at'",
                fparams![],
                |row| row.get_typed(0),
            )
            .optional()
            .unwrap();
        assert!(last_indexed.is_none());

        let salvage_keys: Vec<String> = seeded
            .conn
            .query_map_collect(
                "SELECT key FROM meta WHERE key LIKE 'historical_bundle_salvaged:%' ORDER BY key",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(salvage_keys.len(), 1);

        let reopened_readonly = open_franken_with_flags(
            &canonical_db.to_string_lossy(),
            FrankenOpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let reopened_fts_entries: i64 = reopened_readonly
            .query_row_map(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fts_messages'",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(
            reopened_fts_entries, 1,
            "seeded canonical db should keep a single stock-SQLite fts_messages schema row"
        );
        let reopened_message_count: i64 = reopened_readonly
            .query_row_map("SELECT COUNT(*) FROM messages", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(reopened_message_count, 1);

        let franken_seeded = FrankenStorage::open(&canonical_db).unwrap();
        assert_eq!(
            franken_seeded.schema_version().unwrap(),
            CURRENT_SCHEMA_VERSION
        );
        // Post-V14 fts_messages is recreated lazily. `FrankenStorage::open`
        // alone doesn't re-register the virtual table for the frankensqlite
        // query engine — the consistency pass does, and this is exactly what
        // normal cass startup runs before the first search. Invoke it
        // explicitly so the query below exercises the expected post-repair
        // state rather than the between-steps state.
        franken_seeded
            .ensure_search_fallback_fts_consistency()
            .expect("ensure FTS consistency after seed");
        let post_franken_schema_rows: i64 = franken_seeded
            .raw()
            .query_row_map(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fts_messages'",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(post_franken_schema_rows, 1);
        assert!(
            franken_seeded
                .raw()
                .query("SELECT rowid FROM fts_messages LIMIT 1")
                .is_ok()
        );
    }

    #[test]
    fn failed_baseline_seed_preserves_existing_canonical_bundle() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let canonical_db = dir.path().join("agent_search.db");
        let source_db = dir
            .path()
            .join("backups/agent_search.db.20260325T120000Z.bad-seed.bak");

        fs::create_dir_all(source_db.parent().unwrap()).unwrap();

        let canonical = SqliteStorage::open(&canonical_db).unwrap();
        canonical
            .conn
            .execute_compat(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
                fparams!["sentinel", "keep-me"],
            )
            .unwrap();
        drop(canonical);

        let source = SqliteStorage::open(&source_db).unwrap();
        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = source.ensure_agent(&agent).unwrap();
        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("bad-seed-conv".into()),
            title: Some("Bad seed".into()),
            source_path: PathBuf::from("/tmp/bad-seed.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            approx_tokens: Some(42),
            metadata_json: serde_json::json!({"seed": "bad"}),
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::Agent,
                author: Some("assistant".into()),
                created_at: Some(1_700_000_000_050),
                content: "this seed should fail".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };
        source
            .insert_conversation_tree(agent_id, None, &conversation)
            .unwrap();
        drop(source);

        let legacy = FrankenConnection::open(source_db.to_string_lossy().into_owned()).unwrap();
        legacy
            .execute("UPDATE meta SET value = '12' WHERE key = 'schema_version'")
            .unwrap();
        drop(legacy);

        let err = seed_canonical_from_best_historical_bundle(&canonical_db).unwrap_err();
        assert!(
            err.to_string()
                .contains("schema_version 12 is too old for baseline import"),
            "unexpected seed error: {err:#}"
        );

        let reopened = SqliteStorage::open(&canonical_db).unwrap();
        let sentinel: Option<String> = reopened
            .conn
            .query_row_map(
                "SELECT value FROM meta WHERE key = 'sentinel'",
                fparams![],
                |row| row.get_typed(0),
            )
            .optional()
            .unwrap();
        assert_eq!(sentinel.as_deref(), Some("keep-me"));

        let conversation_count: i64 = reopened
            .conn
            .query_row_map("SELECT COUNT(*) FROM conversations", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(conversation_count, 0);

        let readonly = open_franken_with_flags(
            &canonical_db.to_string_lossy(),
            FrankenOpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let readonly_conversation_count: i64 = readonly
            .query_row_map("SELECT COUNT(*) FROM conversations", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(readonly_conversation_count, 0);
    }

    #[test]
    fn fetch_messages_for_lexical_rebuild_skips_extra_json() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("lexical-rebuild-test".into()),
            title: Some("Lexical rebuild".into()),
            source_path: PathBuf::from("/tmp/lexical-rebuild.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            approx_tokens: Some(42),
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::Agent,
                author: Some("assistant".into()),
                created_at: Some(1_700_000_000_050),
                content: "indexed text".into(),
                extra_json: serde_json::json!({
                    "usage": { "total_tokens": 1234 },
                    "irrelevant_blob": "still preserved in canonical storage"
                }),
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };

        let inserted = storage
            .insert_conversation_tree(agent_id, None, &conversation)
            .unwrap();
        let conversation_id = inserted.conversation_id;

        let stored = storage.fetch_messages(conversation_id).unwrap();
        assert_eq!(stored.len(), 1);
        assert!(!stored[0].extra_json.is_null());

        let lexical = storage
            .fetch_messages_for_lexical_rebuild(conversation_id)
            .unwrap();
        assert_eq!(lexical.len(), 1);
        assert_eq!(lexical[0].content, "indexed text");
        assert_eq!(lexical[0].author.as_deref(), Some("assistant"));
        assert!(lexical[0].extra_json.is_null());
    }

    #[test]
    fn fetch_messages_for_lexical_rebuild_batch_groups_and_orders_messages() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let first = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("lexical-batch-1".into()),
            title: Some("Lexical batch 1".into()),
            source_path: PathBuf::from("/tmp/lexical-batch-1.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            approx_tokens: Some(42),
            metadata_json: serde_json::Value::Null,
            messages: vec![
                Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::User,
                    author: Some("user".into()),
                    created_at: Some(1_700_000_000_010),
                    content: "first-a".into(),
                    extra_json: serde_json::json!({"opaque": true}),
                    snippets: Vec::new(),
                },
                Message {
                    id: None,
                    idx: 1,
                    role: MessageRole::Agent,
                    author: Some("assistant".into()),
                    created_at: Some(1_700_000_000_020),
                    content: "first-b".into(),
                    extra_json: serde_json::json!({"opaque": true}),
                    snippets: Vec::new(),
                },
            ],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };

        let second = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("lexical-batch-2".into()),
            title: Some("Lexical batch 2".into()),
            source_path: PathBuf::from("/tmp/lexical-batch-2.jsonl"),
            started_at: Some(1_700_000_000_200),
            ended_at: Some(1_700_000_000_300),
            approx_tokens: Some(84),
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::Tool,
                author: Some("tool".into()),
                created_at: Some(1_700_000_000_210),
                content: "second-a".into(),
                extra_json: serde_json::json!({"opaque": true}),
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };

        let first_id = storage
            .insert_conversation_tree(agent_id, None, &first)
            .unwrap()
            .conversation_id;
        let second_id = storage
            .insert_conversation_tree(agent_id, None, &second)
            .unwrap()
            .conversation_id;

        let lexical = storage
            .fetch_messages_for_lexical_rebuild_batch(&[second_id, first_id], None, None)
            .unwrap();

        let first_messages = lexical.get(&first_id).expect("first conversation");
        assert_eq!(first_messages.len(), 2);
        assert_eq!(first_messages[0].content, "first-a");
        assert_eq!(first_messages[1].content, "first-b");
        assert!(
            first_messages
                .iter()
                .all(|message| message.extra_json.is_null())
        );

        let second_messages = lexical.get(&second_id).expect("second conversation");
        assert_eq!(second_messages.len(), 1);
        assert_eq!(second_messages[0].content, "second-a");
        assert!(second_messages[0].extra_json.is_null());
    }

    #[test]
    fn fetch_messages_for_lexical_rebuild_batch_enforces_content_byte_guardrail() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("lexical-batch-guard".into()),
            title: Some("Lexical batch guard".into()),
            source_path: PathBuf::from("/tmp/lexical-batch-guard.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            approx_tokens: Some(42),
            metadata_json: serde_json::Value::Null,
            messages: vec![
                Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::User,
                    author: Some("user".into()),
                    created_at: Some(1_700_000_000_010),
                    content: "123456".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                },
                Message {
                    id: None,
                    idx: 1,
                    role: MessageRole::Agent,
                    author: Some("assistant".into()),
                    created_at: Some(1_700_000_000_020),
                    content: "abcdef".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                },
            ],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };

        let conversation_id = storage
            .insert_conversation_tree(agent_id, None, &conversation)
            .unwrap()
            .conversation_id;

        let error = storage
            .fetch_messages_for_lexical_rebuild_batch(&[conversation_id], Some(10), Some(8))
            .expect_err("guardrail should reject oversized batch content");

        let message = format!("{error:#}");
        assert!(
            message.contains("content-byte guardrail"),
            "expected guardrail reason in error, got {message}"
        );
    }

    #[test]
    fn fetch_messages_handles_manual_rows_inserted_via_raw_connection() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("manual-rows.db");
        let storage = FrankenStorage::open(&db_path).unwrap();
        let conn = storage.raw();

        conn.execute(
            "INSERT INTO agents (id, slug, name, kind, created_at, updated_at)
             VALUES (1, 'claude_code', 'Claude Code', 'local', 0, 0)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations
             (id, agent_id, external_id, title, source_path, source_id, started_at)
             VALUES (1, 1, 'manual-ext', 'Manual Session', '/tmp/manual.jsonl', 'local', 200)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages
             (id, conversation_id, idx, role, author, created_at, content, extra_json, extra_bin)
             VALUES (1, 1, 0, 'user', 'tester', 1700000000000, 'manual body', '{\"k\":1}', NULL)",
        )
        .unwrap();

        let lexical = storage.fetch_messages_for_lexical_rebuild(1).unwrap();
        assert_eq!(lexical.len(), 1);
        assert_eq!(lexical[0].content, "manual body");

        let full = storage.fetch_messages(1).unwrap();
        assert_eq!(full.len(), 1);
        assert_eq!(full[0].content, "manual body");
        assert_eq!(full[0].author.as_deref(), Some("tester"));
        assert_eq!(full[0].extra_json, serde_json::json!({ "k": 1 }));
    }

    #[test]
    fn lexical_rebuild_batch_messages_query_avoids_sorter_temp_btrees() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("agent_search.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "claude_code".into(),
            name: "Claude Code".into(),
            version: None,
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        for (external_id, base_ts) in [
            ("conv-1", 1_700_000_000_000_i64),
            ("conv-2", 1_700_000_001_000_i64),
        ] {
            let conversation = Conversation {
                id: None,
                agent_slug: "claude_code".into(),
                workspace: Some(PathBuf::from("/tmp/workspace")),
                external_id: Some(external_id.to_string()),
                title: Some("Lexical rebuild".into()),
                source_path: PathBuf::from(format!("/tmp/{external_id}.jsonl")),
                started_at: Some(base_ts),
                ended_at: Some(base_ts + 100),
                approx_tokens: None,
                metadata_json: serde_json::Value::Null,
                messages: vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::User,
                        author: Some("user".into()),
                        created_at: Some(base_ts + 10),
                        content: format!("{external_id}-first"),
                        extra_json: serde_json::Value::Null,
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Agent,
                        author: Some("assistant".into()),
                        created_at: Some(base_ts + 20),
                        content: format!("{external_id}-second"),
                        extra_json: serde_json::Value::Null,
                        snippets: Vec::new(),
                    },
                ],
                source_id: LOCAL_SOURCE_ID.into(),
                origin_host: None,
            };
            storage
                .insert_conversation_tree(agent_id, None, &conversation)
                .unwrap();
        }

        let conversation_ids: Vec<i64> = storage
            .conn
            .query_map_collect(
                "SELECT id FROM conversations ORDER BY id",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(conversation_ids.len(), 2);

        let plan_details: Vec<String> = storage
            .conn
            .query_map_collect(
                "EXPLAIN QUERY PLAN \
                 SELECT conversation_id, id, idx, role, author, created_at, content \
                 FROM messages \
                 WHERE conversation_id IN (?1, ?2) \
                 ORDER BY conversation_id ASC, idx ASC",
                fparams![conversation_ids[0], conversation_ids[1]],
                |row| row.get_typed(3),
            )
            .unwrap();

        assert!(
            plan_details
                .iter()
                .any(|detail| detail.contains("sqlite_autoindex_messages_1")),
            "expected batched lexical rebuild fetch to use the conversation_id/idx composite index, got {plan_details:?}"
        );
        assert!(
            !plan_details
                .iter()
                .any(|detail| detail.contains("TEMP B-TREE")),
            "expected batched lexical rebuild fetch to avoid sorter temp b-trees, got {plan_details:?}"
        );
    }

    #[test]
    fn stream_messages_for_lexical_rebuild_groups_and_orders_messages() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let first = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("lexical-stream-1".into()),
            title: Some("Lexical stream 1".into()),
            source_path: PathBuf::from("/tmp/lexical-stream-1.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            approx_tokens: Some(42),
            metadata_json: serde_json::Value::Null,
            messages: vec![
                Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::User,
                    author: Some("user".into()),
                    created_at: Some(1_700_000_000_010),
                    content: "first-a".into(),
                    extra_json: serde_json::json!({"opaque": true}),
                    snippets: Vec::new(),
                },
                Message {
                    id: None,
                    idx: 1,
                    role: MessageRole::Agent,
                    author: Some("assistant".into()),
                    created_at: Some(1_700_000_000_020),
                    content: "first-b".into(),
                    extra_json: serde_json::json!({"opaque": true}),
                    snippets: Vec::new(),
                },
            ],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };

        let second = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("lexical-stream-2".into()),
            title: Some("Lexical stream 2".into()),
            source_path: PathBuf::from("/tmp/lexical-stream-2.jsonl"),
            started_at: Some(1_700_000_000_200),
            ended_at: Some(1_700_000_000_300),
            approx_tokens: Some(84),
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::Tool,
                author: Some("tool".into()),
                created_at: Some(1_700_000_000_210),
                content: "second-a".into(),
                extra_json: serde_json::json!({"opaque": true}),
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };

        let first_id = storage
            .insert_conversation_tree(agent_id, None, &first)
            .unwrap()
            .conversation_id;
        let second_id = storage
            .insert_conversation_tree(agent_id, None, &second)
            .unwrap()
            .conversation_id;

        let mut streamed = Vec::new();
        storage
            .stream_messages_for_lexical_rebuild_from_conversation_id(first_id, |row| {
                streamed.push((
                    row.conversation_id,
                    row.idx,
                    row.role,
                    row.author,
                    row.content,
                ));
                Ok(())
            })
            .unwrap();

        assert_eq!(
            streamed,
            vec![
                (
                    first_id,
                    0,
                    "user".to_string(),
                    Some("user".to_string()),
                    "first-a".to_string(),
                ),
                (
                    first_id,
                    1,
                    "agent".to_string(),
                    Some("assistant".to_string()),
                    "first-b".to_string(),
                ),
                (
                    second_id,
                    0,
                    "tool".to_string(),
                    Some("tool".to_string()),
                    "second-a".to_string(),
                ),
            ]
        );
    }

    #[test]
    fn stream_messages_for_lexical_rebuild_between_conversation_ids_respects_upper_bound() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("agent_search.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "claude_code".into(),
            name: "Claude Code".into(),
            version: Some("1.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let first = Conversation {
            id: None,
            agent_slug: "claude_code".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("lexical-range-1".into()),
            title: Some("Lexical range 1".into()),
            source_path: PathBuf::from("/tmp/lexical-range-1.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            approx_tokens: Some(42),
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: Some("user".into()),
                created_at: Some(1_700_000_000_010),
                content: "first-only".into(),
                extra_json: serde_json::json!({"opaque": true}),
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };

        let second = Conversation {
            id: None,
            agent_slug: "claude_code".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("lexical-range-2".into()),
            title: Some("Lexical range 2".into()),
            source_path: PathBuf::from("/tmp/lexical-range-2.jsonl"),
            started_at: Some(1_700_000_000_200),
            ended_at: Some(1_700_000_000_300),
            approx_tokens: Some(84),
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::Tool,
                author: Some("tool".into()),
                created_at: Some(1_700_000_000_210),
                content: "second-should-not-appear".into(),
                extra_json: serde_json::json!({"opaque": true}),
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };

        let first_id = storage
            .insert_conversation_tree(agent_id, None, &first)
            .unwrap()
            .conversation_id;
        let second_id = storage
            .insert_conversation_tree(agent_id, None, &second)
            .unwrap()
            .conversation_id;

        let mut streamed = Vec::new();
        storage
            .stream_messages_for_lexical_rebuild_between_conversation_ids(
                first_id,
                first_id,
                |row| {
                    streamed.push((row.conversation_id, row.idx, row.content));
                    Ok(())
                },
            )
            .unwrap();

        assert_eq!(streamed, vec![(first_id, 0, "first-only".to_string())]);
        assert!(
            streamed
                .iter()
                .all(|(conversation_id, _, _)| *conversation_id != second_id),
            "upper bound should exclude later conversation ids"
        );
    }

    #[test]
    fn stream_messages_for_lexical_rebuild_between_conversation_ids_handles_mixed_ranges() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("agent_search.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let claude_agent_id = storage
            .ensure_agent(&Agent {
                id: None,
                slug: "claude_code".into(),
                name: "Claude Code".into(),
                version: None,
                kind: AgentKind::Cli,
            })
            .unwrap();
        let aider_agent_id = storage
            .ensure_agent(&Agent {
                id: None,
                slug: "aider".into(),
                name: "Aider".into(),
                version: None,
                kind: AgentKind::Cli,
            })
            .unwrap();

        type MessageSpec = (i64, MessageRole, Option<String>, Option<i64>, String);

        let mut expected = Vec::new();
        let mut first_conversation_id = None;
        let mut last_conversation_id = None;
        let mut insert_conversation =
            |agent_id: i64,
             external_id: &str,
             title: &str,
             source_path: &str,
             started_at: i64,
             message_specs: Vec<MessageSpec>| {
                let conversation = Conversation {
                    id: None,
                    agent_slug: if agent_id == aider_agent_id {
                        "aider".into()
                    } else {
                        "claude_code".into()
                    },
                    workspace: Some(PathBuf::from("/tmp/workspace")),
                    external_id: Some(external_id.to_string()),
                    title: Some(title.to_string()),
                    source_path: PathBuf::from(source_path),
                    started_at: Some(started_at),
                    ended_at: Some(started_at + 100),
                    approx_tokens: None,
                    metadata_json: serde_json::Value::Null,
                    messages: message_specs
                        .iter()
                        .map(|(idx, role, author, created_at, content)| Message {
                            id: None,
                            idx: *idx,
                            role: role.clone(),
                            author: author.clone(),
                            created_at: *created_at,
                            content: content.clone(),
                            extra_json: serde_json::Value::Null,
                            snippets: Vec::new(),
                        })
                        .collect(),
                    source_id: LOCAL_SOURCE_ID.into(),
                    origin_host: None,
                };
                let conversation_id = storage
                    .insert_conversation_tree(agent_id, None, &conversation)
                    .unwrap()
                    .conversation_id;
                if first_conversation_id.is_none() {
                    first_conversation_id = Some(conversation_id);
                }
                last_conversation_id = Some(conversation_id);
                expected.extend(message_specs.into_iter().map(
                    |(idx, role, author, created_at, content)| {
                        (
                            conversation_id,
                            idx,
                            match role {
                                MessageRole::User => "user".to_string(),
                                MessageRole::Agent => "agent".to_string(),
                                MessageRole::Tool => "tool".to_string(),
                                MessageRole::System => "system".to_string(),
                                MessageRole::Other(other) => other,
                            },
                            author,
                            created_at,
                            content,
                        )
                    },
                ));
            };

        for (label, base_ts) in [
            ("alpha", 1_700_000_000_000_i64),
            ("beta", 1_700_000_001_000_i64),
            ("gamma", 1_700_000_002_000_i64),
            ("delta", 1_700_000_003_000_i64),
            ("epsilon", 1_700_000_004_000_i64),
        ] {
            insert_conversation(
                claude_agent_id,
                &format!("lexical-{label}"),
                &format!("Lexical {label}"),
                &format!("/tmp/{label}.jsonl"),
                base_ts,
                vec![
                    (
                        0,
                        MessageRole::User,
                        None,
                        Some(base_ts + 10),
                        format!("{label}_content"),
                    ),
                    (
                        1,
                        MessageRole::Agent,
                        None,
                        Some(base_ts + 20),
                        format!("{label}_content_response"),
                    ),
                ],
            );
        }

        insert_conversation(
            aider_agent_id,
            "lexical-aider-history",
            "Aider Chat: coding_agent_session_search",
            "/tmp/.aider.chat.history.md",
            1_764_619_673_394,
            vec![
                (
                    0,
                    MessageRole::System,
                    Some("system".to_string()),
                    None,
                    "# aider chat started at 2025-12-01 20:07:47".to_string(),
                ),
                (
                    1,
                    MessageRole::User,
                    Some("user".to_string()),
                    None,
                    "/tmp/workspace/.venv/bin/aider --no-git --message hello world".to_string(),
                ),
            ],
        );
        insert_conversation(
            aider_agent_id,
            "lexical-aider-fixture",
            "Aider Chat: aider",
            "/tmp/tests/fixtures/aider/.aider.chat.history.md",
            1_764_621_401_399,
            vec![
                (
                    0,
                    MessageRole::User,
                    Some("user".to_string()),
                    None,
                    "/add src/main.rs".to_string(),
                ),
                (
                    1,
                    MessageRole::Agent,
                    Some("assistant".to_string()),
                    None,
                    "Added src/main.rs to the chat.

#### /add src/main.rs"
                        .to_string(),
                ),
                (
                    2,
                    MessageRole::User,
                    Some("user".to_string()),
                    None,
                    "Please refactor.".to_string(),
                ),
                (
                    3,
                    MessageRole::Agent,
                    Some("assistant".to_string()),
                    None,
                    "Sure, here is the code.".to_string(),
                ),
            ],
        );

        let mut streamed = Vec::new();
        storage
            .stream_messages_for_lexical_rebuild_between_conversation_ids(
                first_conversation_id.unwrap(),
                last_conversation_id.unwrap(),
                |row| {
                    streamed.push((
                        row.conversation_id,
                        row.idx,
                        row.role,
                        row.author,
                        row.created_at,
                        row.content,
                    ));
                    Ok(())
                },
            )
            .unwrap();

        assert_eq!(streamed, expected);
    }

    #[test]
    fn lexical_rebuild_stream_queries_use_rowid_and_per_conversation_probes() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("agent_search.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "claude_code".into(),
            name: "Claude Code".into(),
            version: None,
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        for (external_id, base_ts) in [
            ("conv-1", 1_700_000_000_000_i64),
            ("conv-2", 1_700_000_001_000_i64),
        ] {
            let conversation = Conversation {
                id: None,
                agent_slug: "claude_code".into(),
                workspace: Some(PathBuf::from("/tmp/workspace")),
                external_id: Some(external_id.to_string()),
                title: Some("Lexical rebuild".into()),
                source_path: PathBuf::from(format!("/tmp/{external_id}.jsonl")),
                started_at: Some(base_ts),
                ended_at: Some(base_ts + 100),
                approx_tokens: None,
                metadata_json: serde_json::Value::Null,
                messages: vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::User,
                        author: Some("user".into()),
                        created_at: Some(base_ts + 10),
                        content: format!("{external_id}-first"),
                        extra_json: serde_json::Value::Null,
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Agent,
                        author: Some("assistant".into()),
                        created_at: Some(base_ts + 20),
                        content: format!("{external_id}-second"),
                        extra_json: serde_json::Value::Null,
                        snippets: Vec::new(),
                    },
                ],
                source_id: LOCAL_SOURCE_ID.into(),
                origin_host: None,
            };
            storage
                .insert_conversation_tree(agent_id, None, &conversation)
                .unwrap();
        }

        let first_id: i64 = storage
            .conn
            .query_row_map(
                "SELECT id FROM conversations ORDER BY id LIMIT 1",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        let last_id: i64 = storage
            .conn
            .query_row_map(
                "SELECT id FROM conversations ORDER BY id DESC LIMIT 1",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();

        let conversation_plan_details: Vec<String> = storage
            .conn
            .query_map_collect(
                "EXPLAIN QUERY PLAN                  SELECT id FROM conversations                  WHERE id >= ?1 AND id <= ?2                  ORDER BY id ASC",
                fparams![first_id, last_id],
                |row| row.get_typed(3),
            )
            .unwrap();
        assert!(
            !conversation_plan_details
                .iter()
                .any(|detail| detail.contains("TEMP B-TREE")),
            "expected streamed lexical rebuild conversation listing to avoid sorter temp b-trees, got {conversation_plan_details:?}"
        );

        let message_plan_details: Vec<String> = storage
            .conn
            .query_map_collect(
                "EXPLAIN QUERY PLAN                  SELECT id, idx, role, author, created_at, content                  FROM messages INDEXED BY sqlite_autoindex_messages_1                  WHERE conversation_id = ?1                  ORDER BY idx",
                fparams![first_id],
                |row| row.get_typed(3),
            )
            .unwrap();
        assert!(
            message_plan_details
                .iter()
                .any(|detail| detail.contains("sqlite_autoindex_messages_1")
                    || detail.contains("idx_messages_conv_idx")),
            "expected per-conversation lexical rebuild fetch to use the conversation_id/idx index, got {message_plan_details:?}"
        );
        assert!(
            !message_plan_details
                .iter()
                .any(|detail| detail.contains("TEMP B-TREE")),
            "expected per-conversation lexical rebuild fetch to avoid sorter temp b-trees, got {message_plan_details:?}"
        );
    }

    #[test]
    fn discover_historical_database_bundles_prefers_larger_archives_first() {
        let dir = TempDir::new().unwrap();
        let canonical_db = dir.path().join("agent_search.db");
        fs::write(&canonical_db, b"canonical").unwrap();

        let smaller = dir.path().join("agent_search.corrupt.small");
        fs::write(&smaller, vec![0_u8; 32]).unwrap();

        let backups_dir = dir.path().join("backups");
        fs::create_dir_all(&backups_dir).unwrap();
        let larger = backups_dir.join("agent_search.db.20260322T020200.bak");
        fs::write(&larger, vec![0_u8; 128]).unwrap();

        let bundles = discover_historical_database_bundles(&canonical_db);
        let ordered_paths: Vec<PathBuf> =
            bundles.into_iter().map(|bundle| bundle.root_path).collect();

        assert_eq!(ordered_paths, vec![larger, smaller]);
    }

    #[test]
    fn discover_historical_database_bundles_prefers_queryable_direct_bundles_first() {
        let dir = TempDir::new().unwrap();
        let canonical_db = dir.path().join("agent_search.db");
        fs::write(&canonical_db, b"canonical").unwrap();

        let larger_corrupt = dir.path().join("agent_search.corrupt.20260324_212907");
        fs::write(&larger_corrupt, vec![0_u8; 4096]).unwrap();

        let backups_dir = dir.path().join("backups");
        fs::create_dir_all(&backups_dir).unwrap();
        let smaller_healthy = backups_dir.join("agent_search.db.20260322T020200.bak");
        let conn = FrankenConnection::open(smaller_healthy.to_string_lossy().into_owned()).unwrap();
        conn.execute_batch(
            "CREATE TABLE conversations (id INTEGER PRIMARY KEY, source_path TEXT);
             CREATE TABLE messages (
                 id INTEGER PRIMARY KEY,
                 conversation_id INTEGER NOT NULL,
                 idx INTEGER NOT NULL,
                 content TEXT
             );
             INSERT INTO conversations(id, source_path) VALUES (1, '/tmp/history.jsonl');
             INSERT INTO messages(id, conversation_id, idx, content)
             VALUES (1, 1, 0, 'seed');",
        )
        .unwrap();
        drop(conn);

        let bundles = discover_historical_database_bundles(&canonical_db);
        let ordered_paths: Vec<PathBuf> = bundles
            .iter()
            .map(|bundle| bundle.root_path.clone())
            .collect();

        assert_eq!(ordered_paths, vec![smaller_healthy, larger_corrupt]);
        assert!(bundles[0].supports_direct_readonly);
        assert!(!bundles[1].supports_direct_readonly);
    }

    #[test]
    fn salvage_historical_databases_skips_unreadable_quarantined_bundles() {
        let dir = TempDir::new().unwrap();
        let canonical_db = dir.path().join("agent_search.db");
        let storage = SqliteStorage::open(&canonical_db).unwrap();

        let quarantined = dir.path().join("agent_search.corrupt.20260324_212907");
        fs::write(&quarantined, b"not a sqlite database").unwrap();

        let discovered: Vec<PathBuf> = discover_historical_database_bundles(&canonical_db)
            .into_iter()
            .map(|bundle| bundle.root_path)
            .collect();
        assert_eq!(discovered, vec![quarantined]);

        let outcome = storage.salvage_historical_databases(&canonical_db).unwrap();
        assert_eq!(outcome.bundles_considered, 1);
        assert_eq!(outcome.bundles_imported, 0);
        assert_eq!(outcome.conversations_imported, 0);
        assert_eq!(outcome.messages_imported, 0);
        assert!(storage.list_conversations(10, 0).unwrap().is_empty());
    }

    #[test]
    fn discover_historical_database_bundles_includes_repair_lab_and_snapshots_named_roots() {
        let dir = TempDir::new().unwrap();
        let canonical_db = dir.path().join("agent_search.db");
        fs::write(&canonical_db, b"canonical").unwrap();

        let repair_lab_dir = dir.path().join("repair-lab").join("live-copy");
        fs::create_dir_all(&repair_lab_dir).unwrap();
        let repair_lab_db = repair_lab_dir.join("agent_search.db");
        fs::write(&repair_lab_db, vec![0_u8; 96]).unwrap();
        fs::write(
            repair_lab_dir.join("agent_search.rebuild-test.db"),
            vec![0_u8; 192],
        )
        .unwrap();

        let snapshots_dir = dir.path().join("snapshots").join("20260324T013201Z");
        fs::create_dir_all(&snapshots_dir).unwrap();
        let snapshot_db = snapshots_dir.join("agent_search.db");
        fs::write(&snapshot_db, vec![0_u8; 64]).unwrap();

        let bundles = discover_historical_database_bundles(&canonical_db);
        let ordered_paths: Vec<PathBuf> =
            bundles.into_iter().map(|bundle| bundle.root_path).collect();

        assert!(ordered_paths.contains(&repair_lab_db));
        assert!(ordered_paths.contains(&snapshot_db));
        assert!(
            !ordered_paths
                .iter()
                .any(|path| path.file_name().and_then(|name| name.to_str())
                    == Some("agent_search.rebuild-test.db"))
        );
    }

    #[test]
    fn discover_historical_database_bundles_prefers_healthy_backup_over_replay_priority() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};

        let dir = TempDir::new().unwrap();
        let canonical_db = dir.path().join("agent_search.db");
        fs::write(&canonical_db, b"canonical").unwrap();

        let replay_dir = dir
            .path()
            .join("repair-lab")
            .join("replay-20260324T070101Z");
        fs::create_dir_all(&replay_dir).unwrap();
        let replay_db = replay_dir.join("agent_search.db");
        let replay_storage = SqliteStorage::open(&replay_db).unwrap();
        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = replay_storage.ensure_agent(&agent).unwrap();
        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("replay-conv".into()),
            title: Some("Replay bundle".into()),
            source_path: PathBuf::from("/tmp/replay.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            approx_tokens: Some(42),
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::Agent,
                author: Some("assistant".into()),
                created_at: Some(1_700_000_000_050),
                content: "replay message".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };
        replay_storage
            .insert_conversation_tree(agent_id, None, &conversation)
            .unwrap();
        drop(replay_storage);

        let duplicate_legacy_fts_sql = "CREATE VIRTUAL TABLE fts_messages USING fts5(content, title, agent, workspace, source_path, created_at UNINDEXED, message_id UNINDEXED, tokenize='porter')";
        let replay_legacy = rusqlite_test_fixture_conn(&replay_db);
        replay_legacy
            .execute_batch(
                "UPDATE meta SET value = '13' WHERE key = 'schema_version';
                 DELETE FROM _schema_migrations WHERE version = 14;
                 PRAGMA writable_schema = ON;",
            )
            .unwrap();
        replay_legacy
            .execute(
                "DELETE FROM meta WHERE key = ?1",
                [FTS_FRANKEN_REBUILD_META_KEY],
            )
            .unwrap();
        replay_legacy
            .execute(
                "INSERT INTO sqlite_master(type, name, tbl_name, rootpage, sql)
                 VALUES('table', 'fts_messages', 'fts_messages', 0, ?1)",
                [duplicate_legacy_fts_sql],
            )
            .unwrap();
        replay_legacy
            .execute_batch("PRAGMA writable_schema = OFF;")
            .unwrap();
        drop(replay_legacy);

        let backups_dir = dir.path().join("backups");
        fs::create_dir_all(&backups_dir).unwrap();
        let clean_backup = backups_dir.join("agent_search.db.20260322T020200.bak");
        let clean_storage = SqliteStorage::open(&clean_backup).unwrap();
        let clean_agent_id = clean_storage.ensure_agent(&agent).unwrap();
        clean_storage
            .insert_conversation_tree(clean_agent_id, None, &conversation)
            .unwrap();
        drop(clean_storage);

        let bundles = discover_historical_database_bundles(&canonical_db);
        let ordered_paths: Vec<PathBuf> = bundles
            .iter()
            .map(|bundle| bundle.root_path.clone())
            .collect();

        assert_eq!(ordered_paths[0], clean_backup);
        assert_eq!(ordered_paths[1], replay_db);
        assert_eq!(
            bundles[0].probe.schema_version,
            Some(CURRENT_SCHEMA_VERSION)
        );
        // Post-V14 cass drops the fts_messages virtual table during migration
        // and recreates it lazily on first open, so a freshly-migrated "clean"
        // backup has zero fts_messages rows in sqlite_master. The bundle is
        // still ranked as healthy by `bundle_health_rank` because 0 rows is a
        // legitimate lazy-FTS state (see comment there).
        assert_eq!(bundles[0].probe.fts_schema_rows, Some(0));
        // `fts_queryable` mirrors a direct rusqlite probe; with 0 sqlite_master
        // rows the table isn't queryable until lazy repair runs.
        assert!(!bundles[0].probe.fts_queryable);
        assert_eq!(bundles[1].probe.schema_version, Some(13));
        // The replay bundle had V14 run (dropping fts_messages → 0 rows), then
        // the test rolls meta.schema_version back to 13, deletes the V14
        // marker, and manually injects a duplicate sqlite_master row. Net
        // result: one synthetic (malformed) fts_messages entry.
        assert_eq!(bundles[1].probe.fts_schema_rows, Some(1));
    }

    #[test]
    fn ensure_fts_consistency_via_rusqlite_catches_up_missing_rows() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("fts-catchup.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();
        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("fts-catchup".into()),
            title: Some("FTS catchup".into()),
            source_path: PathBuf::from("/tmp/fts-catchup.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            approx_tokens: Some(42),
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: Some("user".into()),
                created_at: Some(1_700_000_000_050),
                content: "initial message".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };
        storage
            .insert_conversation_tree(agent_id, None, &conversation)
            .unwrap();
        drop(storage);

        rebuild_fts_via_rusqlite(&db_path).unwrap();

        let conn = FrankenConnection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let conversation_id: i64 = conn
            .query_row_map("SELECT id FROM conversations LIMIT 1", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        conn.execute_compat(
            "INSERT INTO messages(id, conversation_id, idx, role, author, created_at, content, extra_json, extra_bin)
             VALUES(2, ?1, 1, 'assistant', 'assistant', 1700000000060, 'authentication catchup', NULL, NULL)",
            fparams![conversation_id],
        )
        .unwrap();
        drop(conn);

        let repair = ensure_fts_consistency_via_rusqlite(&db_path).unwrap();
        assert_eq!(
            repair,
            FtsConsistencyRepair::IncrementalCatchUp {
                inserted_rows: 1,
                total_rows: 2
            }
        );

        let conn = FrankenConnection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let auth_rows: i64 = conn
            .query_row_map(
                "SELECT COUNT(*) FROM fts_messages WHERE rowid = 2",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(auth_rows, 1);
    }

    #[test]
    fn rebuild_fts_via_rusqlite_cleans_duplicate_legacy_schema_rows() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("fts-duplicate-rebuild.db");

        let storage = SqliteStorage::open(&db_path).unwrap();
        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("0.2.3".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();
        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/ws")),
            external_id: Some("retro".into()),
            title: Some("retro".into()),
            source_path: PathBuf::from("/tmp/retro.jsonl"),
            started_at: Some(42),
            ended_at: Some(42),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(42),
                content: "retro investigation".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };
        storage
            .insert_conversation_tree(agent_id, None, &conversation)
            .unwrap();
        drop(storage);
        materialize_fresh_fts_schema_via_rusqlite(&db_path).unwrap();

        let conn = rusqlite_test_fixture_conn(&db_path);
        conn.execute_batch("PRAGMA writable_schema = ON;").unwrap();
        conn.execute(
            "INSERT INTO sqlite_master(type, name, tbl_name, rootpage, sql)
             VALUES('table', 'fts_messages', 'fts_messages', 0, ?1)",
            ["CREATE VIRTUAL TABLE fts_messages USING fts5(content, title, agent, workspace, source_path, created_at UNINDEXED, message_id UNINDEXED, tokenize='porter')"],
        )
        .unwrap();
        conn.execute_batch("PRAGMA writable_schema = OFF;").unwrap();
        let duplicate_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fts_messages'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(duplicate_rows, 2);
        drop(conn);

        let inserted = rebuild_fts_via_rusqlite(&db_path).unwrap();
        assert_eq!(inserted, 1);

        let conn = FrankenConnection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let schema_rows = franken_fts_schema_rows(&conn).unwrap();
        assert_eq!(
            schema_rows, 1,
            "DROP TABLE should leave one clean FTS schema"
        );
        let match_count: i64 = conn
            .query_row_map("SELECT COUNT(*) FROM fts_messages", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(match_count, 1);
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
    fn ensure_agent_unchanged_preserves_updated_at() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: Some("1.0".into()),
            kind: AgentKind::Cli,
        };

        storage.ensure_agent(&agent).unwrap();
        let initial_updated_at: i64 = storage
            .conn
            .query_row_map(
                "SELECT updated_at FROM agents WHERE slug = ?1",
                fparams![agent.slug.as_str()],
                |row| row.get_typed(0),
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));

        storage.ensure_agent(&agent).unwrap();
        let fetched_updated_at: i64 = storage
            .conn
            .query_row_map(
                "SELECT updated_at FROM agents WHERE slug = ?1",
                fparams![agent.slug.as_str()],
                |row| row.get_typed(0),
            )
            .unwrap();

        assert_eq!(fetched_updated_at, initial_updated_at);
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
    fn upsert_source_unchanged_preserves_updated_at() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let source = Source {
            id: "stable-source".into(),
            kind: SourceKind::Ssh,
            host_label: Some("builder.local".into()),
            machine_id: None,
            platform: Some("linux".into()),
            config_json: Some(serde_json::json!({"role": "bench"})),
            created_at: None,
            updated_at: None,
        };

        storage.upsert_source(&source).unwrap();
        let initial = storage.get_source("stable-source").unwrap().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));

        storage.upsert_source(&source).unwrap();
        let fetched = storage.get_source("stable-source").unwrap().unwrap();

        assert_eq!(fetched.created_at, initial.created_at);
        assert_eq!(fetched.updated_at, initial.updated_at);
        assert_eq!(fetched.host_label, initial.host_label);
        assert_eq!(fetched.platform, initial.platform);
        assert_eq!(fetched.config_json, initial.config_json);
    }

    #[test]
    fn ensure_source_for_conversation_recreates_remote_source_after_delete() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/ws/cache-recreate")),
            external_id: Some("cache-recreate".into()),
            title: Some("Cache Recreate".into()),
            source_path: PathBuf::from("/log/cache-recreate.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_001),
            approx_tokens: Some(16),
            metadata_json: serde_json::json!({}),
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: Some("tester".into()),
                created_at: Some(1_700_000_000_000),
                content: "cache recreate".into(),
                extra_json: serde_json::json!({}),
                snippets: Vec::new(),
            }],
            source_id: "cache-remote-source".into(),
            origin_host: Some("builder-cache".into()),
        };

        storage
            .ensure_source_for_conversation(&conversation)
            .unwrap();
        assert!(storage.get_source("cache-remote-source").unwrap().is_some());

        let deleted = storage.delete_source("cache-remote-source", false).unwrap();
        assert!(deleted);
        assert!(storage.get_source("cache-remote-source").unwrap().is_none());

        storage
            .ensure_source_for_conversation(&conversation)
            .unwrap();
        let recreated = storage.get_source("cache-remote-source").unwrap();
        assert!(recreated.is_some());
        assert_eq!(
            recreated.unwrap().host_label.as_deref(),
            Some("builder-cache")
        );
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
    fn insert_conversation_tree_blank_local_source_normalizes_to_local_id() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent_id = storage
            .ensure_agent(&Agent {
                id: None,
                slug: "codex".into(),
                name: "Codex".into(),
                version: None,
                kind: AgentKind::Cli,
            })
            .unwrap();

        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some("blank-local-source".into()),
            title: Some("Blank local source".into()),
            source_path: dir.path().join("blank-local.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_001),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_000),
                content: "hello".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: "   ".into(),
            origin_host: None,
        };

        storage
            .insert_conversation_tree(agent_id, None, &conversation)
            .unwrap();

        assert!(storage.get_source("   ").unwrap().is_none());
        let source = storage
            .get_source(LOCAL_SOURCE_ID)
            .unwrap()
            .expect("local source row should exist");
        assert_eq!(source.kind, SourceKind::Local);
        assert_eq!(source.host_label, None);

        let conversations = storage.list_conversations(10, 0).unwrap();
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].source_id, LOCAL_SOURCE_ID);
        assert_eq!(conversations[0].origin_host, None);
    }

    #[test]
    fn repeated_local_inserts_do_not_touch_bootstrap_source_row() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent_id = storage
            .ensure_agent(&Agent {
                id: None,
                slug: "codex".into(),
                name: "Codex".into(),
                version: None,
                kind: AgentKind::Cli,
            })
            .unwrap();

        let bootstrap_updated_at: i64 = storage
            .conn
            .query_row_map(
                "SELECT updated_at FROM sources WHERE id = ?1",
                fparams![LOCAL_SOURCE_ID],
                |row| row.get_typed(0),
            )
            .unwrap();

        let make_conversation = |external_id: &str, suffix: &str| Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some(external_id.into()),
            title: Some(format!("Local source {suffix}")),
            source_path: dir.path().join(format!("local-{suffix}.jsonl")),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_001),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_000),
                content: format!("hello-{suffix}"),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };

        std::thread::sleep(std::time::Duration::from_millis(5));
        storage
            .insert_conversation_tree(agent_id, None, &make_conversation("local-source-1", "one"))
            .unwrap();
        let after_first_insert: i64 = storage
            .conn
            .query_row_map(
                "SELECT updated_at FROM sources WHERE id = ?1",
                fparams![LOCAL_SOURCE_ID],
                |row| row.get_typed(0),
            )
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(5));
        storage
            .insert_conversation_tree(agent_id, None, &make_conversation("local-source-2", "two"))
            .unwrap();
        let after_second_insert: i64 = storage
            .conn
            .query_row_map(
                "SELECT updated_at FROM sources WHERE id = ?1",
                fparams![LOCAL_SOURCE_ID],
                |row| row.get_typed(0),
            )
            .unwrap();

        assert_eq!(after_first_insert, bootstrap_updated_at);
        assert_eq!(after_second_insert, bootstrap_updated_at);
    }

    #[test]
    fn insert_conversation_tree_blank_remote_source_normalizes_to_origin_host() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent_id = storage
            .ensure_agent(&Agent {
                id: None,
                slug: "codex".into(),
                name: "Codex".into(),
                version: None,
                kind: AgentKind::Cli,
            })
            .unwrap();

        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some("blank-remote-source".into()),
            title: Some("Blank remote source".into()),
            source_path: dir.path().join("blank-remote.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_001),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_000),
                content: "hello".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: "   ".into(),
            origin_host: Some("user@work-laptop".into()),
        };

        storage
            .insert_conversation_tree(agent_id, None, &conversation)
            .unwrap();

        assert!(storage.get_source("   ").unwrap().is_none());
        let source = storage
            .get_source("user@work-laptop")
            .unwrap()
            .expect("normalized remote source row should exist");
        assert_eq!(source.kind, SourceKind::Ssh);
        assert_eq!(source.host_label.as_deref(), Some("user@work-laptop"));

        let conversations = storage.list_conversations(10, 0).unwrap();
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].source_id, "user@work-laptop");
        assert_eq!(
            conversations[0].origin_host.as_deref(),
            Some("user@work-laptop")
        );
    }

    #[test]
    fn insert_conversations_batched_normalizes_host_only_remote_source_id() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let agent_id = storage
            .ensure_agent(&Agent {
                id: None,
                slug: "codex".into(),
                name: "Codex".into(),
                version: None,
                kind: AgentKind::Cli,
            })
            .unwrap();

        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some("batched-blank-remote-source".into()),
            title: Some("Batched blank remote source".into()),
            source_path: dir.path().join("batched-blank-remote.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_001),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_000),
                content: "hello".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: "   ".into(),
            origin_host: Some("user@batch-host".into()),
        };

        storage
            .insert_conversations_batched(&[(agent_id, None, &conversation)])
            .unwrap();

        assert!(storage.get_source("   ").unwrap().is_none());
        let source = storage
            .get_source("user@batch-host")
            .unwrap()
            .expect("normalized batched remote source row should exist");
        assert_eq!(source.kind, SourceKind::Ssh);
        assert_eq!(source.host_label.as_deref(), Some("user@batch-host"));

        let conversations = storage.list_conversations(10, 0).unwrap();
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].source_id, "user@batch-host");
        assert_eq!(
            conversations[0].origin_host.as_deref(),
            Some("user@batch-host")
        );
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
        let storage = SqliteStorage::open(&db_path).unwrap();

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
    fn parse_historical_json_column_preserves_large_payloads_as_raw_json() {
        let raw = format!("{{\"blob\":\"{}\"}}", "x".repeat(1_000_000));

        let value = parse_historical_json_column(Some(raw.clone()));

        assert_eq!(historical_raw_json(&value), Some(raw.as_str()));
        assert_eq!(json_value_size_hint(&value), raw.len());
    }

    #[test]
    fn parse_historical_json_column_preserves_small_payloads_as_raw_json() {
        let raw = String::from("{\"ok\":true,\"n\":1}");

        let value = parse_historical_json_column(Some(raw.clone()));

        assert_eq!(historical_raw_json(&value), Some(raw.as_str()));
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
        let has_metadata_bin = storage
            .raw()
            .query("PRAGMA table_info(conversations)")
            .unwrap()
            .iter()
            .any(|row| row.get_typed::<String>(1).unwrap() == "metadata_bin");
        assert!(
            has_metadata_bin,
            "conversations should have metadata_bin column"
        );

        // Verify extra_bin column exists
        let has_extra_bin = storage
            .raw()
            .query("PRAGMA table_info(messages)")
            .unwrap()
            .iter()
            .any(|row| row.get_typed::<String>(1).unwrap() == "extra_bin");
        assert!(has_extra_bin, "messages should have extra_bin column");
    }

    #[test]
    fn insert_conversation_tree_rehydrates_append_tail_state_cache_after_manual_clear() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("append-tail-state-cache.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        let agent_id = storage
            .ensure_agent(&Agent {
                id: None,
                slug: "codex".into(),
                name: "Codex".into(),
                version: None,
                kind: AgentKind::Cli,
            })
            .unwrap();
        let workspace = PathBuf::from("/ws/profiled-append-remote");
        let workspace_id = storage.ensure_workspace(&workspace, None).unwrap();

        let initial = make_profiled_append_remote_merge_conversation(11, 5);
        let insert_outcome = storage
            .insert_conversation_tree(agent_id, Some(workspace_id), &initial)
            .unwrap();
        let conversation_id = insert_outcome.conversation_id;

        let initial_tail: (Option<i64>, Option<i64>) = storage
            .raw()
            .query_row_map(
                "SELECT last_message_idx, last_message_created_at FROM conversations WHERE id = ?1",
                fparams![conversation_id],
                |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
            )
            .unwrap();
        assert_eq!(initial_tail, (Some(4), Some(111_004)));

        storage
            .raw()
            .execute_compat(
                "UPDATE conversations
                 SET last_message_idx = NULL, last_message_created_at = NULL
                 WHERE id = ?1",
                fparams![conversation_id],
            )
            .unwrap();

        let appended = make_profiled_append_remote_merge_conversation(11, 10);
        let append_outcome = storage
            .insert_conversation_tree(agent_id, Some(workspace_id), &appended)
            .unwrap();
        assert_eq!(append_outcome.inserted_indices, vec![5, 6, 7, 8, 9]);

        let final_tail: (Option<i64>, Option<i64>) = storage
            .raw()
            .query_row_map(
                "SELECT last_message_idx, last_message_created_at FROM conversations WHERE id = ?1",
                fparams![conversation_id],
                |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
            )
            .unwrap();
        assert_eq!(final_tail, (Some(9), Some(111_009)));
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
    // LazyFrankenDb tests (bd-1ueu)
    // =========================================================================

    #[test]
    fn lazy_franken_db_not_open_before_get() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("lazy_test.db");

        // Create a real DB so the path exists
        let _storage = SqliteStorage::open(&db_path).unwrap();

        let lazy = LazyFrankenDb::new(db_path);
        assert!(
            !lazy.is_open(),
            "LazyFrankenDb must not open on construction"
        );
    }

    #[test]
    fn lazy_franken_db_opens_on_first_get() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("lazy_test.db");

        // Create a real DB so the path exists
        let _storage = SqliteStorage::open(&db_path).unwrap();
        drop(_storage);

        let lazy = LazyFrankenDb::new(db_path);
        assert!(!lazy.is_open());

        let conn = lazy.get("test").expect("should open successfully");
        let count: i64 = conn
            .query_row_map("SELECT COUNT(*) FROM conversations", fparams![], |r| {
                r.get_typed(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        drop(conn);

        assert!(lazy.is_open(), "LazyFrankenDb must be open after get()");
    }

    #[test]
    fn lazy_franken_db_reuses_connection() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("lazy_test.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        drop(_storage);

        let lazy = LazyFrankenDb::new(db_path);

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
                .query_row_map("SELECT COUNT(*) FROM test_tbl", fparams![], |r| {
                    r.get_typed(0)
                })
                .unwrap();
            assert_eq!(count, 0);
        }
    }

    #[test]
    fn lazy_franken_db_not_found_error() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("nonexistent.db");

        let lazy = LazyFrankenDb::new(db_path);
        let result = lazy.get("test");
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), LazyDbError::NotFound(_)),
            "should return NotFound for missing DB"
        );
    }

    #[test]
    fn lazy_franken_db_path_accessor() {
        let path = PathBuf::from("/tmp/test_lazy.db");
        let lazy = LazyFrankenDb::new(path.clone());
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
        // input excludes cache tokens to avoid double-charging them at both the
        // full input rate and the cache-specific rates.
        // non-cache input: 300K * 15/1M = 4.5, output: 100K * 75/1M = 7.5
        // cache_read: 500K * 1.5/1M = 0.75, cache_creation: 200K * 18.75/1M = 3.75
        // total = 16.5
        assert!((cost.unwrap() - 16.5).abs() < 1e-10);
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
        let storage = FrankenStorage::new(conn, PathBuf::from(":memory:"));
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
    fn franken_transition_with_fts_virtual_table_succeeds() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_transition_with_fts.db");

        let conn = FrankenConnection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta(key, value) VALUES('schema_version', '13');
             CREATE TABLE conversations (id INTEGER PRIMARY KEY);
             CREATE VIRTUAL TABLE fts_messages USING fts5(
                 content,
                 title,
                 agent,
                 workspace,
                 source_path,
                 created_at,
                 content='',
                 tokenize='porter unicode61'
             );",
        )
        .unwrap();
        drop(conn);

        let conn = FrankenConnection::open(db_path.to_string_lossy().to_string()).unwrap();
        transition_from_meta_version(&conn).unwrap();

        let rows = conn
            .query("SELECT version FROM _schema_migrations ORDER BY version;")
            .unwrap();
        let versions: Vec<i64> = rows.iter().filter_map(|r| r.get_typed(0).ok()).collect();
        assert_eq!(versions, (1..=13).collect::<Vec<i64>>());
    }

    #[test]
    fn franken_storage_open_legacy_v13_with_fts_virtual_table_succeeds() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_open_legacy_v13_with_fts.db");

        let conn = FrankenConnection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta(key, value) VALUES('schema_version', '13');
             CREATE TABLE agents (
                 id INTEGER PRIMARY KEY,
                 slug TEXT NOT NULL
             );
             CREATE TABLE workspaces (
                 id INTEGER PRIMARY KEY,
                 path TEXT NOT NULL
             );
             CREATE TABLE conversations (
                 id INTEGER PRIMARY KEY,
                 agent_id INTEGER NOT NULL,
                 workspace_id INTEGER,
                 title TEXT,
                 source_path TEXT NOT NULL
             );
             CREATE TABLE messages (
                 id INTEGER PRIMARY KEY,
                 conversation_id INTEGER NOT NULL,
                 idx INTEGER NOT NULL,
                 role TEXT NOT NULL,
                 author TEXT,
                 created_at INTEGER,
                 content TEXT NOT NULL,
                 extra_json TEXT,
                 extra_bin BLOB
             );
             INSERT INTO agents(id, slug) VALUES (1, 'codex');
             INSERT INTO workspaces(id, path) VALUES (1, '/data/projects/coding_agent_session_search');
             INSERT INTO conversations(id, agent_id, workspace_id, title, source_path)
             VALUES (1, 1, 1, 'legacy session', '/tmp/legacy.jsonl');
             INSERT INTO messages(id, conversation_id, idx, role, author, created_at, content)
             VALUES (1, 1, 0, 'user', 'tester', 1710000000000, 'legacy content');
             CREATE VIRTUAL TABLE fts_messages USING fts5(
                 content,
                 title,
                 agent,
                 workspace,
                 source_path,
                 created_at,
                 message_id,
                 content='',
                 tokenize='porter unicode61'
             );",
        )
        .unwrap();
        drop(conn);

        let storage = FrankenStorage::open(&db_path).unwrap();
        assert_eq!(storage.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);

        let rows = storage
            .raw()
            .query("SELECT version FROM _schema_migrations ORDER BY version;")
            .unwrap();
        let versions: Vec<i64> = rows.iter().filter_map(|r| r.get_typed(0).ok()).collect();
        assert_eq!(versions, (1..=CURRENT_SCHEMA_VERSION).collect::<Vec<i64>>());
    }

    #[test]
    fn franken_storage_open_repairs_duplicate_fts_messages_schema_rows() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_open_repairs_duplicate_fts_schema.db");

        let storage = FrankenStorage::open(&db_path).unwrap();
        let agent = Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: None,
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();
        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("dup-fts-schema".into()),
            title: Some("Duplicate FTS schema".into()),
            source_path: PathBuf::from("/tmp/dup-fts-schema.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            approx_tokens: Some(42),
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: Some("user".into()),
                created_at: Some(1_700_000_000_050),
                content: "message that should remain queryable".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };
        storage
            .insert_conversation_tree(agent_id, None, &conversation)
            .unwrap();
        drop(storage);
        materialize_fresh_fts_schema_via_rusqlite(&db_path).unwrap();

        let duplicate_legacy_fts_sql = "CREATE VIRTUAL TABLE fts_messages USING fts5(content, title, agent, workspace, source_path, created_at UNINDEXED, message_id UNINDEXED, tokenize='porter')";
        let conn = rusqlite_test_fixture_conn(&db_path);
        conn.execute_batch("PRAGMA writable_schema = ON;").unwrap();
        conn.execute(
            "INSERT INTO sqlite_master(type, name, tbl_name, rootpage, sql)
             VALUES('table', 'fts_messages', 'fts_messages', 0, ?1)",
            [duplicate_legacy_fts_sql],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM meta WHERE key = ?1",
            [FTS_FRANKEN_REBUILD_META_KEY],
        )
        .unwrap();
        // Simulate a pre-fix upgraded database that has never gone through the
        // authoritative frankensqlite FTS rebuild generation yet.
        conn.execute_batch("PRAGMA writable_schema = OFF;").unwrap();

        let duplicate_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fts_messages'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(duplicate_rows, 2);
        drop(conn);

        let reopened = FrankenStorage::open(&db_path).unwrap();
        assert_eq!(reopened.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
        let generation_rows: Vec<String> = reopened
            .raw()
            .query_map_collect(
                "SELECT value FROM meta WHERE key = ?1",
                fparams![FTS_FRANKEN_REBUILD_META_KEY],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(
            generation_rows.len(),
            0,
            "canonical open should not eagerly rewrite FTS repair metadata"
        );
        reopened.ensure_search_fallback_fts_consistency().unwrap();
        let repaired = FrankenConnection::open(db_path.to_string_lossy().into_owned()).unwrap();
        assert_eq!(franken_fts_schema_rows(&repaired).unwrap(), 1);

        let total_messages: i64 = reopened
            .raw()
            .query_row_map("SELECT COUNT(*) FROM messages", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        let total_fts_rows: i64 = reopened
            .raw()
            .query_row_map("SELECT COUNT(*) FROM fts_messages", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(total_fts_rows, total_messages);
    }

    #[test]
    fn franken_storage_open_fresh_db_keeps_single_franken_fts_schema_row() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("fresh-franken-storage-open.db");

        let storage = FrankenStorage::open(&db_path).unwrap();
        assert_eq!(storage.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);

        // The FTS5 virtual table is no longer created eagerly by the
        // migration runner (V14 drops the old internal-content table and the
        // current contentless table is recreated lazily — see MIGRATION_V14).
        // Invoke the repair path to match normal cass startup, then assert
        // there is exactly one fts_messages entry in sqlite_schema (no
        // duplicates).
        storage
            .ensure_search_fallback_fts_consistency()
            .expect("ensure FTS consistency after fresh open");
        drop(storage);

        let c_reader = FrankenConnection::open(db_path.to_string_lossy().into_owned())
            .expect("open DB via frankensqlite for sqlite_master inspection");
        assert_eq!(
            franken_fts_schema_rows(&c_reader).unwrap(),
            1,
            "exactly one fts_messages schema row should exist after ensure_search_fallback_fts_consistency"
        );
        drop(c_reader);

        let storage = FrankenStorage::open(&db_path).unwrap();
        assert!(
            storage
                .raw()
                .query("SELECT rowid FROM fts_messages LIMIT 1")
                .is_ok(),
            "fts_messages must be queryable through frankensqlite after open"
        );
    }

    #[test]
    fn franken_storage_open_repairs_missing_analytics_tables_when_version_markers_lie() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_repair_missing_analytics.db");

        {
            let storage = FrankenStorage::open(&db_path).unwrap();
            assert_eq!(storage.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
        }

        {
            let conn = FrankenConnection::open(db_path.to_string_lossy().into_owned()).unwrap();
            for table in &[
                "usage_models_daily",
                "usage_daily",
                "usage_hourly",
                "message_metrics",
                "token_daily_stats",
                "token_usage",
                "model_pricing",
                "embedding_jobs",
                "daily_stats",
            ] {
                conn.execute(&format!("DROP TABLE IF EXISTS {table}"))
                    .unwrap();
            }
            conn.execute_compat(
                "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                &[ParamValue::from(CURRENT_SCHEMA_VERSION.to_string())],
            )
            .unwrap();
        }

        let repaired = FrankenStorage::open(&db_path).unwrap();
        assert_eq!(repaired.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);

        let analytics_count: i64 = repaired
            .raw()
            .query_row_map(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table'
                   AND name IN (
                     'daily_stats',
                     'embedding_jobs',
                     'token_usage',
                     'token_daily_stats',
                     'model_pricing',
                     'message_metrics',
                     'usage_hourly',
                     'usage_daily',
                     'usage_models_daily'
                   )",
                &[],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(
            analytics_count, 9,
            "open() should recreate the missing analytics tables even when schema_version already says current"
        );
    }

    #[test]
    fn current_schema_repair_batches_cover_every_required_probe() {
        let missing_tables: Vec<&'static str> = REQUIRED_CURRENT_SCHEMA_TABLE_PROBES
            .iter()
            .map(|(table_name, _)| *table_name)
            .collect();

        let batches = current_schema_repair_batches_for_missing_tables(&missing_tables).unwrap();
        let covered_tables: HashSet<&'static str> = batches
            .iter()
            .flat_map(|batch| batch.tables.iter().copied())
            .collect();

        for table_name in missing_tables {
            assert!(
                covered_tables.contains(table_name),
                "missing repair coverage for {table_name}"
            );
        }
    }

    #[test]
    fn current_schema_repair_batches_do_not_replay_core_schema_bootstrap() {
        for batch in CURRENT_SCHEMA_REPAIR_BATCHES {
            assert!(
                !batch.sql.contains("CREATE TABLE IF NOT EXISTS meta"),
                "repair batch {} should not recreate meta",
                batch.name
            );
            assert!(
                !batch.sql.contains("CREATE TABLE IF NOT EXISTS agents"),
                "repair batch {} should not recreate agents",
                batch.name
            );
            assert!(
                !batch.sql.contains("CREATE TABLE IF NOT EXISTS workspaces"),
                "repair batch {} should not recreate workspaces",
                batch.name
            );
            assert!(
                !batch
                    .sql
                    .contains("CREATE TABLE IF NOT EXISTS conversations"),
                "repair batch {} should not recreate conversations",
                batch.name
            );
            assert!(
                !batch.sql.contains("CREATE TABLE IF NOT EXISTS messages"),
                "repair batch {} should not recreate messages",
                batch.name
            );
            assert!(
                !batch.sql.contains("CREATE TABLE IF NOT EXISTS snippets"),
                "repair batch {} should not recreate snippets",
                batch.name
            );
            assert!(
                !batch.sql.contains("CREATE VIRTUAL TABLE fts_messages"),
                "repair batch {} should not recreate FTS tables",
                batch.name
            );
            assert!(
                !batch.sql.contains("DROP TABLE"),
                "repair batch {} should never drop tables",
                batch.name
            );
        }
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

    #[test]
    fn franken_insert_conversations_batched_populates_analytics_rollups() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use frankensqlite::compat::{ConnectionExt, RowExt};
        use std::path::PathBuf;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("franken-index.db");
        let storage = FrankenStorage::open(&db_path).unwrap();

        let agent = Agent {
            id: None,
            slug: "claude_code".into(),
            name: "Claude Code".into(),
            version: Some("1.0".into()),
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();

        let ts_ms = 1_770_551_400_000_i64;
        let usage_json = serde_json::json!({
            "message": {
                "model": "claude-opus-4-6",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 50,
                    "cache_read_input_tokens": 25,
                    "cache_creation_input_tokens": 10,
                    "service_tier": "standard"
                }
            }
        });

        let conv = Conversation {
            id: None,
            agent_slug: "claude_code".into(),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            external_id: Some("franken-batch-upsert".into()),
            title: Some("Franken batch upsert".into()),
            source_path: PathBuf::from("/tmp/franken.jsonl"),
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
                    content: "Please make a plan.".into(),
                    extra_json: serde_json::Value::Null,
                    snippets: vec![],
                },
                Message {
                    id: None,
                    idx: 1,
                    role: MessageRole::Agent,
                    author: None,
                    created_at: Some(ts_ms + 30_000),
                    content: "## Plan\n\n1. Reproduce\n2. Patch\n3. Verify".into(),
                    extra_json: usage_json,
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
        assert_eq!(outcomes[0].inserted_indices, vec![0, 1]);

        let conn = storage.raw();
        let daily_stats_rows: i64 = conn
            .query_row_map("SELECT COUNT(*) FROM daily_stats", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        let token_daily_rows: i64 = conn
            .query_row_map(
                "SELECT COUNT(*) FROM token_daily_stats",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        let usage_daily_rows: i64 = conn
            .query_row_map("SELECT COUNT(*) FROM usage_daily", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        let model_daily_rows: i64 = conn
            .query_row_map(
                "SELECT COUNT(*) FROM usage_models_daily",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();

        assert!(daily_stats_rows > 0, "daily_stats should be populated");
        assert!(
            token_daily_rows > 0,
            "token_daily_stats should be populated"
        );
        assert!(usage_daily_rows > 0, "usage_daily should be populated");
        assert!(
            model_daily_rows > 0,
            "usage_models_daily should be populated"
        );
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
    fn connection_manager_clamps_zero_writer_limit_to_prevent_deadlock() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("cm.db");

        let fs = FrankenStorage::open(&db_path).unwrap();
        drop(fs);

        let mgr = std::sync::Arc::new(
            FrankenConnectionManager::new(
                &db_path,
                ConnectionManagerConfig {
                    reader_count: 0,
                    max_writers: 0,
                },
            )
            .unwrap(),
        );
        assert_eq!(mgr.reader_count(), 1);
        assert_eq!(mgr.max_writers(), 1);

        let (tx, rx) = std::sync::mpsc::channel();
        let mgr_for_thread = std::sync::Arc::clone(&mgr);
        std::thread::spawn(move || {
            let result = mgr_for_thread.writer().map(|mut guard| {
                guard.mark_committed();
            });
            tx.send(result.is_ok()).expect("writer result send");
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(500)).unwrap(),
            "writer acquisition should not block forever when configured with zero writer slots"
        );
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

    #[test]
    fn purge_agent_archive_data_removes_only_target_agent_and_rebuilds_derived_tables() {
        use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
        use std::path::PathBuf;

        fn seed_conversation(storage: &FrankenStorage, agent_slug: &str, marker: &str) {
            let agent = Agent {
                id: None,
                slug: agent_slug.into(),
                name: agent_slug.into(),
                version: None,
                kind: AgentKind::Cli,
            };
            let agent_id = storage.ensure_agent(&agent).unwrap();
            let conversation = Conversation {
                id: None,
                agent_slug: agent_slug.into(),
                workspace: Some(PathBuf::from("/tmp/workspace")),
                external_id: Some(format!("{agent_slug}-{marker}")),
                title: Some(format!("{agent_slug} {marker}")),
                source_path: PathBuf::from(format!("/tmp/{agent_slug}-{marker}.jsonl")),
                started_at: Some(1_700_000_000_000),
                ended_at: Some(1_700_000_000_100),
                approx_tokens: None,
                metadata_json: serde_json::Value::Null,
                messages: vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::User,
                        author: Some("user".into()),
                        created_at: Some(1_700_000_000_010),
                        content: format!("{agent_slug} {marker} user"),
                        extra_json: serde_json::Value::Null,
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Agent,
                        author: Some("assistant".into()),
                        created_at: Some(1_700_000_000_020),
                        content: format!("{agent_slug} {marker} assistant"),
                        extra_json: serde_json::Value::Null,
                        snippets: Vec::new(),
                    },
                ],
                source_id: LOCAL_SOURCE_ID.into(),
                origin_host: None,
            };
            storage
                .insert_conversation_tree(agent_id, None, &conversation)
                .unwrap();
        }

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("agent_search.db");
        let storage = FrankenStorage::open(&db_path).unwrap();

        seed_conversation(&storage, "openclaw", "purge-target");
        seed_conversation(&storage, "codex", "keep-target");

        let purge = storage.purge_agent_archive_data("openclaw").unwrap();
        assert_eq!(purge.conversations_deleted, 1);
        assert_eq!(purge.messages_deleted, 2);

        storage.rebuild_fts().unwrap();
        storage.rebuild_analytics().unwrap();
        storage.rebuild_daily_stats().unwrap();
        storage.rebuild_token_daily_stats().unwrap();

        let agents = storage.list_agents().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].slug, "codex");
        assert_eq!(storage.total_conversation_count().unwrap(), 1);
        assert_eq!(storage.total_message_count().unwrap(), 2);

        let fts_rows: i64 = storage
            .raw()
            .query_row_map("SELECT COUNT(*) FROM fts_messages", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(fts_rows, 2);

        let total_daily_sessions: i64 = storage
            .raw()
            .query_row_map(
                "SELECT COALESCE(SUM(session_count), 0)
                 FROM daily_stats
                 WHERE agent_slug = 'all' AND source_id = 'all'",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(total_daily_sessions, 1);

        let openclaw_token_rows: i64 = storage
            .raw()
            .query_row_map(
                "SELECT COUNT(*) FROM token_daily_stats WHERE agent_slug = 'openclaw'",
                fparams![],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(openclaw_token_rows, 0);
    }

    /// Regression for cass#202: a `Connection` dropped mid-transaction can
    /// leave child rows persisted without a matching parent. The next indexer
    /// pass then trips `FOREIGN KEY constraint failed` on every write, the
    /// session never gets marked indexed, and the pending backlog grows
    /// without bound. `cleanup_orphan_fk_rows` is the indexer-startup
    /// self-heal that breaks the cycle.
    #[test]
    fn cleanup_orphan_fk_rows_removes_orphans_and_is_noop_on_clean_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("orphan_fk_self_heal.db");
        let storage = FrankenStorage::open(&db_path).unwrap();

        // Plant orphan rows directly: rows whose FK parent does not exist.
        // FK enforcement is temporarily off so the planted rows can land.
        storage.raw().execute("PRAGMA foreign_keys = OFF").unwrap();

        // Seed a real conversation so a subset of children DO have valid
        // parents — we want the cleanup to be precise, not a table-flush.
        storage
            .raw()
            .execute_compat(
                "INSERT INTO agents(id, slug, name, kind, created_at, updated_at) \
                 VALUES(1, 'test-agent', 'Test Agent', 'cli', 0, 0)",
                fparams![],
            )
            .unwrap();
        storage
            .raw()
            .execute_compat(
                "INSERT INTO conversations(id, agent_id, source_id, source_path, started_at) \
                 VALUES(1, 1, 'local', '/tmp/real.jsonl', 0)",
                fparams![],
            )
            .unwrap();
        storage
            .raw()
            .execute_compat(
                "INSERT INTO messages(id, conversation_id, idx, role, content) \
                 VALUES(1, 1, 0, 'user', 'real message')",
                fparams![],
            )
            .unwrap();

        // Plant orphan messages referencing conversation_id=99999 (does not exist)
        // and conversation_id=0 (the specific shape reported in #202). Distinct
        // (conversation_id, idx) pairs are required by the UNIQUE constraint.
        for (mid, cid, idx) in [(101_i64, 99_999_i64, 0_i64), (102, 0, 0), (103, 0, 1)] {
            storage
                .raw()
                .execute_compat(
                    "INSERT INTO messages(id, conversation_id, idx, role, content) \
                     VALUES(?1, ?2, ?3, 'user', 'orphan message')",
                    fparams![mid, cid, idx],
                )
                .unwrap();
        }

        // Plant a directly-orphan snippet — message_id=99999 does not exist
        // anywhere, so this exercises the snippets DELETE path rather than
        // riding on the cascade from the orphan-message DELETE.
        storage
            .raw()
            .execute_compat(
                "INSERT INTO snippets(message_id, file_path, start_line, end_line, language, snippet_text) \
                 VALUES(99999, '/tmp/orphan-snippet.rs', 1, 2, 'rust', 'fn main() {}')",
                fparams![],
            )
            .unwrap();

        storage.raw().execute("PRAGMA foreign_keys = ON").unwrap();

        // Sanity: the planted orphans are visible.
        let messages_before: i64 = storage
            .raw()
            .query_row_map("SELECT COUNT(*) FROM messages", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(messages_before, 4); // 1 real + 3 orphans
        let snippets_before: i64 = storage
            .raw()
            .query_row_map("SELECT COUNT(*) FROM snippets", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(snippets_before, 1);

        // Run the self-heal.
        let report = storage.cleanup_orphan_fk_rows().unwrap();

        // 3 orphan messages + 1 directly-orphan snippet = 4 deletions reported.
        // The real message (id=1) must still be present afterwards.
        let messages_after: i64 = storage
            .raw()
            .query_row_map("SELECT COUNT(*) FROM messages", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(messages_after, 1, "real message must be preserved");
        let snippets_after: i64 = storage
            .raw()
            .query_row_map("SELECT COUNT(*) FROM snippets", fparams![], |row| {
                row.get_typed(0)
            })
            .unwrap();
        assert_eq!(snippets_after, 0);

        assert_eq!(report.total, 4, "report total: {:?}", report);
        let messages_count = report
            .per_table
            .iter()
            .find(|(t, _)| *t == "messages")
            .map(|(_, c)| *c);
        assert_eq!(messages_count, Some(3));
        let snippets_count = report
            .per_table
            .iter()
            .find(|(t, _)| *t == "snippets")
            .map(|(_, c)| *c);
        assert_eq!(snippets_count, Some(1));

        // Second invocation on a now-clean DB must be a no-op.
        let second = storage.cleanup_orphan_fk_rows().unwrap();
        assert_eq!(second.total, 0);
        assert!(second.per_table.is_empty());
    }
}
